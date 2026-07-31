//! Internal cross-process journal for one recorded build operation.

use std::path::{Path, PathBuf};

use a3s_box_core::OperationId;

use super::{
    operation_key, BuildOutputReceipt, BuildReceiptError, PersistedBuildOperation,
    PersistedBuildPhase, SupervisedBuildOperation, MAX_RECEIPT_BYTES, RECEIPT_DIRECTORY,
};
use crate::file_lock::FileLock;
use crate::oci::image::{read_regular_file_bounded, validate_plain_directory};
use crate::oci::ImageStore;

/// Store co-located with one ImageStore and keyed by hashed operation IDs.
#[derive(Debug, Clone)]
pub struct BuildOperationJournal {
    root: PathBuf,
}

impl BuildOperationJournal {
    /// Open the one receipt directory associated with an ImageStore.
    pub(in crate::oci::build) async fn for_image_store(
        store: &ImageStore,
        operation_id: &OperationId,
    ) -> Result<Self, BuildReceiptError> {
        let store_root = store.store_dir().to_path_buf();
        let operation = operation_id.to_string();
        tokio::task::spawn_blocking(move || Self::open(&store_root))
            .await
            .map_err(|error| BuildReceiptError::Task {
                operation_id: operation,
                message: format!("receipt store initialization task failed: {error}"),
            })?
    }

    fn open(store_root: &Path) -> Result<Self, BuildReceiptError> {
        let store_root = store_root
            .canonicalize()
            .map_err(|error| BuildReceiptError::StoreIo {
                message: format!("failed to canonicalize ImageStore {}", store_root.display()),
                source: error,
            })?;
        let root = store_root.join(RECEIPT_DIRECTORY).join("sha256");
        std::fs::create_dir_all(&root).map_err(|error| BuildReceiptError::StoreIo {
            message: format!("failed to create receipt directory {}", root.display()),
            source: error,
        })?;
        validate_plain_directory(&root, "build receipt").map_err(|error| {
            BuildReceiptError::UnsafeStore {
                message: error.to_string(),
            }
        })?;
        let canonical = root
            .canonicalize()
            .map_err(|error| BuildReceiptError::StoreIo {
                message: format!(
                    "failed to canonicalize receipt directory {}",
                    root.display()
                ),
                source: error,
            })?;
        if !canonical.starts_with(&store_root) {
            return Err(BuildReceiptError::UnsafeStore {
                message: format!(
                    "receipt directory {} escaped ImageStore {}",
                    canonical.display(),
                    store_root.display()
                ),
            });
        }
        Ok(Self { root: canonical })
    }

    pub(super) fn receipt_path(&self, operation_id: &OperationId) -> PathBuf {
        self.root
            .join(format!("{}.json", operation_key(operation_id)))
    }

    pub(super) fn workspace_path(&self, operation_id: &OperationId) -> PathBuf {
        self.root
            .join(format!("{}.workspace", operation_key(operation_id)))
    }

    fn execution_lock_target(&self, operation_id: &OperationId) -> PathBuf {
        self.root
            .join(format!("{}.execution", operation_key(operation_id)))
    }

    pub(in crate::oci::build) async fn lock(
        &self,
        operation_id: &OperationId,
    ) -> Result<LockedBuildOperation, BuildReceiptError> {
        let path = self.receipt_path(operation_id);
        let lock_target = path.clone();
        let operation = operation_id.to_string();
        let lock = tokio::task::spawn_blocking(move || FileLock::acquire(&lock_target))
            .await
            .map_err(|error| BuildReceiptError::Task {
                operation_id: operation.clone(),
                message: format!("receipt lock task failed: {error}"),
            })?
            .map_err(|error| BuildReceiptError::StoreIo {
                message: format!("failed to lock receipt for operation {operation}"),
                source: error,
            })?;
        Ok(LockedBuildOperation {
            path,
            operation_id: operation_id.clone(),
            _lock: lock,
        })
    }

    /// Try to own execution without blocking state inspection or cancellation.
    ///
    /// This uses the same journal and shared [`FileLock`] primitive as receipt
    /// mutation. The crash-released lease is liveness evidence only; the JSON
    /// record remains the sole operation state.
    pub(in crate::oci::build) async fn try_execution_lease(
        &self,
        operation_id: &OperationId,
    ) -> Result<Option<BuildExecutionLease>, BuildReceiptError> {
        let target = self.execution_lock_target(operation_id);
        let lock_target = target.clone();
        let operation = operation_id.to_string();
        let lock = tokio::task::spawn_blocking(move || FileLock::try_acquire(&lock_target))
            .await
            .map_err(|error| BuildReceiptError::Task {
                operation_id: operation.clone(),
                message: format!("execution lease task failed: {error}"),
            })?
            .map_err(|error| BuildReceiptError::StoreIo {
                message: format!("failed to inspect execution lease for operation {operation}"),
                source: error,
            })?;
        Ok(lock.map(|lock| BuildExecutionLease { _lock: lock }))
    }

    pub(in crate::oci::build) async fn prepare_workspace(
        &self,
        operation_id: &OperationId,
    ) -> Result<PathBuf, BuildReceiptError> {
        let root = self.root.clone();
        let workspace = self.workspace_path(operation_id);
        let operation = operation_id.to_string();
        tokio::task::spawn_blocking(move || {
            remove_workspace_if_present(&root, &workspace, &operation)?;
            std::fs::create_dir(&workspace).map_err(|source| BuildReceiptError::StoreIo {
                message: format!("failed to create workspace for operation {operation}"),
                source,
            })?;
            validate_workspace(&root, &workspace, &operation)
        })
        .await
        .map_err(|error| BuildReceiptError::Task {
            operation_id: operation_id.to_string(),
            message: format!("workspace preparation task failed: {error}"),
        })?
    }

    pub(in crate::oci::build) async fn cleanup_workspace(
        &self,
        operation_id: &OperationId,
    ) -> Result<(), BuildReceiptError> {
        let root = self.root.clone();
        let workspace = self.workspace_path(operation_id);
        let operation = operation_id.to_string();
        tokio::task::spawn_blocking(move || {
            remove_workspace_if_present(&root, &workspace, &operation)
        })
        .await
        .map_err(|error| BuildReceiptError::Task {
            operation_id: operation_id.to_string(),
            message: format!("workspace cleanup task failed: {error}"),
        })?
    }
}

/// Crash-released proof that exactly one native engine owns an operation.
pub(in crate::oci::build) struct BuildExecutionLease {
    _lock: FileLock,
}

pub(in crate::oci::build) struct LockedBuildOperation {
    path: PathBuf,
    operation_id: OperationId,
    _lock: FileLock,
}

impl LockedBuildOperation {
    pub(in crate::oci::build) async fn read(
        &self,
    ) -> Result<Option<PersistedBuildOperation>, BuildReceiptError> {
        let path = self.path.clone();
        let operation_id = self.operation_id.clone();
        tokio::task::spawn_blocking(move || read_receipt_file(&path, &operation_id))
            .await
            .map_err(|error| BuildReceiptError::Task {
                operation_id: self.operation_id.to_string(),
                message: format!("receipt read task failed: {error}"),
            })?
    }

    pub(in crate::oci::build) async fn write_succeeded(
        &self,
        receipt: BuildOutputReceipt,
    ) -> Result<BuildOutputReceipt, BuildReceiptError> {
        let path = self.path.clone();
        let operation_id = self.operation_id.clone();
        tokio::task::spawn_blocking(move || {
            match read_receipt_file(&path, &operation_id)? {
                Some(PersistedBuildOperation::Succeeded(existing)) if existing == receipt => {
                    return Ok(existing);
                }
                Some(PersistedBuildOperation::Pending(pending))
                    if pending.matches_receipt(&receipt) => {}
                Some(PersistedBuildOperation::Supervised(operation))
                    if operation.matches_receipt(&receipt)
                        && matches!(
                            operation.phase,
                            PersistedBuildPhase::Running | PersistedBuildPhase::Cancelling
                        ) => {}
                Some(_) => {
                    return Err(BuildReceiptError::Conflict {
                        operation_id: operation_id.to_string(),
                        message: "a different terminal receipt already exists".to_string(),
                    })
                }
                None => {
                    return Err(BuildReceiptError::Conflict {
                        operation_id: operation_id.to_string(),
                        message: "terminal receipt has no persisted build intent".to_string(),
                    })
                }
            }
            receipt.validate()?;
            if receipt.operation_id != operation_id {
                return Err(BuildReceiptError::Conflict {
                    operation_id: operation_id.to_string(),
                    message: "terminal receipt belongs to another operation".to_string(),
                });
            }
            persist_record(
                &path,
                &operation_id,
                &PersistedBuildOperation::Succeeded(receipt.clone()),
            )?;
            Ok(receipt)
        })
        .await
        .map_err(|error| BuildReceiptError::Task {
            operation_id: self.operation_id.to_string(),
            message: format!("receipt write task failed: {error}"),
        })?
    }

    pub(in crate::oci::build) async fn write_supervised(
        &self,
        operation: SupervisedBuildOperation,
    ) -> Result<SupervisedBuildOperation, BuildReceiptError> {
        let path = self.path.clone();
        let operation_id = self.operation_id.clone();
        tokio::task::spawn_blocking(move || {
            operation.validate()?;
            if operation.operation_id != operation_id {
                return Err(BuildReceiptError::Conflict {
                    operation_id: operation_id.to_string(),
                    message: "supervised record belongs to another operation".to_string(),
                });
            }
            match read_receipt_file(&path, &operation_id)? {
                None if operation.phase == PersistedBuildPhase::Running => {}
                Some(PersistedBuildOperation::Pending(pending))
                    if pending.operation_id == operation.operation_id
                        && pending.source_digest == operation.source_digest
                        && pending.plan_digest == operation.plan_digest
                        && pending.output_reference == operation.output_reference
                        && operation.phase == PersistedBuildPhase::Running => {}
                Some(PersistedBuildOperation::Supervised(existing))
                    if valid_supervised_transition(&existing, &operation) => {}
                Some(PersistedBuildOperation::Supervised(existing)) if existing == operation => {
                    return Ok(existing);
                }
                Some(_) | None => {
                    return Err(BuildReceiptError::Conflict {
                        operation_id: operation_id.to_string(),
                        message: "invalid supervised build state transition".to_string(),
                    })
                }
            }
            persist_record(
                &path,
                &operation_id,
                &PersistedBuildOperation::Supervised(operation.clone()),
            )?;
            Ok(operation)
        })
        .await
        .map_err(|error| BuildReceiptError::Task {
            operation_id: self.operation_id.to_string(),
            message: format!("supervised receipt write task failed: {error}"),
        })?
    }

    pub(in crate::oci::build) async fn delete(&self) -> Result<(), BuildReceiptError> {
        let path = self.path.clone();
        let operation_id = self.operation_id.to_string();
        tokio::task::spawn_blocking(move || match std::fs::remove_file(&path) {
            Ok(()) => {
                if let Some(parent) = path.parent() {
                    if let Ok(directory) = std::fs::File::open(parent) {
                        let _ = directory.sync_all();
                    }
                }
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(BuildReceiptError::StoreIo {
                message: format!("failed to remove receipt for operation {operation_id}"),
                source: error,
            }),
        })
        .await
        .map_err(|error| BuildReceiptError::Task {
            operation_id: self.operation_id.to_string(),
            message: format!("receipt removal task failed: {error}"),
        })?
    }
}

fn valid_supervised_transition(
    existing: &SupervisedBuildOperation,
    next: &SupervisedBuildOperation,
) -> bool {
    if existing.operation_id != next.operation_id
        || existing.source_digest != next.source_digest
        || existing.plan_digest != next.plan_digest
        || existing.output_reference != next.output_reference
        || existing.started_at != next.started_at
        || existing.owner != next.owner
    {
        return false;
    }
    matches!(
        (existing.phase, next.phase),
        (
            PersistedBuildPhase::Running,
            PersistedBuildPhase::Running
                | PersistedBuildPhase::Cancelling
                | PersistedBuildPhase::Cancelled
                | PersistedBuildPhase::Failed
        ) | (
            PersistedBuildPhase::Cancelling,
            PersistedBuildPhase::Cancelling
                | PersistedBuildPhase::Cancelled
                | PersistedBuildPhase::Failed
        )
    )
}

fn read_receipt_file(
    path: &Path,
    expected_operation_id: &OperationId,
) -> Result<Option<PersistedBuildOperation>, BuildReceiptError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(BuildReceiptError::InvalidReceipt {
                operation_id: expected_operation_id.to_string(),
                message: "receipt path is not a regular file".to_string(),
            });
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(BuildReceiptError::StoreIo {
                message: format!("failed to inspect receipt {}", path.display()),
                source: error,
            })
        }
    }
    let bytes =
        read_regular_file_bounded(path, MAX_RECEIPT_BYTES, "build receipt").map_err(|error| {
            BuildReceiptError::InvalidReceipt {
                operation_id: expected_operation_id.to_string(),
                message: error.to_string(),
            }
        })?;
    let receipt: PersistedBuildOperation =
        serde_json::from_slice(&bytes).map_err(|error| BuildReceiptError::InvalidReceipt {
            operation_id: expected_operation_id.to_string(),
            message: format!("JSON or schema validation failed: {error}"),
        })?;
    receipt.validate()?;
    if receipt.operation_id() != expected_operation_id {
        return Err(BuildReceiptError::InvalidReceipt {
            operation_id: expected_operation_id.to_string(),
            message: "hashed receipt path contains another operation identity".to_string(),
        });
    }
    Ok(Some(receipt))
}

fn persist_record(
    path: &Path,
    operation_id: &OperationId,
    record: &PersistedBuildOperation,
) -> Result<(), BuildReceiptError> {
    record.validate()?;
    let mut bytes =
        serde_json::to_vec_pretty(record).map_err(|error| BuildReceiptError::InvalidReceipt {
            operation_id: operation_id.to_string(),
            message: format!("serialization failed: {error}"),
        })?;
    bytes.push(b'\n');
    let temporary = path.with_extension("json.tmp");
    a3s_box_core::fs_atomic::write_durable(&temporary, path, &bytes).map_err(|error| {
        BuildReceiptError::StoreIo {
            message: format!("failed to persist receipt for operation {operation_id}"),
            source: error,
        }
    })
}

fn validate_workspace(
    root: &Path,
    workspace: &Path,
    operation_id: &str,
) -> Result<PathBuf, BuildReceiptError> {
    validate_plain_directory(workspace, "build operation workspace").map_err(|error| {
        BuildReceiptError::UnsafeStore {
            message: error.to_string(),
        }
    })?;
    let canonical = workspace
        .canonicalize()
        .map_err(|source| BuildReceiptError::StoreIo {
            message: format!("failed to canonicalize workspace for operation {operation_id}"),
            source,
        })?;
    if canonical.parent() != Some(root) {
        return Err(BuildReceiptError::UnsafeStore {
            message: format!(
                "workspace {} escaped receipt journal {}",
                canonical.display(),
                root.display()
            ),
        });
    }
    Ok(canonical)
}

fn remove_workspace_if_present(
    root: &Path,
    workspace: &Path,
    operation_id: &str,
) -> Result<(), BuildReceiptError> {
    match std::fs::symlink_metadata(workspace) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(BuildReceiptError::UnsafeStore {
                message: format!(
                    "workspace path for operation {operation_id} is not a plain directory"
                ),
            })
        }
        Ok(_) => {
            validate_workspace(root, workspace, operation_id)?;
            std::fs::remove_dir_all(workspace).map_err(|source| BuildReceiptError::StoreIo {
                message: format!("failed to remove workspace for operation {operation_id}"),
                source,
            })?;
            if let Ok(directory) = std::fs::File::open(root) {
                let _ = directory.sync_all();
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(BuildReceiptError::StoreIo {
            message: format!("failed to inspect workspace for operation {operation_id}"),
            source,
        }),
    }
}
