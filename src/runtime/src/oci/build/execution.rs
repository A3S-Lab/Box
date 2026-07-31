//! Plan-bound execution over the one native Box OCI build engine.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use a3s_box_core::error::{BoxError, Result as BoxResult};
use async_trait::async_trait;
use thiserror::Error;

use super::engine::{
    build_supervised, BuildExecutionControl, BuildExecutionObserver, BuildImageCommitPermit,
};
use super::receipt::{
    inspect_stored_output, BuildExecutionLease, BuildOperationJournal, BuildProcessIdentity,
    LockedBuildOperation, PersistedBuildOperation, PersistedBuildPhase, SupervisedBuildOperation,
};
use super::{
    build, BoxBuildOptions, BoxBuildPlan, BoxBuildPlanError, BuildCancellationOutcome,
    BuildOperationIdentity, BuildOutputReceipt, BuildReceiptError, BuildResult,
    RecordedBuildResult, RecordedBuildStatus,
};
use crate::oci::ImageStore;

const EXECUTION_POLL_INTERVAL: Duration = Duration::from_millis(25);
#[cfg(target_os = "linux")]
const RUN_PROCESS_STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Successful native output bound to the canonical admitted build plan.
#[derive(Debug)]
pub(super) struct PlannedBuildResult {
    /// Canonical A3S ACL plan identity.
    pub plan_digest: String,
    /// Durable typed OCI output owned by the Box image store.
    pub output: BuildResult,
}

/// Stable failure boundary for plan admission, compilation, and execution.
#[derive(Debug, Error)]
pub enum BuildPlanExecutionError {
    /// The closed build plan could not be canonicalized or compiled.
    #[error(transparent)]
    Plan(#[from] BoxBuildPlanError),
    /// The native build engine or durable image store rejected the operation.
    #[error(transparent)]
    Build(#[from] BoxError),
    /// Durable receipt identity, persistence, or output validation failed.
    #[error(transparent)]
    Receipt(#[from] BuildReceiptError),
    /// A durable cancellation completed.
    #[error("Box build operation {operation_id} was cancelled: {message}")]
    Cancelled {
        operation_id: String,
        message: String,
    },
    /// A durable supervised execution failed.
    #[error("Box build operation {operation_id} failed: {message}")]
    Failed {
        operation_id: String,
        message: String,
    },
}

/// Compile and execute one canonical plan through Box's existing native engine.
///
/// The returned layout path points at the durable image-store copy rather than
/// the temporary build workspace, so a caller can capture or publish the exact
/// OCI graph after this future returns.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) async fn execute_build_plan(
    plan: &BoxBuildPlan,
    source_root: &Path,
    options: BoxBuildOptions,
    store: Arc<ImageStore>,
) -> Result<PlannedBuildResult, BuildPlanExecutionError> {
    let plan_digest = plan.canonical_digest()?;
    let config = plan.compile(source_root, options)?;
    let output = build(config, store).await?;
    Ok(PlannedBuildResult {
        plan_digest,
        output,
    })
}

async fn execute_supervised_build_plan(
    plan: &BoxBuildPlan,
    source_root: &Path,
    options: BoxBuildOptions,
    store: Arc<ImageStore>,
    workspace: &Path,
    control: BuildExecutionControl,
) -> Result<PlannedBuildResult, BuildPlanExecutionError> {
    let plan_digest = plan.canonical_digest()?;
    let config = plan.compile(source_root, options)?;
    let output = build_supervised(config, store, workspace, control).await?;
    Ok(PlannedBuildResult {
        plan_digest,
        output,
    })
}

/// Start or exactly replay one supervised plan-bound native build.
///
/// Starting is non-blocking. The returned status comes from the existing
/// receipt journal; callers use [`inspect_recorded_build_status`] and
/// [`cancel_recorded_build_plan`] against that same authority.
pub async fn start_recorded_build_plan(
    identity: &BuildOperationIdentity,
    plan: &BoxBuildPlan,
    source_root: &Path,
    quiet: bool,
    store: Arc<ImageStore>,
) -> Result<RecordedBuildStatus, BuildPlanExecutionError> {
    start_recorded_build_plan_internal(identity, plan, source_root, quiet, store)
        .await
        .map(|start| start.status)
}

/// Execute or exactly replay one supervised plan-bound native build.
///
/// This compatibility API starts through the same durable state machine and
/// waits for typed inspection to reach a terminal state. It owns no second
/// execution path, lock, receipt, or cleanup policy.
pub async fn execute_recorded_build_plan(
    identity: &BuildOperationIdentity,
    plan: &BoxBuildPlan,
    source_root: &Path,
    quiet: bool,
    store: Arc<ImageStore>,
) -> Result<RecordedBuildResult, BuildPlanExecutionError> {
    let start =
        start_recorded_build_plan_internal(identity, plan, source_root, quiet, Arc::clone(&store))
            .await?;
    let started_here = start.started_here;
    let mut status = Some(start.status);
    loop {
        let current = match status.take() {
            Some(status) => status,
            None => inspect_recorded_build_status(identity, plan, &store)
                .await?
                .ok_or_else(|| BuildReceiptError::Conflict {
                    operation_id: identity.operation_id().to_string(),
                    message: "supervised operation disappeared while execution was waiting"
                        .to_string(),
                })?,
        };
        match current {
            RecordedBuildStatus::Running | RecordedBuildStatus::Cancelling => {
                tokio::time::sleep(EXECUTION_POLL_INTERVAL).await;
            }
            RecordedBuildStatus::Cancelled { message } => {
                return Err(BuildPlanExecutionError::Cancelled {
                    operation_id: identity.operation_id().to_string(),
                    message,
                });
            }
            RecordedBuildStatus::Failed { message } => {
                return Err(BuildPlanExecutionError::Failed {
                    operation_id: identity.operation_id().to_string(),
                    message,
                });
            }
            RecordedBuildStatus::Succeeded(mut result) => {
                result.replayed = !started_here;
                return Ok(*result);
            }
        }
    }
}

/// Inspect and reconcile the one durable build-operation state machine.
///
/// Inspection never waits for a live build. A nonblocking execution lease
/// distinguishes a live owner from a crashed one; stale work is fenced and its
/// operation-owned workspace is reclaimed before a terminal status is written.
pub async fn inspect_recorded_build_status(
    identity: &BuildOperationIdentity,
    plan: &BoxBuildPlan,
    store: &ImageStore,
) -> Result<Option<RecordedBuildStatus>, BuildPlanExecutionError> {
    let plan_digest = plan.canonical_digest()?;
    let journal = BuildOperationJournal::for_image_store(store, identity.operation_id()).await?;
    let locked = journal.lock(identity.operation_id()).await?;
    let Some(record) = locked.read().await? else {
        return Ok(None);
    };
    match record {
        PersistedBuildOperation::Succeeded(receipt) => {
            recover_recorded_build(identity, &plan_digest, receipt, store)
                .await
                .map(Box::new)
                .map(RecordedBuildStatus::Succeeded)
                .map(Some)
        }
        PersistedBuildOperation::Supervised(operation) => {
            operation.require_identity(identity, &plan_digest)?;
            match operation.phase {
                PersistedBuildPhase::Cancelled | PersistedBuildPhase::Failed => {
                    Ok(Some(status_from_terminal_operation(&operation)?))
                }
                PersistedBuildPhase::Running | PersistedBuildPhase::Cancelling => {
                    let Some(lease) = journal.try_execution_lease(identity.operation_id()).await?
                    else {
                        return Ok(Some(status_from_live_operation(&operation)));
                    };
                    recover_stale_operation(
                        identity,
                        &plan_digest,
                        operation,
                        store,
                        &journal,
                        &locked,
                        lease,
                    )
                    .await
                    .map(Some)
                }
            }
        }
        PersistedBuildOperation::Pending(pending) => {
            pending.require_identity(identity, &plan_digest)?;
            if let Some(result) =
                adopt_committed_output(identity, &plan_digest, store, &journal, &locked).await?
            {
                return Ok(Some(RecordedBuildStatus::Succeeded(Box::new(result))));
            }
            let Some(_lease) = journal.try_execution_lease(identity.operation_id()).await? else {
                return Ok(Some(RecordedBuildStatus::Running));
            };
            journal.cleanup_workspace(identity.operation_id()).await?;
            let mut operation =
                SupervisedBuildOperation::from_pending(&pending, identity, &plan_digest)?;
            operation.finish(
                PersistedBuildPhase::Failed,
                "legacy build intent has no live execution owner or committed output".to_string(),
            );
            let operation = locked.write_supervised(operation).await?;
            Ok(Some(status_from_terminal_operation(&operation)?))
        }
    }
}

/// Inspect only a successful terminal result for compatibility.
///
/// New code should use [`inspect_recorded_build_status`] to distinguish live,
/// cancelling, cancelled, failed, and successful states.
pub async fn inspect_recorded_build_plan(
    identity: &BuildOperationIdentity,
    plan: &BoxBuildPlan,
    store: &ImageStore,
) -> Result<Option<RecordedBuildResult>, BuildPlanExecutionError> {
    Ok(
        match inspect_recorded_build_status(identity, plan, store).await? {
            Some(RecordedBuildStatus::Succeeded(result)) => Some(*result),
            Some(
                RecordedBuildStatus::Running
                | RecordedBuildStatus::Cancelling
                | RecordedBuildStatus::Cancelled { .. }
                | RecordedBuildStatus::Failed { .. },
            )
            | None => None,
        },
    )
}

/// Durably request cancellation through the existing receipt journal.
pub async fn cancel_recorded_build_plan(
    identity: &BuildOperationIdentity,
    plan: &BoxBuildPlan,
    store: &ImageStore,
) -> Result<BuildCancellationOutcome, BuildPlanExecutionError> {
    let plan_digest = plan.canonical_digest()?;
    let journal = BuildOperationJournal::for_image_store(store, identity.operation_id()).await?;
    let locked = journal.lock(identity.operation_id()).await?;
    let Some(record) = locked.read().await? else {
        return Ok(BuildCancellationOutcome::NotFound);
    };
    match record {
        PersistedBuildOperation::Succeeded(receipt) => {
            receipt.require_identity(identity, &plan_digest)?;
            Ok(BuildCancellationOutcome::AlreadyTerminal)
        }
        PersistedBuildOperation::Pending(pending) => {
            pending.require_identity(identity, &plan_digest)?;
            if adopt_committed_output(identity, &plan_digest, store, &journal, &locked)
                .await?
                .is_some()
            {
                return Ok(BuildCancellationOutcome::AlreadyTerminal);
            }
            let Some(_lease) = journal.try_execution_lease(identity.operation_id()).await? else {
                return Err(BuildReceiptError::Conflict {
                    operation_id: identity.operation_id().to_string(),
                    message: "legacy pending operation has an unknown live execution owner"
                        .to_string(),
                }
                .into());
            };
            journal.cleanup_workspace(identity.operation_id()).await?;
            let mut operation =
                SupervisedBuildOperation::from_pending(&pending, identity, &plan_digest)?;
            operation.request_cancellation();
            operation.finish(
                PersistedBuildPhase::Cancelled,
                "cancelled before native execution started".to_string(),
            );
            locked.write_supervised(operation).await?;
            Ok(BuildCancellationOutcome::Requested)
        }
        PersistedBuildOperation::Supervised(mut operation) => {
            operation.require_identity(identity, &plan_digest)?;
            match operation.phase {
                PersistedBuildPhase::Cancelled => {
                    return Ok(BuildCancellationOutcome::AlreadyCancelled);
                }
                PersistedBuildPhase::Failed => {
                    return Ok(BuildCancellationOutcome::AlreadyTerminal);
                }
                PersistedBuildPhase::Running | PersistedBuildPhase::Cancelling => {}
            }
            if adopt_committed_output(identity, &plan_digest, store, &journal, &locked)
                .await?
                .is_some()
            {
                return Ok(BuildCancellationOutcome::AlreadyTerminal);
            }

            let newly_requested = operation.request_cancellation();
            let run_process = operation.run_process;
            operation = locked.write_supervised(operation).await?;
            if let Some(lease) = journal.try_execution_lease(identity.operation_id()).await? {
                fence_run_process(run_process, identity.operation_id()).await?;
                journal.cleanup_workspace(identity.operation_id()).await?;
                operation.finish(
                    PersistedBuildPhase::Cancelled,
                    "cancelled while recovering an abandoned native execution".to_string(),
                );
                locked.write_supervised(operation).await?;
                drop(lease);
                return Ok(if newly_requested {
                    BuildCancellationOutcome::Requested
                } else {
                    BuildCancellationOutcome::AlreadyRequested
                });
            }
            drop(locked);
            fence_run_process(run_process, identity.operation_id()).await?;
            Ok(if newly_requested {
                BuildCancellationOutcome::Requested
            } else {
                BuildCancellationOutcome::AlreadyRequested
            })
        }
    }
}

/// Remove one terminal record and its operation-specific ImageStore reference.
///
/// Live or cancelling work must reach a terminal state first. The same journal
/// cleanup removes the operation workspace; there is no parallel garbage
/// collector or supervisor-owned image store.
pub async fn remove_recorded_build_plan(
    identity: &BuildOperationIdentity,
    plan: &BoxBuildPlan,
    store: &ImageStore,
) -> Result<bool, BuildPlanExecutionError> {
    let plan_digest = plan.canonical_digest()?;
    let journal = BuildOperationJournal::for_image_store(store, identity.operation_id()).await?;
    let locked = journal.lock(identity.operation_id()).await?;
    let Some(record) = locked.read().await? else {
        return Ok(false);
    };
    let (reference, expected_digest) = match record {
        PersistedBuildOperation::Pending(pending) => {
            pending.require_identity(identity, &plan_digest)?;
            (identity.output_reference().to_string(), None)
        }
        PersistedBuildOperation::Succeeded(receipt) => {
            receipt.require_identity(identity, &plan_digest)?;
            (
                receipt.output.reference,
                Some(receipt.output.descriptor.digest),
            )
        }
        PersistedBuildOperation::Supervised(operation) => {
            operation.require_identity(identity, &plan_digest)?;
            if matches!(
                operation.phase,
                PersistedBuildPhase::Running | PersistedBuildPhase::Cancelling
            ) {
                return Err(BuildReceiptError::Conflict {
                    operation_id: identity.operation_id().to_string(),
                    message: "cancel and reconcile the live build before removal".to_string(),
                }
                .into());
            }
            (identity.output_reference().to_string(), None)
        }
    };
    if let Some(stored) = store.get_checked(&reference).await? {
        if expected_digest.is_some_and(|digest| stored.digest != digest) {
            return Err(BuildReceiptError::OutputInvalid {
                operation_id: identity.operation_id().to_string(),
                message: "operation reference was rebound to another digest".to_string(),
            }
            .into());
        }
        store.remove(&reference).await?;
    }
    journal.cleanup_workspace(identity.operation_id()).await?;
    locked.delete().await?;
    Ok(true)
}

struct StartOutcome {
    status: RecordedBuildStatus,
    started_here: bool,
}

async fn start_recorded_build_plan_internal(
    identity: &BuildOperationIdentity,
    plan: &BoxBuildPlan,
    source_root: &Path,
    quiet: bool,
    store: Arc<ImageStore>,
) -> Result<StartOutcome, BuildPlanExecutionError> {
    let plan_digest = plan.canonical_digest()?;
    let journal = BuildOperationJournal::for_image_store(&store, identity.operation_id()).await?;
    let locked = journal.lock(identity.operation_id()).await?;
    let record = locked.read().await?;

    match record {
        Some(PersistedBuildOperation::Succeeded(receipt)) => {
            let result = recover_recorded_build(identity, &plan_digest, receipt, &store).await?;
            return Ok(StartOutcome {
                status: RecordedBuildStatus::Succeeded(Box::new(result)),
                started_here: false,
            });
        }
        Some(PersistedBuildOperation::Supervised(operation)) => {
            operation.require_identity(identity, &plan_digest)?;
            if matches!(
                operation.phase,
                PersistedBuildPhase::Cancelled | PersistedBuildPhase::Failed
            ) {
                return Ok(StartOutcome {
                    status: status_from_terminal_operation(&operation)?,
                    started_here: false,
                });
            }
            let Some(lease) = journal.try_execution_lease(identity.operation_id()).await? else {
                return Ok(StartOutcome {
                    status: status_from_live_operation(&operation),
                    started_here: false,
                });
            };
            let status = recover_stale_operation(
                identity,
                &plan_digest,
                operation,
                &store,
                &journal,
                &locked,
                lease,
            )
            .await?;
            return Ok(StartOutcome {
                status,
                started_here: false,
            });
        }
        Some(PersistedBuildOperation::Pending(pending)) => {
            pending.require_identity(identity, &plan_digest)?;
            if let Some(result) =
                adopt_committed_output(identity, &plan_digest, &store, &journal, &locked).await?
            {
                return Ok(StartOutcome {
                    status: RecordedBuildStatus::Succeeded(Box::new(result)),
                    started_here: false,
                });
            }
            let lease = journal
                .try_execution_lease(identity.operation_id())
                .await?
                .ok_or_else(|| BuildReceiptError::Conflict {
                    operation_id: identity.operation_id().to_string(),
                    message: "pending intent has an unknown live execution owner".to_string(),
                })?;
            let workspace = journal.prepare_workspace(identity.operation_id()).await?;
            let operation =
                SupervisedBuildOperation::from_pending(&pending, identity, &plan_digest)?;
            locked.write_supervised(operation).await?;
            drop(locked);
            spawn_supervised_build(SupervisedBuildTask {
                identity: identity.clone(),
                plan: plan.clone(),
                plan_digest,
                source_root: source_root.to_path_buf(),
                quiet,
                store,
                journal,
                workspace,
                lease,
            });
            return Ok(StartOutcome {
                status: RecordedBuildStatus::Running,
                started_here: true,
            });
        }
        None => {}
    }

    if store
        .get_checked(identity.output_reference())
        .await?
        .is_some()
    {
        return Err(BuildReceiptError::OutputInvalid {
            operation_id: identity.operation_id().to_string(),
            message: "operation output exists without a persisted owning intent".to_string(),
        }
        .into());
    }
    let lease = journal
        .try_execution_lease(identity.operation_id())
        .await?
        .ok_or_else(|| BuildReceiptError::Conflict {
            operation_id: identity.operation_id().to_string(),
            message: "execution lease exists without a persisted operation record".to_string(),
        })?;
    let workspace = journal.prepare_workspace(identity.operation_id()).await?;
    let operation = SupervisedBuildOperation::new(identity, plan_digest.clone())?;
    locked.write_supervised(operation).await?;
    drop(locked);
    spawn_supervised_build(SupervisedBuildTask {
        identity: identity.clone(),
        plan: plan.clone(),
        plan_digest,
        source_root: source_root.to_path_buf(),
        quiet,
        store,
        journal,
        workspace,
        lease,
    });
    Ok(StartOutcome {
        status: RecordedBuildStatus::Running,
        started_here: true,
    })
}

struct SupervisedBuildTask {
    identity: BuildOperationIdentity,
    plan: BoxBuildPlan,
    plan_digest: String,
    source_root: PathBuf,
    quiet: bool,
    store: Arc<ImageStore>,
    journal: BuildOperationJournal,
    workspace: PathBuf,
    lease: BuildExecutionLease,
}

fn spawn_supervised_build(task: SupervisedBuildTask) {
    tokio::spawn(async move {
        let operation_id = task.identity.operation_id().to_string();
        if let Err(error) = run_supervised_build(task).await {
            tracing::error!(
                operation_id,
                %error,
                "Supervised native build ended before committing a terminal journal state"
            );
        }
    });
}

async fn run_supervised_build(task: SupervisedBuildTask) -> Result<(), BuildPlanExecutionError> {
    let SupervisedBuildTask {
        identity,
        plan,
        plan_digest,
        source_root,
        quiet,
        store,
        journal,
        workspace,
        lease: _lease,
    } = task;
    let observer = Arc::new(JournalBuildObserver {
        journal: journal.clone(),
        identity: identity.clone(),
        plan_digest: plan_digest.clone(),
    });
    let control = BuildExecutionControl::new(observer);
    let result = execute_supervised_build_plan(
        &plan,
        &source_root,
        BoxBuildOptions {
            tag: Some(identity.output_reference().to_string()),
            quiet,
        },
        Arc::clone(&store),
        &workspace,
        control,
    )
    .await;

    if let Ok(planned) = result {
        journal.cleanup_workspace(identity.operation_id()).await?;
        let receipt =
            BuildOutputReceipt::from_result(&identity, planned.plan_digest, &planned.output)?;
        let locked = journal.lock(identity.operation_id()).await?;
        locked.write_succeeded(receipt).await?;
        return Ok(());
    }

    let error = result.unwrap_err();
    finish_failed_execution(&identity, &plan_digest, &store, &journal, error.to_string()).await
}

async fn finish_failed_execution(
    identity: &BuildOperationIdentity,
    plan_digest: &str,
    store: &ImageStore,
    journal: &BuildOperationJournal,
    message: String,
) -> Result<(), BuildPlanExecutionError> {
    let locked = journal.lock(identity.operation_id()).await?;
    let Some(record) = locked.read().await? else {
        return Err(BuildReceiptError::Conflict {
            operation_id: identity.operation_id().to_string(),
            message: "operation record disappeared before failure reconciliation".to_string(),
        }
        .into());
    };
    if matches!(record, PersistedBuildOperation::Succeeded(_)) {
        return Ok(());
    }
    if let Some(result) =
        adopt_committed_output(identity, plan_digest, store, journal, &locked).await?
    {
        drop(result);
        return Ok(());
    }
    let mut operation = match record {
        PersistedBuildOperation::Supervised(operation) => operation,
        PersistedBuildOperation::Pending(pending) => {
            SupervisedBuildOperation::from_pending(&pending, identity, plan_digest)?
        }
        PersistedBuildOperation::Succeeded(_) => unreachable!(),
    };
    operation.require_identity(identity, plan_digest)?;
    fence_run_process(operation.run_process, identity.operation_id()).await?;
    journal.cleanup_workspace(identity.operation_id()).await?;
    let phase = if operation.phase == PersistedBuildPhase::Cancelling {
        PersistedBuildPhase::Cancelled
    } else {
        PersistedBuildPhase::Failed
    };
    operation.finish(phase, message);
    locked.write_supervised(operation).await?;
    Ok(())
}

async fn recover_stale_operation(
    identity: &BuildOperationIdentity,
    plan_digest: &str,
    mut operation: SupervisedBuildOperation,
    store: &ImageStore,
    journal: &BuildOperationJournal,
    locked: &LockedBuildOperation,
    _lease: BuildExecutionLease,
) -> Result<RecordedBuildStatus, BuildPlanExecutionError> {
    if let Some(result) =
        adopt_committed_output(identity, plan_digest, store, journal, locked).await?
    {
        return Ok(RecordedBuildStatus::Succeeded(Box::new(result)));
    }
    fence_run_process(operation.run_process, identity.operation_id()).await?;
    journal.cleanup_workspace(identity.operation_id()).await?;
    let (phase, message) = if operation.phase == PersistedBuildPhase::Cancelling {
        (
            PersistedBuildPhase::Cancelled,
            "cancelled after the native execution owner exited".to_string(),
        )
    } else {
        (
            PersistedBuildPhase::Failed,
            "native execution owner exited before committing an output".to_string(),
        )
    };
    operation.finish(phase, message);
    let operation = locked.write_supervised(operation).await?;
    status_from_terminal_operation(&operation)
}

async fn adopt_committed_output(
    identity: &BuildOperationIdentity,
    plan_digest: &str,
    store: &ImageStore,
    journal: &BuildOperationJournal,
    locked: &LockedBuildOperation,
) -> Result<Option<RecordedBuildResult>, BuildPlanExecutionError> {
    let Some(output) =
        inspect_stored_output(identity.operation_id(), identity.output_reference(), store).await?
    else {
        return Ok(None);
    };
    journal.cleanup_workspace(identity.operation_id()).await?;
    let receipt = BuildOutputReceipt::from_result(identity, plan_digest.to_string(), &output)?;
    let receipt = locked.write_succeeded(receipt).await?;
    Ok(Some(RecordedBuildResult {
        receipt,
        output,
        replayed: true,
    }))
}

fn status_from_live_operation(operation: &SupervisedBuildOperation) -> RecordedBuildStatus {
    if operation.phase == PersistedBuildPhase::Cancelling {
        RecordedBuildStatus::Cancelling
    } else {
        RecordedBuildStatus::Running
    }
}

fn status_from_terminal_operation(
    operation: &SupervisedBuildOperation,
) -> Result<RecordedBuildStatus, BuildPlanExecutionError> {
    let message = operation
        .terminal_message()
        .ok_or_else(|| BuildReceiptError::InvalidReceipt {
            operation_id: operation.operation_id().to_string(),
            message: "terminal operation has no message".to_string(),
        })?
        .to_string();
    match operation.phase {
        PersistedBuildPhase::Cancelled => Ok(RecordedBuildStatus::Cancelled { message }),
        PersistedBuildPhase::Failed => Ok(RecordedBuildStatus::Failed { message }),
        PersistedBuildPhase::Running | PersistedBuildPhase::Cancelling => {
            Err(BuildReceiptError::InvalidReceipt {
                operation_id: operation.operation_id().to_string(),
                message: "live operation was decoded through the terminal boundary".to_string(),
            }
            .into())
        }
    }
}

async fn recover_recorded_build(
    identity: &BuildOperationIdentity,
    plan_digest: &str,
    receipt: BuildOutputReceipt,
    store: &ImageStore,
) -> Result<RecordedBuildResult, BuildPlanExecutionError> {
    receipt.require_identity(identity, plan_digest)?;
    let output = receipt.resolve(store).await?;
    Ok(RecordedBuildResult {
        receipt,
        output,
        replayed: true,
    })
}

struct JournalBuildObserver {
    journal: BuildOperationJournal,
    identity: BuildOperationIdentity,
    plan_digest: String,
}

#[async_trait]
impl BuildExecutionObserver for JournalBuildObserver {
    async fn cancellation_requested(&self) -> BoxResult<bool> {
        let locked = self
            .journal
            .lock(self.identity.operation_id())
            .await
            .map_err(observer_error)?;
        let record = locked.read().await.map_err(observer_error)?;
        match record {
            Some(PersistedBuildOperation::Supervised(operation)) => {
                operation
                    .require_identity(&self.identity, &self.plan_digest)
                    .map_err(observer_error)?;
                Ok(operation.phase != PersistedBuildPhase::Running)
            }
            Some(PersistedBuildOperation::Succeeded(_)) => Ok(true),
            Some(PersistedBuildOperation::Pending(_)) | None => Err(BoxError::BuildError(
                "supervised build lost its authoritative operation state".to_string(),
            )),
        }
    }

    async fn acquire_image_commit_permit(&self) -> BoxResult<BuildImageCommitPermit> {
        let locked = self
            .journal
            .lock(self.identity.operation_id())
            .await
            .map_err(observer_error)?;
        let Some(PersistedBuildOperation::Supervised(operation)) =
            locked.read().await.map_err(observer_error)?
        else {
            return Err(BoxError::BuildError(
                "supervised build lost its authoritative operation state before image commit"
                    .to_string(),
            ));
        };
        operation
            .require_identity(&self.identity, &self.plan_digest)
            .map_err(observer_error)?;
        if operation.phase != PersistedBuildPhase::Running {
            return Err(BoxError::BuildError(
                "recorded build operation was cancelled before image commit".to_string(),
            ));
        }
        Ok(BuildImageCommitPermit::new(locked))
    }

    async fn run_process_started(&self, pid: u32, start_time: Option<u64>) -> BoxResult<()> {
        let locked = self
            .journal
            .lock(self.identity.operation_id())
            .await
            .map_err(observer_error)?;
        let Some(PersistedBuildOperation::Supervised(mut operation)) =
            locked.read().await.map_err(observer_error)?
        else {
            return Err(BoxError::BuildError(
                "supervised RUN has no active operation record".to_string(),
            ));
        };
        operation
            .require_identity(&self.identity, &self.plan_digest)
            .map_err(observer_error)?;
        operation
            .set_run_process(Some(BuildProcessIdentity { pid, start_time }))
            .map_err(observer_error)?;
        let cancelling = operation.phase == PersistedBuildPhase::Cancelling;
        locked
            .write_supervised(operation)
            .await
            .map_err(observer_error)?;
        if cancelling {
            return Err(BoxError::BuildError(
                "recorded build cancellation raced Dockerfile RUN startup".to_string(),
            ));
        }
        Ok(())
    }

    async fn run_process_finished(&self, pid: u32, start_time: Option<u64>) -> BoxResult<()> {
        let locked = self
            .journal
            .lock(self.identity.operation_id())
            .await
            .map_err(observer_error)?;
        let Some(PersistedBuildOperation::Supervised(mut operation)) =
            locked.read().await.map_err(observer_error)?
        else {
            return Ok(());
        };
        operation
            .require_identity(&self.identity, &self.plan_digest)
            .map_err(observer_error)?;
        if operation.run_process == Some(BuildProcessIdentity { pid, start_time }) {
            operation.set_run_process(None).map_err(observer_error)?;
            locked
                .write_supervised(operation)
                .await
                .map_err(observer_error)?;
        }
        Ok(())
    }
}

fn observer_error(error: BuildReceiptError) -> BoxError {
    BoxError::BuildError(format!(
        "build operation journal rejected native execution: {error}"
    ))
}

async fn fence_run_process(
    process: Option<BuildProcessIdentity>,
    operation_id: &a3s_box_core::OperationId,
) -> Result<(), BuildReceiptError> {
    let Some(process) = process else {
        return Ok(());
    };
    let operation = operation_id.to_string();
    tokio::task::spawn_blocking(move || fence_run_process_blocking(process, &operation))
        .await
        .map_err(|error| BuildReceiptError::Task {
            operation_id: operation_id.to_string(),
            message: format!("RUN process fencing task failed: {error}"),
        })?
}

#[cfg(target_os = "linux")]
fn fence_run_process_blocking(
    process: BuildProcessIdentity,
    operation_id: &str,
) -> Result<(), BuildReceiptError> {
    if !crate::process::is_process_alive_with_identity(process.pid, process.start_time) {
        return Ok(());
    }
    let pid = i32::try_from(process.pid).map_err(|_| BuildReceiptError::InvalidReceipt {
        operation_id: operation_id.to_string(),
        message: "RUN process PID exceeds the host signal range".to_string(),
    })?;
    if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
        let source = std::io::Error::last_os_error();
        if source.raw_os_error() != Some(libc::ESRCH) {
            return Err(BuildReceiptError::StoreIo {
                message: format!("failed to kill RUN process for operation {operation_id}"),
                source,
            });
        }
    }
    let deadline = std::time::Instant::now() + RUN_PROCESS_STOP_TIMEOUT;
    while crate::process::is_process_running_with_identity(process.pid, process.start_time) {
        if std::time::Instant::now() >= deadline {
            return Err(BuildReceiptError::Task {
                operation_id: operation_id.to_string(),
                message: format!(
                    "RUN process {} did not stop within {:?}",
                    process.pid, RUN_PROCESS_STOP_TIMEOUT
                ),
            });
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn fence_run_process_blocking(
    _process: BuildProcessIdentity,
    operation_id: &str,
) -> Result<(), BuildReceiptError> {
    Err(BuildReceiptError::Task {
        operation_id: operation_id.to_string(),
        message: "a recorded Linux RUN process cannot be fenced on this host".to_string(),
    })
}

#[cfg(test)]
mod tests;
