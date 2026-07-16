use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;

use a3s_box_core::{ExecutionManagerError, ExecutionState};
use chrono::Duration;

use super::test_support::{assert_sandbox_request, create_request, test_time, TestHarness};
use super::*;

#[tokio::test]
async fn lifecycle_service_runs_the_official_control_flow() {
    let harness = TestHarness::new();
    let created = harness
        .service
        .create(create_request("owner-1"))
        .await
        .unwrap();
    assert_eq!(created.disposition, ConnectionDisposition::Created);
    assert_eq!(created.record.sandbox_id().as_str(), "sandbox-1");
    assert_eq!(created.record.owner_id(), "owner-1");
    assert_eq!(
        created.record.public_state(),
        Some(PublicSandboxState::Running)
    );
    assert_eq!(
        created.record.expires_at(),
        test_time() + Duration::seconds(321)
    );
    assert_eq!(
        created.envd_access_token.expose_secret(),
        "fixture-envd-token"
    );
    assert_eq!(
        created.traffic_access_token.expose_secret(),
        "fixture-traffic-token"
    );
    assert_sandbox_request(&harness.executions.requests()[0]);

    let sandbox_id = created.record.sandbox_id().clone();
    let connected = harness
        .service
        .connect("owner-1", &sandbox_id, 222)
        .await
        .unwrap();
    assert_eq!(connected.disposition, ConnectionDisposition::AlreadyRunning);
    assert_eq!(
        connected.record.expires_at(),
        test_time() + Duration::seconds(222)
    );

    let page = harness
        .service
        .list(&SandboxListFilter {
            owner_id: "owner-1".to_string(),
            metadata: BTreeMap::from([("team".to_string(), "alpha beta".to_string())]),
            states: BTreeSet::from([PublicSandboxState::Running, PublicSandboxState::Paused]),
            limit: NonZeroU32::new(2).unwrap(),
            after: None,
        })
        .await
        .unwrap();
    assert_eq!(page.records.len(), 1);

    harness
        .service
        .set_timeout("owner-1", &sandbox_id, 123)
        .await
        .unwrap();
    assert_eq!(
        harness
            .service
            .get("owner-1", &sandbox_id)
            .await
            .unwrap()
            .expires_at(),
        test_time() + Duration::seconds(123)
    );
    assert!(harness.service.kill("owner-1", &sandbox_id).await.unwrap());
    assert!(!harness.service.kill("owner-1", &sandbox_id).await.unwrap());
    assert!(matches!(
        harness.service.connect("owner-1", &sandbox_id, 300).await,
        Err(ControlServiceError::NotFound(_))
    ));
}

#[tokio::test]
async fn lifecycle_service_hides_sandboxes_from_other_owners() {
    let harness = TestHarness::new();
    let created = harness
        .service
        .create(create_request("owner-1"))
        .await
        .unwrap();
    let sandbox_id = created.record.sandbox_id().clone();

    assert!(matches!(
        harness.service.get("owner-2", &sandbox_id).await,
        Err(ControlServiceError::NotFound(_))
    ));
    assert!(!harness.service.kill("owner-2", &sandbox_id).await.unwrap());
    let page = harness
        .service
        .list(&SandboxListFilter {
            owner_id: "owner-2".to_string(),
            metadata: BTreeMap::new(),
            states: BTreeSet::new(),
            limit: NonZeroU32::new(100).unwrap(),
            after: None,
        })
        .await
        .unwrap();
    assert!(page.records.is_empty());
}

#[tokio::test]
async fn failed_runtime_create_is_not_published() {
    let harness = TestHarness::new();
    harness.executions.fail_create();

    assert!(matches!(
        harness.service.create(create_request("owner-1")).await,
        Err(ControlServiceError::Execution(_))
    ));
    let sandbox_id = SandboxId::new("sandbox-1").unwrap();
    assert!(matches!(
        harness.service.get("owner-1", &sandbox_id).await,
        Err(ControlServiceError::NotFound(_))
    ));
}

#[tokio::test]
async fn runtime_envd_is_ready_before_the_sandbox_is_published() {
    let harness = TestHarness::new();
    let mut request = create_request("owner-1");
    request.template_id = "runtime-envd-template".to_string();

    let created = harness.service.create(request).await.unwrap();

    assert_eq!(created.record.state(), LifecycleState::Running);
    assert_eq!(created.record.envd_mode(), EnvdMode::Runtime);
    assert_eq!(
        harness.executions.port_requests(),
        vec![(
            "execution-operation-1".to_string(),
            1,
            crate::routing::ENVD_PORT,
        )]
    );
    let requests = harness.executions.runtime_envd_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, "POST");
    assert_eq!(requests[0].1, "/init");
    assert_eq!(requests[0].2["lifecycleID"], "sandbox-1");
    assert_eq!(requests[0].2["defaultUser"], "user");
    assert_eq!(requests[0].2["envVars"]["ALPHA"], "one");
    assert_eq!(requests[0].2["envVars"]["BETA"], "two");
    assert_eq!(requests[0].2["timestamp"], "2026-07-14T12:00:00Z");
    assert!(requests[0].2.get("accessToken").is_none());
}

#[tokio::test]
async fn permanent_runtime_envd_failure_stops_and_hides_the_execution() {
    let harness = TestHarness::new();
    harness.executions.fail_ports();
    let mut request = create_request("owner-1");
    request.template_id = "runtime-envd-template".to_string();

    assert!(matches!(
        harness.service.create(request).await,
        Err(ControlServiceError::Execution(
            ExecutionManagerError::InvalidRequest(_)
        ))
    ));
    assert_eq!(
        harness.executions.port_requests(),
        vec![(
            "execution-operation-1".to_string(),
            1,
            crate::routing::ENVD_PORT,
        )]
    );
    assert_eq!(
        harness.executions.execution_state("execution-operation-1"),
        Some(ExecutionState::Stopped)
    );
    let sandbox_id = SandboxId::new("sandbox-1").unwrap();
    assert!(matches!(
        harness.service.get("owner-1", &sandbox_id).await,
        Err(ControlServiceError::NotFound(_))
    ));
}

#[tokio::test]
async fn rejected_runtime_envd_initialization_stops_and_hides_the_execution() {
    let harness = TestHarness::new();
    harness.executions.fail_runtime_envd_init();
    let mut request = create_request("owner-1");
    request.template_id = "runtime-envd-template".to_string();

    assert!(matches!(
        harness.service.create(request).await,
        Err(ControlServiceError::Execution(
            ExecutionManagerError::Internal(message)
        )) if message.contains("HTTP 400 Bad Request")
    ));
    assert_eq!(harness.executions.runtime_envd_requests().len(), 1);
    assert_eq!(
        harness.executions.execution_state("execution-operation-1"),
        Some(ExecutionState::Stopped)
    );
    let sandbox_id = SandboxId::new("sandbox-1").unwrap();
    assert!(matches!(
        harness.service.get("owner-1", &sandbox_id).await,
        Err(ControlServiceError::NotFound(_))
    ));
}
