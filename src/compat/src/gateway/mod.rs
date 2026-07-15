mod proxy;
mod tls;

use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use a3s_box_core::ExecutionPortConnector;
use hyper::server::conn::Http;
use hyper::service::service_fn;
use rustls::ServerConfig;
use thiserror::Error;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio::time;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, warn};

use crate::routing::{RouteLeaseService, SandboxRouteParser};

pub use proxy::DataPlaneProxy;

/// Startup-validated TLS listener and bounded proxy settings.
#[derive(Debug, Clone)]
pub struct DataPlaneGatewayConfig {
    pub(crate) listen: SocketAddr,
    pub(crate) certificate_path: PathBuf,
    pub(crate) private_key_path: PathBuf,
    pub(crate) max_connections: NonZeroUsize,
    pub(crate) handshake_timeout: Duration,
    pub(crate) connect_timeout: Duration,
    pub(crate) drain_timeout: Duration,
}

impl DataPlaneGatewayConfig {
    pub const fn listen(&self) -> SocketAddr {
        self.listen
    }

    pub const fn max_connections(&self) -> NonZeroUsize {
        self.max_connections
    }

    pub const fn handshake_timeout(&self) -> Duration {
        self.handshake_timeout
    }

    pub const fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    pub const fn drain_timeout(&self) -> Duration {
        self.drain_timeout
    }
}

#[derive(Clone)]
pub struct DataPlaneGateway {
    config: DataPlaneGatewayConfig,
    tls: Arc<ServerConfig>,
    proxy: DataPlaneProxy,
}

impl DataPlaneGateway {
    pub async fn build(
        config: DataPlaneGatewayConfig,
        parser: SandboxRouteParser,
        leases: RouteLeaseService,
        connector: Arc<dyn ExecutionPortConnector>,
    ) -> DataPlaneGatewayResult<Self> {
        let tls =
            tls::load_server_config(&config.certificate_path, &config.private_key_path).await?;
        let proxy = DataPlaneProxy::new(parser, leases, connector, config.connect_timeout);
        Ok(Self { config, tls, proxy })
    }

    pub const fn listen(&self) -> SocketAddr {
        self.config.listen
    }

    pub fn proxy(&self) -> DataPlaneProxy {
        self.proxy.clone()
    }

    pub async fn serve(
        self,
        listener: TcpListener,
        mut shutdown: watch::Receiver<bool>,
    ) -> DataPlaneGatewayResult<()> {
        let semaphore = Arc::new(Semaphore::new(self.config.max_connections.get()));
        let acceptor = TlsAcceptor::from(self.tls.clone());
        let mut connections = JoinSet::new();

        loop {
            let permit = tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                    continue;
                }
                permit = semaphore.clone().acquire_owned() => {
                    permit.map_err(|_| DataPlaneGatewayError::ConnectionLimiterClosed)?
                }
            };
            let accepted = tokio::select! {
                changed = shutdown.changed() => {
                    drop(permit);
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                    continue;
                }
                accepted = listener.accept() => accepted,
            };
            let (socket, peer) = accepted.map_err(DataPlaneGatewayError::Accept)?;
            let acceptor = acceptor.clone();
            let proxy = self.proxy.clone();
            let connection_shutdown = shutdown.clone();
            let handshake_timeout = self.config.handshake_timeout;
            connections.spawn(async move {
                serve_connection(
                    socket,
                    peer,
                    acceptor,
                    proxy,
                    handshake_timeout,
                    connection_shutdown,
                    permit,
                )
                .await;
            });

            while connections.try_join_next().is_some() {}
        }

        drain_connections(&mut connections, self.config.drain_timeout).await;
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
async fn serve_connection(
    socket: TcpStream,
    peer: SocketAddr,
    acceptor: TlsAcceptor,
    proxy: DataPlaneProxy,
    handshake_timeout: Duration,
    mut shutdown: watch::Receiver<bool>,
    _permit: OwnedSemaphorePermit,
) {
    let tls = match time::timeout(handshake_timeout, acceptor.accept(socket)).await {
        Ok(Ok(tls)) => tls,
        Ok(Err(error)) => {
            debug!(%peer, %error, "sandbox data-plane TLS handshake rejected");
            return;
        }
        Err(_) => {
            debug!(%peer, "sandbox data-plane TLS handshake timed out");
            return;
        }
    };
    let service = service_fn(move |request| {
        let proxy = proxy.clone();
        async move { Ok::<_, std::convert::Infallible>(proxy.handle(request).await) }
    });
    let mut http = Http::new();
    http.http1_keep_alive(true)
        .http1_half_close(true)
        .http2_adaptive_window(true);
    let connection = http.serve_connection(tls, service).with_upgrades();
    tokio::pin!(connection);
    tokio::select! {
        result = &mut connection => {
            if let Err(error) = result {
                debug!(%peer, %error, "sandbox data-plane connection closed with an HTTP error");
            }
        }
        changed = shutdown.changed() => {
            if changed.is_err() || *shutdown.borrow() {
                connection.as_mut().graceful_shutdown();
                if let Err(error) = connection.await {
                    debug!(%peer, %error, "sandbox data-plane connection failed while draining");
                }
            }
        }
    }
}

async fn drain_connections(connections: &mut JoinSet<()>, timeout: Duration) {
    let drain = async { while connections.join_next().await.is_some() {} };
    if time::timeout(timeout, drain).await.is_err() {
        let remaining = connections.len();
        warn!(
            remaining,
            "aborting sandbox data-plane connections after drain timeout"
        );
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }
}

#[derive(Debug, Error)]
pub enum DataPlaneGatewayError {
    #[error("failed to read TLS certificate {path}: {source}")]
    ReadCertificate {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read TLS private key {path}: {source}")]
    ReadPrivateKey {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("TLS certificate file contains no certificates: {0}")]
    MissingCertificate(PathBuf),
    #[error("TLS private key file contains no supported private key: {0}")]
    MissingPrivateKey(PathBuf),
    #[error("invalid TLS certificate or private key: {0}")]
    InvalidTls(#[source] rustls::Error),
    #[error("failed to accept a sandbox data-plane connection: {0}")]
    Accept(#[source] std::io::Error),
    #[error("sandbox data-plane connection limiter closed unexpectedly")]
    ConnectionLimiterClosed,
}

pub type DataPlaneGatewayResult<T> = std::result::Result<T, DataPlaneGatewayError>;

#[cfg(test)]
mod tests;
