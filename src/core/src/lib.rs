//! A3S Box Core - Foundational Types and Abstractions
//!
//! This module provides the foundational types, traits, and abstractions
//! used across the A3S Box ecosystem.

pub mod config;
pub mod error;
pub mod event;
pub mod queue;

// Re-export commonly used types
pub use config::{BoxConfig, LaneConfig, ModelConfig, ResourceConfig};
pub use error::{BoxError, Result};
pub use event::{BoxEvent, EventEmitter};
pub use queue::{CommandQueue, Lane, LaneId};

/// A3S Box version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
