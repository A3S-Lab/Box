//! A3S Box Core - Foundational Types and Abstractions
//!
//! This module provides the foundational types, traits, and abstractions
//! used across the A3S Box MicroVM runtime.

pub mod config;
pub mod dns;
pub mod error;
pub mod event;
pub mod exec;
pub mod network;

// Re-export commonly used types
pub use config::{BoxConfig, ResourceConfig};
pub use error::{BoxError, Result};
pub use event::{BoxEvent, EventEmitter};
pub use exec::{ExecOutput, ExecRequest};
pub use network::{NetworkConfig, NetworkEndpoint, NetworkMode};

/// A3S Box version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
