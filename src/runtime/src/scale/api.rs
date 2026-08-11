//! Machine-facing HTTP boundary for Gateway scaling decisions.

use std::{net::SocketAddr, sync::Arc};

use a3s_box_core::scale::{ScaleObservation, ScaleOperationConflict, ScaleOperationRequest};
use axum::{
    extract::{DefaultBodyLimit, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use tokio::sync::Mutex;

use super::{DurableScaleAuthority, ScaleAuthorityError};

pub type SharedScaleAuthority = Arc<Mutex<DurableScaleAuthority>>;

pub fn scale_router(authority: SharedScaleAuthority) -> Router {
    Router::new()
        .route("/v1/scale/:service", get(observe).post(apply))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(authority)
}

pub async fn serve_scale_api(
    address: SocketAddr,
    authority: SharedScaleAuthority,
) -> Result<(), std::io::Error> {
    axum::Server::bind(&address)
        .serve(scale_router(authority).into_make_service())
        .await
        .map_err(std::io::Error::other)
}

async fn observe(
    Path(service): Path<String>,
    State(authority): State<SharedScaleAuthority>,
) -> Json<ScaleObservation> {
    Json(authority.lock().await.observation(&service))
}

async fn apply(
    Path(service): Path<String>,
    State(authority): State<SharedScaleAuthority>,
    Json(request): Json<ScaleOperationRequest>,
) -> Response {
    if request.service != service {
        return conflict_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ScaleOperationConflict {
                code: "service_mismatch".to_string(),
                message: format!(
                    "request service {:?} does not match path service {:?}",
                    request.service, service
                ),
                observation: authority.lock().await.observation(&service),
            },
        );
    }

    match authority.lock().await.apply(&request) {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(ScaleAuthorityError::Conflict(_, conflict)) => {
            let status = match conflict.code.as_str() {
                "stale_revision" | "operation_conflict" => StatusCode::CONFLICT,
                "capacity_exceeded" => StatusCode::INSUFFICIENT_STORAGE,
                _ => StatusCode::UNPROCESSABLE_ENTITY,
            };
            conflict_response(status, conflict)
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ScaleOperationConflict {
                code: "authority_state_error".to_string(),
                message: error.to_string(),
                observation: authority.lock().await.observation(&service),
            }),
        )
            .into_response(),
    }
}

fn conflict_response(status: StatusCode, conflict: ScaleOperationConflict) -> Response {
    (status, Json(conflict)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::scale::{
        ScaleDirection, ScaleOperationResponse, SCALE_OPERATION_SCHEMA_VERSION,
    };

    fn request(operation_id: &str, revision: &str) -> ScaleOperationRequest {
        ScaleOperationRequest {
            schema_version: SCALE_OPERATION_SCHEMA_VERSION,
            operation_id: operation_id.to_string(),
            service: "api".to_string(),
            expected_revision: Some(revision.to_string()),
            direction: ScaleDirection::Up,
            current_replicas: 0,
            desired_replicas: 2,
            reason: "load".to_string(),
        }
    }

    #[tokio::test]
    async fn tcp_boundary_applies_replays_and_rejects_stale_operations() {
        let directory = tempfile::tempdir().unwrap();
        let authority = Arc::new(Mutex::new(
            DurableScaleAuthority::open(directory.path().join("state.json"), 10).unwrap(),
        ));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let server = tokio::spawn(serve_scale_api(address, authority));
        let client = reqwest::Client::new();
        let url = format!("http://{address}/v1/scale/api");
        for _ in 0..50 {
            if client.get(&url).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let operation = request("scale-v1-http", "0");
        let accepted = client.post(&url).json(&operation).send().await.unwrap();
        assert_eq!(accepted.status(), StatusCode::OK);
        let accepted: ScaleOperationResponse = accepted.json().await.unwrap();

        let replayed: ScaleOperationResponse = client
            .post(&url)
            .json(&operation)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(replayed, accepted);

        let stale = client
            .post(&url)
            .json(&request("scale-v1-stale", "0"))
            .send()
            .await
            .unwrap();
        assert_eq!(stale.status(), StatusCode::CONFLICT);
        server.abort();
    }
}
