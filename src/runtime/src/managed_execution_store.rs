//! Durable generation-fenced transitions for managed local executions.

use std::path::{Path, PathBuf};

use a3s_box_core::{ExecutionGeneration, ExecutionId, OperationId};
use thiserror::Error;

use crate::{
    BoxRecord, BoxStateStore, ManagedExecutionOperation, ManagedExecutionState,
    ManagedRestartCompletion, ManagedRestartOutcome,
};

/// Strict durable repository used by the local `ExecutionManager`.
#[derive(Debug, Clone)]
pub struct ManagedExecutionStore {
    path: PathBuf,
}

/// Result of reserving an idempotent create operation.
#[derive(Debug, Clone)]
pub enum ManagedExecutionReservation {
    /// The creation intent was inserted by this call.
    Reserved(BoxRecord),
    /// The operation already existed with the same creation intent.
    Existing(BoxRecord),
}

impl ManagedExecutionReservation {
    pub const fn is_new(&self) -> bool {
        matches!(self, Self::Reserved(_))
    }

    pub fn record(&self) -> &BoxRecord {
        match self {
            Self::Reserved(record) | Self::Existing(record) => record,
        }
    }

    pub fn into_record(self) -> BoxRecord {
        match self {
            Self::Reserved(record) | Self::Existing(record) => record,
        }
    }
}

/// Fail-closed errors from managed lifecycle persistence.
#[derive(Debug, Error)]
pub enum ManagedExecutionStoreError {
    #[error("managed execution state I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("managed execution not found: {0}")]
    NotFound(ExecutionId),
    #[error("execution record is not managed: {0}")]
    Unmanaged(ExecutionId),
    #[error("managed execution conflict for {execution_id}: {message}")]
    Conflict {
        execution_id: ExecutionId,
        message: String,
    },
    #[error("invalid managed execution record: {0}")]
    InvalidRecord(String),
    #[error("invalid managed execution transition for {execution_id}: {from} -> {to}")]
    InvalidTransition {
        execution_id: ExecutionId,
        from: ManagedExecutionState,
        to: ManagedExecutionState,
    },
}

pub type ManagedExecutionStoreResult<T> = std::result::Result<T, ManagedExecutionStoreError>;

impl ManagedExecutionStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return one managed record without mutating or reconciling state.
    pub fn get(
        &self,
        execution_id: &ExecutionId,
    ) -> ManagedExecutionStoreResult<Option<BoxRecord>> {
        let store = BoxStateStore::load_readonly(&self.path)?;
        let Some(record) = store.find_by_id(execution_id.as_str()).cloned() else {
            return Ok(None);
        };
        if record.managed_execution.is_none() {
            return Err(ManagedExecutionStoreError::Unmanaged(execution_id.clone()));
        }
        Ok(Some(record))
    }

    /// Return every managed record without reconciling provider state.
    ///
    /// Legacy CLI records share the same state file and are deliberately
    /// excluded. Loading remains strict: one malformed managed record fails the
    /// complete snapshot instead of letting provider discovery skip corrupt
    /// ownership metadata.
    pub fn list(&self) -> ManagedExecutionStoreResult<Vec<BoxRecord>> {
        let store = BoxStateStore::load_readonly(&self.path)?;
        Ok(store
            .records()
            .iter()
            .filter(|record| record.managed_execution.is_some())
            .cloned()
            .collect())
    }

    /// Return the record reserved by an idempotent creation operation.
    pub fn get_by_operation_id(
        &self,
        operation_id: &OperationId,
    ) -> ManagedExecutionStoreResult<Option<BoxRecord>> {
        let store = BoxStateStore::load_readonly(&self.path)?;
        Ok(store.find_by_operation_id(operation_id).cloned())
    }

    /// Atomically reserve one creation operation before backend side effects.
    ///
    /// Retrying the same operation with the same full request returns the
    /// existing record. Reusing an operation ID for different creation intent
    /// fails without changing durable state.
    pub fn reserve(
        &self,
        mut record: BoxRecord,
    ) -> ManagedExecutionStoreResult<ManagedExecutionReservation> {
        let execution_id = validate_new_record(&record)?;
        record.status = ManagedExecutionState::Created.as_status().to_string();
        let incoming_metadata = record
            .managed_execution
            .as_ref()
            .ok_or_else(|| ManagedExecutionStoreError::Unmanaged(execution_id.clone()))?
            .clone();

        BoxStateStore::transact(&self.path, move |store| {
            if let Some(existing) = store
                .find_by_operation_id(&incoming_metadata.operation_id)
                .cloned()
            {
                let existing_id = ExecutionId::new(existing.id.clone()).map_err(|error| {
                    ManagedExecutionStoreError::InvalidRecord(error.to_string())
                })?;
                let existing_metadata = existing
                    .managed_execution
                    .as_ref()
                    .ok_or_else(|| ManagedExecutionStoreError::Unmanaged(existing_id.clone()))?;
                if !same_creation_intent(existing_metadata, &incoming_metadata)? {
                    return Err(ManagedExecutionStoreError::Conflict {
                        execution_id: existing_id,
                        message: format!(
                            "operation {} was already reserved with different creation intent",
                            incoming_metadata.operation_id
                        ),
                    });
                }
                return Ok(ManagedExecutionReservation::Existing(existing));
            }

            if store.find_by_id(execution_id.as_str()).is_some() {
                return Err(ManagedExecutionStoreError::Conflict {
                    execution_id,
                    message: "execution ID is already present".to_string(),
                });
            }

            store.records_mut().push(record.clone());
            Ok(ManagedExecutionReservation::Reserved(record))
        })
    }

    /// Claim terminal-record removal before deleting host resources.
    ///
    /// The durable `removing` state prevents another lifecycle operation from
    /// reviving the execution while cleanup is in progress. A retry observes
    /// the same claim and can resume cleanup after a process crash.
    pub fn begin_remove(
        &self,
        execution_id: &ExecutionId,
        expected_generation: ExecutionGeneration,
    ) -> ManagedExecutionStoreResult<Option<BoxRecord>> {
        let execution_id = execution_id.clone();
        BoxStateStore::transact(&self.path, move |store| {
            let Some(record) = store.find_by_id_mut(execution_id.as_str()) else {
                return Ok(None);
            };
            let state = record
                .managed_state()
                .map_err(|error| ManagedExecutionStoreError::InvalidRecord(error.to_string()))?
                .ok_or_else(|| ManagedExecutionStoreError::Unmanaged(execution_id.clone()))?;
            let metadata = record
                .managed_execution
                .as_mut()
                .ok_or_else(|| ManagedExecutionStoreError::Unmanaged(execution_id.clone()))?;
            if metadata.generation != expected_generation {
                return Err(ManagedExecutionStoreError::Conflict {
                    execution_id: execution_id.clone(),
                    message: format!(
                        "expected generation {}, found {}",
                        expected_generation.get(),
                        metadata.generation.get()
                    ),
                });
            }
            match state {
                ManagedExecutionState::Removing => Ok(Some(record.clone())),
                ManagedExecutionState::Created
                | ManagedExecutionState::Stopped
                | ManagedExecutionState::Failed => {
                    record.status = ManagedExecutionState::Removing.as_status().to_string();
                    metadata.pending_operation = Some(ManagedExecutionOperation::Remove);
                    Ok(Some(record.clone()))
                }
                _ => Err(ManagedExecutionStoreError::Conflict {
                    execution_id: execution_id.clone(),
                    message: format!("cannot remove execution in state {state}"),
                }),
            }
        })
    }

    /// Forget a generation only after its durable removal claim has completed.
    pub fn finish_remove(
        &self,
        execution_id: &ExecutionId,
        expected_generation: ExecutionGeneration,
    ) -> ManagedExecutionStoreResult<bool> {
        let execution_id = execution_id.clone();
        BoxStateStore::transact(&self.path, move |store| {
            let Some(record) = store.find_by_id(execution_id.as_str()) else {
                return Ok(false);
            };
            let state = record
                .managed_state()
                .map_err(|error| ManagedExecutionStoreError::InvalidRecord(error.to_string()))?
                .ok_or_else(|| ManagedExecutionStoreError::Unmanaged(execution_id.clone()))?;
            let metadata = record
                .managed_execution
                .as_ref()
                .ok_or_else(|| ManagedExecutionStoreError::Unmanaged(execution_id.clone()))?;
            if metadata.generation != expected_generation
                || state != ManagedExecutionState::Removing
            {
                return Err(ManagedExecutionStoreError::Conflict {
                    execution_id: execution_id.clone(),
                    message: format!(
                        "expected removing generation {}, found {state} generation {}",
                        expected_generation.get(),
                        metadata.generation.get()
                    ),
                });
            }
            Ok(store.remove_by_id(execution_id.as_str()))
        })
    }

    /// Atomically compare generation and state, then persist one legal edge.
    ///
    /// Completing pause and resume, and advancing a restart from teardown to
    /// startup, increments the runtime generation exactly once. Other edges
    /// retain the current generation.
    pub fn transition(
        &self,
        execution_id: &ExecutionId,
        expected_generation: ExecutionGeneration,
        expected_state: ManagedExecutionState,
        next_state: ManagedExecutionState,
    ) -> ManagedExecutionStoreResult<BoxRecord> {
        self.transition_with(
            execution_id,
            expected_generation,
            expected_state,
            next_state,
            |_| {},
        )
    }

    /// Persist one legal transition and update runtime evidence in the same
    /// transaction.
    pub fn transition_with(
        &self,
        execution_id: &ExecutionId,
        expected_generation: ExecutionGeneration,
        expected_state: ManagedExecutionState,
        next_state: ManagedExecutionState,
        update: impl FnOnce(&mut BoxRecord),
    ) -> ManagedExecutionStoreResult<BoxRecord> {
        let execution_id = execution_id.clone();
        BoxStateStore::transact(&self.path, move |store| {
            let record = store
                .find_by_id_mut(execution_id.as_str())
                .ok_or_else(|| ManagedExecutionStoreError::NotFound(execution_id.clone()))?;
            let actual_state = record
                .managed_state()
                .map_err(|error| ManagedExecutionStoreError::InvalidRecord(error.to_string()))?
                .ok_or_else(|| ManagedExecutionStoreError::Unmanaged(execution_id.clone()))?;
            let metadata = record
                .managed_execution
                .as_ref()
                .ok_or_else(|| ManagedExecutionStoreError::Unmanaged(execution_id.clone()))?;

            if metadata.generation != expected_generation || actual_state != expected_state {
                return Err(ManagedExecutionStoreError::Conflict {
                    execution_id: execution_id.clone(),
                    message: format!(
                        "expected {expected_state} generation {}, found {actual_state} generation {}",
                        expected_generation.get(),
                        metadata.generation.get()
                    ),
                });
            }

            let next_generation = transition_generation(
                &execution_id,
                expected_state,
                next_state,
                expected_generation,
            )?;
            let original_metadata = record
                .managed_execution
                .as_ref()
                .ok_or_else(|| ManagedExecutionStoreError::Unmanaged(execution_id.clone()))?
                .clone();
            update(record);
            if record.id != execution_id.as_str() {
                return Err(ManagedExecutionStoreError::InvalidRecord(format!(
                    "transition changed execution ID {execution_id}"
                )));
            }
            let updated_metadata = record
                .managed_execution
                .as_ref()
                .ok_or_else(|| ManagedExecutionStoreError::Unmanaged(execution_id.clone()))?;
            if updated_metadata.operation_id != original_metadata.operation_id
                || !same_creation_intent(updated_metadata, &original_metadata)?
            {
                return Err(ManagedExecutionStoreError::InvalidRecord(format!(
                    "transition changed creation identity for {execution_id}"
                )));
            }
            record.status = next_state.as_status().to_string();
            let metadata = record
                .managed_execution
                .as_mut()
                .ok_or_else(|| ManagedExecutionStoreError::Unmanaged(execution_id.clone()))?;
            metadata.generation = next_generation;
            if expected_state == ManagedExecutionState::RestartStarting
                && matches!(
                    next_state,
                    ManagedExecutionState::Running | ManagedExecutionState::Failed
                )
            {
                let Some(ManagedExecutionOperation::Restart {
                    operation_id,
                    source_generation,
                    stop_timeout_secs,
                    ..
                }) = metadata.pending_operation.as_ref()
                else {
                    return Err(ManagedExecutionStoreError::InvalidRecord(format!(
                        "restart completion for {execution_id} has no persisted restart intent"
                    )));
                };
                metadata.last_restart = Some(ManagedRestartCompletion {
                    operation_id: operation_id.clone(),
                    source_generation: *source_generation,
                    target_generation: next_generation,
                    outcome: if next_state == ManagedExecutionState::Running {
                        ManagedRestartOutcome::Running
                    } else {
                        ManagedRestartOutcome::Failed
                    },
                    stop_timeout_secs: *stop_timeout_secs,
                });
            }
            metadata.pending_operation = match next_state {
                ManagedExecutionState::Starting => Some(ManagedExecutionOperation::Start),
                ManagedExecutionState::Pausing => match metadata.pending_operation.take() {
                    Some(operation @ ManagedExecutionOperation::Pause { .. }) => Some(operation),
                    _ => Some(ManagedExecutionOperation::Pause { keep_memory: false }),
                },
                ManagedExecutionState::Resuming => Some(ManagedExecutionOperation::Resume),
                ManagedExecutionState::Snapshotting => match metadata.pending_operation.take() {
                    Some(operation @ ManagedExecutionOperation::Snapshot { .. }) => Some(operation),
                    _ => {
                        return Err(ManagedExecutionStoreError::InvalidRecord(format!(
                        "snapshot transition for {execution_id} has no persisted snapshot intent"
                    )))
                    }
                },
                ManagedExecutionState::Killing => Some(ManagedExecutionOperation::Kill),
                ManagedExecutionState::Removing => Some(ManagedExecutionOperation::Remove),
                ManagedExecutionState::RestartStopping | ManagedExecutionState::RestartStarting => {
                    match metadata.pending_operation.take() {
                        Some(operation @ ManagedExecutionOperation::Restart { .. }) => {
                            Some(operation)
                        }
                        _ => {
                            return Err(ManagedExecutionStoreError::InvalidRecord(format!(
                            "restart transition for {execution_id} has no persisted restart intent"
                        )))
                        }
                    }
                }
                ManagedExecutionState::Creating
                | ManagedExecutionState::Created
                | ManagedExecutionState::Running
                | ManagedExecutionState::Paused
                | ManagedExecutionState::Stopped
                | ManagedExecutionState::Failed => None,
            };
            Ok(record.clone())
        })
    }
}

fn validate_new_record(record: &BoxRecord) -> ManagedExecutionStoreResult<ExecutionId> {
    let execution_id = ExecutionId::new(record.id.clone())
        .map_err(|error| ManagedExecutionStoreError::InvalidRecord(error.to_string()))?;
    let metadata = record
        .managed_execution
        .as_ref()
        .ok_or_else(|| ManagedExecutionStoreError::Unmanaged(execution_id.clone()))?;
    metadata
        .validate()
        .map_err(|error| ManagedExecutionStoreError::InvalidRecord(error.to_string()))?;
    let state = record
        .managed_state()
        .map_err(|error| ManagedExecutionStoreError::InvalidRecord(error.to_string()))?
        .ok_or_else(|| ManagedExecutionStoreError::Unmanaged(execution_id.clone()))?;
    if state != ManagedExecutionState::Created {
        return Err(ManagedExecutionStoreError::InvalidRecord(format!(
            "new execution {execution_id} must be created, found {state}"
        )));
    }
    if metadata.generation != ExecutionGeneration::INITIAL {
        return Err(ManagedExecutionStoreError::InvalidRecord(format!(
            "new execution {execution_id} must start at generation {}",
            ExecutionGeneration::INITIAL.get()
        )));
    }
    Ok(execution_id)
}

fn same_creation_intent(
    left: &crate::ManagedExecutionMetadata,
    right: &crate::ManagedExecutionMetadata,
) -> ManagedExecutionStoreResult<bool> {
    let left_request = serde_json::to_value(&left.request)
        .map_err(|error| ManagedExecutionStoreError::InvalidRecord(error.to_string()))?;
    let right_request = serde_json::to_value(&right.request)
        .map_err(|error| ManagedExecutionStoreError::InvalidRecord(error.to_string()))?;
    Ok(left_request == right_request && left.plan == right.plan)
}

fn transition_generation(
    execution_id: &ExecutionId,
    from: ManagedExecutionState,
    to: ManagedExecutionState,
    current: ExecutionGeneration,
) -> ManagedExecutionStoreResult<ExecutionGeneration> {
    use ManagedExecutionState::{
        Created, Creating, Failed, Killing, Paused, Pausing, RestartStarting, RestartStopping,
        Resuming, Running, Snapshotting, Starting, Stopped,
    };

    let legal = matches!(
        (from, to),
        (Creating, Created | Starting | Killing | Stopped | Failed)
            | (
                Created,
                Starting | Killing | RestartStopping | Stopped | Failed
            )
            | (
                Starting,
                Created | Creating | Running | Killing | Stopped | Failed
            )
            | (
                Running,
                Pausing | Snapshotting | Killing | RestartStopping | Stopped | Failed
            )
            | (Pausing, Paused | Running | Killing | Stopped | Failed)
            | (
                Paused,
                Resuming | Snapshotting | Killing | RestartStopping | Stopped | Failed
            )
            | (Resuming, Running | Paused | Killing | Stopped | Failed)
            | (Snapshotting, Running | Paused | Stopped | Failed)
            | (Killing, Stopped | Failed)
            | (Stopped | Failed, RestartStopping)
            | (RestartStopping, RestartStarting)
            | (RestartStarting, Running | Failed)
    );
    if !legal {
        return Err(ManagedExecutionStoreError::InvalidTransition {
            execution_id: execution_id.clone(),
            from,
            to,
        });
    }

    if matches!(
        (from, to),
        (Pausing, Paused) | (Resuming, Running) | (RestartStopping, RestartStarting)
    ) {
        let value = current.get().checked_add(1).ok_or_else(|| {
            ManagedExecutionStoreError::InvalidRecord(format!(
                "execution {execution_id} generation is exhausted"
            ))
        })?;
        return ExecutionGeneration::new(value)
            .map_err(|error| ManagedExecutionStoreError::InvalidRecord(error.to_string()));
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use a3s_box_core::{CreateExecutionRequest, ExecutionIsolation, ExecutionSnapshotId};

    use super::*;
    use crate::ManagedExecutionMetadata;

    fn managed_record(id: &str, operation: &str) -> BoxRecord {
        let mut record: BoxRecord = serde_json::from_value(serde_json::json!({
            "id": id,
            "short_id": BoxRecord::make_short_id(id),
            "name": format!("box-{id}"),
            "image": "alpine:latest",
            "isolation": "sandbox",
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
        .unwrap();
        let config = a3s_box_core::BoxConfig {
            image: "alpine:latest".to_string(),
            isolation: ExecutionIsolation::Sandbox,
            ..Default::default()
        };
        record.managed_execution = Some(
            ManagedExecutionMetadata::new(
                OperationId::new(operation).unwrap(),
                ExecutionGeneration::INITIAL,
                CreateExecutionRequest {
                    external_sandbox_id: "sandbox-1".to_string(),
                    config,
                    labels: Default::default(),
                    policy: Default::default(),
                    rootfs_snapshot_id: None,
                },
            )
            .unwrap(),
        );
        record
    }

    #[test]
    fn reservation_is_idempotent_for_the_same_full_request() {
        let directory = tempfile::tempdir().unwrap();
        let store = ManagedExecutionStore::new(directory.path().join("boxes.json"));

        let first = store.reserve(managed_record("execution-1", "operation-1"));
        let retry = store.reserve(managed_record("execution-2", "operation-1"));

        assert!(first.unwrap().is_new());
        let retry = retry.unwrap();
        assert!(!retry.is_new());
        assert_eq!(retry.record().id, "execution-1");
        assert_eq!(
            BoxStateStore::load(store.path()).unwrap().records().len(),
            1
        );
    }

    #[test]
    fn reservation_rejects_operation_reuse_with_different_intent() {
        let directory = tempfile::tempdir().unwrap();
        let store = ManagedExecutionStore::new(directory.path().join("boxes.json"));
        store
            .reserve(managed_record("execution-1", "operation-1"))
            .unwrap();
        let mut conflicting = managed_record("execution-2", "operation-1");
        conflicting
            .managed_execution
            .as_mut()
            .unwrap()
            .request
            .external_sandbox_id = "sandbox-2".to_string();

        let error = store.reserve(conflicting).unwrap_err();

        assert!(matches!(error, ManagedExecutionStoreError::Conflict { .. }));
        assert_eq!(
            BoxStateStore::load(store.path()).unwrap().records().len(),
            1
        );
    }

    #[test]
    fn removal_claim_is_generation_fenced_durable_and_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let store = ManagedExecutionStore::new(directory.path().join("boxes.json"));
        let id = ExecutionId::new("execution-1").unwrap();
        store
            .reserve(managed_record(id.as_str(), "operation-1"))
            .unwrap();

        let claimed = store
            .begin_remove(&id, ExecutionGeneration::INITIAL)
            .unwrap()
            .unwrap();
        assert_eq!(
            claimed.managed_state().unwrap(),
            Some(ManagedExecutionState::Removing)
        );
        assert!(matches!(
            claimed
                .managed_execution
                .as_ref()
                .unwrap()
                .pending_operation,
            Some(ManagedExecutionOperation::Remove)
        ));

        let reopened = ManagedExecutionStore::new(store.path().to_path_buf());
        assert_eq!(
            reopened
                .begin_remove(&id, ExecutionGeneration::INITIAL)
                .unwrap()
                .unwrap()
                .managed_state()
                .unwrap(),
            Some(ManagedExecutionState::Removing)
        );
        assert!(matches!(
            reopened.begin_remove(&id, ExecutionGeneration::new(2).unwrap()),
            Err(ManagedExecutionStoreError::Conflict { .. })
        ));

        assert!(reopened
            .finish_remove(&id, ExecutionGeneration::INITIAL)
            .unwrap());
        assert!(!reopened
            .finish_remove(&id, ExecutionGeneration::INITIAL)
            .unwrap());
        assert!(reopened.get(&id).unwrap().is_none());
    }

    #[test]
    fn pause_and_resume_completion_advance_generation_once() {
        let directory = tempfile::tempdir().unwrap();
        let store = ManagedExecutionStore::new(directory.path().join("boxes.json"));
        let id = ExecutionId::new("execution-1").unwrap();
        store
            .reserve(managed_record(id.as_str(), "operation-1"))
            .unwrap();

        store
            .transition(
                &id,
                ExecutionGeneration::INITIAL,
                ManagedExecutionState::Created,
                ManagedExecutionState::Starting,
            )
            .unwrap();
        let running = store
            .transition(
                &id,
                ExecutionGeneration::INITIAL,
                ManagedExecutionState::Starting,
                ManagedExecutionState::Running,
            )
            .unwrap();
        assert_eq!(
            running.managed_execution.unwrap().generation,
            ExecutionGeneration::INITIAL
        );
        store
            .transition(
                &id,
                ExecutionGeneration::INITIAL,
                ManagedExecutionState::Running,
                ManagedExecutionState::Pausing,
            )
            .unwrap();
        let paused = store
            .transition(
                &id,
                ExecutionGeneration::INITIAL,
                ManagedExecutionState::Pausing,
                ManagedExecutionState::Paused,
            )
            .unwrap();
        let generation_two = ExecutionGeneration::new(2).unwrap();
        assert_eq!(paused.managed_execution.unwrap().generation, generation_two);
        store
            .transition(
                &id,
                generation_two,
                ManagedExecutionState::Paused,
                ManagedExecutionState::Resuming,
            )
            .unwrap();
        let resumed = store
            .transition(
                &id,
                generation_two,
                ManagedExecutionState::Resuming,
                ManagedExecutionState::Running,
            )
            .unwrap();
        assert_eq!(
            resumed.managed_execution.unwrap().generation,
            ExecutionGeneration::new(3).unwrap()
        );
    }

    #[test]
    fn snapshot_intent_is_durable_and_preserves_runtime_generation() {
        let directory = tempfile::tempdir().unwrap();
        let store = ManagedExecutionStore::new(directory.path().join("boxes.json"));
        let id = ExecutionId::new("execution-1").unwrap();
        store
            .reserve(managed_record(id.as_str(), "operation-1"))
            .unwrap();
        store
            .transition(
                &id,
                ExecutionGeneration::INITIAL,
                ManagedExecutionState::Created,
                ManagedExecutionState::Starting,
            )
            .unwrap();
        store
            .transition(
                &id,
                ExecutionGeneration::INITIAL,
                ManagedExecutionState::Starting,
                ManagedExecutionState::Running,
            )
            .unwrap();
        let snapshot_id = ExecutionSnapshotId::new("snapshot-1").unwrap();
        let claimed = store
            .transition_with(
                &id,
                ExecutionGeneration::INITIAL,
                ManagedExecutionState::Running,
                ManagedExecutionState::Snapshotting,
                |record| {
                    record.managed_execution.as_mut().unwrap().pending_operation =
                        Some(ManagedExecutionOperation::Snapshot {
                            snapshot_id: snapshot_id.clone(),
                            source_state: ManagedExecutionState::Running,
                        });
                },
            )
            .unwrap();
        assert_eq!(
            claimed.managed_execution.as_ref().unwrap().generation,
            ExecutionGeneration::INITIAL
        );
        assert!(matches!(
            claimed
                .managed_execution
                .as_ref()
                .unwrap()
                .pending_operation
                .as_ref(),
            Some(ManagedExecutionOperation::Snapshot {
                snapshot_id,
                source_state: ManagedExecutionState::Running,
            }) if snapshot_id.as_str() == "snapshot-1"
        ));

        let reopened = ManagedExecutionStore::new(store.path().to_path_buf());
        let persisted = reopened.get(&id).unwrap().unwrap();
        assert_eq!(
            persisted.managed_state().unwrap(),
            Some(ManagedExecutionState::Snapshotting)
        );
        let completed = reopened
            .transition(
                &id,
                ExecutionGeneration::INITIAL,
                ManagedExecutionState::Snapshotting,
                ManagedExecutionState::Running,
            )
            .unwrap();
        let metadata = completed.managed_execution.unwrap();
        assert_eq!(metadata.generation, ExecutionGeneration::INITIAL);
        assert!(metadata.pending_operation.is_none());
        assert!(matches!(
            reopened.transition(
                &id,
                ExecutionGeneration::INITIAL,
                ManagedExecutionState::Running,
                ManagedExecutionState::Snapshotting,
            ),
            Err(ManagedExecutionStoreError::InvalidRecord(_))
        ));
    }

    #[test]
    fn restart_advances_generation_between_durable_teardown_and_startup() {
        let directory = tempfile::tempdir().unwrap();
        let store = ManagedExecutionStore::new(directory.path().join("boxes.json"));
        let id = ExecutionId::new("execution-1").unwrap();
        store
            .reserve(managed_record(id.as_str(), "operation-create"))
            .unwrap();
        store
            .transition(
                &id,
                ExecutionGeneration::INITIAL,
                ManagedExecutionState::Created,
                ManagedExecutionState::Starting,
            )
            .unwrap();
        store
            .transition(
                &id,
                ExecutionGeneration::INITIAL,
                ManagedExecutionState::Starting,
                ManagedExecutionState::Running,
            )
            .unwrap();
        let restart_operation = OperationId::new("operation-restart").unwrap();
        let stopping = store
            .transition_with(
                &id,
                ExecutionGeneration::INITIAL,
                ManagedExecutionState::Running,
                ManagedExecutionState::RestartStopping,
                |record| {
                    record.managed_execution.as_mut().unwrap().pending_operation =
                        Some(ManagedExecutionOperation::Restart {
                            operation_id: restart_operation.clone(),
                            source_generation: ExecutionGeneration::INITIAL,
                            source_state: ManagedExecutionState::Running,
                            stop_timeout_secs: Some(10),
                        });
                },
            )
            .unwrap();
        assert_eq!(
            stopping.managed_execution.as_ref().unwrap().generation,
            ExecutionGeneration::INITIAL
        );

        let starting = store
            .transition(
                &id,
                ExecutionGeneration::INITIAL,
                ManagedExecutionState::RestartStopping,
                ManagedExecutionState::RestartStarting,
            )
            .unwrap();
        let generation_two = ExecutionGeneration::new(2).unwrap();
        assert_eq!(
            starting.managed_execution.as_ref().unwrap().generation,
            generation_two
        );
        let running = store
            .transition(
                &id,
                generation_two,
                ManagedExecutionState::RestartStarting,
                ManagedExecutionState::Running,
            )
            .unwrap();
        let metadata = running.managed_execution.unwrap();
        assert_eq!(metadata.generation, generation_two);
        assert!(metadata.pending_operation.is_none());
        let completed = metadata.last_restart.unwrap();
        assert_eq!(completed.operation_id, restart_operation);
        assert_eq!(completed.source_generation, ExecutionGeneration::INITIAL);
        assert_eq!(completed.target_generation, generation_two);
        assert_eq!(completed.outcome, ManagedRestartOutcome::Running);
        assert_eq!(completed.stop_timeout_secs, Some(10));
    }

    #[test]
    fn stale_generation_and_invalid_edges_do_not_change_disk() {
        let directory = tempfile::tempdir().unwrap();
        let store = ManagedExecutionStore::new(directory.path().join("boxes.json"));
        let id = ExecutionId::new("execution-1").unwrap();
        store
            .reserve(managed_record(id.as_str(), "operation-1"))
            .unwrap();
        store
            .transition(
                &id,
                ExecutionGeneration::INITIAL,
                ManagedExecutionState::Created,
                ManagedExecutionState::Starting,
            )
            .unwrap();
        store
            .transition(
                &id,
                ExecutionGeneration::INITIAL,
                ManagedExecutionState::Starting,
                ManagedExecutionState::Running,
            )
            .unwrap();

        let stale = store.transition(
            &id,
            ExecutionGeneration::new(2).unwrap(),
            ManagedExecutionState::Running,
            ManagedExecutionState::Pausing,
        );
        let invalid = store.transition(
            &id,
            ExecutionGeneration::INITIAL,
            ManagedExecutionState::Running,
            ManagedExecutionState::Paused,
        );

        assert!(matches!(
            stale,
            Err(ManagedExecutionStoreError::Conflict { .. })
        ));
        assert!(matches!(
            invalid,
            Err(ManagedExecutionStoreError::InvalidTransition { .. })
        ));
        let persisted = store.get(&id).unwrap().unwrap();
        assert_eq!(
            persisted.managed_state().unwrap(),
            Some(ManagedExecutionState::Running)
        );
        assert_eq!(
            persisted.managed_execution.unwrap().generation,
            ExecutionGeneration::INITIAL
        );
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_claims_have_one_winner() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(ManagedExecutionStore::new(
            directory.path().join("boxes.json"),
        ));
        let id = ExecutionId::new("execution-1").unwrap();
        store
            .reserve(managed_record(id.as_str(), "operation-1"))
            .unwrap();
        store
            .transition(
                &id,
                ExecutionGeneration::INITIAL,
                ManagedExecutionState::Created,
                ManagedExecutionState::Starting,
            )
            .unwrap();
        store
            .transition(
                &id,
                ExecutionGeneration::INITIAL,
                ManagedExecutionState::Starting,
                ManagedExecutionState::Running,
            )
            .unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let store = Arc::clone(&store);
                let id = id.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    store.transition(
                        &id,
                        ExecutionGeneration::INITIAL,
                        ManagedExecutionState::Running,
                        ManagedExecutionState::Pausing,
                    )
                })
            })
            .collect();
        barrier.wait();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(ManagedExecutionStoreError::Conflict { .. })))
                .count(),
            1
        );
        assert_eq!(
            store.get(&id).unwrap().unwrap().managed_state().unwrap(),
            Some(ManagedExecutionState::Pausing)
        );
    }
}
