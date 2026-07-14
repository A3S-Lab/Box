use a3s_box_core::{
    ExecutionGeneration, ExecutionId, ExecutionManagerError, ExecutionManagerResult,
    ExecutionState, ReconcileOutcome,
};

use super::record::{lease_from_record, reservation_from_record};
use super::{
    BoxRecord, LocalExecutionHandle, LocalExecutionObservation, ManagedExecutionOperation,
    ManagedExecutionState,
};

pub(super) fn managed_state(record: &BoxRecord) -> ExecutionManagerResult<ManagedExecutionState> {
    record
        .managed_state()
        .map_err(|error| ExecutionManagerError::Internal(error.to_string()))?
        .ok_or_else(|| {
            ExecutionManagerError::Internal(format!("execution {} is not managed", record.id))
        })
}

pub(super) fn generation(
    record: &BoxRecord,
    execution_id: &ExecutionId,
) -> ExecutionManagerResult<ExecutionGeneration> {
    record
        .managed_execution
        .as_ref()
        .map(|metadata| metadata.generation)
        .ok_or_else(|| {
            ExecutionManagerError::Internal(format!(
                "execution {execution_id} has no managed generation"
            ))
        })
}

pub(super) fn require_generation(
    record: &BoxRecord,
    execution_id: &ExecutionId,
    expected: ExecutionGeneration,
) -> ExecutionManagerResult<()> {
    let actual = generation(record, execution_id)?;
    if actual == expected {
        Ok(())
    } else {
        Err(ExecutionManagerError::Conflict {
            execution_id: execution_id.clone(),
            message: format!(
                "expected generation {}, found {}",
                expected.get(),
                actual.get()
            ),
        })
    }
}

pub(super) fn state_conflict(
    record: &BoxRecord,
    execution_id: &ExecutionId,
    operation: &str,
) -> ExecutionManagerError {
    let state = managed_state(record)
        .map(|state| state.to_string())
        .unwrap_or_else(|error| error.to_string());
    ExecutionManagerError::Conflict {
        execution_id: execution_id.clone(),
        message: format!("cannot {operation} execution in state {state}"),
    }
}

pub(super) fn required_handle(
    observation: &LocalExecutionObservation,
    execution_id: &ExecutionId,
) -> ExecutionManagerResult<LocalExecutionHandle> {
    observation.handle.clone().ok_or_else(|| {
        ExecutionManagerError::Internal(format!(
            "backend returned no runtime evidence for {execution_id}"
        ))
    })
}

pub(super) fn pending_pause_policy(
    record: &BoxRecord,
    execution_id: &ExecutionId,
) -> ExecutionManagerResult<bool> {
    match record
        .managed_execution
        .as_ref()
        .and_then(|metadata| metadata.pending_operation.as_ref())
    {
        Some(ManagedExecutionOperation::Pause { keep_memory }) => Ok(*keep_memory),
        _ => Err(ExecutionManagerError::Internal(format!(
            "pausing execution {execution_id} has no persisted pause policy"
        ))),
    }
}

pub(super) fn outcome_from_record(
    record: BoxRecord,
    state: ExecutionState,
) -> ExecutionManagerResult<ReconcileOutcome> {
    match state {
        ExecutionState::Created => Ok(ReconcileOutcome::Created(reservation_from_record(&record)?)),
        ExecutionState::Creating => Ok(ReconcileOutcome::Creating),
        ExecutionState::Running | ExecutionState::Paused => {
            Ok(ReconcileOutcome::Ready(lease_from_record(&record)?))
        }
        ExecutionState::Stopped | ExecutionState::Failed => Ok(ReconcileOutcome::Failed),
    }
}
