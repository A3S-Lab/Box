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

pub(crate) mod cache;
pub mod dockerfile;
pub(crate) mod dockerignore;
pub mod engine;
mod execution;
pub mod layer;
pub mod plan;

pub use dockerfile::{Dockerfile, Instruction};
pub use engine::{
    build, BuildConfig, BuildNetworkPolicy, BuildOutputDescriptor, BuildResult, BuildRunPoolConfig,
    OCI_IMAGE_MANIFEST_MEDIA_TYPE,
};
pub use execution::{execute_build_plan, BuildPlanExecutionError, PlannedBuildResult};
pub use layer::{DirSnapshot, LayerInfo};
pub use plan::{BoxBuildOptions, BoxBuildPlan, BoxBuildPlanError, BuildCachePolicy};
