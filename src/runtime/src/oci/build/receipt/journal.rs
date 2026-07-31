//! Internal cross-process journal for one recorded build operation.

use std::path::{Path, PathBuf};

use a3s_box_core::OperationId;

use super::{
    operation_key, BuildOutputReceipt, BuildReceiptError, PendingBuildOperation,
    PersistedBuildOperation, MAX_RECEIPT_BYTES, RECEIPT_DIRECTORY,
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

    pub(in crate::oci::build) async fn write_pending(
        &self,
        pending: PendingBuildOperation,
    ) -> Result<PendingBuildOperation, BuildReceiptError> {
        let path = self.path.clone();
        let operation_id = self.operation_id.clone();
        tokio::task::spawn_blocking(move || {
            if let Some(existing) = read_receipt_file(&path, &operation_id)? {
                if existing == PersistedBuildOperation::Pending(pending.clone()) {
                    return Ok(pending);
                }
                return Err(BuildReceiptError::Conflict {
                    operation_id: operation_id.to_string(),
                    message: "a different build operation record already exists".to_string(),
                });
            }
            pending.validate()?;
            if pending.operation_id != operation_id {
                return Err(BuildReceiptError::Conflict {
                    operation_id: operation_id.to_string(),
                    message: "pending intent belongs to another operation".to_string(),
                });
            }
            persist_record(
                &path,
                &operation_id,
                &PersistedBuildOperation::Pending(pending.clone()),
            )?;
            Ok(pending)
        })
        .await
        .map_err(|error| BuildReceiptError::Task {
            operation_id: self.operation_id.to_string(),
            message: format!("pending receipt write task failed: {error}"),
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
