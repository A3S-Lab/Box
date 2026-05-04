//! Platform abstraction layer for cross-platform support.
//!
//! This module provides trait-based backends that encapsulate platform-specific
//! operations, allowing a3s-box to run on macOS (HVF), Linux (KVM), and Windows (WHPX)
//! with a unified interface.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────┐
//! │    Platform-Agnostic Runtime            │
//! │  (Container lifecycle, OCI, etc.)       │
//! └─────────────────────────────────────────┘
//!               │
//! ┌─────────────────────────────────────────┐
//! │    Platform Abstraction Layer           │
//! │  (Traits defined in this module)        │
//! └─────────────────────────────────────────┘
//!               │
//!     ┌─────────┼─────────┐
//!     │         │         │
//! ┌───▼───┐ ┌──▼───┐ ┌──▼────┐
//! │ HVF   │ │ KVM  │ │ WHPX  │
//! │(macOS)│ │(Linux│ │(Win)  │
//! └───────┘ └──────┘ └───────┘
//! ```
//!
//! # Design Principles
//!
//! 1. **Trait-based backends**: All platform-specific code behind traits
//! 2. **Compile-time selection**: Use `#[cfg(target_os)]` for platform selection
//! 3. **Graceful degradation**: Features unavailable on a platform return clear errors
//! 4. **Unified testing**: Same test suite runs on all platforms

mod traits;

// Platform-specific backend implementations
#[cfg(target_os = "macos")]
mod hvf;
#[cfg(target_os = "linux")]
mod kvm;
#[cfg(target_os = "windows")]
mod whpx;

// Re-export traits
pub use traits::{ExecBackend, FsBackend, NetworkBackend, VmmBackend};

// Re-export platform-specific backends
#[cfg(target_os = "macos")]
pub use hvf::HvfBackend;
#[cfg(target_os = "linux")]
pub use kvm::KvmBackend;
#[cfg(target_os = "windows")]
pub use whpx::WhpxBackend;

/// Get the default VMM backend for the current platform.
pub fn default_vmm_backend() -> Box<dyn VmmBackend> {
    #[cfg(target_os = "macos")]
    {
        Box::new(HvfBackend::new())
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(KvmBackend::new())
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(WhpxBackend::new())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        compile_error!("Unsupported platform")
    }
}

/// Get the default network backend for the current platform.
pub fn default_network_backend() -> Box<dyn NetworkBackend> {
    #[cfg(target_os = "macos")]
    {
        Box::new(hvf::HvfNetworkBackend::new())
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(kvm::KvmNetworkBackend::new())
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(whpx::WhpxNetworkBackend::new())
    }
}

/// Get the default filesystem backend for the current platform.
pub fn default_fs_backend() -> Box<dyn FsBackend> {
    #[cfg(target_os = "macos")]
    {
        Box::new(hvf::HvfFsBackend::new())
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(kvm::KvmFsBackend::new())
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(whpx::WhpxFsBackend::new())
    }
}

/// Get the default exec backend for the current platform.
pub fn default_exec_backend() -> Box<dyn ExecBackend> {
    #[cfg(target_os = "macos")]
    {
        Box::new(hvf::HvfExecBackend::new())
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(kvm::KvmExecBackend::new())
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(whpx::WhpxExecBackend::new())
    }
}
