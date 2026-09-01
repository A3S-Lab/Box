use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use a3s_box_core::{
    BoxConfig, CreateExecutionRequest, ExecutionId, ExecutionIsolation, ExecutionManager,
    ExecutionManagerError, ExecutionManagerResult, ExecutionState, KillOutcome, OperationId,
};
use async_trait::async_trait;

use super::record::build_managed_record;
use super::{
    LocalExecutionBackend, LocalExecutionBackendRouter, LocalExecutionHandle,
    LocalExecutionManager, LocalExecutionObservation, OciMigrationPolicy,
};
use crate::{BoxRecord, ManagedRuntimeRoute};

#[derive(Default)]
struct RoutedProbeBackend {
    fail_isolation_preflight: AtomicBool,
    fail_preflight: AtomicBool,
    isolation_preflights: AtomicUsize,
    preflights: AtomicUsize,
    kills: AtomicUsize,
    observed_isolations: Mutex<Vec<ExecutionIsolation>>,
    observed_routes: Mutex<Vec<ManagedRuntimeRoute>>,
}

impl RoutedProbeBackend {
    fn reject_isolation_preflight(&self) {
        self.fail_isolation_preflight.store(true, Ordering::Relaxed);
    }

    fn reject_preflight(&self) {
        self.fail_preflight.store(true, Ordering::Relaxed);
    }

    fn preflights(&self) -> usize {
        self.preflights.load(Ordering::Relaxed)
    }

    fn isolation_preflights(&self) -> usize {
        self.isolation_preflights.load(Ordering::Relaxed)
    }

    fn kills(&self) -> usize {
        self.kills.load(Ordering::Relaxed)
    }

    fn observed_routes(&self) -> Vec<ManagedRuntimeRoute> {
        self.observed_routes.lock().unwrap().clone()
    }

    fn observed_isolations(&self) -> Vec<ExecutionIsolation> {
        self.observed_isolations.lock().unwrap().clone()
    }
}

#[async_trait]
impl LocalExecutionBackend for RoutedProbeBackend {
    async fn preflight_isolation(
        &self,
        isolation: ExecutionIsolation,
    ) -> ExecutionManagerResult<()> {
        self.isolation_preflights.fetch_add(1, Ordering::Relaxed);
        self.observed_isolations.lock().unwrap().push(isolation);
        if self.fail_isolation_preflight.load(Ordering::Relaxed) {
            Err(ExecutionManagerError::Unavailable(
                "selected isolation is unavailable".to_string(),
            ))
        } else {
            Ok(())
        }
    }

    async fn preflight(&self, record: &BoxRecord) -> ExecutionManagerResult<()> {
        self.preflights.fetch_add(1, Ordering::Relaxed);
        self.observed_routes.lock().unwrap().push(
            record
                .managed_execution
                .as_ref()
                .expect("managed metadata")
                .runtime_route,
        );
        if self.fail_preflight.load(Ordering::Relaxed) {
            Err(ExecutionManagerError::Unavailable(
                "selected backend is unavailable".to_string(),
            ))
        } else {
            Ok(())
        }
    }

    async fn start(&self, _record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
        Err(ExecutionManagerError::Unavailable(
            "probe backend does not launch".to_string(),
        ))
    }

    async fn inspect(
        &self,
        _record: &BoxRecord,
    ) -> ExecutionManagerResult<LocalExecutionObservation> {
        Ok(LocalExecutionObservation {
            state: ExecutionState::Created,
            handle: None,
            exit_code: None,
        })
    }

    async fn pause(
        &self,
        _record: &BoxRecord,
        _keep_memory: bool,
    ) -> ExecutionManagerResult<LocalExecutionHandle> {
        Err(ExecutionManagerError::Unavailable(
            "probe backend does not pause".to_string(),
        ))
    }

    async fn resume(&self, _record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
        Err(ExecutionManagerError::Unavailable(
            "probe backend does not resume".to_string(),
        ))
    }

    async fn kill(&self, _record: &BoxRecord) -> ExecutionManagerResult<KillOutcome> {
        self.kills.fetch_add(1, Ordering::Relaxed);
        Ok(KillOutcome::Killed)
    }
}

#[tokio::test]
async fn selected_oci_isolation_preflight_fails_closed_without_fallback_or_state() {
    let temporary = tempfile::tempdir().unwrap();
    let state_path = temporary.path().join("boxes.json");
    let legacy = Arc::new(RoutedProbeBackend::default());
    let oci = Arc::new(RoutedProbeBackend::default());
    oci.reject_isolation_preflight();
    let manager = LocalExecutionManager::new(
        &state_path,
        temporary.path(),
        Arc::new(LocalExecutionBackendRouter::new(
            legacy.clone(),
            oci.clone(),
            OciMigrationPolicy::AllViaOci,
        )),
    );

    let error = manager
        .preflight_isolation(ExecutionIsolation::Microvm)
        .await
        .expect_err("selected OCI isolation must fail closed");

    assert!(matches!(error, ExecutionManagerError::Unavailable(_)));
    assert_eq!(legacy.isolation_preflights(), 0);
    assert_eq!(oci.isolation_preflights(), 1);
    assert_eq!(oci.observed_isolations(), vec![ExecutionIsolation::Microvm]);
    assert!(!state_path.exists());
}

#[tokio::test]
async fn sandbox_policy_stamps_oci_route_before_preflight_and_reservation() {
    let temporary = tempfile::tempdir().unwrap();
    let legacy = Arc::new(RoutedProbeBackend::default());
    let oci = Arc::new(RoutedProbeBackend::default());
    let router = LocalExecutionBackendRouter::new(
        legacy.clone(),
        oci.clone(),
        OciMigrationPolicy::SandboxViaOci,
    );
    let manager = LocalExecutionManager::new(
        temporary.path().join("boxes.json"),
        temporary.path(),
        Arc::new(router),
    );

    let reservation = manager
        .create(
            request(ExecutionIsolation::Sandbox),
            &OperationId::new("route-sandbox").unwrap(),
        )
        .await
        .unwrap();
    let record = manager
        .get(&reservation.execution_id)
        .await
        .unwrap()
        .expect("reserved record");

    assert_eq!(
        record.managed_execution.unwrap().runtime_route,
        ManagedRuntimeRoute::OciSdk
    );
    assert_eq!(legacy.preflights(), 0);
    assert_eq!(oci.preflights(), 1);
    assert_eq!(oci.observed_routes(), vec![ManagedRuntimeRoute::OciSdk]);
}

#[tokio::test]
async fn sandbox_policy_keeps_microvm_on_the_persisted_legacy_route() {
    let temporary = tempfile::tempdir().unwrap();
    let legacy = Arc::new(RoutedProbeBackend::default());
    let oci = Arc::new(RoutedProbeBackend::default());
    let manager = LocalExecutionManager::new(
        temporary.path().join("boxes.json"),
        temporary.path(),
        Arc::new(LocalExecutionBackendRouter::new(
            legacy.clone(),
            oci.clone(),
            OciMigrationPolicy::SandboxViaOci,
        )),
    );

    let reservation = manager
        .create(
            request(ExecutionIsolation::Microvm),
            &OperationId::new("route-microvm").unwrap(),
        )
        .await
        .unwrap();
    let record = manager
        .get(&reservation.execution_id)
        .await
        .unwrap()
        .expect("reserved record");

    assert_eq!(
        record.managed_execution.unwrap().runtime_route,
        ManagedRuntimeRoute::BoxVm
    );
    assert_eq!(legacy.preflights(), 1);
    assert_eq!(oci.preflights(), 0);
}

#[tokio::test]
async fn selected_oci_preflight_failure_never_falls_back_or_reserves() {
    let temporary = tempfile::tempdir().unwrap();
    let state_path = temporary.path().join("boxes.json");
    let legacy = Arc::new(RoutedProbeBackend::default());
    let oci = Arc::new(RoutedProbeBackend::default());
    oci.reject_preflight();
    let manager = LocalExecutionManager::new(
        &state_path,
        temporary.path(),
        Arc::new(LocalExecutionBackendRouter::new(
            legacy.clone(),
            oci.clone(),
            OciMigrationPolicy::AllViaOci,
        )),
    );

    let error = manager
        .create(
            request(ExecutionIsolation::Microvm),
            &OperationId::new("route-no-fallback").unwrap(),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, ExecutionManagerError::Unavailable(_)));
    assert_eq!(legacy.preflights(), 0);
    assert_eq!(oci.preflights(), 1);
    assert!(!state_path.exists());
}

#[tokio::test]
async fn persisted_routes_ignore_a_later_policy_change() {
    let temporary = tempfile::tempdir().unwrap();
    let legacy = Arc::new(RoutedProbeBackend::default());
    let oci = Arc::new(RoutedProbeBackend::default());
    let all_oci = LocalExecutionBackendRouter::new(
        legacy.clone(),
        oci.clone(),
        OciMigrationPolicy::AllViaOci,
    );
    let legacy_only = LocalExecutionBackendRouter::new(
        legacy.clone(),
        oci.clone(),
        OciMigrationPolicy::LegacyOnly,
    );
    let mut legacy_record = record(temporary.path(), ExecutionIsolation::Sandbox, "legacy");
    legacy_record
        .managed_execution
        .as_mut()
        .unwrap()
        .runtime_route = ManagedRuntimeRoute::BoxVm;
    let mut oci_record = record(temporary.path(), ExecutionIsolation::Microvm, "oci");
    oci_record.managed_execution.as_mut().unwrap().runtime_route = ManagedRuntimeRoute::OciSdk;

    all_oci.kill(&legacy_record).await.unwrap();
    legacy_only.kill(&oci_record).await.unwrap();

    assert_eq!(legacy.kills(), 1);
    assert_eq!(oci.kills(), 1);
}

#[tokio::test]
async fn pre_routing_records_use_durable_runtime_evidence_not_current_policy() {
    let temporary = tempfile::tempdir().unwrap();
    let legacy = Arc::new(RoutedProbeBackend::default());
    let oci = Arc::new(RoutedProbeBackend::default());
    let router = LocalExecutionBackendRouter::new(
        legacy.clone(),
        oci.clone(),
        OciMigrationPolicy::AllViaOci,
    );
    let legacy_record = record(temporary.path(), ExecutionIsolation::Sandbox, "old-legacy");
    let mut stopped_oci_record = record(temporary.path(), ExecutionIsolation::Sandbox, "old-oci");
    stopped_oci_record.exec_socket_path.clear();

    router.kill(&legacy_record).await.unwrap();
    router.kill(&stopped_oci_record).await.unwrap();

    assert_eq!(legacy.kills(), 1);
    assert_eq!(oci.kills(), 1);
}

fn request(isolation: ExecutionIsolation) -> CreateExecutionRequest {
    CreateExecutionRequest {
        external_sandbox_id: format!("router-{isolation:?}"),
        config: BoxConfig {
            image: "alpine:latest".to_string(),
            isolation,
            ..Default::default()
        },
        labels: Default::default(),
        policy: Default::default(),
        rootfs_snapshot_id: None,
    }
}

fn record(home_dir: &Path, isolation: ExecutionIsolation, operation_suffix: &str) -> BoxRecord {
    build_managed_record(
        home_dir,
        &ExecutionId::new(format!("00000000-0000-4000-8000-{operation_suffix:0>12}")).unwrap(),
        OperationId::new(format!("route-{operation_suffix}")).unwrap(),
        request(isolation),
        chrono::Utc::now(),
    )
    .unwrap()
}
