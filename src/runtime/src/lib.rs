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
pub mod log;
pub mod metrics;
pub mod network;
pub mod oci;
pub mod pool;
pub mod rootfs;
pub mod tee;
pub mod vm;
pub mod vmm;
pub mod volume;

// Re-export common types
pub use cache::{LayerCache, RootfsCache};
pub use grpc::{AgentClient, AttestationClient, ExecClient, PtyClient};
pub use host_check::{check_virtualization_support, VirtualizationSupport};
pub use network::NetworkStore;
pub use network::PasstManager;
pub use oci::{BuildConfig, BuildResult, Dockerfile, Instruction};
pub use oci::{CredentialStore, PushResult, RegistryPusher};
pub use oci::{ImagePuller, ImageReference, ImageStore, RegistryAuth, RegistryPuller, StoredImage};
pub use oci::{OciImage, OciImageConfig, OciRootfsBuilder, RootfsComposition};
pub use pool::{PoolStats, WarmPool};
pub use rootfs::{find_agent_binary, GuestLayout, RootfsBuilder, GUEST_AGENT_PATH, GUEST_WORKDIR};
pub use tee::{check_sev_snp_support, require_sev_snp_support, SevSnpSupport};
pub use tee::{
    verify_attestation, AmdKdsClient, AttestationPolicy, MinTcbPolicy, PolicyResult,
    VerificationResult,
};
pub use tee::{AttestationReport, AttestationRequest, CertificateChain, PlatformInfo, TcbVersion};
pub use vm::{BoxState, VmManager};
pub use vmm::{
    Entrypoint, FsMount, InstanceSpec, NetworkInstanceConfig, ShimHandler, TeeInstanceConfig,
    VmController, VmHandler, VmMetrics,
};
pub use volume::VolumeStore;

/// A3S Box Runtime version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default vsock port for communication with Guest Agent.
pub const AGENT_VSOCK_PORT: u32 = 4088;

/// Default vsock port for exec server in the guest.
pub const EXEC_VSOCK_PORT: u32 = 4089;

/// Default vsock port for PTY server in the guest.
pub const PTY_VSOCK_PORT: u32 = 4090;

/// Default vsock port for TEE attestation server in the guest.
pub const ATTEST_VSOCK_PORT: u32 = 4091;

/// Default maximum image cache size: 10 GB.
pub const DEFAULT_IMAGE_CACHE_SIZE: u64 = 10 * 1024 * 1024 * 1024;
