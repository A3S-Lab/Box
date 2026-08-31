//! OCI image support for A3S Box.
//!
//! This module provides functionality to parse and extract OCI images
//! for use as VM rootfs. It supports:
//!
//! - OCI image layout parsing (manifest, config)
//! - Layer extraction (tar.gz)
//! - Rootfs composition from multiple images
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    OCI Image Layout                          │
//! │                                                              │
//! │  image/                                                      │
//! │  ├── oci-layout           (OCI layout marker)               │
//! │  ├── index.json           (Image index)                     │
//! │  └── blobs/                                                 │
//! │      └── sha256/                                            │
//! │          ├── <manifest>   (Image manifest)                  │
//! │          ├── <config>     (Image configuration)             │
//! │          └── <layers>     (Filesystem layers)               │
//! └─────────────────────────────────────────────────────────────┘
//! ```

#[cfg(feature = "build")]
pub mod build;
pub mod credentials;
mod image;
pub(crate) mod layer_reader;
mod layers;
pub(crate) mod limited_reader;
mod pull;
pub mod reference;
pub mod registry;
pub(crate) mod rootfs;
pub mod signing;
pub mod store;

#[cfg(feature = "build")]
pub use build::{
    assemble_recorded_build_outputs, cancel_recorded_build_plan, execute_recorded_build_plan,
    hydrate_recorded_build_cache, inspect_recorded_build_plan, inspect_recorded_build_status,
    remove_recorded_build_plan, start_recorded_build_plan, BoxBuildOptions, BoxBuildPlan,
    BoxBuildPlanError, BuildAssemblyError, BuildCachePolicy, BuildCacheReceipt,
    BuildCancellationOutcome, BuildConfig, BuildNetworkPolicy, BuildOperationIdentity,
    BuildOutputAssembly, BuildOutputAssemblyInput, BuildOutputDescriptor, BuildOutputReceipt,
    BuildPlanExecutionError, BuildReceiptError, BuildReceiptOutput, BuildResult,
    BuildRunPoolConfig, Dockerfile, Instruction, MultiPlatformBuildResult, RecordedBuildCache,
    RecordedBuildResult, RecordedBuildStatus, BUILD_CACHE_ARTIFACT_MEDIA_TYPE,
    BUILD_CACHE_CONFIG_MEDIA_TYPE, OCI_IMAGE_INDEX_MEDIA_TYPE, OCI_IMAGE_MANIFEST_MEDIA_TYPE,
};
pub use credentials::CredentialStore;
pub use image::{OciHealthCheck, OciImage, OciImageConfig};
pub use layers::extract_layer;
#[cfg(test)]
pub(crate) use layers::extract_layer_with_metadata;
pub use pull::{prune_stale_pull_temp_dirs, ImagePuller, PullTempPruneResult};
pub use reference::ImageReference;
pub use registry::{
    PullProgress, PullProgressEventFn, PullProgressState, PushResult, RegistryAuth,
    RegistryProtocol, RegistryPullPolicy, RegistryPusher,
};
pub use rootfs::OciRootfsBuilder;
pub use signing::{SignResult, SignaturePolicy, VerifyResult};
pub use store::ImageStore;
