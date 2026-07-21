use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;

use a3s_box_core::{ExecutionManagerError, ExecutionState};
use async_trait::async_trait;
use chrono::Duration;

use super::test_support::{
    assert_sandbox_request, create_request, test_time, AdvancingClock, TestHarness,
};
use super::*;
use crate::volume::{ResolvedVolumeMount, VolumeMount, VolumeMountResolver, VolumeServiceResult};

struct TestVolumeMountResolver;

#[async_trait]
impl VolumeMountResolver for TestVolumeMountResolver {
    async fn resolve_mounts(
        &self,
        owner_id: &str,
        mounts: &[VolumeMount],
    ) -> VolumeServiceResult<Vec<ResolvedVolumeMount>> {
        assert_eq!(owner_id, "owner-1");
        assert_eq!(mounts, &[VolumeMount::new("data", "/mnt/data").unwrap()]);
        Ok(vec![ResolvedVolumeMount {
            public: mounts[0].clone(),
            runtime_name: "e2b-internal-volume".to_string(),
            host_path: PathBuf::from("/var/lib/a3s/volumes/e2b-internal-volume"),
        }])
    }
}

#[tokio::test]
async fn typed_volume_mounts_reach_runtime_policy_and_public_records() {
    let harness = TestHarness::new();
    let service = harness
        .service
        .as_ref()
        .clone()
        .with_volume_mount_resolver(Arc::new(TestVolumeMountResolver));
    let mount = VolumeMount::new("data", "/mnt/data").unwrap();

    let created = service
        .create_with_mounts(create_request("owner-1"), vec![mount.clone()])
        .await
        .unwrap();

    assert_eq!(created.record.volume_mounts(), &[mount]);
    let requests = harness.executions.requests();
    assert_eq!(
        requests[0].config.volumes,
        vec!["/var/lib/a3s/volumes/e2b-internal-volume:/mnt/data:rw"]
    );
    assert_eq!(requests[0].policy.volume_names, vec!["e2b-internal-volume"]);
}

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
        test_time() + Duration::seconds(321)
    );

    harness
        .service
        .pause("owner-1", &sandbox_id, true)
        .await
        .unwrap();
    let paused = harness.service.get("owner-1", &sandbox_id).await.unwrap();
    assert_eq!(paused.state(), LifecycleState::Paused);
    assert_eq!(paused.execution_generation().unwrap().get(), 2);
    assert!(matches!(
        harness.service.pause("owner-1", &sandbox_id, true).await,
        Err(ControlServiceError::Conflict(_))
    ));

    let resumed = harness
        .service
        .resume("owner-1", &sandbox_id, 600, true)
        .await
        .unwrap();
    assert_eq!(resumed.disposition, ConnectionDisposition::Resumed);
    assert_eq!(resumed.record.state(), LifecycleState::Running);
    assert_eq!(resumed.record.execution_generation().unwrap().get(), 3);
    assert_eq!(
        resumed.record.expires_at(),
        test_time() + Duration::seconds(600)
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

    let generation = harness
        .service
        .get("owner-1", &sandbox_id)
        .await
        .unwrap()
        .generation();
    harness
        .service
        .refresh_timeout("owner-1", &sandbox_id, 60)
        .await
        .unwrap();
    let unchanged = harness.service.get("owner-1", &sandbox_id).await.unwrap();
    assert_eq!(unchanged.expires_at(), test_time() + Duration::seconds(123));
    assert_eq!(unchanged.generation(), generation);

    harness
        .service
        .refresh_timeout("owner-1", &sandbox_id, 600)
        .await
        .unwrap();
    let refreshed = harness.service.get("owner-1", &sandbox_id).await.unwrap();
    assert_eq!(refreshed.expires_at(), test_time() + Duration::seconds(600));
    assert!(refreshed.generation() > generation);
    assert!(matches!(
        harness
            .service
            .refresh_timeout("owner-2", &sandbox_id, 900)
            .await,
        Err(ControlServiceError::NotFound(_))
    ));

    assert!(harness.service.kill("owner-1", &sandbox_id).await.unwrap());
    assert!(!harness.service.kill("owner-1", &sandbox_id).await.unwrap());
    assert!(matches!(
        harness.service.connect("owner-1", &sandbox_id, 300).await,
        Err(ControlServiceError::NotFound(_))
    ));
}

#[tokio::test]
async fn cold_start_gets_the_full_usable_timeout_after_readiness() {
    let ready_at = test_time() + Duration::seconds(120);
    let clock = Arc::new(AdvancingClock::new(test_time(), ready_at));
    let harness = TestHarness::with_clock(clock);
    let mut request = create_request("owner-1");
    request.timeout_seconds = 60;
    request.lifecycle.on_timeout = OnTimeoutAction::Kill;

    let created = harness.service.create(request).await.unwrap();

    assert_eq!(created.record.started_at(), Some(ready_at));
    assert_eq!(created.record.resources().timeout, 60);
    assert_eq!(
        created.record.expires_at(),
        ready_at + Duration::seconds(60)
    );

    let supervisor = LifecycleSupervisor::new(LifecycleSupervisorDependencies {
        repository: harness.repository.clone(),
        executions: harness.executions.clone(),
        ports: harness.executions.clone(),
        clock: harness.clock.clone(),
    });
    let report = supervisor
        .reap_expired(NonZeroU32::new(10).unwrap())
        .await
        .unwrap();
    assert_eq!(report.examined, 0);
    assert_eq!(
        harness
            .service
            .get("owner-1", created.record.sandbox_id())
            .await
            .unwrap()
            .state(),
        LifecycleState::Running
    );
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
async fn filesystem_only_resume_reinitializes_runtime_envd_with_persisted_environment() {
    let harness = TestHarness::new();
    let mut request = create_request("owner-1");
    request.template_id = "runtime-envd-template".to_string();
    let created = harness.service.create(request).await.unwrap();
    let sandbox_id = created.record.sandbox_id().clone();

    harness
        .service
        .pause("owner-1", &sandbox_id, false)
        .await
        .unwrap();
    let paused = harness.service.get("owner-1", &sandbox_id).await.unwrap();
    assert_eq!(paused.state(), LifecycleState::Paused);
    assert!(!paused.paused_with_memory());
    assert_eq!(paused.runtime_env_vars()["ALPHA"], "one");

    let resumed = harness
        .service
        .connect("owner-1", &sandbox_id, 600)
        .await
        .unwrap();

    assert_eq!(resumed.disposition, ConnectionDisposition::Resumed);
    assert!(resumed.record.paused_with_memory());
    assert_eq!(resumed.record.execution_generation().unwrap().get(), 3);
    assert_eq!(
        harness.executions.port_requests(),
        vec![
            (
                "execution-operation-1".to_string(),
                1,
                crate::routing::ENVD_PORT,
            ),
            (
                "execution-operation-1".to_string(),
                3,
                crate::routing::ENVD_PORT,
            ),
        ]
    );
    let requests = harness.executions.runtime_envd_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].2["lifecycleID"], "sandbox-1");
    assert_eq!(requests[1].2["envVars"]["ALPHA"], "one");
    assert_eq!(requests[1].2["envVars"]["BETA"], "two");
}

#[tokio::test]
async fn runtime_envd_metrics_are_generation_fenced_and_typed() {
    let harness = TestHarness::new();
    let mut request = create_request("owner-1");
    request.template_id = "runtime-envd-template".to_string();
    let created = harness.service.create(request).await.unwrap();

    let metric = harness
        .service
        .current_metric("owner-1", created.record.sandbox_id())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(metric.timestamp, test_time());
    assert_eq!(metric.cpu_count, 2);
    assert_eq!(metric.cpu_used_pct, 12.5);
    assert_eq!(metric.mem_used, 134_217_728);
    assert_eq!(metric.mem_total, 536_870_912);
    assert_eq!(metric.mem_cache, 0);
    assert_eq!(metric.disk_used, 268_435_456);
    assert_eq!(metric.disk_total, 1_073_741_824);
    assert_eq!(
        harness.executions.port_requests(),
        vec![
            (
                "execution-operation-1".to_string(),
                1,
                crate::routing::ENVD_PORT,
            ),
            (
                "execution-operation-1".to_string(),
                1,
                crate::routing::ENVD_PORT,
            ),
        ]
    );
    let requests = harness.executions.runtime_envd_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1],
        (
            "GET".to_string(),
            "/metrics".to_string(),
            serde_json::Value::Null
        )
    );
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
