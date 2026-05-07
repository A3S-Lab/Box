//! A3S Box Docker Engine API server binary.

use std::path::PathBuf;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use a3s_box_api::ApiServer;

/// A3S Box Docker Engine API
#[derive(Parser, Debug)]
#[command(name = "a3s-box-api", about = "A3S Box Docker Engine API Server")]
struct Args {
    /// Path to the Unix domain socket for Docker API communication.
    ///
    /// Default: /var/run/a3s-box/docker.sock
    ///
    /// For Docker compatibility, you can:
    /// 1. Set DOCKER_HOST environment variable:
    ///    export DOCKER_HOST=unix:///var/run/a3s-box/docker.sock
    ///
    /// 2. Create a symlink (requires root):
    ///    sudo ln -s /var/run/a3s-box/docker.sock /var/run/docker.sock
    ///
    /// 3. Use the standard Docker socket path directly:
    ///    --socket /var/run/docker.sock
    #[arg(long, default_value = "/var/run/a3s-box/docker.sock")]
    socket: PathBuf,

    /// Use the standard Docker socket path (/var/run/docker.sock).
    ///
    /// WARNING: This will conflict with Docker if it's running.
    /// Make sure to stop Docker first: sudo systemctl stop docker
    #[arg(long)]
    docker_compat: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    // Override socket path if docker_compat is enabled
    let socket_path = if args.docker_compat {
        PathBuf::from("/var/run/docker.sock")
    } else {
        args.socket
    };

    tracing::info!(
        socket = %socket_path.display(),
        docker_compat = args.docker_compat,
        "Starting A3S Box Docker Engine API Server"
    );

    // Warn if using standard Docker socket
    if socket_path == PathBuf::from("/var/run/docker.sock") {
        tracing::warn!(
            "Using standard Docker socket path. Make sure Docker is not running to avoid conflicts."
        );
    } else {
        tracing::info!(
            "To use Docker tools, set: export DOCKER_HOST=unix://{}",
            socket_path.display()
        );
    }

    // Create and start API server
    let server = ApiServer::new(socket_path);
    server.serve().await?;

    Ok(())
}
