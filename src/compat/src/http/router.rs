use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::Request;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;

use crate::control::ControlService;

use super::auth::{CredentialVerifier, PresentedCredential};
use super::cursor::CursorDecoder;
use super::error::ApiError;
use super::lifecycle;

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
        }
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
}

pub fn lifecycle_router(state: LifecycleHttpState) -> Router {
    let max_json_bytes = state.config.max_json_bytes;
    Router::new()
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
        .route("/sandboxes/:sandbox_id/pause", post(lifecycle::pause))
        .route("/sandboxes/:sandbox_id/resume", post(lifecycle::resume))
        .route(
            "/sandboxes/:sandbox_id/metrics",
            get(lifecycle::get_metrics),
        )
        .route("/sandboxes/:sandbox_id/refreshes", post(lifecycle::refresh))
        .route(
            "/sandboxes/:sandbox_id/timeout",
            post(lifecycle::set_timeout),
        )
        .fallback(fallback)
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate))
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
