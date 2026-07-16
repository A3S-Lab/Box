use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::Request;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;

use crate::control::ControlService;
use crate::snapshot::SnapshotService;
use crate::volume::VolumeService;

use super::auth::{CredentialVerifier, PresentedCredential};
use super::cursor::CursorDecoder;
use super::error::ApiError;
use super::lifecycle;
use super::logs;
use super::snapshots;
use super::volume_content;
use super::volumes;

#[derive(Debug, Clone)]
pub struct LifecycleHttpConfig {
    pub domain: Option<String>,
    pub max_json_bytes: usize,
}

impl Default for LifecycleHttpConfig {
    fn default() -> Self {
        Self {
            domain: None,
            max_json_bytes: 1024 * 1024,
        }
    }
}

#[derive(Clone)]
pub struct LifecycleHttpState {
    service: Arc<ControlService>,
    verifier: Arc<dyn CredentialVerifier>,
    cursors: Arc<dyn CursorDecoder>,
    config: LifecycleHttpConfig,
    volumes: Option<Arc<VolumeService>>,
    snapshots: Option<Arc<SnapshotService>>,
}

impl LifecycleHttpState {
    pub fn new(
        service: Arc<ControlService>,
        verifier: Arc<dyn CredentialVerifier>,
        cursors: Arc<dyn CursorDecoder>,
        config: LifecycleHttpConfig,
    ) -> Self {
        Self {
            service,
            verifier,
            cursors,
            config,
            volumes: None,
            snapshots: None,
        }
    }

    pub fn with_volume_service(mut self, volumes: Arc<VolumeService>) -> Self {
        self.volumes = Some(volumes);
        self
    }

    pub fn with_snapshot_service(mut self, snapshots: Arc<SnapshotService>) -> Self {
        self.snapshots = Some(snapshots);
        self
    }

    pub(crate) fn service(&self) -> &ControlService {
        &self.service
    }

    pub(crate) fn cursors(&self) -> &Arc<dyn CursorDecoder> {
        &self.cursors
    }

    pub(crate) fn domain(&self) -> Option<&str> {
        self.config.domain.as_deref()
    }

    pub(crate) fn volume_service(&self) -> Result<&VolumeService, ApiError> {
        self.volumes.as_deref().ok_or_else(ApiError::internal)
    }

    pub(crate) fn snapshot_service(&self) -> Result<&SnapshotService, ApiError> {
        self.snapshots.as_deref().ok_or_else(ApiError::internal)
    }
}

pub fn lifecycle_router(state: LifecycleHttpState) -> Router {
    let max_json_bytes = state.config.max_json_bytes;
    let control = Router::new()
        .route(
            "/sandboxes",
            get(lifecycle::list_running).post(lifecycle::create),
        )
        .route("/sandboxes/metrics", get(lifecycle::get_metrics_batch))
        .route("/v2/sandboxes", get(lifecycle::list))
        .route(
            "/sandboxes/:sandbox_id",
            get(lifecycle::get).delete(lifecycle::kill),
        )
        .route("/sandboxes/:sandbox_id/connect", post(lifecycle::connect))
        .route("/sandboxes/:sandbox_id/logs", get(logs::legacy))
        .route("/sandboxes/:sandbox_id/pause", post(lifecycle::pause))
        .route("/sandboxes/:sandbox_id/resume", post(lifecycle::resume))
        .route(
            "/sandboxes/:sandbox_id/metrics",
            get(lifecycle::get_metrics),
        )
        .route("/sandboxes/:sandbox_id/refreshes", post(lifecycle::refresh))
        .route("/sandboxes/:sandbox_id/snapshots", post(snapshots::create))
        .route(
            "/sandboxes/:sandbox_id/timeout",
            post(lifecycle::set_timeout),
        )
        .route("/v2/sandboxes/:sandbox_id/logs", get(logs::v2))
        .route("/volumes", get(volumes::list).post(volumes::create))
        .route(
            "/volumes/:volume_id",
            get(volumes::get).delete(volumes::delete),
        )
        .route("/snapshots", get(snapshots::list))
        .route(
            "/templates/*template_id",
            axum::routing::delete(snapshots::delete),
        )
        .fallback(fallback)
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate));
    let content = Router::new()
        .route(
            "/volumecontent/:volume_id/path",
            get(volume_content::stat)
                .patch(volume_content::update_metadata)
                .delete(volume_content::remove),
        )
        .route(
            "/volumecontent/:volume_id/dir",
            get(volume_content::list).post(volume_content::make_dir),
        )
        .route(
            "/volumecontent/:volume_id/file",
            get(volume_content::read_file).put(volume_content::write_file),
        );
    control
        .merge(content)
        .layer(DefaultBodyLimit::max(max_json_bytes))
        .with_state(state)
}

async fn authenticate<B>(
    State(state): State<LifecycleHttpState>,
    mut request: Request<B>,
    next: Next<B>,
) -> Response {
    let result = async {
        let credential = PresentedCredential::from_headers(request.headers())?;
        let account = state.verifier.verify(&credential).await?;
        request.extensions_mut().insert(account);
        Ok::<_, ApiError>(next.run(request).await)
    }
    .await;
    result.unwrap_or_else(IntoResponse::into_response)
}

async fn fallback() -> ApiError {
    ApiError::not_found()
}
