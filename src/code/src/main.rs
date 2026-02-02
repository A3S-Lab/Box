//! A3S Box Code Agent
//!
//! Rust implementation of the coding agent that runs inside the guest VM.
//! Provides gRPC service for host-guest communication.
//!
//! ## Architecture
//!
//! ```text
//! Host (SDK) --gRPC-over-vsock--> Guest Agent
//!                                    |
//!                                    +-- Session Manager
//!                                    |      +-- Session 1
//!                                    |      +-- Session 2
//!                                    |      +-- ...
//!                                    |
//!                                    +-- Agent Loop
//!                                    |      +-- LLM Client
//!                                    |      +-- Tool Executor
//!                                    |
//!                                    +-- Tools
//!                                           +-- bash
//!                                           +-- read/write/edit
//!                                           +-- grep/glob/ls
//! ```

mod agent;
mod llm;
mod service;
mod session;
mod tools;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    tracing::info!("Starting A3S Box Code Agent");
    tracing::info!("Version: {}", env!("CARGO_PKG_VERSION"));

    // Start gRPC service
    service::start_server().await?;

    Ok(())
}
