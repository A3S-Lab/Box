//! A3S Box Core - Foundational Types and Abstractions
//!
//! This module provides the foundational types, traits, and abstractions
//! used across the A3S Box ecosystem.
//!
//! ## Related Crates
//!
//! - **a3s-lane**: Priority-based command queue system (extracted)
//! - **a3s-context**: Hierarchical context management (standalone)
//! - **a3s-code**: AI coding agent (standalone)

pub mod config;
pub mod context;
pub mod error;
pub mod event;

// Re-export commonly used types
pub use config::{BoxConfig, LaneConfig, ModelConfig, ResourceConfig};
pub use context::{
    ContextDepth, ContextItem, ContextProvider, ContextQuery, ContextResult, ContextType,
};
pub use error::{BoxError, Result};
pub use event::{BoxEvent, EventEmitter};

/// A3S Box version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
