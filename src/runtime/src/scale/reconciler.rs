//! Desired-state reconciliation against the durable local execution facade.

use std::{collections::BTreeMap, sync::Arc};

use a3s_box_core::{
    CreateExecutionRequest, ExecutionGeneration, ExecutionId, ExecutionManager, ExecutionState,
    OperationId, ReconcileOutcome,
};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::Mutex;

use crate::{LocalExecutionManager, ManagedExecutionState};

use super::catalog::{
    ScaleServiceCatalog, SCALE_MANAGED_LABEL, SCALE_SERVICE_LABEL, SCALE_SLOT_LABEL,
    SCALE_TEMPLATE_DIGEST_LABEL,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstancePhase {
    Active,
    Ready,
    Terminal,
    Removing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScaleExecution {
    execution_id: ExecutionId,
    generation: ExecutionGeneration,
    service: String,
    slot: u32,
    template_digest: String,
    phase: InstancePhase,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScaleReconcileReport {
    pub service: String,
    pub desired_replicas: u32,
    pub ready_replicas: u32,
    pub created: u32,
    pub removed: u32,
}

#[derive(Debug, Error)]
pub enum ScaleReconcileError {
    #[error("unknown scale service {0:?}")]
    UnknownService(String),
    #[error("invalid scale template for {service:?}: {message}")]
    Template { service: String, message: String },
    #[error("scale lifecycle error: {0}")]
    Lifecycle(String),
}

#[async_trait]
trait ScaleExecutionLifecycle: Send + Sync {
    async fn inventory(&self) -> Result<Vec<ScaleExecution>, ScaleReconcileError>;

    async fn ensure_running(
        &self,
        request: CreateExecutionRequest,
        operation_id: OperationId,
    ) -> Result<(), ScaleReconcileError>;

    async fn ensure_removed(&self, execution: &ScaleExecution) -> Result<(), ScaleReconcileError>;
}

/// Serial reconciler that converges one service to deterministic replica slots.
pub struct LocalScaleReconciler {
    catalog: ScaleServiceCatalog,
    lifecycle: Arc<dyn ScaleExecutionLifecycle>,
    reconcile_lock: Mutex<()>,
}

impl LocalScaleReconciler {
    pub fn new(manager: LocalExecutionManager, catalog: ScaleServiceCatalog) -> Self {
        Self {
            catalog,
            lifecycle: Arc::new(LocalScaleExecutionLifecycle { manager }),
            reconcile_lock: Mutex::new(()),
        }
    }

    #[cfg(test)]
    fn with_lifecycle(
        catalog: ScaleServiceCatalog,
        lifecycle: Arc<dyn ScaleExecutionLifecycle>,
    ) -> Self {
        Self {
            catalog,
            lifecycle,
            reconcile_lock: Mutex::new(()),
        }
    }

    pub fn knows_service(&self, service: &str) -> bool {
        self.catalog.contains(service)
    }

    pub fn services(&self) -> Vec<String> {
        self.catalog.services()
    }

    pub async fn ready_replicas(&self, service: &str) -> Result<u32, ScaleReconcileError> {
        let _guard = self.reconcile_lock.lock().await;
        if !self.catalog.contains(service) {
            return Err(ScaleReconcileError::UnknownService(service.to_string()));
        }
        Ok(self
            .service_inventory(service)
            .await?
            .into_iter()
            .filter(|execution| execution.phase == InstancePhase::Ready)
            .count() as u32)
    }

    pub async fn reconcile(
        &self,
        service: &str,
        desired_replicas: u32,
    ) -> Result<ScaleReconcileReport, ScaleReconcileError> {
        let _guard = self.reconcile_lock.lock().await;
        if !self.catalog.contains(service) {
            return Err(ScaleReconcileError::UnknownService(service.to_string()));
        }

        let desired_templates = (0..desired_replicas)
            .map(|slot| {
                self.catalog
                    .create_request(service, slot)
                    .map(|request| (slot, request))
                    .map_err(|error| ScaleReconcileError::Template {
                        service: service.to_string(),
                        message: error.to_string(),
                    })
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let mut inventory = self.service_inventory(service).await?;
        inventory.sort_by(|left, right| {
            left.slot
                .cmp(&right.slot)
                .then_with(|| left.execution_id.as_str().cmp(right.execution_id.as_str()))
        });

        let mut retained = BTreeMap::<u32, ScaleExecution>::new();
        let mut removed = 0_u32;
        for execution in inventory {
            let expected_digest = desired_templates
                .get(&execution.slot)
                .and_then(template_digest);
            let should_retain = expected_digest
                .is_some_and(|digest| digest == execution.template_digest)
                && matches!(
                    execution.phase,
                    InstancePhase::Active | InstancePhase::Ready
                )
                && !retained.contains_key(&execution.slot);
            if should_retain {
                retained.insert(execution.slot, execution);
            } else {
                self.lifecycle.ensure_removed(&execution).await?;
                removed = removed.saturating_add(1);
            }
        }

        let mut created = 0_u32;
        for (slot, request) in desired_templates {
            if retained.contains_key(&slot) {
                continue;
            }
            let operation_id = create_operation_id(
                service,
                slot,
                template_digest(&request).ok_or_else(|| ScaleReconcileError::Template {
                    service: service.to_string(),
                    message: "generated template has no digest label".to_string(),
                })?,
            )?;
            self.lifecycle.ensure_running(request, operation_id).await?;
            created = created.saturating_add(1);
        }

        let ready_replicas = self
            .service_inventory(service)
            .await?
            .into_iter()
            .filter(|execution| {
                execution.slot < desired_replicas && execution.phase == InstancePhase::Ready
            })
            .count() as u32;

        Ok(ScaleReconcileReport {
            service: service.to_string(),
            desired_replicas,
            ready_replicas,
            created,
            removed,
        })
    }

    async fn service_inventory(
        &self,
        service: &str,
    ) -> Result<Vec<ScaleExecution>, ScaleReconcileError> {
        Ok(self
            .lifecycle
            .inventory()
            .await?
            .into_iter()
            .filter(|execution| execution.service == service)
            .collect())
    }
}

fn template_digest(request: &CreateExecutionRequest) -> Option<&str> {
    request
        .labels
        .get(SCALE_TEMPLATE_DIGEST_LABEL)
        .map(String::as_str)
}

fn create_operation_id(
    service: &str,
    slot: u32,
    template_digest: &str,
) -> Result<OperationId, ScaleReconcileError> {
    let identity = format!("{service}\0{slot}\0{template_digest}");
    OperationId::new(format!(
        "scale-create-v1-{:x}",
        Sha256::digest(identity.as_bytes())
    ))
    .map_err(|error| ScaleReconcileError::Lifecycle(error.to_string()))
}

struct LocalScaleExecutionLifecycle {
    manager: LocalExecutionManager,
}

#[async_trait]
impl ScaleExecutionLifecycle for LocalScaleExecutionLifecycle {
    async fn inventory(&self) -> Result<Vec<ScaleExecution>, ScaleReconcileError> {
        let records = self
            .manager
            .managed_records()
            .await
            .map_err(lifecycle_error)?;
        let mut inventory = Vec::new();
        for record in records {
            if record.labels.get(SCALE_MANAGED_LABEL).map(String::as_str) != Some("true") {
                continue;
            }
            let service = required_label(&record.labels, SCALE_SERVICE_LABEL, &record.id)?;
            let slot = required_label(&record.labels, SCALE_SLOT_LABEL, &record.id)?
                .parse::<u32>()
                .map_err(|error| {
                    ScaleReconcileError::Lifecycle(format!(
                        "scale execution {} has invalid slot label: {error}",
                        record.id
                    ))
                })?;
            let template_digest =
                required_label(&record.labels, SCALE_TEMPLATE_DIGEST_LABEL, &record.id)?;
            let metadata = record.managed_execution.as_ref().ok_or_else(|| {
                ScaleReconcileError::Lifecycle(format!(
                    "scale execution {} has no managed lifecycle metadata",
                    record.id
                ))
            })?;
            let execution_id = ExecutionId::new(record.id.clone()).map_err(lifecycle_error)?;
            let internal = ManagedExecutionState::from_status(&record.status)
                .map_err(|error| ScaleReconcileError::Lifecycle(error.to_string()))?;
            let phase = if internal == ManagedExecutionState::Removing {
                InstancePhase::Removing
            } else if internal.is_terminal() {
                InstancePhase::Terminal
            } else {
                let status = self
                    .manager
                    .inspect(&execution_id)
                    .await
                    .map_err(lifecycle_error)?;
                match status.state {
                    ExecutionState::Running => InstancePhase::Ready,
                    ExecutionState::Stopped | ExecutionState::Failed => InstancePhase::Terminal,
                    ExecutionState::Created | ExecutionState::Creating | ExecutionState::Paused => {
                        InstancePhase::Active
                    }
                }
            };
            inventory.push(ScaleExecution {
                execution_id,
                generation: metadata.generation,
                service,
                slot,
                template_digest,
                phase,
            });
        }
        Ok(inventory)
    }

    async fn ensure_running(
        &self,
        request: CreateExecutionRequest,
        operation_id: OperationId,
    ) -> Result<(), ScaleReconcileError> {
        let outcome = self
            .manager
            .reconcile(&operation_id)
            .await
            .map_err(lifecycle_error)?;
        let reservation = match outcome {
            ReconcileOutcome::Ready(_) => return Ok(()),
            ReconcileOutcome::Created(reservation) => reservation,
            ReconcileOutcome::Absent => match self.manager.create(request, &operation_id).await {
                Ok(reservation) => reservation,
                Err(create_error) => match self.manager.reconcile(&operation_id).await {
                    Ok(ReconcileOutcome::Ready(_)) => return Ok(()),
                    Ok(ReconcileOutcome::Created(reservation)) => reservation,
                    _ => return Err(lifecycle_error(create_error)),
                },
            },
            ReconcileOutcome::Creating => {
                return Err(ScaleReconcileError::Lifecycle(format!(
                    "scale create operation {operation_id} is still converging"
                )))
            }
            ReconcileOutcome::Failed => {
                return Err(ScaleReconcileError::Lifecycle(format!(
                    "scale create operation {operation_id} is terminal"
                )))
            }
        };

        match self
            .manager
            .start(&reservation.execution_id, reservation.generation)
            .await
        {
            Ok(_) => Ok(()),
            Err(start_error) => match self.manager.reconcile(&operation_id).await {
                Ok(ReconcileOutcome::Ready(_)) => Ok(()),
                _ => Err(lifecycle_error(start_error)),
            },
        }
    }

    async fn ensure_removed(&self, execution: &ScaleExecution) -> Result<(), ScaleReconcileError> {
        if !matches!(
            execution.phase,
            InstancePhase::Terminal | InstancePhase::Removing
        ) {
            if let Err(kill_error) = self
                .manager
                .kill(&execution.execution_id, execution.generation)
                .await
            {
                let status = self.manager.inspect(&execution.execution_id).await;
                if !matches!(
                    status,
                    Ok(status)
                        if matches!(status.state, ExecutionState::Stopped | ExecutionState::Failed)
                ) {
                    return Err(lifecycle_error(kill_error));
                }
            }
        }
        self.manager
            .remove(&execution.execution_id, execution.generation)
            .await
            .map(|_| ())
            .map_err(lifecycle_error)
    }
}

fn required_label(
    labels: &std::collections::HashMap<String, String>,
    name: &str,
    execution_id: &str,
) -> Result<String, ScaleReconcileError> {
    labels.get(name).cloned().ok_or_else(|| {
        ScaleReconcileError::Lifecycle(format!(
            "scale execution {execution_id} is missing label {name}"
        ))
    })
}

fn lifecycle_error(error: impl std::fmt::Display) -> ScaleReconcileError {
    ScaleReconcileError::Lifecycle(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Mutex as StdMutex,
        },
    };

    use a3s_box_core::{
        ExecutionIsolation, ExecutionManagerError, ExecutionManagerResult, KillOutcome,
    };
    use chrono::Utc;

    use crate::{
        BoxRecord, LocalExecutionBackend, LocalExecutionHandle, LocalExecutionObservation,
    };

    use super::*;

    const CATALOG: &str = r#"service "api" { image = "api:v1" }"#;

    struct FakeLifecycle {
        executions: Mutex<HashMap<String, ScaleExecution>>,
        lose_next_create_response: AtomicBool,
    }

    impl FakeLifecycle {
        fn new() -> Self {
            Self {
                executions: Mutex::new(HashMap::new()),
                lose_next_create_response: AtomicBool::new(false),
            }
        }
    }

    #[async_trait]
    impl ScaleExecutionLifecycle for FakeLifecycle {
        async fn inventory(&self) -> Result<Vec<ScaleExecution>, ScaleReconcileError> {
            Ok(self.executions.lock().await.values().cloned().collect())
        }

        async fn ensure_running(
            &self,
            request: CreateExecutionRequest,
            _operation_id: OperationId,
        ) -> Result<(), ScaleReconcileError> {
            let service = request.labels[SCALE_SERVICE_LABEL].clone();
            let slot = request.labels[SCALE_SLOT_LABEL].parse().unwrap();
            let key = format!("{service}-{slot}");
            self.executions
                .lock()
                .await
                .entry(key.clone())
                .or_insert(ScaleExecution {
                    execution_id: ExecutionId::new(key).unwrap(),
                    generation: ExecutionGeneration::INITIAL,
                    service,
                    slot,
                    template_digest: request.labels[SCALE_TEMPLATE_DIGEST_LABEL].clone(),
                    phase: InstancePhase::Ready,
                });
            if self.lose_next_create_response.swap(false, Ordering::SeqCst) {
                return Err(ScaleReconcileError::Lifecycle("response lost".to_string()));
            }
            Ok(())
        }

        async fn ensure_removed(
            &self,
            execution: &ScaleExecution,
        ) -> Result<(), ScaleReconcileError> {
            self.executions
                .lock()
                .await
                .retain(|_, current| current.execution_id != execution.execution_id);
            Ok(())
        }
    }

    fn reconciler(lifecycle: Arc<FakeLifecycle>) -> LocalScaleReconciler {
        let catalog = ScaleServiceCatalog::from_acl_str(
            CATALOG,
            "gateway-scale",
            ExecutionIsolation::Sandbox,
        )
        .unwrap();
        LocalScaleReconciler::with_lifecycle(catalog, lifecycle)
    }

    #[tokio::test]
    async fn converges_up_and_down_using_stable_slots() {
        let lifecycle = Arc::new(FakeLifecycle::new());
        let reconciler = reconciler(lifecycle.clone());
        let up = reconciler.reconcile("api", 3).await.unwrap();
        assert_eq!(up.ready_replicas, 3);
        assert_eq!(up.created, 3);

        let replay = reconciler.reconcile("api", 3).await.unwrap();
        assert_eq!(replay.created, 0);
        assert_eq!(replay.removed, 0);

        let down = reconciler.reconcile("api", 1).await.unwrap();
        assert_eq!(down.ready_replicas, 1);
        assert_eq!(down.removed, 2);
        let inventory = lifecycle.inventory().await.unwrap();
        assert_eq!(inventory[0].slot, 0);
    }

    #[tokio::test]
    async fn ambiguous_create_is_adopted_without_a_duplicate_on_retry() {
        let lifecycle = Arc::new(FakeLifecycle::new());
        lifecycle
            .lose_next_create_response
            .store(true, Ordering::SeqCst);
        let reconciler = reconciler(lifecycle.clone());
        assert!(reconciler.reconcile("api", 1).await.is_err());

        let recovered = reconciler.reconcile("api", 1).await.unwrap();
        assert_eq!(recovered.ready_replicas, 1);
        assert_eq!(recovered.created, 0);
        assert_eq!(lifecycle.inventory().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unknown_service_fails_before_any_lifecycle_side_effect() {
        let lifecycle = Arc::new(FakeLifecycle::new());
        let reconciler = reconciler(lifecycle.clone());
        assert!(matches!(
            reconciler.reconcile("missing", 1).await,
            Err(ScaleReconcileError::UnknownService(_))
        ));
        assert!(lifecycle.inventory().await.unwrap().is_empty());
    }

    struct RecordingBackend {
        running: StdMutex<HashSet<String>>,
        starts: AtomicUsize,
        lose_next_start_response: AtomicBool,
    }

    impl RecordingBackend {
        fn new() -> Self {
            Self {
                running: StdMutex::new(HashSet::new()),
                starts: AtomicUsize::new(0),
                lose_next_start_response: AtomicBool::new(false),
            }
        }

        fn handle(record: &BoxRecord) -> LocalExecutionHandle {
            LocalExecutionHandle {
                started_at: Utc::now(),
                pid: None,
                pid_start_time: None,
                exec_socket_path: record.box_dir.join("sockets/exec.sock"),
                console_log: record.box_dir.join("logs/console.log"),
                anonymous_volumes: Vec::new(),
                oci_runtime: None,
            }
        }
    }

    #[async_trait]
    impl LocalExecutionBackend for RecordingBackend {
        async fn start(&self, record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            self.running.lock().unwrap().insert(record.id.clone());
            if self.lose_next_start_response.swap(false, Ordering::SeqCst) {
                return Err(ExecutionManagerError::Unavailable(
                    "start response lost".to_string(),
                ));
            }
            Ok(Self::handle(record))
        }

        async fn inspect(
            &self,
            record: &BoxRecord,
        ) -> ExecutionManagerResult<LocalExecutionObservation> {
            if self.running.lock().unwrap().contains(&record.id) {
                Ok(LocalExecutionObservation {
                    state: ExecutionState::Running,
                    handle: Some(Self::handle(record)),
                    exit_code: None,
                })
            } else {
                Ok(LocalExecutionObservation {
                    state: ExecutionState::Stopped,
                    handle: None,
                    exit_code: Some(0),
                })
            }
        }

        async fn pause(
            &self,
            _record: &BoxRecord,
            _keep_memory: bool,
        ) -> ExecutionManagerResult<LocalExecutionHandle> {
            Err(ExecutionManagerError::Unavailable(
                "pause unsupported".into(),
            ))
        }

        async fn resume(
            &self,
            _record: &BoxRecord,
        ) -> ExecutionManagerResult<LocalExecutionHandle> {
            Err(ExecutionManagerError::Unavailable(
                "resume unsupported".into(),
            ))
        }

        async fn kill(&self, record: &BoxRecord) -> ExecutionManagerResult<KillOutcome> {
            let removed = self.running.lock().unwrap().remove(&record.id);
            Ok(if removed {
                KillOutcome::Killed
            } else {
                KillOutcome::AlreadyStopped
            })
        }
    }

    #[tokio::test]
    async fn local_execution_facade_converges_and_recovers_lost_start_response() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        let state = directory.path().join("boxes.json");
        let backend = Arc::new(RecordingBackend::new());
        backend
            .lose_next_start_response
            .store(true, Ordering::SeqCst);
        let manager = LocalExecutionManager::new(&state, &home, backend.clone());
        let catalog = ScaleServiceCatalog::from_acl_str(
            CATALOG,
            "gateway-scale",
            ExecutionIsolation::Sandbox,
        )
        .unwrap();
        let reconciler = LocalScaleReconciler::new(manager, catalog.clone());

        let up = reconciler.reconcile("api", 2).await.unwrap();
        assert_eq!(up.ready_replicas, 2);
        assert_eq!(backend.starts.load(Ordering::SeqCst), 2);

        let reopened = LocalExecutionManager::new(&state, &home, backend.clone());
        let restarted = LocalScaleReconciler::new(reopened.clone(), catalog);
        let replay = restarted.reconcile("api", 2).await.unwrap();
        assert_eq!(replay.created, 0);
        assert_eq!(replay.ready_replicas, 2);
        assert_eq!(backend.starts.load(Ordering::SeqCst), 2);

        let down = restarted.reconcile("api", 0).await.unwrap();
        assert_eq!(down.removed, 2);
        assert_eq!(down.ready_replicas, 0);
        assert!(reopened.managed_records().await.unwrap().is_empty());
    }
}
