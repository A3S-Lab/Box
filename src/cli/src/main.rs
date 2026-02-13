//! A3S Box CLI entry point.

use clap::Parser;
use tracing_subscriber::EnvFilter;

use a3s_box_cli::commands::{dispatch, Cli};

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();

    if let Err(e) = dispatch(cli).await {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
