use std::num::NonZeroU32;
use std::sync::Arc;

use a3s_box_core::{ExecutionManager, ExecutionSnapshot};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use crate::control::{
    Clock, PublicSandboxState, ResolvedTemplate, SandboxId, SandboxRecord,
};

use super::{
    validate_snapshot_name, SnapshotId, SnapshotModelError, SnapshotRecord,
    SnapshotReplaceResult, SnapshotRepository, SnapshotRepositoryError, SnapshotState,
};

#[derive(Debug)]
pub struct PendingSnapshot {
    pub record: SnapshotRecord,
    pub execution: ExecutionSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SnapshotCursor {
    pub created_at: DateTime<Utc>,
    pub snapshot_id: SnapshotId,
}

#[derive(Debug, Clone)]
pub struct SnapshotPage {
    pub records: Vec<SnapshotRecord>,
    pub next: Option<SnapshotCursor>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SnapshotReconciliationReport {
    pub examined: usize,
    pub completed: usize,
    pub deferred: usize,
    pub failures: Vec<String>,
}

#[derive(Debug, Error)]
pub enum SnapshotServiceError {
    #[error("invalid snapshot request: {0}")]
    InvalidRequest(String),
    #[error("snapshot not found")]
    NotFound,
    #[error("snapshot already exists")]
    Duplicate,
    #[error("snapshot is in use or changing state")]
    Conflict,
    #[error(transparent)]
    Repository(#[from] SnapshotRepositoryError),
    #[error(transparent)]
    Execution(#[from] a3s_box_core::ExecutionManagerError),
    #[error(transparent)]
    Model(#[from] SnapshotModelError),
}

pub type SnapshotServiceResult<T> = std::result::Result<T, SnapshotServiceError>;

#[derive(Clone)]
pub struct SnapshotService {
    repository: Arc<dyn SnapshotRepository>,
    executions: Arc<dyn ExecutionManager>,
    clock: Arc<dyn Clock>,
}

pub struct SnapshotServiceDependencies {
    pub repository: Arc<dyn SnapshotRepository>,
    pub executions: Arc<dyn ExecutionManager>,
    pub clock: Arc<dyn Clock>,
}

impl SnapshotService {
    pub fn new(dependencies: SnapshotServiceDependencies) -> Self {
        Self {
            repository: dependencies.repository,
            executions: dependencies.executions,
            clock: dependencies.clock,
        }
    }

    pub async fn capture(
        &self,
        owner_id: &str,
        source: &SandboxRecord,
        name: Option<&str>,
        template: ResolvedTemplate,
    ) -> SnapshotServiceResult<PendingSnapshot> {
        if owner_id.trim().is_empty() || source.owner_id() != owner_id {
            return Err(SnapshotServiceError::NotFound);
        }
        if let Some(name) = name {
            validate_snapshot_name(name)?;
        }
        let source_state = source.public_state().ok_or(SnapshotServiceError::NotFound)?;
        let execution_id = source
            .execution_id()
            .ok_or(SnapshotServiceError::Conflict)?;
        let generation = source
            .execution_generation()
            .ok_or(SnapshotServiceError::Conflict)?;
        let snapshot_id = SnapshotId::new(format!("snap-{}", Uuid::new_v4().simple()))?;
        let content_id = a3s_box_core::ExecutionSnapshotId::new(format!(
            "e2bsnap-{}",
            Uuid::new_v4().simple()
        ))?;
        let record = SnapshotRecord::creating(
            snapshot_id,
            content_id.clone(),
            owner_id,
            source.sandbox_id().clone(),
            execution_id.clone(),
            generation,
            source_state,
            name.map(str::to_string),
            owner_namespace(owner_id),
            template,
            self.clock.now(),
        )?;
        match self.repository.insert(record.clone()).await {
            Ok(()) => {}
            Err(SnapshotRepositoryError::Duplicate) => {
                return Err(SnapshotServiceError::Duplicate)
            }
            Err(error) => return Err(error.into()),
        }

        let execution = match self
            .executions
            .create_filesystem_snapshot(execution_id, generation, &content_id)
            .await
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                if matches!(
                    self.executions.filesystem_snapshot_size(&content_id).await,
                    Ok(None)
                ) {
                    let _ = self
                        .repository
                        .delete(record.snapshot_id(), SnapshotState::Creating)
                        .await;
                }
                return Err(match error {
                    a3s_box_core::ExecutionManagerError::Conflict { .. } => {
                        SnapshotServiceError::Conflict
                    }
                    error => SnapshotServiceError::Execution(error),
                });
            }
        };
        if !consistent_execution(&record, &execution) {
            return Err(SnapshotServiceError::Execution(
                a3s_box_core::ExecutionManagerError::Internal(
                    "runtime returned inconsistent snapshot completion evidence".to_string(),
                ),
            ));
        }
        Ok(PendingSnapshot { record, execution })
    }

    pub async fn publish(
        &self,
        mut pending: PendingSnapshot,
    ) -> SnapshotServiceResult<SnapshotRecord> {
        pending.record.mark_active(pending.execution.size_bytes)?;
        self.replace(SnapshotState::Creating, pending.record.clone())
            .await?;
        Ok(pending.record)
    }

    pub async fn list(
        &self,
        owner_id: &str,
        source_sandbox_id: Option<&SandboxId>,
        limit: NonZeroU32,
        after: Option<&SnapshotCursor>,
    ) -> SnapshotServiceResult<SnapshotPage> {
        let records = self.repository.list(owner_id).await?;
        let mut eligible = records.into_iter().filter(|record| {
            source_sandbox_id
                .is_none_or(|source| record.source_sandbox_id() == source)
                && after.is_none_or(|cursor| {
                    (record.created_at(), record.snapshot_id())
                        > (cursor.created_at, &cursor.snapshot_id)
                })
        });
        let mut page = eligible
            .by_ref()
            .take(limit.get() as usize + 1)
            .collect::<Vec<_>>();
        let has_more = page.len() > limit.get() as usize;
        if has_more {
            page.pop();
        }
        let next = if has_more {
            page.last().map(|last| SnapshotCursor {
                created_at: last.created_at(),
                snapshot_id: last.snapshot_id().clone(),
            })
        } else {
            None
        };
        Ok(SnapshotPage {
            records: page,
            next,
        })
    }

    pub async fn delete(&self, owner_id: &str, reference: &str) -> SnapshotServiceResult<bool> {
        let Some(mut record) = self
            .repository
            .get_by_reference(owner_id, &normalize_reference(reference))
            .await?
            .filter(|record| record.state() == SnapshotState::Active)
        else {
            return Ok(false);
        };
        record.begin_delete()?;
        self.replace(SnapshotState::Active, record.clone()).await?;
        match self
            .executions
            .delete_filesystem_snapshot(record.content_id())
            .await
        {
            Ok(_) => {}
            Err(a3s_box_core::ExecutionManagerError::Conflict { .. }) => {
                record.abort_delete()?;
                self.replace(SnapshotState::Deleting, record).await?;
                return Err(SnapshotServiceError::Conflict);
            }
            Err(error) => {
                record.abort_delete()?;
                self.replace(SnapshotState::Deleting, record).await?;
                return Err(error.into());
            }
        }
        self.delete_record(record.snapshot_id(), SnapshotState::Deleting)
            .await?;
        Ok(true)
    }

    pub async fn reconcile_startup(
        &self,
    ) -> SnapshotServiceResult<SnapshotReconciliationReport> {
        let mut report = SnapshotReconciliationReport::default();
        for state in [SnapshotState::Creating, SnapshotState::Deleting] {
            for mut record in self.repository.list_in_state(state).await? {
                report.examined += 1;
                let outcome = match state {
                    SnapshotState::Creating => self
                        .reconcile_creating(&mut record, &mut report)
                        .await?,
                    SnapshotState::Deleting => match self
                        .executions
                        .delete_filesystem_snapshot(record.content_id())
                        .await
                    {
                        Ok(_) => {
                            self.delete_record(
                                record.snapshot_id(),
                                SnapshotState::Deleting,
                            )
                            .await?;
                            ReconciliationOutcome::Completed
                        }
                        Err(a3s_box_core::ExecutionManagerError::Conflict { .. }) => {
                            record.abort_delete()?;
                            self.replace(SnapshotState::Deleting, record).await?;
                            ReconciliationOutcome::Deferred
                        }
                        Err(error) => {
                            report.failures.push(error.to_string());
                            ReconciliationOutcome::Deferred
                        }
                    },
                    SnapshotState::Active => {
                        report.failures.push(format!(
                            "active snapshot {} was returned by a transitional-state query",
                            record.snapshot_id()
                        ));
                        ReconciliationOutcome::Deferred
                    }
                };
                match outcome {
                    ReconciliationOutcome::Completed => report.completed += 1,
                    ReconciliationOutcome::Deferred => report.deferred += 1,
                }
            }
        }
        Ok(report)
    }

    async fn replace(
        &self,
        expected: SnapshotState,
        record: SnapshotRecord,
    ) -> SnapshotServiceResult<()> {
        match self.repository.replace(expected, record).await? {
            SnapshotReplaceResult::Updated => Ok(()),
            SnapshotReplaceResult::NotFound => Err(SnapshotServiceError::NotFound),
            SnapshotReplaceResult::Conflict => Err(SnapshotServiceError::Conflict),
        }
    }

    async fn delete_record(
        &self,
        snapshot_id: &SnapshotId,
        expected: SnapshotState,
    ) -> SnapshotServiceResult<()> {
        match self.repository.delete(snapshot_id, expected).await? {
            SnapshotReplaceResult::Updated => Ok(()),
            SnapshotReplaceResult::NotFound => Err(SnapshotServiceError::NotFound),
            SnapshotReplaceResult::Conflict => Err(SnapshotServiceError::Conflict),
        }
    }

    async fn reconcile_creating(
        &self,
        record: &mut SnapshotRecord,
        report: &mut SnapshotReconciliationReport,
    ) -> SnapshotServiceResult<ReconciliationOutcome> {
        match self
            .executions
            .create_filesystem_snapshot(
                record.source_execution_id(),
                record.source_execution_generation(),
                record.content_id(),
            )
            .await
        {
            Ok(execution) if consistent_execution(record, &execution) => {
                record.mark_active(execution.size_bytes)?;
                self.replace(SnapshotState::Creating, record.clone())
                    .await?;
                Ok(ReconciliationOutcome::Completed)
            }
            Ok(_) => {
                report.failures.push(format!(
                    "runtime returned inconsistent recovery evidence for snapshot {}",
                    record.snapshot_id()
                ));
                Ok(ReconciliationOutcome::Deferred)
            }
            Err(error @ a3s_box_core::ExecutionManagerError::NotFound(_)) => {
                self.resolve_missing_source(record, report, error).await
            }
            Err(error @ a3s_box_core::ExecutionManagerError::Conflict { .. }) => {
                self.resolve_missing_source(record, report, error).await
            }
            Err(error) => {
                report.failures.push(error.to_string());
                Ok(ReconciliationOutcome::Deferred)
            }
        }
    }

    async fn resolve_missing_source(
        &self,
        record: &mut SnapshotRecord,
        report: &mut SnapshotReconciliationReport,
        error: a3s_box_core::ExecutionManagerError,
    ) -> SnapshotServiceResult<ReconciliationOutcome> {
        match self
            .executions
            .filesystem_snapshot_size(record.content_id())
            .await
        {
            Ok(Some(size_bytes)) => {
                record.mark_active(size_bytes)?;
                self.replace(SnapshotState::Creating, record.clone())
                    .await?;
                Ok(ReconciliationOutcome::Completed)
            }
            Ok(None) if matches!(error, a3s_box_core::ExecutionManagerError::NotFound(_)) => {
                self.delete_record(record.snapshot_id(), SnapshotState::Creating)
                    .await?;
                Ok(ReconciliationOutcome::Completed)
            }
            Ok(None) => {
                report.failures.push(error.to_string());
                Ok(ReconciliationOutcome::Deferred)
            }
            Err(inspect_error) => {
                report
                    .failures
                    .push(format!("{error}; snapshot inspection failed: {inspect_error}"));
                Ok(ReconciliationOutcome::Deferred)
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ReconciliationOutcome {
    Completed,
    Deferred,
}

fn owner_namespace(owner_id: &str) -> String {
    let digest = hex::encode(Sha256::digest(owner_id.as_bytes()));
    format!("a3s-{}", &digest[..12])
}

fn normalize_reference(reference: &str) -> String {
    if reference.contains(':') {
        reference.to_string()
    } else {
        format!("{reference}:default")
    }
}

fn public_execution_state(state: PublicSandboxState) -> a3s_box_core::ExecutionState {
    match state {
        PublicSandboxState::Running => a3s_box_core::ExecutionState::Running,
        PublicSandboxState::Paused => a3s_box_core::ExecutionState::Paused,
    }
}

fn consistent_execution(record: &SnapshotRecord, execution: &ExecutionSnapshot) -> bool {
    &execution.snapshot_id == record.content_id()
        && execution.state == public_execution_state(record.source_state())
        && &execution.lease.execution_id == record.source_execution_id()
        && execution.lease.generation == record.source_execution_generation()
}
