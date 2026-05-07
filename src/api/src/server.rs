//! Docker Engine API server implementation.

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    Router,
    routing::{get, post, delete},
};
use tokio::net::UnixListener;
use tower_http::trace::TraceLayer;
use tracing::info;

use crate::handlers;

/// Docker Engine API server.
pub struct ApiServer {
    socket_path: PathBuf,
}

impl ApiServer {
    /// Create a new API server.
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    /// Start the API server.
    pub async fn serve(self) -> Result<(), Box<dyn std::error::Error>> {
        // Remove existing socket if it exists
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)?;
        }

        // Ensure parent directory exists
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        info!(socket = %self.socket_path.display(), "Starting Docker Engine API server");

        // Build router with all API endpoints
        let app = self.build_router();

        // Create Unix socket listener
        let listener = UnixListener::bind(&self.socket_path)?;

        info!(socket = %self.socket_path.display(), "Docker Engine API server listening");

        // Serve the API
        axum::serve(listener, app).await?;

        Ok(())
    }

    /// Build the API router with all endpoints.
    fn build_router(&self) -> Router {
        Router::new()
            // System endpoints
            .route("/_ping", get(handlers::system::ping))
            .route("/version", get(handlers::system::version))
            .route("/info", get(handlers::system::info))
            .route("/events", get(handlers::system::events))

            // Container endpoints
            .route("/containers/json", get(handlers::containers::list))
            .route("/containers/create", post(handlers::containers::create))
            .route("/containers/:id/json", get(handlers::containers::inspect))
            .route("/containers/:id/start", post(handlers::containers::start))
            .route("/containers/:id/stop", post(handlers::containers::stop))
            .route("/containers/:id/restart", post(handlers::containers::restart))
            .route("/containers/:id/kill", post(handlers::containers::kill))
            .route("/containers/:id/pause", post(handlers::containers::pause))
            .route("/containers/:id/unpause", post(handlers::containers::unpause))
            .route("/containers/:id/wait", post(handlers::containers::wait))
            .route("/containers/:id", delete(handlers::containers::remove))
            .route("/containers/:id/logs", get(handlers::containers::logs))
            .route("/containers/:id/stats", get(handlers::containers::stats))
            .route("/containers/:id/top", get(handlers::containers::top))
            .route("/containers/:id/exec", post(handlers::containers::exec_create))
            .route("/exec/:id/start", post(handlers::containers::exec_start))

            // Image endpoints
            .route("/images/json", get(handlers::images::list))
            .route("/images/create", post(handlers::images::pull))
            .route("/images/:name/json", get(handlers::images::inspect))
            .route("/images/:name/history", get(handlers::images::history))
            .route("/images/:name/push", post(handlers::images::push))
            .route("/images/:name/tag", post(handlers::images::tag))
            .route("/images/:name", delete(handlers::images::remove))
            .route("/build", post(handlers::images::build))

            // Network endpoints
            .route("/networks", get(handlers::networks::list))
            .route("/networks/create", post(handlers::networks::create))
            .route("/networks/:id", get(handlers::networks::inspect))
            .route("/networks/:id", delete(handlers::networks::remove))
            .route("/networks/:id/connect", post(handlers::networks::connect))
            .route("/networks/:id/disconnect", post(handlers::networks::disconnect))

            // Volume endpoints
            .route("/volumes", get(handlers::volumes::list))
            .route("/volumes/create", post(handlers::volumes::create))
            .route("/volumes/:name", get(handlers::volumes::inspect))
            .route("/volumes/:name", delete(handlers::volumes::remove))

            // Add tracing middleware
            .layer(TraceLayer::new_for_http())
    }
}
