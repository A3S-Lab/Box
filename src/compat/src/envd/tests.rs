use std::sync::{Arc, Mutex};

use a3s_box_core::{
    resolve_execution, BoxConfig, ExecutionGeneration, ExecutionId, ExecutionLease,
    ExecutionManager, ExecutionManagerError, ExecutionManagerResult, ExecutionState,
    ExecutionStatus, KillOutcome, OperationId, ReconcileOutcome,
};
use async_trait::async_trait;
use axum::http::{Method, StatusCode};

use super::EnvdBroker;

#[derive(Clone)]
enum Inspection {
    Status(ExecutionStatus),
    NotFound,
    Unavailable,
}

struct InspectOnlyManager {
    inspection: Mutex<Inspection>,
}

impl InspectOnlyManager {
    fn new(inspection: Inspection) -> Self {
        Self {
            inspection: Mutex::new(inspection),
        }
    }
}

#[async_trait]
impl ExecutionManager for InspectOnlyManager {
    async fn inspect(&self, execution_id: &ExecutionId) -> ExecutionManagerResult<ExecutionStatus> {
        match self.inspection.lock().unwrap().clone() {
            Inspection::Status(status) => Ok(status),
            Inspection::NotFound => Err(ExecutionManagerError::NotFound(execution_id.clone())),
            Inspection::Unavailable => Err(ExecutionManagerError::Unavailable(
                "test inspector unavailable".to_string(),
            )),
        }
    }

    async fn pause(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
        _keep_memory: bool,
    ) -> ExecutionManagerResult<ExecutionLease> {
        Err(unsupported())
    }

    async fn resume(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<ExecutionLease> {
        Err(unsupported())
    }

    async fn kill(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<KillOutcome> {
        Err(unsupported())
    }

    async fn reconcile(
        &self,
        _operation_id: &OperationId,
    ) -> ExecutionManagerResult<ReconcileOutcome> {
        Err(unsupported())
    }
}

fn unsupported() -> ExecutionManagerError {
    ExecutionManagerError::Unavailable("unsupported test operation".to_string())
}

fn status(state: ExecutionState, generation: ExecutionGeneration) -> ExecutionStatus {
    ExecutionStatus {
        execution_id: ExecutionId::new("execution-envd-1").unwrap(),
        generation,
        state,
        plan: resolve_execution(&BoxConfig::default()).unwrap(),
    }
}

fn broker(inspection: Inspection) -> EnvdBroker {
    EnvdBroker::new(Arc::new(InspectOnlyManager::new(inspection)))
}

#[tokio::test]
async fn health_requires_exact_running_execution_generation() {
    let execution_id = ExecutionId::new("execution-envd-1").unwrap();
    let response = broker(Inspection::Status(status(
        ExecutionState::Running,
        ExecutionGeneration::INITIAL,
    )))
    .dispatch(
        &Method::GET,
        "/health",
        &execution_id,
        ExecutionGeneration::INITIAL,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let response = broker(Inspection::Status(status(
        ExecutionState::Running,
        ExecutionGeneration::new(2).unwrap(),
    )))
    .dispatch(
        &Method::GET,
        "/health",
        &execution_id,
        ExecutionGeneration::INITIAL,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

    let response = broker(Inspection::Status(status(
        ExecutionState::Stopped,
        ExecutionGeneration::INITIAL,
    )))
    .dispatch(
        &Method::GET,
        "/health",
        &execution_id,
        ExecutionGeneration::INITIAL,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn health_distinguishes_missing_runtime_from_inspector_outage() {
    let execution_id = ExecutionId::new("execution-envd-1").unwrap();
    let missing = broker(Inspection::NotFound)
        .dispatch(
            &Method::GET,
            "/health",
            &execution_id,
            ExecutionGeneration::INITIAL,
        )
        .await;
    assert_eq!(missing.status(), StatusCode::BAD_GATEWAY);

    let unavailable = broker(Inspection::Unavailable)
        .dispatch(
            &Method::GET,
            "/health",
            &execution_id,
            ExecutionGeneration::INITIAL,
        )
        .await;
    assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn broker_rejects_unimplemented_routes_and_methods_without_inspection() {
    let execution_id = ExecutionId::new("execution-envd-1").unwrap();
    let broker = broker(Inspection::Unavailable);
    let missing = broker
        .dispatch(
            &Method::GET,
            "/metrics",
            &execution_id,
            ExecutionGeneration::INITIAL,
        )
        .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    let wrong_method = broker
        .dispatch(
            &Method::POST,
            "/health",
            &execution_id,
            ExecutionGeneration::INITIAL,
        )
        .await;
    assert_eq!(wrong_method.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(wrong_method.headers()["allow"], "GET");
}
