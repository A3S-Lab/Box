use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use a3s_box_core::{
    ExecutionId, ExecutionManagerError, ExecutionManagerResult, ExecutionState, KillOutcome,
};
use a3s_runtime::contract::{
    ArtifactRef, IsolationLevel, NetworkMode, ResourceLimits, RestartPolicy,
    RuntimeActionRequest, RuntimeNetworkSpec, RuntimeObservation, RuntimeProcessSpec,
    RuntimeUnitClass, RuntimeUnitSpec, RuntimeUnitState,
};
use a3s_runtime::RuntimeUnitRecord;
use async_trait::async_trait;

use crate::{
    BoxRecord, LocalExecutionBackend, LocalExecutionHandle, LocalExecutionManager,
    LocalExecutionObservation,
};

use super::{BoxRuntimeDriver, BoxRuntimeDriverConfig, OCI_IMAGE_MANIFEST};

#[derive(Clone)]
struct FakeExecution {
    state: ExecutionState,
    handle: LocalExecutionHandle,
    exit_code: Option<i32>,
}

#[derive(Default)]
pub(super) struct DriverFakeBackend {
    executions: Mutex<HashMap<String, FakeExecution>>,
    starts: AtomicUsize,
    kills: AtomicUsize,
    fail_start_after_effect: AtomicBool,
    next_start_terminal: Mutex<Option<(ExecutionState, i32)>>,
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
        Ok(LocalExecutionHandle {
            started_at: chrono::Utc::now(),
            pid: Some(pid),
            pid_start_time: crate::process::pid_start_time(pid),
            exec_socket_path: record.box_dir.join("sockets/exec.sock"),
            console_log: record.console_log.clone(),
            anonymous_volumes: Vec::new(),
        })
    }

    pub(super) fn fail_next_start_response(&self) {
        self.fail_start_after_effect.store(true, Ordering::SeqCst);
    }

    pub(super) fn finish_next_start(&self, exit_code: i32) {
        let state = if exit_code == 0 {
            ExecutionState::Stopped
        } else {
            ExecutionState::Failed
        };
        *self.next_start_terminal.lock().unwrap() = Some((state, exit_code));
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
}

#[async_trait]
impl LocalExecutionBackend for DriverFakeBackend {
    async fn start(&self, record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
        let handle = Self::handle(record)?;
        let mut executions = self.executions.lock().unwrap();
        if let Some(execution) = executions.get(&record.id) {
            if matches!(execution.state, ExecutionState::Running | ExecutionState::Paused) {
                return Ok(execution.handle.clone());
            }
        }
        self.starts.fetch_add(1, Ordering::SeqCst);
        let terminal = self.next_start_terminal.lock().unwrap().take();
        let (state, exit_code) = terminal
            .map(|(state, exit_code)| (state, Some(exit_code)))
            .unwrap_or((ExecutionState::Running, None));
        executions.insert(
            record.id.clone(),
            FakeExecution {
                state,
                handle: handle.clone(),
                exit_code,
            },
        );
        if self.fail_start_after_effect.swap(false, Ordering::SeqCst) {
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
        let executions = self.executions.lock().unwrap();
        let execution = executions
            .get(&record.id)
            .ok_or_else(|| ExecutionManagerError::NotFound(Self::execution_id(record)))?;
        Ok(LocalExecutionObservation {
            state: execution.state,
            handle: matches!(execution.state, ExecutionState::Running | ExecutionState::Paused)
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
        if matches!(execution.state, ExecutionState::Stopped | ExecutionState::Failed) {
            Ok(KillOutcome::AlreadyStopped)
        } else {
            Ok(KillOutcome::Killed)
        }
    }
}

pub(super) fn fake_driver(
    directory: &tempfile::TempDir,
) -> (BoxRuntimeDriver, Arc<DriverFakeBackend>) {
    let backend = Arc::new(DriverFakeBackend::default());
    let driver = fake_driver_with_backend(directory, backend.clone());
    (driver, backend)
}

pub(super) fn fake_driver_with_backend(
    directory: &tempfile::TempDir,
    backend: Arc<DriverFakeBackend>,
) -> BoxRuntimeDriver {
    let home_dir = directory.path().join("home");
    let manager = LocalExecutionManager::new(
        home_dir.join("boxes.json"),
        &home_dir,
        backend.clone(),
    );
    let driver = BoxRuntimeDriver::with_manager(
        BoxRuntimeDriverConfig {
            home_dir,
            control_timeout: Duration::from_secs(2),
            task_poll_interval: Duration::from_millis(5),
        },
        manager,
    )
    .unwrap();
    driver
        .provider_build
        .set("a3s-box/test crun/test sha256:0123456789abcdef".into())
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
        restart: if class == RuntimeUnitClass::Service {
            RestartPolicy::Always
        } else {
            RestartPolicy::Never
        },
        outputs: Vec::new(),
        semantics_profile_digest: None,
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
