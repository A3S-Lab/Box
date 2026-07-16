use std::sync::Arc;

use tempfile::tempdir;

use crate::control::SqliteSandboxRepository;

use super::super::*;
use super::support::record;

#[tokio::test]
async fn memory_repository_enforces_the_snapshot_contract() {
    exercise_repository(Arc::new(MemorySnapshotRepository::default())).await;
}

#[tokio::test]
async fn sqlite_repository_enforces_the_snapshot_contract() {
    let directory = tempdir().unwrap();
    let control = SqliteSandboxRepository::open(directory.path().join("control.db"))
        .await
        .unwrap();
    exercise_repository(Arc::new(SqliteSnapshotRepository::new(control.connection()))).await;
}

async fn exercise_repository(repository: Arc<dyn SnapshotRepository>) {
    let owner_a = record(
        "snapshot-a",
        "owner-a",
        Some("state"),
        SnapshotState::Active,
        2,
    );
    let owner_b = record(
        "snapshot-b",
        "owner-b",
        Some("state"),
        SnapshotState::Active,
        1,
    );
    let creating = record(
        "snapshot-c",
        "owner-a",
        Some("cache"),
        SnapshotState::Creating,
        0,
    );
    repository.insert(owner_a.clone()).await.unwrap();
    repository.insert(owner_b.clone()).await.unwrap();
    repository.insert(creating.clone()).await.unwrap();

    let listed = repository.list("owner-a").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].snapshot_id(), owner_a.snapshot_id());
    assert_eq!(
        repository
            .get_by_reference("owner-b", owner_b.reference())
            .await
            .unwrap()
            .unwrap()
            .snapshot_id(),
        owner_b.snapshot_id()
    );
    assert!(repository
        .get_by_reference("owner-a", owner_b.reference())
        .await
        .unwrap()
        .is_none());
    let transitional = repository
        .list_in_state(SnapshotState::Creating)
        .await
        .unwrap();
    assert_eq!(transitional.len(), 1);
    assert_eq!(transitional[0].snapshot_id(), creating.snapshot_id());

    let duplicate_reference = record(
        "snapshot-d",
        "owner-a",
        Some("state"),
        SnapshotState::Active,
        3,
    );
    assert!(matches!(
        repository.insert(duplicate_reference).await,
        Err(SnapshotRepositoryError::Duplicate)
    ));
    assert!(matches!(
        repository.insert(owner_a).await,
        Err(SnapshotRepositoryError::Duplicate)
    ));

    assert_eq!(
        repository
            .replace(SnapshotState::Active, creating.clone())
            .await
            .unwrap(),
        SnapshotReplaceResult::Conflict
    );
    let mut active = creating;
    active.mark_active(8_192).unwrap();
    assert_eq!(
        repository
            .replace(SnapshotState::Creating, active.clone())
            .await
            .unwrap(),
        SnapshotReplaceResult::Updated
    );
    assert_eq!(
        repository
            .delete(active.snapshot_id(), SnapshotState::Creating)
            .await
            .unwrap(),
        SnapshotReplaceResult::Conflict
    );
    assert_eq!(
        repository
            .delete(active.snapshot_id(), SnapshotState::Active)
            .await
            .unwrap(),
        SnapshotReplaceResult::Updated
    );
    assert!(repository.get(active.snapshot_id()).await.unwrap().is_none());
}
