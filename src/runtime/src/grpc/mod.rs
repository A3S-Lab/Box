//! Host-guest communication clients over Unix socket.
//!
//! - `AgentClient`: Health-checking the guest agent (port 4088).
//! - `ExecClient`: Executing commands in the guest (port 4089).
//!
//! Agent-level operations (sessions, generation, skills) are handled
//! by the a3s-code crate, not the Box runtime.

mod agent;
mod attestation;
mod exec;
mod pty;

pub use agent::AgentClient;
pub use attestation::{
    AttestationClient, RaTlsAttestationClient, SealClient, SealResult, SecretEntry,
    SecretInjectionResult, SecretInjector, UnsealResult,
};
pub use exec::{ExecClient, StreamingExec};
pub use pty::PtyClient;
