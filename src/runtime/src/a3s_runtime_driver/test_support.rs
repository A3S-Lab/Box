use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use a3s_box_core::config::SevSnpGeneration;
use a3s_box_core::{
    ExecutionId, ExecutionManagerError, ExecutionManagerResult, ExecutionPortConnector,
    ExecutionState, KillOutcome,
};
use a3s_runtime::contract::{
    ArtifactRef, IsolationLevel, NetworkMode, ResourceLimits, RestartPolicy, RuntimeActionRequest,
    RuntimeNetworkSpec, RuntimeObservation, RuntimeProcessSpec, RuntimeUnitClass, RuntimeUnitSpec,
    RuntimeUnitState, SecretReference,
};
use a3s_runtime::{RuntimeError, RuntimeResult, RuntimeUnitRecord};
use async_trait::async_trait;
use tokio::sync::{oneshot, Semaphore};

use crate::local_execution::TransientRegistryAuthBroker;
use crate::tee::{
    build_simulated_report, parse_platform_info, AttestationPolicy, AttestationReport,
    CertificateChain,
};
use crate::{
    BoxRecord, LocalExecutionBackend, LocalExecutionHandle, LocalExecutionManager,
    LocalExecutionObservation,
};

use super::attestation::{BoxAttestationPayload, BoxAttestationTransport};
use super::{
    BoxRegistryCredential, BoxRuntimeDriver, BoxRuntimeDriverConfig, BoxRuntimeSevSnpConfig,
    BoxSecretMaterial, BoxSecretMaterializationError, BoxSecretMaterializer, OCI_IMAGE_MANIFEST,
};

#[derive(Clone)]
struct FakeExecution {
    state: ExecutionState,
    handle: LocalExecutionHandle,
    exit_code: Option<i32>,
}

struct CancelledInspection {
    claimed: AtomicBool,
    completed: AtomicBool,
    started: Semaphore,
    release: Semaphore,
}

impl CancelledInspection {
    fn new() -> Self {
        Self {
            claimed: AtomicBool::new(false),
            completed: AtomicBool::new(false),
            started: Semaphore::new(0),
            release: Semaphore::new(0),
        }
    }
}

#[derive(Default)]
pub(super) struct DriverFakeBackend {
    executions: Arc<Mutex<HashMap<String, FakeExecution>>>,
    starts: AtomicUsize,
    kills: AtomicUsize,
    fail_start_after_effect: AtomicBool,
    fail_start_after_effect_at: AtomicUsize,
    next_start_terminal: Mutex<VecDeque<(ExecutionState, Option<i32>)>>,
    next_start_writes: Mutex<VecDeque<(PathBuf, Vec<u8>)>>,
    cancelled_inspection: Mutex<Option<Arc<CancelledInspection>>>,
}

impl DriverFakeBackend {
    fn execution_id(record: &BoxRecord) -> ExecutionId {
        ExecutionId::new(record.id.clone()).unwrap()
    }

    fn handle(record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
        if let Some(parent) = record.console_log.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                ExecutionManagerError::Internal(format!(
                    "failed to create fake execution log directory: {error}"
                ))
            })?;
        }
        if !record.console_log.exists() {
            std::fs::write(&record.console_log, []).map_err(|error| {
                ExecutionManagerError::Internal(format!(
                    "failed to create fake execution log: {error}"
                ))
            })?;
        }
        let pid = std::process::id();
        let generation = record
            .managed_execution
            .as_ref()
            .expect("fake managed execution metadata")
            .generation
            .get();
        let generation_seconds = i64::try_from(generation).expect("fake execution generation");
        let started_at = record.started_at.unwrap_or_else(|| {
            chrono::DateTime::<chrono::Utc>::from_timestamp(1_784_031_000, 0)
                .expect("fake start timestamp")
                + chrono::Duration::seconds(generation_seconds)
        });
        Ok(LocalExecutionHandle {
            started_at,
            pid: Some(pid),
            pid_start_time: crate::process::pid_start_time(pid),
            exec_socket_path: record.box_dir.join("sockets/exec.sock"),
            console_log: record.console_log.clone(),
            anonymous_volumes: Vec::new(),
            oci_runtime: None,
        })
    }

    pub(super) fn fail_next_start_response(&self) {
        self.fail_start_after_effect.store(true, Ordering::SeqCst);
    }

    pub(super) fn fail_start_response_at(&self, start_number: usize) {
        assert!(start_number > 0);
        self.fail_start_after_effect_at
            .store(start_number, Ordering::SeqCst);
    }

    pub(super) fn fail_next_start_without_exit_code(&self) {
        self.next_start_terminal
            .lock()
            .unwrap()
            .push_back((ExecutionState::Failed, None));
        self.fail_start_after_effect.store(true, Ordering::SeqCst);
    }

    pub(super) fn finish_next_start(&self, exit_code: i32) {
        let state = if exit_code == 0 {
            ExecutionState::Stopped
        } else {
            ExecutionState::Failed
        };
        self.next_start_terminal
            .lock()
            .unwrap()
            .push_back((state, Some(exit_code)));
    }

    pub(super) fn write_on_next_start(&self, path: PathBuf, value: impl Into<Vec<u8>>) {
        self.next_start_writes
            .lock()
            .unwrap()
            .push_back((path, value.into()));
    }

    pub(super) fn finish(&self, execution_id: &str, exit_code: i32) {
        let mut executions = self.executions.lock().unwrap();
        let execution = executions.get_mut(execution_id).unwrap();
        execution.state = if exit_code == 0 {
            ExecutionState::Stopped
        } else {
            ExecutionState::Failed
        };
        execution.exit_code = Some(exit_code);
    }

    pub(super) fn lose(&self, execution_id: &str) {
        self.executions.lock().unwrap().remove(execution_id);
    }

    pub(super) fn starts(&self) -> usize {
        self.starts.load(Ordering::SeqCst)
    }

    pub(super) fn kills(&self) -> usize {
        self.kills.load(Ordering::SeqCst)
    }

    pub(super) fn arm_cancelled_inspection(&self) {
        *self.cancelled_inspection.lock().unwrap() = Some(Arc::new(CancelledInspection::new()));
    }

    pub(super) async fn wait_for_cancelled_inspection(&self) {
        let inspection = self
            .cancelled_inspection
            .lock()
            .unwrap()
            .clone()
            .expect("cancelled inspection must be armed");
        inspection
            .started
            .acquire()
            .await
            .expect("cancelled inspection start semaphore must remain open")
            .forget();
    }

    pub(super) fn release_cancelled_inspection(&self) {
        self.cancelled_inspection
            .lock()
            .unwrap()
            .as_ref()
            .expect("cancelled inspection must be armed")
            .release
            .add_permits(1);
    }
}

#[async_trait]
impl LocalExecutionBackend for DriverFakeBackend {
    async fn start(&self, record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
        let handle = Self::handle(record)?;
        if let Some((path, value)) = self.next_start_writes.lock().unwrap().pop_front() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    ExecutionManagerError::Internal(format!(
                        "failed to create fake Task-output directory: {error}"
                    ))
                })?;
            }
            std::fs::write(path, value).map_err(|error| {
                ExecutionManagerError::Internal(format!(
                    "failed to write fake Task output: {error}"
                ))
            })?;
        }
        let mut executions = self.executions.lock().unwrap();
        if let Some(execution) = executions.get(&record.id) {
            if matches!(
                execution.state,
                ExecutionState::Running | ExecutionState::Paused
            ) {
                return Ok(execution.handle.clone());
            }
        }
        let start_number = self.starts.fetch_add(1, Ordering::SeqCst) + 1;
        let terminal = self.next_start_terminal.lock().unwrap().pop_front();
        let (state, exit_code) = terminal.unwrap_or((ExecutionState::Running, None));
        executions.insert(
            record.id.clone(),
            FakeExecution {
                state,
                handle: handle.clone(),
                exit_code,
            },
        );
        let failed_at_scheduled_start = self
            .fail_start_after_effect_at
            .compare_exchange(start_number, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if self.fail_start_after_effect.swap(false, Ordering::SeqCst) || failed_at_scheduled_start {
            return Err(ExecutionManagerError::Unavailable(
                "fake start response was lost".into(),
            ));
        }
        Ok(handle)
    }

    async fn inspect(
        &self,
        record: &BoxRecord,
    ) -> ExecutionManagerResult<LocalExecutionObservation> {
        let cancelled_inspection = self.cancelled_inspection.lock().unwrap().clone();
        if let Some(inspection) = cancelled_inspection {
            if inspection
                .claimed
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                let executions = Arc::clone(&self.executions);
                let execution_id = record.id.clone();
                let inspection_task = Arc::clone(&inspection);
                let (completed_tx, completed_rx) = oneshot::channel();
                tokio::spawn(async move {
                    inspection_task.started.add_permits(1);
                    inspection_task
                        .release
                        .acquire()
                        .await
                        .expect("cancelled inspection release semaphore must remain open")
                        .forget();
                    if let Some(execution) = executions.lock().unwrap().get_mut(&execution_id) {
                        execution.state = ExecutionState::Stopped;
                        execution.exit_code = Some(0);
                    }
                    inspection_task.completed.store(true, Ordering::SeqCst);
                    let _ = completed_tx.send(());
                });
                completed_rx.await.map_err(|error| {
                    ExecutionManagerError::Internal(format!(
                        "cancelled fake inspection task failed: {error}"
                    ))
                })?;
            } else if !inspection.completed.load(Ordering::SeqCst) {
                return Err(ExecutionManagerError::NotFound(Self::execution_id(record)));
            }
        }
        let executions = self.executions.lock().unwrap();
        let execution = executions
            .get(&record.id)
            .ok_or_else(|| ExecutionManagerError::NotFound(Self::execution_id(record)))?;
        Ok(LocalExecutionObservation {
            state: execution.state,
            handle: matches!(
                execution.state,
                ExecutionState::Running | ExecutionState::Paused
            )
            .then(|| execution.handle.clone()),
            exit_code: execution.exit_code,
        })
    }

    async fn pause(
        &self,
        record: &BoxRecord,
        _keep_memory: bool,
    ) -> ExecutionManagerResult<LocalExecutionHandle> {
        Err(ExecutionManagerError::Unavailable(format!(
            "fake pause is unavailable for {}",
            record.id
        )))
    }

    async fn resume(&self, record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
        Err(ExecutionManagerError::Unavailable(format!(
            "fake resume is unavailable for {}",
            record.id
        )))
    }

    async fn kill(&self, record: &BoxRecord) -> ExecutionManagerResult<KillOutcome> {
        self.kills.fetch_add(1, Ordering::SeqCst);
        let Some(execution) = self.executions.lock().unwrap().remove(&record.id) else {
            return Err(ExecutionManagerError::NotFound(Self::execution_id(record)));
        };
        if matches!(
            execution.state,
            ExecutionState::Stopped | ExecutionState::Failed
        ) {
            Ok(KillOutcome::AlreadyStopped)
        } else {
            Ok(KillOutcome::Killed)
        }
    }
}

#[derive(Default)]
pub(super) struct DriverFakeSecretMaterializer {
    materials: Mutex<HashMap<String, Vec<u8>>>,
    registry_credentials: Mutex<HashMap<String, (String, String)>>,
    calls: AtomicUsize,
    failures: AtomicUsize,
}

impl DriverFakeSecretMaterializer {
    pub(super) fn insert(&self, reference: impl Into<String>, value: impl Into<Vec<u8>>) {
        self.materials
            .lock()
            .unwrap()
            .insert(reference.into(), value.into());
    }

    pub(super) fn fail_next(&self) {
        self.failures.fetch_add(1, Ordering::SeqCst);
    }

    pub(super) fn insert_registry_credential(
        &self,
        reference: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) {
        self.registry_credentials
            .lock()
            .unwrap()
            .insert(reference.into(), (username.into(), password.into()));
    }

    pub(super) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn should_fail(&self) -> bool {
        self.failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                (remaining > 0).then(|| remaining - 1)
            })
            .is_ok()
    }
}

#[async_trait]
impl BoxSecretMaterializer for DriverFakeSecretMaterializer {
    async fn materialize(
        &self,
        reference: &SecretReference,
    ) -> Result<BoxSecretMaterial, BoxSecretMaterializationError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.should_fail() {
            return Err(BoxSecretMaterializationError::Unavailable(
                "injected retryable fixture failure".into(),
            ));
        }
        let value = self
            .materials
            .lock()
            .unwrap()
            .get(&reference.reference)
            .cloned()
            .ok_or_else(|| {
                BoxSecretMaterializationError::Rejected(
                    "fixture reference is not registered".into(),
                )
            })?;
        BoxSecretMaterial::new(value)
    }

    async fn materialize_registry_credential(
        &self,
        reference: &SecretReference,
        _registry: &str,
    ) -> Result<BoxRegistryCredential, BoxSecretMaterializationError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.should_fail() {
            return Err(BoxSecretMaterializationError::Unavailable(
                "injected retryable fixture failure".into(),
            ));
        }
        let (username, password) = self
            .registry_credentials
            .lock()
            .unwrap()
            .get(&reference.reference)
            .cloned()
            .ok_or_else(|| {
                BoxSecretMaterializationError::Rejected(
                    "fixture registry reference is not registered".into(),
                )
            })?;
        BoxRegistryCredential::new(username, password)
    }
}

#[derive(Default)]
pub(super) struct DriverFakeAttestationTransport {
    calls: AtomicUsize,
    fail_next: AtomicBool,
    epoch: AtomicUsize,
}

impl DriverFakeAttestationTransport {
    pub(super) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub(super) fn fail_next(&self) {
        self.fail_next.store(true, Ordering::SeqCst);
    }

    pub(super) fn rotate(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl BoxAttestationTransport for DriverFakeAttestationTransport {
    async fn fetch_report(
        &self,
        _socket_path: &Path,
        _policy: &AttestationPolicy,
        allow_simulated: bool,
        expected_runtime_binding: &[u8; 32],
    ) -> RuntimeResult<BoxAttestationPayload> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(RuntimeError::ProviderUnavailable(
                "injected RA-TLS fixture failure".into(),
            ));
        }
        if !allow_simulated {
            return Err(RuntimeError::ProviderUnavailable(
                "fixture exposes only simulated SEV-SNP reports".into(),
            ));
        }
        let mut report_data = [0u8; 64];
        report_data[..8].copy_from_slice(&(self.epoch.load(Ordering::SeqCst) as u64).to_be_bytes());
        report_data[32..].copy_from_slice(expected_runtime_binding);
        let report = build_simulated_report(&report_data);
        Ok(BoxAttestationPayload {
            report: AttestationReport {
                platform: parse_platform_info(&report).unwrap(),
                report,
                cert_chain: CertificateChain::default(),
            },
            certificate_der: Vec::new(),
        })
    }
}

pub(super) fn fake_driver(
    directory: &tempfile::TempDir,
) -> (BoxRuntimeDriver, Arc<DriverFakeBackend>) {
    let backend = Arc::new(DriverFakeBackend::default());
    let driver = fake_driver_with_backend(directory, backend.clone());
    (driver, backend)
}

pub(super) fn fake_confidential_driver(
    directory: &tempfile::TempDir,
) -> (
    BoxRuntimeDriver,
    Arc<DriverFakeBackend>,
    Arc<DriverFakeAttestationTransport>,
) {
    let backend = Arc::new(DriverFakeBackend::default());
    let transport = Arc::new(DriverFakeAttestationTransport::default());
    let driver = fake_confidential_driver_with_backend_and_attestation(
        directory,
        backend.clone(),
        transport.clone(),
    );
    (driver, backend, transport)
}

pub(super) fn fake_confidential_driver_with_backend_and_attestation<B>(
    directory: &tempfile::TempDir,
    backend: Arc<B>,
    transport: Arc<DriverFakeAttestationTransport>,
) -> BoxRuntimeDriver
where
    B: LocalExecutionBackend + 'static,
{
    let home_dir = directory.path().join("home");
    let manager = LocalExecutionManager::new(home_dir.join("boxes.json"), &home_dir, backend);
    let connector: Arc<dyn ExecutionPortConnector> = Arc::new(manager.clone());
    let mut driver = BoxRuntimeDriver::with_manager_connector_and_materializer(
        BoxRuntimeDriverConfig {
            secret_root: home_dir.join("runtime-secrets"),
            home_dir,
            control_timeout: Duration::from_secs(2),
            task_poll_interval: Duration::from_millis(5),
        },
        manager,
        connector,
        a3s_box_core::ExecutionIsolation::Microvm,
        None,
        None,
        Some(TransientRegistryAuthBroker::default()),
    )
    .unwrap()
    .with_attestation_transport(transport);
    driver.sev_snp = Some(BoxRuntimeSevSnpConfig {
        generation: SevSnpGeneration::Milan,
        simulate: true,
        attestation_policy: AttestationPolicy::default(),
    });
    driver
        .provider_build
        .set("a3s-box/test isolation/microvm hypervisor/test tee/sev-snp-simulated".into())
        .unwrap();
    driver
}

pub(super) fn fake_driver_with_backend<B>(
    directory: &tempfile::TempDir,
    backend: Arc<B>,
) -> BoxRuntimeDriver
where
    B: LocalExecutionBackend + 'static,
{
    let home_dir = directory.path().join("home");
    let manager =
        LocalExecutionManager::new(home_dir.join("boxes.json"), &home_dir, backend.clone());
    let connector: Arc<dyn ExecutionPortConnector> = Arc::new(manager.clone());
    configured_driver(home_dir, manager, connector)
}

pub(super) fn fake_driver_with_backend_and_connector<B, C>(
    directory: &tempfile::TempDir,
    backend: Arc<B>,
    connector: Arc<C>,
) -> BoxRuntimeDriver
where
    B: LocalExecutionBackend + 'static,
    C: ExecutionPortConnector + 'static,
{
    let home_dir = directory.path().join("home");
    let manager = LocalExecutionManager::new(home_dir.join("boxes.json"), &home_dir, backend);
    configured_driver(home_dir, manager, connector)
}

pub(super) fn fake_driver_with_secret_materializer(
    directory: &tempfile::TempDir,
    secret_root: std::path::PathBuf,
    materializer: Arc<dyn BoxSecretMaterializer>,
) -> (BoxRuntimeDriver, Arc<DriverFakeBackend>) {
    let backend = Arc::new(DriverFakeBackend::default());
    let driver = fake_driver_with_backend_and_secret_materializer(
        directory,
        backend.clone(),
        secret_root,
        materializer,
    );
    (driver, backend)
}

pub(super) fn fake_driver_with_backend_and_secret_materializer<B>(
    directory: &tempfile::TempDir,
    backend: Arc<B>,
    secret_root: std::path::PathBuf,
    materializer: Arc<dyn BoxSecretMaterializer>,
) -> BoxRuntimeDriver
where
    B: LocalExecutionBackend + 'static,
{
    let home_dir = directory.path().join("home");
    let manager = LocalExecutionManager::new(home_dir.join("boxes.json"), &home_dir, backend);
    let connector: Arc<dyn ExecutionPortConnector> = Arc::new(manager.clone());
    configured_driver_with_materializer(
        home_dir,
        secret_root,
        manager,
        connector,
        Some(materializer),
    )
}

fn configured_driver(
    home_dir: std::path::PathBuf,
    manager: LocalExecutionManager,
    connector: Arc<dyn ExecutionPortConnector>,
) -> BoxRuntimeDriver {
    let secret_root = home_dir.join("runtime-secrets");
    configured_driver_with_materializer(home_dir, secret_root, manager, connector, None)
}

fn configured_driver_with_materializer(
    home_dir: std::path::PathBuf,
    secret_root: std::path::PathBuf,
    manager: LocalExecutionManager,
    connector: Arc<dyn ExecutionPortConnector>,
    materializer: Option<Arc<dyn BoxSecretMaterializer>>,
) -> BoxRuntimeDriver {
    let driver = BoxRuntimeDriver::with_manager_connector_and_materializer(
        BoxRuntimeDriverConfig {
            secret_root,
            home_dir,
            control_timeout: Duration::from_secs(2),
            task_poll_interval: Duration::from_millis(5),
        },
        manager,
        connector,
        a3s_box_core::ExecutionIsolation::Microvm,
        materializer,
        None,
        Some(TransientRegistryAuthBroker::default()),
    )
    .unwrap();
    driver
        .provider_build
        .set("a3s-box/test isolation/microvm hypervisor/test".into())
        .unwrap();
    driver
}

pub(super) fn runtime_spec(
    unit_id: &str,
    generation: u64,
    class: RuntimeUnitClass,
) -> RuntimeUnitSpec {
    let digest = format!("sha256:{}", "a".repeat(64));
    RuntimeUnitSpec {
        schema: RuntimeUnitSpec::SCHEMA.into(),
        unit_id: unit_id.into(),
        generation,
        class,
        artifact: ArtifactRef {
            uri: format!("oci://registry.example/a3s/runtime@{digest}"),
            digest,
            media_type: OCI_IMAGE_MANIFEST.into(),
        },
        process: RuntimeProcessSpec {
            command: vec!["/bin/sh".into()],
            args: vec!["-c".into(), "echo ready".into()],
            working_directory: Some("/work".into()),
            environment: BTreeMap::new(),
        },
        mounts: Vec::new(),
        secrets: Vec::new(),
        network: RuntimeNetworkSpec {
            mode: NetworkMode::None,
            ports: Vec::new(),
        },
        resources: ResourceLimits {
            cpu_millis: 500,
            memory_bytes: 64 * 1024 * 1024,
            pids: 32,
            ephemeral_storage_bytes: None,
            execution_timeout_ms: (class == RuntimeUnitClass::Task).then_some(200),
        },
        isolation: IsolationLevel::Sandbox,
        health: None,
        service_lifecycle: None,
        restart: if class == RuntimeUnitClass::Service {
            RestartPolicy::Always
        } else {
            RestartPolicy::Never
        },
        outputs: Vec::new(),
        semantics_profile_digest: None,
        identity_attachment_digest: None,
    }
}

pub(super) fn accepted(spec: &RuntimeUnitSpec) -> RuntimeObservation {
    RuntimeObservation {
        schema: RuntimeObservation::SCHEMA.into(),
        unit_id: spec.unit_id.clone(),
        generation: spec.generation,
        spec_digest: spec.digest().unwrap(),
        class: spec.class,
        state: RuntimeUnitState::Accepted,
        provider_resource_id: None,
        provider_build: None,
        observed_at_ms: 1,
        started_at_ms: None,
        finished_at_ms: None,
        health: None,
        liveness: None,
        outputs: Vec::new(),
        usage: None,
        evidence: None,
        provider_attestation: None,
        failure: None,
    }
}

pub(super) fn unknown(previous: &RuntimeObservation) -> RuntimeObservation {
    let mut observation = previous.clone();
    observation.state = RuntimeUnitState::Unknown;
    observation.finished_at_ms = None;
    observation.failure = None;
    observation.clear_service_endpoints();
    observation
}

pub(super) fn unit(spec: RuntimeUnitSpec, observation: RuntimeObservation) -> RuntimeUnitRecord {
    RuntimeUnitRecord {
        schema: RuntimeUnitRecord::SCHEMA.into(),
        spec,
        observation,
        removed_at_ms: None,
    }
}

pub(super) fn action(request_id: &str, spec: &RuntimeUnitSpec) -> RuntimeActionRequest {
    RuntimeActionRequest {
        schema: RuntimeActionRequest::SCHEMA.into(),
        request_id: request_id.into(),
        unit_id: spec.unit_id.clone(),
        generation: spec.generation,
        deadline_at_ms: None,
    }
}
