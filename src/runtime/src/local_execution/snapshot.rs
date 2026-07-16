//! Crash-recoverable filesystem snapshots for managed executions.

use std::path::{Path, PathBuf};

use a3s_box_core::snapshot::SnapshotMetadata;
use a3s_box_core::{
    ExecutionBackend, ExecutionGeneration, ExecutionId, ExecutionLease, ExecutionManagerError,
    ExecutionManagerResult, ExecutionSnapshot, ExecutionSnapshotId, ExecutionState,
};

use super::record::lease_from_record;
use super::support::{managed_state, require_generation, required_handle, state_conflict};
use super::{LocalExecutionHandle, LocalExecutionManager, ManagedExecutionState, RuntimeUpdate};
use crate::{BoxRecord, BoxStateStore, ManagedExecutionOperation, SnapshotStore};

impl LocalExecutionManager {
    pub(super) async fn create_snapshot(
        &self,
        execution_id: &ExecutionId,
        expected_generation: ExecutionGeneration,
        snapshot_id: &ExecutionSnapshotId,
    ) -> ExecutionManagerResult<ExecutionSnapshot> {
        let record = self
            .get(execution_id)
            .await?
            .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
        require_generation(&record, execution_id, expected_generation)?;
        let source_state = managed_state(&record)?;
        if source_state == ManagedExecutionState::Snapshotting {
            let (pending_snapshot_id, _) = snapshot_operation(&record)?;
            if &pending_snapshot_id != snapshot_id {
                return Err(state_conflict(&record, execution_id, "snapshot"));
            }
            return self.drive_snapshot(record).await;
        }
        if !matches!(
            source_state,
            ManagedExecutionState::Running | ManagedExecutionState::Paused
        ) {
            return Err(state_conflict(&record, execution_id, "snapshot"));
        }
        let metadata = record.managed_execution.as_ref().ok_or_else(|| {
            ExecutionManagerError::Internal(format!(
                "execution {execution_id} lost managed lifecycle metadata"
            ))
        })?;
        if metadata.plan.backend != ExecutionBackend::Crun {
            return Err(ExecutionManagerError::Conflict {
                execution_id: execution_id.clone(),
                message: "filesystem snapshots currently require the Sandbox backend"
                    .to_string(),
            });
        }
        if let Some(existing) = self.load_snapshot(snapshot_id).await? {
            if existing.source_box_id != record.id {
                return Err(ExecutionManagerError::Conflict {
                    execution_id: execution_id.clone(),
                    message: format!(
                        "filesystem snapshot {snapshot_id} belongs to another execution"
                    ),
                });
            }
            return Ok(ExecutionSnapshot {
                snapshot_id: snapshot_id.clone(),
                size_bytes: existing.size_bytes,
                state: execution_state(source_state)?,
                lease: lease_from_record(&record)?,
            });
        }

        let claimed = self
            .transition(
                &record,
                source_state,
                ManagedExecutionState::Snapshotting,
                RuntimeUpdate::SnapshotClaim {
                    snapshot_id: snapshot_id.clone(),
                    source_state,
                },
            )
            .await?;
        self.drive_snapshot(claimed).await
    }

    pub(super) async fn recover_snapshot(
        &self,
        record: BoxRecord,
    ) -> ExecutionManagerResult<ExecutionLease> {
        self.drive_snapshot(record).await.map(|snapshot| snapshot.lease)
    }

    pub(super) async fn stabilize_snapshot(
        &self,
        record: BoxRecord,
    ) -> ExecutionManagerResult<BoxRecord> {
        if managed_state(&record)? != ManagedExecutionState::Snapshotting {
            return Ok(record);
        }
        let execution_id = ExecutionId::new(record.id.clone())?;
        self.drive_snapshot(record).await?;
        self.get(&execution_id)
            .await?
            .ok_or(ExecutionManagerError::NotFound(execution_id))
    }

    async fn drive_snapshot(
        &self,
        record: BoxRecord,
    ) -> ExecutionManagerResult<ExecutionSnapshot> {
        let (snapshot_id, source_state) = snapshot_operation(&record)?;
        let execution_id = ExecutionId::new(record.id.clone())?;
        let observation = self.backend.inspect(&record).await?;
        observation.validate(&execution_id)?;
        let mut handle = required_handle(&observation, &execution_id)?;

        match (source_state, observation.state) {
            (ManagedExecutionState::Running, ExecutionState::Running) => {
                handle = self.pause_for_snapshot(&record, &execution_id).await?;
            }
            (ManagedExecutionState::Running, ExecutionState::Paused)
            | (ManagedExecutionState::Paused, ExecutionState::Paused) => {}
            (ManagedExecutionState::Paused, ExecutionState::Running) => {
                return Err(ExecutionManagerError::Conflict {
                    execution_id,
                    message: "a paused execution resumed while its filesystem snapshot was pending"
                        .to_string(),
                })
            }
            (_, state) => {
                return Err(ExecutionManagerError::Conflict {
                    execution_id,
                    message: format!(
                        "execution entered {state:?} while its filesystem snapshot was pending"
                    ),
                })
            }
        }

        let capture = self
            .capture_snapshot_rootfs(record.clone(), snapshot_id.clone())
            .await;
        let size_bytes = match capture {
            Ok(size_bytes) => size_bytes,
            Err(capture_error) => {
                let restored = self
                    .restore_snapshot_source_state(&record, source_state, handle)
                    .await;
                return match restored {
                    Ok(_) => Err(capture_error),
                    Err(restore_error) => Err(ExecutionManagerError::Internal(format!(
                        "{capture_error}; failed to restore execution {} after snapshot failure: {restore_error}",
                        record.id
                    ))),
                };
            }
        };

        let completed = self
            .restore_snapshot_source_state(&record, source_state, handle)
            .await?;
        Ok(ExecutionSnapshot {
            snapshot_id,
            size_bytes,
            state: execution_state(source_state)?,
            lease: lease_from_record(&completed)?,
        })
    }

    async fn pause_for_snapshot(
        &self,
        record: &BoxRecord,
        execution_id: &ExecutionId,
    ) -> ExecutionManagerResult<LocalExecutionHandle> {
        match self.backend.pause(record, true).await {
            Ok(handle) => {
                handle.validate(execution_id)?;
                Ok(handle)
            }
            Err(pause_error) => match self.backend.inspect(record).await {
                Ok(observation) if observation.state == ExecutionState::Paused => {
                    observation.validate(execution_id)?;
                    required_handle(&observation, execution_id)
                }
                _ => Err(pause_error),
            },
        }
    }

    async fn restore_snapshot_source_state(
        &self,
        record: &BoxRecord,
        source_state: ManagedExecutionState,
        mut handle: LocalExecutionHandle,
    ) -> ExecutionManagerResult<BoxRecord> {
        if source_state == ManagedExecutionState::Running {
            let execution_id = ExecutionId::new(record.id.clone())?;
            handle = match self.backend.resume(record).await {
                Ok(handle) => {
                    handle.validate(&execution_id)?;
                    handle
                }
                Err(resume_error) => match self.backend.inspect(record).await {
                    Ok(observation) if observation.state == ExecutionState::Running => {
                        observation.validate(&execution_id)?;
                        required_handle(&observation, &execution_id)?
                    }
                    _ => return Err(resume_error),
                },
            };
        }
        self.complete_with_handle(
            record,
            ManagedExecutionState::Snapshotting,
            source_state,
            handle,
        )
        .await
    }

    async fn capture_snapshot_rootfs(
        &self,
        record: BoxRecord,
        snapshot_id: ExecutionSnapshotId,
    ) -> ExecutionManagerResult<u64> {
        let home_dir = self.home_dir.clone();
        let execution_id = record.id.clone();
        tokio::task::spawn_blocking(move || {
            let store = SnapshotStore::new(&home_dir.join("snapshots"))
                .map_err(|error| snapshot_error(&record, "open snapshot store", error))?;
            if let Some(existing) = store
                .get(snapshot_id.as_str())
                .map_err(|error| snapshot_error(&record, "load snapshot", error))?
            {
                if existing.id != snapshot_id.as_str()
                    || existing.source_box_id != record.id
                    || !store.rootfs_path(snapshot_id.as_str()).is_dir()
                {
                    return Err(ExecutionManagerError::Unavailable(format!(
                        "filesystem snapshot {snapshot_id} has inconsistent persisted metadata"
                    )));
                }
                return Ok(existing.size_bytes);
            }
            let rootfs = resolve_managed_rootfs(&record.box_dir).ok_or_else(|| {
                ExecutionManagerError::Unavailable(format!(
                    "execution {} has no populated managed rootfs to snapshot",
                    record.id
                ))
            })?;
            let metadata = build_snapshot_metadata(&record, &snapshot_id);
            match store.save(metadata, &rootfs) {
                Ok(saved) => Ok(saved.size_bytes),
                Err(save_error) => match store.get(snapshot_id.as_str()) {
                    Ok(Some(existing))
                        if existing.id == snapshot_id.as_str()
                            && existing.source_box_id == record.id
                            && store.rootfs_path(snapshot_id.as_str()).is_dir() =>
                    {
                        Ok(existing.size_bytes)
                    }
                    _ => Err(snapshot_error(&record, "capture rootfs", save_error)),
                },
            }
        })
        .await
        .map_err(|error| {
            ExecutionManagerError::Internal(format!(
                "filesystem snapshot task failed for {}: {error}",
                execution_id
            ))
        })?
    }

    pub(super) async fn snapshot_size(
        &self,
        snapshot_id: &ExecutionSnapshotId,
    ) -> ExecutionManagerResult<Option<u64>> {
        Ok(self
            .load_snapshot(snapshot_id)
            .await?
            .map(|snapshot| snapshot.size_bytes))
    }

    async fn load_snapshot(
        &self,
        snapshot_id: &ExecutionSnapshotId,
    ) -> ExecutionManagerResult<Option<SnapshotMetadata>> {
        let home_dir = self.home_dir.clone();
        let snapshot_id = snapshot_id.clone();
        tokio::task::spawn_blocking(move || {
            let store = SnapshotStore::new(&home_dir.join("snapshots")).map_err(|error| {
                ExecutionManagerError::Unavailable(format!(
                    "failed to open filesystem snapshot store: {error}"
                ))
            })?;
            let Some(metadata) = store.get(snapshot_id.as_str()).map_err(|error| {
                ExecutionManagerError::Unavailable(format!(
                    "failed to inspect filesystem snapshot {snapshot_id}: {error}"
                ))
            })? else {
                return Ok(None);
            };
            if metadata.id != snapshot_id.as_str()
                || !store.rootfs_path(snapshot_id.as_str()).is_dir()
            {
                return Err(ExecutionManagerError::Unavailable(format!(
                    "filesystem snapshot {snapshot_id} is not a valid published snapshot"
                )));
            }
            Ok(Some(metadata))
        })
        .await
        .map_err(|error| {
            ExecutionManagerError::Internal(format!("filesystem snapshot task failed: {error}"))
        })?
    }

    pub(super) async fn delete_snapshot(
        &self,
        snapshot_id: &ExecutionSnapshotId,
    ) -> ExecutionManagerResult<bool> {
        let home_dir = self.home_dir.clone();
        let state_path = self.store.path().to_path_buf();
        let snapshot_id = snapshot_id.clone();
        tokio::task::spawn_blocking(move || {
            let store = SnapshotStore::new(&home_dir.join("snapshots")).map_err(|error| {
                ExecutionManagerError::Unavailable(format!(
                    "failed to open filesystem snapshot store: {error}"
                ))
            })?;
            let _snapshot_lock = store.acquire_exclusive_lock().map_err(|error| {
                ExecutionManagerError::Unavailable(format!(
                    "failed to lock filesystem snapshot store: {error}"
                ))
            })?;
            let state = BoxStateStore::load_readonly(state_path).map_err(|error| {
                ExecutionManagerError::Unavailable(format!(
                    "failed to inspect snapshot users: {error}"
                ))
            })?;
            let rootfs = store.rootfs_path(snapshot_id.as_str());
            let in_use = state.records().iter().any(|record| {
                let requested = record
                    .managed_execution
                    .as_ref()
                    .and_then(|metadata| metadata.request.rootfs_snapshot_id.as_ref())
                    == Some(&snapshot_id);
                let request_is_live = requested
                    && !record
                        .managed_state()
                        .is_ok_and(|state| state.is_some_and(ManagedExecutionState::is_terminal));
                let marker_uses_snapshot =
                    std::fs::read_to_string(record.box_dir.join(".snapshot-lower"))
                        .is_ok_and(|value| Path::new(value.trim()) == rootfs.as_path());
                request_is_live || marker_uses_snapshot
            });
            if in_use {
                return Err(ExecutionManagerError::Conflict {
                    execution_id: ExecutionId::new(format!("snapshot-{snapshot_id}"))?,
                    message: "filesystem snapshot is in use by an active execution".to_string(),
                });
            }
            store
                .delete_locked(snapshot_id.as_str())
                .map_err(|error| {
                    ExecutionManagerError::Unavailable(format!(
                        "failed to delete filesystem snapshot {snapshot_id}: {error}"
                    ))
                })
        })
        .await
        .map_err(|error| {
            ExecutionManagerError::Internal(format!("filesystem snapshot task failed: {error}"))
        })?
    }
}

fn snapshot_operation(
    record: &BoxRecord,
) -> ExecutionManagerResult<(ExecutionSnapshotId, ManagedExecutionState)> {
    match record
        .managed_execution
        .as_ref()
        .and_then(|metadata| metadata.pending_operation.as_ref())
    {
        Some(ManagedExecutionOperation::Snapshot {
            snapshot_id,
            source_state,
        }) if matches!(
            source_state,
            ManagedExecutionState::Running | ManagedExecutionState::Paused
        ) => Ok((snapshot_id.clone(), *source_state)),
        _ => Err(ExecutionManagerError::Internal(format!(
            "execution {} has invalid snapshot recovery metadata",
            record.id
        ))),
    }
}

fn execution_state(state: ManagedExecutionState) -> ExecutionManagerResult<ExecutionState> {
    match state {
        ManagedExecutionState::Running => Ok(ExecutionState::Running),
        ManagedExecutionState::Paused => Ok(ExecutionState::Paused),
        _ => Err(ExecutionManagerError::Internal(format!(
            "invalid stable snapshot state {state}"
        ))),
    }
}

fn resolve_managed_rootfs(box_dir: &Path) -> Option<PathBuf> {
    let populated = |path: &Path| {
        path.is_dir()
            && std::fs::read_dir(path)
                .map(|mut entries| entries.next().is_some())
                .unwrap_or(false)
    };
    let merged = box_dir.join("merged");
    if populated(&merged) {
        return Some(merged);
    }
    let rootfs = box_dir.join("rootfs");
    let apfs_data = rootfs.join(".a3s-rootfs");
    if populated(&apfs_data) {
        return Some(apfs_data);
    }
    populated(&rootfs).then_some(rootfs)
}

fn build_snapshot_metadata(
    record: &BoxRecord,
    snapshot_id: &ExecutionSnapshotId,
) -> SnapshotMetadata {
    let mut metadata = SnapshotMetadata::new(
        snapshot_id.to_string(),
        snapshot_id.to_string(),
        record.id.clone(),
        record.image.clone(),
    );
    metadata.vcpus = record.cpus;
    metadata.memory_mb = record.memory_mb;
    metadata.env = record.env.clone();
    metadata.cmd = record.cmd.clone();
    metadata.entrypoint = record.entrypoint.clone();
    metadata.workdir = record.workdir.clone();
    metadata.labels = record.labels.clone();
    metadata
}

fn snapshot_error(
    record: &BoxRecord,
    operation: &str,
    error: impl std::fmt::Display,
) -> ExecutionManagerError {
    ExecutionManagerError::Unavailable(format!(
        "failed to {operation} for execution {}: {error}",
        record.id
    ))
}
