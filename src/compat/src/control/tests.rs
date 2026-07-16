use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;

use a3s_box_core::{
    resolve_execution, BoxConfig, ExecutionGeneration, ExecutionId, ExecutionLease,
    ExecutionManagerError, ExecutionManagerResult, ExecutionState, ExecutionStatus, KillOutcome,
    OperationId, ReconcileOutcome,
};
use async_trait::async_trait;
use chrono::{DateTime, Duration, TimeZone, Utc};

use super::*;

fn instant(second: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 14, 12, 0, second)
        .single()
        .unwrap()
}

fn stored_token(marker: u8) -> StoredToken {
    StoredToken::new(1, vec![marker, 1], vec![marker, 2]).unwrap()
}

fn creating_record(id: &str) -> SandboxRecord {
    creating_record_with_timeout_action(id, OnTimeoutAction::Kill)
}

fn creating_record_with_timeout_action(id: &str, on_timeout: OnTimeoutAction) -> SandboxRecord {
    let config = BoxConfig {
        isolation: a3s_box_core::ExecutionIsolation::Sandbox,
        ..BoxConfig::default()
    };
    let plan = resolve_execution(&config).unwrap();
    SandboxRecord::creating(NewSandboxRecord {
        sandbox_id: SandboxId::new(id).unwrap(),
        operation_id: OperationId::new(format!("operation-{id}")).unwrap(),
        owner_id: "fixture-client".to_string(),
        template_id: "code-interpreter-v1".to_string(),
        plan,
        resources: config.resources,
        lifecycle: LifecyclePolicy {
            on_timeout,
            auto_resume: false,
            keep_memory_on_pause: false,
        },
        created_at: instant(0),
        expires_at: instant(0) + Duration::seconds(300),
        metadata: BTreeMap::from([("team".to_string(), "fixture".to_string())]),
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
    .unwrap()
}

#[tokio::test]
async fn memory_repository_claims_only_actionable_expired_records() {
    let repository = MemorySandboxRepository::default();

    let mut kill = creating_record("kill");
    kill.mark_running(execution_lease(&kill, 1)).unwrap();
    kill.replace_expiry(instant(10)).unwrap();
    repository.insert(kill).await.unwrap();

    let mut pause = creating_record_with_timeout_action("pause", OnTimeoutAction::Pause);
    pause.mark_running(execution_lease(&pause, 1)).unwrap();
    pause.replace_expiry(instant(10)).unwrap();
    repository.insert(pause).await.unwrap();

    let mut already_paused =
        creating_record_with_timeout_action("already-paused", OnTimeoutAction::Pause);
    already_paused
        .mark_running(execution_lease(&already_paused, 1))
        .unwrap();
    already_paused.begin_pause().unwrap();
    already_paused
        .mark_paused(execution_lease(&already_paused, 2))
        .unwrap();
    already_paused.replace_expiry(instant(10)).unwrap();
    repository.insert(already_paused).await.unwrap();

    let mut renewed = creating_record("renewed");
    renewed.mark_running(execution_lease(&renewed, 1)).unwrap();
    renewed.replace_expiry(instant(30)).unwrap();
    repository.insert(renewed).await.unwrap();

    let claimed = repository
        .claim_expired(instant(20), NonZeroU32::new(10).unwrap())
        .await
        .unwrap();
    let states = claimed
        .iter()
        .map(|record| (record.sandbox_id().as_str(), record.state()))
        .collect::<BTreeMap<_, _>>();

    assert_eq!(states.len(), 2);
    assert_eq!(states["kill"], LifecycleState::Killing);
    assert_eq!(states["pause"], LifecycleState::Pausing);
    assert_eq!(
        repository
            .get(&SandboxId::new("already-paused").unwrap())
            .await
            .unwrap()
            .unwrap()
            .state(),
        LifecycleState::Paused
    );
    assert_eq!(
        repository
            .get(&SandboxId::new("renewed").unwrap())
            .await
            .unwrap()
            .unwrap()
            .state(),
        LifecycleState::Running
    );
}

#[tokio::test]
async fn memory_repository_pages_reconcilable_records() {
    let repository = MemorySandboxRepository::default();
    for id in ["sandbox-1", "sandbox-2", "sandbox-3"] {
        repository.insert(creating_record(id)).await.unwrap();
    }
    let mut terminal = creating_record("terminal");
    terminal
        .mark_failed(LifecycleFailure::RuntimeFailed)
        .unwrap();
    repository.insert(terminal).await.unwrap();

    let first = repository
        .list_reconcilable(None, NonZeroU32::new(2).unwrap())
        .await
        .unwrap();
    assert_eq!(first.records.len(), 2);
    assert!(first.next.is_some());

    let second = repository
        .list_reconcilable(first.next.as_ref(), NonZeroU32::new(2).unwrap())
        .await
        .unwrap();
    assert_eq!(second.records.len(), 1);
    assert_eq!(second.records[0].sandbox_id().as_str(), "sandbox-3");
    assert!(second.next.is_none());
}

fn execution_lease(record: &SandboxRecord, generation: u64) -> ExecutionLease {
    ExecutionLease {
        execution_id: ExecutionId::new("execution-1").unwrap(),
        generation: ExecutionGeneration::new(generation).unwrap(),
        plan: record.plan().clone(),
        resources: record.resources().clone(),
        started_at: instant(1),
    }
}

#[test]
fn lifecycle_transitions_are_generation_fenced() {
    let mut record = creating_record("sandbox-1");
    assert_eq!(record.generation(), SandboxGeneration::INITIAL);
    assert_eq!(record.public_state(), None);

    assert_eq!(
        record.mark_running(execution_lease(&record, 1)).unwrap(),
        SandboxGeneration::new(2).unwrap()
    );
    assert_eq!(record.public_state(), Some(PublicSandboxState::Running));
    let started_at = record.started_at();

    record
        .replace_expiry(instant(0) + Duration::seconds(600))
        .unwrap();
    record.begin_pause().unwrap();
    record.mark_paused(execution_lease(&record, 2)).unwrap();
    assert_eq!(record.public_state(), Some(PublicSandboxState::Paused));
    record.begin_resume().unwrap();
    record.mark_running(execution_lease(&record, 3)).unwrap();

    assert_eq!(record.generation(), SandboxGeneration::new(7).unwrap());
    assert_eq!(record.started_at(), started_at);
    assert_eq!(record.execution_generation().unwrap().get(), 3);
}

#[test]
fn pause_and_resume_reject_stale_execution_generations() {
    let mut record = creating_record("sandbox-1");
    record.mark_running(execution_lease(&record, 1)).unwrap();
    record.begin_pause().unwrap();

    assert_eq!(
        record.mark_paused(execution_lease(&record, 1)).unwrap_err(),
        LifecycleError::ExecutionGenerationMismatch
    );
    assert_eq!(record.state(), LifecycleState::Pausing);

    record.mark_paused(execution_lease(&record, 2)).unwrap();
    record.begin_resume().unwrap();
    assert_eq!(
        record
            .mark_running(execution_lease(&record, 2))
            .unwrap_err(),
        LifecycleError::ExecutionGenerationMismatch
    );
    assert_eq!(record.state(), LifecycleState::Resuming);
}

#[test]
fn failed_pause_and_resume_attempts_restore_the_stable_state() {
    let mut record = creating_record("sandbox-1");
    record.mark_running(execution_lease(&record, 1)).unwrap();
    let running_generation = record.generation();

    record.begin_pause().unwrap();
    record.abort_pause().unwrap();
    assert_eq!(record.state(), LifecycleState::Running);
    assert!(record.generation() > running_generation);
    assert_eq!(record.execution_generation().unwrap().get(), 1);

    record.begin_pause().unwrap();
    record.mark_paused(execution_lease(&record, 2)).unwrap();
    record.begin_resume().unwrap();
    record.abort_resume().unwrap();
    assert_eq!(record.state(), LifecycleState::Paused);
    assert_eq!(record.execution_generation().unwrap().get(), 2);
}

#[test]
fn invalid_transition_does_not_mutate_record() {
    let mut record = creating_record("sandbox-1");
    let generation = record.generation();

    let error = record.begin_resume().unwrap_err();

    assert_eq!(error, LifecycleError::MissingExecution);
    assert_eq!(record.state(), LifecycleState::Creating);
    assert_eq!(record.generation(), generation);
}

#[test]
fn persisted_control_identifiers_preserve_invariants() {
    assert!(serde_json::from_str::<SandboxId>("\"\"").is_err());
    for invalid in [
        "Uppercase",
        "-leading",
        "trailing-",
        "contains.dot",
        "contains/slash",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ] {
        assert!(SandboxId::new(invalid).is_err(), "accepted {invalid}");
    }
    assert!(serde_json::from_str::<SandboxGeneration>("0").is_err());
    assert!(serde_json::from_str::<StoredToken>(
        r#"{"key_version":0,"ciphertext":[],"digest":[]}"#
    )
    .is_err());
}

#[test]
fn records_written_before_route_policies_and_envd_modes_use_broker_defaults() {
    let record = creating_record("legacy-route-record");
    let mut value = serde_json::to_value(record).unwrap();
    value.as_object_mut().unwrap().remove("routing");
    value.as_object_mut().unwrap().remove("envd_mode");

    let restored: SandboxRecord = serde_json::from_value(value).unwrap();
    assert_eq!(restored.envd_mode(), EnvdMode::Broker);
    assert_eq!(
        restored.routing().token_scope(crate::routing::ENVD_PORT),
        Some(TokenScope::Envd)
    );
    assert_eq!(restored.routing().ports().count(), 1);
}

#[test]
fn runtime_plan_mismatch_does_not_publish_execution() {
    let mut record = creating_record("sandbox-1");
    let config = BoxConfig::default();
    let mut lease = execution_lease(&record, 1);
    lease.plan = resolve_execution(&config).unwrap();

    assert_eq!(
        record.mark_running(lease).unwrap_err(),
        LifecycleError::ExecutionPlanMismatch
    );
    assert_eq!(record.state(), LifecycleState::Creating);
    assert_eq!(record.execution_id(), None);
    assert_eq!(record.generation(), SandboxGeneration::INITIAL);
}

#[test]
fn killed_record_is_terminal() {
    let mut record = creating_record("sandbox-1");
    record.mark_running(execution_lease(&record, 1)).unwrap();
    record.begin_kill().unwrap();
    record.mark_killed().unwrap();

    assert!(record.is_terminal());
    assert!(matches!(
        record.begin_kill(),
        Err(LifecycleError::InvalidTransition { .. })
    ));
}

#[tokio::test]
async fn repository_compare_and_swap_rejects_stale_generation() {
    let repository = MemorySandboxRepository::default();
    let mut original = creating_record("sandbox-1");
    repository.insert(original.clone()).await.unwrap();
    let stale_generation = original.generation();

    original
        .mark_running(execution_lease(&original, 1))
        .unwrap();
    assert_eq!(
        repository
            .compare_and_swap(original.sandbox_id(), stale_generation, original.clone())
            .await
            .unwrap(),
        CompareAndSwapResult::Updated
    );

    let mut stale = original.clone();
    stale.replace_expiry(instant(30)).unwrap();
    let stale_id = stale.sandbox_id().clone();
    assert_eq!(
        repository
            .compare_and_swap(&stale_id, stale_generation, stale)
            .await
            .unwrap(),
        CompareAndSwapResult::Conflict {
            actual_generation: original.generation(),
        }
    );
}

#[tokio::test]
async fn repository_list_port_preserves_cursor_and_filters() {
    let repository = MemorySandboxRepository::default();
    for id in ["sandbox-1", "sandbox-2"] {
        let mut record = creating_record(id);
        record.mark_running(execution_lease(&record, 1)).unwrap();
        repository.insert(record).await.unwrap();
    }
    let first = repository
        .list(&SandboxListFilter {
            owner_id: "fixture-client".to_string(),
            metadata: BTreeMap::from([("team".to_string(), "fixture".to_string())]),
            states: BTreeSet::from([PublicSandboxState::Running]),
            limit: NonZeroU32::new(1).unwrap(),
            after: None,
        })
        .await
        .unwrap();
    assert_eq!(first.records.len(), 1);
    assert_eq!(first.records[0].sandbox_id().as_str(), "sandbox-1");

    let second = repository
        .list(&SandboxListFilter {
            owner_id: "fixture-client".to_string(),
            metadata: BTreeMap::new(),
            states: BTreeSet::new(),
            limit: NonZeroU32::new(1).unwrap(),
            after: first.next,
        })
        .await
        .unwrap();
    assert_eq!(second.records.len(), 1);
    assert_eq!(second.records[0].sandbox_id().as_str(), "sandbox-2");
    assert!(second.next.is_none());
}

struct FixedClock(DateTime<Utc>);

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}

struct FixedTokenIssuer;

#[async_trait]
impl TokenIssuer for FixedTokenIssuer {
    async fn issue(&self, scope: TokenScope) -> TokenIssuerResult<IssuedToken> {
        let marker = match scope {
            TokenScope::Envd => 1,
            TokenScope::Traffic => 2,
        };
        Ok(IssuedToken {
            secret: SecretToken::new(format!("secret-{marker}")).unwrap(),
            stored: stored_token(marker),
        })
    }
}

#[tokio::test]
async fn deterministic_ports_do_not_expose_token_material_in_debug() {
    let clock = FixedClock(instant(9));
    let issued = FixedTokenIssuer.issue(TokenScope::Envd).await.unwrap();

    assert_eq!(clock.now(), instant(9));
    assert_eq!(issued.secret.expose_secret(), "secret-1");
    let debug = format!("{issued:?}");
    assert!(!debug.contains("secret-1"));
    assert!(debug.contains("REDACTED"));
}

struct ObjectSafeExecutionManager;

#[async_trait]
impl a3s_box_core::ExecutionManager for ObjectSafeExecutionManager {
    async fn create_and_start(
        &self,
        _request: a3s_box_core::CreateExecutionRequest,
        _operation_id: &OperationId,
    ) -> ExecutionManagerResult<ExecutionLease> {
        Err(ExecutionManagerError::Unavailable("fixture".to_string()))
    }

    async fn inspect(&self, execution_id: &ExecutionId) -> ExecutionManagerResult<ExecutionStatus> {
        Ok(ExecutionStatus {
            execution_id: execution_id.clone(),
            generation: ExecutionGeneration::INITIAL,
            state: ExecutionState::Running,
            plan: creating_record("manager-check").plan().clone(),
        })
    }

    async fn pause(
        &self,
        execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
        _keep_memory: bool,
    ) -> ExecutionManagerResult<ExecutionLease> {
        Err(ExecutionManagerError::NotFound(execution_id.clone()))
    }

    async fn resume(
        &self,
        execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<ExecutionLease> {
        Err(ExecutionManagerError::NotFound(execution_id.clone()))
    }

    async fn kill(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<KillOutcome> {
        Ok(KillOutcome::AlreadyStopped)
    }

    async fn reconcile(
        &self,
        _operation_id: &OperationId,
    ) -> ExecutionManagerResult<ReconcileOutcome> {
        Ok(ReconcileOutcome::Absent)
    }
}

#[tokio::test]
async fn execution_manager_port_is_object_safe() {
    let manager: &dyn a3s_box_core::ExecutionManager = &ObjectSafeExecutionManager;
    let execution_id = ExecutionId::new("execution-1").unwrap();

    assert_eq!(
        manager.inspect(&execution_id).await.unwrap().state,
        ExecutionState::Running
    );
    assert!(matches!(
        manager
            .pause(&execution_id, ExecutionGeneration::INITIAL, true)
            .await,
        Err(ExecutionManagerError::NotFound(_))
    ));
}
