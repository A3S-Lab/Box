use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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
const CHILD_TEST_NAME: &str = concat!(
    "local_execution::oci_backend::tests::process_restart::",
    "retained_backend_recovers_after_runtime_owner_process_restart"
);

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
}

struct RuntimeOwnerChild {
    child: Option<Child>,
    stderr_path: PathBuf,
}

impl RuntimeOwnerChild {
    fn spawn(
        state_path: &Path,
        endpoint: &OsStr,
        ready_path: &Path,
        call_log: &Path,
        stderr_path: PathBuf,
    ) -> Self {
        let stderr = std::fs::File::create(&stderr_path).expect("create owner stderr file");
        let child = Command::new(std::env::current_exe().expect("resolve runtime test executable"))
            .args(["--exact", CHILD_TEST_NAME, "--nocapture"])
            .env(CHILD_ENV, "1")
            .env(STATE_ENV, state_path)
            .env(ENDPOINT_ENV, endpoint)
            .env(READY_ENV, ready_path)
            .env(CALL_LOG_ENV, call_log)
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
            request("runtime-owner-process-restart", ExecutionIsolation::Sandbox),
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
