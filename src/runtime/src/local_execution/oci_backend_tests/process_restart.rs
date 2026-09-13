//! Cross-process retained-backend recovery against a durable **fixture** owner.
//!
//! `retained_backend_recovers_after_runtime_owner_process_restart` proves the
//! Box client contract: one create/start/exec, expose `Unavailable` on owner
//! death, reconnect, continue stdin/output/signal/wait on the same process
//! handle.
//!
//! `retained_backend_recovers_filesystem_session_after_runtime_owner_process_restart`
//! proves the same owner-death / reconnect contract for keyed file upload and
//! mutating filesystem state that lives in the durable fixture journal (mkdir,
//! list, download after reconnect).
//!
//! Neither test is real Native Linux or utility-VM driver evidence and must
//! not be cited as closing ROADMAP B2 (`b2_process_session_recovery_closed`
//! stays false). Real-driver Live observation lives in
//! `linux-native-live-session-qualification` (fresh manager + keyed exec /
//! keyed upload; no retained-stream claim).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;

#[path = "process_restart/model.rs"]
mod model;
use model::{DurableFixtureProcess, DurableFixtureService, DurableFixtureState};

const CHILD_ENV: &str = "A3S_BOX_TEST_RUNTIME_OWNER_CHILD";
const STATE_ENV: &str = "A3S_BOX_TEST_RUNTIME_OWNER_STATE";
const ENDPOINT_ENV: &str = "A3S_BOX_TEST_RUNTIME_OWNER_ENDPOINT";
const READY_ENV: &str = "A3S_BOX_TEST_RUNTIME_OWNER_READY";
const CALL_LOG_ENV: &str = "A3S_BOX_TEST_RUNTIME_OWNER_CALL_LOG";
const PROCESS_CHILD_TEST_NAME: &str = concat!(
    "local_execution::oci_backend::tests::process_restart::",
    "retained_backend_recovers_after_runtime_owner_process_restart"
);
const FILESYSTEM_CHILD_TEST_NAME: &str = concat!(
    "local_execution::oci_backend::tests::process_restart::",
    "retained_backend_recovers_filesystem_session_after_runtime_owner_process_restart"
);

fn fixture_isolation() -> ExecutionIsolation {
    // Product policy rejects Sandbox on Windows; the durable fixture owner still
    // models retained backend recovery for either isolation class.
    if cfg!(windows) {
        ExecutionIsolation::Microvm
    } else {
        ExecutionIsolation::Sandbox
    }
}

#[async_trait]
impl OciRuntimeService for DurableFixtureService {
    async fn features(&self) -> OciResult<RuntimeInfo> {
        let mut info = runtime_info("experimental");
        info.operations.retain(|operation| {
            matches!(
                operation,
                RuntimeOperation::Features
                    | RuntimeOperation::Create
                    | RuntimeOperation::State
                    | RuntimeOperation::Start
                    | RuntimeOperation::Kill
                    | RuntimeOperation::Delete
                    | RuntimeOperation::Wait
                    | RuntimeOperation::Exec
                    | RuntimeOperation::Processes
                    | RuntimeOperation::ReadOutput
                    | RuntimeOperation::WriteStdin
                    | RuntimeOperation::CloseStdin
                    | RuntimeOperation::SignalProcess
                    | RuntimeOperation::WaitProcess
                    | RuntimeOperation::File
                    | RuntimeOperation::Filesystem
            )
        });
        Ok(info)
    }

    async fn create(&self, request: CreateRequest) -> OciResult<ContainerRecord> {
        let _guard = self
            .lock
            .lock()
            .map_err(|error| lock_error("process-fixture-create", error))?;
        let operation = request.context.operation_id.to_string();
        let config_digest = request.bundle.config_digest().to_string();
        let attachments_digest = request.attachments.digest()?;
        if let Some(state) = self.load("process-fixture-create")? {
            if state.create_operation == operation
                && state.record.state.id() == request.id.as_str()
                && state.record.config_digest == config_digest
                && state.record.attachments_digest.as_deref() == Some(&attachments_digest)
            {
                return Ok(state.record);
            }
            return Err(oci_error(
                ErrorCode::AlreadyExists,
                "process-fixture-create",
                "durable process fixture already owns another create identity",
            ));
        }
        let (driver, isolation) = selected_driver(request.isolation);
        let record = runtime_record(
            &request.id,
            RUNTIME_GENERATION,
            ContainerState::Created,
            driver,
            isolation,
            &config_digest,
            Some(&attachments_digest),
        )?;
        self.store(
            &DurableFixtureState {
                record: record.clone(),
                create_operation: operation,
                start_operation: None,
                exit_status: None,
                process: None,
                directories: Default::default(),
                files: Default::default(),
                file_operations: Default::default(),
                filesystem_operations: Default::default(),
            },
            "process-fixture-create",
        )?;
        self.append_call("create")?;
        Ok(record)
    }

    async fn state(&self, request: StateRequest) -> OciResult<ContainerRecord> {
        let _guard = self
            .lock
            .lock()
            .map_err(|error| lock_error("process-fixture-state", error))?;
        let state = self.load("process-fixture-state")?.ok_or_else(|| {
            oci_error(
                ErrorCode::NotFound,
                "process-fixture-state",
                "durable process fixture is absent",
            )
        })?;
        validate_target(&request.target, &state.record, "process-fixture-state")?;
        Ok(state.record)
    }

    async fn start(&self, request: StartRequest) -> OciResult<ContainerRecord> {
        let _guard = self
            .lock
            .lock()
            .map_err(|error| lock_error("process-fixture-start", error))?;
        let mut state = self.load("process-fixture-start")?.ok_or_else(|| {
            oci_error(
                ErrorCode::NotFound,
                "process-fixture-start",
                "durable process fixture is absent",
            )
        })?;
        validate_target(&request.target, &state.record, "process-fixture-start")?;
        let operation = request.context.operation_id.to_string();
        if state.start_operation.as_deref() == Some(operation.as_str()) {
            return Ok(state.record);
        }
        if state.start_operation.is_some() {
            return Err(oci_error(
                ErrorCode::FailedPrecondition,
                "process-fixture-start",
                "durable process fixture start identity changed",
            ));
        }
        if state.record.state.status() != &ContainerState::Created {
            return Err(oci_error(
                ErrorCode::FailedPrecondition,
                "process-fixture-start",
                "durable process fixture is not created",
            ));
        }
        state.record = Self::rewritten_record(&state, ContainerState::Running)?;
        state.start_operation = Some(operation);
        self.store(&state, "process-fixture-start")?;
        self.append_call("start")?;
        Ok(state.record)
    }

    async fn kill(&self, request: KillRequest) -> OciResult<ContainerRecord> {
        let _guard = self
            .lock
            .lock()
            .map_err(|error| lock_error("process-fixture-kill", error))?;
        let mut state = self.load("process-fixture-kill")?.ok_or_else(|| {
            oci_error(
                ErrorCode::NotFound,
                "process-fixture-kill",
                "durable process fixture is absent",
            )
        })?;
        validate_target(&request.target, &state.record, "process-fixture-kill")?;
        state.record = Self::rewritten_record(&state, ContainerState::Stopped)?;
        state.exit_status = Some(ExitStatus::signaled(request.signal.get(), false)?);
        self.store(&state, "process-fixture-kill")?;
        self.append_call("kill")?;
        Ok(state.record)
    }

    async fn delete(&self, request: DeleteRequest) -> OciResult<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|error| lock_error("process-fixture-delete", error))?;
        let state = self.load("process-fixture-delete")?.ok_or_else(|| {
            oci_error(
                ErrorCode::NotFound,
                "process-fixture-delete",
                "durable process fixture is absent",
            )
        })?;
        validate_target(&request.target, &state.record, "process-fixture-delete")?;
        if request.mode == DeleteMode::StoppedOnly
            && state.record.state.status() != &ContainerState::Stopped
        {
            return Err(oci_error(
                ErrorCode::FailedPrecondition,
                "process-fixture-delete",
                "durable process fixture is not stopped",
            ));
        }
        std::fs::remove_file(&self.state_path).map_err(|error| {
            oci_error(
                ErrorCode::Internal,
                "process-fixture-delete",
                format!("failed to remove durable process fixture: {error}"),
            )
        })?;
        self.append_call("delete")
    }

    async fn exec(&self, request: OciExecRequest) -> OciResult<ProcessRecord> {
        let _guard = self
            .lock
            .lock()
            .map_err(|error| lock_error("process-fixture-exec", error))?;
        let mut state = self.load("process-fixture-exec")?.ok_or_else(|| {
            oci_error(
                ErrorCode::NotFound,
                "process-fixture-exec",
                "durable process fixture is absent",
            )
        })?;
        validate_target(&request.container, &state.record, "process-fixture-exec")?;
        if state.record.state.status() != &ContainerState::Running {
            return Err(oci_error(
                ErrorCode::FailedPrecondition,
                "process-fixture-exec",
                "durable process fixture is not running",
            ));
        }
        if let Some(process) = &state.process {
            if process.request == request {
                return Ok(process.record.clone());
            }
            return Err(oci_error(
                ErrorCode::Conflict,
                "process-fixture-exec",
                "durable process fixture already owns another exec identity",
            ));
        }

        let terminal = request.process.terminal().unwrap_or(false);
        let record = ProcessRecord {
            target: ProcessTarget {
                container: request.container.clone(),
                process_id: request.process_id.clone(),
            },
            pid: Some(9_001),
            terminal,
        };
        let mut output = Vec::new();
        append_output(
            &mut output,
            OutputStream::Stdout,
            b"runtime owner session\n".to_vec(),
            false,
        );
        state.process = Some(DurableFixtureProcess {
            request,
            record: record.clone(),
            output,
            exit_status: None,
            stdin_operations: BTreeMap::new(),
            close_stdin_operations: BTreeMap::new(),
            signal_operations: BTreeMap::new(),
        });
        self.store(&state, "process-fixture-exec")?;
        self.append_call("exec")?;
        Ok(record)
    }

    async fn processes(&self, request: ProcessesRequest) -> OciResult<Vec<ProcessRecord>> {
        let _guard = self
            .lock
            .lock()
            .map_err(|error| lock_error("process-fixture-processes", error))?;
        let state = self.load("process-fixture-processes")?.ok_or_else(|| {
            oci_error(
                ErrorCode::NotFound,
                "process-fixture-processes",
                "durable process fixture is absent",
            )
        })?;
        validate_target(&request.target, &state.record, "process-fixture-processes")?;
        let mut records = Vec::new();
        if state.record.state.status() != &ContainerState::Stopped {
            records.push(ProcessRecord {
                target: ProcessTarget {
                    container: ContainerTarget::exact(
                        request.target.id.clone(),
                        state.record.generation,
                    ),
                    process_id: ProcessId::init(),
                },
                pid: state
                    .record
                    .state
                    .pid()
                    .and_then(|pid| u32::try_from(pid).ok()),
                terminal: false,
            });
        }
        if let Some(process) = state
            .process
            .filter(|process| process.exit_status.is_none())
        {
            records.push(process.record);
        }
        Ok(records)
    }

    async fn read_output(&self, request: ReadOutputRequest) -> OciResult<Vec<OutputChunk>> {
        let _guard = self
            .lock
            .lock()
            .map_err(|error| lock_error("process-fixture-read-output", error))?;
        let state = self.load("process-fixture-read-output")?.ok_or_else(|| {
            oci_error(
                ErrorCode::NotFound,
                "process-fixture-read-output",
                "durable process fixture is absent",
            )
        })?;
        let process = state.process.ok_or_else(|| {
            oci_error(
                ErrorCode::NotFound,
                "process-fixture-read-output",
                "durable exec process is absent",
            )
        })?;
        validate_process_target(
            &request.process,
            &process.record,
            "process-fixture-read-output",
        )?;
        let mut bytes = 0_u64;
        Ok(process
            .output
            .into_iter()
            .filter(|chunk| chunk.sequence > request.after_sequence)
            .take_while(|chunk| {
                let next = bytes.saturating_add(chunk.data.len() as u64);
                if next > u64::from(request.max_bytes) {
                    false
                } else {
                    bytes = next;
                    true
                }
            })
            .collect())
    }

    async fn write_stdin(&self, request: WriteStdinRequest) -> OciResult<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|error| lock_error("process-fixture-write-stdin", error))?;
        let mut state = self.load("process-fixture-write-stdin")?.ok_or_else(|| {
            oci_error(
                ErrorCode::NotFound,
                "process-fixture-write-stdin",
                "durable process fixture is absent",
            )
        })?;
        let process = state.process.as_mut().ok_or_else(|| {
            oci_error(
                ErrorCode::NotFound,
                "process-fixture-write-stdin",
                "durable exec process is absent",
            )
        })?;
        validate_process_target(
            &request.process,
            &process.record,
            "process-fixture-write-stdin",
        )?;
        let operation = request.context.operation_id.to_string();
        if let Some(previous) = process.stdin_operations.get(&operation) {
            if previous == &request {
                return Ok(());
            }
            return Err(oci_error(
                ErrorCode::Conflict,
                "process-fixture-write-stdin",
                "stdin operation identity was reused with different data",
            ));
        }
        append_output(
            &mut process.output,
            OutputStream::Stdout,
            request.data.clone(),
            false,
        );
        process.stdin_operations.insert(operation, request);
        self.store(&state, "process-fixture-write-stdin")?;
        self.append_call("write-stdin")
    }

    async fn close_stdin(&self, request: CloseStdinRequest) -> OciResult<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|error| lock_error("process-fixture-close-stdin", error))?;
        let mut state = self.load("process-fixture-close-stdin")?.ok_or_else(|| {
            oci_error(
                ErrorCode::NotFound,
                "process-fixture-close-stdin",
                "durable process fixture is absent",
            )
        })?;
        let process = state.process.as_mut().ok_or_else(|| {
            oci_error(
                ErrorCode::NotFound,
                "process-fixture-close-stdin",
                "durable exec process is absent",
            )
        })?;
        validate_process_target(
            &request.process,
            &process.record,
            "process-fixture-close-stdin",
        )?;
        let operation = request.context.operation_id.to_string();
        if let Some(previous) = process.close_stdin_operations.get(&operation) {
            if previous == &request {
                return Ok(());
            }
            return Err(oci_error(
                ErrorCode::Conflict,
                "process-fixture-close-stdin",
                "close-stdin operation identity was reused with different content",
            ));
        }
        process.close_stdin_operations.insert(operation, request);
        self.store(&state, "process-fixture-close-stdin")?;
        self.append_call("close-stdin")
    }

    async fn signal_process(&self, request: SignalProcessRequest) -> OciResult<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|error| lock_error("process-fixture-signal-process", error))?;
        let mut state = self
            .load("process-fixture-signal-process")?
            .ok_or_else(|| {
                oci_error(
                    ErrorCode::NotFound,
                    "process-fixture-signal-process",
                    "durable process fixture is absent",
                )
            })?;
        let process = state.process.as_mut().ok_or_else(|| {
            oci_error(
                ErrorCode::NotFound,
                "process-fixture-signal-process",
                "durable exec process is absent",
            )
        })?;
        validate_process_target(
            &request.process,
            &process.record,
            "process-fixture-signal-process",
        )?;
        let operation = request.context.operation_id.to_string();
        if let Some(previous) = process.signal_operations.get(&operation) {
            if previous == &request {
                return Ok(());
            }
            return Err(oci_error(
                ErrorCode::Conflict,
                "process-fixture-signal-process",
                "signal operation identity was reused with different content",
            ));
        }
        if process.exit_status.is_some() {
            return Err(oci_error(
                ErrorCode::FailedPrecondition,
                "process-fixture-signal-process",
                "durable exec process already exited",
            ));
        }
        process.exit_status = Some(ExitStatus::signaled(request.signal.get(), false)?);
        append_missing_eof(&mut process.output, OutputStream::Stdout);
        if !process.record.terminal {
            append_missing_eof(&mut process.output, OutputStream::Stderr);
        }
        process.signal_operations.insert(operation, request);
        self.store(&state, "process-fixture-signal-process")?;
        self.append_call("signal-process")
    }

    async fn wait_process(&self, request: WaitProcessRequest) -> OciResult<ExitStatus> {
        let _guard = self
            .lock
            .lock()
            .map_err(|error| lock_error("process-fixture-wait-process", error))?;
        let state = self.load("process-fixture-wait-process")?.ok_or_else(|| {
            oci_error(
                ErrorCode::NotFound,
                "process-fixture-wait-process",
                "durable process fixture is absent",
            )
        })?;
        let process = state.process.ok_or_else(|| {
            oci_error(
                ErrorCode::NotFound,
                "process-fixture-wait-process",
                "durable exec process is absent",
            )
        })?;
        validate_process_target(
            &request.process,
            &process.record,
            "process-fixture-wait-process",
        )?;
        process.exit_status.ok_or_else(|| {
            Error::new(
                ErrorCode::DeadlineExceeded,
                "durable exec process is still running",
            )
            .for_operation("process-fixture-wait-process")
            .retryable(true)
        })
    }

    async fn wait(&self, request: WaitRequest) -> OciResult<ExitStatus> {
        let _guard = self
            .lock
            .lock()
            .map_err(|error| lock_error("process-fixture-wait", error))?;
        let state = self.load("process-fixture-wait")?.ok_or_else(|| {
            oci_error(
                ErrorCode::NotFound,
                "process-fixture-wait",
                "durable process fixture is absent",
            )
        })?;
        validate_target(&request.target, &state.record, "process-fixture-wait")?;
        state.exit_status.ok_or_else(|| {
            Error::new(
                ErrorCode::DeadlineExceeded,
                "durable process fixture is still running",
            )
            .for_operation("process-fixture-wait")
            .retryable(true)
        })
    }

    async fn file(&self, request: OciFileRequest) -> OciResult<OciFileResponse> {
        let _guard = self
            .lock
            .lock()
            .map_err(|error| lock_error("process-fixture-file", error))?;
        let mut state = self.load("process-fixture-file")?.ok_or_else(|| {
            oci_error(
                ErrorCode::NotFound,
                "process-fixture-file",
                "durable process fixture is absent",
            )
        })?;
        validate_target(&request.target, &state.record, "process-fixture-file")?;
        if state.record.state.status() != &ContainerState::Running {
            return Err(oci_error(
                ErrorCode::FailedPrecondition,
                "process-fixture-file",
                "durable process fixture is not running",
            ));
        }

        let response = match request.op {
            OciFileOp::Upload => {
                let operation = request
                    .context
                    .as_ref()
                    .ok_or_else(|| {
                        oci_error(
                            ErrorCode::InvalidArgument,
                            "process-fixture-file",
                            "durable upload requires an operation context",
                        )
                    })?
                    .operation_id
                    .to_string();
                if let Some(previous) = state.file_operations.get(&operation) {
                    if previous != &request {
                        return Err(oci_error(
                            ErrorCode::Conflict,
                            "process-fixture-file",
                            "upload operation identity was reused with different content",
                        ));
                    }
                    let size = STANDARD
                        .decode(previous.data.as_deref().unwrap_or_default())
                        .map(|bytes| bytes.len() as u64)
                        .unwrap_or(0);
                    return Ok(OciFileResponse {
                        target: request.target.clone(),
                        data: None,
                        size,
                    });
                }
                let decoded = STANDARD
                    .decode(request.data.as_deref().unwrap_or_default())
                    .map_err(|error| {
                        oci_error(
                            ErrorCode::InvalidArgument,
                            "process-fixture-file",
                            format!("invalid base64 upload: {error}"),
                        )
                    })?;
                ensure_parent_directories(&mut state.directories, &request.path);
                state
                    .files
                    .insert(request.path.clone(), STANDARD.encode(&decoded));
                state.file_operations.insert(operation, request.clone());
                self.store(&state, "process-fixture-file")?;
                self.append_call("file-upload")?;
                OciFileResponse {
                    target: request.target.clone(),
                    data: None,
                    size: decoded.len() as u64,
                }
            }
            OciFileOp::Download => {
                let encoded = state.files.get(&request.path).ok_or_else(|| {
                    oci_error(
                        ErrorCode::NotFound,
                        "process-fixture-file",
                        format!("durable file {} is absent", request.path),
                    )
                })?;
                let decoded = STANDARD.decode(encoded).map_err(|error| {
                    oci_error(
                        ErrorCode::Internal,
                        "process-fixture-file",
                        format!("corrupt durable file payload: {error}"),
                    )
                })?;
                self.append_call("file-download")?;
                OciFileResponse {
                    target: request.target.clone(),
                    data: Some(encoded.clone()),
                    size: decoded.len() as u64,
                }
            }
        };
        Ok(response)
    }

    async fn filesystem(&self, request: OciFilesystemRequest) -> OciResult<OciFilesystemResponse> {
        let _guard = self
            .lock
            .lock()
            .map_err(|error| lock_error("process-fixture-filesystem", error))?;
        let mut state = self.load("process-fixture-filesystem")?.ok_or_else(|| {
            oci_error(
                ErrorCode::NotFound,
                "process-fixture-filesystem",
                "durable process fixture is absent",
            )
        })?;
        validate_target(&request.target, &state.record, "process-fixture-filesystem")?;
        if state.record.state.status() != &ContainerState::Running {
            return Err(oci_error(
                ErrorCode::FailedPrecondition,
                "process-fixture-filesystem",
                "durable process fixture is not running",
            ));
        }

        if request.op.is_mutating() {
            let operation = request
                .context
                .as_ref()
                .ok_or_else(|| {
                    oci_error(
                        ErrorCode::InvalidArgument,
                        "process-fixture-filesystem",
                        "durable filesystem mutation requires an operation context",
                    )
                })?
                .operation_id
                .to_string();
            if let Some(previous) = state.filesystem_operations.get(&operation) {
                if previous != &request {
                    return Err(oci_error(
                        ErrorCode::Conflict,
                        "process-fixture-filesystem",
                        "filesystem operation identity was reused with different content",
                    ));
                }
                return Ok(filesystem_mutation_response(&request));
            }
            apply_filesystem_mutation(&mut state, &request)?;
            state
                .filesystem_operations
                .insert(operation, request.clone());
            self.store(&state, "process-fixture-filesystem")?;
            self.append_call("filesystem-mutation")?;
            return Ok(filesystem_mutation_response(&request));
        }

        let response = match request.op {
            OciFilesystemOp::Stat => {
                let entry = durable_stat_entry(&state, &request.path).ok_or_else(|| {
                    oci_error(
                        ErrorCode::NotFound,
                        "process-fixture-filesystem",
                        format!("durable path {} is absent", request.path),
                    )
                })?;
                self.append_call("filesystem-stat")?;
                OciFilesystemResponse {
                    target: request.target.clone(),
                    entry: Some(entry),
                    entries: Vec::new(),
                }
            }
            OciFilesystemOp::ListDir => {
                let entries = durable_list_entries(&state, &request.path);
                self.append_call("filesystem-listdir")?;
                OciFilesystemResponse {
                    target: request.target.clone(),
                    entry: None,
                    entries,
                }
            }
            OciFilesystemOp::MakeDir | OciFilesystemOp::Move | OciFilesystemOp::Remove => {
                unreachable!("mutating filesystem ops are handled above")
            }
        };
        Ok(response)
    }
}

fn normalize_guest_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

fn ensure_parent_directories(directories: &mut std::collections::BTreeSet<String>, path: &str) {
    let normalized = normalize_guest_path(path);
    let Some((parent, _)) = normalized.rsplit_once('/') else {
        return;
    };
    if parent.is_empty() {
        return;
    }
    ensure_directory_tree(directories, parent);
}

fn ensure_directory_tree(directories: &mut std::collections::BTreeSet<String>, path: &str) {
    let normalized = normalize_guest_path(path);
    if normalized == "/" {
        return;
    }
    let mut current = String::new();
    for part in normalized.trim_start_matches('/').split('/') {
        current.push('/');
        current.push_str(part);
        directories.insert(current.clone());
    }
}

fn durable_entry(path: &str, kind: OciFilesystemEntryKind, size: u64) -> OciFilesystemEntry {
    let mut entry = fake_filesystem_entry(path, kind);
    entry.size = size as i64;
    entry
}

fn durable_stat_entry(state: &DurableFixtureState, path: &str) -> Option<OciFilesystemEntry> {
    let normalized = normalize_guest_path(path);
    if let Some(encoded) = state.files.get(&normalized) {
        let size = STANDARD
            .decode(encoded)
            .map(|bytes| bytes.len() as u64)
            .unwrap_or(0);
        return Some(durable_entry(
            &normalized,
            OciFilesystemEntryKind::File,
            size,
        ));
    }
    if state.directories.contains(&normalized) {
        return Some(durable_entry(
            &normalized,
            OciFilesystemEntryKind::Directory,
            0,
        ));
    }
    None
}

fn durable_list_entries(state: &DurableFixtureState, path: &str) -> Vec<OciFilesystemEntry> {
    let normalized = normalize_guest_path(path);
    let prefix = if normalized == "/" {
        "/".to_string()
    } else {
        format!("{normalized}/")
    };
    let mut entries = Vec::new();
    for directory in &state.directories {
        if let Some(rest) = directory.strip_prefix(&prefix) {
            if !rest.is_empty() && !rest.contains('/') {
                entries.push(durable_entry(
                    directory,
                    OciFilesystemEntryKind::Directory,
                    0,
                ));
            }
        }
    }
    for (file_path, encoded) in &state.files {
        if let Some(rest) = file_path.strip_prefix(&prefix) {
            if !rest.is_empty() && !rest.contains('/') {
                let size = STANDARD
                    .decode(encoded)
                    .map(|bytes| bytes.len() as u64)
                    .unwrap_or(0);
                entries.push(durable_entry(file_path, OciFilesystemEntryKind::File, size));
            }
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    entries
}

fn apply_filesystem_mutation(
    state: &mut DurableFixtureState,
    request: &OciFilesystemRequest,
) -> OciResult<()> {
    match request.op {
        OciFilesystemOp::MakeDir => {
            ensure_directory_tree(&mut state.directories, &request.path);
            Ok(())
        }
        OciFilesystemOp::Move => {
            let source = normalize_guest_path(&request.path);
            let destination = normalize_guest_path(request.destination.as_deref().unwrap_or(""));
            if destination.is_empty() || destination == "/" {
                return Err(oci_error(
                    ErrorCode::InvalidArgument,
                    "process-fixture-filesystem",
                    "durable move requires a destination path",
                ));
            }
            if let Some(payload) = state.files.remove(&source) {
                ensure_parent_directories(&mut state.directories, &destination);
                state.files.insert(destination, payload);
                return Ok(());
            }
            if state.directories.remove(&source) {
                ensure_directory_tree(&mut state.directories, &destination);
                let source_prefix = format!("{source}/");
                let destination_prefix = format!("{destination}/");
                let child_dirs: Vec<String> = state
                    .directories
                    .iter()
                    .filter(|path| path.starts_with(&source_prefix))
                    .cloned()
                    .collect();
                for child in child_dirs {
                    state.directories.remove(&child);
                    let suffix = child.trim_start_matches(&source_prefix);
                    state
                        .directories
                        .insert(format!("{destination_prefix}{suffix}"));
                }
                let child_files: Vec<(String, String)> = state
                    .files
                    .iter()
                    .filter(|(path, _)| path.starts_with(&source_prefix))
                    .map(|(path, payload)| (path.clone(), payload.clone()))
                    .collect();
                for (child, payload) in child_files {
                    state.files.remove(&child);
                    let suffix = child.trim_start_matches(&source_prefix);
                    state
                        .files
                        .insert(format!("{destination_prefix}{suffix}"), payload);
                }
                return Ok(());
            }
            Err(oci_error(
                ErrorCode::NotFound,
                "process-fixture-filesystem",
                format!("durable path {source} is absent"),
            ))
        }
        OciFilesystemOp::Remove => {
            let target = normalize_guest_path(&request.path);
            let prefix = format!("{target}/");
            state
                .files
                .retain(|path, _| path != &target && !path.starts_with(&prefix));
            state
                .directories
                .retain(|path| path != &target && !path.starts_with(&prefix));
            Ok(())
        }
        OciFilesystemOp::Stat | OciFilesystemOp::ListDir => unreachable!("read-only ops"),
    }
}

fn filesystem_mutation_response(request: &OciFilesystemRequest) -> OciFilesystemResponse {
    match request.op {
        OciFilesystemOp::MakeDir => OciFilesystemResponse {
            target: request.target.clone(),
            entry: Some(durable_entry(
                &normalize_guest_path(&request.path),
                OciFilesystemEntryKind::Directory,
                0,
            )),
            entries: Vec::new(),
        },
        OciFilesystemOp::Move => OciFilesystemResponse {
            target: request.target.clone(),
            entry: Some(durable_entry(
                &normalize_guest_path(request.destination.as_deref().unwrap_or_default()),
                OciFilesystemEntryKind::File,
                0,
            )),
            entries: Vec::new(),
        },
        OciFilesystemOp::Remove => OciFilesystemResponse {
            target: request.target.clone(),
            entry: None,
            entries: Vec::new(),
        },
        OciFilesystemOp::Stat | OciFilesystemOp::ListDir => {
            unreachable!("read-only ops use dedicated response builders")
        }
    }
}

struct RuntimeOwnerChild {
    child: Option<Child>,
    stderr_path: PathBuf,
}

impl RuntimeOwnerChild {
    fn spawn(
        child_test_name: &str,
        state_path: &Path,
        endpoint: &OsStr,
        ready_path: &Path,
        call_log: &Path,
        stderr_path: PathBuf,
    ) -> Self {
        let stderr = std::fs::File::create(&stderr_path).expect("create owner stderr file");
        let child = Command::new(std::env::current_exe().expect("resolve runtime test executable"))
            .args(["--exact", child_test_name, "--nocapture"])
            .env(CHILD_ENV, "1")
            .env(STATE_ENV, state_path)
            .env(ENDPOINT_ENV, endpoint)
            .env(READY_ENV, ready_path)
            .env(CALL_LOG_ENV, call_log)
            // The runtime owner is deliberately killed and replaced. Model the
            // independently living Sandbox init with the parent test process so
            // PID identity validation proves owner recovery instead of observing
            // the expected death of the transport process itself.
            .env(RUNTIME_RECORD_PID_ENV, std::process::id().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .expect("spawn Box runtime-owner fixture");
        Self {
            child: Some(child),
            stderr_path,
        }
    }

    fn wait_until_ready(&mut self, ready_path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if ready_path.is_file() {
                return;
            }
            if let Some(status) = self
                .child
                .as_mut()
                .expect("runtime owner child")
                .try_wait()
                .expect("inspect runtime owner child")
            {
                let stderr = std::fs::read_to_string(&self.stderr_path).unwrap_or_default();
                panic!("runtime owner exited before readiness ({status}): {stderr}");
            }
            assert!(
                Instant::now() < deadline,
                "runtime owner did not become ready: {}",
                std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn terminate(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = child.kill();
        child.wait().expect("reap Box runtime-owner fixture");
    }
}

impl Drop for RuntimeOwnerChild {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn required_path(name: &'static str) -> PathBuf {
    std::env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("missing runtime-owner fixture environment {name}"))
}

async fn run_child() {
    let state_path = required_path(STATE_ENV);
    let ready_path = required_path(READY_ENV);
    let call_log = required_path(CALL_LOG_ENV);
    let service: Arc<dyn OciRuntimeService> =
        Arc::new(DurableFixtureService::new(state_path, call_log));

    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ServerOptions;

        let pipe_name = std::env::var(ENDPOINT_ENV).expect("runtime-owner pipe environment");
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe_name)
            .expect("bind runtime-owner named pipe");
        std::fs::write(&ready_path, b"ready").expect("publish runtime-owner readiness");
        server
            .connect()
            .await
            .expect("accept Box named-pipe client");
        a3s_oci_sdk::serve_transport_connection(service, server)
            .await
            .expect("serve Box named-pipe connection");
    }

    #[cfg(unix)]
    {
        let socket_path = required_path(ENDPOINT_ENV);
        if socket_path.exists() {
            std::fs::remove_file(&socket_path).expect("remove stale runtime-owner socket");
        }
        let listener =
            tokio::net::UnixListener::bind(&socket_path).expect("bind runtime-owner Unix socket");
        std::fs::write(&ready_path, b"ready").expect("publish runtime-owner readiness");
        let (stream, _) = listener.accept().await.expect("accept Box Unix client");
        a3s_oci_sdk::serve_transport_connection(service, stream)
            .await
            .expect("serve Box Unix connection");
    }
}

#[cfg(windows)]
fn process_endpoint(_directory: &tempfile::TempDir) -> (OsString, OciRuntimeEndpoint) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_PIPE: AtomicU64 = AtomicU64::new(40_000);
    let name = format!(
        r"\\.\pipe\a3s-box-owner-restart-test-{}-{}",
        std::process::id(),
        NEXT_PIPE.fetch_add(1, Ordering::Relaxed)
    );
    let endpoint =
        OciRuntimeEndpoint::windows_named_pipe(name.clone()).expect("valid named-pipe endpoint");
    (OsString::from(name), endpoint)
}

#[cfg(unix)]
fn process_endpoint(directory: &tempfile::TempDir) -> (OsString, OciRuntimeEndpoint) {
    let path = directory.path().join("runtime-owner.sock");
    let endpoint = OciRuntimeEndpoint::unix_socket(path.clone()).expect("valid Unix endpoint");
    (path.into_os_string(), endpoint)
}

#[test]
fn process_restart_module_docs_refuse_b2_overclaim() {
    let docs = include_str!("process_restart.rs");
    assert!(
        docs.contains("must not be cited as closing ROADMAP B2"),
        "process_restart must keep an explicit anti-overfit B2 disclaimer"
    );
    assert!(
        docs.contains("b2_process_session_recovery_closed"),
        "process_restart must name the B2 gate that stays false"
    );
    assert!(
        docs.contains("linux-native-live-session-qualification"),
        "process_restart must point reviewers at the real-driver observation gate"
    );
    assert!(
        docs.contains(
            "retained_backend_recovers_filesystem_session_after_runtime_owner_process_restart"
        ),
        "process_restart must document the cross-process filesystem-session fixture"
    );
}

#[tokio::test]
async fn retained_backend_recovers_after_runtime_owner_process_restart() {
    if std::env::var_os(CHILD_ENV).is_some() {
        run_child().await;
        return;
    }

    let directory = tempfile::tempdir().expect("temporary directory");
    let state_path = directory.path().join("runtime-state.json");
    let call_log = directory.path().join("runtime-calls.log");
    let (endpoint_value, endpoint) = process_endpoint(&directory);
    let first_ready = directory.path().join("owner-1.ready");
    let mut first_owner = RuntimeOwnerChild::spawn(
        PROCESS_CHILD_TEST_NAME,
        &state_path,
        &endpoint_value,
        &first_ready,
        &call_log,
        directory.path().join("owner-1.stderr"),
    );
    first_owner.wait_until_ready(&first_ready);

    let provider = Arc::new(FakeBundleProvider::default());
    let backend = OciLocalExecutionBackend::connect(endpoint.clone(), provider.clone())
        .await
        .expect("connect Box backend to first runtime-owner process");
    let manager = LocalExecutionManager::new(
        directory.path().join("boxes.json"),
        directory.path().join("home"),
        Arc::new(backend),
    );
    let operation = box_operation("runtime-owner-process-restart-operation");
    let running = manager
        .create_and_start(
            request("runtime-owner-process-restart", fixture_isolation()),
            &operation,
        )
        .await
        .expect("initial launch through first runtime-owner process");
    let mut exec = box_exec_request(None);
    exec.streaming = true;
    exec.stdin_streaming = true;
    exec.timeout_ns = 30_000_000_000;
    let mut process = manager
        .start_process(&running.execution_id, running.generation, exec)
        .await
        .expect("start live process session through first runtime owner");
    let input = process.input();
    let first_event = process
        .next_event()
        .await
        .expect("read first process event")
        .expect("first process event");
    assert!(matches!(
        first_event,
        ExecEvent::Chunk(chunk)
            if chunk.stream == StreamType::Stdout
                && chunk.data == b"runtime owner session\n"
    ));
    input
        .write_stdin(b"before restart\n")
        .await
        .expect("write process stdin through first runtime owner");

    first_owner.terminate();
    let process_error = process
        .next_event()
        .await
        .expect_err("live process session must expose the observed owner disconnect");
    assert!(matches!(
        process_error,
        ExecutionManagerError::Unavailable(_)
    ));
    let error = manager
        .reconcile(&operation)
        .await
        .expect_err("the request that observes owner death must fail");
    assert!(matches!(error, ExecutionManagerError::Unavailable(_)));

    let second_ready = directory.path().join("owner-2.ready");
    let mut second_owner = RuntimeOwnerChild::spawn(
        PROCESS_CHILD_TEST_NAME,
        &state_path,
        &endpoint_value,
        &second_ready,
        &call_log,
        directory.path().join("owner-2.stderr"),
    );
    second_owner.wait_until_ready(&second_ready);
    let ReconcileOutcome::Ready(recovered) = manager
        .reconcile(&operation)
        .await
        .expect("retained Box backend must reconnect and reconcile")
    else {
        panic!("expected the process-restarted execution to remain ready")
    };
    assert_eq!(recovered.execution_id, running.execution_id);
    assert_eq!(recovered.generation, running.generation);
    assert_eq!(provider.prepares.load(Ordering::SeqCst), 1);
    let inventory = manager
        .list_processes(&running.execution_id, running.generation)
        .await
        .expect("recover live process inventory through replacement owner");
    assert_eq!(inventory.processes.len(), 2);
    assert!(inventory
        .processes
        .iter()
        .any(|candidate| candidate.process_id != "init"));
    input
        .write_stdin(b"after restart\n")
        .await
        .expect("continue process stdin through replacement owner");
    input
        .close_stdin()
        .await
        .expect("close recovered process stdin");
    input
        .send_signal(ExecutionProcessSignal::Kill)
        .await
        .expect("signal recovered process session");

    let mut resumed_output = Vec::new();
    let mut exit = None;
    while let Some(event) = process
        .next_event()
        .await
        .expect("continue recovered process stream")
    {
        match event {
            ExecEvent::Chunk(chunk) => resumed_output.extend_from_slice(&chunk.data),
            ExecEvent::Exit(status) => exit = Some(status),
            ExecEvent::FlushAck => {}
        }
    }
    assert_eq!(resumed_output, b"before restart\nafter restart\n");
    assert_eq!(exit.expect("recovered process exit").exit_code, 137);
    manager
        .kill(&running.execution_id, running.generation)
        .await
        .expect("clean up recovered container");

    let calls = std::fs::read_to_string(&call_log).expect("read runtime-owner call log");
    assert_eq!(calls.lines().filter(|call| *call == "create").count(), 1);
    assert_eq!(calls.lines().filter(|call| *call == "start").count(), 1);
    assert_eq!(calls.lines().filter(|call| *call == "exec").count(), 1);
    assert_eq!(
        calls.lines().filter(|call| *call == "write-stdin").count(),
        2
    );
    assert_eq!(
        calls.lines().filter(|call| *call == "close-stdin").count(),
        1
    );
    assert_eq!(
        calls
            .lines()
            .filter(|call| *call == "signal-process")
            .count(),
        1
    );
    assert_eq!(calls.lines().filter(|call| *call == "kill").count(), 1);
    assert_eq!(calls.lines().filter(|call| *call == "delete").count(), 1);
    assert!(
        !state_path.exists(),
        "container cleanup must remove durable process-session state"
    );

    second_owner.terminate();
    drop(manager);
}

#[tokio::test]
async fn retained_backend_recovers_filesystem_session_after_runtime_owner_process_restart() {
    if std::env::var_os(CHILD_ENV).is_some() {
        run_child().await;
        return;
    }

    let directory = tempfile::tempdir().expect("temporary directory");
    let state_path = directory.path().join("runtime-state.json");
    let call_log = directory.path().join("runtime-calls.log");
    let (endpoint_value, endpoint) = process_endpoint(&directory);
    let first_ready = directory.path().join("owner-1.ready");
    let mut first_owner = RuntimeOwnerChild::spawn(
        FILESYSTEM_CHILD_TEST_NAME,
        &state_path,
        &endpoint_value,
        &first_ready,
        &call_log,
        directory.path().join("owner-1.stderr"),
    );
    first_owner.wait_until_ready(&first_ready);

    let provider = Arc::new(FakeBundleProvider::default());
    let backend = OciLocalExecutionBackend::connect(endpoint.clone(), provider.clone())
        .await
        .expect("connect Box backend to first runtime-owner process");
    let manager = LocalExecutionManager::new(
        directory.path().join("boxes.json"),
        directory.path().join("home"),
        Arc::new(backend),
    );
    let operation = box_operation("runtime-owner-filesystem-restart-operation");
    let running = manager
        .create_and_start(
            request("runtime-owner-filesystem-restart", fixture_isolation()),
            &operation,
        )
        .await
        .expect("initial launch through first runtime-owner process");

    let created = manager
        .filesystem(
            &running.execution_id,
            running.generation,
            BoxFilesystemRequest {
                op: BoxFilesystemOp::MakeDir,
                path: "/work/tree".to_string(),
                destination: None,
                depth: 0,
                user: None,
                request_id: Some("fixture-fs-mkdir-before-owner-kill".to_string()),
            },
        )
        .await
        .expect("mkdir through first runtime owner");
    assert_eq!(
        created.entry.expect("created directory").kind,
        BoxFilesystemEntryKind::Directory
    );

    let payload = b"cross-process filesystem session\n";
    let upload = manager
        .transfer_file(
            &running.execution_id,
            running.generation,
            BoxFileRequest {
                op: BoxFileOp::Upload,
                guest_path: "/work/tree/payload.txt".to_string(),
                data: Some(STANDARD.encode(payload)),
                user: None,
                max_bytes: None,
                request_id: Some("fixture-file-upload-before-owner-kill".to_string()),
            },
        )
        .await
        .expect("upload through first runtime owner");
    assert!(upload.success);
    assert_eq!(upload.size, payload.len() as u64);

    first_owner.terminate();
    let error = manager
        .reconcile(&operation)
        .await
        .expect_err("the request that observes owner death must fail");
    assert!(matches!(error, ExecutionManagerError::Unavailable(_)));

    let second_ready = directory.path().join("owner-2.ready");
    let mut second_owner = RuntimeOwnerChild::spawn(
        FILESYSTEM_CHILD_TEST_NAME,
        &state_path,
        &endpoint_value,
        &second_ready,
        &call_log,
        directory.path().join("owner-2.stderr"),
    );
    second_owner.wait_until_ready(&second_ready);
    let ReconcileOutcome::Ready(recovered) = manager
        .reconcile(&operation)
        .await
        .expect("retained Box backend must reconnect and reconcile")
    else {
        panic!("expected the filesystem-restarted execution to remain ready")
    };
    assert_eq!(recovered.execution_id, running.execution_id);
    assert_eq!(recovered.generation, running.generation);
    assert_eq!(provider.prepares.load(Ordering::SeqCst), 1);

    let listing = manager
        .filesystem(
            &running.execution_id,
            running.generation,
            BoxFilesystemRequest {
                op: BoxFilesystemOp::ListDir,
                path: "/work/tree".to_string(),
                destination: None,
                depth: 0,
                user: None,
                request_id: None,
            },
        )
        .await
        .expect("list recovered directory through replacement owner");
    assert_eq!(listing.entries.len(), 1);
    assert_eq!(listing.entries[0].path, "/work/tree/payload.txt");
    assert_eq!(listing.entries[0].kind, BoxFilesystemEntryKind::File);

    let download = manager
        .transfer_file(
            &running.execution_id,
            running.generation,
            BoxFileRequest {
                op: BoxFileOp::Download,
                guest_path: "/work/tree/payload.txt".to_string(),
                data: None,
                user: None,
                max_bytes: None,
                request_id: None,
            },
        )
        .await
        .expect("download recovered file through replacement owner");
    assert!(download.success);
    assert_eq!(
        STANDARD
            .decode(download.data.expect("download payload"))
            .expect("valid download base64"),
        payload
    );

    manager
        .kill(&running.execution_id, running.generation)
        .await
        .expect("clean up recovered container");

    let calls = std::fs::read_to_string(&call_log).expect("read runtime-owner call log");
    assert_eq!(calls.lines().filter(|call| *call == "create").count(), 1);
    assert_eq!(calls.lines().filter(|call| *call == "start").count(), 1);
    assert_eq!(
        calls
            .lines()
            .filter(|call| *call == "filesystem-mutation")
            .count(),
        1
    );
    assert_eq!(
        calls.lines().filter(|call| *call == "file-upload").count(),
        1
    );
    assert_eq!(
        calls
            .lines()
            .filter(|call| *call == "filesystem-listdir")
            .count(),
        1
    );
    assert_eq!(
        calls
            .lines()
            .filter(|call| *call == "file-download")
            .count(),
        1
    );
    assert_eq!(calls.lines().filter(|call| *call == "kill").count(), 1);
    assert_eq!(calls.lines().filter(|call| *call == "delete").count(), 1);
    assert!(
        !state_path.exists(),
        "container cleanup must remove durable filesystem-session state"
    );

    second_owner.terminate();
    drop(manager);
}
