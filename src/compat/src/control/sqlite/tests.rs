use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;

use a3s_box_core::{
    resolve_execution, BoxConfig, ExecutionGeneration, ExecutionId, ExecutionIsolation,
    ExecutionLease, OperationId,
};
use chrono::{DateTime, Duration, TimeZone, Utc};
use tempfile::tempdir;

use super::*;
use crate::control::{
    EnvdMode, LifecyclePolicy, NewSandboxRecord, OnTimeoutAction, PublicSandboxState,
    SandboxCredentials, StoredToken,
};

fn instant(second: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 14, 12, 0, second)
        .single()
        .unwrap()
}

fn stored_token(marker: u8) -> StoredToken {
    StoredToken::new(1, vec![marker, 1], vec![marker, 2]).unwrap()
}

fn running_record(
    sandbox_id: &str,
    owner_id: &str,
    second: u32,
    metadata: BTreeMap<String, String>,
) -> SandboxRecord {
    running_record_with_action(
        sandbox_id,
        owner_id,
        second,
        metadata,
        OnTimeoutAction::Kill,
    )
}

fn running_record_with_action(
    sandbox_id: &str,
    owner_id: &str,
    second: u32,
    metadata: BTreeMap<String, String>,
    on_timeout: OnTimeoutAction,
) -> SandboxRecord {
    running_record_at(sandbox_id, owner_id, instant(second), metadata, on_timeout)
}

fn running_record_at(
    sandbox_id: &str,
    owner_id: &str,
    created_at: DateTime<Utc>,
    metadata: BTreeMap<String, String>,
    on_timeout: OnTimeoutAction,
) -> SandboxRecord {
    let config = BoxConfig {
        isolation: ExecutionIsolation::Sandbox,
        ..BoxConfig::default()
    };
    let mut record = SandboxRecord::creating(NewSandboxRecord {
        sandbox_id: SandboxId::new(sandbox_id).unwrap(),
        operation_id: OperationId::new(format!("operation-{sandbox_id}")).unwrap(),
        owner_id: owner_id.to_string(),
        template_id: "fixture-template".to_string(),
        plan: resolve_execution(&config).unwrap(),
        resources: config.resources,
        lifecycle: LifecyclePolicy {
            on_timeout,
            auto_resume: false,
            keep_memory_on_pause: false,
        },
        created_at,
        expires_at: created_at + Duration::seconds(300),
        metadata,
        envd_version: "0.1.3".to_string(),
        envd_mode: EnvdMode::Broker,
        runtime_env_vars: BTreeMap::new(),
        secure: true,
        allow_internet_access: Some(false),
        credentials: SandboxCredentials {
            envd: stored_token(10),
            traffic: stored_token(20),
        },
        routing: crate::routing::SandboxRoutePolicy::default(),
    })
    .unwrap();
    record
        .mark_running(ExecutionLease {
            execution_id: ExecutionId::new(format!("execution-{sandbox_id}")).unwrap(),
            generation: ExecutionGeneration::INITIAL,
            plan: record.plan().clone(),
            resources: record.resources().clone(),
            started_at: created_at + Duration::seconds(1),
        })
        .unwrap();
    record
}

#[tokio::test]
async fn opens_in_wal_mode_and_applies_exact_migration_history() {
    let directory = tempdir().unwrap();
    let repository = SqliteSandboxRepository::open(directory.path().join("control.db"))
        .await
        .unwrap();

    let (journal_mode, migrations, strict, created_index, expiry_index) = repository
        .call(|connection| {
            let journal_mode = connection
                .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
                .map_err(|error| unavailable("read journal mode", error))?;
            let migrations = connection
                .query_row(
                    "SELECT group_concat(version || ':' || name, ',') \
                     FROM compatibility_schema_migrations",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .map_err(|error| unavailable("read migration history", error))?;
            let strict = connection
                .query_row(
                    "SELECT strict FROM pragma_table_list WHERE name = 'sandbox_records'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(|error| unavailable("read strict table status", error))?;
            let created_index = connection
                .query_row(
                    "SELECT sql FROM sqlite_master \
                     WHERE type = 'index' \
                       AND name = 'sandbox_records_owner_state_created'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .map_err(|error| unavailable("read creation index definition", error))?;
            let expiry_index = connection
                .query_row(
                    "SELECT sql FROM sqlite_master \
                     WHERE type = 'index' AND name = 'sandbox_records_expiry'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .map_err(|error| unavailable("read expiry index definition", error))?;
            Ok((
                journal_mode,
                migrations,
                strict,
                created_index,
                expiry_index,
            ))
        })
        .await
        .unwrap();

    assert_eq!(journal_mode, "wal");
    assert_eq!(
        migrations,
        "1:lifecycle_records,2:temporal_indexes,3:volume_records,4:snapshot_records"
    );
    assert_eq!(strict, 1);
    assert!(created_index.contains("julianday(created_at)"));
    assert!(expiry_index.contains("julianday(expires_at)"));
}

#[tokio::test]
async fn upgrades_a_version_one_repository_without_rewriting_records() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("control.db");
    let repository = SqliteSandboxRepository::open(&path).await.unwrap();
    let record = running_record("sandbox-1", "owner-1", 0, BTreeMap::new());
    repository.insert(record.clone()).await.unwrap();
    repository
        .call(|connection| {
            connection
                .execute_batch(
                    "DROP INDEX sandbox_records_owner_state_created; \
                     DROP INDEX sandbox_records_expiry; \
                     DROP INDEX sandbox_records_reconcilable; \
                     DROP TABLE snapshot_records; \
                     DROP TABLE volume_records; \
                     CREATE INDEX sandbox_records_owner_state_created \
                         ON sandbox_records(\
                             owner_id, state, created_at, sandbox_id\
                         ); \
                     CREATE INDEX sandbox_records_expiry \
                         ON sandbox_records(state, expires_at, sandbox_id); \
                     DELETE FROM compatibility_schema_migrations WHERE version >= 2;",
                )
                .map_err(|error| unavailable("downgrade migration fixture", error))?;
            Ok(())
        })
        .await
        .unwrap();
    drop(repository);

    let repository = SqliteSandboxRepository::open(&path).await.unwrap();
    assert!(repository.get(record.sandbox_id()).await.unwrap().is_some());
    let (migrations, expiry_index) = repository
        .call(|connection| {
            let migrations = connection
                .query_row(
                    "SELECT group_concat(version || ':' || name, ',') \
                     FROM compatibility_schema_migrations",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .map_err(|error| unavailable("read upgraded migration history", error))?;
            let expiry_index = connection
                .query_row(
                    "SELECT sql FROM sqlite_master \
                     WHERE type = 'index' AND name = 'sandbox_records_expiry'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .map_err(|error| unavailable("read upgraded expiry index", error))?;
            Ok((migrations, expiry_index))
        })
        .await
        .unwrap();
    assert_eq!(
        migrations,
        "1:lifecycle_records,2:temporal_indexes,3:volume_records,4:snapshot_records"
    );
    assert!(expiry_index.contains("julianday(expires_at)"));
}

#[tokio::test]
async fn records_survive_restart_and_cas_rejects_stale_writers() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("control.db");
    let repository = SqliteSandboxRepository::open(&path).await.unwrap();
    let record = running_record("sandbox-1", "owner-1", 0, BTreeMap::new());
    repository.insert(record.clone()).await.unwrap();
    drop(repository);

    let repository = SqliteSandboxRepository::open(&path).await.unwrap();
    let loaded = repository.get(record.sandbox_id()).await.unwrap().unwrap();
    assert_eq!(loaded.owner_id(), "owner-1");
    assert_eq!(loaded.execution_id(), record.execution_id());
    assert_eq!(loaded.credentials(), record.credentials());
    assert_eq!(loaded.routing(), record.routing());

    let expected = loaded.generation();
    let mut replacement = loaded.clone();
    replacement.replace_expiry(instant(30)).unwrap();
    assert_eq!(
        repository
            .compare_and_swap(record.sandbox_id(), expected, replacement.clone())
            .await
            .unwrap(),
        CompareAndSwapResult::Updated
    );

    let mut stale = loaded;
    stale.replace_expiry(instant(40)).unwrap();
    assert_eq!(
        repository
            .compare_and_swap(record.sandbox_id(), expected, stale)
            .await
            .unwrap(),
        CompareAndSwapResult::Conflict {
            actual_generation: replacement.generation(),
        }
    );
}

#[tokio::test]
async fn concurrent_cas_allows_exactly_one_writer() {
    let directory = tempdir().unwrap();
    let repository = SqliteSandboxRepository::open(directory.path().join("control.db"))
        .await
        .unwrap();
    let record = running_record("sandbox-1", "owner-1", 0, BTreeMap::new());
    repository.insert(record.clone()).await.unwrap();
    let expected = record.generation();
    let mut first = record.clone();
    first.replace_expiry(instant(30)).unwrap();
    let mut second = record.clone();
    second.replace_expiry(instant(40)).unwrap();

    let first_write = repository.compare_and_swap(record.sandbox_id(), expected, first);
    let second_write = repository.compare_and_swap(record.sandbox_id(), expected, second);
    let (first_result, second_result) = tokio::join!(first_write, second_write);
    let results = [first_result.unwrap(), second_result.unwrap()];

    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, CompareAndSwapResult::Updated))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, CompareAndSwapResult::Conflict { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn claims_expired_records_by_action_in_one_transaction() {
    let directory = tempdir().unwrap();
    let repository = SqliteSandboxRepository::open(directory.path().join("control.db"))
        .await
        .unwrap();

    let mut kill = running_record("kill", "owner-1", 0, BTreeMap::new());
    kill.replace_expiry(instant(10)).unwrap();
    repository.insert(kill).await.unwrap();

    let mut pause = running_record_with_action(
        "pause",
        "owner-1",
        0,
        BTreeMap::new(),
        OnTimeoutAction::Pause,
    );
    pause.replace_expiry(instant(10)).unwrap();
    repository.insert(pause).await.unwrap();

    let mut already_paused = running_record_with_action(
        "already-paused",
        "owner-1",
        0,
        BTreeMap::new(),
        OnTimeoutAction::Pause,
    );
    already_paused.begin_pause(false).unwrap();
    already_paused
        .mark_paused(ExecutionLease {
            execution_id: already_paused.execution_id().unwrap().clone(),
            generation: ExecutionGeneration::new(2).unwrap(),
            plan: already_paused.plan().clone(),
            resources: already_paused.resources().clone(),
            started_at: already_paused.started_at().unwrap(),
        })
        .unwrap();
    already_paused.replace_expiry(instant(10)).unwrap();
    repository.insert(already_paused).await.unwrap();

    let mut renewed = running_record("renewed", "owner-1", 0, BTreeMap::new());
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
    assert_eq!(states["kill"], crate::control::LifecycleState::Killing);
    assert_eq!(states["pause"], crate::control::LifecycleState::Pausing);
    assert_eq!(
        repository
            .get(&SandboxId::new("already-paused").unwrap())
            .await
            .unwrap()
            .unwrap()
            .state(),
        crate::control::LifecycleState::Paused
    );
    assert_eq!(
        repository
            .get(&SandboxId::new("renewed").unwrap())
            .await
            .unwrap()
            .unwrap()
            .state(),
        crate::control::LifecycleState::Running
    );
}

#[tokio::test]
async fn expiry_claim_compares_fractional_rfc3339_values_chronologically() {
    let directory = tempdir().unwrap();
    let repository = SqliteSandboxRepository::open(directory.path().join("control.db"))
        .await
        .unwrap();
    let mut record = running_record("sandbox-1", "owner-1", 0, BTreeMap::new());
    record.replace_expiry(instant(10)).unwrap();
    repository.insert(record).await.unwrap();

    let claimed = repository
        .claim_expired(
            instant(10) + Duration::milliseconds(1),
            NonZeroU32::new(1).unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].state(), crate::control::LifecycleState::Killing);
}

#[tokio::test]
async fn timeout_replacement_and_expiry_claim_have_one_winner() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("control.db");
    let reaper = SqliteSandboxRepository::open(&path).await.unwrap();
    let api = SqliteSandboxRepository::open(&path).await.unwrap();
    let mut record = running_record("sandbox-1", "owner-1", 0, BTreeMap::new());
    record.replace_expiry(instant(10)).unwrap();
    reaper.insert(record.clone()).await.unwrap();

    let expected = record.generation();
    let mut renewed = record.clone();
    renewed.replace_expiry(instant(40)).unwrap();
    let claim = reaper.claim_expired(instant(20), NonZeroU32::new(1).unwrap());
    let replace = api.compare_and_swap(record.sandbox_id(), expected, renewed);
    let (claimed, replaced) = tokio::join!(claim, replace);
    let claimed = claimed.unwrap();
    let replaced = replaced.unwrap();

    assert!(matches!(
        (claimed.len(), replaced),
        (1, CompareAndSwapResult::Conflict { .. }) | (0, CompareAndSwapResult::Updated)
    ));
    let persisted = reaper.get(record.sandbox_id()).await.unwrap().unwrap();
    if claimed.is_empty() {
        assert_eq!(persisted.state(), crate::control::LifecycleState::Running);
        assert_eq!(persisted.expires_at(), instant(40));
    } else {
        assert_eq!(persisted.state(), crate::control::LifecycleState::Killing);
        assert_eq!(persisted.expires_at(), instant(10));
    }
}

#[tokio::test]
async fn list_preserves_owner_metadata_state_and_cursor_boundaries() {
    let directory = tempdir().unwrap();
    let repository = SqliteSandboxRepository::open(directory.path().join("control.db"))
        .await
        .unwrap();
    for record in [
        running_record(
            "sandbox-1",
            "owner-1",
            0,
            BTreeMap::from([("team".to_string(), "alpha".to_string())]),
        ),
        running_record(
            "sandbox-2",
            "owner-1",
            2,
            BTreeMap::from([("team".to_string(), "alpha".to_string())]),
        ),
        running_record(
            "sandbox-3",
            "owner-2",
            4,
            BTreeMap::from([("team".to_string(), "alpha".to_string())]),
        ),
        running_record(
            "sandbox-4",
            "owner-1",
            6,
            BTreeMap::from([("team".to_string(), "beta".to_string())]),
        ),
    ] {
        repository.insert(record).await.unwrap();
    }

    let first = repository
        .list(&SandboxListFilter {
            owner_id: "owner-1".to_string(),
            metadata: BTreeMap::from([("team".to_string(), "alpha".to_string())]),
            states: BTreeSet::from([PublicSandboxState::Running]),
            limit: NonZeroU32::new(1).unwrap(),
            after: None,
        })
        .await
        .unwrap();
    assert_eq!(first.records[0].sandbox_id().as_str(), "sandbox-1");
    assert!(first.next.is_some());

    let second = repository
        .list(&SandboxListFilter {
            owner_id: "owner-1".to_string(),
            metadata: BTreeMap::from([("team".to_string(), "alpha".to_string())]),
            states: BTreeSet::new(),
            limit: NonZeroU32::new(1).unwrap(),
            after: first.next,
        })
        .await
        .unwrap();
    assert_eq!(second.records[0].sandbox_id().as_str(), "sandbox-2");
    assert!(second.next.is_none());
}

#[tokio::test]
async fn list_orders_fractional_rfc3339_timestamps_chronologically() {
    let directory = tempdir().unwrap();
    let repository = SqliteSandboxRepository::open(directory.path().join("control.db"))
        .await
        .unwrap();
    let early = running_record_at(
        "z-early",
        "owner-1",
        instant(0),
        BTreeMap::new(),
        OnTimeoutAction::Kill,
    );
    let late = running_record_at(
        "a-late",
        "owner-1",
        instant(0) + Duration::milliseconds(1),
        BTreeMap::new(),
        OnTimeoutAction::Kill,
    );
    repository.insert(late).await.unwrap();
    repository.insert(early).await.unwrap();

    let first = repository
        .list(&SandboxListFilter {
            owner_id: "owner-1".to_string(),
            metadata: BTreeMap::new(),
            states: BTreeSet::new(),
            limit: NonZeroU32::new(1).unwrap(),
            after: None,
        })
        .await
        .unwrap();
    assert_eq!(first.records[0].sandbox_id().as_str(), "z-early");

    let second = repository
        .list(&SandboxListFilter {
            owner_id: "owner-1".to_string(),
            metadata: BTreeMap::new(),
            states: BTreeSet::new(),
            limit: NonZeroU32::new(1).unwrap(),
            after: first.next.as_ref().cloned(),
        })
        .await
        .unwrap();
    assert_eq!(second.records[0].sandbox_id().as_str(), "a-late");
}

#[tokio::test]
async fn semantically_corrupt_rows_are_rejected_on_read() {
    let directory = tempdir().unwrap();
    let repository = SqliteSandboxRepository::open(directory.path().join("control.db"))
        .await
        .unwrap();
    let record = running_record("sandbox-1", "owner-1", 0, BTreeMap::new());
    repository.insert(record.clone()).await.unwrap();
    let sandbox_id = record.sandbox_id().to_string();

    repository
        .call(move |connection| {
            connection
                .execute(
                    "UPDATE sandbox_records \
                     SET record_json = json_set(\
                         record_json,\
                         '$.credentials.envd.key_version',\
                         0\
                     ) \
                     WHERE sandbox_id = ?1",
                    [sandbox_id],
                )
                .map_err(|error| unavailable("inject corrupt test row", error))?;
            Ok(())
        })
        .await
        .unwrap();

    assert!(matches!(
        repository.get(record.sandbox_id()).await,
        Err(RepositoryError::Corrupt(_))
    ));

    let second = running_record("sandbox-2", "owner-1", 2, BTreeMap::new());
    repository.insert(second.clone()).await.unwrap();
    let sandbox_id = second.sandbox_id().to_string();
    repository
        .call(move |connection| {
            connection
                .execute(
                    "UPDATE sandbox_records \
                     SET record_json = json_remove(record_json, '$.execution_id') \
                     WHERE sandbox_id = ?1",
                    [sandbox_id],
                )
                .map_err(|error| unavailable("inject inconsistent test row", error))?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(matches!(
        repository.get(second.sandbox_id()).await,
        Err(RepositoryError::Corrupt(_))
    ));
}

#[tokio::test]
async fn unknown_migration_history_refuses_to_open() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("control.db");
    let repository = SqliteSandboxRepository::open(&path).await.unwrap();
    repository
        .call(|connection| {
            connection
                .execute(
                    "INSERT INTO compatibility_schema_migrations(version, name, applied_at) \
                     VALUES (99, 'future', '2026-07-14T12:00:00Z')",
                    [],
                )
                .map_err(|error| unavailable("inject future migration", error))?;
            Ok(())
        })
        .await
        .unwrap();
    drop(repository);

    assert!(matches!(
        SqliteSandboxRepository::open(path).await,
        Err(RepositoryError::Corrupt(_))
    ));
}
