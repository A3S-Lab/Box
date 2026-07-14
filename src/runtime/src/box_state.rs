//! Durable local state store for box execution records.

use std::path::{Path, PathBuf};

use crate::file_lock::FileLock;
use crate::store_io::quarantine_label;
use crate::BoxRecord;
use a3s_box_core::{ExecutionId, OperationId};

/// Durable collection of local box execution records.
///
/// All mutating operations use the sibling `boxes.json.lock` advisory lock and
/// a durable temporary-file rename. Callers must keep transaction closures
/// synchronous and must not acquire the same store lock recursively.
#[derive(Debug)]
pub struct BoxStateStore {
    path: PathBuf,
    records: Vec<BoxRecord>,
}

impl BoxStateStore {
    /// Build an in-memory store for `path` from existing records.
    pub fn from_records(path: impl Into<PathBuf>, records: Vec<BoxRecord>) -> Self {
        Self {
            path: path.into(),
            records,
        }
    }

    /// Load state strictly, returning invalid JSON or schema data as an error.
    ///
    /// A missing state file is represented by an empty store and its parent
    /// directory is created for subsequent writes.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        Self::load_unlocked(path, CorruptionPolicy::ReturnError, true)
    }

    /// Load state and preserve an invalid file as a timestamped sibling.
    ///
    /// This compatibility path keeps the CLI available for manual recovery.
    /// New runtime services should prefer [`Self::load`] and fail closed.
    pub fn load_or_quarantine(path: &Path) -> std::io::Result<Self> {
        Self::load_unlocked(path, CorruptionPolicy::Quarantine, true)
    }

    /// Load a side-effect-free snapshot.
    ///
    /// This never creates directories, quarantines invalid data, acquires a
    /// lock, reconciles process state, or writes the file back.
    pub fn load_readonly(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        Self::load_unlocked(&path, CorruptionPolicy::ReturnError, false)
    }

    /// Save this snapshot under the cross-process state lock.
    pub fn save(&self) -> std::io::Result<()> {
        let _lock = FileLock::acquire(&self.path)?;
        self.write_unlocked()
    }

    /// Apply a strict atomic read-modify-write transaction.
    ///
    /// The closure runs while the cross-process lock is held. If it returns an
    /// error, no write is performed.
    pub fn modify<R>(
        path: &Path,
        f: impl FnOnce(&mut Self) -> std::io::Result<R>,
    ) -> std::io::Result<R> {
        Self::modify_with_policy(path, CorruptionPolicy::ReturnError, f)
    }

    /// Apply a strict atomic transaction with a caller-defined error type.
    ///
    /// This is equivalent to [`Self::modify`] but lets a domain repository
    /// return typed conflicts while still converting state I/O errors.
    pub fn transact<R, E>(path: &Path, f: impl FnOnce(&mut Self) -> Result<R, E>) -> Result<R, E>
    where
        E: From<std::io::Error>,
    {
        Self::modify_with_policy(path, CorruptionPolicy::ReturnError, f)
    }

    /// Apply an atomic read-modify-write transaction that quarantines invalid
    /// existing state before starting from an empty collection.
    ///
    /// This exists for CLI behavior compatibility. Runtime services should use
    /// [`Self::modify`] so corrupt durable state fails closed.
    pub fn modify_or_quarantine<R, E>(
        path: &Path,
        f: impl FnOnce(&mut Self) -> Result<R, E>,
    ) -> Result<R, E>
    where
        E: From<std::io::Error>,
    {
        Self::modify_with_policy(path, CorruptionPolicy::Quarantine, f)
    }

    fn modify_with_policy<R, E>(
        path: &Path,
        policy: CorruptionPolicy,
        f: impl FnOnce(&mut Self) -> Result<R, E>,
    ) -> Result<R, E>
    where
        E: From<std::io::Error>,
    {
        let _lock = FileLock::acquire(path).map_err(E::from)?;
        let mut store = Self::load_unlocked(path, policy, true).map_err(E::from)?;
        let output = f(&mut store)?;
        store.write_unlocked().map_err(E::from)?;
        Ok(output)
    }

    fn load_unlocked(
        path: &Path,
        corruption_policy: CorruptionPolicy,
        create_parent: bool,
    ) -> std::io::Result<Self> {
        if !path.exists() {
            if create_parent {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            return Ok(Self::from_records(path.to_path_buf(), Vec::new()));
        }

        let data = std::fs::read_to_string(path)?;
        let parsed = serde_json::from_str::<Vec<BoxRecord>>(&data)
            .map_err(|error| error.to_string())
            .and_then(|records| {
                validate_managed_records(&records)?;
                Ok(records)
            });
        match parsed {
            Ok(records) => Ok(Self::from_records(path.to_path_buf(), records)),
            Err(error) if corruption_policy == CorruptionPolicy::ReturnError => {
                Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
            }
            Err(error) => {
                let preserved = quarantine_label(path);
                eprintln!(
                    "a3s-box: WARNING: state file {} is corrupt ({error}); preserved a \
                     copy at {preserved} and started from empty state. Running boxes are \
                     no longer tracked; repair and restore the preserved records, then \
                     reconcile state. Otherwise remove leaked executions manually.",
                    path.display(),
                );
                Ok(Self::from_records(path.to_path_buf(), Vec::new()))
            }
        }
    }

    fn write_unlocked(&self) -> std::io::Result<()> {
        validate_managed_records(&self.records)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_vec_pretty(&self.records).map_err(std::io::Error::other)?;
        let temporary_path = self.path.with_extension("json.tmp");
        a3s_box_core::fs_atomic::write_durable(&temporary_path, &self.path, &data)
    }

    /// Path of the durable state file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// All execution records in persistence order.
    pub fn records(&self) -> &[BoxRecord] {
        &self.records
    }

    /// Mutable execution records for a synchronous transaction.
    pub fn records_mut(&mut self) -> &mut Vec<BoxRecord> {
        &mut self.records
    }

    /// Find a record by exact execution ID.
    pub fn find_by_id(&self, id: &str) -> Option<&BoxRecord> {
        self.records.iter().find(|record| record.id == id)
    }

    /// Find a mutable record by exact execution ID.
    pub fn find_by_id_mut(&mut self, id: &str) -> Option<&mut BoxRecord> {
        self.records.iter_mut().find(|record| record.id == id)
    }

    /// Remove a record by exact execution ID.
    pub fn remove_by_id(&mut self, id: &str) -> bool {
        let previous_len = self.records.len();
        self.records.retain(|record| record.id != id);
        self.records.len() < previous_len
    }

    /// Find a record by exact user-visible name.
    pub fn find_by_name(&self, name: &str) -> Option<&BoxRecord> {
        self.records.iter().find(|record| record.name == name)
    }

    /// Find a managed execution by its idempotent creation operation.
    pub fn find_by_operation_id(&self, operation_id: &OperationId) -> Option<&BoxRecord> {
        self.records.iter().find(|record| {
            record
                .managed_execution
                .as_ref()
                .is_some_and(|metadata| &metadata.operation_id == operation_id)
        })
    }

    /// Find a mutable managed execution by its idempotent creation operation.
    pub fn find_by_operation_id_mut(
        &mut self,
        operation_id: &OperationId,
    ) -> Option<&mut BoxRecord> {
        self.records.iter_mut().find(|record| {
            record
                .managed_execution
                .as_ref()
                .is_some_and(|metadata| &metadata.operation_id == operation_id)
        })
    }

    /// Find records matching a full-ID or short-ID prefix.
    pub fn find_by_id_prefix(&self, prefix: &str) -> Vec<&BoxRecord> {
        self.records
            .iter()
            .filter(|record| record.id.starts_with(prefix) || record.short_id.starts_with(prefix))
            .collect()
    }

    /// List all records or only records in the running state.
    pub fn list(&self, all: bool) -> Vec<&BoxRecord> {
        self.records
            .iter()
            .filter(|record| all || record.status == "running")
            .collect()
    }
}

fn validate_managed_records(records: &[BoxRecord]) -> Result<(), String> {
    let mut operation_ids = std::collections::HashSet::new();
    for record in records {
        let Some(metadata) = &record.managed_execution else {
            continue;
        };
        metadata
            .validate()
            .map_err(|error| format!("invalid managed execution {}: {error}", record.id))?;
        ExecutionId::new(record.id.clone())
            .map_err(|error| format!("invalid managed execution {}: {error}", record.id))?;
        record
            .managed_state()
            .map_err(|error| format!("invalid managed execution {}: {error}", record.id))?;
        if !operation_ids.insert(metadata.operation_id.clone()) {
            return Err(format!(
                "duplicate managed operation ID: {}",
                metadata.operation_id
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CorruptionPolicy {
    ReturnError,
    Quarantine,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str) -> BoxRecord {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "short_id": BoxRecord::make_short_id(id),
            "name": format!("box-{id}"),
            "image": "alpine:latest",
            "status": "created",
            "pid": null,
            "cpus": 1,
            "memory_mb": 128,
            "volumes": [],
            "env": {},
            "cmd": ["sh"],
            "box_dir": format!("/tmp/{id}"),
            "console_log": format!("/tmp/{id}/console.log"),
            "created_at": "2026-07-14T12:00:00Z",
            "started_at": null,
            "auto_remove": false
        }))
        .unwrap()
    }

    fn managed_record(id: &str, operation_id: OperationId) -> BoxRecord {
        let mut record = record(id);
        let config = a3s_box_core::BoxConfig {
            image: "alpine:latest".to_string(),
            isolation: a3s_box_core::ExecutionIsolation::Sandbox,
            ..Default::default()
        };
        record.managed_execution = Some(
            crate::ManagedExecutionMetadata::new(
                operation_id,
                a3s_box_core::ExecutionGeneration::INITIAL,
                a3s_box_core::CreateExecutionRequest {
                    external_sandbox_id: format!("sandbox-{id}"),
                    config,
                    labels: Default::default(),
                },
            )
            .unwrap(),
        );
        record
    }

    #[test]
    fn missing_state_is_empty_and_creates_parent() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested").join("boxes.json");

        let store = BoxStateStore::load(&path).unwrap();

        assert!(store.records().is_empty());
        assert!(path.parent().unwrap().exists());
        assert!(!path.exists());
    }

    #[test]
    fn strict_load_reports_corruption_without_moving_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("boxes.json");
        std::fs::write(&path, "invalid json").unwrap();

        let error = BoxStateStore::load(&path).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "invalid json");
    }

    #[test]
    fn compatibility_load_quarantines_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("boxes.json");
        std::fs::write(&path, "invalid json").unwrap();

        let store = BoxStateStore::load_or_quarantine(&path).unwrap();

        assert!(store.records().is_empty());
        assert!(!path.exists());
        let backups: Vec<_> = std::fs::read_dir(directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".corrupt-"))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(
            std::fs::read_to_string(backups[0].path()).unwrap(),
            "invalid json"
        );
    }

    #[test]
    fn failed_transaction_does_not_write_mutations() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("boxes.json");
        BoxStateStore::from_records(path.clone(), vec![record("original")])
            .save()
            .unwrap();

        let result = BoxStateStore::modify(&path, |store| {
            store.records_mut().push(record("discarded"));
            Err::<(), _>(std::io::Error::other("abort"))
        });

        assert!(result.is_err());
        let persisted = BoxStateStore::load(&path).unwrap();
        assert_eq!(persisted.records().len(), 1);
        assert_eq!(persisted.records()[0].id, "original");
    }

    #[test]
    fn save_preserves_runtime_owned_fields() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("boxes.json");
        let mut value = record("runtime-field");
        value.virtiofs_cache = Some("always".to_string());

        BoxStateStore::from_records(path.clone(), vec![value])
            .save()
            .unwrap();

        let persisted = BoxStateStore::load(&path).unwrap();
        assert_eq!(
            persisted.records()[0].virtiofs_cache.as_deref(),
            Some("always")
        );
    }

    #[test]
    fn operation_lookup_ignores_legacy_records_and_finds_managed_intent() {
        let operation_id = OperationId::new("operation-1").unwrap();
        let managed = managed_record("managed", operation_id.clone());
        let mut store =
            BoxStateStore::from_records("/tmp/boxes.json", vec![record("legacy"), managed]);

        assert_eq!(
            store.find_by_operation_id(&operation_id).unwrap().id,
            "managed"
        );
        store
            .find_by_operation_id_mut(&operation_id)
            .unwrap()
            .status = "running".to_string();
        assert_eq!(store.find_by_id("managed").unwrap().status, "running");
        assert!(store
            .find_by_operation_id(&OperationId::new("missing").unwrap())
            .is_none());
    }

    #[test]
    fn strict_load_rejects_duplicate_managed_operation_ids() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("boxes.json");
        let operation_id = OperationId::new("operation-1").unwrap();
        let records = vec![
            managed_record("first", operation_id.clone()),
            managed_record("second", operation_id),
        ];
        std::fs::write(&path, serde_json::to_vec(&records).unwrap()).unwrap();

        let error = BoxStateStore::load(&path).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("duplicate managed operation ID"));
    }

    #[test]
    fn transaction_rejects_duplicate_operation_without_changing_disk() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("boxes.json");
        let operation_id = OperationId::new("operation-1").unwrap();
        BoxStateStore::from_records(
            path.clone(),
            vec![managed_record("first", operation_id.clone())],
        )
        .save()
        .unwrap();

        let error = BoxStateStore::modify(&path, |store| {
            store
                .records_mut()
                .push(managed_record("second", operation_id));
            Ok(())
        })
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        let persisted = BoxStateStore::load(&path).unwrap();
        assert_eq!(persisted.records().len(), 1);
        assert_eq!(persisted.records()[0].id, "first");
    }

    #[test]
    fn strict_load_rejects_managed_plan_drift() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("boxes.json");
        let mut record = managed_record("managed", OperationId::new("operation-1").unwrap());
        record.managed_execution.as_mut().unwrap().plan =
            a3s_box_core::resolve_execution(&a3s_box_core::BoxConfig::default()).unwrap();
        std::fs::write(&path, serde_json::to_vec(&vec![record]).unwrap()).unwrap();

        let error = BoxStateStore::load(&path).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("does not match"));
    }

    #[test]
    fn strict_load_rejects_unknown_managed_lifecycle_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("boxes.json");
        let mut record = managed_record("managed", OperationId::new("operation-1").unwrap());
        record.status = "future-state".to_string();
        std::fs::write(&path, serde_json::to_vec(&vec![record]).unwrap()).unwrap();

        let error = BoxStateStore::load(&path).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error
            .to_string()
            .contains("unknown managed execution state"));
    }

    #[test]
    fn strict_load_rejects_transition_without_matching_pending_operation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("boxes.json");
        let mut record = managed_record("managed", OperationId::new("operation-1").unwrap());
        record.status = "pausing".to_string();
        std::fs::write(&path, serde_json::to_vec(&vec![record]).unwrap()).unwrap();

        let error = BoxStateStore::load(&path).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("inconsistent pending operation"));
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_transactions_do_not_lose_records() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("boxes.json");
        let handles: Vec<_> = (0..8)
            .map(|index| {
                let path = path.clone();
                std::thread::spawn(move || {
                    BoxStateStore::modify(&path, |store| {
                        store.records_mut().push(record(&format!("id-{index}")));
                        Ok::<(), std::io::Error>(())
                    })
                    .unwrap();
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        let store = BoxStateStore::load(&path).unwrap();
        assert_eq!(store.records().len(), 8);
    }
}
