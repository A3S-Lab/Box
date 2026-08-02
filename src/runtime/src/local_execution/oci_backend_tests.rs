use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use a3s_box_core::exec::{ExecRequest as BoxExecRequest, StreamType};
use a3s_box_core::pty::PtyRequest;
use a3s_box_core::{
    BoxConfig, CreateExecutionRequest, ExecEvent, ExecutionEventsRequest, ExecutionGeneration,
    ExecutionId, ExecutionIsolation, ExecutionManager, ExecutionManagerError,
    ExecutionProcessSignal, ExecutionResourceUpdate, ExecutionSessionManager, ExecutionSnapshotId,
    ExecutionState, FileOp as BoxFileOp, FileRequest as BoxFileRequest,
    FilesystemEntryKind as BoxFilesystemEntryKind, FilesystemOp as BoxFilesystemOp,
    FilesystemRequest as BoxFilesystemRequest, KillExecutionOptions, KillOutcome, NetworkMode,
    OperationId as BoxOperationId, ReconcileOutcome,
};
use a3s_oci_sdk::oci_spec::runtime::{ContainerState, StateBuilder};
use a3s_oci_sdk::{
    async_trait, CloseStdinRequest, ContainerId, ContainerOperationRequest, ContainerRecord,
    ContainerStats, ContainerTarget, CpuStats, CreateRequest, DeleteMode, DeleteRequest,
    DriverKind, Error, ErrorCode, EventBatch, EventsRequest, ExecRequest as OciExecRequest,
    ExitStatus, FileOp as OciFileOp, FileRequest as OciFileRequest,
    FileResponse as OciFileResponse, FilesystemEntry as OciFilesystemEntry,
    FilesystemEntryKind as OciFilesystemEntryKind, FilesystemOp as OciFilesystemOp,
    FilesystemRequest as OciFilesystemRequest, FilesystemResponse as OciFilesystemResponse,
    Generation, IsolationClass, IsolationRequest, KillRequest, MemoryStats, OciBundle,
    OciRuntimeService, OutputChunk, OutputStream, ProcessId, ProcessRecord, ProcessTarget,
    ProcessesRequest, ReadOutputRequest, ResizeRequest, Result as OciResult, RuntimeClient,
    RuntimeEvent, RuntimeEventKind, RuntimeInfo, RuntimeOperation, SignalProcessRequest,
    StartRequest, StateRequest, StatsRequest, TerminalSize, UpdateRequest, WaitProcessRequest,
    WaitRequest, WriteStdinRequest, PAUSED_STATE_ANNOTATION,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::Utc;
use serde_json::json;

use super::super::{build_managed_record, LocalExecutionManager, RuntimeUpdate};
use super::*;
use crate::{ManagedExecutionState, ManagedExecutionStore};

const RUNTIME_GENERATION: Generation = Generation(41);
const CONFIG_DIGEST: &str =
    "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const ATTACHMENTS_DIGEST: &str =
    "sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

#[cfg(any(unix, windows))]
#[path = "oci_backend_tests/process_restart.rs"]
mod process_restart;

#[derive(Clone)]
struct FakeContainer {
    record: ContainerRecord,
    exit_status: Option<ExitStatus>,
}

#[derive(Clone)]
struct FakeProcess {
    request: OciExecRequest,
    record: ProcessRecord,
    output: Vec<OutputChunk>,
    exit_status: Option<ExitStatus>,
}

struct FakeRuntimeService {
    info: RuntimeInfo,
    containers: Mutex<HashMap<String, FakeContainer>>,
    processes: Mutex<HashMap<String, FakeProcess>>,
    create_requests: Mutex<Vec<CreateRequest>>,
    start_requests: Mutex<Vec<StartRequest>>,
    pause_requests: Mutex<Vec<ContainerOperationRequest>>,
    resume_requests: Mutex<Vec<ContainerOperationRequest>>,
    update_requests: Mutex<Vec<UpdateRequest>>,
    update_effects: Mutex<Vec<UpdateRequest>>,
    update_journal: Mutex<HashMap<String, UpdateRequest>>,
    processes_requests: Mutex<Vec<ProcessesRequest>>,
    stats_requests: Mutex<Vec<StatsRequest>>,
    events_requests: Mutex<Vec<EventsRequest>>,
    file_requests: Mutex<Vec<OciFileRequest>>,
    file_effects: Mutex<Vec<OciFileRequest>>,
    file_journal: Mutex<HashMap<String, OciFileRequest>>,
    filesystem_requests: Mutex<Vec<OciFilesystemRequest>>,
    filesystem_effects: Mutex<Vec<OciFilesystemRequest>>,
    filesystem_journal: Mutex<HashMap<String, OciFilesystemRequest>>,
    exec_requests: Mutex<Vec<OciExecRequest>>,
    output_requests: Mutex<Vec<ReadOutputRequest>>,
    stdin_requests: Mutex<Vec<WriteStdinRequest>>,
    stdin_effects: Mutex<Vec<WriteStdinRequest>>,
    stdin_journal: Mutex<HashMap<String, WriteStdinRequest>>,
    close_stdin_requests: Mutex<Vec<CloseStdinRequest>>,
    resize_requests: Mutex<Vec<ResizeRequest>>,
    signal_process_requests: Mutex<Vec<SignalProcessRequest>>,
    wait_process_requests: Mutex<Vec<WaitProcessRequest>>,
    kill_signals: Mutex<Vec<i32>>,
    delete_modes: Mutex<Vec<DeleteMode>>,
    create_digest_override: Mutex<Option<String>>,
    create_attachments_digest_override: Mutex<Option<String>>,
    fail_create_after_effect: AtomicBool,
    fail_start_after_effect: AtomicBool,
    fail_pause_before_effect: AtomicBool,
    fail_pause_after_effect: AtomicBool,
    fail_resume_after_effect: AtomicBool,
    fail_update_after_effect: AtomicBool,
    fail_exec_after_effect: AtomicBool,
    fail_stdin_after_effect: AtomicBool,
    fail_file_after_effect: AtomicBool,
    fail_filesystem_after_effect: AtomicBool,
    hold_next_process: AtomicBool,
    ignore_graceful_signal: AtomicBool,
    drift_process_target: AtomicBool,
    drift_stats_target: AtomicBool,
    drift_file_target: AtomicBool,
    drift_filesystem_target: AtomicBool,
    misorder_events: AtomicBool,
}

impl FakeRuntimeService {
    fn launch_ready() -> Self {
        Self::with_dedicated_readiness("experimental")
    }

    fn probe_only() -> Self {
        Self::with_dedicated_readiness("probe-only")
    }

    fn without_attachment_schema() -> Self {
        let mut service = Self::launch_ready();
        let mut info = serde_json::to_value(&service.info).expect("encode runtime info");
        info["attachments"]["schemas"] = json!([]);
        service.info = serde_json::from_value(info).expect("decode runtime info");
        service
    }

    fn without_operation(operation: RuntimeOperation) -> Self {
        let mut service = Self::launch_ready();
        service.info.operations.retain(|item| *item != operation);
        service
    }

    fn with_dedicated_readiness(readiness: &str) -> Self {
        Self {
            info: runtime_info(readiness),
            containers: Mutex::new(HashMap::new()),
            processes: Mutex::new(HashMap::new()),
            create_requests: Mutex::new(Vec::new()),
            start_requests: Mutex::new(Vec::new()),
            pause_requests: Mutex::new(Vec::new()),
            resume_requests: Mutex::new(Vec::new()),
            update_requests: Mutex::new(Vec::new()),
            update_effects: Mutex::new(Vec::new()),
            update_journal: Mutex::new(HashMap::new()),
            processes_requests: Mutex::new(Vec::new()),
            stats_requests: Mutex::new(Vec::new()),
            events_requests: Mutex::new(Vec::new()),
            file_requests: Mutex::new(Vec::new()),
            file_effects: Mutex::new(Vec::new()),
            file_journal: Mutex::new(HashMap::new()),
            filesystem_requests: Mutex::new(Vec::new()),
            filesystem_effects: Mutex::new(Vec::new()),
            filesystem_journal: Mutex::new(HashMap::new()),
            exec_requests: Mutex::new(Vec::new()),
            output_requests: Mutex::new(Vec::new()),
            stdin_requests: Mutex::new(Vec::new()),
            stdin_effects: Mutex::new(Vec::new()),
            stdin_journal: Mutex::new(HashMap::new()),
            close_stdin_requests: Mutex::new(Vec::new()),
            resize_requests: Mutex::new(Vec::new()),
            signal_process_requests: Mutex::new(Vec::new()),
            wait_process_requests: Mutex::new(Vec::new()),
            kill_signals: Mutex::new(Vec::new()),
            delete_modes: Mutex::new(Vec::new()),
            create_digest_override: Mutex::new(None),
            create_attachments_digest_override: Mutex::new(None),
            fail_create_after_effect: AtomicBool::new(false),
            fail_start_after_effect: AtomicBool::new(false),
            fail_pause_before_effect: AtomicBool::new(false),
            fail_pause_after_effect: AtomicBool::new(false),
            fail_resume_after_effect: AtomicBool::new(false),
            fail_update_after_effect: AtomicBool::new(false),
            fail_exec_after_effect: AtomicBool::new(false),
            fail_stdin_after_effect: AtomicBool::new(false),
            fail_file_after_effect: AtomicBool::new(false),
            fail_filesystem_after_effect: AtomicBool::new(false),
            hold_next_process: AtomicBool::new(false),
            ignore_graceful_signal: AtomicBool::new(false),
            drift_process_target: AtomicBool::new(false),
            drift_stats_target: AtomicBool::new(false),
            drift_file_target: AtomicBool::new(false),
            drift_filesystem_target: AtomicBool::new(false),
            misorder_events: AtomicBool::new(false),
        }
    }

    fn seed(
        &self,
        execution_id: &ExecutionId,
        isolation: ExecutionIsolation,
        status: ContainerState,
    ) {
        let id = runtime_container_id(execution_id).expect("runtime container ID");
        let (driver, isolation) = selected_driver(oci_isolation_request(isolation));
        let record = runtime_record(
            &id,
            RUNTIME_GENERATION,
            status,
            driver,
            isolation,
            CONFIG_DIGEST,
            Some(ATTACHMENTS_DIGEST),
        )
        .expect("seed runtime record");
        self.containers.lock().expect("container lock").insert(
            id.to_string(),
            FakeContainer {
                record,
                exit_status: None,
            },
        );
    }

    fn mark_stopped(&self, execution_id: &ExecutionId, status: ExitStatus) {
        let id = runtime_container_id(execution_id).expect("runtime container ID");
        let mut containers = self.containers.lock().expect("container lock");
        let container = containers.get_mut(id.as_str()).expect("runtime exists");
        container.record = runtime_record(
            &id,
            container.record.generation,
            ContainerState::Stopped,
            container.record.driver,
            container.record.isolation,
            &container.record.config_digest,
            container.record.attachments_digest.as_deref(),
        )
        .expect("stopped runtime record");
        container.exit_status = Some(status);
    }

    fn create_requests(&self) -> Vec<CreateRequest> {
        self.create_requests.lock().expect("create lock").clone()
    }

    fn start_requests(&self) -> Vec<StartRequest> {
        self.start_requests.lock().expect("start lock").clone()
    }

    fn pause_requests(&self) -> Vec<ContainerOperationRequest> {
        self.pause_requests.lock().expect("pause lock").clone()
    }

    fn resume_requests(&self) -> Vec<ContainerOperationRequest> {
        self.resume_requests.lock().expect("resume lock").clone()
    }

    fn update_requests(&self) -> Vec<UpdateRequest> {
        self.update_requests.lock().expect("update lock").clone()
    }

    fn update_effects(&self) -> Vec<UpdateRequest> {
        self.update_effects
            .lock()
            .expect("update effect lock")
            .clone()
    }

    fn processes_requests(&self) -> Vec<ProcessesRequest> {
        self.processes_requests
            .lock()
            .expect("processes lock")
            .clone()
    }

    fn stats_requests(&self) -> Vec<StatsRequest> {
        self.stats_requests.lock().expect("stats lock").clone()
    }

    fn events_requests(&self) -> Vec<EventsRequest> {
        self.events_requests.lock().expect("events lock").clone()
    }

    fn file_requests(&self) -> Vec<OciFileRequest> {
        self.file_requests.lock().expect("file lock").clone()
    }

    fn file_effects(&self) -> Vec<OciFileRequest> {
        self.file_effects.lock().expect("file effect lock").clone()
    }

    fn filesystem_requests(&self) -> Vec<OciFilesystemRequest> {
        self.filesystem_requests
            .lock()
            .expect("filesystem lock")
            .clone()
    }

    fn filesystem_effects(&self) -> Vec<OciFilesystemRequest> {
        self.filesystem_effects
            .lock()
            .expect("filesystem effect lock")
            .clone()
    }

    fn exec_requests(&self) -> Vec<OciExecRequest> {
        self.exec_requests.lock().expect("exec lock").clone()
    }

    fn stdin_requests(&self) -> Vec<WriteStdinRequest> {
        self.stdin_requests.lock().expect("stdin lock").clone()
    }

    fn stdin_effects(&self) -> Vec<WriteStdinRequest> {
        self.stdin_effects
            .lock()
            .expect("stdin effect lock")
            .clone()
    }

    fn close_stdin_requests(&self) -> Vec<CloseStdinRequest> {
        self.close_stdin_requests
            .lock()
            .expect("close stdin lock")
            .clone()
    }

    fn resize_requests(&self) -> Vec<ResizeRequest> {
        self.resize_requests.lock().expect("resize lock").clone()
    }

    fn signal_process_requests(&self) -> Vec<SignalProcessRequest> {
        self.signal_process_requests
            .lock()
            .expect("signal process lock")
            .clone()
    }

    fn wait_process_requests(&self) -> Vec<WaitProcessRequest> {
        self.wait_process_requests
            .lock()
            .expect("wait process lock")
            .clone()
    }

    fn require_fake_process(&self, target: &ProcessTarget, operation: &str) -> OciResult<()> {
        let processes = self
            .processes
            .lock()
            .map_err(|error| lock_error(operation, error))?;
        let process = processes
            .get(target.process_id.as_str())
            .ok_or_else(|| oci_error(ErrorCode::NotFound, operation, "fake process is absent"))?;
        validate_process_target(target, &process.record, operation)
    }

    fn require_running_container(
        &self,
        target: &ContainerTarget,
        operation: &str,
    ) -> OciResult<()> {
        let containers = self
            .containers
            .lock()
            .map_err(|error| lock_error(operation, error))?;
        let container = containers
            .get(target.id.as_str())
            .ok_or_else(|| oci_error(ErrorCode::NotFound, operation, "fake runtime is absent"))?;
        validate_target(target, &container.record, operation)?;
        if *container.record.state.status() != ContainerState::Running {
            return Err(oci_error(
                ErrorCode::FailedPrecondition,
                operation,
                "fake runtime is not running",
            ));
        }
        Ok(())
    }

    fn kill_signals(&self) -> Vec<i32> {
        self.kill_signals.lock().expect("kill lock").clone()
    }

    fn delete_modes(&self) -> Vec<DeleteMode> {
        self.delete_modes.lock().expect("delete lock").clone()
    }

    fn container_count(&self) -> usize {
        self.containers.lock().expect("container lock").len()
    }
}

#[async_trait]
impl OciRuntimeService for FakeRuntimeService {
    async fn features(&self) -> OciResult<RuntimeInfo> {
        Ok(self.info.clone())
    }

    async fn create(&self, request: CreateRequest) -> OciResult<ContainerRecord> {
        self.create_requests
            .lock()
            .map_err(|error| lock_error("create", error))?
            .push(request.clone());
        let (driver, isolation) = selected_driver(request.isolation.clone());
        let digest_override = self
            .create_digest_override
            .lock()
            .map_err(|error| lock_error("create", error))?
            .clone();
        let attachments_digest = self
            .create_attachments_digest_override
            .lock()
            .map_err(|error| lock_error("create", error))?
            .clone()
            .unwrap_or(request.attachments.digest()?);
        let record = runtime_record(
            &request.id,
            RUNTIME_GENERATION,
            ContainerState::Created,
            driver,
            isolation,
            digest_override
                .as_deref()
                .unwrap_or_else(|| request.bundle.config_digest()),
            Some(&attachments_digest),
        )?;
        let mut containers = self
            .containers
            .lock()
            .map_err(|error| lock_error("create", error))?;
        if containers.contains_key(request.id.as_str()) {
            return Err(oci_error(
                ErrorCode::AlreadyExists,
                "create",
                "fake runtime ID already exists",
            ));
        }
        containers.insert(
            request.id.to_string(),
            FakeContainer {
                record: record.clone(),
                exit_status: None,
            },
        );
        drop(containers);
        if self.fail_create_after_effect.swap(false, Ordering::SeqCst) {
            return Err(
                Error::new(ErrorCode::Unavailable, "fake create response was lost")
                    .for_operation("create")
                    .retryable(true),
            );
        }
        Ok(record)
    }

    async fn state(&self, request: StateRequest) -> OciResult<ContainerRecord> {
        let containers = self
            .containers
            .lock()
            .map_err(|error| lock_error("state", error))?;
        let container = containers
            .get(request.target.id.as_str())
            .ok_or_else(|| oci_error(ErrorCode::NotFound, "state", "fake runtime is absent"))?;
        validate_target(&request.target, &container.record, "state")?;
        Ok(container.record.clone())
    }

    async fn start(&self, request: StartRequest) -> OciResult<ContainerRecord> {
        self.start_requests
            .lock()
            .map_err(|error| lock_error("start", error))?
            .push(request.clone());
        let mut containers = self
            .containers
            .lock()
            .map_err(|error| lock_error("start", error))?;
        let container = containers
            .get_mut(request.target.id.as_str())
            .ok_or_else(|| oci_error(ErrorCode::NotFound, "start", "fake runtime is absent"))?;
        validate_target(&request.target, &container.record, "start")?;
        match *container.record.state.status() {
            ContainerState::Created => {
                container.record = runtime_record(
                    &request.target.id,
                    container.record.generation,
                    ContainerState::Running,
                    container.record.driver,
                    container.record.isolation,
                    &container.record.config_digest,
                    container.record.attachments_digest.as_deref(),
                )?;
            }
            ContainerState::Running => {}
            status => {
                return Err(oci_error(
                    ErrorCode::FailedPrecondition,
                    "start",
                    format!("fake runtime cannot start from {status:?}"),
                ))
            }
        }
        let record = container.record.clone();
        drop(containers);
        if self.fail_start_after_effect.swap(false, Ordering::SeqCst) {
            return Err(
                Error::new(ErrorCode::Unavailable, "fake start response was lost")
                    .for_operation("start")
                    .retryable(true),
            );
        }
        Ok(record)
    }

    async fn pause(&self, request: ContainerOperationRequest) -> OciResult<ContainerRecord> {
        self.pause_requests
            .lock()
            .map_err(|error| lock_error("pause", error))?
            .push(request.clone());
        if self.fail_pause_before_effect.swap(false, Ordering::SeqCst) {
            return Err(
                Error::new(ErrorCode::Unavailable, "fake pause failed before mutation")
                    .for_operation("pause")
                    .retryable(false),
            );
        }
        let mut containers = self
            .containers
            .lock()
            .map_err(|error| lock_error("pause", error))?;
        let container = containers
            .get_mut(request.target.id.as_str())
            .ok_or_else(|| oci_error(ErrorCode::NotFound, "pause", "fake runtime is absent"))?;
        validate_target(&request.target, &container.record, "pause")?;
        if *container.record.state.status() != ContainerState::Running {
            return Err(oci_error(
                ErrorCode::FailedPrecondition,
                "pause",
                "fake runtime is not running",
            ));
        }
        container.record = rebuild_paused_record(&container.record, true)?;
        let record = container.record.clone();
        drop(containers);
        if self.fail_pause_after_effect.swap(false, Ordering::SeqCst) {
            return Err(
                Error::new(ErrorCode::Unavailable, "fake pause response was lost")
                    .for_operation("pause")
                    .retryable(true),
            );
        }
        Ok(record)
    }

    async fn resume(&self, request: ContainerOperationRequest) -> OciResult<ContainerRecord> {
        self.resume_requests
            .lock()
            .map_err(|error| lock_error("resume", error))?
            .push(request.clone());
        let mut containers = self
            .containers
            .lock()
            .map_err(|error| lock_error("resume", error))?;
        let container = containers
            .get_mut(request.target.id.as_str())
            .ok_or_else(|| oci_error(ErrorCode::NotFound, "resume", "fake runtime is absent"))?;
        validate_target(&request.target, &container.record, "resume")?;
        if *container.record.state.status() != ContainerState::Running {
            return Err(oci_error(
                ErrorCode::FailedPrecondition,
                "resume",
                "fake runtime is not running",
            ));
        }
        container.record = rebuild_paused_record(&container.record, false)?;
        let record = container.record.clone();
        drop(containers);
        if self.fail_resume_after_effect.swap(false, Ordering::SeqCst) {
            return Err(
                Error::new(ErrorCode::Unavailable, "fake resume response was lost")
                    .for_operation("resume")
                    .retryable(true),
            );
        }
        Ok(record)
    }

    async fn update(&self, request: UpdateRequest) -> OciResult<ContainerRecord> {
        self.update_requests
            .lock()
            .map_err(|error| lock_error("update", error))?
            .push(request.clone());
        let record = {
            let containers = self
                .containers
                .lock()
                .map_err(|error| lock_error("update", error))?;
            let container = containers.get(request.target.id.as_str()).ok_or_else(|| {
                oci_error(ErrorCode::NotFound, "update", "fake runtime is absent")
            })?;
            validate_target(&request.target, &container.record, "update")?;
            if *container.record.state.status() != ContainerState::Running
                || container.record.is_paused()
            {
                return Err(oci_error(
                    ErrorCode::FailedPrecondition,
                    "update",
                    "fake runtime is not running",
                ));
            }
            container.record.clone()
        };
        let operation_id = request.context.operation_id.to_string();
        let mut journal = self
            .update_journal
            .lock()
            .map_err(|error| lock_error("update", error))?;
        if let Some(previous) = journal.get(&operation_id) {
            if previous != &request {
                return Err(oci_error(
                    ErrorCode::Conflict,
                    "update",
                    "operation identity was reused with different resources",
                ));
            }
            return Ok(record);
        }
        journal.insert(operation_id, request.clone());
        self.update_effects
            .lock()
            .map_err(|error| lock_error("update", error))?
            .push(request);
        drop(journal);
        if self.fail_update_after_effect.swap(false, Ordering::SeqCst) {
            return Err(
                Error::new(ErrorCode::Unavailable, "fake update response was lost")
                    .for_operation("update")
                    .retryable(true),
            );
        }
        Ok(record)
    }

    async fn processes(&self, request: ProcessesRequest) -> OciResult<Vec<ProcessRecord>> {
        self.processes_requests
            .lock()
            .map_err(|error| lock_error("processes", error))?
            .push(request.clone());
        {
            let containers = self
                .containers
                .lock()
                .map_err(|error| lock_error("processes", error))?;
            let container = containers.get(request.target.id.as_str()).ok_or_else(|| {
                oci_error(ErrorCode::NotFound, "processes", "fake runtime is absent")
            })?;
            validate_target(&request.target, &container.record, "processes")?;
        }
        let target = if self.drift_process_target.load(Ordering::SeqCst) {
            ContainerTarget::exact(
                ContainerId::new("a3s-box-drift").expect("drift container ID"),
                Generation(99),
            )
        } else {
            request.target.clone()
        };
        let mut records = vec![ProcessRecord {
            target: ProcessTarget {
                container: target,
                process_id: ProcessId::init(),
            },
            pid: Some(4_242),
            terminal: false,
        }];
        records.extend(
            self.processes
                .lock()
                .map_err(|error| lock_error("processes", error))?
                .values()
                .filter(|process| process.exit_status.is_none())
                .map(|process| process.record.clone()),
        );
        Ok(records)
    }

    async fn stats(&self, request: StatsRequest) -> OciResult<ContainerStats> {
        self.stats_requests
            .lock()
            .map_err(|error| lock_error("stats", error))?
            .push(request.clone());
        {
            let containers = self
                .containers
                .lock()
                .map_err(|error| lock_error("stats", error))?;
            let container = containers
                .get(request.target.id.as_str())
                .ok_or_else(|| oci_error(ErrorCode::NotFound, "stats", "fake runtime is absent"))?;
            validate_target(&request.target, &container.record, "stats")?;
        }
        let target = if self.drift_stats_target.load(Ordering::SeqCst) {
            ContainerTarget::exact(
                ContainerId::new("a3s-box-drift").expect("drift container ID"),
                Generation(99),
            )
        } else {
            request.target
        };
        Ok(ContainerStats {
            target,
            timestamp_unix_ns: 1_700_000_000_000_000_000,
            cpu: CpuStats {
                usage_ns: 100,
                user_ns: 60,
                system_ns: 30,
                throttled_ns: 5,
            },
            memory: MemoryStats {
                usage_bytes: 64 * 1024 * 1024,
                limit_bytes: Some(128 * 1024 * 1024),
                peak_bytes: Some(72 * 1024 * 1024),
            },
            process_count: 1,
            metrics: BTreeMap::from([("io.read_bytes".to_string(), 4096)]),
        })
    }

    async fn events(&self, request: EventsRequest) -> OciResult<EventBatch> {
        self.events_requests
            .lock()
            .map_err(|error| lock_error("events", error))?
            .push(request.clone());
        let target = request.container.clone().ok_or_else(|| {
            oci_error(
                ErrorCode::InvalidArgument,
                "events",
                "fake events require a container filter",
            )
        })?;
        {
            let containers = self
                .containers
                .lock()
                .map_err(|error| lock_error("events", error))?;
            let container = containers.get(target.id.as_str()).ok_or_else(|| {
                oci_error(ErrorCode::NotFound, "events", "fake runtime is absent")
            })?;
            validate_target(&target, &container.record, "events")?;
        }
        let sequences = if self.misorder_events.load(Ordering::SeqCst) {
            vec![8, 7]
        } else {
            vec![5, 8]
        };
        let events = sequences
            .into_iter()
            .filter(|sequence| *sequence > request.after_sequence)
            .take(request.limit as usize)
            .map(|sequence| RuntimeEvent {
                sequence,
                timestamp_unix_ns: 1_700_000_000_000_000_000 + sequence,
                container: target.clone(),
                process_id: (sequence == 8).then(ProcessId::init),
                kind: if sequence == 5 {
                    RuntimeEventKind::ContainerStarted
                } else {
                    RuntimeEventKind::ProcessStarted
                },
                attributes: BTreeMap::new(),
            })
            .collect();
        Ok(EventBatch {
            events,
            next_sequence: 8_u64.max(request.after_sequence),
        })
    }

    async fn file(&self, request: OciFileRequest) -> OciResult<OciFileResponse> {
        self.file_requests
            .lock()
            .map_err(|error| lock_error("file", error))?
            .push(request.clone());
        self.require_running_container(&request.target, "file")?;
        let response_target = if self.drift_file_target.load(Ordering::SeqCst) {
            ContainerTarget::exact(
                ContainerId::new("a3s-box-file-drift").expect("file drift container ID"),
                Generation(99),
            )
        } else {
            request.target.clone()
        };
        let response = match request.op {
            OciFileOp::Upload => {
                let size = STANDARD
                    .decode(request.data.as_deref().unwrap_or_default())
                    .map_err(|error| {
                        oci_error(
                            ErrorCode::InvalidArgument,
                            "file",
                            format!("invalid base64 upload: {error}"),
                        )
                    })?
                    .len() as u64;
                OciFileResponse {
                    target: response_target,
                    data: None,
                    size,
                }
            }
            OciFileOp::Download => {
                let payload = b"fake file\n";
                OciFileResponse {
                    target: response_target,
                    data: Some(STANDARD.encode(payload)),
                    size: payload.len() as u64,
                }
            }
        };
        if request.op == OciFileOp::Upload {
            let operation_id = request
                .context
                .as_ref()
                .ok_or_else(|| {
                    oci_error(
                        ErrorCode::InvalidArgument,
                        "file",
                        "fake upload requires an operation context",
                    )
                })?
                .operation_id
                .to_string();
            let mut journal = self
                .file_journal
                .lock()
                .map_err(|error| lock_error("file", error))?;
            if let Some(previous) = journal.get(&operation_id) {
                if previous != &request {
                    return Err(oci_error(
                        ErrorCode::Conflict,
                        "file",
                        "operation identity was reused with a different upload",
                    ));
                }
                return Ok(response);
            }
            journal.insert(operation_id, request.clone());
            self.file_effects
                .lock()
                .map_err(|error| lock_error("file", error))?
                .push(request);
            drop(journal);
            if self.fail_file_after_effect.swap(false, Ordering::SeqCst) {
                return Err(
                    Error::new(ErrorCode::Unavailable, "fake file response was lost")
                        .for_operation("file")
                        .retryable(true),
                );
            }
        }
        Ok(response)
    }

    async fn filesystem(&self, request: OciFilesystemRequest) -> OciResult<OciFilesystemResponse> {
        self.filesystem_requests
            .lock()
            .map_err(|error| lock_error("filesystem", error))?
            .push(request.clone());
        self.require_running_container(&request.target, "filesystem")?;
        let response_target = if self.drift_filesystem_target.load(Ordering::SeqCst) {
            ContainerTarget::exact(
                ContainerId::new("a3s-box-filesystem-drift")
                    .expect("filesystem drift container ID"),
                Generation(99),
            )
        } else {
            request.target.clone()
        };
        let response = match request.op {
            OciFilesystemOp::Stat => OciFilesystemResponse {
                target: response_target,
                entry: Some(fake_filesystem_entry(
                    &request.path,
                    OciFilesystemEntryKind::File,
                )),
                entries: Vec::new(),
            },
            OciFilesystemOp::MakeDir => OciFilesystemResponse {
                target: response_target,
                entry: Some(fake_filesystem_entry(
                    &request.path,
                    OciFilesystemEntryKind::Directory,
                )),
                entries: Vec::new(),
            },
            OciFilesystemOp::Move => OciFilesystemResponse {
                target: response_target,
                entry: Some(fake_filesystem_entry(
                    request.destination.as_deref().unwrap_or_default(),
                    OciFilesystemEntryKind::File,
                )),
                entries: Vec::new(),
            },
            OciFilesystemOp::ListDir => OciFilesystemResponse {
                target: response_target,
                entry: None,
                entries: vec![fake_filesystem_entry(
                    &format!("{}/fixture.txt", request.path.trim_end_matches('/')),
                    OciFilesystemEntryKind::File,
                )],
            },
            OciFilesystemOp::Remove => OciFilesystemResponse {
                target: response_target,
                entry: None,
                entries: Vec::new(),
            },
        };
        if request.op.is_mutating() {
            let operation_id = request
                .context
                .as_ref()
                .ok_or_else(|| {
                    oci_error(
                        ErrorCode::InvalidArgument,
                        "filesystem",
                        "fake mutation requires an operation context",
                    )
                })?
                .operation_id
                .to_string();
            let mut journal = self
                .filesystem_journal
                .lock()
                .map_err(|error| lock_error("filesystem", error))?;
            if let Some(previous) = journal.get(&operation_id) {
                if previous != &request {
                    return Err(oci_error(
                        ErrorCode::Conflict,
                        "filesystem",
                        "operation identity was reused with a different mutation",
                    ));
                }
                return Ok(response);
            }
            journal.insert(operation_id, request.clone());
            self.filesystem_effects
                .lock()
                .map_err(|error| lock_error("filesystem", error))?
                .push(request);
            drop(journal);
            if self
                .fail_filesystem_after_effect
                .swap(false, Ordering::SeqCst)
            {
                return Err(Error::new(
                    ErrorCode::Unavailable,
                    "fake filesystem response was lost",
                )
                .for_operation("filesystem")
                .retryable(true));
            }
        }
        Ok(response)
    }

    async fn kill(&self, request: KillRequest) -> OciResult<ContainerRecord> {
        self.kill_signals
            .lock()
            .map_err(|error| lock_error("kill", error))?
            .push(request.signal.get());
        let mut containers = self
            .containers
            .lock()
            .map_err(|error| lock_error("kill", error))?;
        let container = containers
            .get_mut(request.target.id.as_str())
            .ok_or_else(|| oci_error(ErrorCode::NotFound, "kill", "fake runtime is absent"))?;
        validate_target(&request.target, &container.record, "kill")?;
        if *container.record.state.status() != ContainerState::Stopped
            && !(self.ignore_graceful_signal.load(Ordering::SeqCst)
                && request.signal.get() != DEFAULT_KILL_SIGNAL)
        {
            container.record = runtime_record(
                &request.target.id,
                container.record.generation,
                ContainerState::Stopped,
                container.record.driver,
                container.record.isolation,
                &container.record.config_digest,
                container.record.attachments_digest.as_deref(),
            )?;
            container.exit_status = Some(ExitStatus::signaled(request.signal.get(), false)?);
        }
        Ok(container.record.clone())
    }

    async fn delete(&self, request: DeleteRequest) -> OciResult<()> {
        self.delete_modes
            .lock()
            .map_err(|error| lock_error("delete", error))?
            .push(request.mode);
        let mut containers = self
            .containers
            .lock()
            .map_err(|error| lock_error("delete", error))?;
        let container = containers
            .get(request.target.id.as_str())
            .ok_or_else(|| oci_error(ErrorCode::NotFound, "delete", "fake runtime is absent"))?;
        validate_target(&request.target, &container.record, "delete")?;
        if request.mode == DeleteMode::StoppedOnly
            && *container.record.state.status() != ContainerState::Stopped
        {
            return Err(oci_error(
                ErrorCode::FailedPrecondition,
                "delete",
                "fake runtime is still running",
            ));
        }
        containers.remove(request.target.id.as_str());
        Ok(())
    }

    async fn exec(&self, request: OciExecRequest) -> OciResult<ProcessRecord> {
        self.exec_requests
            .lock()
            .map_err(|error| lock_error("exec", error))?
            .push(request.clone());
        {
            let containers = self
                .containers
                .lock()
                .map_err(|error| lock_error("exec", error))?;
            let container = containers
                .get(request.container.id.as_str())
                .ok_or_else(|| oci_error(ErrorCode::NotFound, "exec", "container is absent"))?;
            validate_target(&request.container, &container.record, "exec")?;
            if *container.record.state.status() != ContainerState::Running {
                return Err(oci_error(
                    ErrorCode::FailedPrecondition,
                    "exec",
                    "container is not running",
                ));
            }
        }

        let key = request.process_id.to_string();
        let mut processes = self
            .processes
            .lock()
            .map_err(|error| lock_error("exec", error))?;
        if let Some(process) = processes.get(&key) {
            if process.request != request {
                return Err(oci_error(
                    ErrorCode::Conflict,
                    "exec",
                    "process identity was reused with a different request",
                ));
            }
            return Ok(process.record.clone());
        }

        let held = self.hold_next_process.swap(false, Ordering::SeqCst);
        let terminal = request.process.terminal().unwrap_or(false);
        let process = FakeProcess {
            request: request.clone(),
            record: ProcessRecord {
                target: ProcessTarget {
                    container: request.container.clone(),
                    process_id: request.process_id.clone(),
                },
                pid: Some(9_000 + processes.len() as u32),
                terminal,
            },
            output: fake_process_output(terminal, held),
            exit_status: if held {
                None
            } else {
                Some(ExitStatus::exited(23)?)
            },
        };
        let record = process.record.clone();
        processes.insert(key, process);
        drop(processes);
        if self.fail_exec_after_effect.swap(false, Ordering::SeqCst) {
            return Err(
                Error::new(ErrorCode::Unavailable, "fake exec response was lost")
                    .for_operation("exec")
                    .retryable(true),
            );
        }
        Ok(record)
    }

    async fn read_output(&self, request: ReadOutputRequest) -> OciResult<Vec<OutputChunk>> {
        self.output_requests
            .lock()
            .map_err(|error| lock_error("read-output", error))?
            .push(request.clone());
        let processes = self
            .processes
            .lock()
            .map_err(|error| lock_error("read-output", error))?;
        let process = processes
            .get(request.process.process_id.as_str())
            .ok_or_else(|| oci_error(ErrorCode::NotFound, "read-output", "process is absent"))?;
        validate_process_target(&request.process, &process.record, "read-output")?;
        let mut bytes = 0_u64;
        Ok(process
            .output
            .iter()
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
            .cloned()
            .collect())
    }

    async fn write_stdin(&self, request: WriteStdinRequest) -> OciResult<()> {
        self.stdin_requests
            .lock()
            .map_err(|error| lock_error("write-stdin", error))?
            .push(request.clone());
        self.require_fake_process(&request.process, "write-stdin")?;
        let operation_id = request.context.operation_id.to_string();
        let mut journal = self
            .stdin_journal
            .lock()
            .map_err(|error| lock_error("write-stdin", error))?;
        if let Some(previous) = journal.get(&operation_id) {
            if previous != &request {
                return Err(oci_error(
                    ErrorCode::Conflict,
                    "write-stdin",
                    "operation identity was reused with a different payload",
                ));
            }
            return Ok(());
        }
        journal.insert(operation_id, request.clone());
        self.stdin_effects
            .lock()
            .map_err(|error| lock_error("write-stdin", error))?
            .push(request);
        drop(journal);
        if self.fail_stdin_after_effect.swap(false, Ordering::SeqCst) {
            return Err(
                Error::new(ErrorCode::Unavailable, "fake stdin response was lost")
                    .for_operation("write-stdin")
                    .retryable(true),
            );
        }
        Ok(())
    }

    async fn close_stdin(&self, request: CloseStdinRequest) -> OciResult<()> {
        self.require_fake_process(&request.process, "close-stdin")?;
        self.close_stdin_requests
            .lock()
            .map_err(|error| lock_error("close-stdin", error))?
            .push(request);
        Ok(())
    }

    async fn resize(&self, request: ResizeRequest) -> OciResult<()> {
        self.require_fake_process(&request.process, "resize")?;
        self.resize_requests
            .lock()
            .map_err(|error| lock_error("resize", error))?
            .push(request);
        Ok(())
    }

    async fn signal_process(&self, request: SignalProcessRequest) -> OciResult<()> {
        self.signal_process_requests
            .lock()
            .map_err(|error| lock_error("signal-process", error))?
            .push(request.clone());
        let mut processes = self
            .processes
            .lock()
            .map_err(|error| lock_error("signal-process", error))?;
        let process = processes
            .get_mut(request.process.process_id.as_str())
            .ok_or_else(|| oci_error(ErrorCode::NotFound, "signal-process", "process is absent"))?;
        validate_process_target(&request.process, &process.record, "signal-process")?;
        process.exit_status = Some(ExitStatus::signaled(request.signal.get(), false)?);
        append_missing_eof(&mut process.output, OutputStream::Stdout);
        if !process.record.terminal {
            append_missing_eof(&mut process.output, OutputStream::Stderr);
        }
        Ok(())
    }

    async fn wait_process(&self, request: WaitProcessRequest) -> OciResult<ExitStatus> {
        self.wait_process_requests
            .lock()
            .map_err(|error| lock_error("wait-process", error))?
            .push(request.clone());
        let processes = self
            .processes
            .lock()
            .map_err(|error| lock_error("wait-process", error))?;
        let process = processes
            .get(request.process.process_id.as_str())
            .ok_or_else(|| oci_error(ErrorCode::NotFound, "wait-process", "process is absent"))?;
        validate_process_target(&request.process, &process.record, "wait-process")?;
        process.exit_status.clone().ok_or_else(|| {
            Error::new(ErrorCode::DeadlineExceeded, "fake process is still running")
                .for_operation("wait-process")
                .retryable(true)
        })
    }

    async fn wait(&self, request: WaitRequest) -> OciResult<ExitStatus> {
        let containers = self
            .containers
            .lock()
            .map_err(|error| lock_error("wait", error))?;
        let container = containers
            .get(request.target.id.as_str())
            .ok_or_else(|| oci_error(ErrorCode::NotFound, "wait", "fake runtime is absent"))?;
        validate_target(&request.target, &container.record, "wait")?;
        container.exit_status.clone().ok_or_else(|| {
            Error::new(ErrorCode::DeadlineExceeded, "fake runtime is still running")
                .for_operation("wait")
                .retryable(true)
        })
    }
}

#[derive(Default)]
struct FakeBundleProvider {
    prepares: AtomicUsize,
    cleanups: AtomicUsize,
    invalid_console: AtomicBool,
    expected_snapshot_lower: Mutex<Option<std::path::PathBuf>>,
    snapshot_lower_observed: AtomicBool,
    last_box_dir: Mutex<Option<std::path::PathBuf>>,
}

#[async_trait]
impl OciBundleProvider for FakeBundleProvider {
    async fn prepare(&self, record: &BoxRecord) -> ExecutionManagerResult<OciPreparedExecution> {
        self.prepares.fetch_add(1, Ordering::SeqCst);
        *self
            .last_box_dir
            .lock()
            .map_err(|error| ExecutionManagerError::Internal(error.to_string()))? =
            Some(record.box_dir.clone());
        if let Some(expected) = self
            .expected_snapshot_lower
            .lock()
            .map_err(|error| ExecutionManagerError::Internal(error.to_string()))?
            .as_ref()
        {
            let marker = std::fs::read_to_string(record.box_dir.join(".snapshot-lower")).map_err(
                |error| {
                    ExecutionManagerError::Internal(format!(
                        "OCI bundle preparation did not receive a snapshot lower marker: {error}"
                    ))
                },
            )?;
            if std::path::Path::new(marker.trim()) != expected {
                return Err(ExecutionManagerError::Internal(format!(
                    "OCI bundle preparation received snapshot lower {} instead of {}",
                    marker.trim(),
                    expected.display()
                )));
            }
            self.snapshot_lower_observed.store(true, Ordering::SeqCst);
        }
        let spec = serde_json::from_value(json!({
            "ociVersion": "1.3.0",
            "process": {
                "terminal": false,
                "user": { "uid": 0, "gid": 0 },
                "args": ["/bin/true"],
                "cwd": "/"
            },
            "root": { "path": "rootfs", "readonly": false }
        }))
        .map_err(|error| ExecutionManagerError::Internal(error.to_string()))?;
        let bundle = OciBundle::from_spec(record.box_dir.join("oci-bundle"), spec)
            .map_err(|error| ExecutionManagerError::Internal(error.to_string()))?;
        let console_log = if self.invalid_console.load(Ordering::SeqCst) {
            record.box_dir.join("logs/provider-owned-console.log")
        } else {
            record.console_log.clone()
        };
        let mut prepared = OciPreparedExecution::new(bundle, console_log)?;
        prepared.anonymous_volumes = record.anonymous_volumes.clone();
        Ok(prepared)
    }

    async fn cleanup(&self, _record: &BoxRecord) -> ExecutionManagerResult<()> {
        self.cleanups.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn maps_product_isolation_without_selecting_a_driver() {
    assert_eq!(
        oci_isolation_request(ExecutionIsolation::Microvm),
        IsolationRequest::DedicatedVm
    );
    assert_eq!(
        oci_isolation_request(ExecutionIsolation::Sandbox),
        IsolationRequest::SharedHostKernel
    );
}

#[test]
fn operation_ids_are_stable_and_separate_by_generation_and_stage() {
    let generation = ExecutionGeneration::new(7).expect("Box generation");
    let first = operation_context(
        "box-operation",
        generation,
        "create",
        IsolationClass::DedicatedVm,
    )
    .expect("first context");
    let replay = operation_context(
        "box-operation",
        generation,
        "create",
        IsolationClass::DedicatedVm,
    )
    .expect("replayed context");
    let start = operation_context(
        "box-operation",
        generation,
        "start",
        IsolationClass::DedicatedVm,
    )
    .expect("start context");
    let next_generation = operation_context(
        "box-operation",
        ExecutionGeneration::new(8).expect("next Box generation"),
        "create",
        IsolationClass::DedicatedVm,
    )
    .expect("next-generation context");

    assert_eq!(first.operation_id, replay.operation_id);
    assert_ne!(first.operation_id, start.operation_id);
    assert_ne!(first.operation_id, next_generation.operation_id);
}

#[test]
fn terminal_status_conversion_is_exact_and_rejects_overflow() {
    assert_eq!(
        exit_code(&ExitStatus::exited(23).expect("normal exit")).expect("Box exit"),
        23
    );
    assert_eq!(
        exit_code(&ExitStatus::signaled(15, false).expect("signal exit")).expect("Box exit"),
        143
    );
    let overflow = ExitStatus::signaled(i32::MAX, false).expect("SDK signal status");
    assert!(matches!(
        exit_code(&overflow),
        Err(ExecutionManagerError::Internal(message)) if message.contains("cannot be represented")
    ));
}

#[test]
fn binding_validation_rejects_schema_identity_generation_and_evidence_drift() {
    let execution_id = ExecutionId::new("product-execution").expect("execution ID");
    let runtime_id = runtime_container_id(&execution_id).expect("runtime ID");
    let record = runtime_record(
        &runtime_id,
        RUNTIME_GENERATION,
        ContainerState::Running,
        DriverKind::LibkrunWhpx,
        IsolationClass::DedicatedVm,
        CONFIG_DIGEST,
        Some(ATTACHMENTS_DIGEST),
    )
    .expect("runtime record");
    let binding = OciRuntimeBinding::from_record(test_endpoint(), &runtime_id, &record)
        .expect("valid binding");
    let encoded = serde_json::to_string(&binding).expect("serialize binding");
    let decoded: OciRuntimeBinding = serde_json::from_str(&encoded).expect("deserialize binding");
    decoded
        .validate_for(&execution_id)
        .expect("round-tripped binding");

    let mut wrong_schema = decoded.clone();
    wrong_schema.schema_version = "a3s.box.oci-runtime-binding.v1".to_string();
    assert!(wrong_schema.validate().is_err());

    let mut current_target = decoded.clone();
    current_target.target.generation = None;
    assert!(current_target.validate().is_err());

    let mut zero_generation = decoded.clone();
    zero_generation.target.generation = Some(Generation(0));
    assert!(zero_generation.validate().is_err());

    let mut wrong_identity = decoded.clone();
    wrong_identity.target.id = ContainerId::new("a3s-box-other").expect("other runtime ID");
    assert!(wrong_identity.validate_for(&execution_id).is_err());

    let mut malformed_digest = decoded.clone();
    malformed_digest.config_digest = "sha256:ABC".to_string();
    assert!(malformed_digest.validate().is_err());

    let mut malformed_attachments = decoded.clone();
    malformed_attachments.attachments_digest = "sha256:ABC".to_string();
    assert!(malformed_attachments.validate().is_err());

    let mut missing_attachments = record.clone();
    missing_attachments.attachments_digest = None;
    assert!(
        OciRuntimeBinding::from_record(test_endpoint(), &runtime_id, &missing_attachments).is_err()
    );

    let mut drifted = record;
    drifted.driver = DriverKind::LibkrunKvm;
    assert!(decoded.validate_record(&drifted).is_err());

    let mut attachment_drifted = drifted;
    attachment_drifted.driver = decoded.driver;
    attachment_drifted.attachments_digest =
        Some("sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".into());
    assert!(decoded.validate_record(&attachment_drifted).is_err());
}

#[test]
fn durable_state_rejects_a_runtime_binding_owned_by_another_product_execution() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let execution_id = ExecutionId::new("durable-product").expect("execution ID");
    let mut record = build_managed_record(
        &directory.path().join("home"),
        &execution_id,
        box_operation("durable-product-operation"),
        request("durable-product", ExecutionIsolation::Sandbox),
        Utc::now(),
    )
    .expect("managed record");
    let other_id = ContainerId::new("a3s-box-other-product").expect("other runtime ID");
    let other_record = runtime_record(
        &other_id,
        RUNTIME_GENERATION,
        ContainerState::Running,
        DriverKind::NativeLinux,
        IsolationClass::SharedHostKernel,
        CONFIG_DIGEST,
        Some(ATTACHMENTS_DIGEST),
    )
    .expect("other runtime record");
    record
        .managed_execution
        .as_mut()
        .expect("managed metadata")
        .oci_runtime = Some(
        OciRuntimeBinding::from_record(test_endpoint(), &other_id, &other_record)
            .expect("standalone binding"),
    );
    let store =
        crate::BoxStateStore::from_records(directory.path().join("boxes.json"), vec![record]);

    let error = store.save().expect_err("cross-product binding must fail");

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("does not belong"));
}

#[tokio::test]
async fn preflight_rejects_probe_only_isolation_before_store_or_preparation() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::probe_only());
    let provider = Arc::new(FakeBundleProvider::default());
    let manager = manager(
        &directory,
        test_endpoint(),
        service.clone(),
        provider.clone(),
    );
    let operation = box_operation("probe-only-create");

    let error = manager
        .create(
            request("probe-only", ExecutionIsolation::Microvm),
            &operation,
        )
        .await
        .expect_err("probe-only driver must fail closed");

    assert!(matches!(
        error,
        ExecutionManagerError::Unavailable(message) if message.contains("launch-ready")
    ));
    assert!(!manager.state_path().exists());
    assert_eq!(provider.prepares.load(Ordering::SeqCst), 0);
    assert!(service.create_requests().is_empty());
    assert!(matches!(
        manager.reconcile(&operation).await.expect("reconcile"),
        ReconcileOutcome::Absent
    ));
}

#[tokio::test]
async fn preflight_requires_attachment_v1_before_store_or_preparation() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::without_attachment_schema());
    let provider = Arc::new(FakeBundleProvider::default());
    let manager = manager(
        &directory,
        test_endpoint(),
        service.clone(),
        provider.clone(),
    );
    let operation = box_operation("missing-attachment-schema-create");

    let error = manager
        .create(
            request("missing-attachment-schema", ExecutionIsolation::Sandbox),
            &operation,
        )
        .await
        .expect_err("attachment-unaware runtime must fail before mutation");

    assert!(matches!(
        error,
        ExecutionManagerError::Unavailable(message)
            if message.contains("a3s.oci.attachments.v1")
    ));
    assert!(!manager.state_path().exists());
    assert_eq!(provider.prepares.load(Ordering::SeqCst), 0);
    assert!(service.create_requests().is_empty());
}

#[tokio::test]
async fn snapshot_restore_prepares_the_managed_lower_before_oci_bundle_creation() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let home = directory.path().join("home");
    let snapshot_id = install_managed_snapshot(&home, "oci-restore-source");
    let expected_lower = home
        .join("snapshots")
        .join(snapshot_id.as_str())
        .join("rootfs")
        .canonicalize()
        .expect("canonical snapshot rootfs");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    let provider = Arc::new(FakeBundleProvider::default());
    *provider
        .expected_snapshot_lower
        .lock()
        .expect("expected snapshot lower lock") = Some(expected_lower.clone());
    let manager = manager(&directory, test_endpoint(), service, provider.clone());
    let mut restore = request("snapshot-restore", ExecutionIsolation::Sandbox);
    restore.rootfs_snapshot_id = Some(snapshot_id);

    let lease = manager
        .create_and_start(restore, &box_operation("snapshot-restore-operation"))
        .await
        .expect("snapshot-backed OCI start");
    let record = persisted(&manager, &lease.execution_id);

    assert!(provider.snapshot_lower_observed.load(Ordering::SeqCst));
    assert_eq!(
        std::path::PathBuf::from(
            std::fs::read_to_string(record.box_dir.join(".snapshot-lower"))
                .expect("persisted snapshot lower marker")
                .trim()
        ),
        expected_lower
    );
}

#[tokio::test]
async fn invalid_preparation_is_cleaned_before_runtime_create() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let home = directory.path().join("home");
    let snapshot_id = install_managed_snapshot(&home, "invalid-oci-restore-source");
    let expected_lower = home
        .join("snapshots")
        .join(snapshot_id.as_str())
        .join("rootfs")
        .canonicalize()
        .expect("canonical snapshot rootfs");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    let provider = Arc::new(FakeBundleProvider::default());
    provider.invalid_console.store(true, Ordering::SeqCst);
    *provider
        .expected_snapshot_lower
        .lock()
        .expect("expected snapshot lower lock") = Some(expected_lower);
    let manager = manager(
        &directory,
        test_endpoint(),
        service.clone(),
        provider.clone(),
    );
    let mut restore = request("invalid-preparation", ExecutionIsolation::Sandbox);
    restore.rootfs_snapshot_id = Some(snapshot_id);

    let error = manager
        .create_and_start(restore, &box_operation("invalid-preparation-operation"))
        .await
        .expect_err("provider must not change durable preparation fields");

    assert!(matches!(
        error,
        ExecutionManagerError::InvalidRequest(message) if message.contains("console path")
    ));
    assert_eq!(provider.prepares.load(Ordering::SeqCst), 1);
    assert_eq!(provider.cleanups.load(Ordering::SeqCst), 1);
    assert!(service.create_requests().is_empty());
    assert_eq!(service.container_count(), 0);
    let box_dir = provider
        .last_box_dir
        .lock()
        .expect("last box directory lock")
        .clone()
        .expect("prepared box directory");
    assert!(!box_dir.join(".snapshot-lower").exists());
}

#[tokio::test]
async fn mismatched_runtime_config_evidence_forces_exact_cleanup() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    *service
        .create_digest_override
        .lock()
        .expect("digest override lock") =
        Some("sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".into());
    let provider = Arc::new(FakeBundleProvider::default());
    let manager = manager(
        &directory,
        test_endpoint(),
        service.clone(),
        provider.clone(),
    );

    let error = manager
        .create_and_start(
            request("digest-drift", ExecutionIsolation::Sandbox),
            &box_operation("digest-drift-operation"),
        )
        .await
        .expect_err("runtime digest drift must fail closed");

    assert!(matches!(
        error,
        ExecutionManagerError::Internal(message) if message.contains("submitted bundle")
    ));
    assert_eq!(service.delete_modes(), vec![DeleteMode::Force]);
    assert_eq!(service.container_count(), 0);
    assert_eq!(provider.prepares.load(Ordering::SeqCst), 1);
    assert_eq!(provider.cleanups.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn mismatched_runtime_attachment_evidence_forces_exact_cleanup() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    *service
        .create_attachments_digest_override
        .lock()
        .expect("attachment digest override lock") =
        Some("sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".into());
    let provider = Arc::new(FakeBundleProvider::default());
    let manager = manager(
        &directory,
        test_endpoint(),
        service.clone(),
        provider.clone(),
    );

    let error = manager
        .create_and_start(
            request("attachment-drift", ExecutionIsolation::Sandbox),
            &box_operation("attachment-drift-operation"),
        )
        .await
        .expect_err("runtime attachment drift must fail closed");

    assert!(matches!(
        error,
        ExecutionManagerError::Internal(message) if message.contains("submitted manifest")
    ));
    assert_eq!(service.delete_modes(), vec![DeleteMode::Force]);
    assert_eq!(service.container_count(), 0);
    assert_eq!(provider.prepares.load(Ordering::SeqCst), 1);
    assert_eq!(provider.cleanups.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn launch_persists_exact_runtime_binding_for_both_product_isolations() {
    for (index, isolation, expected_request, expected_driver) in [
        (
            0,
            ExecutionIsolation::Sandbox,
            IsolationRequest::SharedHostKernel,
            DriverKind::NativeLinux,
        ),
        (
            1,
            ExecutionIsolation::Microvm,
            IsolationRequest::DedicatedVm,
            DriverKind::LibkrunWhpx,
        ),
    ] {
        let directory = tempfile::tempdir().expect("temporary directory");
        let service = Arc::new(FakeRuntimeService::launch_ready());
        let provider = Arc::new(FakeBundleProvider::default());
        let endpoint = test_endpoint();
        let manager = manager(
            &directory,
            endpoint.clone(),
            service.clone(),
            provider.clone(),
        );
        let operation = box_operation(&format!("mapped-launch-{index}"));

        let lease = manager
            .create_and_start(request(&format!("mapped-{index}"), isolation), &operation)
            .await
            .expect("launch through OCI backend");
        let persisted = persisted(&manager, &lease.execution_id);
        let metadata = persisted
            .managed_execution
            .as_ref()
            .expect("managed metadata");
        let binding = metadata.oci_runtime.as_ref().expect("OCI binding");
        let creates = service.create_requests();
        let starts = service.start_requests();

        assert_eq!(lease.generation, ExecutionGeneration::INITIAL);
        assert_eq!(metadata.generation, ExecutionGeneration::INITIAL);
        assert_ne!(metadata.generation.get(), RUNTIME_GENERATION.0);
        assert_eq!(creates.len(), 1);
        assert_eq!(creates[0].isolation, expected_request);
        assert_eq!(
            creates[0].attachments.schema_version(),
            ATTACHMENT_SCHEMA_V1
        );
        assert_eq!(creates[0].attachments.process_io().stdin, IoMode::Null);
        assert_eq!(starts.len(), 1);
        assert_ne!(
            creates[0].context.operation_id,
            starts[0].context.operation_id
        );
        assert_eq!(binding.endpoint, endpoint);
        assert_eq!(
            binding.target.id.as_str(),
            format!("a3s-box-{}", lease.execution_id)
        );
        assert_eq!(binding.target.generation, Some(RUNTIME_GENERATION));
        assert_eq!(binding.driver, expected_driver);
        assert_eq!(binding.isolation, expected_request.class());
        assert_eq!(binding.config_digest, creates[0].bundle.config_digest());
        assert_eq!(
            binding.attachments_digest,
            creates[0]
                .attachments
                .digest()
                .expect("submitted attachment digest")
        );
        assert_eq!(persisted.pid, None);
        assert_eq!(persisted.pid_start_time, None);
        assert!(persisted.exec_socket_path.as_os_str().is_empty());
        assert_eq!(provider.prepares.load(Ordering::SeqCst), 1);
        assert_eq!(provider.cleanups.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn captured_exec_replays_after_lost_response_and_backend_reopen() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    let provider = Arc::new(FakeBundleProvider::default());
    let endpoint = test_endpoint();
    let mut create = request("captured-exec", ExecutionIsolation::Sandbox);
    create.config.extra_env = vec![
        ("ALPHA".to_string(), "container".to_string()),
        ("BETA".to_string(), "container".to_string()),
    ];
    let first = manager(
        &directory,
        endpoint.clone(),
        service.clone(),
        provider.clone(),
    );
    let lease = first
        .create_and_start(create, &box_operation("captured-exec-create"))
        .await
        .expect("initial launch");
    let exec = BoxExecRequest {
        request_id: Some("health-probe-1".to_string()),
        cmd: vec!["/bin/check".to_string(), "--ready".to_string()],
        timeout_ns: 1_000_000_000,
        env: vec!["BETA=request".to_string(), "GAMMA=request".to_string()],
        working_dir: Some("/work".to_string()),
        rootfs: None,
        stdin: Some(b"probe input".to_vec()),
        stdin_streaming: false,
        user: Some("1000:1001".to_string()),
        streaming: false,
    };
    service.fail_exec_after_effect.store(true, Ordering::SeqCst);

    first
        .execute(&lease.execution_id, lease.generation, exec.clone())
        .await
        .expect_err("first exec response is intentionally lost");
    drop(first);

    let reopened = manager(&directory, endpoint, service.clone(), provider);
    let output = reopened
        .execute(&lease.execution_id, lease.generation, exec)
        .await
        .expect("replayed captured exec");

    assert_eq!(output.stdout, b"fake stdout\n");
    assert_eq!(output.stderr, b"fake stderr\n");
    assert_eq!(output.exit_code, 23);
    assert!(!output.truncated);
    assert!(reopened
        .read_logs(&lease.execution_id, lease.generation)
        .await
        .expect("structured Box logs remain readable")
        .is_empty());
    let calls = service.exec_requests();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0], calls[1]);
    assert_eq!(calls[0].container.generation, Some(RUNTIME_GENERATION));
    assert_ne!(
        calls[0].process_id.as_str(),
        calls[0].context.operation_id.as_str()
    );
    assert!(calls[0]
        .process_id
        .as_str()
        .starts_with("a3s-box-exec-process-"));
    assert_eq!(
        calls[0].process.args().as_ref().expect("process args"),
        &["/bin/check", "--ready"]
    );
    assert_eq!(
        calls[0]
            .process
            .env()
            .as_ref()
            .expect("process environment"),
        &["ALPHA=container", "BETA=request", "GAMMA=request"]
    );
    assert_eq!(calls[0].process.cwd(), &std::path::PathBuf::from("/work"));
    assert_eq!(calls[0].process.user().uid(), 1000);
    assert_eq!(calls[0].process.user().gid(), 1001);
    let capabilities = calls[0]
        .process
        .capabilities()
        .as_ref()
        .expect("explicit exec capability profile");
    assert!(capabilities
        .bounding()
        .as_ref()
        .is_some_and(|set| set.is_empty()));
    assert!(capabilities
        .effective()
        .as_ref()
        .is_some_and(|set| set.is_empty()));
    assert!(capabilities
        .permitted()
        .as_ref()
        .is_some_and(|set| set.is_empty()));
    assert_eq!(service.processes.lock().expect("process lock").len(), 1);
    let stdin = service.stdin_requests();
    assert_eq!(stdin.len(), 1);
    assert_eq!(stdin[0].data, b"probe input");
    assert_eq!(stdin[0].process.container, calls[0].container);
    assert_eq!(service.close_stdin_requests().len(), 1);
}

#[tokio::test]
async fn file_and_filesystem_sessions_preserve_exact_targets_and_replay_mutations_once() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    let manager = manager(
        &directory,
        test_endpoint(),
        service.clone(),
        Arc::new(FakeBundleProvider::default()),
    );
    let lease = manager
        .create_and_start(
            request("filesystem-sessions", ExecutionIsolation::Sandbox),
            &box_operation("filesystem-sessions-create"),
        )
        .await
        .expect("initial launch");

    let upload_data = STANDARD.encode(b"adapter payload");
    service.fail_file_after_effect.store(true, Ordering::SeqCst);
    let upload = manager
        .transfer_file(
            &lease.execution_id,
            lease.generation,
            BoxFileRequest {
                op: BoxFileOp::Upload,
                guest_path: "~/payload.txt".to_string(),
                data: Some(upload_data.clone()),
                user: Some("1000:1001".to_string()),
            },
        )
        .await
        .expect("upload replay after lost response");
    assert!(upload.success);
    assert_eq!(upload.data, None);
    assert_eq!(upload.size, 15);
    assert_eq!(upload.error, None);

    let download = manager
        .transfer_file(
            &lease.execution_id,
            lease.generation,
            BoxFileRequest {
                op: BoxFileOp::Download,
                guest_path: "/work/result.txt".to_string(),
                data: None,
                user: None,
            },
        )
        .await
        .expect("download through OCI SDK");
    assert!(download.success);
    assert_eq!(download.size, 10);
    assert_eq!(
        STANDARD
            .decode(download.data.expect("download payload"))
            .expect("valid download base64"),
        b"fake file\n"
    );

    service
        .fail_filesystem_after_effect
        .store(true, Ordering::SeqCst);
    let created = manager
        .filesystem(
            &lease.execution_id,
            lease.generation,
            BoxFilesystemRequest {
                op: BoxFilesystemOp::MakeDir,
                path: "/work/tree".to_string(),
                destination: None,
                depth: 0,
                user: Some("1000".to_string()),
            },
        )
        .await
        .expect("mkdir replay after lost response");
    assert_eq!(
        created.entry.expect("created directory").kind,
        BoxFilesystemEntryKind::Directory
    );

    let stat = manager
        .filesystem(
            &lease.execution_id,
            lease.generation,
            BoxFilesystemRequest {
                op: BoxFilesystemOp::Stat,
                path: "/work/tree/payload.txt".to_string(),
                destination: None,
                depth: 0,
                user: None,
            },
        )
        .await
        .expect("stat through OCI SDK");
    let stat_entry = stat.entry.expect("stat entry");
    assert_eq!(stat_entry.kind, BoxFilesystemEntryKind::File);
    assert_eq!(stat_entry.permissions, "-rw-r--r--");
    assert_eq!(
        stat_entry.metadata.get("fake").map(String::as_str),
        Some("true")
    );

    let listing = manager
        .filesystem(
            &lease.execution_id,
            lease.generation,
            BoxFilesystemRequest {
                op: BoxFilesystemOp::ListDir,
                path: "/work/tree".to_string(),
                destination: None,
                depth: 2,
                user: None,
            },
        )
        .await
        .expect("list through OCI SDK");
    assert_eq!(listing.entry, None);
    assert_eq!(listing.entries.len(), 1);
    assert_eq!(listing.entries[0].path, "/work/tree/fixture.txt");

    let moved = manager
        .filesystem(
            &lease.execution_id,
            lease.generation,
            BoxFilesystemRequest {
                op: BoxFilesystemOp::Move,
                path: "/work/tree/payload.txt".to_string(),
                destination: Some("/work/tree/moved.txt".to_string()),
                depth: 0,
                user: None,
            },
        )
        .await
        .expect("move through OCI SDK");
    assert_eq!(
        moved.entry.expect("moved entry").path,
        "/work/tree/moved.txt"
    );
    let removed = manager
        .filesystem(
            &lease.execution_id,
            lease.generation,
            BoxFilesystemRequest {
                op: BoxFilesystemOp::Remove,
                path: "/work/tree".to_string(),
                destination: None,
                depth: 0,
                user: None,
            },
        )
        .await
        .expect("remove through OCI SDK");
    assert!(removed.entry.is_none());
    assert!(removed.entries.is_empty());

    let file_calls = service.file_requests();
    assert_eq!(file_calls.len(), 3);
    assert_eq!(file_calls[0], file_calls[1]);
    assert_eq!(file_calls[0].op, OciFileOp::Upload);
    assert_eq!(file_calls[0].path, "~/payload.txt");
    assert_eq!(file_calls[0].data.as_deref(), Some(upload_data.as_str()));
    assert!(file_calls[0].context.is_some());
    assert_eq!(file_calls[2].op, OciFileOp::Download);
    assert!(file_calls[2].context.is_none());
    assert!(file_calls[2].data.is_none());
    assert_eq!(service.file_effects(), vec![file_calls[0].clone()]);
    assert!(file_calls
        .iter()
        .all(|call| call.target == file_calls[0].target));

    let filesystem_calls = service.filesystem_requests();
    assert_eq!(filesystem_calls.len(), 6);
    assert_eq!(filesystem_calls[0], filesystem_calls[1]);
    assert_eq!(filesystem_calls[0].op, OciFilesystemOp::MakeDir);
    assert!(filesystem_calls[0].context.is_some());
    assert_eq!(filesystem_calls[2].op, OciFilesystemOp::Stat);
    assert!(filesystem_calls[2].context.is_none());
    assert_eq!(filesystem_calls[3].op, OciFilesystemOp::ListDir);
    assert_eq!(filesystem_calls[3].depth, 2);
    assert!(filesystem_calls[3].context.is_none());
    assert_eq!(filesystem_calls[4].op, OciFilesystemOp::Move);
    assert!(filesystem_calls[4].context.is_some());
    assert_eq!(filesystem_calls[5].op, OciFilesystemOp::Remove);
    assert!(filesystem_calls[5].context.is_some());
    assert_eq!(service.filesystem_effects().len(), 3);
    assert!(filesystem_calls
        .iter()
        .all(|call| call.target == file_calls[0].target));
}

#[tokio::test]
async fn file_and_filesystem_capabilities_and_box_generation_fail_before_dispatch() {
    let file_directory = tempfile::tempdir().expect("file temporary directory");
    let file_service = Arc::new(FakeRuntimeService::without_operation(
        RuntimeOperation::File,
    ));
    let file_manager = manager(
        &file_directory,
        test_endpoint(),
        file_service.clone(),
        Arc::new(FakeBundleProvider::default()),
    );
    let file_lease = file_manager
        .create_and_start(
            request("missing-file", ExecutionIsolation::Sandbox),
            &box_operation("missing-file-create"),
        )
        .await
        .expect("file capability launch");
    let stale =
        ExecutionGeneration::new(file_lease.generation.get() + 1).expect("future generation");
    let file_request = BoxFileRequest {
        op: BoxFileOp::Download,
        guest_path: "/tmp/result".to_string(),
        data: None,
        user: None,
    };
    let stale_error = file_manager
        .transfer_file(&file_lease.execution_id, stale, file_request.clone())
        .await
        .expect_err("stale file generation must fail");
    assert!(matches!(
        stale_error,
        ExecutionManagerError::Conflict { .. }
    ));
    let capability_error = file_manager
        .transfer_file(
            &file_lease.execution_id,
            file_lease.generation,
            file_request,
        )
        .await
        .expect_err("missing file capability must fail");
    assert!(matches!(
        capability_error,
        ExecutionManagerError::Unavailable(message)
            if message.contains("does not advertise file")
    ));
    assert!(file_service.file_requests().is_empty());

    let filesystem_directory = tempfile::tempdir().expect("filesystem temporary directory");
    let filesystem_service = Arc::new(FakeRuntimeService::without_operation(
        RuntimeOperation::Filesystem,
    ));
    let filesystem_manager = manager(
        &filesystem_directory,
        test_endpoint(),
        filesystem_service.clone(),
        Arc::new(FakeBundleProvider::default()),
    );
    let filesystem_lease = filesystem_manager
        .create_and_start(
            request("missing-filesystem", ExecutionIsolation::Sandbox),
            &box_operation("missing-filesystem-create"),
        )
        .await
        .expect("filesystem capability launch");
    let filesystem_request = BoxFilesystemRequest {
        op: BoxFilesystemOp::Stat,
        path: "/tmp".to_string(),
        destination: None,
        depth: 0,
        user: None,
    };
    let stale_error = filesystem_manager
        .filesystem(
            &filesystem_lease.execution_id,
            ExecutionGeneration::new(filesystem_lease.generation.get() + 1)
                .expect("future filesystem generation"),
            filesystem_request.clone(),
        )
        .await
        .expect_err("stale filesystem generation must fail");
    assert!(matches!(
        stale_error,
        ExecutionManagerError::Conflict { .. }
    ));
    let capability_error = filesystem_manager
        .filesystem(
            &filesystem_lease.execution_id,
            filesystem_lease.generation,
            filesystem_request,
        )
        .await
        .expect_err("missing filesystem capability must fail");
    assert!(matches!(
        capability_error,
        ExecutionManagerError::Unavailable(message)
            if message.contains("does not advertise filesystem")
    ));
    assert!(filesystem_service.filesystem_requests().is_empty());
}

#[tokio::test]
async fn file_and_filesystem_sessions_reject_runtime_target_drift() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    let manager = manager(
        &directory,
        test_endpoint(),
        service.clone(),
        Arc::new(FakeBundleProvider::default()),
    );
    let lease = manager
        .create_and_start(
            request("filesystem-drift", ExecutionIsolation::Sandbox),
            &box_operation("filesystem-drift-create"),
        )
        .await
        .expect("initial launch");

    service.drift_file_target.store(true, Ordering::SeqCst);
    let file_error = manager
        .transfer_file(
            &lease.execution_id,
            lease.generation,
            BoxFileRequest {
                op: BoxFileOp::Download,
                guest_path: "/tmp/result".to_string(),
                data: None,
                user: None,
            },
        )
        .await
        .expect_err("file response target drift must fail closed");
    assert!(matches!(
        file_error,
        ExecutionManagerError::Internal(message)
            if message.contains("invalid file target")
    ));

    service.drift_file_target.store(false, Ordering::SeqCst);
    service
        .drift_filesystem_target
        .store(true, Ordering::SeqCst);
    let filesystem_error = manager
        .filesystem(
            &lease.execution_id,
            lease.generation,
            BoxFilesystemRequest {
                op: BoxFilesystemOp::Stat,
                path: "/tmp".to_string(),
                destination: None,
                depth: 0,
                user: None,
            },
        )
        .await
        .expect_err("filesystem response target drift must fail closed");
    assert!(matches!(
        filesystem_error,
        ExecutionManagerError::Internal(message)
            if message.contains("invalid filesystem target")
    ));
}

#[tokio::test]
async fn keyed_exec_rejects_changed_content_without_starting_a_second_process() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    let manager = manager(
        &directory,
        test_endpoint(),
        service.clone(),
        Arc::new(FakeBundleProvider::default()),
    );
    let lease = manager
        .create_and_start(
            request("keyed-conflict", ExecutionIsolation::Sandbox),
            &box_operation("keyed-conflict-create"),
        )
        .await
        .expect("initial launch");
    let mut original = box_exec_request(Some("one-process-key"));
    original.stdin = Some(b"original input".to_vec());
    original.timeout_ns = 1_000_000_000;
    manager
        .execute(&lease.execution_id, lease.generation, original.clone())
        .await
        .expect("original keyed exec");
    let mut changed = original;
    changed.cmd.push("changed".to_string());
    changed.stdin = Some(b"changed input".to_vec());
    changed.timeout_ns = 2_000_000_000;

    let error = manager
        .execute(&lease.execution_id, lease.generation, changed)
        .await
        .expect_err("changed content must not reuse one keyed process");

    assert!(matches!(
        error,
        ExecutionManagerError::Unavailable(message)
            if message.contains("different request")
    ));
    let calls = service.exec_requests();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].process_id, calls[1].process_id);
    assert_ne!(calls[0].context.operation_id, calls[1].context.operation_id);
    assert_eq!(service.processes.lock().expect("process lock").len(), 1);
    assert_eq!(service.stdin_effects().len(), 1);
}

#[tokio::test]
async fn exec_capability_and_box_generation_fail_before_runtime_mutation() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::without_operation(
        RuntimeOperation::Exec,
    ));
    let provider = Arc::new(FakeBundleProvider::default());
    let manager = manager(&directory, test_endpoint(), service.clone(), provider);
    let lease = manager
        .create_and_start(
            request("missing-exec", ExecutionIsolation::Microvm),
            &box_operation("missing-exec-create"),
        )
        .await
        .expect("initial launch");
    let stale = ExecutionGeneration::new(lease.generation.get() + 1).expect("future generation");

    let stale_error = manager
        .execute(&lease.execution_id, stale, box_exec_request(None))
        .await
        .expect_err("Box generation mismatch must fail");
    assert!(matches!(
        stale_error,
        ExecutionManagerError::Conflict { .. }
    ));
    let capability_error = manager
        .execute(
            &lease.execution_id,
            lease.generation,
            box_exec_request(None),
        )
        .await
        .expect_err("missing SDK exec must fail closed");
    assert!(matches!(
        capability_error,
        ExecutionManagerError::Unavailable(message)
            if message.contains("does not advertise exec")
    ));
    assert!(service.exec_requests().is_empty());
}

#[tokio::test]
async fn exec_rejects_a_second_rootfs_before_runtime_mutation() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    let manager = manager(
        &directory,
        test_endpoint(),
        service.clone(),
        Arc::new(FakeBundleProvider::default()),
    );
    let lease = manager
        .create_and_start(
            request("second-rootfs", ExecutionIsolation::Sandbox),
            &box_operation("second-rootfs-create"),
        )
        .await
        .expect("initial launch");
    let mut exec = box_exec_request(None);
    exec.rootfs = Some("/nested-rootfs".to_string());

    let error = manager
        .execute(&lease.execution_id, lease.generation, exec)
        .await
        .expect_err("OCI exec must not silently reinterpret another rootfs");

    assert!(matches!(
        error,
        ExecutionManagerError::InvalidRequest(message)
            if message.contains("second rootfs")
    ));
    assert!(service.exec_requests().is_empty());
}

#[tokio::test]
async fn invalid_exec_identity_and_empty_command_fail_before_runtime_mutation() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    let manager = manager(
        &directory,
        test_endpoint(),
        service.clone(),
        Arc::new(FakeBundleProvider::default()),
    );
    let lease = manager
        .create_and_start(
            request("invalid-exec", ExecutionIsolation::Sandbox),
            &box_operation("invalid-exec-create"),
        )
        .await
        .expect("initial launch");
    let invalid_requests = [
        BoxExecRequest {
            request_id: Some(String::new()),
            ..box_exec_request(None)
        },
        BoxExecRequest {
            request_id: Some("x".repeat(513)),
            ..box_exec_request(None)
        },
        BoxExecRequest {
            request_id: Some("contains\0nul".to_string()),
            ..box_exec_request(None)
        },
        BoxExecRequest {
            cmd: Vec::new(),
            ..box_exec_request(None)
        },
    ];

    for request in invalid_requests {
        let error = manager
            .execute(&lease.execution_id, lease.generation, request)
            .await
            .expect_err("invalid exec must fail locally");
        assert!(matches!(error, ExecutionManagerError::InvalidRequest(_)));
    }
    assert!(service.exec_requests().is_empty());
}

#[tokio::test]
async fn session_mode_capabilities_are_checked_before_exec_dispatch() {
    {
        let directory = tempfile::tempdir().expect("temporary directory");
        let service = Arc::new(FakeRuntimeService::without_operation(
            RuntimeOperation::ReadOutput,
        ));
        let manager = manager(
            &directory,
            test_endpoint(),
            service.clone(),
            Arc::new(FakeBundleProvider::default()),
        );
        let lease = manager
            .create_and_start(
                request("missing-output", ExecutionIsolation::Sandbox),
                &box_operation("missing-output-create"),
            )
            .await
            .expect("initial launch");
        let error = manager
            .execute(
                &lease.execution_id,
                lease.generation,
                box_exec_request(None),
            )
            .await
            .expect_err("captured exec requires read-output");
        assert!(error.to_string().contains("read-output"));
        assert!(service.exec_requests().is_empty());
    }

    {
        let directory = tempfile::tempdir().expect("temporary directory");
        let service = Arc::new(FakeRuntimeService::without_operation(
            RuntimeOperation::WriteStdin,
        ));
        let manager = manager(
            &directory,
            test_endpoint(),
            service.clone(),
            Arc::new(FakeBundleProvider::default()),
        );
        let lease = manager
            .create_and_start(
                request("missing-stdin", ExecutionIsolation::Sandbox),
                &box_operation("missing-stdin-create"),
            )
            .await
            .expect("initial launch");
        let mut exec = box_exec_request(None);
        exec.streaming = true;
        exec.stdin_streaming = true;
        let error = match manager
            .start_process(&lease.execution_id, lease.generation, exec)
            .await
        {
            Ok(_) => panic!("streaming exec requires write-stdin"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("write-stdin"));
        assert!(service.exec_requests().is_empty());
    }

    {
        let directory = tempfile::tempdir().expect("temporary directory");
        let service = Arc::new(FakeRuntimeService::without_operation(
            RuntimeOperation::Resize,
        ));
        let manager = manager(
            &directory,
            test_endpoint(),
            service.clone(),
            Arc::new(FakeBundleProvider::default()),
        );
        let lease = manager
            .create_and_start(
                request("missing-resize", ExecutionIsolation::Microvm),
                &box_operation("missing-resize-create"),
            )
            .await
            .expect("initial launch");
        let error = match manager
            .start_pty(
                &lease.execution_id,
                lease.generation,
                PtyRequest {
                    cmd: vec!["/bin/sh".to_string()],
                    env: Vec::new(),
                    working_dir: Some("/".to_string()),
                    rootfs: None,
                    user: None,
                    cols: 80,
                    rows: 24,
                },
            )
            .await
        {
            Ok(_) => panic!("PTY requires resize"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("resize"));
        assert!(service.exec_requests().is_empty());
    }
}

#[tokio::test]
async fn streaming_exec_retries_lost_stdin_and_preserves_exact_process_control() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    let provider = Arc::new(FakeBundleProvider::default());
    let manager = manager(&directory, test_endpoint(), service.clone(), provider);
    let lease = manager
        .create_and_start(
            request("streaming-exec", ExecutionIsolation::Sandbox),
            &box_operation("streaming-exec-create"),
        )
        .await
        .expect("initial launch");
    let mut request = box_exec_request(None);
    request.streaming = true;
    request.stdin_streaming = true;
    let mut process = manager
        .start_process(&lease.execution_id, lease.generation, request)
        .await
        .expect("start streaming exec");
    let input = process.input();
    service
        .fail_stdin_after_effect
        .store(true, Ordering::SeqCst);

    input
        .write_stdin(b"replayed input")
        .await
        .expect_err("stdin response is intentionally lost");
    input
        .write_stdin(b"changed input")
        .await
        .expect_err("changed retry content must retain and conflict on the same mutation");
    input
        .write_stdin(b"replayed input")
        .await
        .expect("same stdin mutation replays");
    input
        .write_stdin(b"next input")
        .await
        .expect("next stdin mutation");
    input.close_stdin().await.expect("close stdin");
    input
        .send_signal(ExecutionProcessSignal::Kill)
        .await
        .expect("signal exact process");

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut terminal = None;
    while let Some(event) = process.next_event().await.expect("stream event") {
        match event {
            ExecEvent::Chunk(chunk) => match chunk.stream {
                StreamType::Stdout => stdout.extend_from_slice(&chunk.data),
                StreamType::Stderr => stderr.extend_from_slice(&chunk.data),
            },
            ExecEvent::Exit(exit) => terminal = Some(exit),
            ExecEvent::FlushAck => {}
        }
    }

    assert_eq!(stdout, b"fake stdout\n");
    assert_eq!(stderr, b"fake stderr\n");
    assert_eq!(terminal.expect("terminal status").exit_code, 137);
    let stdin_calls = service.stdin_requests();
    assert_eq!(stdin_calls.len(), 4);
    assert_eq!(
        stdin_calls[0].context.operation_id,
        stdin_calls[1].context.operation_id
    );
    assert_eq!(
        stdin_calls[1].context.operation_id,
        stdin_calls[2].context.operation_id
    );
    assert_ne!(
        stdin_calls[2].context.operation_id,
        stdin_calls[3].context.operation_id
    );
    assert_eq!(service.stdin_effects().len(), 2);
    assert_eq!(service.close_stdin_requests().len(), 1);
    let signals = service.signal_process_requests();
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].signal.get(), 9);
    assert_eq!(
        signals[0].process.container.generation,
        Some(RUNTIME_GENERATION)
    );
    assert!(!service.wait_process_requests().is_empty());
}

#[tokio::test]
async fn pty_routes_merged_output_resize_and_signal_to_the_exact_sdk_process() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    let provider = Arc::new(FakeBundleProvider::default());
    let manager = manager(&directory, test_endpoint(), service.clone(), provider);
    let lease = manager
        .create_and_start(
            request("pty-exec", ExecutionIsolation::Microvm),
            &box_operation("pty-exec-create"),
        )
        .await
        .expect("initial launch");
    let mut process = manager
        .start_pty(
            &lease.execution_id,
            lease.generation,
            PtyRequest {
                cmd: vec!["/bin/sh".to_string()],
                env: vec!["TERM=xterm".to_string()],
                working_dir: Some("/".to_string()),
                rootfs: None,
                user: Some("root".to_string()),
                cols: 80,
                rows: 24,
            },
        )
        .await
        .expect("start PTY");
    let input = process.input();
    input.write_stdin(b"echo ready\n").await.expect("PTY input");
    input.resize_pty(120, 40).await.expect("PTY resize");
    input
        .send_signal(ExecutionProcessSignal::Terminate)
        .await
        .expect("PTY signal");

    let ExecEvent::Chunk(chunk) = process
        .next_event()
        .await
        .expect("PTY output")
        .expect("PTY output event")
    else {
        panic!("expected PTY output")
    };
    assert_eq!(chunk.stream, StreamType::Stdout);
    assert_eq!(chunk.data, b"fake tty\n");
    let ExecEvent::Exit(exit) = process
        .next_event()
        .await
        .expect("PTY exit")
        .expect("PTY exit event")
    else {
        panic!("expected PTY exit")
    };
    assert_eq!(exit.exit_code, 143);
    assert!(process.next_event().await.expect("PTY end").is_none());

    let exec = service.exec_requests();
    assert_eq!(exec.len(), 1);
    assert!(exec[0].process.terminal().unwrap_or(false));
    assert_eq!(exec[0].io.stdin, IoMode::Terminal);
    assert_eq!(
        exec[0].io.terminal_size,
        Some(TerminalSize {
            width: 80,
            height: 24
        })
    );
    let resize = service.resize_requests();
    assert_eq!(resize.len(), 1);
    assert_eq!(
        resize[0].size,
        TerminalSize {
            width: 120,
            height: 40
        }
    );
    assert_eq!(
        resize[0].process,
        ProcessTarget {
            container: exec[0].container.clone(),
            process_id: exec[0].process_id.clone(),
        }
    );
    assert_eq!(service.signal_process_requests()[0].signal.get(), 15);
}

#[tokio::test]
async fn captured_exec_timeout_kills_and_returns_legacy_terminal_shape() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    service.hold_next_process.store(true, Ordering::SeqCst);
    let provider = Arc::new(FakeBundleProvider::default());
    let manager = manager(&directory, test_endpoint(), service.clone(), provider);
    let lease = manager
        .create_and_start(
            request("timed-exec", ExecutionIsolation::Sandbox),
            &box_operation("timed-exec-create"),
        )
        .await
        .expect("initial launch");
    let mut exec = box_exec_request(Some("timed-exec-request"));
    exec.timeout_ns = 1;

    let output = manager
        .execute(&lease.execution_id, lease.generation, exec)
        .await
        .expect("timed exec result");

    assert_eq!(output.stdout, b"fake stdout\n");
    assert_eq!(
        output.stderr,
        b"fake stderr\n\nProcess killed: timeout exceeded"
    );
    assert_eq!(output.exit_code, 137);
    assert_eq!(service.signal_process_requests().len(), 1);
    assert_eq!(service.signal_process_requests()[0].signal.get(), 9);
}

#[tokio::test]
async fn dropped_exec_future_keeps_its_exact_timeout_watchdog() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    service.hold_next_process.store(true, Ordering::SeqCst);
    let provider = Arc::new(FakeBundleProvider::default());
    let manager = manager(&directory, test_endpoint(), service.clone(), provider);
    let lease = manager
        .create_and_start(
            request("detached-timeout", ExecutionIsolation::Sandbox),
            &box_operation("detached-timeout-create"),
        )
        .await
        .expect("initial launch");
    let mut exec = box_exec_request(None);
    exec.timeout_ns = 50_000_000;

    assert!(tokio::time::timeout(
        std::time::Duration::from_millis(5),
        manager.execute(&lease.execution_id, lease.generation, exec),
    )
    .await
    .is_err());
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let signals = service.signal_process_requests();
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].signal.get(), 9);
    assert_eq!(
        signals[0].process.container.generation,
        Some(RUNTIME_GENERATION)
    );
}

#[tokio::test]
async fn pause_resume_use_exact_runtime_target_and_unique_box_generations() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    let provider = Arc::new(FakeBundleProvider::default());
    let manager = manager(
        &directory,
        test_endpoint(),
        service.clone(),
        provider.clone(),
    );
    let operation = box_operation("freezer-cycle-operation");
    let running = manager
        .create_and_start(
            request("freezer-cycle", ExecutionIsolation::Sandbox),
            &operation,
        )
        .await
        .expect("initial launch");
    let original = persisted(&manager, &running.execution_id);
    let binding = original
        .managed_execution
        .as_ref()
        .and_then(|metadata| metadata.oci_runtime.clone())
        .expect("runtime binding");

    let paused = manager
        .pause(&running.execution_id, running.generation, true)
        .await
        .expect("pause through OCI SDK");
    let resumed = manager
        .resume(&running.execution_id, paused.generation)
        .await
        .expect("resume through OCI SDK");
    let paused_again = manager
        .pause(&running.execution_id, resumed.generation, true)
        .await
        .expect("second pause through OCI SDK");

    let pauses = service.pause_requests();
    let resumes = service.resume_requests();
    assert_eq!(pauses.len(), 2);
    assert_eq!(resumes.len(), 1);
    assert!(pauses
        .iter()
        .all(|request| request.target == binding.target));
    assert_eq!(resumes[0].target, binding.target);
    assert_ne!(
        pauses[0].context.operation_id,
        resumes[0].context.operation_id
    );
    assert_ne!(
        pauses[0].context.operation_id,
        pauses[1].context.operation_id
    );
    assert_eq!(paused.generation.get(), running.generation.get() + 1);
    assert_eq!(resumed.generation.get(), paused.generation.get() + 1);
    assert_eq!(paused_again.generation.get(), resumed.generation.get() + 1);
    let persisted = persisted(&manager, &running.execution_id);
    assert_eq!(persisted.status, ManagedExecutionState::Paused.as_status());
    assert_eq!(
        persisted
            .managed_execution
            .as_ref()
            .and_then(|metadata| metadata.oci_runtime.as_ref()),
        Some(&binding)
    );
    let runtime = service
        .containers
        .lock()
        .expect("container lock")
        .get(binding.target.id.as_str())
        .expect("runtime container")
        .record
        .clone();
    assert!(runtime.is_paused());
    assert_eq!(runtime.generation, RUNTIME_GENERATION);
    assert_eq!(service.create_requests().len(), 1);
    assert_eq!(service.start_requests().len(), 1);
    assert_eq!(provider.prepares.load(Ordering::SeqCst), 1);
    assert_eq!(provider.cleanups.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn running_snapshot_uses_a_durable_freezer_identity_per_attempt() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    let provider = Arc::new(FakeBundleProvider::default());
    let manager = manager(&directory, test_endpoint(), service.clone(), provider);
    let running = manager
        .create_and_start(
            request("snapshot-freezer", ExecutionIsolation::Sandbox),
            &box_operation("snapshot-freezer-create"),
        )
        .await
        .expect("initial launch");
    let snapshot_id = ExecutionSnapshotId::new("snapshot-freezer-attempt").unwrap();

    for attempt in 0..2 {
        let error = manager
            .create_filesystem_snapshot(&running.execution_id, running.generation, &snapshot_id)
            .await
            .expect_err("an empty fake rootfs cannot be captured");
        assert!(error
            .to_string()
            .contains("has no populated managed rootfs to snapshot"));

        let pauses = service.pause_requests();
        let resumes = service.resume_requests();
        assert_eq!(pauses.len(), attempt + 1);
        assert_eq!(resumes.len(), attempt + 1);
        assert_eq!(pauses[attempt].target, resumes[attempt].target);
        assert_ne!(
            pauses[attempt].context.operation_id,
            resumes[attempt].context.operation_id
        );
        if attempt > 0 {
            assert_ne!(
                pauses[attempt - 1].context.operation_id,
                pauses[attempt].context.operation_id
            );
            assert_ne!(
                resumes[attempt - 1].context.operation_id,
                resumes[attempt].context.operation_id
            );
        }

        let restored = persisted(&manager, &running.execution_id);
        assert_eq!(restored.status, ManagedExecutionState::Running.as_status());
        assert_eq!(
            restored.managed_execution.as_ref().unwrap().generation,
            running.generation
        );
        assert!(restored
            .managed_execution
            .as_ref()
            .unwrap()
            .pending_operation
            .is_none());
    }
}

#[tokio::test]
async fn snapshot_recovery_does_not_replay_a_completed_pause_after_thaw() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    let provider = Arc::new(FakeBundleProvider::default());
    let first = manager(
        &directory,
        test_endpoint(),
        service.clone(),
        provider.clone(),
    );
    let create_operation = box_operation("snapshot-thaw-recovery-create");
    let running = first
        .create_and_start(
            request("snapshot-thaw-recovery", ExecutionIsolation::Sandbox),
            &create_operation,
        )
        .await
        .expect("initial launch");
    let snapshot_id = ExecutionSnapshotId::new("snapshot-thaw-recovery").unwrap();
    let record = persisted(&first, &running.execution_id);
    let claimed = first
        .transition(
            &record,
            ManagedExecutionState::Running,
            ManagedExecutionState::Snapshotting,
            RuntimeUpdate::SnapshotClaim {
                snapshot_id: snapshot_id.clone(),
                source_state: ManagedExecutionState::Running,
                operation_id: box_operation("snapshot-thaw-freezer"),
            },
        )
        .await
        .expect("persist snapshot claim");
    first
        .backend
        .pause(&claimed, true)
        .await
        .expect("freeze snapshot source");
    let frozen = first
        .mark_snapshot_freezer_applied(&claimed)
        .await
        .expect("persist frozen phase");
    first
        .backend
        .resume(&frozen)
        .await
        .expect("simulate thaw before Box completion");
    drop(first);

    let recovered = manager(&directory, test_endpoint(), service.clone(), provider);
    let error = recovered
        .reconcile(&create_operation)
        .await
        .expect_err("an unpublished thawed snapshot must roll back");
    assert!(error
        .to_string()
        .contains("was thawed before filesystem snapshot"));
    assert_eq!(service.pause_requests().len(), 1);
    assert_eq!(service.resume_requests().len(), 1);
    let rolled_back = persisted(&recovered, &running.execution_id);
    assert_eq!(
        rolled_back.status,
        ManagedExecutionState::Running.as_status()
    );
    assert_eq!(
        rolled_back.managed_execution.as_ref().unwrap().generation,
        running.generation
    );
    assert!(rolled_back
        .managed_execution
        .as_ref()
        .unwrap()
        .pending_operation
        .is_none());
}

#[tokio::test]
async fn pause_requires_advertised_sdk_operation_before_runtime_mutation() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::without_operation(
        RuntimeOperation::Pause,
    ));
    let provider = Arc::new(FakeBundleProvider::default());
    let manager = manager(
        &directory,
        test_endpoint(),
        service.clone(),
        provider.clone(),
    );
    let running = manager
        .create_and_start(
            request("missing-pause", ExecutionIsolation::Microvm),
            &box_operation("missing-pause-operation"),
        )
        .await
        .expect("initial launch");

    let error = manager
        .pause(&running.execution_id, running.generation, true)
        .await
        .expect_err("missing pause capability must fail closed");

    assert!(matches!(
        error,
        ExecutionManagerError::Unavailable(message)
            if message.contains("does not advertise pause")
    ));
    assert!(service.pause_requests().is_empty());
    let persisted = persisted(&manager, &running.execution_id);
    assert_eq!(persisted.status, ManagedExecutionState::Running.as_status());
    assert_eq!(
        persisted
            .managed_execution
            .as_ref()
            .expect("managed metadata")
            .generation,
        running.generation
    );
}

#[tokio::test]
async fn pause_retry_after_rollback_uses_a_new_durable_mutation_identity() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    service
        .fail_pause_before_effect
        .store(true, Ordering::SeqCst);
    let provider = Arc::new(FakeBundleProvider::default());
    let manager = manager(&directory, test_endpoint(), service.clone(), provider);
    let running = manager
        .create_and_start(
            request("pause-retry", ExecutionIsolation::Sandbox),
            &box_operation("pause-retry-operation"),
        )
        .await
        .expect("initial launch");

    manager
        .pause(&running.execution_id, running.generation, true)
        .await
        .expect_err("first pause fails before mutation");
    let rolled_back = persisted(&manager, &running.execution_id);
    assert_eq!(
        rolled_back.status,
        ManagedExecutionState::Running.as_status()
    );
    assert_eq!(
        rolled_back
            .managed_execution
            .as_ref()
            .expect("managed metadata")
            .generation,
        running.generation
    );

    let paused = manager
        .pause(&running.execution_id, running.generation, true)
        .await
        .expect("new pause claim succeeds");
    let requests = service.pause_requests();
    assert_eq!(requests.len(), 2);
    assert_ne!(
        requests[0].context.operation_id,
        requests[1].context.operation_id
    );
    assert_eq!(paused.generation.get(), running.generation.get() + 1);
}

#[tokio::test]
async fn resume_requires_advertised_sdk_operation_before_runtime_mutation() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::without_operation(
        RuntimeOperation::Resume,
    ));
    let provider = Arc::new(FakeBundleProvider::default());
    let manager = manager(&directory, test_endpoint(), service.clone(), provider);
    let running = manager
        .create_and_start(
            request("missing-resume", ExecutionIsolation::Sandbox),
            &box_operation("missing-resume-operation"),
        )
        .await
        .expect("initial launch");
    let paused = manager
        .pause(&running.execution_id, running.generation, true)
        .await
        .expect("pause remains advertised");

    let error = manager
        .resume(&running.execution_id, paused.generation)
        .await
        .expect_err("missing resume capability must fail closed");

    assert!(matches!(
        error,
        ExecutionManagerError::Unavailable(message)
            if message.contains("does not advertise resume")
    ));
    assert!(service.resume_requests().is_empty());
    let persisted = persisted(&manager, &running.execution_id);
    assert_eq!(persisted.status, ManagedExecutionState::Paused.as_status());
    assert_eq!(
        persisted
            .managed_execution
            .as_ref()
            .expect("managed metadata")
            .generation,
        paused.generation
    );
}

#[tokio::test]
async fn reopened_backend_reconciles_lost_pause_and_resume_responses_without_replay() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    let provider = Arc::new(FakeBundleProvider::default());
    let endpoint = test_endpoint();
    let operation = box_operation("interrupted-freezer-operation");
    let first = manager(
        &directory,
        endpoint.clone(),
        service.clone(),
        provider.clone(),
    );
    let running = first
        .create_and_start(
            request("interrupted-freezer", ExecutionIsolation::Sandbox),
            &operation,
        )
        .await
        .expect("initial launch");
    let record = persisted(&first, &running.execution_id);
    let pausing = first
        .transition(
            &record,
            ManagedExecutionState::Running,
            ManagedExecutionState::Pausing,
            RuntimeUpdate::PauseClaim {
                keep_memory: true,
                operation_id: box_operation("interrupted-pause-runtime-operation"),
            },
        )
        .await
        .expect("persist pause claim");
    let binding = pausing
        .managed_execution
        .as_ref()
        .and_then(|metadata| metadata.oci_runtime.clone())
        .expect("pause binding");
    service
        .fail_pause_after_effect
        .store(true, Ordering::SeqCst);
    service
        .pause(ContainerOperationRequest {
            context: operation_context(
                "interrupted-pause-runtime-operation",
                running.generation,
                "pause",
                (&binding.target, true),
            )
            .expect("pause context"),
            target: binding.target.clone(),
        })
        .await
        .expect_err("pause response is intentionally lost");
    drop(first);

    let reopened = manager(
        &directory,
        endpoint.clone(),
        service.clone(),
        provider.clone(),
    );
    let ReconcileOutcome::Ready(paused) = reopened
        .reconcile(&operation)
        .await
        .expect("reconcile paused runtime")
    else {
        panic!("expected paused execution to remain ready")
    };
    assert_eq!(service.pause_requests().len(), 1);
    assert_eq!(paused.generation.get(), running.generation.get() + 1);

    let record = persisted(&reopened, &running.execution_id);
    reopened
        .transition(
            &record,
            ManagedExecutionState::Paused,
            ManagedExecutionState::Resuming,
            RuntimeUpdate::ResumeClaim(box_operation("interrupted-resume-runtime-operation")),
        )
        .await
        .expect("persist resume claim");
    service
        .fail_resume_after_effect
        .store(true, Ordering::SeqCst);
    service
        .resume(ContainerOperationRequest {
            context: operation_context(
                "interrupted-resume-runtime-operation",
                paused.generation,
                "resume",
                (&binding.target, false),
            )
            .expect("resume context"),
            target: binding.target.clone(),
        })
        .await
        .expect_err("resume response is intentionally lost");
    drop(reopened);

    let reopened = manager(&directory, endpoint, service.clone(), provider.clone());
    let ReconcileOutcome::Ready(resumed) = reopened
        .reconcile(&operation)
        .await
        .expect("reconcile resumed runtime")
    else {
        panic!("expected resumed execution to remain ready")
    };
    assert_eq!(service.resume_requests().len(), 1);
    assert_eq!(resumed.generation.get(), paused.generation.get() + 1);
    let persisted = persisted(&reopened, &running.execution_id);
    assert_eq!(persisted.status, ManagedExecutionState::Running.as_status());
    assert_eq!(
        persisted
            .managed_execution
            .as_ref()
            .and_then(|metadata| metadata.oci_runtime.as_ref()),
        Some(&binding)
    );
    assert_eq!(service.create_requests().len(), 1);
    assert_eq!(service.start_requests().len(), 1);
    assert_eq!(provider.prepares.load(Ordering::SeqCst), 1);
    assert_eq!(provider.cleanups.load(Ordering::SeqCst), 0);
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn retained_backend_reconnects_and_reconciles_after_local_runtime_server_restart() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    let provider = Arc::new(FakeBundleProvider::default());
    let endpoint = restart_test_endpoint(&directory);
    let first_server = spawn_runtime_transport_server(&endpoint, service.clone());
    let backend = OciLocalExecutionBackend::connect(endpoint.clone(), provider.clone())
        .await
        .expect("connect Box backend over local IPC");
    let manager = LocalExecutionManager::new(
        directory.path().join("boxes.json"),
        directory.path().join("home"),
        Arc::new(backend),
    );
    let operation = box_operation("runtime-server-restart-operation");
    let running = manager
        .create_and_start(
            request("runtime-server-restart", ExecutionIsolation::Sandbox),
            &operation,
        )
        .await
        .expect("initial launch over local IPC");

    first_server.abort();
    assert!(first_server
        .await
        .expect_err("first runtime server must be aborted")
        .is_cancelled());
    let error = manager
        .reconcile(&operation)
        .await
        .expect_err("the request that observes the disconnect must fail");
    assert!(matches!(error, ExecutionManagerError::Unavailable(_)));

    let replacement_server = spawn_runtime_transport_server(&endpoint, service.clone());
    let ReconcileOutcome::Ready(recovered) = manager
        .reconcile(&operation)
        .await
        .expect("retained backend must reconnect and reconcile")
    else {
        panic!("expected the running execution to reconcile as ready")
    };
    assert_eq!(recovered.execution_id, running.execution_id);
    assert_eq!(recovered.generation, running.generation);
    assert_eq!(service.create_requests().len(), 1);
    assert_eq!(service.start_requests().len(), 1);
    assert_eq!(provider.prepares.load(Ordering::SeqCst), 1);

    drop(manager);
    replacement_server
        .await
        .expect("replacement runtime server must join");
}

#[tokio::test]
async fn lost_start_response_reconciles_the_existing_generation_without_duplicate_create() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    service
        .fail_start_after_effect
        .store(true, Ordering::SeqCst);
    let provider = Arc::new(FakeBundleProvider::default());
    let manager = manager(
        &directory,
        test_endpoint(),
        service.clone(),
        provider.clone(),
    );

    let lease = manager
        .create_and_start(
            request("lost-start", ExecutionIsolation::Sandbox),
            &box_operation("lost-start-operation"),
        )
        .await
        .expect("start reconciliation");
    let record = persisted(&manager, &lease.execution_id);

    assert_eq!(service.create_requests().len(), 1);
    assert_eq!(service.start_requests().len(), 1);
    assert_eq!(provider.prepares.load(Ordering::SeqCst), 1);
    assert_eq!(provider.cleanups.load(Ordering::SeqCst), 0);
    assert_eq!(record.status, ManagedExecutionState::Running.as_status());
    assert!(record
        .managed_execution
        .as_ref()
        .and_then(|metadata| metadata.oci_runtime.as_ref())
        .is_some());
}

#[tokio::test]
async fn lost_create_response_starts_the_existing_generation_without_duplicate_create() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    service
        .fail_create_after_effect
        .store(true, Ordering::SeqCst);
    let provider = Arc::new(FakeBundleProvider::default());
    let manager = manager(
        &directory,
        test_endpoint(),
        service.clone(),
        provider.clone(),
    );

    let lease = manager
        .create_and_start(
            request("lost-create", ExecutionIsolation::Microvm),
            &box_operation("lost-create-operation"),
        )
        .await
        .expect("create reconciliation");
    let record = persisted(&manager, &lease.execution_id);

    assert_eq!(service.create_requests().len(), 1);
    assert_eq!(service.start_requests().len(), 1);
    assert_eq!(provider.prepares.load(Ordering::SeqCst), 1);
    assert_eq!(provider.cleanups.load(Ordering::SeqCst), 0);
    assert_eq!(record.status, ManagedExecutionState::Running.as_status());
    assert_eq!(
        record
            .managed_execution
            .as_ref()
            .and_then(|metadata| metadata.oci_runtime.as_ref())
            .and_then(|binding| binding.target.generation),
        Some(RUNTIME_GENERATION)
    );
}

#[tokio::test]
async fn interrupted_starting_record_starts_created_runtime_without_duplicate_create() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    let provider = Arc::new(FakeBundleProvider::default());
    let manager = manager(
        &directory,
        test_endpoint(),
        service.clone(),
        provider.clone(),
    );
    let execution_id = ExecutionId::new("interrupted-start").expect("execution ID");
    let operation = box_operation("interrupted-start-operation");
    let record = build_managed_record(
        &directory.path().join("home"),
        &execution_id,
        operation.clone(),
        request("interrupted-start", ExecutionIsolation::Microvm),
        Utc::now(),
    )
    .expect("managed record");
    let reserved = manager
        .reserve(record)
        .await
        .expect("reserve record")
        .into_record();
    manager
        .transition(
            &reserved,
            ManagedExecutionState::Created,
            ManagedExecutionState::Starting,
            RuntimeUpdate::None,
        )
        .await
        .expect("claim startup");
    service.seed(
        &execution_id,
        ExecutionIsolation::Microvm,
        ContainerState::Created,
    );

    let outcome = manager
        .reconcile(&operation)
        .await
        .expect("recover startup");
    let ReconcileOutcome::Ready(lease) = outcome else {
        panic!("expected a ready execution after recovery")
    };
    let record = persisted(&manager, &execution_id);

    assert_eq!(lease.execution_id, execution_id);
    assert!(service.create_requests().is_empty());
    assert_eq!(service.start_requests().len(), 1);
    assert_eq!(provider.prepares.load(Ordering::SeqCst), 0);
    assert_eq!(record.status, ManagedExecutionState::Running.as_status());
    assert_eq!(
        record
            .managed_execution
            .as_ref()
            .and_then(|metadata| metadata.oci_runtime.as_ref())
            .and_then(|binding| binding.target.generation),
        Some(RUNTIME_GENERATION)
    );
}

#[tokio::test]
async fn reopened_backend_kills_with_persisted_signal_and_preserves_exact_exit() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    let provider = Arc::new(FakeBundleProvider::default());
    let endpoint = test_endpoint();
    let first = manager(
        &directory,
        endpoint.clone(),
        service.clone(),
        provider.clone(),
    );
    let lease = first
        .create_and_start(
            request("reopen-kill", ExecutionIsolation::Microvm),
            &box_operation("reopen-kill-operation"),
        )
        .await
        .expect("initial launch");
    let reopened = manager(&directory, endpoint, service.clone(), provider.clone());

    let status = reopened
        .inspect(&lease.execution_id)
        .await
        .expect("inspect through reopened backend");
    assert_eq!(status.state, ExecutionState::Running);
    assert_eq!(service.create_requests().len(), 1);

    let invalid_signal = reopened
        .kill_with_options(
            &lease.execution_id,
            lease.generation,
            KillExecutionOptions {
                signal: Some(i32::MAX),
                timeout_secs: None,
            },
        )
        .await
        .expect_err("overflowing exit-code mapping must fail before the runtime");
    assert!(matches!(
        invalid_signal,
        ExecutionManagerError::InvalidRequest(message) if message.contains("representable")
    ));
    assert!(service.kill_signals().is_empty());

    let outcome = reopened
        .kill_with_options(
            &lease.execution_id,
            lease.generation,
            KillExecutionOptions {
                signal: Some(15),
                timeout_secs: None,
            },
        )
        .await
        .expect("kill exact generation");
    let record = persisted(&reopened, &lease.execution_id);

    assert_eq!(outcome, KillOutcome::Killed);
    assert_eq!(service.kill_signals(), vec![15]);
    assert_eq!(service.delete_modes(), vec![DeleteMode::StoppedOnly]);
    assert_eq!(service.container_count(), 0);
    assert_eq!(provider.cleanups.load(Ordering::SeqCst), 1);
    assert_eq!(record.exit_code, Some(143));
    assert_eq!(record.status, ManagedExecutionState::Stopped.as_status());
    assert!(record
        .managed_execution
        .as_ref()
        .and_then(|metadata| metadata.oci_runtime.as_ref())
        .is_none());
}

#[tokio::test]
async fn reopened_backend_observes_natural_terminal_status_before_stopped_only_delete() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    let provider = Arc::new(FakeBundleProvider::default());
    let endpoint = test_endpoint();
    let first = manager(
        &directory,
        endpoint.clone(),
        service.clone(),
        provider.clone(),
    );
    let lease = first
        .create_and_start(
            request("natural-exit", ExecutionIsolation::Sandbox),
            &box_operation("natural-exit-operation"),
        )
        .await
        .expect("initial launch");
    service.mark_stopped(
        &lease.execution_id,
        ExitStatus::exited(23).expect("exit status"),
    );
    let reopened = manager(&directory, endpoint, service.clone(), provider.clone());

    let status = reopened
        .inspect(&lease.execution_id)
        .await
        .expect("terminal inspection");
    let record = persisted(&reopened, &lease.execution_id);

    assert_eq!(status.state, ExecutionState::Stopped);
    assert_eq!(record.exit_code, Some(23));
    assert_eq!(service.delete_modes(), vec![DeleteMode::StoppedOnly]);
    assert_eq!(service.container_count(), 0);
    assert_eq!(provider.cleanups.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn graceful_kill_timeout_escalates_through_a_distinct_sdk_signal() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    service.ignore_graceful_signal.store(true, Ordering::SeqCst);
    let provider = Arc::new(FakeBundleProvider::default());
    let manager = manager(&directory, test_endpoint(), service.clone(), provider);
    let lease = manager
        .create_and_start(
            request("kill-escalation", ExecutionIsolation::Sandbox),
            &box_operation("kill-escalation-operation"),
        )
        .await
        .expect("initial launch");

    let outcome = manager
        .kill_with_options(
            &lease.execution_id,
            lease.generation,
            KillExecutionOptions {
                signal: Some(15),
                timeout_secs: Some(0),
            },
        )
        .await
        .expect("graceful kill escalation");
    let record = persisted(&manager, &lease.execution_id);
    let graceful = operation_context(lease.execution_id.as_str(), lease.generation, "kill", 15)
        .expect("graceful kill context");
    let force = operation_context(
        lease.execution_id.as_str(),
        lease.generation,
        "kill",
        DEFAULT_KILL_SIGNAL,
    )
    .expect("force kill context");

    assert_eq!(outcome, KillOutcome::Killed);
    assert_eq!(service.kill_signals(), vec![15, DEFAULT_KILL_SIGNAL]);
    assert_ne!(graceful.operation_id, force.operation_id);
    assert_eq!(record.exit_code, Some(137));
    assert_eq!(service.delete_modes(), vec![DeleteMode::StoppedOnly]);
    assert_eq!(service.container_count(), 0);
}

#[tokio::test]
async fn exact_generation_processes_stats_and_events_use_the_public_sdk() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    let manager = manager(
        &directory,
        test_endpoint(),
        service.clone(),
        Arc::new(FakeBundleProvider::default()),
    );
    let lease = manager
        .create_and_start(
            request("observable", ExecutionIsolation::Sandbox),
            &box_operation("observable-create"),
        )
        .await
        .expect("initial launch");
    let binding = persisted(&manager, &lease.execution_id)
        .managed_execution
        .expect("managed metadata")
        .oci_runtime
        .expect("OCI binding");

    let inventory = manager
        .list_processes(&lease.execution_id, lease.generation)
        .await
        .expect("process inventory");
    assert_eq!(inventory.execution_id, lease.execution_id);
    assert_eq!(inventory.generation, lease.generation);
    assert_eq!(inventory.processes.len(), 1);
    assert_eq!(inventory.processes[0].process_id, "init");
    assert_eq!(inventory.processes[0].pid, Some(4_242));
    assert_eq!(service.processes_requests()[0].target, binding.target);

    let stats = manager
        .stats(&lease.execution_id, lease.generation)
        .await
        .expect("runtime stats");
    assert_eq!(stats.execution_id, lease.execution_id);
    assert_eq!(stats.cpu.usage_ns, 100);
    assert_eq!(stats.memory.limit_bytes, Some(128 * 1024 * 1024));
    assert_eq!(stats.metrics["io.read_bytes"], 4096);
    assert_eq!(service.stats_requests()[0].target, binding.target);

    let batch = manager
        .events(
            &lease.execution_id,
            lease.generation,
            ExecutionEventsRequest {
                after_sequence: 0,
                limit: 16,
                wait_timeout_ms: Some(250),
            },
        )
        .await
        .expect("runtime events");
    assert_eq!(batch.execution_id, lease.execution_id);
    assert_eq!(batch.generation, lease.generation);
    assert_eq!(
        batch
            .events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![5, 8]
    );
    assert_eq!(batch.events[1].process_id.as_deref(), Some("init"));
    assert_eq!(batch.next_sequence, 8);
    let request = &service.events_requests()[0];
    assert_eq!(request.container.as_ref(), Some(&binding.target));
    assert_eq!(request.wait_timeout_ms, Some(250));
}

#[tokio::test]
async fn resource_update_persists_complete_intent_and_replays_locally() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    let manager = manager(
        &directory,
        test_endpoint(),
        service.clone(),
        Arc::new(FakeBundleProvider::default()),
    );
    let create_operation = box_operation("resource-update-create");
    let create_request = request("resource-update", ExecutionIsolation::Microvm);
    let lease = manager
        .create_and_start(create_request.clone(), &create_operation)
        .await
        .expect("initial launch");
    let operation = box_operation("resource-update-live");
    let update = ExecutionResourceUpdate {
        memory_reservation: Some(64 * 1024 * 1024),
        pids_limit: Some(64),
        cpu_shares: Some(512),
        cpu_quota: Some(50_000),
        cpu_period: Some(100_000),
        cpuset_cpus: Some("0-1".to_string()),
        ..Default::default()
    };

    let updated = manager
        .update_resources(
            &lease.execution_id,
            lease.generation,
            &operation,
            update.clone(),
        )
        .await
        .expect("resource update");
    assert_eq!(updated.generation, lease.generation);
    assert_eq!(service.update_requests().len(), 1);
    assert_eq!(service.update_effects().len(), 1);
    let runtime_request = &service.update_requests()[0];
    let memory = runtime_request
        .resources
        .memory()
        .as_ref()
        .expect("memory resources");
    let cpu = runtime_request
        .resources
        .cpu()
        .as_ref()
        .expect("CPU resources");
    let pids = runtime_request
        .resources
        .pids()
        .as_ref()
        .expect("PID resources");
    assert_eq!(memory.limit(), Some(128 * 1024 * 1024));
    assert_eq!(memory.reservation(), Some(64 * 1024 * 1024));
    assert_eq!(cpu.shares(), Some(512));
    assert_eq!(cpu.quota(), Some(50_000));
    assert_eq!(cpu.period(), Some(100_000));
    assert_eq!(cpu.cpus().as_deref(), Some("0-1"));
    assert_eq!(pids.limit(), 64);

    let record = persisted(&manager, &lease.execution_id);
    assert_eq!(record.status, ManagedExecutionState::Running.as_status());
    assert_eq!(record.resource_limits.cpu_shares, Some(512));
    let metadata = record.managed_execution.as_ref().expect("managed metadata");
    assert_eq!(
        metadata.request.config.resource_limits,
        record.resource_limits
    );
    let completed = metadata
        .last_resource_update
        .as_ref()
        .expect("resource completion");
    assert_eq!(completed.operation_id, operation);
    assert_eq!(completed.update, update);

    manager
        .update_resources(
            &lease.execution_id,
            lease.generation,
            &operation,
            update.clone(),
        )
        .await
        .expect("local completion replay");
    assert_eq!(service.update_requests().len(), 1);
    let error = manager
        .update_resources(
            &lease.execution_id,
            lease.generation,
            &operation,
            ExecutionResourceUpdate {
                cpu_shares: Some(1024),
                ..update
            },
        )
        .await
        .expect_err("changed completed intent must fail");
    assert!(matches!(error, ExecutionManagerError::Conflict { .. }));
    assert_eq!(service.update_requests().len(), 1);

    let replayed_create = manager
        .create_and_start(create_request, &create_operation)
        .await
        .expect("original create remains idempotent after mutable resource intent");
    assert_eq!(replayed_create.execution_id, lease.execution_id);
    assert_eq!(replayed_create.generation, lease.generation);
    assert_eq!(service.create_requests().len(), 1);
}

#[tokio::test]
async fn lost_resource_update_response_recovers_with_the_same_runtime_mutation() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    service
        .fail_update_after_effect
        .store(true, Ordering::SeqCst);
    let provider = Arc::new(FakeBundleProvider::default());
    let endpoint = test_endpoint();
    let create_operation = box_operation("lost-update-create");
    let first = manager(
        &directory,
        endpoint.clone(),
        service.clone(),
        provider.clone(),
    );
    let lease = first
        .create_and_start(
            request("lost-update", ExecutionIsolation::Sandbox),
            &create_operation,
        )
        .await
        .expect("initial launch");
    let update = ExecutionResourceUpdate {
        pids_limit: Some(72),
        ..Default::default()
    };

    first
        .update_resources(
            &lease.execution_id,
            lease.generation,
            &box_operation("lost-update-live"),
            update.clone(),
        )
        .await
        .expect_err("first response is lost");
    let claimed = persisted(&first, &lease.execution_id);
    assert_eq!(
        claimed.status,
        ManagedExecutionState::UpdatingResources.as_status()
    );
    assert_eq!(service.update_effects().len(), 1);
    let conflict = first
        .update_resources(
            &lease.execution_id,
            lease.generation,
            &box_operation("lost-update-live"),
            ExecutionResourceUpdate {
                pids_limit: Some(73),
                ..Default::default()
            },
        )
        .await
        .expect_err("pending operation identity cannot change content");
    assert!(matches!(conflict, ExecutionManagerError::Conflict { .. }));
    assert_eq!(service.update_requests().len(), 1);

    let reopened = manager(&directory, endpoint, service.clone(), provider);
    let outcome = reopened
        .reconcile(&create_operation)
        .await
        .expect("resource update recovery");
    assert!(matches!(outcome, ReconcileOutcome::Ready(_)));
    let recovered = persisted(&reopened, &lease.execution_id);
    assert_eq!(recovered.status, ManagedExecutionState::Running.as_status());
    assert_eq!(recovered.resource_limits.pids_limit, Some(72));
    assert_eq!(service.update_requests().len(), 2);
    assert_eq!(service.update_effects().len(), 1);
    assert_eq!(
        service.update_requests()[0].context.operation_id,
        service.update_requests()[1].context.operation_id
    );
}

#[tokio::test]
async fn resource_update_racing_natural_exit_settles_the_terminal_record() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    let provider = Arc::new(FakeBundleProvider::default());
    let create_operation = box_operation("terminal-update-create");
    let manager = manager(&directory, test_endpoint(), service.clone(), provider);
    let lease = manager
        .create_and_start(
            request("terminal-update", ExecutionIsolation::Sandbox),
            &create_operation,
        )
        .await
        .expect("initial launch");
    service.mark_stopped(
        &lease.execution_id,
        ExitStatus::exited(17).expect("exit status"),
    );

    manager
        .update_resources(
            &lease.execution_id,
            lease.generation,
            &box_operation("terminal-update-live"),
            ExecutionResourceUpdate {
                pids_limit: Some(44),
                ..Default::default()
            },
        )
        .await
        .expect_err("stopped runtime cannot accept a live update");

    let record = persisted(&manager, &lease.execution_id);
    assert_eq!(record.status, ManagedExecutionState::Stopped.as_status());
    assert_eq!(record.exit_code, Some(17));
    assert!(record
        .managed_execution
        .as_ref()
        .expect("managed metadata")
        .pending_operation
        .is_none());
    assert!(matches!(
        manager
            .reconcile(&create_operation)
            .await
            .expect("terminal reconciliation"),
        ReconcileOutcome::Failed
    ));
}

#[tokio::test]
async fn observability_rejects_runtime_target_and_event_order_drift() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    let manager = manager(
        &directory,
        test_endpoint(),
        service.clone(),
        Arc::new(FakeBundleProvider::default()),
    );
    let lease = manager
        .create_and_start(
            request("drift-observation", ExecutionIsolation::Sandbox),
            &box_operation("drift-observation-create"),
        )
        .await
        .expect("initial launch");

    service.drift_process_target.store(true, Ordering::SeqCst);
    let process_error = manager
        .list_processes(&lease.execution_id, lease.generation)
        .await
        .expect_err("process target drift must fail");
    assert!(matches!(process_error, ExecutionManagerError::Internal(_)));
    service.drift_process_target.store(false, Ordering::SeqCst);

    service.drift_stats_target.store(true, Ordering::SeqCst);
    let stats_error = manager
        .stats(&lease.execution_id, lease.generation)
        .await
        .expect_err("stats target drift must fail");
    assert!(matches!(stats_error, ExecutionManagerError::Internal(_)));
    service.drift_stats_target.store(false, Ordering::SeqCst);

    service.misorder_events.store(true, Ordering::SeqCst);
    let events_error = manager
        .events(
            &lease.execution_id,
            lease.generation,
            ExecutionEventsRequest {
                after_sequence: 0,
                limit: 16,
                wait_timeout_ms: None,
            },
        )
        .await
        .expect_err("event order drift must fail");
    assert!(matches!(events_error, ExecutionManagerError::Internal(_)));
}

#[tokio::test]
async fn new_runtime_operations_require_advertised_capabilities_before_dispatch() {
    for operation in [
        RuntimeOperation::Processes,
        RuntimeOperation::Stats,
        RuntimeOperation::Events,
        RuntimeOperation::Update,
    ] {
        let directory = tempfile::tempdir().expect("temporary directory");
        let service = Arc::new(FakeRuntimeService::without_operation(operation));
        let manager = manager(
            &directory,
            test_endpoint(),
            service.clone(),
            Arc::new(FakeBundleProvider::default()),
        );
        let create_operation = box_operation(&format!("missing-{operation:?}-create"));
        let lease = manager
            .create_and_start(
                request("missing-observation", ExecutionIsolation::Sandbox),
                &create_operation,
            )
            .await
            .expect("initial launch");

        let error = match operation {
            RuntimeOperation::Processes => manager
                .list_processes(&lease.execution_id, lease.generation)
                .await
                .map(|_| ()),
            RuntimeOperation::Stats => manager
                .stats(&lease.execution_id, lease.generation)
                .await
                .map(|_| ()),
            RuntimeOperation::Events => manager
                .events(
                    &lease.execution_id,
                    lease.generation,
                    ExecutionEventsRequest {
                        after_sequence: 0,
                        limit: 1,
                        wait_timeout_ms: None,
                    },
                )
                .await
                .map(|_| ()),
            RuntimeOperation::Update => manager
                .update_resources(
                    &lease.execution_id,
                    lease.generation,
                    &box_operation("missing-update-live"),
                    ExecutionResourceUpdate {
                        pids_limit: Some(32),
                        ..Default::default()
                    },
                )
                .await
                .map(|_| ()),
            _ => unreachable!(),
        }
        .expect_err("missing operation must fail closed");
        assert!(matches!(error, ExecutionManagerError::Unavailable(_)));
        let record = persisted(&manager, &lease.execution_id);
        assert_eq!(record.status, ManagedExecutionState::Running.as_status());
    }
}

fn manager(
    directory: &tempfile::TempDir,
    endpoint: OciRuntimeEndpoint,
    service: Arc<FakeRuntimeService>,
    provider: Arc<FakeBundleProvider>,
) -> LocalExecutionManager {
    let runtime_service: Arc<dyn OciRuntimeService> = service;
    let backend = OciLocalExecutionBackend::from_client(
        endpoint,
        RuntimeClient::from_arc(runtime_service),
        provider,
    )
    .expect("OCI backend");
    LocalExecutionManager::new(
        directory.path().join("boxes.json"),
        directory.path().join("home"),
        Arc::new(backend),
    )
}

#[cfg(windows)]
fn restart_test_endpoint(_directory: &tempfile::TempDir) -> OciRuntimeEndpoint {
    static NEXT_PIPE: AtomicUsize = AtomicUsize::new(20_000);
    OciRuntimeEndpoint::windows_named_pipe(format!(
        r"\\.\pipe\a3s-box-oci-reconnect-test-{}-{}",
        std::process::id(),
        NEXT_PIPE.fetch_add(1, Ordering::Relaxed)
    ))
    .expect("Windows named-pipe endpoint")
}

#[cfg(unix)]
fn restart_test_endpoint(directory: &tempfile::TempDir) -> OciRuntimeEndpoint {
    OciRuntimeEndpoint::unix_socket(directory.path().join("runtime-reconnect.sock"))
        .expect("Unix socket endpoint")
}

#[cfg(windows)]
fn spawn_runtime_transport_server(
    endpoint: &OciRuntimeEndpoint,
    service: Arc<FakeRuntimeService>,
) -> tokio::task::JoinHandle<()> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let OciRuntimeEndpoint::WindowsNamedPipe { name } = endpoint else {
        panic!("Windows test requires a named-pipe endpoint")
    };
    let server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(name)
        .expect("create runtime named-pipe server");
    tokio::spawn(async move {
        server
            .connect()
            .await
            .expect("accept Box named-pipe client");
        let runtime_service: Arc<dyn OciRuntimeService> = service;
        a3s_oci_sdk::serve_transport_connection(runtime_service, server)
            .await
            .expect("serve Box SDK connection");
    })
}

#[cfg(unix)]
fn spawn_runtime_transport_server(
    endpoint: &OciRuntimeEndpoint,
    service: Arc<FakeRuntimeService>,
) -> tokio::task::JoinHandle<()> {
    let OciRuntimeEndpoint::UnixSocket { path } = endpoint else {
        panic!("Unix test requires a socket endpoint")
    };
    if path.exists() {
        std::fs::remove_file(path).expect("remove stale runtime test socket");
    }
    let listener = tokio::net::UnixListener::bind(path).expect("bind runtime Unix socket");
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept Box Unix client");
        let runtime_service: Arc<dyn OciRuntimeService> = service;
        a3s_oci_sdk::serve_transport_connection(runtime_service, stream)
            .await
            .expect("serve Box SDK connection");
    })
}

fn persisted(manager: &LocalExecutionManager, execution_id: &ExecutionId) -> BoxRecord {
    ManagedExecutionStore::new(manager.state_path().to_path_buf())
        .get(execution_id)
        .expect("read managed store")
        .expect("persisted execution")
}

fn install_managed_snapshot(home: &std::path::Path, id: &str) -> ExecutionSnapshotId {
    let snapshot_id = ExecutionSnapshotId::new(id).expect("snapshot ID");
    let source = home.join(format!("{id}-rootfs-source"));
    std::fs::create_dir_all(&source).expect("snapshot source rootfs");
    std::fs::write(source.join("captured.txt"), b"captured").expect("snapshot source marker");
    let mut metadata = a3s_box_core::SnapshotMetadata::new(
        id.to_string(),
        id.to_string(),
        "source-execution".to_string(),
        "alpine:3.20".to_string(),
    );
    metadata.image_config = Some(a3s_box_core::SnapshotImageConfig::default());
    crate::SnapshotStore::new(&home.join("snapshots"))
        .expect("snapshot store")
        .save(metadata, &source)
        .expect("published managed snapshot");
    snapshot_id
}

fn request(external_id: &str, isolation: ExecutionIsolation) -> CreateExecutionRequest {
    CreateExecutionRequest {
        external_sandbox_id: external_id.to_string(),
        config: BoxConfig {
            image: "alpine:3.20".to_string(),
            isolation,
            network: NetworkMode::None,
            resources: a3s_box_core::ResourceConfig {
                vcpus: 1,
                memory_mb: 128,
                disk_mb: 512,
                timeout: 300,
            },
            ..Default::default()
        },
        labels: BTreeMap::new(),
        policy: Default::default(),
        rootfs_snapshot_id: None,
    }
}

fn box_operation(value: &str) -> BoxOperationId {
    BoxOperationId::new(value).expect("Box operation ID")
}

fn box_exec_request(request_id: Option<&str>) -> BoxExecRequest {
    BoxExecRequest {
        request_id: request_id.map(str::to_string),
        cmd: vec!["/bin/echo".to_string(), "ready".to_string()],
        timeout_ns: 1_000_000_000,
        env: Vec::new(),
        working_dir: Some("/".to_string()),
        rootfs: None,
        stdin: None,
        stdin_streaming: false,
        user: None,
        streaming: false,
    }
}

fn runtime_info(dedicated_readiness: &str) -> RuntimeInfo {
    let platform = if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    serde_json::from_value(json!({
        "oci": {
            "ociVersionMin": "1.0.0",
            "ociVersionMax": "1.3.0"
        },
        "drivers": {
            "schema_version": "a3s.oci.features.v1",
            "platform": platform,
            "architecture": std::env::consts::ARCH,
            "drivers": [
                {
                    "driver": "native-linux",
                    "status": "available",
                    "readiness": "supported",
                    "isolation_classes": ["shared-host-kernel"],
                    "evidence": { "fake": "native" }
                },
                {
                    "driver": "libkrun-whpx",
                    "status": "available",
                    "readiness": dedicated_readiness,
                    "isolation_classes": ["dedicated-vm"],
                    "evidence": { "fake": "whpx" }
                }
            ]
        },
        "operations": [
            "features", "create", "state", "start", "kill", "delete", "wait",
            "pause", "resume", "exec", "read-output", "write-stdin", "close-stdin",
            "resize", "signal-process", "wait-process", "update", "processes", "stats",
            "events", "file", "filesystem"
        ],
        "attachments": {
            "schemas": ["a3s.oci.attachments.v1"],
            "extensions": {}
        }
    }))
    .expect("runtime feature fixture")
}

fn fake_filesystem_entry(path: &str, kind: OciFilesystemEntryKind) -> OciFilesystemEntry {
    OciFilesystemEntry {
        name: path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_string(),
        kind,
        path: path.to_string(),
        size: if kind == OciFilesystemEntryKind::File {
            10
        } else {
            0
        },
        mode: if kind == OciFilesystemEntryKind::Directory {
            0o755
        } else {
            0o644
        },
        permissions: if kind == OciFilesystemEntryKind::Directory {
            "drwxr-xr-x".to_string()
        } else {
            "-rw-r--r--".to_string()
        },
        owner: "root".to_string(),
        group: "root".to_string(),
        modified_seconds: 1_700_000_000,
        modified_nanos: 123_000_000,
        symlink_target: None,
        metadata: BTreeMap::from([("fake".to_string(), "true".to_string())]),
    }
}

fn selected_driver(request: IsolationRequest) -> (DriverKind, IsolationClass) {
    match request {
        IsolationRequest::DedicatedVm => (DriverKind::LibkrunWhpx, IsolationClass::DedicatedVm),
        IsolationRequest::SharedHostKernel => {
            (DriverKind::NativeLinux, IsolationClass::SharedHostKernel)
        }
        IsolationRequest::SharedGuestKernel { .. } => {
            panic!("the Box adapter does not request shared guest kernels")
        }
    }
}

fn fake_process_output(terminal: bool, held: bool) -> Vec<OutputChunk> {
    let mut output = Vec::new();
    append_output(
        &mut output,
        OutputStream::Stdout,
        if terminal {
            b"fake tty\n".to_vec()
        } else {
            b"fake stdout\n".to_vec()
        },
        false,
    );
    if !terminal {
        append_output(
            &mut output,
            OutputStream::Stderr,
            b"fake stderr\n".to_vec(),
            false,
        );
    }
    if !held {
        append_missing_eof(&mut output, OutputStream::Stdout);
        if !terminal {
            append_missing_eof(&mut output, OutputStream::Stderr);
        }
    }
    output
}

fn append_missing_eof(output: &mut Vec<OutputChunk>, stream: OutputStream) {
    if output
        .iter()
        .any(|chunk| chunk.stream == stream && chunk.eof)
    {
        return;
    }
    append_output(output, stream, Vec::new(), true);
}

fn append_output(output: &mut Vec<OutputChunk>, stream: OutputStream, data: Vec<u8>, eof: bool) {
    let previous = output.last().map_or(0, |chunk| chunk.sequence);
    let width = if eof { 1 } else { data.len() as u64 };
    output.push(OutputChunk {
        sequence: previous + width,
        stream,
        data,
        eof,
    });
}

fn validate_process_target(
    target: &ProcessTarget,
    record: &ProcessRecord,
    operation: &str,
) -> OciResult<()> {
    if target != &record.target {
        return Err(oci_error(
            ErrorCode::Conflict,
            operation,
            "fake process target does not match its exact generation",
        ));
    }
    Ok(())
}

fn runtime_record(
    id: &ContainerId,
    generation: Generation,
    status: ContainerState,
    driver: DriverKind,
    isolation: IsolationClass,
    config_digest: &str,
    attachments_digest: Option<&str>,
) -> OciResult<ContainerRecord> {
    let state = StateBuilder::default()
        .version("1.3.0")
        .id(id.as_str())
        .status(status)
        .pid(4242)
        .bundle(std::env::temp_dir().join("a3s-box-oci-backend-tests"))
        .build()
        .map_err(|error| Error::new(ErrorCode::Internal, error.to_string()))?;
    Ok(ContainerRecord {
        state,
        generation,
        driver,
        isolation,
        config_digest: config_digest.to_string(),
        attachments_digest: attachments_digest.map(str::to_string),
    })
}

fn rebuild_paused_record(record: &ContainerRecord, paused: bool) -> OciResult<ContainerRecord> {
    let mut annotations = record.state.annotations().clone().unwrap_or_default();
    if paused {
        annotations.insert(PAUSED_STATE_ANNOTATION.to_string(), "true".to_string());
    } else {
        annotations.remove(PAUSED_STATE_ANNOTATION);
    }
    let mut builder = StateBuilder::default()
        .version(record.state.version())
        .id(record.state.id())
        .status(*record.state.status())
        .bundle(record.state.bundle().clone())
        .annotations(annotations);
    if let Some(pid) = record.state.pid() {
        builder = builder.pid(*pid);
    }
    let mut updated = record.clone();
    updated.state = builder
        .build()
        .map_err(|error| Error::new(ErrorCode::Internal, error.to_string()))?;
    Ok(updated)
}

fn validate_target(
    target: &ContainerTarget,
    record: &ContainerRecord,
    operation: &str,
) -> OciResult<()> {
    if record.state.id() != target.id.as_str() {
        return Err(oci_error(
            ErrorCode::NotFound,
            operation,
            "fake target ID does not exist",
        ));
    }
    if target
        .generation
        .is_some_and(|generation| generation != record.generation)
    {
        return Err(oci_error(
            ErrorCode::FailedPrecondition,
            operation,
            "fake target generation is stale",
        ));
    }
    Ok(())
}

fn oci_error(code: ErrorCode, operation: &str, message: impl Into<String>) -> Error {
    Error::new(code, message).for_operation(operation)
}

fn lock_error<T>(operation: &str, error: std::sync::PoisonError<T>) -> Error {
    oci_error(ErrorCode::Internal, operation, error.to_string())
}

#[cfg(windows)]
fn test_endpoint() -> OciRuntimeEndpoint {
    OciRuntimeEndpoint::windows_named_pipe(r"\\.\pipe\a3s-box-oci-backend-tests")
        .expect("Windows named-pipe endpoint")
}

#[cfg(unix)]
fn test_endpoint() -> OciRuntimeEndpoint {
    OciRuntimeEndpoint::unix_socket(std::env::temp_dir().join("a3s-box-oci-backend-tests.sock"))
        .expect("Unix socket endpoint")
}

#[cfg(not(any(unix, windows)))]
fn test_endpoint() -> OciRuntimeEndpoint {
    OciRuntimeEndpoint::unix_socket(std::path::PathBuf::from("/a3s-box-oci-backend-tests.sock"))
        .expect("synthetic Unix socket endpoint")
}
