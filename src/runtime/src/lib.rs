//! A3S Box Runtime - MicroVM runtime implementation.
//!
//! This module provides the actual runtime implementation for A3S Box,
//! including VM management, session handling, skill execution, and gRPC communication.

#![allow(clippy::result_large_err)]

pub mod fs;
pub mod grpc;
pub mod host_check;
pub mod krun;
pub mod metrics;
pub mod oci;
pub mod rootfs;
pub mod tee;
pub mod vm;
pub mod vmm;

// Re-export common types
pub use host_check::{check_virtualization_support, VirtualizationSupport};
pub use oci::{OciImage, OciImageConfig, OciRootfsBuilder, RootfsComposition};
pub use oci::{ImagePuller, ImageReference, ImageStore, RegistryAuth, RegistryPuller, StoredImage};
pub use rootfs::{find_agent_binary, GuestLayout, RootfsBuilder, GUEST_AGENT_PATH, GUEST_WORKDIR};
pub use tee::{check_sev_snp_support, require_sev_snp_support, SevSnpSupport};
pub use grpc::AgentClient;
pub use vm::{BoxState, VmManager};
pub use vmm::{Entrypoint, FsMount, InstanceSpec, ShimHandler, TeeInstanceConfig, VmController, VmHandler, VmMetrics};

/// A3S Box Runtime version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default vsock port for communication with Guest Agent.
pub const AGENT_VSOCK_PORT: u32 = 4088;
