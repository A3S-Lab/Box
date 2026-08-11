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

use super::{
    DurableScaleAuthority, LocalScaleReconciler, ScaleAuthorityError, ScaleReconcileError,
};

pub type SharedScaleAuthority = Arc<ScaleApiState>;

pub struct ScaleApiState {
    authority: Mutex<DurableScaleAuthority>,
    reconciler: Option<Arc<LocalScaleReconciler>>,
}

impl ScaleApiState {
    pub fn authority_only(authority: DurableScaleAuthority) -> SharedScaleAuthority {
        Arc::new(Self {
            authority: Mutex::new(authority),
            reconciler: None,
        })
    }

    pub fn with_reconciler(
        authority: DurableScaleAuthority,
        reconciler: LocalScaleReconciler,
    ) -> SharedScaleAuthority {
        Arc::new(Self {
            authority: Mutex::new(authority),
            reconciler: Some(Arc::new(reconciler)),
        })
    }

    async fn observation(&self, service: &str) -> Result<ScaleObservation, ScaleReconcileError> {
        let mut observation = self.authority.lock().await.observation(service);
        if let Some(reconciler) = &self.reconciler {
            let workloads = reconciler
                .observation(service, observation.replicas)
                .await?;
            observation.ready_replicas = workloads.ready_replicas;
            observation.endpoints = workloads.endpoints;
        }
        Ok(observation)
    }

    async fn reconcile_desired_services(&self) {
        let Some(reconciler) = &self.reconciler else {
            return;
        };
        for service in reconciler.services() {
            let desired = self.authority.lock().await.observation(&service).replicas;
            if let Err(error) = reconciler.reconcile(&service, desired).await {
                tracing::warn!(%service, %error, "Scale workload reconciliation failed");
            }
        }
    }
}

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
    let convergence_state = Arc::downgrade(&authority);
    let convergence = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            interval.tick().await;
            let Some(state) = convergence_state.upgrade() else {
                return;
            };
            state.reconcile_desired_services().await;
        }
    });
    let result = axum::Server::bind(&address)
        .serve(scale_router(authority).into_make_service())
        .await
        .map_err(std::io::Error::other);
    convergence.abort();
    result
}

async fn observe(
    Path(service): Path<String>,
    State(authority): State<SharedScaleAuthority>,
) -> Response {
    match authority.observation(&service).await {
        Ok(observation) => (StatusCode::OK, Json(observation)).into_response(),
        Err(error) => {
            let (status, code) = if matches!(&error, ScaleReconcileError::UnknownService(_)) {
                (StatusCode::NOT_FOUND, "unknown_service")
            } else {
                (StatusCode::SERVICE_UNAVAILABLE, "observation_failed")
            };
            conflict_response(
                status,
                ScaleOperationConflict {
                    code: code.to_string(),
                    message: error.to_string(),
                    observation: authority.authority.lock().await.observation(&service),
                },
            )
        }
    }
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
                observation: authority.authority.lock().await.observation(&service),
            },
        );
    }

    if let Some(reconciler) = &authority.reconciler {
        if !reconciler.knows_service(&service) {
            return conflict_response(
                StatusCode::NOT_FOUND,
                ScaleOperationConflict {
                    code: "unknown_service".to_string(),
                    message: format!("service {service:?} has no Box workload template"),
                    observation: authority.authority.lock().await.observation(&service),
                },
            );
        }
    }

    let applied = {
        let mut authority = authority.authority.lock().await;
        authority.apply(&request)
    };
    match applied {
        Ok(mut response) => {
            if let Some(reconciler) = &authority.reconciler {
                match reconciler
                    .reconcile(&service, request.desired_replicas)
                    .await
                {
                    Ok(report) => {
                        response.actual_replicas = report.ready_replicas;
                        response.message = format!(
                            "Box reconciled service '{}' to {} ready replicas",
                            service, report.ready_replicas
                        );
                        match authority
                            .authority
                            .lock()
                            .await
                            .finalize(&request, response.clone())
                        {
                            Ok(response) => (StatusCode::OK, Json(response)).into_response(),
                            Err(error) => {
                                authority_error_response(&authority, &service, error).await
                            }
                        }
                    }
                    Err(error) => {
                        let observation = match authority.observation(&service).await {
                            Ok(observation) => observation,
                            Err(_) => authority.authority.lock().await.observation(&service),
                        };
                        conflict_response(
                            StatusCode::SERVICE_UNAVAILABLE,
                            ScaleOperationConflict {
                                code: "reconcile_failed".to_string(),
                                message: error.to_string(),
                                observation,
                            },
                        )
                    }
                }
            } else {
                (StatusCode::OK, Json(response)).into_response()
            }
        }
        Err(ScaleAuthorityError::Conflict(_, conflict)) => {
            let status = match conflict.code.as_str() {
                "stale_revision" | "operation_conflict" => StatusCode::CONFLICT,
                "capacity_exceeded" => StatusCode::INSUFFICIENT_STORAGE,
                _ => StatusCode::UNPROCESSABLE_ENTITY,
            };
            conflict_response(status, conflict)
        }
        Err(error) => authority_error_response(&authority, &service, error).await,
    }
}

async fn authority_error_response(
    authority: &SharedScaleAuthority,
    service: &str,
    error: ScaleAuthorityError,
) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ScaleOperationConflict {
            code: "authority_state_error".to_string(),
            message: error.to_string(),
            observation: authority.authority.lock().await.observation(service),
        }),
    )
        .into_response()
}

fn conflict_response(status: StatusCode, conflict: ScaleOperationConflict) -> Response {
    (status, Json(conflict)).into_response()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex as StdMutex,
        },
    };

    use super::*;
    use a3s_box_core::{
        scale::{ScaleDirection, ScaleOperationResponse, SCALE_OPERATION_SCHEMA_VERSION},
        ExecutionIsolation, ExecutionManagerError, ExecutionManagerResult, ExecutionState,
        KillOutcome,
    };
    use async_trait::async_trait;
    use chrono::Utc;

    use crate::{
        BoxRecord, LocalExecutionBackend, LocalExecutionHandle, LocalExecutionManager,
        LocalExecutionObservation, ScaleServiceCatalog,
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
        let authority = ScaleApiState::authority_only(
            DurableScaleAuthority::open(directory.path().join("state.json"), 10).unwrap(),
        );
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

    struct RecordingBackend {
        running: StdMutex<HashSet<String>>,
        starts: AtomicUsize,
    }

    impl RecordingBackend {
        fn new() -> Self {
            Self {
                running: StdMutex::new(HashSet::new()),
                starts: AtomicUsize::new(0),
            }
        }

        fn handle(record: &BoxRecord) -> LocalExecutionHandle {
            LocalExecutionHandle {
                started_at: Utc::now(),
                pid: None,
                pid_start_time: None,
                exec_socket_path: record.box_dir.join("sockets/exec.sock"),
                console_log: record.box_dir.join("logs/console.log"),
                anonymous_volumes: Vec::new(),
                oci_runtime: None,
            }
        }
    }

    #[async_trait]
    impl LocalExecutionBackend for RecordingBackend {
        async fn start(&self, record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            self.running.lock().unwrap().insert(record.id.clone());
            Ok(Self::handle(record))
        }

        async fn inspect(
            &self,
            record: &BoxRecord,
        ) -> ExecutionManagerResult<LocalExecutionObservation> {
            if self.running.lock().unwrap().contains(&record.id) {
                Ok(LocalExecutionObservation {
                    state: ExecutionState::Running,
                    handle: Some(Self::handle(record)),
                    exit_code: None,
                })
            } else {
                Ok(LocalExecutionObservation {
                    state: ExecutionState::Stopped,
                    handle: None,
                    exit_code: Some(0),
                })
            }
        }

        async fn pause(
            &self,
            _record: &BoxRecord,
            _keep_memory: bool,
        ) -> ExecutionManagerResult<LocalExecutionHandle> {
            Err(ExecutionManagerError::Unavailable(
                "pause unsupported".to_string(),
            ))
        }

        async fn resume(
            &self,
            _record: &BoxRecord,
        ) -> ExecutionManagerResult<LocalExecutionHandle> {
            Err(ExecutionManagerError::Unavailable(
                "resume unsupported".to_string(),
            ))
        }

        async fn kill(&self, record: &BoxRecord) -> ExecutionManagerResult<KillOutcome> {
            let removed = self.running.lock().unwrap().remove(&record.id);
            Ok(if removed {
                KillOutcome::Killed
            } else {
                KillOutcome::AlreadyStopped
            })
        }
    }

    fn reconciled_state(
        authority_path: &std::path::Path,
        box_state_path: &std::path::Path,
        home: &std::path::Path,
        backend: Arc<RecordingBackend>,
    ) -> SharedScaleAuthority {
        let authority = DurableScaleAuthority::open(authority_path, 10).unwrap();
        let manager = LocalExecutionManager::new(box_state_path, home, backend);
        let catalog = ScaleServiceCatalog::from_acl_str(
            r#"service "api" { image = "api:v1" }"#,
            "gateway-scale",
            ExecutionIsolation::Sandbox,
        )
        .unwrap();
        ScaleApiState::with_reconciler(authority, LocalScaleReconciler::new(manager, catalog))
    }

    async fn wait_for_server(client: &reqwest::Client, url: &str) {
        for _ in 0..100 {
            if client.get(url).send().await.is_ok() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("scale API did not become ready at {url}");
    }

    #[tokio::test]
    async fn tcp_boundary_reconciles_real_facade_and_recovers_after_restart() {
        let directory = tempfile::tempdir().unwrap();
        let authority_path = directory.path().join("scale-authority.json");
        let box_state_path = directory.path().join("boxes.json");
        let home = directory.path().join("home");
        let backend = Arc::new(RecordingBackend::new());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let client = reqwest::Client::new();
        let url = format!("http://{address}/v1/scale/api");

        let first_state =
            reconciled_state(&authority_path, &box_state_path, &home, backend.clone());
        let first_server = tokio::spawn(serve_scale_api(address, first_state));
        wait_for_server(&client, &url).await;
        let up = request("scale-v1-facade-up", "0");
        let accepted: ScaleOperationResponse = client
            .post(&url)
            .json(&up)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(accepted.actual_replicas, 2);
        assert_eq!(backend.starts.load(Ordering::SeqCst), 2);
        let observation: ScaleObservation = client
            .get(&url)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(observation.replicas, 2);
        assert_eq!(observation.ready_replicas, 2);
        assert!(observation.endpoints.is_empty());
        first_server.abort();
        let _ = first_server.await;

        let restarted_state =
            reconciled_state(&authority_path, &box_state_path, &home, backend.clone());
        let restarted_server = tokio::spawn(serve_scale_api(address, restarted_state));
        wait_for_server(&client, &url).await;
        let replayed: ScaleOperationResponse = client
            .post(&url)
            .json(&up)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(replayed, accepted);
        assert_eq!(backend.starts.load(Ordering::SeqCst), 2);

        let down = ScaleOperationRequest {
            operation_id: "scale-v1-facade-down".to_string(),
            expected_revision: Some("1".to_string()),
            direction: ScaleDirection::Down,
            current_replicas: 2,
            desired_replicas: 0,
            ..request("unused", "unused")
        };
        let removed: ScaleOperationResponse = client
            .post(&url)
            .json(&down)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(removed.actual_replicas, 0);
        assert!(backend.running.lock().unwrap().is_empty());
        restarted_server.abort();
        let _ = restarted_server.await;
    }
}
