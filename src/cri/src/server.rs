//! gRPC server setup for CRI services.
//!
//! Listens on a Unix domain socket for CRI RuntimeService and ImageService RPCs.
//! Also starts an HTTP streaming server for exec/attach/port-forward.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{UnixListener, UnixStream};
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;

use a3s_box_runtime::oci::{ImageStore, RegistryAuth};

use crate::cri_api::image_service_server::ImageServiceServer;
use crate::cri_api::runtime_service_server::RuntimeServiceServer;
use crate::image_service::BoxImageService;
use crate::runtime_service::{BoxRuntimeService, CriRuntimeOptions};
use crate::streaming::StreamingServer;

/// CRI gRPC server configuration.
pub struct CriServer {
    /// Path to the Unix domain socket.
    socket_path: PathBuf,
    /// Shared image store.
    image_store: Arc<ImageStore>,
    /// Registry authentication.
    auth: RegistryAuth,
    /// Streaming server bind address.
    streaming_addr: SocketAddr,
    /// Runtime-level CRI defaults and RuntimeClass overrides.
    runtime_options: CriRuntimeOptions,
}

/// Default streaming server bind address.
const DEFAULT_STREAMING_ADDR: ([u8; 4], u16) = ([127, 0, 0, 1], 18800);
/// A stale Unix socket can be removed, but a live socket must never be
/// replaced: doing so lets a second CRI server steal the pathname while the
/// first one continues writing state and managing the same sandboxes.
const SOCKET_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// Prepare a CRI Unix-socket pathname for binding.
///
/// Unix sockets survive an ungraceful process exit as filesystem entries, so a
/// stale entry is normal.  The old implementation unconditionally removed the
/// pathname, which made a second `a3s-box-cri` silently take over a live
/// server's endpoint.  Probe a socket first and fail closed when it is live,
/// unresponsive, or has been replaced while we were probing.  Never remove a
/// regular file or symlink at the configured path.
async fn prepare_socket_path(path: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "CRI socket path {} exists but is not a Unix socket; refusing to remove it",
                path.display()
            ),
        ));
    }

    match tokio::time::timeout(SOCKET_PROBE_TIMEOUT, UnixStream::connect(path)).await {
        Ok(Ok(_stream)) => {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("CRI socket {} is already in use", path.display()),
            ));
        }
        Ok(Err(error))
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) => {}
        Ok(Err(error)) => return Err(error),
        Err(_) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "timed out probing CRI socket {}; refusing to remove a possibly live endpoint",
                    path.display()
                ),
            ));
        }
    }

    // Do not unlink a pathname that changed between the probe and cleanup.
    let current = std::fs::symlink_metadata(path)?;
    if !current.file_type().is_socket()
        || current.dev() != metadata.dev()
        || current.ino() != metadata.ino()
    {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "CRI socket path {} changed while probing; refusing to remove it",
                path.display()
            ),
        ));
    }

    std::fs::remove_file(path)
}

impl CriServer {
    /// Create a new CRI server.
    pub fn new(socket_path: PathBuf, image_store: Arc<ImageStore>, auth: RegistryAuth) -> Self {
        Self {
            socket_path,
            image_store,
            auth,
            streaming_addr: SocketAddr::from(DEFAULT_STREAMING_ADDR),
            runtime_options: CriRuntimeOptions::default(),
        }
    }

    /// Set the streaming server bind address.
    pub fn with_streaming_addr(mut self, addr: SocketAddr) -> Self {
        self.streaming_addr = addr;
        self
    }

    /// Set runtime-level CRI defaults and RuntimeClass image overrides.
    pub fn with_runtime_options(mut self, options: CriRuntimeOptions) -> Self {
        self.runtime_options = options;
        self
    }

    /// Start serving CRI RPCs on the Unix socket.
    pub async fn serve(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Ensure parent directory exists
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // A previous process may have left a stale socket after a crash.  Do
        // this check before binding either endpoint, and never replace a live
        // server's pathname.
        prepare_socket_path(&self.socket_path).await?;

        // Bind the TCP streaming endpoint before the UDS.  If either endpoint
        // is unavailable, no background streaming task or CRI socket is left
        // behind for a later process to mistake for a healthy server.
        let streaming_server = StreamingServer::new(self.streaming_addr).bind().await?;
        let streaming_handle = streaming_server.handle();

        let uds = UnixListener::bind(&self.socket_path)?;
        let uds_stream = UnixListenerStream::new(uds);

        let streaming_task = tokio::spawn(async move {
            if let Err(e) = streaming_server.serve().await {
                tracing::error!(error = %e, "CRI streaming server failed");
            }
        });

        let runtime_service = BoxRuntimeService::new(
            self.image_store.clone(),
            self.auth.clone(),
            streaming_handle,
        )
        .with_runtime_options(self.runtime_options.clone());
        runtime_service.load_state().await;
        let image_service = BoxImageService::new(self.image_store.clone(), self.auth.clone());

        tracing::info!(
            socket = %self.socket_path.display(),
            streaming_addr = %self.streaming_addr,
            "CRI server listening"
        );

        // Keep a handle so we can reap sandbox VMs once the server stops; the
        // service itself is moved into the gRPC server below.
        let shutdown_service = runtime_service.clone();
        let result = Server::builder()
            .add_service(RuntimeServiceServer::new(runtime_service))
            .add_service(ImageServiceServer::new(image_service))
            .serve_with_incoming_shutdown(uds_stream, shutdown_signal())
            .await;

        // The server stopped — on graceful signal (Ok) OR a transport error.
        // Reap sandbox VMs + unmount overlays unconditionally so they do not
        // orphan across restarts, then surface any server error.
        tracing::info!("CRI server stopping — reaping sandbox VMs");
        shutdown_service.shutdown_all_sandboxes().await;
        // The streaming listener is an independent task and does not observe
        // the gRPC shutdown future.  Abort and join it before returning so the
        // TCP port is released and a replacement CRI server can start without
        // inheriting a hidden accept loop.  Existing per-connection tasks are
        // allowed to finish independently; they own their sockets and do not
        // retain the listener.
        streaming_task.abort();
        if let Err(error) = streaming_task.await {
            if !error.is_cancelled() {
                tracing::warn!(%error, "CRI streaming server task ended unexpectedly");
            }
        }
        result?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stale_socket_is_removed_before_bind() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("cri.sock");
        let listener = UnixListener::bind(&path).unwrap();
        drop(listener);
        assert!(path.exists());

        prepare_socket_path(&path).await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn live_socket_is_not_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("cri.sock");
        let _listener = UnixListener::bind(&path).unwrap();

        let error = prepare_socket_path(&path)
            .await
            .expect_err("a live listener must block takeover");
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(path.exists());
    }

    #[tokio::test]
    async fn regular_file_at_socket_path_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("cri.sock");
        std::fs::write(&path, b"keep me").unwrap();

        let error = prepare_socket_path(&path)
            .await
            .expect_err("a regular file must not be unlinked");
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert_eq!(std::fs::read(&path).unwrap(), b"keep me");
    }
}

/// Resolves when the process receives a termination signal, driving a graceful
/// gRPC server shutdown so the CRI can reap its sandbox VMs.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) {
            (Ok(mut sigterm), Ok(mut sigint)) => {
                tokio::select! {
                    _ = sigterm.recv() => tracing::info!("Received SIGTERM, shutting down CRI"),
                    _ = sigint.recv() => tracing::info!("Received SIGINT, shutting down CRI"),
                }
            }
            _ => {
                tracing::error!("Failed to install signal handlers; graceful shutdown disabled");
                std::future::pending::<()>().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("Received Ctrl-C, shutting down CRI");
    }
}
