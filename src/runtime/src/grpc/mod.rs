//! Host-guest communication clients over Unix socket.
//!
//! - `ExecClient`: Executing commands in the guest (port 4089).
//! - `PtyClient`: Interactive terminal access (port 4090).
//! - `AttestationClient`: TEE attestation and secret injection (port 4091).

mod attestation;
mod exec;
mod pty;

pub use attestation::{
    AttestationClient, RaTlsAttestationClient, SealClient, SealResult, SecretEntry,
    SecretInjectionResult, SecretInjector, UnsealResult,
};
pub use exec::{ExecClient, StreamingExec};
pub use pty::PtyClient;
