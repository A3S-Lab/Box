use std::sync::Arc;

use tempfile::tempdir;

use crate::control::SqliteSandboxRepository;

use super::super::*;
use super::support::record;

#[tokio::test]
async fn memory_repository_enforces_the_volume_contract() {
    exercise_repository(Arc::new(MemoryVolumeRepository::default())).await;
}

#[tokio::test]
async fn sqlite_repository_enforces_the_volume_contract() {
    let directory = tempdir().unwrap();
    let control = SqliteSandboxRepository::open(directory.path().join("control.db"))
        .await
        .unwrap();
    exercise_repository(Arc::new(SqliteVolumeRepository::new(control.connection()))).await;
}

async fn exercise_repository(repository: Arc<dyn VolumeRepository>) {
    let owner_a = record(
        "volume-a",
        "owner-a",
        "data",
        "runtime-a",
        VolumeState::Active,
        2,
    );
    let owner_b = record(
        "volume-b",
        "owner-b",
        "data",
        "runtime-b",
        VolumeState::Active,
        1,
    );
    let creating = record(
        "volume-c",
        "owner-a",
        "cache",
        "runtime-c",
        VolumeState::Creating,
        0,
    );
    repository.insert(owner_a.clone()).await.unwrap();
    repository.insert(owner_b.clone()).await.unwrap();
    repository.insert(creating.clone()).await.unwrap();

    let listed = repository.list("owner-a").await.unwrap();
    assert_eq!(listed, vec![owner_a.clone()]);
    assert_eq!(
        repository
            .get_by_owner_name("owner-b", "data")
            .await
            .unwrap(),
        Some(owner_b)
    );
    assert_eq!(
        repository
            .list_in_state(VolumeState::Creating)
            .await
            .unwrap(),
        vec![creating.clone()]
    );

    let duplicate_name = record(
        "volume-d",
        "owner-a",
        "data",
        "runtime-d",
        VolumeState::Active,
        3,
    );
    assert!(matches!(
        repository.insert(duplicate_name).await,
        Err(VolumeRepositoryError::Duplicate)
    ));
    assert!(matches!(
        repository.insert(owner_a).await,
        Err(VolumeRepositoryError::Duplicate)
    ));

    assert_eq!(
        repository
            .replace(VolumeState::Active, creating.clone())
            .await
            .unwrap(),
        VolumeReplaceResult::Conflict
    );
    let mut active = creating;
    active.mark_active().unwrap();
    assert_eq!(
        repository
            .replace(VolumeState::Creating, active.clone())
            .await
            .unwrap(),
        VolumeReplaceResult::Updated
    );
    assert_eq!(
        repository
            .delete(active.volume_id(), VolumeState::Creating)
            .await
            .unwrap(),
        VolumeReplaceResult::Conflict
    );
    assert_eq!(
        repository
            .delete(active.volume_id(), VolumeState::Active)
            .await
            .unwrap(),
        VolumeReplaceResult::Updated
    );
    assert!(repository.get(active.volume_id()).await.unwrap().is_none());
}
