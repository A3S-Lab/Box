use a3s_box_core::{
    ExecutionGeneration, ExecutionId, ExecutionManagerError, ExecutionManagerResult,
    ExecutionSnapshotId, OperationId, RestartExecutionOptions,
};

use super::record::{
    apply_handle, apply_restart_handle, apply_start_handle, clear_live_runtime, execution_id,
};
use super::support::{generation, managed_state};
use super::{LocalExecutionHandle, LocalExecutionManager};
use crate::{
    BoxRecord, ManagedExecutionOperation, ManagedExecutionReservation, ManagedExecutionState,
    ManagedExecutionStoreError, SnapshotStore,
};

impl LocalExecutionManager {
    pub(super) async fn reserve(
        &self,
        record: BoxRecord,
    ) -> ExecutionManagerResult<ManagedExecutionReservation> {
        let store = self.store.clone();
        let Some(snapshot_id) = record
            .managed_execution
            .as_ref()
            .and_then(|metadata| metadata.request.rootfs_snapshot_id.clone())
        else {
            return run_store(move || store.reserve(record)).await;
        };
        let home_dir = self.home_dir.clone();
        tokio::task::spawn_blocking(move || {
            let snapshots = SnapshotStore::new(&home_dir.join("snapshots")).map_err(|error| {
                ExecutionManagerError::Unavailable(format!(
                    "failed to open filesystem snapshot store: {error}"
                ))
            })?;
            let _snapshot_lock = snapshots.acquire_exclusive_lock().map_err(|error| {
                ExecutionManagerError::Unavailable(format!(
                    "failed to lock filesystem snapshot store: {error}"
                ))
            })?;
            let metadata = snapshots
                .get(snapshot_id.as_str())
                .map_err(|error| {
                    ExecutionManagerError::Unavailable(format!(
                        "failed to inspect filesystem snapshot {snapshot_id}: {error}"
                    ))
                })?
                .ok_or_else(|| {
                    ExecutionManagerError::Unavailable(format!(
                        "filesystem snapshot {snapshot_id} is unavailable"
                    ))
                })?;
            if metadata.id != snapshot_id.as_str()
                || !snapshots.rootfs_path(snapshot_id.as_str()).is_dir()
            {
                return Err(ExecutionManagerError::Unavailable(format!(
                    "filesystem snapshot {snapshot_id} is not a valid published snapshot"
                )));
            }
            metadata.require_image_config().map_err(|error| {
                ExecutionManagerError::Unavailable(format!(
                    "filesystem snapshot {snapshot_id} cannot be restored: {error}"
                ))
            })?;
            store.reserve(record).map_err(map_store_error)
        })
        .await
        .map_err(|error| {
            ExecutionManagerError::Internal(format!("managed state task failed: {error}"))
        })?
    }

    pub(super) async fn get(
        &self,
        execution_id: &ExecutionId,
    ) -> ExecutionManagerResult<Option<BoxRecord>> {
        let store = self.store.clone();
        let execution_id = execution_id.clone();
        run_store(move || store.get(&execution_id)).await
    }

    pub(super) async fn get_by_operation(
        &self,
        operation_id: &OperationId,
    ) -> ExecutionManagerResult<Option<BoxRecord>> {
        let store = self.store.clone();
        let operation_id = operation_id.clone();
        run_store(move || store.get_by_operation_id(&operation_id)).await
    }

    pub(super) async fn transition(
        &self,
        record: &BoxRecord,
        from: ManagedExecutionState,
        to: ManagedExecutionState,
        update: RuntimeUpdate,
    ) -> ExecutionManagerResult<BoxRecord> {
        let store = self.store.clone();
        let execution_id = execution_id(record)?;
        let generation = generation(record, &execution_id)?;
        run_store(move || {
            store.transition_with(&execution_id, generation, from, to, |record| match update {
                RuntimeUpdate::None => {}
                RuntimeUpdate::Handle(handle) => apply_handle(record, &handle),
                RuntimeUpdate::StartHandle(handle) => apply_start_handle(record, &handle),
                RuntimeUpdate::Terminal(exit_code) => clear_live_runtime(record, exit_code),
                RuntimeUpdate::PauseClaim(keep_memory) => {
                    if let Some(metadata) = record.managed_execution.as_mut() {
                        metadata.pending_operation =
                            Some(ManagedExecutionOperation::Pause { keep_memory });
                    }
                }
                RuntimeUpdate::SnapshotClaim {
                    snapshot_id,
                    source_state,
                } => {
                    if let Some(metadata) = record.managed_execution.as_mut() {
                        metadata.pending_operation = Some(ManagedExecutionOperation::Snapshot {
                            snapshot_id,
                            source_state,
                        });
                    }
                }
                RuntimeUpdate::RestartClaim {
                    operation_id,
                    options,
                } => {
                    if let Some(metadata) = record.managed_execution.as_mut() {
                        metadata.pending_operation = Some(ManagedExecutionOperation::Restart {
                            operation_id,
                            source_generation: metadata.generation,
                            source_state: from,
                            stop_timeout_secs: options.stop_timeout_secs,
                        });
                    }
                }
                RuntimeUpdate::RestartAdvance => clear_live_runtime(record, None),
                RuntimeUpdate::RestartHandle(handle) => apply_restart_handle(record, &handle),
                RuntimeUpdate::RestartFailed(exit_code) => clear_live_runtime(record, exit_code),
            })
        })
        .await
    }

    pub(super) async fn complete_with_handle(
        &self,
        record: &BoxRecord,
        from: ManagedExecutionState,
        to: ManagedExecutionState,
        handle: LocalExecutionHandle,
    ) -> ExecutionManagerResult<BoxRecord> {
        let execution_id = execution_id(record)?;
        let current_generation = generation(record, &execution_id)?;
        let expected_generation = if matches!(
            (from, to),
            (
                ManagedExecutionState::Pausing,
                ManagedExecutionState::Paused
            ) | (
                ManagedExecutionState::Resuming,
                ManagedExecutionState::Running
            )
        ) {
            ExecutionGeneration::new(current_generation.get().checked_add(1).ok_or_else(|| {
                ExecutionManagerError::Internal(format!(
                    "execution {execution_id} generation is exhausted"
                ))
            })?)?
        } else {
            current_generation
        };
        let update =
            if from == ManagedExecutionState::Starting && to == ManagedExecutionState::Running {
                RuntimeUpdate::StartHandle(handle)
            } else {
                RuntimeUpdate::Handle(handle)
            };
        match self.transition(record, from, to, update).await {
            Ok(record) => Ok(record),
            Err(error @ ExecutionManagerError::Conflict { .. }) => {
                let Some(current) = self.get(&execution_id).await? else {
                    return Err(ExecutionManagerError::NotFound(execution_id));
                };
                if managed_state(&current)? == to
                    && generation(&current, &execution_id)? == expected_generation
                {
                    Ok(current)
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(error),
        }
    }
}

pub(super) enum RuntimeUpdate {
    None,
    Handle(LocalExecutionHandle),
    StartHandle(LocalExecutionHandle),
    Terminal(Option<i32>),
    PauseClaim(bool),
    SnapshotClaim {
        snapshot_id: ExecutionSnapshotId,
        source_state: ManagedExecutionState,
    },
    RestartClaim {
        operation_id: OperationId,
        options: RestartExecutionOptions,
    },
    RestartAdvance,
    RestartHandle(LocalExecutionHandle),
    RestartFailed(Option<i32>),
}

pub(super) async fn run_store<T>(
    operation: impl FnOnce() -> Result<T, ManagedExecutionStoreError> + Send + 'static,
) -> ExecutionManagerResult<T>
where
    T: Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| {
            ExecutionManagerError::Internal(format!("managed state task failed: {error}"))
        })?
        .map_err(map_store_error)
}

pub(super) fn map_store_error(error: ManagedExecutionStoreError) -> ExecutionManagerError {
    match error {
        ManagedExecutionStoreError::Io(error) => {
            ExecutionManagerError::Unavailable(error.to_string())
        }
        ManagedExecutionStoreError::NotFound(execution_id) => {
            ExecutionManagerError::NotFound(execution_id)
        }
        ManagedExecutionStoreError::Conflict {
            execution_id,
            message,
        } => ExecutionManagerError::Conflict {
            execution_id,
            message,
        },
        ManagedExecutionStoreError::Unmanaged(execution_id) => ExecutionManagerError::Internal(
            format!("execution record is not managed: {execution_id}"),
        ),
        ManagedExecutionStoreError::InvalidRecord(message) => {
            ExecutionManagerError::Internal(message)
        }
        ManagedExecutionStoreError::InvalidTransition {
            execution_id,
            from,
            to,
        } => ExecutionManagerError::Internal(format!(
            "invalid managed transition for {execution_id}: {from} -> {to}"
        )),
    }
}
