use std::num::NonZeroU32;

use crate::control::test_support::{create_request, TestHarness};
use crate::control::{ControlServiceError, TemplateProviderError};

use super::super::*;
use super::support::{record, template};

#[tokio::test]
async fn capture_restores_after_source_deletion_and_enforces_owner_and_active_use() {
    let harness = TestHarness::new();
    let source = harness
        .service
        .create(create_request("owner-a"))
        .await
        .unwrap();
    let snapshot = harness
        .service
        .create_snapshot("owner-a", source.record.sandbox_id(), Some("fixture-state"))
        .await
        .unwrap();
    assert_eq!(snapshot.state(), SnapshotState::Active);
    assert!(snapshot.reference().ends_with("/fixture-state:default"));
    assert_eq!(snapshot.names(), vec![snapshot.reference().to_string()]);
    assert_eq!(harness.executions.snapshot_ids().len(), 1);
    assert!(matches!(
        harness
            .service
            .create_snapshot("owner-a", source.record.sandbox_id(), Some("fixture-state"),)
            .await,
        Err(ControlServiceError::Snapshot(
            SnapshotServiceError::Duplicate
        ))
    ));

    let mut forbidden = create_request("owner-b");
    forbidden.template_id = snapshot.reference().to_string();
    assert!(matches!(
        harness.service.create(forbidden).await,
        Err(ControlServiceError::Template(
            TemplateProviderError::NotFound(_)
        ))
    ));

    assert!(harness
        .service
        .kill("owner-a", source.record.sandbox_id())
        .await
        .unwrap());
    let mut restore_request = create_request("owner-a");
    restore_request.template_id = snapshot.reference().to_string();
    let restored = harness.service.create(restore_request).await.unwrap();
    let runtime_request = harness.executions.requests().last().cloned().unwrap();
    assert_eq!(
        runtime_request.rootfs_snapshot_id.as_ref(),
        Some(snapshot.content_id())
    );

    assert!(matches!(
        harness
            .snapshots
            .delete("owner-a", snapshot.reference())
            .await,
        Err(SnapshotServiceError::Conflict)
    ));
    assert!(harness
        .service
        .kill("owner-a", restored.record.sandbox_id())
        .await
        .unwrap());
    assert!(harness
        .snapshots
        .delete("owner-a", snapshot.reference())
        .await
        .unwrap());
    assert!(!harness
        .snapshots
        .delete("owner-a", snapshot.reference())
        .await
        .unwrap());
}

#[tokio::test]
async fn list_filters_and_paginates_with_stable_cursors() {
    let harness = TestHarness::new();
    let first_source = harness
        .service
        .create(create_request("owner-a"))
        .await
        .unwrap();
    let second_source = harness
        .service
        .create(create_request("owner-a"))
        .await
        .unwrap();
    for (source, name) in [
        (&first_source, "first"),
        (&first_source, "second"),
        (&second_source, "third"),
    ] {
        harness
            .service
            .create_snapshot("owner-a", source.record.sandbox_id(), Some(name))
            .await
            .unwrap();
    }

    let limit = NonZeroU32::new(1).unwrap();
    let first = harness
        .snapshots
        .list(
            "owner-a",
            Some(first_source.record.sandbox_id()),
            limit,
            None,
        )
        .await
        .unwrap();
    assert_eq!(first.records.len(), 1);
    let second = harness
        .snapshots
        .list(
            "owner-a",
            Some(first_source.record.sandbox_id()),
            limit,
            first.next.as_ref(),
        )
        .await
        .unwrap();
    assert_eq!(second.records.len(), 1);
    assert!(second.next.is_none());
    assert_ne!(
        first.records[0].snapshot_id(),
        second.records[0].snapshot_id()
    );
    assert!(harness
        .snapshots
        .list("owner-b", None, NonZeroU32::new(100).unwrap(), None)
        .await
        .unwrap()
        .records
        .is_empty());
}

#[tokio::test]
async fn snapshot_of_snapshot_has_independent_content() {
    let harness = TestHarness::new();
    let source = harness
        .service
        .create(create_request("owner-a"))
        .await
        .unwrap();
    let first = harness
        .service
        .create_snapshot("owner-a", source.record.sandbox_id(), Some("first"))
        .await
        .unwrap();
    let mut restore_request = create_request("owner-a");
    restore_request.template_id = first.reference().to_string();
    let restored = harness.service.create(restore_request).await.unwrap();
    let second = harness
        .service
        .create_snapshot("owner-a", restored.record.sandbox_id(), Some("second"))
        .await
        .unwrap();
    assert_ne!(first.content_id(), second.content_id());
    assert_eq!(
        second.template().rootfs_snapshot_id.as_ref(),
        Some(second.content_id())
    );

    assert!(harness
        .service
        .kill("owner-a", restored.record.sandbox_id())
        .await
        .unwrap());
    assert!(harness
        .snapshots
        .delete("owner-a", first.reference())
        .await
        .unwrap());
    let mut second_restore = create_request("owner-a");
    second_restore.template_id = second.reference().to_string();
    let nested = harness.service.create(second_restore).await.unwrap();
    assert_eq!(
        harness
            .executions
            .requests()
            .last()
            .unwrap()
            .rootfs_snapshot_id
            .as_ref(),
        Some(second.content_id())
    );
    assert!(harness
        .service
        .kill("owner-a", nested.record.sandbox_id())
        .await
        .unwrap());
}

#[tokio::test]
async fn startup_reconciliation_publishes_or_removes_creating_records() {
    let harness = TestHarness::new();
    let source = harness
        .service
        .create(create_request("owner-a"))
        .await
        .unwrap();
    let pending = harness
        .snapshots
        .capture("owner-a", &source.record, Some("recover"), template())
        .await
        .unwrap();
    let pending_id = pending.record.snapshot_id().clone();
    assert_eq!(pending.record.state(), SnapshotState::Creating);

    let report = harness.snapshots.reconcile_startup().await.unwrap();
    assert_eq!(report.examined, 1);
    assert_eq!(report.completed, 1);
    assert_eq!(
        harness
            .snapshot_repository
            .get(&pending_id)
            .await
            .unwrap()
            .unwrap()
            .state(),
        SnapshotState::Active
    );

    let orphan = record(
        "orphan-snapshot",
        "owner-a",
        Some("orphan"),
        SnapshotState::Creating,
        10,
    );
    let orphan_id = orphan.snapshot_id().clone();
    harness.snapshot_repository.insert(orphan).await.unwrap();
    let report = harness.snapshots.reconcile_startup().await.unwrap();
    assert_eq!(report.completed, 1);
    assert!(harness
        .snapshot_repository
        .get(&orphan_id)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn startup_reconciliation_rolls_an_in_use_delete_back_to_active() {
    let harness = TestHarness::new();
    let source = harness
        .service
        .create(create_request("owner-a"))
        .await
        .unwrap();
    let snapshot = harness
        .service
        .create_snapshot("owner-a", source.record.sandbox_id(), Some("active"))
        .await
        .unwrap();
    let mut restore_request = create_request("owner-a");
    restore_request.template_id = snapshot.reference().to_string();
    let restored = harness.service.create(restore_request).await.unwrap();
    let mut deleting = snapshot.clone();
    deleting.begin_delete().unwrap();
    assert_eq!(
        harness
            .snapshot_repository
            .replace(SnapshotState::Active, deleting)
            .await
            .unwrap(),
        SnapshotReplaceResult::Updated
    );

    let report = harness.snapshots.reconcile_startup().await.unwrap();
    assert_eq!(report.deferred, 1);
    assert_eq!(
        harness
            .snapshot_repository
            .get(snapshot.snapshot_id())
            .await
            .unwrap()
            .unwrap()
            .state(),
        SnapshotState::Active
    );
    assert!(harness
        .service
        .kill("owner-a", restored.record.sandbox_id())
        .await
        .unwrap());
}
