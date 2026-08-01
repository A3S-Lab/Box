use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use a3s_box_core::{
    BoxConfig, CreateExecutionRequest, ExecutionGeneration, ExecutionId, ExecutionIsolation,
    ExecutionManager, ExecutionManagerError, ExecutionState, KillExecutionOptions, KillOutcome,
    NetworkMode, OperationId as BoxOperationId, ReconcileOutcome,
};
use a3s_oci_sdk::oci_spec::runtime::{ContainerState, StateBuilder};
use a3s_oci_sdk::{
    async_trait, ContainerId, ContainerRecord, ContainerTarget, CreateRequest, DeleteMode,
    DeleteRequest, DriverKind, Error, ErrorCode, ExitStatus, Generation, IsolationClass,
    IsolationRequest, KillRequest, OciBundle, OciRuntimeService, Result as OciResult,
    RuntimeClient, RuntimeInfo, StartRequest, StateRequest, WaitRequest,
};
use chrono::Utc;
use serde_json::json;

use super::super::{build_managed_record, LocalExecutionManager, RuntimeUpdate};
use super::*;
use crate::{ManagedExecutionState, ManagedExecutionStore};

const RUNTIME_GENERATION: Generation = Generation(41);
const CONFIG_DIGEST: &str =
    "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[derive(Clone)]
struct FakeContainer {
    record: ContainerRecord,
    exit_status: Option<ExitStatus>,
}

struct FakeRuntimeService {
    info: RuntimeInfo,
    containers: Mutex<HashMap<String, FakeContainer>>,
    create_requests: Mutex<Vec<CreateRequest>>,
    start_requests: Mutex<Vec<StartRequest>>,
    kill_signals: Mutex<Vec<i32>>,
    delete_modes: Mutex<Vec<DeleteMode>>,
    create_digest_override: Mutex<Option<String>>,
    fail_create_after_effect: AtomicBool,
    fail_start_after_effect: AtomicBool,
    ignore_graceful_signal: AtomicBool,
}

impl FakeRuntimeService {
    fn launch_ready() -> Self {
        Self::with_dedicated_readiness("experimental")
    }

    fn probe_only() -> Self {
        Self::with_dedicated_readiness("probe-only")
    }

    fn with_dedicated_readiness(readiness: &str) -> Self {
        Self {
            info: runtime_info(readiness),
            containers: Mutex::new(HashMap::new()),
            create_requests: Mutex::new(Vec::new()),
            start_requests: Mutex::new(Vec::new()),
            kill_signals: Mutex::new(Vec::new()),
            delete_modes: Mutex::new(Vec::new()),
            create_digest_override: Mutex::new(None),
            fail_create_after_effect: AtomicBool::new(false),
            fail_start_after_effect: AtomicBool::new(false),
            ignore_graceful_signal: AtomicBool::new(false),
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
        let record = runtime_record(
            &request.id,
            RUNTIME_GENERATION,
            ContainerState::Created,
            driver,
            isolation,
            digest_override
                .as_deref()
                .unwrap_or_else(|| request.bundle.config_digest()),
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
}

#[async_trait]
impl OciBundleProvider for FakeBundleProvider {
    async fn prepare(&self, record: &BoxRecord) -> ExecutionManagerResult<OciPreparedExecution> {
        self.prepares.fetch_add(1, Ordering::SeqCst);
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
        let mut prepared = OciPreparedExecution::new(bundle, console_log);
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
    wrong_schema.schema_version = "a3s.box.oci-runtime-binding.v2".to_string();
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

    let mut drifted = record;
    drifted.driver = DriverKind::LibkrunKvm;
    assert!(decoded.validate_record(&drifted).is_err());
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
async fn invalid_preparation_is_cleaned_before_runtime_create() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    let provider = Arc::new(FakeBundleProvider::default());
    provider.invalid_console.store(true, Ordering::SeqCst);
    let manager = manager(
        &directory,
        test_endpoint(),
        service.clone(),
        provider.clone(),
    );

    let error = manager
        .create_and_start(
            request("invalid-preparation", ExecutionIsolation::Sandbox),
            &box_operation("invalid-preparation-operation"),
        )
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
        assert_eq!(persisted.pid, None);
        assert_eq!(persisted.pid_start_time, None);
        assert!(persisted.exec_socket_path.as_os_str().is_empty());
        assert_eq!(provider.prepares.load(Ordering::SeqCst), 1);
        assert_eq!(provider.cleanups.load(Ordering::SeqCst), 0);
    }
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

fn persisted(manager: &LocalExecutionManager, execution_id: &ExecutionId) -> BoxRecord {
    ManagedExecutionStore::new(manager.state_path().to_path_buf())
        .get(execution_id)
        .expect("read managed store")
        .expect("persisted execution")
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
            "features", "create", "state", "start", "kill", "delete", "wait"
        ]
    }))
    .expect("runtime feature fixture")
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

fn runtime_record(
    id: &ContainerId,
    generation: Generation,
    status: ContainerState,
    driver: DriverKind,
    isolation: IsolationClass,
    config_digest: &str,
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
    })
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
