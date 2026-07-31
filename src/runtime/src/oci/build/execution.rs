//! Plan-bound execution over the one native Box OCI build engine.

use std::path::Path;
use std::sync::Arc;

use a3s_box_core::error::BoxError;
use thiserror::Error;

use super::{build, BoxBuildOptions, BoxBuildPlan, BoxBuildPlanError, BuildResult};
use crate::oci::ImageStore;

/// Successful native output bound to the canonical admitted build plan.
#[derive(Debug)]
pub struct PlannedBuildResult {
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
}

/// Compile and execute one canonical plan through Box's existing native engine.
///
/// The returned layout path points at the durable image-store copy rather than
/// the temporary build workspace, so a caller can capture or publish the exact
/// OCI graph after this future returns.
pub async fn execute_build_plan(
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

#[cfg(test)]
mod tests;
