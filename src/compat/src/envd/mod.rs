mod connect;
mod files;
mod process;

use std::sync::Arc;

use a3s_box_core::{
    ExecutionGeneration, ExecutionId, ExecutionManager, ExecutionManagerError,
    ExecutionSessionManager, ExecutionState,
};
use axum::body::Body;
use axum::http::header::{ALLOW, CONTENT_TYPE};
use axum::http::{HeaderValue, Method, Request, Response, StatusCode};
use tracing::debug;

use crate::routing::RouteLease;

/// Host-side implementation of the pinned envd HTTP surface.
///
/// The broker receives only requests that already passed sandbox route and
/// token validation. Runtime state is checked again against the immutable
/// execution generation in that route lease before a response is returned.
#[derive(Clone)]
pub struct EnvdBroker {
    executions: Arc<dyn ExecutionManager>,
    files: files::FileBroker,
    processes: process::ProcessBroker,
}

impl EnvdBroker {
    pub fn new(
        executions: Arc<dyn ExecutionManager>,
        sessions: Arc<dyn ExecutionSessionManager>,
    ) -> Self {
        Self {
            executions,
            files: files::FileBroker::new(sessions.clone()),
            processes: process::ProcessBroker::new(sessions),
        }
    }

    pub(crate) async fn handle(
        &self,
        request: Request<Body>,
        lease: &RouteLease,
    ) -> Response<Body> {
        let path = request.uri().path().to_string();
        if path.starts_with("/process.Process/") {
            return self
                .processes
                .handle(request, lease.execution_id(), lease.execution_generation())
                .await;
        }
        if path == "/files" {
            return self
                .files
                .handle(request, lease.execution_id(), lease.execution_generation())
                .await;
        }
        self.dispatch(
            request.method(),
            &path,
            lease.execution_id(),
            lease.execution_generation(),
        )
        .await
    }

    pub(crate) fn inactive_health(&self) -> Response<Body> {
        sandbox_not_running()
    }

    async fn dispatch(
        &self,
        method: &Method,
        path: &str,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> Response<Body> {
        if path != "/health" {
            return json_response(
                StatusCode::NOT_FOUND,
                "ENVD_ROUTE_NOT_FOUND",
                "envd route not found",
            );
        }
        if method != Method::GET {
            let mut response = json_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "METHOD_NOT_ALLOWED",
                "method not allowed",
            );
            response
                .headers_mut()
                .insert(ALLOW, HeaderValue::from_static("GET"));
            return response;
        }

        match self.executions.inspect(execution_id).await {
            Ok(status)
                if status.execution_id == *execution_id
                    && status.generation == generation
                    && status.state == ExecutionState::Running =>
            {
                let mut response = Response::new(Body::empty());
                *response.status_mut() = StatusCode::NO_CONTENT;
                response
            }
            Ok(status) => {
                debug!(
                    execution_id = %execution_id,
                    lease_generation = generation.get(),
                    observed_execution_id = %status.execution_id,
                    observed_generation = status.generation.get(),
                    observed_state = ?status.state,
                    "envd health rejected stale or inactive runtime evidence"
                );
                sandbox_not_running()
            }
            Err(ExecutionManagerError::NotFound(_) | ExecutionManagerError::Conflict { .. }) => {
                sandbox_not_running()
            }
            Err(error) => {
                debug!(%execution_id, %error, "envd health runtime inspection failed");
                json_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "ENVD_UNAVAILABLE",
                    "envd is temporarily unavailable",
                )
            }
        }
    }
}

fn sandbox_not_running() -> Response<Body> {
    json_response(
        StatusCode::BAD_GATEWAY,
        "SANDBOX_NOT_RUNNING",
        "sandbox is not running",
    )
}

fn json_response(status: StatusCode, code: &'static str, message: &'static str) -> Response<Body> {
    let body = serde_json::json!({ "code": code, "message": message }).to_string();
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

#[cfg(test)]
mod file_tests;
#[cfg(test)]
mod tests;
