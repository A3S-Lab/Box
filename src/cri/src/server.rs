//! gRPC server setup for CRI services.
//!
//! Listens on a Unix domain socket for CRI RuntimeService and ImageService RPCs.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;

use a3s_box_runtime::oci::{ImageStore, RegistryAuth};

use crate::cri_api::image_service_server::ImageServiceServer;
use crate::cri_api::runtime_service_server::RuntimeServiceServer;
use crate::image_service::BoxImageService;
use crate::runtime_service::BoxRuntimeService;

/// CRI gRPC server configuration.
pub struct CriServer {
    /// Path to the Unix domain socket.
    socket_path: PathBuf,
    /// Shared image store.
    image_store: Arc<ImageStore>,
    /// Registry authentication.
    auth: RegistryAuth,
}

impl CriServer {
    /// Create a new CRI server.
    pub fn new(
        socket_path: PathBuf,
        image_store: Arc<ImageStore>,
        auth: RegistryAuth,
    ) -> Self {
        Self {
            socket_path,
            image_store,
            auth,
        }
    }

    /// Start serving CRI RPCs on the Unix socket.
    pub async fn serve(&self) -> Result<(), Box<dyn std::error::Error>> {
        // Remove existing socket file if present
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)?;
        }

        // Ensure parent directory exists
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let runtime_service = BoxRuntimeService::new(
            self.image_store.clone(),
            self.auth.clone(),
        );
        let image_service = BoxImageService::new(
            self.image_store.clone(),
            self.auth.clone(),
        );

        let uds = UnixListener::bind(&self.socket_path)?;
        let uds_stream = UnixListenerStream::new(uds);

        tracing::info!(
            socket = %self.socket_path.display(),
            "CRI server listening"
        );

        Server::builder()
            .add_service(RuntimeServiceServer::new(runtime_service))
            .add_service(ImageServiceServer::new(image_service))
            .serve_with_incoming(uds_stream)
            .await?;

        Ok(())
    }
}
