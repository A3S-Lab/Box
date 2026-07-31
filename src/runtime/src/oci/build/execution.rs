//! Plan-bound execution over the one native Box OCI build engine.

use std::path::Path;
use std::sync::Arc;

use a3s_box_core::error::BoxError;
use thiserror::Error;

use super::receipt::{
    inspect_stored_output, BuildOperationJournal, PendingBuildOperation, PersistedBuildOperation,
};
use super::{
    build, BoxBuildOptions, BoxBuildPlan, BoxBuildPlanError, BuildOperationIdentity,
    BuildOutputReceipt, BuildReceiptError, BuildResult, RecordedBuildResult,
};
use crate::oci::ImageStore;

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
}

/// Compile and execute one canonical plan through Box's existing native engine.
///
/// The returned layout path points at the durable image-store copy rather than
/// the temporary build workspace, so a caller can capture or publish the exact
/// OCI graph after this future returns.
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

/// Execute or exactly replay one terminal plan-bound native build.
///
/// The per-operation lock serializes the complete build and receipt commit
/// across processes. A successful receipt is written durably before this
/// future returns. A retry validates the same source and plan identities, then
/// reconstructs the exact output from the existing ImageStore without touching
/// the source tree.
///
/// This terminal receipt boundary does not supervise or cancel an in-flight
/// build. Those controls require the separate durable operation supervisor.
pub async fn execute_recorded_build_plan(
    identity: &BuildOperationIdentity,
    plan: &BoxBuildPlan,
    source_root: &Path,
    quiet: bool,
    store: Arc<ImageStore>,
) -> Result<RecordedBuildResult, BuildPlanExecutionError> {
    let plan_digest = plan.canonical_digest()?;
    let journal = BuildOperationJournal::for_image_store(&store, identity.operation_id()).await?;
    let locked = journal.lock(identity.operation_id()).await?;
    match locked.read().await? {
        Some(PersistedBuildOperation::Succeeded(receipt)) => {
            return recover_recorded_build(identity, &plan_digest, receipt, &store).await;
        }
        Some(PersistedBuildOperation::Pending(pending)) => {
            pending.require_identity(identity, &plan_digest)?;
            if let Some(output) =
                inspect_stored_output(identity.operation_id(), identity.output_reference(), &store)
                    .await?
            {
                let receipt =
                    BuildOutputReceipt::from_result(identity, plan_digest.clone(), &output)?;
                let receipt = locked.write_succeeded(receipt).await?;
                return Ok(RecordedBuildResult {
                    receipt,
                    output,
                    replayed: true,
                });
            }
        }
        None => {
            if store
                .get_checked(identity.output_reference())
                .await?
                .is_some()
            {
                return Err(BuildReceiptError::OutputInvalid {
                    operation_id: identity.operation_id().to_string(),
                    message: "operation output exists without a persisted owning intent"
                        .to_string(),
                }
                .into());
            }
            let pending = PendingBuildOperation::new(identity, plan_digest.clone())?;
            locked.write_pending(pending).await?;
        }
    }

    let planned = execute_build_plan(
        plan,
        source_root,
        BoxBuildOptions {
            tag: Some(identity.output_reference().to_string()),
            quiet,
        },
        Arc::clone(&store),
    )
    .await?;
    let receipt = BuildOutputReceipt::from_result(identity, planned.plan_digest, &planned.output)?;
    let receipt = locked.write_succeeded(receipt).await?;
    Ok(RecordedBuildResult {
        receipt,
        output: planned.output,
        replayed: false,
    })
}

/// Inspect and revalidate one terminal receipt without accessing build source.
pub async fn inspect_recorded_build_plan(
    identity: &BuildOperationIdentity,
    plan: &BoxBuildPlan,
    store: &ImageStore,
) -> Result<Option<RecordedBuildResult>, BuildPlanExecutionError> {
    let plan_digest = plan.canonical_digest()?;
    let journal = BuildOperationJournal::for_image_store(store, identity.operation_id()).await?;
    let locked = journal.lock(identity.operation_id()).await?;
    match locked.read().await? {
        None => Ok(None),
        Some(PersistedBuildOperation::Succeeded(receipt)) => {
            recover_recorded_build(identity, &plan_digest, receipt, store)
                .await
                .map(Some)
        }
        Some(PersistedBuildOperation::Pending(pending)) => {
            pending.require_identity(identity, &plan_digest)?;
            let Some(output) =
                inspect_stored_output(identity.operation_id(), identity.output_reference(), store)
                    .await?
            else {
                return Ok(None);
            };
            let receipt = BuildOutputReceipt::from_result(identity, plan_digest, &output)?;
            let receipt = locked.write_succeeded(receipt).await?;
            Ok(Some(RecordedBuildResult {
                receipt,
                output,
                replayed: true,
            }))
        }
    }
}

/// Remove one terminal receipt and its operation-specific ImageStore reference.
///
/// Missing image content is tolerated during cleanup so an already-pruned
/// output cannot leave a permanent stale receipt. A present reference must
/// still match the receipt digest before it is removed.
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
    locked.delete().await?;
    Ok(true)
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

#[cfg(test)]
mod tests;
