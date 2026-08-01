use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::*;

const CHILD_ENV: &str = "A3S_BOX_TEST_RUNTIME_OWNER_CHILD";
const STATE_ENV: &str = "A3S_BOX_TEST_RUNTIME_OWNER_STATE";
const ENDPOINT_ENV: &str = "A3S_BOX_TEST_RUNTIME_OWNER_ENDPOINT";
const READY_ENV: &str = "A3S_BOX_TEST_RUNTIME_OWNER_READY";
const CALL_LOG_ENV: &str = "A3S_BOX_TEST_RUNTIME_OWNER_CALL_LOG";
const CHILD_TEST_NAME: &str = concat!(
    "local_execution::oci_backend::tests::process_restart::",
    "retained_backend_recovers_after_runtime_owner_process_restart"
);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DurableFixtureState {
    record: ContainerRecord,
    create_operation: String,
    start_operation: Option<String>,
    exit_status: Option<ExitStatus>,
}

struct DurableFixtureService {
    state_path: PathBuf,
    call_log: PathBuf,
    lock: Mutex<()>,
}

impl DurableFixtureService {
    fn new(state_path: PathBuf, call_log: PathBuf) -> Self {
        Self {
            state_path,
            call_log,
            lock: Mutex::new(()),
        }
    }

    fn load(&self, operation: &str) -> OciResult<Option<DurableFixtureState>> {
        match std::fs::read(&self.state_path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
                oci_error(
                    ErrorCode::Internal,
                    operation,
                    format!(
                        "failed to decode durable process fixture {}: {error}",
                        self.state_path.display()
                    ),
                )
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(oci_error(
                ErrorCode::Internal,
                operation,
                format!(
                    "failed to read durable process fixture {}: {error}",
                    self.state_path.display()
                ),
            )),
        }
    }

    fn store(&self, state: &DurableFixtureState, operation: &str) -> OciResult<()> {
        let bytes = serde_json::to_vec(state).map_err(|error| {
            oci_error(
                ErrorCode::Internal,
                operation,
                format!("failed to encode durable process fixture: {error}"),
            )
        })?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&self.state_path)
            .map_err(|error| {
                oci_error(
                    ErrorCode::Internal,
                    operation,
                    format!(
                        "failed to open durable process fixture {}: {error}",
                        self.state_path.display()
                    ),
                )
            })?;
        file.write_all(&bytes).map_err(|error| {
            oci_error(
                ErrorCode::Internal,
                operation,
                format!("failed to write durable process fixture: {error}"),
            )
        })?;
        file.sync_all().map_err(|error| {
            oci_error(
                ErrorCode::Internal,
                operation,
                format!("failed to sync durable process fixture: {error}"),
            )
        })
    }

    fn append_call(&self, operation: &'static str) -> OciResult<()> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.call_log)
            .map_err(|error| {
                oci_error(
                    ErrorCode::Internal,
                    operation,
                    format!(
                        "failed to open process fixture call log {}: {error}",
                        self.call_log.display()
                    ),
                )
            })?;
        writeln!(file, "{operation}").map_err(|error| {
            oci_error(
                ErrorCode::Internal,
                operation,
                format!("failed to append process fixture call log: {error}"),
            )
        })?;
        file.sync_all().map_err(|error| {
            oci_error(
                ErrorCode::Internal,
                operation,
                format!("failed to sync process fixture call log: {error}"),
            )
        })
    }

    fn rewritten_record(
        state: &DurableFixtureState,
        status: ContainerState,
    ) -> OciResult<ContainerRecord> {
        let id = ContainerId::new(state.record.state.id().to_string())?;
        runtime_record(
            &id,
            state.record.generation,
            status,
            state.record.driver,
            state.record.isolation,
            &state.record.config_digest,
            state.record.attachments_digest.as_deref(),
        )
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

    first_owner.terminate();
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
    let calls = std::fs::read_to_string(&call_log).expect("read runtime-owner call log");
    assert_eq!(calls.lines().filter(|call| *call == "create").count(), 1);
    assert_eq!(calls.lines().filter(|call| *call == "start").count(), 1);

    second_owner.terminate();
    drop(manager);
}
