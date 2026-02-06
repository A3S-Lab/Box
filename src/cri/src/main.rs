//! A3S Box CRI - Kubernetes Container Runtime Interface binary.
//!
//! Serves CRI RuntimeService and ImageService over a Unix domain socket,
//! allowing kubelet to schedule pods onto A3S Box microVMs.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use a3s_box_runtime::oci::{ImageStore, RegistryAuth};

use a3s_box_cri::server::CriServer;

/// A3S Box CRI Runtime
#[derive(Parser, Debug)]
#[command(name = "a3s-box-cri", about = "A3S Box CRI Runtime")]
struct Args {
    /// Path to the Unix domain socket for CRI communication.
    #[arg(long, default_value = "/var/run/a3s-box/a3s-box.sock")]
    socket: PathBuf,

    /// Directory for storing pulled OCI images.
    #[arg(long, default_value = "~/.a3s/images")]
    image_dir: String,

    /// Maximum image cache size in bytes (default: 10GB).
    #[arg(long, default_value = "10737418240")]
    image_cache_size: u64,
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

    // Resolve image directory (expand ~)
    let image_dir = if args.image_dir.starts_with('~') {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        home.join(args.image_dir.strip_prefix("~/").unwrap_or(&args.image_dir))
    } else {
        PathBuf::from(&args.image_dir)
    };

    tracing::info!(
        socket = %args.socket.display(),
        image_dir = %image_dir.display(),
        cache_size = args.image_cache_size,
        "Starting A3S Box CRI Runtime"
    );

    // Initialize image store
    let image_store = Arc::new(
        ImageStore::new(&image_dir, args.image_cache_size)
            .map_err(|e| format!("Failed to initialize image store: {}", e))?,
    );

    // Use environment-based auth
    let auth = RegistryAuth::from_env();

    // Create and start CRI server
    let server = CriServer::new(args.socket, image_store, auth);
    server.serve().await?;

    Ok(())
}
