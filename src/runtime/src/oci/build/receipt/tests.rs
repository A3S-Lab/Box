use std::sync::Arc;

use a3s_box_core::OperationId;

use super::*;
use crate::oci::build::{
    execute_recorded_build_plan, inspect_recorded_build_plan, inspect_recorded_build_status,
    remove_recorded_build_plan, BoxBuildPlan, BuildPlanExecutionError,
};
use crate::oci::ImageStore;

fn source_digest(byte: char) -> String {
    format!("sha256:{}", byte.to_string().repeat(64))
}

fn platform() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "linux/arm64"
    } else {
        "linux/amd64"
    }
}

fn plan(cache: &str) -> BoxBuildPlan {
    BoxBuildPlan::parse_acl(&format!(
        r#"
build "oci" {{
  cache = "{cache}"
  context = "."
  file = "Dockerfile"
  network = "none"
  platform = "{}"
  schema = "a3s.box.build-plan.v1"
}}
"#,
        platform()
    ))
    .unwrap()
}

fn write_source(root: &std::path::Path) {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(root.join("Dockerfile"), "FROM scratch\nCOPY value /value\n").unwrap();
    std::fs::write(root.join("value"), "durable-receipt\n").unwrap();
}

fn identity(operation: &str, digest_byte: char) -> BuildOperationIdentity {
    BuildOperationIdentity::new(
        OperationId::new(operation).unwrap(),
        source_digest(digest_byte),
    )
    .unwrap()
}

#[tokio::test]
async fn recorded_build_replays_the_exact_output_after_reconstruction() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    let store_root = temporary.path().join("images");
    write_source(&source);

    let operation = identity("cloud-build-run-1", 'a');
    let build_plan = plan("disabled");
    let store = Arc::new(ImageStore::new(&store_root, 64 * 1024 * 1024).unwrap());
    let receipts = BuildOperationJournal::for_image_store(&store, operation.operation_id())
        .await
        .unwrap();

    let first =
        execute_recorded_build_plan(&operation, &build_plan, &source, true, Arc::clone(&store))
            .await
            .unwrap();

    assert!(!first.replayed);
    assert_eq!(first.receipt.schema, BuildOutputReceipt::SCHEMA);
    assert_eq!(first.receipt.operation_id, *operation.operation_id());
    assert_eq!(first.receipt.source_digest, source_digest('a'));
    assert_eq!(
        first.receipt.plan_digest,
        build_plan.canonical_digest().unwrap()
    );
    assert!(first
        .receipt
        .output
        .reference
        .starts_with("a3s-box/build-operation:"));
    assert_eq!(first.receipt.output.descriptor, first.output.descriptor);
    assert_eq!(first.receipt.output.platform, first.output.platform);
    assert_eq!(
        first.receipt.output.content_bytes,
        first.output.content_bytes()
    );

    let receipt_path = receipts.receipt_path(operation.operation_id());
    let persisted = std::fs::read_to_string(&receipt_path).unwrap();
    assert!(persisted.contains(BuildOutputReceipt::SCHEMA));
    assert!(
        !persisted.contains(store_root.to_string_lossy().as_ref()),
        "the receipt must re-derive its store-owned layout path"
    );

    let expected_receipt = first.receipt.clone();
    let expected_digest = first.output.descriptor.digest.clone();
    let expected_path = first.output.layout_directory.clone();
    drop(first);
    drop(receipts);
    drop(store);
    std::fs::remove_dir_all(&source).unwrap();

    let reopened_store = Arc::new(ImageStore::new(&store_root, 64 * 1024 * 1024).unwrap());
    let replay = execute_recorded_build_plan(
        &operation,
        &build_plan,
        &source,
        true,
        Arc::clone(&reopened_store),
    )
    .await
    .unwrap();

    assert!(replay.replayed);
    assert_eq!(replay.output.descriptor.digest, expected_digest);
    assert_eq!(replay.output.layout_directory, expected_path);
    assert_eq!(replay.receipt, expected_receipt);

    let inspected = inspect_recorded_build_plan(&operation, &build_plan, &reopened_store)
        .await
        .unwrap()
        .expect("persisted receipt");
    assert!(inspected.replayed);
    assert_eq!(inspected.receipt, replay.receipt);
    assert_eq!(
        inspected.output.layout_directory,
        replay.output.layout_directory
    );
}

#[tokio::test]
async fn pending_intent_adopts_output_committed_before_terminal_receipt() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    let store_root = temporary.path().join("images");
    write_source(&source);

    let operation = identity("cloud-build-run-response-gap", 'a');
    let build_plan = plan("disabled");
    let plan_digest = build_plan.canonical_digest().unwrap();
    let store = Arc::new(ImageStore::new(&store_root, 64 * 1024 * 1024).unwrap());
    let receipts = BuildOperationJournal::for_image_store(&store, operation.operation_id())
        .await
        .unwrap();
    let built =
        execute_recorded_build_plan(&operation, &build_plan, &source, true, Arc::clone(&store))
            .await
            .unwrap();
    let expected_descriptor = built.output.descriptor.clone();
    let pending = PendingBuildOperation::new(&operation, plan_digest).unwrap();
    let mut pending_bytes = serde_json::to_vec_pretty(&pending).unwrap();
    pending_bytes.push(b'\n');
    std::fs::write(
        receipts.receipt_path(operation.operation_id()),
        pending_bytes,
    )
    .unwrap();
    std::fs::remove_dir_all(&source).unwrap();
    drop(receipts);
    drop(store);

    let reopened_store = Arc::new(ImageStore::new(&store_root, 64 * 1024 * 1024).unwrap());
    let reopened_receipts =
        BuildOperationJournal::for_image_store(&reopened_store, operation.operation_id())
            .await
            .unwrap();
    let recovered = execute_recorded_build_plan(
        &operation,
        &build_plan,
        &source,
        true,
        Arc::clone(&reopened_store),
    )
    .await
    .unwrap();

    assert!(recovered.replayed);
    assert_eq!(recovered.output.descriptor, expected_descriptor);
    let committed =
        std::fs::read_to_string(reopened_receipts.receipt_path(operation.operation_id())).unwrap();
    assert!(committed.contains(BuildOutputReceipt::SCHEMA));
    assert!(!committed.contains(PendingBuildOperation::SCHEMA));
}

#[tokio::test]
async fn supervised_recovery_adopts_the_image_store_commit_gap() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    let store_root = temporary.path().join("images");
    write_source(&source);

    let operation = identity("cloud-build-supervised-output-gap", 'a');
    let build_plan = plan("disabled");
    let plan_digest = build_plan.canonical_digest().unwrap();
    let store = Arc::new(ImageStore::new(&store_root, 64 * 1024 * 1024).unwrap());
    let journal = BuildOperationJournal::for_image_store(&store, operation.operation_id())
        .await
        .unwrap();
    let built =
        execute_recorded_build_plan(&operation, &build_plan, &source, true, Arc::clone(&store))
            .await
            .unwrap();
    let expected_descriptor = built.output.descriptor.clone();
    let workspace = journal
        .prepare_workspace(operation.operation_id())
        .await
        .unwrap();
    std::fs::write(workspace.join("uncommitted"), "partial").unwrap();
    let active = SupervisedBuildOperation::new(&operation, plan_digest).unwrap();
    let mut active_bytes = serde_json::to_vec_pretty(&active).unwrap();
    active_bytes.push(b'\n');
    std::fs::write(journal.receipt_path(operation.operation_id()), active_bytes).unwrap();
    std::fs::remove_dir_all(&source).unwrap();
    drop(store);

    let reopened = ImageStore::new(&store_root, 64 * 1024 * 1024).unwrap();
    let recovered = inspect_recorded_build_status(&operation, &build_plan, &reopened)
        .await
        .unwrap()
        .expect("committed output is terminal");
    let RecordedBuildStatus::Succeeded(recovered) = recovered else {
        panic!("committed ImageStore output must win the receipt gap");
    };
    assert_eq!(recovered.output.descriptor, expected_descriptor);
    assert!(!journal.workspace_path(operation.operation_id()).exists());
    let committed =
        std::fs::read_to_string(journal.receipt_path(operation.operation_id())).unwrap();
    assert!(committed.contains(BuildOutputReceipt::SCHEMA));
    assert!(!committed.contains(SupervisedBuildOperation::SCHEMA));
}

#[tokio::test]
async fn live_operation_inspection_is_nonblocking_and_cancellation_is_one_state_transition() {
    let temporary = tempfile::tempdir().unwrap();
    let store_root = temporary.path().join("images");
    let operation = identity("cloud-build-live-supervision", 'a');
    let build_plan = plan("disabled");
    let plan_digest = build_plan.canonical_digest().unwrap();
    let store = ImageStore::new(&store_root, 64 * 1024 * 1024).unwrap();
    let journal = BuildOperationJournal::for_image_store(&store, operation.operation_id())
        .await
        .unwrap();
    let locked = journal.lock(operation.operation_id()).await.unwrap();
    let lease = journal
        .try_execution_lease(operation.operation_id())
        .await
        .unwrap()
        .expect("test owns the execution lease");
    let workspace = journal
        .prepare_workspace(operation.operation_id())
        .await
        .unwrap();
    std::fs::write(workspace.join("partial"), "uncommitted").unwrap();
    locked
        .write_supervised(SupervisedBuildOperation::new(&operation, plan_digest.clone()).unwrap())
        .await
        .unwrap();
    drop(locked);

    let inspected = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        inspect_recorded_build_status(&operation, &build_plan, &store),
    )
    .await
    .expect("inspection must not wait for the live execution lease")
    .unwrap();
    assert!(matches!(inspected, Some(RecordedBuildStatus::Running)));

    assert_eq!(
        crate::oci::build::cancel_recorded_build_plan(&operation, &build_plan, &store)
            .await
            .unwrap(),
        BuildCancellationOutcome::Requested
    );
    assert!(matches!(
        inspect_recorded_build_status(&operation, &build_plan, &store)
            .await
            .unwrap(),
        Some(RecordedBuildStatus::Cancelling)
    ));

    drop(lease);
    assert!(matches!(
        inspect_recorded_build_status(&operation, &build_plan, &store)
            .await
            .unwrap(),
        Some(RecordedBuildStatus::Cancelled { .. })
    ));
    assert!(!journal.workspace_path(operation.operation_id()).exists());
    let persisted =
        std::fs::read_to_string(journal.receipt_path(operation.operation_id())).unwrap();
    assert!(persisted.contains(SupervisedBuildOperation::SCHEMA));
    assert!(!persisted.contains(PendingBuildOperation::SCHEMA));
}

#[tokio::test]
async fn abandoned_operation_reclaims_workspace_and_becomes_failed() {
    let temporary = tempfile::tempdir().unwrap();
    let store_root = temporary.path().join("images");
    let operation = identity("cloud-build-abandoned-supervision", 'a');
    let build_plan = plan("disabled");
    let store = ImageStore::new(&store_root, 64 * 1024 * 1024).unwrap();
    let journal = BuildOperationJournal::for_image_store(&store, operation.operation_id())
        .await
        .unwrap();
    let workspace = journal
        .prepare_workspace(operation.operation_id())
        .await
        .unwrap();
    std::fs::create_dir_all(workspace.join("rootfs_0")).unwrap();
    std::fs::write(workspace.join("rootfs_0/partial"), "partial").unwrap();
    let locked = journal.lock(operation.operation_id()).await.unwrap();
    locked
        .write_supervised(
            SupervisedBuildOperation::new(&operation, build_plan.canonical_digest().unwrap())
                .unwrap(),
        )
        .await
        .unwrap();
    drop(locked);

    let status = inspect_recorded_build_status(&operation, &build_plan, &store)
        .await
        .unwrap();
    assert!(matches!(status, Some(RecordedBuildStatus::Failed { .. })));
    assert!(!journal.workspace_path(operation.operation_id()).exists());
    assert!(store.get(operation.output_reference()).await.is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn operation_workspace_rejects_a_symlink_without_touching_its_target() {
    let temporary = tempfile::tempdir().unwrap();
    let store_root = temporary.path().join("images");
    let outside = temporary.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("keep"), "keep").unwrap();
    let operation = identity("cloud-build-workspace-symlink", 'a');
    let store = ImageStore::new(&store_root, 64 * 1024 * 1024).unwrap();
    let journal = BuildOperationJournal::for_image_store(&store, operation.operation_id())
        .await
        .unwrap();
    std::os::unix::fs::symlink(&outside, journal.workspace_path(operation.operation_id())).unwrap();

    let error = journal
        .prepare_workspace(operation.operation_id())
        .await
        .unwrap_err();
    assert!(matches!(error, BuildReceiptError::UnsafeStore { .. }));
    assert_eq!(
        std::fs::read_to_string(outside.join("keep")).unwrap(),
        "keep"
    );
}

#[tokio::test]
async fn recorded_build_rejects_operation_identity_drift_before_source_access() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    let store_root = temporary.path().join("images");
    write_source(&source);

    let operation = identity("cloud-build-run-2", 'a');
    let build_plan = plan("disabled");
    let store = Arc::new(ImageStore::new(&store_root, 64 * 1024 * 1024).unwrap());
    execute_recorded_build_plan(&operation, &build_plan, &source, true, Arc::clone(&store))
        .await
        .unwrap();
    std::fs::remove_dir_all(&source).unwrap();

    let changed_source = identity("cloud-build-run-2", 'b');
    let source_error = execute_recorded_build_plan(
        &changed_source,
        &build_plan,
        &source,
        true,
        Arc::clone(&store),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        source_error,
        BuildPlanExecutionError::Receipt(BuildReceiptError::Conflict { .. })
    ));

    let plan_error = execute_recorded_build_plan(
        &operation,
        &plan("content-addressed"),
        &source,
        true,
        Arc::clone(&store),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        plan_error,
        BuildPlanExecutionError::Receipt(BuildReceiptError::Conflict { .. })
    ));
}

#[tokio::test]
async fn concurrent_retries_publish_once_and_replay_once() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    let store_root = temporary.path().join("images");
    write_source(&source);

    let operation = identity("cloud-build-run-concurrent", 'a');
    let build_plan = plan("disabled");
    let store = Arc::new(ImageStore::new(&store_root, 64 * 1024 * 1024).unwrap());
    let first =
        execute_recorded_build_plan(&operation, &build_plan, &source, true, Arc::clone(&store));
    let second =
        execute_recorded_build_plan(&operation, &build_plan, &source, true, Arc::clone(&store));
    let (first, second) = tokio::join!(first, second);
    let first = first.unwrap();
    let second = second.unwrap();

    assert_ne!(first.replayed, second.replayed);
    assert_eq!(first.receipt, second.receipt);
    assert_eq!(first.output.descriptor, second.output.descriptor);
    assert_eq!(store.list().await.len(), 1);
}

#[tokio::test]
async fn replay_refreshes_the_single_image_store_authority_across_instances() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    let store_root = temporary.path().join("images");
    write_source(&source);

    let operation = identity("cloud-build-run-cross-instance", 'a');
    let build_plan = plan("disabled");
    let builder_store = Arc::new(ImageStore::new(&store_root, 64 * 1024 * 1024).unwrap());
    let stale_replay_store = ImageStore::new(&store_root, 64 * 1024 * 1024).unwrap();

    let built = execute_recorded_build_plan(
        &operation,
        &build_plan,
        &source,
        true,
        Arc::clone(&builder_store),
    )
    .await
    .unwrap();
    std::fs::remove_dir_all(&source).unwrap();

    let replay = inspect_recorded_build_plan(&operation, &build_plan, &stale_replay_store)
        .await
        .unwrap()
        .expect("terminal receipt");

    assert!(replay.replayed);
    assert_eq!(replay.receipt, built.receipt);
    assert_eq!(replay.output.descriptor, built.output.descriptor);
}

#[tokio::test]
async fn failed_build_persists_one_terminal_state_until_idempotent_cleanup() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    let store_root = temporary.path().join("images");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(
        source.join("Dockerfile"),
        "FROM scratch\nCOPY missing /missing\n",
    )
    .unwrap();

    let operation = identity("cloud-build-run-failed", 'a');
    let build_plan = plan("disabled");
    let store = Arc::new(ImageStore::new(&store_root, 64 * 1024 * 1024).unwrap());
    let receipts = BuildOperationJournal::for_image_store(&store, operation.operation_id())
        .await
        .unwrap();
    let failure =
        execute_recorded_build_plan(&operation, &build_plan, &source, true, Arc::clone(&store))
            .await
            .unwrap_err();
    assert!(matches!(failure, BuildPlanExecutionError::Failed { .. }));

    let persisted =
        std::fs::read_to_string(receipts.receipt_path(operation.operation_id())).unwrap();
    assert!(persisted.contains(SupervisedBuildOperation::SCHEMA));
    assert!(persisted.contains("\"phase\": \"failed\""));
    assert!(!receipts.workspace_path(operation.operation_id()).exists());
    assert!(matches!(
        inspect_recorded_build_status(&operation, &build_plan, &store)
            .await
            .unwrap(),
        Some(RecordedBuildStatus::Failed { .. })
    ));
    assert!(inspect_recorded_build_plan(&operation, &build_plan, &store)
        .await
        .unwrap()
        .is_none());

    let changed_plan = execute_recorded_build_plan(
        &operation,
        &plan("content-addressed"),
        &source,
        true,
        Arc::clone(&store),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        changed_plan,
        BuildPlanExecutionError::Receipt(BuildReceiptError::Conflict { .. })
    ));

    assert!(remove_recorded_build_plan(&operation, &build_plan, &store)
        .await
        .unwrap());
    assert!(!receipts.receipt_path(operation.operation_id()).exists());
}

#[tokio::test]
async fn recorded_build_fails_closed_for_corrupt_receipt_or_missing_output() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    let store_root = temporary.path().join("images");
    write_source(&source);

    let operation = identity("cloud-build-run-3", 'a');
    let build_plan = plan("disabled");
    let store = Arc::new(ImageStore::new(&store_root, 64 * 1024 * 1024).unwrap());
    let receipts = BuildOperationJournal::for_image_store(&store, operation.operation_id())
        .await
        .unwrap();
    let built =
        execute_recorded_build_plan(&operation, &build_plan, &source, true, Arc::clone(&store))
            .await
            .unwrap();

    let receipt_path = receipts.receipt_path(operation.operation_id());
    let valid_receipt = std::fs::read(&receipt_path).unwrap();
    std::fs::write(&receipt_path, br#"{"schema":"unknown"}"#).unwrap();
    let corrupt = inspect_recorded_build_plan(&operation, &build_plan, &store)
        .await
        .unwrap_err();
    assert!(matches!(
        corrupt,
        BuildPlanExecutionError::Receipt(BuildReceiptError::InvalidReceipt { .. })
    ));

    std::fs::write(&receipt_path, valid_receipt).unwrap();
    store.remove(&built.receipt.output.reference).await.unwrap();
    let missing = inspect_recorded_build_plan(&operation, &build_plan, &store)
        .await
        .unwrap_err();
    assert!(matches!(
        missing,
        BuildPlanExecutionError::Receipt(BuildReceiptError::OutputMissing { .. })
    ));
}

#[tokio::test]
async fn recorded_build_revalidates_the_exact_blob_inventory_and_bytes() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    let store_root = temporary.path().join("images");
    write_source(&source);

    let operation = identity("cloud-build-run-inventory", 'a');
    let build_plan = plan("disabled");
    let store = Arc::new(ImageStore::new(&store_root, 64 * 1024 * 1024).unwrap());
    let built =
        execute_recorded_build_plan(&operation, &build_plan, &source, true, Arc::clone(&store))
            .await
            .unwrap();
    assert_eq!(
        built.receipt.output.blob_inventory_digest,
        built.output.blob_inventory_digest
    );

    let blob_root = built.output.layout_directory.join("blobs").join("sha256");
    let unreferenced = blob_root.join("f".repeat(64));
    std::fs::write(&unreferenced, b"unreferenced").unwrap();
    let extra = inspect_recorded_build_plan(&operation, &build_plan, &store)
        .await
        .unwrap_err();
    assert!(matches!(
        extra,
        BuildPlanExecutionError::Receipt(BuildReceiptError::OutputInvalid { .. })
    ));
    std::fs::remove_file(unreferenced).unwrap();

    let manifest_hex = built
        .output
        .descriptor
        .digest
        .strip_prefix("sha256:")
        .unwrap();
    let manifest_path = blob_root.join(manifest_hex);
    let mut manifest = std::fs::read(&manifest_path).unwrap();
    manifest[0] ^= 1;
    std::fs::write(manifest_path, manifest).unwrap();
    let changed = inspect_recorded_build_plan(&operation, &build_plan, &store)
        .await
        .unwrap_err();
    assert!(matches!(
        changed,
        BuildPlanExecutionError::Receipt(BuildReceiptError::OutputInvalid { .. })
    ));
}

#[tokio::test]
async fn recorded_build_cleanup_removes_receipt_and_internal_image_idempotently() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    let store_root = temporary.path().join("images");
    write_source(&source);

    let operation = identity("cloud-build-run-4", 'a');
    let build_plan = plan("disabled");
    let store = Arc::new(ImageStore::new(&store_root, 64 * 1024 * 1024).unwrap());
    let receipts = BuildOperationJournal::for_image_store(&store, operation.operation_id())
        .await
        .unwrap();
    let built =
        execute_recorded_build_plan(&operation, &build_plan, &source, true, Arc::clone(&store))
            .await
            .unwrap();
    let reference = built.receipt.output.reference.clone();
    assert!(receipts.receipt_path(operation.operation_id()).is_file());
    assert!(store.get(&reference).await.is_some());

    assert!(remove_recorded_build_plan(&operation, &build_plan, &store)
        .await
        .unwrap());
    assert!(!receipts.receipt_path(operation.operation_id()).exists());
    assert!(store.get(&reference).await.is_none());

    assert!(!remove_recorded_build_plan(&operation, &build_plan, &store)
        .await
        .unwrap());
}

#[test]
fn operation_identity_is_bounded_and_requires_a_canonical_source_digest() {
    let oversized = OperationId::new("x".repeat(256)).unwrap();
    assert!(matches!(
        BuildOperationIdentity::new(oversized, source_digest('a')),
        Err(BuildReceiptError::InvalidIdentity {
            field: "operation_id",
            ..
        })
    ));

    assert!(matches!(
        BuildOperationIdentity::new(
            OperationId::new("cloud-build-run-5").unwrap(),
            "not-a-digest",
        ),
        Err(BuildReceiptError::InvalidIdentity {
            field: "source_digest",
            ..
        })
    ));
}
