use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::Arc;

use a3s_box_core::{
    resolve_execution, BoxConfig, CreateExecutionRequest, ExecutionGeneration, ExecutionIsolation,
    ExecutionManager, ExecutionState, OperationId,
};
use chrono::{DateTime, TimeZone, Utc};
use tempfile::tempdir;

use super::test_support::RecordingExecutionManager;
use super::*;

fn instant(second: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 14, 12, 0, second)
        .single()
        .unwrap()
}

#[derive(Debug)]
struct FixedClock(DateTime<Utc>);

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}

fn stored_token(marker: u8) -> StoredToken {
    StoredToken::new(1, vec![marker, 1], vec![marker, 2]).unwrap()
}

fn creating_record(sandbox_id: &str, action: OnTimeoutAction) -> (SandboxRecord, BoxConfig) {
    let config = BoxConfig {
        isolation: ExecutionIsolation::Sandbox,
        ..BoxConfig::default()
    };
    let record = SandboxRecord::creating(NewSandboxRecord {
        sandbox_id: SandboxId::new(sandbox_id).unwrap(),
        operation_id: OperationId::new(format!("operation-{sandbox_id}")).unwrap(),
        owner_id: "owner-1".to_string(),
        template_id: "fixture-template".to_string(),
        plan: resolve_execution(&config).unwrap(),
        resources: config.resources.clone(),
        lifecycle: LifecyclePolicy {
            on_timeout: action,
            auto_resume: false,
            keep_memory_on_pause: false,
        },
        created_at: instant(0),
        expires_at: instant(10),
        metadata: BTreeMap::new(),
        envd_version: "0.1.3".to_string(),
        envd_mode: EnvdMode::Broker,
        secure: true,
        allow_internet_access: Some(false),
        credentials: SandboxCredentials {
            envd: stored_token(10),
            traffic: stored_token(20),
        },
        routing: crate::routing::SandboxRoutePolicy::default(),
    })
    .unwrap();
    (record, config)
}

async fn start_runtime(
    manager: &RecordingExecutionManager,
    record: &SandboxRecord,
    config: BoxConfig,
) -> a3s_box_core::ExecutionLease {
    manager
        .create_and_start(
            CreateExecutionRequest {
                external_sandbox_id: record.sandbox_id().to_string(),
                config,
                labels: BTreeMap::new(),
                policy: Default::default(),
            },
            record.operation_id(),
        )
        .await
        .unwrap()
}

async fn create_runtime(
    manager: &RecordingExecutionManager,
    record: &SandboxRecord,
    config: BoxConfig,
) -> a3s_box_core::ExecutionReservation {
    manager
        .create(
            CreateExecutionRequest {
                external_sandbox_id: record.sandbox_id().to_string(),
                config,
                labels: BTreeMap::new(),
                policy: Default::default(),
            },
            record.operation_id(),
        )
        .await
        .unwrap()
}

async fn assert_created_reservation_drift_is_rejected(
    sandbox_id: &str,
    record: SandboxRecord,
    runtime_config: BoxConfig,
    expected_failure: &str,
) {
    let repository = Arc::new(MemorySandboxRepository::default());
    let clock: Arc<dyn Clock> = Arc::new(FixedClock(instant(20)));
    let executions = Arc::new(RecordingExecutionManager::new(clock.clone()));
    repository.insert(record.clone()).await.unwrap();
    let reservation = create_runtime(&executions, &record, runtime_config).await;

    let report = supervisor(repository, executions.clone(), clock)
        .reconcile_startup(NonZeroU32::new(10).unwrap())
        .await
        .unwrap();

    assert_eq!(report.examined, 1, "{sandbox_id}");
    assert_eq!(report.completed, 0, "{sandbox_id}");
    assert_eq!(report.failures.len(), 1, "{sandbox_id}");
    assert!(
        report.failures[0].message.contains(expected_failure),
        "unexpected reconciliation failure for {sandbox_id}: {}",
        report.failures[0].message
    );
    assert_eq!(
        executions
            .inspect(&reservation.execution_id)
            .await
            .unwrap()
            .state,
        ExecutionState::Created,
        "reconciliation must reject drift before starting {sandbox_id}"
    );
}

fn supervisor(
    repository: Arc<dyn SandboxRepository>,
    executions: Arc<RecordingExecutionManager>,
    clock: Arc<dyn Clock>,
) -> LifecycleSupervisor {
    LifecycleSupervisor::new(LifecycleSupervisorDependencies {
        repository,
        executions,
        clock,
    })
}

#[tokio::test]
async fn reaper_completes_generation_fenced_kill_and_pause() {
    let repository = Arc::new(MemorySandboxRepository::default());
    let clock: Arc<dyn Clock> = Arc::new(FixedClock(instant(20)));
    let executions = Arc::new(RecordingExecutionManager::new(clock.clone()));

    for (sandbox_id, action) in [
        ("kill", OnTimeoutAction::Kill),
        ("pause", OnTimeoutAction::Pause),
    ] {
        let (mut record, config) = creating_record(sandbox_id, action);
        let lease = start_runtime(&executions, &record, config).await;
        record.mark_running(lease).unwrap();
        repository.insert(record).await.unwrap();
    }

    let report = supervisor(repository.clone(), executions.clone(), clock)
        .reap_expired(NonZeroU32::new(10).unwrap())
        .await
        .unwrap();

    assert_eq!(report.examined, 2);
    assert_eq!(report.completed, 2);
    assert_eq!(report.deferred, 0);
    assert!(report.failures.is_empty());
    assert_eq!(
        repository
            .get(&SandboxId::new("kill").unwrap())
            .await
            .unwrap()
            .unwrap()
            .state(),
        LifecycleState::Killed
    );
    let paused = repository
        .get(&SandboxId::new("pause").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(paused.state(), LifecycleState::Paused);
    assert_eq!(paused.execution_generation().unwrap().get(), 2);
    assert_eq!(
        executions
            .inspect(paused.execution_id().unwrap())
            .await
            .unwrap()
            .state,
        ExecutionState::Paused
    );
}

#[tokio::test]
async fn startup_recovers_create_committed_only_in_runtime() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("control.db");
    let repository = SqliteSandboxRepository::open(&path).await.unwrap();
    let clock: Arc<dyn Clock> = Arc::new(FixedClock(instant(20)));
    let executions = Arc::new(RecordingExecutionManager::new(clock.clone()));
    let (record, config) = creating_record("sandbox-1", OnTimeoutAction::Kill);
    repository.insert(record.clone()).await.unwrap();
    let lease = start_runtime(&executions, &record, config).await;
    drop(repository);

    let repository = Arc::new(SqliteSandboxRepository::open(&path).await.unwrap());
    let report = supervisor(repository.clone(), executions, clock)
        .reconcile_startup(NonZeroU32::new(10).unwrap())
        .await
        .unwrap();

    assert_eq!(report.examined, 1);
    assert_eq!(report.completed, 1);
    assert!(report.failures.is_empty());
    let recovered = repository.get(record.sandbox_id()).await.unwrap().unwrap();
    assert_eq!(recovered.state(), LifecycleState::Running);
    assert_eq!(recovered.execution_id(), Some(&lease.execution_id));
    assert_eq!(
        recovered.execution_generation(),
        Some(ExecutionGeneration::INITIAL)
    );
}

#[tokio::test]
async fn startup_starts_a_durable_runtime_reservation_before_publishing_running() {
    let repository = Arc::new(MemorySandboxRepository::default());
    let clock: Arc<dyn Clock> = Arc::new(FixedClock(instant(20)));
    let executions = Arc::new(RecordingExecutionManager::new(clock.clone()));
    let (record, config) = creating_record("sandbox-1", OnTimeoutAction::Kill);
    repository.insert(record.clone()).await.unwrap();
    let reservation = create_runtime(&executions, &record, config).await;

    let report = supervisor(repository.clone(), executions.clone(), clock)
        .reconcile_startup(NonZeroU32::new(10).unwrap())
        .await
        .unwrap();

    assert_eq!(report.examined, 1);
    assert_eq!(report.completed, 1);
    assert!(report.failures.is_empty());
    let recovered = repository.get(record.sandbox_id()).await.unwrap().unwrap();
    assert_eq!(recovered.state(), LifecycleState::Running);
    assert_eq!(recovered.execution_id(), Some(&reservation.execution_id));
    assert_eq!(
        executions
            .inspect(&reservation.execution_id)
            .await
            .unwrap()
            .state,
        ExecutionState::Running
    );
}

#[tokio::test]
async fn startup_rejects_created_plan_drift_before_starting_the_runtime() {
    let (record, mut runtime_config) = creating_record("plan-drift", OnTimeoutAction::Kill);
    runtime_config.isolation = ExecutionIsolation::Microvm;

    assert_created_reservation_drift_is_rejected(
        "plan-drift",
        record,
        runtime_config,
        "reservation plan differs",
    )
    .await;
}

#[tokio::test]
async fn startup_rejects_created_resource_drift_before_starting_the_runtime() {
    let (record, mut runtime_config) = creating_record("resource-drift", OnTimeoutAction::Kill);
    runtime_config.resources.memory_mb += 1;

    assert_created_reservation_drift_is_rejected(
        "resource-drift",
        record,
        runtime_config,
        "reservation resources differ",
    )
    .await;
}

#[tokio::test]
async fn startup_marks_an_absent_incomplete_runtime_as_failed() {
    let repository = Arc::new(MemorySandboxRepository::default());
    let clock: Arc<dyn Clock> = Arc::new(FixedClock(instant(20)));
    let executions = Arc::new(RecordingExecutionManager::new(clock.clone()));
    let (record, _) = creating_record("sandbox-1", OnTimeoutAction::Kill);
    repository.insert(record.clone()).await.unwrap();

    let report = supervisor(repository.clone(), executions, clock)
        .reconcile_startup(NonZeroU32::new(10).unwrap())
        .await
        .unwrap();

    assert_eq!(report.completed, 1);
    assert!(report.failures.is_empty());
    let failed = repository.get(record.sandbox_id()).await.unwrap().unwrap();
    assert_eq!(failed.state(), LifecycleState::Failed);
    assert_eq!(
        failed.failure(),
        Some(LifecycleFailure::ReconciliationFailed)
    );
}

#[tokio::test]
async fn startup_kills_a_runtime_created_after_its_record_was_claimed() {
    let repository = Arc::new(MemorySandboxRepository::default());
    let clock: Arc<dyn Clock> = Arc::new(FixedClock(instant(20)));
    let executions = Arc::new(RecordingExecutionManager::new(clock.clone()));
    let (mut record, config) = creating_record("sandbox-1", OnTimeoutAction::Kill);
    repository.insert(record.clone()).await.unwrap();
    let lease = start_runtime(&executions, &record, config).await;
    let expected = record.generation();
    record.begin_kill().unwrap();
    assert_eq!(
        repository
            .compare_and_swap(record.sandbox_id(), expected, record.clone())
            .await
            .unwrap(),
        CompareAndSwapResult::Updated
    );

    let report = supervisor(repository.clone(), executions.clone(), clock)
        .reconcile_startup(NonZeroU32::new(10).unwrap())
        .await
        .unwrap();

    assert_eq!(report.completed, 1);
    assert!(report.failures.is_empty());
    assert_eq!(
        repository
            .get(record.sandbox_id())
            .await
            .unwrap()
            .unwrap()
            .state(),
        LifecycleState::Killed
    );
    assert_eq!(
        executions.inspect(&lease.execution_id).await.unwrap().state,
        ExecutionState::Stopped
    );
}

#[tokio::test]
async fn startup_kills_a_created_reservation_without_starting_it() {
    let repository = Arc::new(MemorySandboxRepository::default());
    let clock: Arc<dyn Clock> = Arc::new(FixedClock(instant(20)));
    let executions = Arc::new(RecordingExecutionManager::new(clock.clone()));
    let (mut record, config) = creating_record("sandbox-1", OnTimeoutAction::Kill);
    let reservation = create_runtime(&executions, &record, config).await;
    record.begin_kill().unwrap();
    repository.insert(record.clone()).await.unwrap();

    let report = supervisor(repository.clone(), executions.clone(), clock)
        .reconcile_startup(NonZeroU32::new(10).unwrap())
        .await
        .unwrap();

    assert_eq!(report.completed, 1);
    assert!(report.failures.is_empty());
    assert_eq!(
        repository
            .get(record.sandbox_id())
            .await
            .unwrap()
            .unwrap()
            .state(),
        LifecycleState::Killed
    );
    assert_eq!(
        executions
            .inspect(&reservation.execution_id)
            .await
            .unwrap()
            .state,
        ExecutionState::Stopped
    );
}

#[tokio::test]
async fn startup_finishes_expiry_claims_after_service_crash() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("control.db");
    let repository = SqliteSandboxRepository::open(&path).await.unwrap();
    let clock: Arc<dyn Clock> = Arc::new(FixedClock(instant(20)));
    let executions = Arc::new(RecordingExecutionManager::new(clock.clone()));

    for (sandbox_id, action) in [
        ("kill", OnTimeoutAction::Kill),
        ("pause", OnTimeoutAction::Pause),
    ] {
        let (mut record, config) = creating_record(sandbox_id, action);
        let lease = start_runtime(&executions, &record, config).await;
        record.mark_running(lease).unwrap();
        repository.insert(record).await.unwrap();
    }
    let claimed = repository
        .claim_expired(instant(20), NonZeroU32::new(10).unwrap())
        .await
        .unwrap();
    let pausing = claimed
        .iter()
        .find(|record| record.state() == LifecycleState::Pausing)
        .unwrap();
    executions
        .pause(
            pausing.execution_id().unwrap(),
            pausing.execution_generation().unwrap(),
            false,
        )
        .await
        .unwrap();
    drop(repository);

    let repository = Arc::new(SqliteSandboxRepository::open(&path).await.unwrap());
    let report = supervisor(repository.clone(), executions, clock)
        .reconcile_startup(NonZeroU32::new(1).unwrap())
        .await
        .unwrap();

    assert_eq!(report.examined, 2);
    assert_eq!(report.completed, 2);
    assert!(report.failures.is_empty());
    assert_eq!(
        repository
            .get(&SandboxId::new("kill").unwrap())
            .await
            .unwrap()
            .unwrap()
            .state(),
        LifecycleState::Killed
    );
    let paused = repository
        .get(&SandboxId::new("pause").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(paused.state(), LifecycleState::Paused);
    assert_eq!(paused.execution_generation().unwrap().get(), 2);
}

#[tokio::test]
async fn startup_publishes_a_resume_completed_before_database_commit() {
    let repository = Arc::new(MemorySandboxRepository::default());
    let clock: Arc<dyn Clock> = Arc::new(FixedClock(instant(20)));
    let executions = Arc::new(RecordingExecutionManager::new(clock.clone()));
    let (mut record, config) = creating_record("sandbox-1", OnTimeoutAction::Pause);
    let initial = start_runtime(&executions, &record, config).await;
    record.mark_running(initial).unwrap();
    record.begin_pause().unwrap();
    let paused_lease = executions
        .pause(
            record.execution_id().unwrap(),
            record.execution_generation().unwrap(),
            false,
        )
        .await
        .unwrap();
    record.mark_paused(paused_lease).unwrap();
    record.begin_resume().unwrap();
    let resumed_lease = executions
        .resume(
            record.execution_id().unwrap(),
            record.execution_generation().unwrap(),
        )
        .await
        .unwrap();
    repository.insert(record.clone()).await.unwrap();

    let report = supervisor(repository.clone(), executions, clock)
        .reconcile_startup(NonZeroU32::new(10).unwrap())
        .await
        .unwrap();

    assert_eq!(report.completed, 1);
    assert!(report.failures.is_empty());
    let running = repository.get(record.sandbox_id()).await.unwrap().unwrap();
    assert_eq!(running.state(), LifecycleState::Running);
    assert_eq!(
        running.execution_generation(),
        Some(resumed_lease.generation)
    );
}
