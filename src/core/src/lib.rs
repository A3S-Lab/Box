//! A3S Box Core - Foundational Types and Abstractions
//!
//! This module provides the foundational types, traits, and abstractions
//! used across the A3S Box MicroVM runtime.

pub mod config;
pub mod error;
pub mod event;

// Re-export commonly used types
pub use config::{BoxConfig, ResourceConfig};
pub use error::{BoxError, Result};
pub use event::{BoxEvent, EventEmitter};

/// A3S Box version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
