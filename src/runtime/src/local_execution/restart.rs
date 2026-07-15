use a3s_box_core::{
    ExecutionGeneration, ExecutionLease, ExecutionManagerError, ExecutionManagerResult,
    ExecutionState, OperationId, RestartExecutionOptions,
};

use super::record::{execution_id, lease_from_record};
use super::store::RuntimeUpdate;
use super::support::{generation, managed_state, require_generation, required_handle};
use super::{
    BoxRecord, LocalExecutionHandle, LocalExecutionManager, ManagedExecutionOperation,
    ManagedExecutionState,
};
use crate::ManagedRestartOutcome;

#[derive(Clone)]
struct RestartIntent {
    operation_id: OperationId,
    source_generation: ExecutionGeneration,
    source_state: ManagedExecutionState,
    stop_timeout_secs: Option<u64>,
}

impl RestartIntent {
    const fn options(&self) -> RestartExecutionOptions {
        RestartExecutionOptions {
            stop_timeout_secs: self.stop_timeout_secs,
        }
    }
}

impl LocalExecutionManager {
    pub(super) async fn restart_record(
        &self,
        record: BoxRecord,
        expected_generation: ExecutionGeneration,
        operation_id: &OperationId,
        options: RestartExecutionOptions,
    ) -> ExecutionManagerResult<ExecutionLease> {
        if let Some(result) =
            completed_restart_result(&record, expected_generation, operation_id, options)?
        {
            return result;
        }
        if record
            .managed_execution
            .as_ref()
            .is_some_and(|metadata| metadata.operation_id == *operation_id)
        {
            return Err(ExecutionManagerError::Conflict {
                execution_id: execution_id(&record)?,
                message: format!(
                    "operation {operation_id} is already the execution creation identity"
                ),
            });
        }

        match managed_state(&record)? {
            ManagedExecutionState::RestartStopping | ManagedExecutionState::RestartStarting => {
                require_matching_restart(&record, expected_generation, operation_id, options)?;
                self.continue_restart(record).await
            }
            state @ (ManagedExecutionState::Created
            | ManagedExecutionState::Running
            | ManagedExecutionState::Paused
            | ManagedExecutionState::Stopped
            | ManagedExecutionState::Failed) => {
                let id = execution_id(&record)?;
                require_generation(&record, &id, expected_generation)?;
                ensure_restart_generation_available(&id, expected_generation)?;
                ensure_restart_timeout_valid(
                    &id,
                    options.stop_timeout_secs.or(record.stop_timeout),
                )?;
                let claimed = match self
                    .transition(
                        &record,
                        state,
                        ManagedExecutionState::RestartStopping,
                        RuntimeUpdate::RestartClaim {
                            operation_id: operation_id.clone(),
                            options,
                        },
                    )
                    .await
                {
                    Ok(claimed) => claimed,
                    Err(ExecutionManagerError::Conflict { .. }) => {
                        let current = self
                            .get(&id)
                            .await?
                            .ok_or_else(|| ExecutionManagerError::NotFound(id.clone()))?;
                        if let Some(result) = completed_restart_result(
                            &current,
                            expected_generation,
                            operation_id,
                            options,
                        )? {
                            return result;
                        }
                        require_matching_restart(
                            &current,
                            expected_generation,
                            operation_id,
                            options,
                        )?;
                        return self.continue_restart(current).await;
                    }
                    Err(error) => return Err(error),
                };
                self.continue_restart(claimed).await
            }
            state => Err(ExecutionManagerError::Conflict {
                execution_id: execution_id(&record)?,
                message: format!("cannot restart execution in state {state}"),
            }),
        }
    }

    pub(super) async fn resume_restart(
        &self,
        record: BoxRecord,
    ) -> ExecutionManagerResult<ExecutionLease> {
        restart_intent(&record)?;
        self.continue_restart(record).await
    }

    async fn continue_restart(&self, record: BoxRecord) -> ExecutionManagerResult<ExecutionLease> {
        match managed_state(&record)? {
            ManagedExecutionState::RestartStopping => self.finish_restart_stop(record).await,
            ManagedExecutionState::RestartStarting => self.finish_restart_start(record).await,
            state => Err(ExecutionManagerError::Internal(format!(
                "restart continuation reached state {state} for {}",
                record.id
            ))),
        }
    }

    async fn finish_restart_stop(
        &self,
        record: BoxRecord,
    ) -> ExecutionManagerResult<ExecutionLease> {
        let intent = restart_intent(&record)?;
        if matches!(
            intent.source_state,
            ManagedExecutionState::Running | ManagedExecutionState::Paused
        ) {
            self.confirm_restart_kill(&record, intent.stop_timeout_secs)
                .await?;
        }
        self.release_execution_resources(&record).await?;

        let id = execution_id(&record)?;
        let starting = match self
            .transition(
                &record,
                ManagedExecutionState::RestartStopping,
                ManagedExecutionState::RestartStarting,
                RuntimeUpdate::RestartAdvance,
            )
            .await
        {
            Ok(starting) => starting,
            Err(ExecutionManagerError::Conflict { .. }) => {
                let current = self
                    .get(&id)
                    .await?
                    .ok_or_else(|| ExecutionManagerError::NotFound(id.clone()))?;
                if let Some(result) = completed_restart_result(
                    &current,
                    intent.source_generation,
                    &intent.operation_id,
                    intent.options(),
                )? {
                    return result;
                }
                require_matching_restart(
                    &current,
                    intent.source_generation,
                    &intent.operation_id,
                    intent.options(),
                )?;
                if managed_state(&current)? != ManagedExecutionState::RestartStarting {
                    return Err(ExecutionManagerError::Unavailable(format!(
                        "restart teardown for {id} is owned by another caller"
                    )));
                }
                current
            }
            Err(error) => return Err(error),
        };
        self.finish_restart_start(starting).await
    }

    async fn confirm_restart_kill(
        &self,
        record: &BoxRecord,
        stop_timeout_secs: Option<u64>,
    ) -> ExecutionManagerResult<()> {
        match self
            .backend
            .stop_for_restart(record, stop_timeout_secs)
            .await
        {
            Ok(_) | Err(ExecutionManagerError::NotFound(_)) => Ok(()),
            Err(error) => {
                let terminal = match self.backend.inspect(record).await {
                    Err(ExecutionManagerError::NotFound(_)) => true,
                    Ok(observation) => matches!(
                        observation.state,
                        ExecutionState::Stopped | ExecutionState::Failed
                    ),
                    Err(_) => false,
                };
                if terminal {
                    Ok(())
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn finish_restart_start(
        &self,
        record: BoxRecord,
    ) -> ExecutionManagerResult<ExecutionLease> {
        let id = execution_id(&record)?;
        match self.backend.start(&record).await {
            Ok(handle) => {
                handle.validate(&id)?;
                self.complete_restart_with_handle(&record, handle).await
            }
            Err(error) => self.resolve_restart_start_error(record, error).await,
        }
    }

    pub(super) async fn complete_restart_with_handle(
        &self,
        record: &BoxRecord,
        handle: LocalExecutionHandle,
    ) -> ExecutionManagerResult<ExecutionLease> {
        let intent = restart_intent(record)?;
        let id = execution_id(record)?;
        match self
            .transition(
                record,
                ManagedExecutionState::RestartStarting,
                ManagedExecutionState::Running,
                RuntimeUpdate::RestartHandle(handle),
            )
            .await
        {
            Ok(running) => lease_from_record(&running),
            Err(error @ ExecutionManagerError::Conflict { .. }) => {
                let current = self
                    .get(&id)
                    .await?
                    .ok_or_else(|| ExecutionManagerError::NotFound(id.clone()))?;
                match completed_restart_result(
                    &current,
                    intent.source_generation,
                    &intent.operation_id,
                    intent.options(),
                )? {
                    Some(result) => result,
                    None => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    async fn resolve_restart_start_error(
        &self,
        record: BoxRecord,
        start_error: ExecutionManagerError,
    ) -> ExecutionManagerResult<ExecutionLease> {
        let id = execution_id(&record)?;
        match self.backend.inspect(&record).await {
            Ok(observation) => {
                observation.validate(&id)?;
                match observation.state {
                    ExecutionState::Running => {
                        self.complete_restart_with_handle(
                            &record,
                            required_handle(&observation, &id)?,
                        )
                        .await
                    }
                    ExecutionState::Stopped | ExecutionState::Failed => {
                        self.publish_restart_failure(&record, observation.exit_code)
                            .await?;
                        Err(start_error)
                    }
                    ExecutionState::Created | ExecutionState::Creating | ExecutionState::Paused => {
                        Err(start_error)
                    }
                }
            }
            Err(ExecutionManagerError::NotFound(_)) => {
                self.publish_restart_failure(&record, None).await?;
                Err(start_error)
            }
            Err(_) => Err(start_error),
        }
    }

    async fn publish_restart_failure(
        &self,
        record: &BoxRecord,
        exit_code: Option<i32>,
    ) -> ExecutionManagerResult<()> {
        self.release_execution_resources(record).await?;
        let intent = restart_intent(record)?;
        let id = execution_id(record)?;
        match self
            .transition(
                record,
                ManagedExecutionState::RestartStarting,
                ManagedExecutionState::Failed,
                RuntimeUpdate::RestartFailed(exit_code),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(error @ ExecutionManagerError::Conflict { .. }) => {
                let current = self
                    .get(&id)
                    .await?
                    .ok_or_else(|| ExecutionManagerError::NotFound(id.clone()))?;
                if completed_restart_result(
                    &current,
                    intent.source_generation,
                    &intent.operation_id,
                    intent.options(),
                )?
                .is_some()
                {
                    Ok(())
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(error),
        }
    }
}

fn ensure_restart_generation_available(
    execution_id: &a3s_box_core::ExecutionId,
    generation: ExecutionGeneration,
) -> ExecutionManagerResult<()> {
    if generation.get().checked_add(1).is_some() {
        Ok(())
    } else {
        Err(ExecutionManagerError::Conflict {
            execution_id: execution_id.clone(),
            message: "execution generation is exhausted".to_string(),
        })
    }
}

fn ensure_restart_timeout_valid(
    execution_id: &a3s_box_core::ExecutionId,
    timeout_secs: Option<u64>,
) -> ExecutionManagerResult<()> {
    if timeout_secs.is_some_and(|timeout| timeout.checked_mul(1_000).is_none()) {
        Err(ExecutionManagerError::InvalidRequest(format!(
            "restart timeout is too large for execution {execution_id}"
        )))
    } else {
        Ok(())
    }
}

fn restart_intent(record: &BoxRecord) -> ExecutionManagerResult<RestartIntent> {
    let id = execution_id(record)?;
    match record
        .managed_execution
        .as_ref()
        .and_then(|metadata| metadata.pending_operation.as_ref())
    {
        Some(ManagedExecutionOperation::Restart {
            operation_id,
            source_generation,
            source_state,
            stop_timeout_secs,
        }) => Ok(RestartIntent {
            operation_id: operation_id.clone(),
            source_generation: *source_generation,
            source_state: *source_state,
            stop_timeout_secs: *stop_timeout_secs,
        }),
        _ => Err(ExecutionManagerError::Internal(format!(
            "restarting execution {id} has no persisted restart intent"
        ))),
    }
}

fn require_matching_restart(
    record: &BoxRecord,
    expected_generation: ExecutionGeneration,
    operation_id: &OperationId,
    options: RestartExecutionOptions,
) -> ExecutionManagerResult<()> {
    let id = execution_id(record)?;
    let intent = restart_intent(record)?;
    if intent.operation_id == *operation_id
        && intent.source_generation == expected_generation
        && intent.stop_timeout_secs == options.stop_timeout_secs
    {
        Ok(())
    } else {
        Err(ExecutionManagerError::Conflict {
            execution_id: id,
            message: format!(
                "restart is already owned by operation {} from generation {}",
                intent.operation_id,
                intent.source_generation.get()
            ),
        })
    }
}

fn completed_restart_result(
    record: &BoxRecord,
    expected_generation: ExecutionGeneration,
    operation_id: &OperationId,
    options: RestartExecutionOptions,
) -> ExecutionManagerResult<Option<ExecutionManagerResult<ExecutionLease>>> {
    let id = execution_id(record)?;
    let Some(completed) = record
        .managed_execution
        .as_ref()
        .and_then(|metadata| metadata.last_restart.as_ref())
        .filter(|completed| completed.operation_id == *operation_id)
    else {
        return Ok(None);
    };
    if completed.source_generation != expected_generation {
        return Ok(Some(Err(ExecutionManagerError::Conflict {
            execution_id: id,
            message: format!(
                "restart operation {operation_id} belongs to generation {}, not {}",
                completed.source_generation.get(),
                expected_generation.get()
            ),
        })));
    }
    if completed.stop_timeout_secs != options.stop_timeout_secs {
        return Ok(Some(Err(ExecutionManagerError::Conflict {
            execution_id: id,
            message: format!(
                "restart operation {operation_id} was retried with different stop options"
            ),
        })));
    }
    if completed.outcome == ManagedRestartOutcome::Failed {
        return Ok(Some(Err(ExecutionManagerError::Conflict {
            execution_id: id,
            message: format!(
                "restart operation {operation_id} failed at generation {}",
                completed.target_generation.get()
            ),
        })));
    }
    let current_generation = generation(record, &id)?;
    if managed_state(record)? == ManagedExecutionState::Running
        && current_generation == completed.target_generation
    {
        return Ok(Some(lease_from_record(record)));
    }
    Ok(Some(Err(ExecutionManagerError::Conflict {
        execution_id: id,
        message: format!(
            "restart operation {operation_id} completed, but the execution has since moved"
        ),
    })))
}
