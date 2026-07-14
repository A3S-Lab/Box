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
    LifecyclePolicy, NewSandboxRecord, OnTimeoutAction, PublicSandboxState, SandboxCredentials,
    StoredToken,
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
            on_timeout: OnTimeoutAction::Kill,
            auto_resume: false,
            keep_memory_on_pause: false,
        },
        created_at: instant(second),
        expires_at: instant(second) + Duration::seconds(300),
        metadata,
        envd_version: "0.1.3".to_string(),
        secure: true,
        allow_internet_access: Some(false),
        credentials: SandboxCredentials {
            envd: stored_token(10),
            traffic: stored_token(20),
        },
    })
    .unwrap();
    record
        .mark_running(ExecutionLease {
            execution_id: ExecutionId::new(format!("execution-{sandbox_id}")).unwrap(),
            generation: ExecutionGeneration::INITIAL,
            plan: record.plan().clone(),
            resources: record.resources().clone(),
            started_at: instant(second) + Duration::seconds(1),
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

    let (journal_mode, migrations, strict) = repository
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
            Ok((journal_mode, migrations, strict))
        })
        .await
        .unwrap();

    assert_eq!(journal_mode, "wal");
    assert_eq!(migrations, "1:lifecycle_records");
    assert_eq!(strict, 1);
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
