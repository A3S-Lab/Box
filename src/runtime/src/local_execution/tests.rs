use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use a3s_box_core::{
    BoxConfig, CreateExecutionRequest, ExecutionGeneration, ExecutionId, ExecutionIsolation,
    ExecutionManager, ExecutionManagerError, ExecutionManagerResult, ExecutionState, KillOutcome,
    NetworkMode, OperationId, ReconcileOutcome,
};
use async_trait::async_trait;
use chrono::{TimeZone, Utc};

use super::*;
use crate::{ManagedExecutionState, ManagedExecutionStore};

#[derive(Clone)]
struct FakeExecution {
    state: ExecutionState,
    handle: LocalExecutionHandle,
    exit_code: Option<i32>,
}

#[derive(Default)]
struct FakeBackend {
    executions: Mutex<HashMap<String, FakeExecution>>,
    starts: AtomicUsize,
    pauses: AtomicUsize,
    resumes: AtomicUsize,
    kills: AtomicUsize,
    fail_pause: AtomicBool,
    fail_pause_after_effect: AtomicBool,
    last_keep_memory: Mutex<Option<bool>>,
}

impl FakeBackend {
    fn handle(record: &BoxRecord) -> LocalExecutionHandle {
        LocalExecutionHandle {
            started_at: Utc.with_ymd_and_hms(2026, 7, 14, 12, 30, 0).unwrap(),
            pid: Some(4242),
            pid_start_time: Some(777),
            exec_socket_path: record.box_dir.join("sockets/exec.sock"),
            console_log: record.box_dir.join("logs/console.log"),
            anonymous_volumes: vec!["anonymous-1".to_string()],
        }
    }

    fn execution_id(record: &BoxRecord) -> ExecutionId {
        ExecutionId::new(record.id.clone()).unwrap()
    }

    fn stop_externally(&self, execution_id: &ExecutionId, exit_code: i32) {
        let mut executions = self.executions.lock().unwrap();
        let execution = executions.get_mut(execution_id.as_str()).unwrap();
        execution.state = ExecutionState::Stopped;
        execution.exit_code = Some(exit_code);
    }
}

#[async_trait]
impl LocalExecutionBackend for FakeBackend {
    async fn start(&self, record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
        let mut executions = self.executions.lock().unwrap();
        if let Some(execution) = executions.get(&record.id) {
            return match execution.state {
                ExecutionState::Running | ExecutionState::Paused => Ok(execution.handle.clone()),
                state => Err(ExecutionManagerError::Conflict {
                    execution_id: Self::execution_id(record),
                    message: format!("fake execution is {state:?}"),
                }),
            };
        }
        self.starts.fetch_add(1, Ordering::Relaxed);
        let handle = Self::handle(record);
        executions.insert(
            record.id.clone(),
            FakeExecution {
                state: ExecutionState::Running,
                handle: handle.clone(),
                exit_code: None,
            },
        );
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
        keep_memory: bool,
    ) -> ExecutionManagerResult<LocalExecutionHandle> {
        self.pauses.fetch_add(1, Ordering::Relaxed);
        *self.last_keep_memory.lock().unwrap() = Some(keep_memory);
        if self.fail_pause.load(Ordering::Relaxed) {
            return Err(ExecutionManagerError::Unavailable(
                "fake pause is unavailable".to_string(),
            ));
        }
        let mut executions = self.executions.lock().unwrap();
        let execution = executions
            .get_mut(&record.id)
            .ok_or_else(|| ExecutionManagerError::NotFound(Self::execution_id(record)))?;
        if execution.state != ExecutionState::Running {
            return Err(ExecutionManagerError::Conflict {
                execution_id: Self::execution_id(record),
                message: "fake execution is not running".to_string(),
            });
        }
        execution.state = ExecutionState::Paused;
        if self.fail_pause_after_effect.load(Ordering::Relaxed) {
            return Err(ExecutionManagerError::Unavailable(
                "fake pause response was lost".to_string(),
            ));
        }
        Ok(execution.handle.clone())
    }

    async fn resume(&self, record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
        self.resumes.fetch_add(1, Ordering::Relaxed);
        let mut executions = self.executions.lock().unwrap();
        let execution = executions
            .get_mut(&record.id)
            .ok_or_else(|| ExecutionManagerError::NotFound(Self::execution_id(record)))?;
        if execution.state != ExecutionState::Paused {
            return Err(ExecutionManagerError::Conflict {
                execution_id: Self::execution_id(record),
                message: "fake execution is not paused".to_string(),
            });
        }
        execution.state = ExecutionState::Running;
        Ok(execution.handle.clone())
    }

    async fn kill(&self, record: &BoxRecord) -> ExecutionManagerResult<KillOutcome> {
        self.kills.fetch_add(1, Ordering::Relaxed);
        let mut executions = self.executions.lock().unwrap();
        let execution = executions
            .get_mut(&record.id)
            .ok_or_else(|| ExecutionManagerError::NotFound(Self::execution_id(record)))?;
        if execution.state == ExecutionState::Stopped {
            return Ok(KillOutcome::AlreadyStopped);
        }
        execution.state = ExecutionState::Stopped;
        Ok(KillOutcome::Killed)
    }
}

fn harness() -> (tempfile::TempDir, LocalExecutionManager, Arc<FakeBackend>) {
    let directory = tempfile::tempdir().unwrap();
    let backend = Arc::new(FakeBackend::default());
    let manager = LocalExecutionManager::new(
        directory.path().join("boxes.json"),
        directory.path().join("home"),
        backend.clone(),
    );
    (directory, manager, backend)
}

fn request(external_id: &str) -> CreateExecutionRequest {
    let mut labels = BTreeMap::new();
    labels.insert("purpose".to_string(), "test".to_string());
    CreateExecutionRequest {
        external_sandbox_id: external_id.to_string(),
        config: BoxConfig {
            image: "alpine:3.20".to_string(),
            isolation: ExecutionIsolation::Sandbox,
            network: NetworkMode::None,
            resources: a3s_box_core::ResourceConfig {
                vcpus: 1,
                memory_mb: 128,
                disk_mb: 512,
                timeout: 300,
            },
            ..Default::default()
        },
        labels,
    }
}

fn operation(value: &str) -> OperationId {
    OperationId::new(value).unwrap()
}

fn persisted(manager: &LocalExecutionManager, execution_id: &ExecutionId) -> BoxRecord {
    ManagedExecutionStore::new(manager.state_path().to_path_buf())
        .get(execution_id)
        .unwrap()
        .unwrap()
}

async fn reserve_starting(
    manager: &LocalExecutionManager,
    execution_id: &ExecutionId,
    operation_id: &OperationId,
) -> BoxRecord {
    let record = build_managed_record(
        &manager.home_dir,
        execution_id,
        operation_id.clone(),
        request("external-recovery-id"),
        Utc::now(),
    )
    .unwrap();
    let record = manager.reserve(record).await.unwrap().into_record();
    manager
        .transition(
            &record,
            ManagedExecutionState::Creating,
            ManagedExecutionState::Starting,
            RuntimeUpdate::None,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn create_persists_trusted_identity_and_returns_running_lease() {
    let (directory, manager, backend) = harness();
    let operation_id = operation("operation-1");

    let lease = manager
        .create_and_start(request("external/../../sandbox"), &operation_id)
        .await
        .unwrap();
    let status = manager.inspect(&lease.execution_id).await.unwrap();
    let record = persisted(&manager, &lease.execution_id);

    assert_eq!(status.state, ExecutionState::Running);
    assert_eq!(status.generation, ExecutionGeneration::INITIAL);
    assert_eq!(backend.starts.load(Ordering::Relaxed), 1);
    assert_eq!(
        record
            .managed_execution
            .as_ref()
            .unwrap()
            .request
            .external_sandbox_id,
        "external/../../sandbox"
    );
    assert!(!record.name.contains("external"));
    assert!(record
        .box_dir
        .starts_with(directory.path().join("home/boxes")));
    assert_eq!(record.pid, Some(4242));
    assert_eq!(record.anonymous_volumes, vec!["anonymous-1"]);
}

#[tokio::test]
async fn repeated_create_is_idempotent_and_request_drift_conflicts() {
    let (_directory, manager, backend) = harness();
    let operation_id = operation("operation-1");
    let create_request = request("sandbox-1");

    let first = manager
        .create_and_start(create_request.clone(), &operation_id)
        .await
        .unwrap();
    let retry = manager
        .create_and_start(create_request, &operation_id)
        .await
        .unwrap();
    let drift = manager
        .create_and_start(request("sandbox-2"), &operation_id)
        .await;

    assert_eq!(retry.execution_id, first.execution_id);
    assert_eq!(backend.starts.load(Ordering::Relaxed), 1);
    assert!(matches!(drift, Err(ExecutionManagerError::Conflict { .. })));
}

#[tokio::test]
async fn concurrent_create_calls_start_one_internal_execution() {
    let (_directory, manager, backend) = harness();
    let operation_id = operation("operation-1");
    let create_request = request("sandbox-1");

    let (left, right) = tokio::join!(
        manager.create_and_start(create_request.clone(), &operation_id),
        manager.create_and_start(create_request.clone(), &operation_id),
    );
    let retry = manager
        .create_and_start(create_request, &operation_id)
        .await
        .unwrap();

    let successes: Vec<_> = [left, right].into_iter().filter_map(Result::ok).collect();
    assert!(!successes.is_empty());
    assert!(successes
        .iter()
        .all(|lease| lease.execution_id == retry.execution_id));
    assert_eq!(backend.starts.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn pause_and_resume_are_generation_fenced_and_persist_policy() {
    let (_directory, manager, backend) = harness();
    let operation_id = operation("operation-1");
    let running = manager
        .create_and_start(request("sandbox-1"), &operation_id)
        .await
        .unwrap();

    let paused = manager
        .pause(&running.execution_id, running.generation, true)
        .await
        .unwrap();
    let stale = manager
        .resume(&running.execution_id, running.generation)
        .await;
    let resumed = manager
        .resume(&paused.execution_id, paused.generation)
        .await
        .unwrap();

    assert_eq!(paused.generation, ExecutionGeneration::new(2).unwrap());
    assert_eq!(resumed.generation, ExecutionGeneration::new(3).unwrap());
    assert!(matches!(stale, Err(ExecutionManagerError::Conflict { .. })));
    assert_eq!(*backend.last_keep_memory.lock().unwrap(), Some(true));
    assert_eq!(backend.pauses.load(Ordering::Relaxed), 1);
    assert_eq!(backend.resumes.load(Ordering::Relaxed), 1);
    let record = persisted(&manager, &running.execution_id);
    assert_eq!(
        record.managed_state().unwrap(),
        Some(ManagedExecutionState::Running)
    );
    assert!(record
        .managed_execution
        .unwrap()
        .pending_operation
        .is_none());
}

#[tokio::test]
async fn failed_pause_rolls_back_without_changing_backend_or_generation() {
    let (_directory, manager, backend) = harness();
    let running = manager
        .create_and_start(request("sandbox-1"), &operation("operation-1"))
        .await
        .unwrap();
    backend.fail_pause.store(true, Ordering::Relaxed);

    let error = manager
        .pause(&running.execution_id, running.generation, false)
        .await
        .unwrap_err();

    assert!(matches!(error, ExecutionManagerError::Unavailable(_)));
    let record = persisted(&manager, &running.execution_id);
    assert_eq!(
        record.managed_state().unwrap(),
        Some(ManagedExecutionState::Running)
    );
    assert_eq!(
        record.managed_execution.unwrap().generation,
        ExecutionGeneration::INITIAL
    );
    assert_eq!(backend.starts.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn ambiguous_pause_error_uses_backend_evidence_and_publishes_success() {
    let (_directory, manager, backend) = harness();
    let running = manager
        .create_and_start(request("sandbox-1"), &operation("operation-1"))
        .await
        .unwrap();
    backend
        .fail_pause_after_effect
        .store(true, Ordering::Relaxed);

    let paused = manager
        .pause(&running.execution_id, running.generation, true)
        .await
        .unwrap();

    assert_eq!(paused.generation, ExecutionGeneration::new(2).unwrap());
    assert_eq!(
        persisted(&manager, &running.execution_id)
            .managed_state()
            .unwrap(),
        Some(ManagedExecutionState::Paused)
    );
}

#[tokio::test]
async fn kill_is_generation_fenced_and_idempotent() {
    let (_directory, manager, backend) = harness();
    let running = manager
        .create_and_start(request("sandbox-1"), &operation("operation-1"))
        .await
        .unwrap();

    let killed = manager
        .kill(&running.execution_id, running.generation)
        .await
        .unwrap();
    let repeated = manager
        .kill(&running.execution_id, running.generation)
        .await
        .unwrap();

    assert_eq!(killed, KillOutcome::Killed);
    assert_eq!(repeated, KillOutcome::AlreadyStopped);
    assert_eq!(backend.kills.load(Ordering::Relaxed), 1);
    assert_eq!(
        manager.inspect(&running.execution_id).await.unwrap().state,
        ExecutionState::Stopped
    );
}

#[tokio::test]
async fn startup_reconciliation_restarts_a_claim_without_backend_evidence() {
    let (_directory, manager, backend) = harness();
    let operation_id = operation("operation-1");
    let execution_id = ExecutionId::new("execution-recovery-1").unwrap();
    reserve_starting(&manager, &execution_id, &operation_id).await;

    let outcome = manager.reconcile(&operation_id).await.unwrap();

    let ReconcileOutcome::Ready(lease) = outcome else {
        panic!("expected ready reconciliation");
    };
    assert_eq!(lease.execution_id, execution_id);
    assert_eq!(backend.starts.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn startup_reconciliation_publishes_an_already_started_backend_once() {
    let (_directory, manager, backend) = harness();
    let operation_id = operation("operation-1");
    let execution_id = ExecutionId::new("execution-recovery-1").unwrap();
    let starting = reserve_starting(&manager, &execution_id, &operation_id).await;
    backend.start(&starting).await.unwrap();

    let outcome = manager.reconcile(&operation_id).await.unwrap();

    let ReconcileOutcome::Ready(lease) = outcome else {
        panic!("expected ready reconciliation");
    };
    assert_eq!(lease.execution_id, execution_id);
    assert_eq!(backend.starts.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn reconciliation_completes_pause_after_backend_side_effect() {
    let (_directory, manager, backend) = harness();
    let operation_id = operation("operation-1");
    let running = manager
        .create_and_start(request("sandbox-1"), &operation_id)
        .await
        .unwrap();
    let record = persisted(&manager, &running.execution_id);
    let pausing = manager
        .transition(
            &record,
            ManagedExecutionState::Running,
            ManagedExecutionState::Pausing,
            RuntimeUpdate::PauseClaim(true),
        )
        .await
        .unwrap();
    backend.pause(&pausing, true).await.unwrap();

    let outcome = manager.reconcile(&operation_id).await.unwrap();

    let ReconcileOutcome::Ready(lease) = outcome else {
        panic!("expected ready reconciliation");
    };
    assert_eq!(lease.generation, ExecutionGeneration::new(2).unwrap());
    assert_eq!(backend.pauses.load(Ordering::Relaxed), 1);
    assert_eq!(
        manager.inspect(&running.execution_id).await.unwrap().state,
        ExecutionState::Paused
    );
}

#[tokio::test]
async fn inspection_persists_an_external_terminal_observation() {
    let (_directory, manager, backend) = harness();
    let running = manager
        .create_and_start(request("sandbox-1"), &operation("operation-1"))
        .await
        .unwrap();
    backend.stop_externally(&running.execution_id, 7);

    let status = manager.inspect(&running.execution_id).await.unwrap();
    let record = persisted(&manager, &running.execution_id);

    assert_eq!(status.state, ExecutionState::Stopped);
    assert_eq!(record.exit_code, Some(7));
    assert_eq!(record.pid, None);
    assert_eq!(
        record.managed_state().unwrap(),
        Some(ManagedExecutionState::Stopped)
    );
}
