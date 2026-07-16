use crate::control::SecretToken;

use super::super::*;
use super::support::{record, ServiceHarness};

#[tokio::test]
async fn service_scopes_names_owners_and_tokens_without_leaking_runtime_names() {
    let harness = ServiceHarness::new();
    let owner_a = harness.service.create("owner-a", "data").await.unwrap();
    let owner_b = harness.service.create("owner-b", "data").await.unwrap();

    assert_ne!(owner_a.record.volume_id(), owner_b.record.volume_id());
    assert_ne!(owner_a.record.runtime_name(), owner_b.record.runtime_name());
    assert_ne!(
        owner_a.token.expose_secret(),
        owner_b.token.expose_secret()
    );
    assert!(matches!(
        harness
            .service
            .get("owner-b", owner_a.record.volume_id())
            .await,
        Err(VolumeServiceError::NotFound)
    ));
    assert_eq!(harness.service.list("owner-a").await.unwrap().len(), 1);

    let authorized = harness
        .service
        .authorize(owner_a.record.volume_id(), &owner_a.token)
        .await
        .unwrap();
    assert_eq!(authorized.record.owner_id(), "owner-a");
    assert!(authorized.root.ends_with(owner_a.record.runtime_name()));
    assert!(matches!(
        harness
            .service
            .authorize(owner_a.record.volume_id(), &owner_b.token)
            .await,
        Err(VolumeServiceError::Forbidden)
    ));
    assert!(matches!(
        harness
            .service
            .authorize(
                owner_a.record.volume_id(),
                &SecretToken::new("invalid-volume-token").unwrap()
            )
            .await,
        Err(VolumeServiceError::Forbidden)
    ));

    let mounts = harness
        .service
        .resolve_mounts(
            "owner-a",
            &[VolumeMount::new("data", "/mnt/data").unwrap()],
        )
        .await
        .unwrap();
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].public.name, "data");
    assert_eq!(mounts[0].runtime_name, owner_a.record.runtime_name());
    assert_eq!(
        mounts[0].runtime_spec(),
        format!("{}:/mnt/data:rw", authorized.root.display())
    );
}

#[tokio::test]
async fn deletion_conflict_restores_visibility_and_success_removes_content() {
    let harness = ServiceHarness::new();
    let created = harness.service.create("owner-a", "data").await.unwrap();
    let id = created.record.volume_id().clone();
    let runtime_name = created.record.runtime_name().to_string();
    let root = harness
        .service
        .authorize(&id, &created.token)
        .await
        .unwrap()
        .root;
    std::fs::write(root.join("value.txt"), b"value").unwrap();

    harness.runtime.set_in_use(&runtime_name, true);
    assert!(matches!(
        harness.service.delete("owner-a", &id).await,
        Err(VolumeServiceError::Conflict)
    ));
    assert_eq!(
        harness.service.get("owner-a", &id).await.unwrap().record.state(),
        VolumeState::Active
    );
    assert!(root.join("value.txt").exists());

    harness.runtime.set_in_use(&runtime_name, false);
    harness.service.delete("owner-a", &id).await.unwrap();
    assert!(!root.exists());
    assert!(matches!(
        harness.service.get("owner-a", &id).await,
        Err(VolumeServiceError::NotFound)
    ));
}

#[tokio::test]
async fn startup_reconciliation_completes_creates_and_safe_deletes() {
    let harness = ServiceHarness::new();
    let creating = record(
        "creating-volume",
        "owner-a",
        "creating",
        "runtime-creating",
        VolumeState::Creating,
        0,
    );
    let deleting = record(
        "deleting-volume",
        "owner-a",
        "deleting",
        "runtime-deleting",
        VolumeState::Deleting,
        1,
    );
    let busy = record(
        "busy-volume",
        "owner-a",
        "busy",
        "runtime-busy",
        VolumeState::Deleting,
        2,
    );
    harness.repository.insert(creating.clone()).await.unwrap();
    harness.repository.insert(deleting.clone()).await.unwrap();
    harness.repository.insert(busy.clone()).await.unwrap();
    harness
        .runtime
        .materialize(deleting.runtime_name())
        .await
        .unwrap();
    harness
        .runtime
        .materialize(busy.runtime_name())
        .await
        .unwrap();
    harness.runtime.set_in_use(busy.runtime_name(), true);

    let report = harness.service.reconcile_startup().await.unwrap();

    assert_eq!(report.examined, 3);
    assert_eq!(report.completed, 2);
    assert_eq!(report.deferred, 1);
    assert!(report.failures.is_empty());
    assert_eq!(
        harness
            .repository
            .get(creating.volume_id())
            .await
            .unwrap()
            .unwrap()
            .state(),
        VolumeState::Active
    );
    assert!(harness
        .repository
        .get(deleting.volume_id())
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        harness
            .repository
            .get(busy.volume_id())
            .await
            .unwrap()
            .unwrap()
            .state(),
        VolumeState::Active
    );
}
