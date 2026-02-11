//! A3S Box Runtime - MicroVM runtime implementation.
//!
//! This module provides the actual runtime implementation for A3S Box,
//! including VM management, OCI image handling, rootfs building, and gRPC health checks.

#![allow(clippy::result_large_err)]

pub mod cache;
pub mod fs;
pub mod grpc;
pub mod host_check;
pub mod krun;
pub mod metrics;
pub mod network;
pub mod oci;
pub mod pool;
pub mod rootfs;
pub mod tee;
pub mod vm;
pub mod vmm;

// Re-export common types
pub use cache::{LayerCache, RootfsCache};
pub use host_check::{check_virtualization_support, VirtualizationSupport};
pub use oci::{OciImage, OciImageConfig, OciRootfsBuilder, RootfsComposition};
pub use oci::{ImagePuller, ImageReference, ImageStore, RegistryAuth, RegistryPuller, StoredImage};
pub use oci::{BuildConfig, BuildResult, Dockerfile, Instruction};
pub use pool::{PoolStats, WarmPool};
pub use rootfs::{find_agent_binary, GuestLayout, RootfsBuilder, GUEST_AGENT_PATH, GUEST_WORKDIR};
pub use tee::{check_sev_snp_support, require_sev_snp_support, SevSnpSupport};
pub use tee::{AttestationReport, AttestationRequest, CertificateChain, PlatformInfo, TcbVersion};
pub use tee::{verify_attestation, AttestationPolicy, AmdKdsClient, MinTcbPolicy, PolicyResult, VerificationResult};
pub use grpc::{AgentClient, AttestationClient, ExecClient};
pub use network::NetworkStore;
pub use vm::{BoxState, VmManager};
pub use vmm::{Entrypoint, FsMount, InstanceSpec, ShimHandler, TeeInstanceConfig, VmController, VmHandler, VmMetrics};

/// A3S Box Runtime version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default vsock port for communication with Guest Agent.
pub const AGENT_VSOCK_PORT: u32 = 4088;

/// Default vsock port for exec server in the guest.
pub const EXEC_VSOCK_PORT: u32 = 4089;

/// Default maximum image cache size: 10 GB.
pub const DEFAULT_IMAGE_CACHE_SIZE: u64 = 10 * 1024 * 1024 * 1024;
