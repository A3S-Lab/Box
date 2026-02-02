//! A3S Box Runtime - MicroVM Runtime Implementation
//!
//! This package provides the actual runtime implementation for A3S Box,
//! including VM management, session handling, skill execution, and gRPC communication.

pub mod fs;
pub mod grpc;
pub mod metrics;
pub mod session;
pub mod skill;
pub mod vm;

// Re-export commonly used types
pub use session::{Session, SessionId, SessionManager};
pub use skill::{Skill, SkillManager, SkillPackage};
pub use vm::{BoxState, VmManager};

/// A3S Box Runtime version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default vsock port for guest agent communication
pub const AGENT_VSOCK_PORT: u32 = 4088;
