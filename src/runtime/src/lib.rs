//! A3S Box Runtime - MicroVM runtime implementation.
//!
//! This module provides the actual runtime implementation for A3S Box,
//! including VM management, OCI image handling, rootfs building, and gRPC health checks.
//!
//! # Feature Flags
//!
//! - `pool` — Warm VM pool with autoscaling (enabled by default)
//! - `scale` — Multi-node scale manager and instance registry (enabled by default)
//! - `compose` — Multi-container compose orchestration (enabled by default)
//! - `operator` — Kubernetes CRD autoscaler controller (enabled by default)
//! - `build` — Dockerfile/Containerfile build engine (enabled by default)

#![allow(clippy::result_large_err)]

// -- Core modules (always compiled) --
#[cfg(all(feature = "vm", target_os = "linux"))]
pub mod a3s_runtime_driver;
pub mod audit;
pub mod box_record;
pub mod box_state;
pub mod cache;
pub(crate) mod file_lock;
pub mod fs;
pub mod grpc;
pub mod host_check;
pub mod local_execution;
pub mod log;
pub mod managed_execution_store;
pub mod network;
pub mod oci;
pub mod process;
mod process_path;
pub mod prom;
pub mod resize;
mod resolved_image;
pub mod rootfs;
pub mod sandbox;
pub mod snapshot;
mod store_io;
#[cfg(unix)]
pub mod tee;
#[cfg(feature = "vm")]
pub mod vm;
#[cfg(feature = "vm")]
pub mod vmm;
pub mod volume;

// -- Optional modules (feature-gated) --
#[cfg(feature = "compose")]
pub mod compose;
#[cfg(feature = "operator")]
pub mod operator;
#[cfg(feature = "pool")]
pub mod pool;
#[cfg(feature = "scale")]
pub mod scale;

// ── Core re-exports (used by CLI, CRI, SDK, shim) ──

// Audit
pub use audit::{read_audit_log, AuditLog, AuditQuery};

#[cfg(all(feature = "vm", target_os = "linux"))]
pub use a3s_runtime_driver::{
    BoxArtifactPort, BoxArtifactPortError, BoxRegistryCredential, BoxRuntimeDriver,
    BoxRuntimeDriverConfig, BoxRuntimeSevSnpConfig, BoxSecretMaterial,
    BoxSecretMaterializationError, BoxSecretMaterializer,
};

// Canonical local execution metadata
pub use a3s_oci_sdk::{IO_READ_BYTES_METRIC, IO_WRITE_BYTES_METRIC};
pub use box_record::{
    BoxRecord, HealthCheck, ManagedExecutionMetadata, ManagedExecutionOperation,
    ManagedExecutionState, ManagedResourceUpdateCompletion, ManagedRestartCompletion,
    ManagedRestartOutcome, ManagedRuntimeRoute,
};
pub use box_state::BoxStateStore;
pub use local_execution::{
    acquire_execution_lifecycle_lock, ExecutionLifecycleLock, LocalExecutionBackend,
    LocalExecutionBackendRouter, LocalExecutionHandle, LocalExecutionManager,
    LocalExecutionObservation, LocalExecutionTermination, OciBundlePreparationContext,
    OciBundleProvider, OciLifecycleAdapter, OciLocalExecutionBackend, OciMigrationPolicy,
    OciPreparedExecution, OciRuntimeBinding, OciRuntimeEndpoint, OciRuntimeLaunch,
    OCI_RUNTIME_BINDING_SCHEMA_VERSION,
};
#[cfg(feature = "vm")]
pub use local_execution::{
    NativeLinuxOciBundleProvider, NativeLinuxOciMigrationConfig, VmLocalExecutionBackend,
    WindowsWhpxOciBundleProvider, WindowsWhpxOciMigrationConfig,
};
pub use managed_execution_store::{
    ManagedExecutionReservation, ManagedExecutionStore, ManagedExecutionStoreError,
    ManagedExecutionStoreResult,
};
pub use process::{is_process_alive, is_process_alive_with_identity, pid_start_time};

// gRPC clients
#[cfg(unix)]
pub use grpc::{
    AttestationClient, PtyClient, RaTlsAttestationClient, StreamingPty, StreamingPtyInput,
};
pub use grpc::{ExecClient, StreamingExec, StreamingExecInput};
#[cfg(unix)]
pub use grpc::{SealClient, SecretEntry, SecretInjector};

// Host checks
pub use host_check::check_virtualization_support;

// Network
pub use network::NetworkStore;

// OCI images
pub use a3s_box_core::{ExecutionIsolation, StoredImage};
pub use oci::{
    prune_stale_pull_temp_dirs, ImagePuller, ImageReference, ImageStore, PullProgress,
    PullProgressEventFn, PullProgressState, PullTempPruneResult, RegistryAuth, RegistryPullPolicy,
};
pub use oci::{CredentialStore, PushResult, RegistryProtocol, RegistryPusher};
pub use oci::{OciImage, SignResult, SignaturePolicy};

// Metrics
pub use prom::RuntimeMetrics;

// Snapshot
pub use resolved_image::{load_resolved_image_config, RESOLVED_IMAGE_CONFIG_FILE};
pub use snapshot::SnapshotStore;

// TEE
#[cfg(unix)]
pub use tee::{seal, unseal};
#[cfg(unix)]
pub use tee::{
    verify_attestation, verify_attestation_with_time, AmdKdsClient, AttestationPolicy,
    MinTcbPolicy, PolicyResult, VerificationResult,
};
#[cfg(unix)]
pub use tee::{AttestationReport, AttestationRequest, PlatformInfo};

// VM
#[cfg(feature = "vm")]
pub use vm::{BoxState, PullProgressFn, VmManager};
#[cfg(feature = "vm")]
pub use vmm::{
    Entrypoint, FsMount, InstanceSpec, NetworkInstanceConfig, ShimHandler, TeeInstanceConfig,
    VmController, VmHandler, VmMetrics, VmmProvider,
};

// Resize
pub use resize::{validate_update, ResizeResult, ResourceUpdate};

// Volume
pub use volume::VolumeStore;

// ── Feature-gated re-exports ──

#[cfg(feature = "build")]
pub use oci::{
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

#[cfg(feature = "compose")]
#[allow(deprecated)]
pub use compose::{ComposeProject, ComposeRuntimePlan, HealthCheckSpec};

#[cfg(feature = "operator")]
pub use operator::AutoscalerController;

#[cfg(feature = "pool")]
pub use pool::WarmPool;

#[cfg(feature = "scale")]
pub use scale::{
    serve_scale_api, DurableScaleAuthority, LocalScaleReconciler, ScaleApiState,
    ScaleAuthorityError, ScaleCatalogError, ScaleManager, ScaleReconcileError,
    ScaleReconcileReport, ScaleServiceCatalog, SharedScaleAuthority,
};

// ── Constants ──

/// A3S Box Runtime version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub use a3s_box_core::{ATTEST_VSOCK_PORT, EXEC_VSOCK_PORT, PORT_FWD_VSOCK_PORT, PTY_VSOCK_PORT};

/// Default maximum image cache size: 10 GB.
pub const DEFAULT_IMAGE_CACHE_SIZE: u64 = 10 * 1024 * 1024 * 1024;
