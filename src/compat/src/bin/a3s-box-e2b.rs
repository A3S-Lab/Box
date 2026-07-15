use std::path::PathBuf;

use a3s_box_compat::production::{E2bCompatConfig, E2bCompatService};
use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::EnvFilter;

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

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("a3s-box-e2b failed: {error:#}");
        std::process::exit(1);
    }
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
        .context("initialize tracing")
}
