use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use a3s_box_core::{
    BoxConfig, CreateExecutionRequest, ExecutionId, ExecutionIsolation, ExecutionManager,
    ExecutionManagerError, ExecutionManagerResult, ExecutionState, KillOutcome, NetworkMode,
    OperationId,
};
use async_trait::async_trait;
use tokio::sync::{oneshot, Semaphore};

use super::{
    LocalExecutionBackend, LocalExecutionHandle, LocalExecutionManager, LocalExecutionObservation,
};
use crate::{ManagedExecutionState, ManagedExecutionStore};

#[derive(Clone)]
struct FakeExecution {
    state: ExecutionState,
    handle: LocalExecutionHandle,
    exit_code: Option<i32>,
}

struct InspectionControl {
    claimed: AtomicBool,
    completed: AtomicBool,
    started: Semaphore,
    release: Semaphore,
}

impl InspectionControl {
    fn new() -> Self {
        Self {
            claimed: AtomicBool::new(false),
            completed: AtomicBool::new(false),
            started: Semaphore::new(0),
            release: Semaphore::new(0),
        }
    }

    async fn wait_until_started(&self) {
        self.started
            .acquire()
            .await
            .expect("inspection start semaphore must remain open")
            .forget();
    }

    fn release(&self) {
        self.release.add_permits(1);
    }
}

struct CancellationBackend {
    execution: Arc<Mutex<Option<FakeExecution>>>,
    inspection: Arc<InspectionControl>,
}

impl CancellationBackend {
    fn new() -> Self {
        Self {
            execution: Arc::new(Mutex::new(None)),
            inspection: Arc::new(InspectionControl::new()),
        }
    }

    fn execution_id(record: &crate::BoxRecord) -> ExecutionId {
        ExecutionId::new(record.id.clone()).unwrap()
    }

    fn handle(record: &crate::BoxRecord) -> LocalExecutionHandle {
        LocalExecutionHandle {
            started_at: chrono::Utc::now(),
            pid: Some(std::process::id()),
            pid_start_time: crate::process::pid_start_time(std::process::id()),
            exec_socket_path: record.box_dir.join("sockets/exec.sock"),
            console_log: record.box_dir.join("logs/console.log"),
            anonymous_volumes: Vec::new(),
            oci_runtime: None,
        }
    }
}

#[async_trait]
impl LocalExecutionBackend for CancellationBackend {
    async fn start(
        &self,
        record: &crate::BoxRecord,
    ) -> ExecutionManagerResult<LocalExecutionHandle> {
        let handle = Self::handle(record);
        *self.execution.lock().unwrap() = Some(FakeExecution {
            state: ExecutionState::Running,
            handle: handle.clone(),
            exit_code: None,
        });
        Ok(handle)
    }

    async fn inspect(
        &self,
        record: &crate::BoxRecord,
    ) -> ExecutionManagerResult<LocalExecutionObservation> {
        if self
            .inspection
            .claimed
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            let execution = Arc::clone(&self.execution);
            let inspection = Arc::clone(&self.inspection);
            let (completed_tx, completed_rx) = oneshot::channel();
            tokio::spawn(async move {
                inspection.started.add_permits(1);
                inspection
                    .release
                    .acquire()
                    .await
                    .expect("inspection release semaphore must remain open")
                    .forget();
                if let Some(execution) = execution.lock().unwrap().as_mut() {
                    execution.state = ExecutionState::Stopped;
                    execution.exit_code = Some(0);
                }
                inspection.completed.store(true, Ordering::SeqCst);
                let _ = completed_tx.send(());
            });
            completed_rx.await.map_err(|error| {
                ExecutionManagerError::Internal(format!("detached inspection task failed: {error}"))
            })?;
        } else if !self.inspection.completed.load(Ordering::SeqCst) {
            return Err(ExecutionManagerError::NotFound(Self::execution_id(record)));
        }

        let execution = self.execution.lock().unwrap();
        let execution = execution
            .as_ref()
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
        record: &crate::BoxRecord,
        _keep_memory: bool,
    ) -> ExecutionManagerResult<LocalExecutionHandle> {
        Err(ExecutionManagerError::Unavailable(format!(
            "fake pause is unavailable for {}",
            record.id
        )))
    }

    async fn resume(
        &self,
        record: &crate::BoxRecord,
    ) -> ExecutionManagerResult<LocalExecutionHandle> {
        Err(ExecutionManagerError::Unavailable(format!(
            "fake resume is unavailable for {}",
            record.id
        )))
    }

    async fn kill(&self, record: &crate::BoxRecord) -> ExecutionManagerResult<KillOutcome> {
        let mut execution = self.execution.lock().unwrap();
        let execution = execution
            .as_mut()
            .ok_or_else(|| ExecutionManagerError::NotFound(Self::execution_id(record)))?;
        if execution.state == ExecutionState::Stopped {
            Ok(KillOutcome::AlreadyStopped)
        } else {
            execution.state = ExecutionState::Stopped;
            Ok(KillOutcome::Killed)
        }
    }
}

fn request() -> CreateExecutionRequest {
    CreateExecutionRequest {
        external_sandbox_id: "cancelled-inspection".to_string(),
        config: BoxConfig {
            image: "alpine:3.20".to_string(),
            isolation: ExecutionIsolation::Sandbox,
            network: NetworkMode::None,
            ..Default::default()
        },
        labels: BTreeMap::new(),
        policy: Default::default(),
        rootfs_snapshot_id: None,
    }
}

#[tokio::test]
async fn cancelled_inspection_keeps_its_lifecycle_lock_until_projection_finishes() {
    let directory = tempfile::tempdir().unwrap();
    let home_dir = directory.path().join("home");
    let state_path = directory.path().join("boxes.json");
    let backend = Arc::new(CancellationBackend::new());
    let manager = LocalExecutionManager::new(&state_path, &home_dir, backend.clone());
    let lease = manager
        .create_and_start(
            request(),
            &OperationId::new("cancelled-inspection-operation").unwrap(),
        )
        .await
        .unwrap();
    let execution_id = lease.execution_id;

    let first = {
        let manager = manager.clone();
        let execution_id = execution_id.clone();
        tokio::spawn(async move { manager.inspect(&execution_id).await })
    };
    backend.inspection.wait_until_started().await;
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());

    let restarted = LocalExecutionManager::new(&state_path, &home_dir, backend.clone());
    let mut replay = {
        let restarted = restarted.clone();
        let execution_id = execution_id.clone();
        tokio::spawn(async move { restarted.inspect(&execution_id).await })
    };
    let early = tokio::time::timeout(Duration::from_millis(50), &mut replay).await;
    backend.inspection.release();
    let (waited_for_original_inspection, status) = match early {
        Ok(result) => (false, result.unwrap().unwrap()),
        Err(_) => (
            true,
            tokio::time::timeout(Duration::from_secs(2), replay)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
        ),
    };

    assert!(
        waited_for_original_inspection,
        "replay bypassed the cancelled inspection's lifecycle lock"
    );
    assert_eq!(status.state, ExecutionState::Stopped);
    let record = ManagedExecutionStore::new(state_path)
        .get(&execution_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        record.managed_state().unwrap(),
        Some(ManagedExecutionState::Stopped)
    );
    assert_eq!(record.exit_code, Some(0));
}
