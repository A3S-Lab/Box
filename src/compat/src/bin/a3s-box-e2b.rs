use std::path::PathBuf;

use a3s_box_compat::production::{E2bCompatConfig, E2bCompatService};
use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[cfg(windows)]
const WINDOWS_SERVICE_WORKER_STACK_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Parser)]
#[command(
    name = "a3s-box-e2b",
    version,
    about = "ACL-configured E2B compatibility service for A3S Box"
)]
struct Arguments {
    /// Production ACL configuration file.
    #[arg(long, value_name = "PATH")]
    config: PathBuf,
}

fn main() {
    if let Err(error) = build_runtime().and_then(|runtime| runtime.block_on(run())) {
        eprintln!("a3s-box-e2b failed: {error:#}");
        std::process::exit(1);
    }
}

fn build_runtime() -> Result<tokio::runtime::Runtime> {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.thread_name("a3s-box-e2b-worker");
    #[cfg(windows)]
    builder
        // Debug builds on Windows need more than Tokio's default worker stack
        // while the OCI pull and rootfs preparation pipeline is active. Keep
        // the same explicit worker reservation used by the runtime
        // conformance harness so `cargo run` exercises the production path.
        .thread_stack_size(WINDOWS_SERVICE_WORKER_STACK_BYTES);
    builder
        .enable_all()
        .build()
        .context("build E2B compatibility runtime")
}

async fn run() -> Result<()> {
    let arguments = Arguments::parse();
    initialize_tracing()?;
    let config = E2bCompatConfig::load(&arguments.config)
        .await
        .with_context(|| format!("load {}", arguments.config.display()))?;
    E2bCompatService::build(config)
        .await
        .context("compose production service")?
        .serve()
        .await
        .context("run production service")
}

fn initialize_tracing() -> Result<()> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("a3s_box_compat=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .try_init()
        .map_err(|error| anyhow::anyhow!("initialize tracing: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_runtime_uses_named_workers() {
        let runtime = build_runtime().expect("service runtime");
        let name = runtime
            .block_on(async {
                tokio::spawn(async { std::thread::current().name().map(str::to_string) }).await
            })
            .expect("worker task");
        assert_eq!(name.as_deref(), Some("a3s-box-e2b-worker"));
        #[cfg(windows)]
        assert_eq!(WINDOWS_SERVICE_WORKER_STACK_BYTES, 8 * 1024 * 1024);
    }
}
