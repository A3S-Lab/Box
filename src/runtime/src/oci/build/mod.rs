//! OCI image build support.
//!
//! Provides Dockerfile/Containerfile parsing, layer creation, and a build engine
//! that produces OCI images from Dockerfile-compatible build files.
//!
//! # Usage
//!
//! ```text
//! a3s-box build -t myimage:latest .
//! ```
//!
//! # Supported Instructions
//!
//! FROM, shell/exec-form RUN, shell-form COPY/ADD, WORKDIR, ENV, ENTRYPOINT, CMD,
//! EXPOSE, LABEL, USER, ARG, SHELL, STOPSIGNAL, HEALTHCHECK, ONBUILD metadata
//! triggers, VOLUME.
//!
//! Unsupported Dockerfile flags and instructions fail with contextual errors
//! instead of being silently ignored.

mod assembly;
pub(crate) mod cache;
pub mod dockerfile;
pub(crate) mod dockerignore;
pub mod engine;
mod execution;
pub mod layer;
mod layout;
mod output;
pub mod plan;
mod receipt;

pub use assembly::{
    assemble_recorded_build_outputs, BuildAssemblyError, BuildOutputAssembly,
    BuildOutputAssemblyInput,
};
pub use cache::{
    hydrate_recorded_build_cache, BuildCacheReceipt, RecordedBuildCache,
    BUILD_CACHE_ARTIFACT_MEDIA_TYPE, BUILD_CACHE_CONFIG_MEDIA_TYPE,
};
pub use dockerfile::{Dockerfile, Instruction};
pub use engine::{build, BuildConfig, BuildNetworkPolicy, BuildRunPoolConfig};
pub use execution::{
    cancel_recorded_build_plan, execute_recorded_build_plan, inspect_recorded_build_plan,
    inspect_recorded_build_status, remove_recorded_build_plan, start_recorded_build_plan,
    BuildPlanExecutionError,
};
pub use layer::{DirSnapshot, LayerInfo};
pub use output::{
    BuildOutputDescriptor, BuildResult, MultiPlatformBuildResult, OCI_IMAGE_INDEX_MEDIA_TYPE,
    OCI_IMAGE_MANIFEST_MEDIA_TYPE,
};
pub use plan::{BoxBuildOptions, BoxBuildPlan, BoxBuildPlanError, BuildCachePolicy};
pub use receipt::{
    BuildCancellationOutcome, BuildOperationIdentity, BuildOutputReceipt, BuildReceiptError,
    BuildReceiptOutput, RecordedBuildResult, RecordedBuildStatus,
};
