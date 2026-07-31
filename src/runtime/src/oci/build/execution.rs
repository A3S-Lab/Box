//! Plan-bound execution over the one native Box OCI build engine.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use a3s_box_core::error::BoxError;
use thiserror::Error;

use super::cache::{inspect_build_cache_artifact, BuildCacheExportIdentity, RecordedBuildCache};
use super::engine::{build_supervised, BuildExecutionControl};
use super::receipt::{
    inspect_stored_output, BuildExecutionLease, BuildOperationJournal, LockedBuildOperation,
    PersistedBuildOperation, PersistedBuildPhase, SupervisedBuildOperation,
};
use super::{
    build, BoxBuildOptions, BoxBuildPlan, BoxBuildPlanError, BuildCachePolicy,
    BuildCancellationOutcome, BuildOperationIdentity, BuildOutputReceipt, BuildReceiptError,
    BuildResult, RecordedBuildResult, RecordedBuildStatus,
};
use crate::oci::ImageStore;

mod supervision;

use supervision::{fence_run_process, JournalBuildObserver};

const EXECUTION_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Successful native output bound to the canonical admitted build plan.
#[derive(Debug)]
pub(super) struct PlannedBuildResult {
    /// Canonical A3S ACL plan identity.
    pub plan_digest: String,
    /// Durable typed OCI output owned by the Box image store.
    pub output: BuildResult,
    /// Portable native cache artifact committed with the image output.
    pub cache: Option<RecordedBuildCache>,
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
        cache: None,
    })
}

async fn execute_supervised_build_plan(
    identity: &BuildOperationIdentity,
    plan: &BoxBuildPlan,
    source_root: &Path,
    options: BoxBuildOptions,
    store: Arc<ImageStore>,
    workspace: &Path,
    control: BuildExecutionControl,
) -> Result<PlannedBuildResult, BuildPlanExecutionError> {
    let plan_digest = plan.canonical_digest()?;
    let config = plan.compile(source_root, options)?;
    let cache_identity = (plan.cache() == BuildCachePolicy::ContentAddressed)
        .then(|| {
            BuildCacheExportIdentity::new(
                identity.source_digest(),
                plan_digest.clone(),
                plan.platform().clone(),
            )
        })
        .transpose()?;
    let result = build_supervised(config, store, workspace, control, cache_identity).await?;
    Ok(PlannedBuildResult {
        plan_digest,
        output: result.output,
        cache: result.cache,
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
    let cache_policy = plan.cache();
    let journal = BuildOperationJournal::for_image_store(store, identity.operation_id()).await?;
    let locked = journal.lock(identity.operation_id()).await?;
    let Some(record) = locked.read().await? else {
        return Ok(None);
    };
    match record {
        PersistedBuildOperation::Succeeded(receipt) => recover_recorded_build(
            identity,
            &plan_digest,
            cache_policy,
            *receipt,
            store,
            &journal,
        )
        .await
        .map(Box::new)
        .map(RecordedBuildStatus::Succeeded)
        .map(Some),
        PersistedBuildOperation::Supervised(operation) => {
            operation.require_identity(identity, &plan_digest, cache_policy)?;
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
                adopt_committed_output(identity, &plan_digest, None, store, &journal, &locked)
                    .await?
            {
                return Ok(Some(RecordedBuildStatus::Succeeded(Box::new(result))));
            }
            let Some(_lease) = journal.try_execution_lease(identity.operation_id()).await? else {
                return Ok(Some(RecordedBuildStatus::Running));
            };
            journal.cleanup_workspace(identity.operation_id()).await?;
            journal
                .cleanup_cache_export(identity.operation_id())
                .await?;
            let mut operation = SupervisedBuildOperation::from_pending(
                &pending,
                identity,
                &plan_digest,
                cache_policy,
            )?;
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
    let cache_policy = plan.cache();
    let journal = BuildOperationJournal::for_image_store(store, identity.operation_id()).await?;
    let locked = journal.lock(identity.operation_id()).await?;
    let Some(record) = locked.read().await? else {
        return Ok(BuildCancellationOutcome::NotFound);
    };
    match record {
        PersistedBuildOperation::Succeeded(receipt) => {
            receipt.require_identity(identity, &plan_digest, cache_policy)?;
            Ok(BuildCancellationOutcome::AlreadyTerminal)
        }
        PersistedBuildOperation::Pending(pending) => {
            pending.require_identity(identity, &plan_digest)?;
            if adopt_committed_output(identity, &plan_digest, None, store, &journal, &locked)
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
            journal
                .cleanup_cache_export(identity.operation_id())
                .await?;
            let mut operation = SupervisedBuildOperation::from_pending(
                &pending,
                identity,
                &plan_digest,
                cache_policy,
            )?;
            operation.request_cancellation();
            operation.finish(
                PersistedBuildPhase::Cancelled,
                "cancelled before native execution started".to_string(),
            );
            locked.write_supervised(operation).await?;
            Ok(BuildCancellationOutcome::Requested)
        }
        PersistedBuildOperation::Supervised(mut operation) => {
            operation.require_identity(identity, &plan_digest, cache_policy)?;
            match operation.phase {
                PersistedBuildPhase::Cancelled => {
                    return Ok(BuildCancellationOutcome::AlreadyCancelled);
                }
                PersistedBuildPhase::Failed => {
                    return Ok(BuildCancellationOutcome::AlreadyTerminal);
                }
                PersistedBuildPhase::Running | PersistedBuildPhase::Cancelling => {}
            }
            if adopt_committed_output(
                identity,
                &plan_digest,
                operation.cache_policy(),
                store,
                &journal,
                &locked,
            )
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
                journal
                    .cleanup_cache_export(identity.operation_id())
                    .await?;
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
    let cache_policy = plan.cache();
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
            let receipt = *receipt;
            receipt.require_identity(identity, &plan_digest, cache_policy)?;
            (
                receipt.output.reference,
                Some(receipt.output.descriptor.digest),
            )
        }
        PersistedBuildOperation::Supervised(operation) => {
            operation.require_identity(identity, &plan_digest, cache_policy)?;
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
    journal
        .cleanup_cache_export(identity.operation_id())
        .await?;
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
    let cache_policy = plan.cache();
    let journal = BuildOperationJournal::for_image_store(&store, identity.operation_id()).await?;
    let locked = journal.lock(identity.operation_id()).await?;
    let record = locked.read().await?;

    match record {
        Some(PersistedBuildOperation::Succeeded(receipt)) => {
            let result = recover_recorded_build(
                identity,
                &plan_digest,
                cache_policy,
                *receipt,
                &store,
                &journal,
            )
            .await?;
            return Ok(StartOutcome {
                status: RecordedBuildStatus::Succeeded(Box::new(result)),
                started_here: false,
            });
        }
        Some(PersistedBuildOperation::Supervised(operation)) => {
            operation.require_identity(identity, &plan_digest, cache_policy)?;
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
                adopt_committed_output(identity, &plan_digest, None, &store, &journal, &locked)
                    .await?
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
            let operation = SupervisedBuildOperation::from_pending(
                &pending,
                identity,
                &plan_digest,
                cache_policy,
            )?;
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
    let operation = SupervisedBuildOperation::new(identity, plan_digest.clone(), cache_policy)?;
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
        cache_policy: plan.cache(),
    });
    let control = BuildExecutionControl::new(observer);
    let result = execute_supervised_build_plan(
        &identity,
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
        let receipt = BuildOutputReceipt::from_result(
            &identity,
            planned.plan_digest,
            &planned.output,
            plan.cache(),
            planned.cache.as_ref(),
        )?;
        let locked = journal.lock(identity.operation_id()).await?;
        locked.write_succeeded(receipt).await?;
        return Ok(());
    }

    let error = result.unwrap_err();
    finish_failed_execution(
        &identity,
        &plan_digest,
        plan.cache(),
        &store,
        &journal,
        error.to_string(),
    )
    .await
}

async fn finish_failed_execution(
    identity: &BuildOperationIdentity,
    plan_digest: &str,
    cache_policy: BuildCachePolicy,
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
    let persisted_cache_policy = match &record {
        PersistedBuildOperation::Supervised(operation) => operation.cache_policy(),
        PersistedBuildOperation::Pending(_) => None,
        PersistedBuildOperation::Succeeded(_) => unreachable!(),
    };
    if let Some(result) = adopt_committed_output(
        identity,
        plan_digest,
        persisted_cache_policy,
        store,
        journal,
        &locked,
    )
    .await?
    {
        drop(result);
        return Ok(());
    }
    let mut operation = match record {
        PersistedBuildOperation::Supervised(operation) => operation,
        PersistedBuildOperation::Pending(pending) => {
            SupervisedBuildOperation::from_pending(&pending, identity, plan_digest, cache_policy)?
        }
        PersistedBuildOperation::Succeeded(_) => unreachable!(),
    };
    operation.require_identity(identity, plan_digest, cache_policy)?;
    fence_run_process(operation.run_process, identity.operation_id()).await?;
    journal.cleanup_workspace(identity.operation_id()).await?;
    journal
        .cleanup_cache_export(identity.operation_id())
        .await?;
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
    if let Some(result) = adopt_committed_output(
        identity,
        plan_digest,
        operation.cache_policy(),
        store,
        journal,
        locked,
    )
    .await?
    {
        return Ok(RecordedBuildStatus::Succeeded(Box::new(result)));
    }
    fence_run_process(operation.run_process, identity.operation_id()).await?;
    journal.cleanup_workspace(identity.operation_id()).await?;
    journal
        .cleanup_cache_export(identity.operation_id())
        .await?;
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
    cache_policy: Option<BuildCachePolicy>,
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
    let cache =
        inspect_committed_cache(identity, plan_digest, cache_policy, &output, journal).await?;
    let receipt = match cache_policy {
        Some(cache_policy) => BuildOutputReceipt::from_result(
            identity,
            plan_digest.to_string(),
            &output,
            cache_policy,
            cache.as_ref(),
        )?,
        None => BuildOutputReceipt::from_legacy_result(identity, plan_digest.to_string(), &output)?,
    };
    let receipt = locked.write_succeeded(receipt).await?;
    Ok(Some(RecordedBuildResult {
        receipt,
        output,
        cache,
        replayed: true,
    }))
}

async fn inspect_committed_cache(
    identity: &BuildOperationIdentity,
    plan_digest: &str,
    cache_policy: Option<BuildCachePolicy>,
    output: &BuildResult,
    journal: &BuildOperationJournal,
) -> Result<Option<RecordedBuildCache>, BuildPlanExecutionError> {
    let path = journal.cache_export_path(identity.operation_id());
    let cache_exists = match tokio::fs::symlink_metadata(&path).await {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(BuildReceiptError::StoreIo {
                message: format!(
                    "failed to inspect cache export for operation {}",
                    identity.operation_id()
                ),
                source: error,
            }
            .into())
        }
    };
    match (cache_policy, cache_exists) {
        (None, true) => {
            journal
                .cleanup_cache_export(identity.operation_id())
                .await?;
            return Ok(None);
        }
        (None | Some(BuildCachePolicy::Disabled), false) => return Ok(None),
        (Some(BuildCachePolicy::Disabled), true) => {
            return Err(BuildReceiptError::CacheInvalid {
                operation_id: identity.operation_id().to_string(),
                message: "cache export exists for a cache-disabled operation".to_string(),
            }
            .into())
        }
        (Some(BuildCachePolicy::ContentAddressed), false) => {
            return Err(BuildReceiptError::CacheInvalid {
                operation_id: identity.operation_id().to_string(),
                message: "content-addressed operation committed an image without its cache export"
                    .to_string(),
            }
            .into())
        }
        (Some(BuildCachePolicy::ContentAddressed), true) => {}
    }
    let cache_identity = BuildCacheExportIdentity::new(
        identity.source_digest(),
        plan_digest,
        output.platform.clone(),
    )?;
    let operation = identity.operation_id().to_string();
    tokio::task::spawn_blocking(move || inspect_build_cache_artifact(&path, &cache_identity, None))
        .await
        .map_err(|error| BuildReceiptError::Task {
            operation_id: operation.clone(),
            message: format!("cache export validation task failed: {error}"),
        })?
        .map(Some)
        .map_err(|error| {
            BuildReceiptError::CacheInvalid {
                operation_id: operation,
                message: error.to_string(),
            }
            .into()
        })
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
    cache_policy: BuildCachePolicy,
    receipt: BuildOutputReceipt,
    store: &ImageStore,
    journal: &BuildOperationJournal,
) -> Result<RecordedBuildResult, BuildPlanExecutionError> {
    receipt.require_identity(identity, plan_digest, cache_policy)?;
    let output = receipt.resolve(store).await?;
    let cache = receipt
        .resolve_cache(&journal.cache_export_path(identity.operation_id()))
        .await?;
    Ok(RecordedBuildResult {
        receipt,
        output,
        cache,
        replayed: true,
    })
}

#[cfg(test)]
mod tests;
