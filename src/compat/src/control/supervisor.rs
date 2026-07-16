use std::num::NonZeroU32;
use std::sync::Arc;

use a3s_box_core::{
    ExecutionLease, ExecutionManager, ExecutionManagerError, ExecutionReservation, ExecutionState,
    ReconcileOutcome,
};
use thiserror::Error;

use super::lifetime::ready_lifetime;
use super::{
    Clock, CompareAndSwapResult, LifecycleError, LifecycleFailure, LifecycleState, RepositoryError,
    SandboxGeneration, SandboxId, SandboxRecord, SandboxRepository,
};

#[derive(Clone)]
pub struct LifecycleSupervisor {
    repository: Arc<dyn SandboxRepository>,
    executions: Arc<dyn ExecutionManager>,
    clock: Arc<dyn Clock>,
}

pub struct LifecycleSupervisorDependencies {
    pub repository: Arc<dyn SandboxRepository>,
    pub executions: Arc<dyn ExecutionManager>,
    pub clock: Arc<dyn Clock>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LifecycleMaintenanceReport {
    pub examined: usize,
    pub completed: usize,
    pub deferred: usize,
    pub failures: Vec<LifecycleMaintenanceFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleMaintenanceFailure {
    pub sandbox_id: SandboxId,
    pub message: String,
}

#[derive(Debug, Error)]
pub enum LifecycleSupervisorError {
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

pub type LifecycleSupervisorResult<T> = std::result::Result<T, LifecycleSupervisorError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaintenanceDisposition {
    Completed,
    Deferred,
}

#[derive(Debug, Error)]
enum MaintenanceItemError {
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    Execution(#[from] ExecutionManagerError),
    #[error(transparent)]
    Lifecycle(#[from] LifecycleError),
    #[error("inconsistent runtime reconciliation evidence: {0}")]
    Inconsistent(String),
}

type MaintenanceItemResult<T> = std::result::Result<T, MaintenanceItemError>;

impl LifecycleSupervisor {
    pub fn new(dependencies: LifecycleSupervisorDependencies) -> Self {
        Self {
            repository: dependencies.repository,
            executions: dependencies.executions,
            clock: dependencies.clock,
        }
    }

    /// Claim and finish one bounded batch of expired lifecycle records.
    pub async fn reap_expired(
        &self,
        limit: NonZeroU32,
    ) -> LifecycleSupervisorResult<LifecycleMaintenanceReport> {
        let claimed = self
            .repository
            .claim_expired(self.clock.now(), limit)
            .await?;
        let mut report = LifecycleMaintenanceReport::default();
        for record in claimed {
            report.observe(record.sandbox_id().clone(), self.finish_claim(record).await);
        }
        Ok(report)
    }

    /// Reconcile every non-terminal record once after service startup.
    pub async fn reconcile_startup(
        &self,
        page_size: NonZeroU32,
    ) -> LifecycleSupervisorResult<LifecycleMaintenanceReport> {
        let mut report = LifecycleMaintenanceReport::default();
        let mut cursor = None;
        loop {
            let page = self
                .repository
                .list_reconcilable(cursor.as_ref(), page_size)
                .await?;
            let next = page.next;
            for record in page.records {
                report.observe(
                    record.sandbox_id().clone(),
                    self.reconcile_record(record).await,
                );
            }
            let Some(next) = next else {
                break;
            };
            cursor = Some(next);
        }
        Ok(report)
    }

    async fn finish_claim(
        &self,
        record: SandboxRecord,
    ) -> MaintenanceItemResult<MaintenanceDisposition> {
        match record.state() {
            LifecycleState::Pausing => self.finish_pause(record).await,
            LifecycleState::Killing => self.finish_kill(record).await,
            state => Err(MaintenanceItemError::Inconsistent(format!(
                "expiry claim returned {state:?}"
            ))),
        }
    }

    async fn reconcile_record(
        &self,
        record: SandboxRecord,
    ) -> MaintenanceItemResult<MaintenanceDisposition> {
        let outcome = self.executions.reconcile(record.operation_id()).await?;
        match outcome {
            ReconcileOutcome::Absent | ReconcileOutcome::Failed => {
                self.finish_missing(record).await
            }
            ReconcileOutcome::Created(reservation) => {
                self.reconcile_created(record, reservation).await
            }
            ReconcileOutcome::Creating => match record.state() {
                LifecycleState::Creating | LifecycleState::Killing => {
                    Ok(MaintenanceDisposition::Deferred)
                }
                state => Err(MaintenanceItemError::Inconsistent(format!(
                    "persisted state {state:?} is ahead of a creating runtime"
                ))),
            },
            ReconcileOutcome::Ready(lease) => self.reconcile_ready(record, lease).await,
        }
    }

    async fn reconcile_created(
        &self,
        record: SandboxRecord,
        reservation: ExecutionReservation,
    ) -> MaintenanceItemResult<MaintenanceDisposition> {
        ensure_created_reservation(&record, &reservation)?;
        match record.state() {
            LifecycleState::Creating => {
                let lease = self
                    .executions
                    .start(&reservation.execution_id, reservation.generation)
                    .await?;
                if lease.execution_id != reservation.execution_id
                    || lease.generation != reservation.generation
                    || lease.plan != reservation.plan
                    || !same_resources(&lease.resources, &reservation.resources)
                {
                    return Err(MaintenanceItemError::Inconsistent(
                        "created reservation and started execution disagree".to_string(),
                    ));
                }
                self.publish_running(record, lease).await
            }
            LifecycleState::Killing => {
                self.finish_kill_with_target(
                    record,
                    Some((reservation.execution_id, reservation.generation)),
                )
                .await
            }
            state => Err(MaintenanceItemError::Inconsistent(format!(
                "persisted state {state:?} disagrees with a created runtime reservation"
            ))),
        }
    }

    async fn reconcile_ready(
        &self,
        record: SandboxRecord,
        lease: ExecutionLease,
    ) -> MaintenanceItemResult<MaintenanceDisposition> {
        let status = match self.executions.inspect(&lease.execution_id).await {
            Ok(status) => status,
            Err(ExecutionManagerError::NotFound(_)) => return self.finish_missing(record).await,
            Err(error) => return Err(error.into()),
        };
        if status.execution_id != lease.execution_id
            || status.generation != lease.generation
            || status.plan != lease.plan
        {
            return Err(MaintenanceItemError::Inconsistent(
                "reconcile lease and inspection status disagree".to_string(),
            ));
        }

        match status.state {
            ExecutionState::Created => Err(MaintenanceItemError::Inconsistent(
                "ready reconciliation inspected a created execution".to_string(),
            )),
            ExecutionState::Creating => match record.state() {
                LifecycleState::Creating | LifecycleState::Killing => {
                    Ok(MaintenanceDisposition::Deferred)
                }
                state => Err(MaintenanceItemError::Inconsistent(format!(
                    "persisted state {state:?} is ahead of inspected creating runtime"
                ))),
            },
            ExecutionState::Stopped | ExecutionState::Failed => self.finish_missing(record).await,
            ExecutionState::Running => match record.state() {
                LifecycleState::Creating | LifecycleState::Resuming => {
                    self.publish_running(record, lease).await
                }
                LifecycleState::Running => {
                    ensure_current_execution(&record, &lease)?;
                    Ok(MaintenanceDisposition::Completed)
                }
                LifecycleState::Pausing => {
                    ensure_current_execution(&record, &lease)?;
                    self.finish_pause(record).await
                }
                LifecycleState::Killing => {
                    ensure_kill_target(&record, &lease)?;
                    self.finish_kill_with_target(
                        record,
                        Some((lease.execution_id, lease.generation)),
                    )
                    .await
                }
                state => Err(MaintenanceItemError::Inconsistent(format!(
                    "persisted state {state:?} disagrees with running runtime"
                ))),
            },
            ExecutionState::Paused => match record.state() {
                LifecycleState::Paused => {
                    ensure_current_execution(&record, &lease)?;
                    Ok(MaintenanceDisposition::Completed)
                }
                LifecycleState::Pausing => self.publish_paused(record, lease).await,
                LifecycleState::Resuming => {
                    ensure_current_execution(&record, &lease)?;
                    self.finish_resume(record).await
                }
                LifecycleState::Killing => {
                    ensure_kill_target(&record, &lease)?;
                    self.finish_kill_with_target(
                        record,
                        Some((lease.execution_id, lease.generation)),
                    )
                    .await
                }
                state => Err(MaintenanceItemError::Inconsistent(format!(
                    "persisted state {state:?} disagrees with paused runtime"
                ))),
            },
        }
    }

    async fn finish_pause(
        &self,
        record: SandboxRecord,
    ) -> MaintenanceItemResult<MaintenanceDisposition> {
        let execution_id = required_execution_id(&record)?.clone();
        let execution_generation = required_execution_generation(&record)?;
        let lease = match self
            .executions
            .pause(
                &execution_id,
                execution_generation,
                record.lifecycle().keep_memory_on_pause,
            )
            .await
        {
            Ok(lease) => lease,
            Err(ExecutionManagerError::NotFound(_)) => return self.publish_failed(record).await,
            Err(error) => return Err(error.into()),
        };
        self.publish_paused(record, lease).await
    }

    async fn finish_resume(
        &self,
        record: SandboxRecord,
    ) -> MaintenanceItemResult<MaintenanceDisposition> {
        let execution_id = required_execution_id(&record)?.clone();
        let execution_generation = required_execution_generation(&record)?;
        let lease = match self
            .executions
            .resume(&execution_id, execution_generation)
            .await
        {
            Ok(lease) => lease,
            Err(ExecutionManagerError::NotFound(_)) => return self.publish_failed(record).await,
            Err(error) => return Err(error.into()),
        };
        self.publish_running(record, lease).await
    }

    async fn finish_kill(
        &self,
        record: SandboxRecord,
    ) -> MaintenanceItemResult<MaintenanceDisposition> {
        let target = if let (Some(execution_id), Some(execution_generation)) = (
            record.execution_id().cloned(),
            record.execution_generation(),
        ) {
            Some((execution_id, execution_generation))
        } else {
            None
        };
        self.finish_kill_with_target(record, target).await
    }

    async fn finish_kill_with_target(
        &self,
        mut record: SandboxRecord,
        target: Option<(a3s_box_core::ExecutionId, a3s_box_core::ExecutionGeneration)>,
    ) -> MaintenanceItemResult<MaintenanceDisposition> {
        if let Some((execution_id, execution_generation)) = target {
            match self
                .executions
                .kill(&execution_id, execution_generation)
                .await
            {
                Ok(_) | Err(ExecutionManagerError::NotFound(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
        let expected = record.generation();
        record.mark_killed()?;
        self.replace(expected, record).await
    }

    async fn finish_missing(
        &self,
        mut record: SandboxRecord,
    ) -> MaintenanceItemResult<MaintenanceDisposition> {
        if record.state() == LifecycleState::Killing {
            let expected = record.generation();
            record.mark_killed()?;
            self.replace(expected, record).await
        } else {
            self.publish_failed(record).await
        }
    }

    async fn publish_running(
        &self,
        mut record: SandboxRecord,
        lease: ExecutionLease,
    ) -> MaintenanceItemResult<MaintenanceDisposition> {
        let expected = record.generation();
        if record.state() == LifecycleState::Creating {
            let (ready_at, expires_at) = ready_lifetime(
                self.clock.now(),
                lease.started_at,
                record.resources().timeout,
            )
            .map_err(|error| MaintenanceItemError::Inconsistent(error.to_string()))?;
            record.mark_ready(lease, ready_at, expires_at)?;
        } else {
            record.mark_running(lease)?;
        }
        self.replace(expected, record).await
    }

    async fn publish_paused(
        &self,
        mut record: SandboxRecord,
        lease: ExecutionLease,
    ) -> MaintenanceItemResult<MaintenanceDisposition> {
        let expected = record.generation();
        record.mark_paused(lease)?;
        self.replace(expected, record).await
    }

    async fn publish_failed(
        &self,
        mut record: SandboxRecord,
    ) -> MaintenanceItemResult<MaintenanceDisposition> {
        let expected = record.generation();
        record.mark_failed(LifecycleFailure::ReconciliationFailed)?;
        self.replace(expected, record).await
    }

    async fn replace(
        &self,
        expected: SandboxGeneration,
        replacement: SandboxRecord,
    ) -> MaintenanceItemResult<MaintenanceDisposition> {
        let sandbox_id = replacement.sandbox_id().clone();
        Ok(
            match self
                .repository
                .compare_and_swap(&sandbox_id, expected, replacement)
                .await?
            {
                CompareAndSwapResult::Updated => MaintenanceDisposition::Completed,
                CompareAndSwapResult::NotFound | CompareAndSwapResult::Conflict { .. } => {
                    MaintenanceDisposition::Deferred
                }
            },
        )
    }
}

fn ensure_created_reservation(
    record: &SandboxRecord,
    reservation: &ExecutionReservation,
) -> MaintenanceItemResult<()> {
    if record.plan() != &reservation.plan {
        return Err(MaintenanceItemError::Inconsistent(
            "created reservation plan differs from persisted sandbox".to_string(),
        ));
    }
    if !same_resources(record.resources(), &reservation.resources) {
        return Err(MaintenanceItemError::Inconsistent(
            "created reservation resources differ from persisted sandbox".to_string(),
        ));
    }
    match (record.execution_id(), record.execution_generation()) {
        (None, None) => Ok(()),
        (Some(execution_id), Some(generation))
            if execution_id == &reservation.execution_id
                && generation == reservation.generation =>
        {
            Ok(())
        }
        _ => Err(MaintenanceItemError::Inconsistent(
            "created reservation differs from persisted execution mapping".to_string(),
        )),
    }
}

fn same_resources(
    left: &a3s_box_core::ResourceConfig,
    right: &a3s_box_core::ResourceConfig,
) -> bool {
    left.vcpus == right.vcpus
        && left.memory_mb == right.memory_mb
        && left.disk_mb == right.disk_mb
        && left.timeout == right.timeout
}

impl LifecycleMaintenanceReport {
    fn observe(
        &mut self,
        sandbox_id: SandboxId,
        result: MaintenanceItemResult<MaintenanceDisposition>,
    ) {
        self.examined += 1;
        match result {
            Ok(MaintenanceDisposition::Completed) => self.completed += 1,
            Ok(MaintenanceDisposition::Deferred) => self.deferred += 1,
            Err(error) => self.failures.push(LifecycleMaintenanceFailure {
                sandbox_id,
                message: error.to_string(),
            }),
        }
    }
}

fn ensure_current_execution(
    record: &SandboxRecord,
    lease: &ExecutionLease,
) -> MaintenanceItemResult<()> {
    if record.execution_id() != Some(&lease.execution_id) {
        return Err(MaintenanceItemError::Inconsistent(
            "runtime execution ID differs from persisted mapping".to_string(),
        ));
    }
    if record.execution_generation() != Some(lease.generation) {
        return Err(MaintenanceItemError::Inconsistent(
            "runtime execution generation differs from persisted mapping".to_string(),
        ));
    }
    if record.plan() != &lease.plan {
        return Err(MaintenanceItemError::Inconsistent(
            "runtime execution plan differs from persisted mapping".to_string(),
        ));
    }
    Ok(())
}

fn ensure_kill_target(record: &SandboxRecord, lease: &ExecutionLease) -> MaintenanceItemResult<()> {
    if record.plan() != &lease.plan {
        return Err(MaintenanceItemError::Inconsistent(
            "runtime execution plan differs from persisted kill target".to_string(),
        ));
    }
    match (record.execution_id(), record.execution_generation()) {
        (None, None) => Ok(()),
        (Some(execution_id), Some(generation))
            if execution_id == &lease.execution_id && generation == lease.generation =>
        {
            Ok(())
        }
        _ => Err(MaintenanceItemError::Inconsistent(
            "runtime execution differs from persisted kill target".to_string(),
        )),
    }
}

fn required_execution_id(
    record: &SandboxRecord,
) -> MaintenanceItemResult<&a3s_box_core::ExecutionId> {
    record.execution_id().ok_or_else(|| {
        MaintenanceItemError::Inconsistent(format!(
            "persisted state {:?} has no execution ID",
            record.state()
        ))
    })
}

fn required_execution_generation(
    record: &SandboxRecord,
) -> MaintenanceItemResult<a3s_box_core::ExecutionGeneration> {
    record.execution_generation().ok_or_else(|| {
        MaintenanceItemError::Inconsistent(format!(
            "persisted state {:?} has no execution generation",
            record.state()
        ))
    })
}
