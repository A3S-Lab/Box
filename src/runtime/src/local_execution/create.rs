use a3s_box_core::{ExecutionLease, ExecutionManagerError, ExecutionManagerResult, ExecutionState};

use super::record::{execution_id, lease_from_record};
use super::store::RuntimeUpdate;
use super::support::{managed_state, required_handle};
use super::{BoxRecord, LocalExecutionManager, ManagedExecutionState};

impl LocalExecutionManager {
    pub(super) async fn ensure_started(
        &self,
        record: BoxRecord,
    ) -> ExecutionManagerResult<ExecutionLease> {
        super::record::validate_record_health(&record)?;
        match managed_state(&record)? {
            state @ (ManagedExecutionState::Creating | ManagedExecutionState::Created) => {
                self.claim_and_start(record, state).await
            }
            ManagedExecutionState::Starting => {
                let execution_id = execution_id(&record)?;
                match self.backend.inspect(&record).await {
                    Ok(observation) => {
                        observation.validate(&execution_id)?;
                        match observation.state {
                            ExecutionState::Running => {
                                let handle = required_handle(&observation, &execution_id)?;
                                let running = self
                                    .complete_with_handle(
                                        &record,
                                        ManagedExecutionState::Starting,
                                        ManagedExecutionState::Running,
                                        handle,
                                    )
                                    .await?;
                                lease_from_record(&running)
                            }
                            ExecutionState::Creating => Err(ExecutionManagerError::Unavailable(
                                format!("execution {execution_id} is still starting"),
                            )),
                            ExecutionState::Created => {
                                Err(ExecutionManagerError::Internal(format!(
                                    "backend reported created state while starting {execution_id}"
                                )))
                            }
                            ExecutionState::Stopped | ExecutionState::Failed => {
                                self.release_execution_resources(&record).await?;
                                let terminal_state = startup_terminal_state(
                                    observation.state,
                                    observation.exit_code,
                                );
                                self.transition(
                                    &record,
                                    ManagedExecutionState::Starting,
                                    terminal_state,
                                    RuntimeUpdate::Terminal(observation.exit_code),
                                )
                                .await?;
                                Err(ExecutionManagerError::Conflict {
                                    execution_id: execution_id.clone(),
                                    message: "the reserved creation operation is terminal"
                                        .to_string(),
                                })
                            }
                            ExecutionState::Paused => Err(ExecutionManagerError::Internal(
                                format!("execution {execution_id} became paused while starting"),
                            )),
                        }
                    }
                    Err(ExecutionManagerError::NotFound(_)) => {
                        Err(ExecutionManagerError::Unavailable(format!(
                            "execution {execution_id} has been claimed for startup"
                        )))
                    }
                    Err(error) => Err(error),
                }
            }
            ManagedExecutionState::Running => lease_from_record(&record),
            state => Err(ExecutionManagerError::Conflict {
                execution_id: execution_id(&record)?,
                message: format!("creation operation is {state}"),
            }),
        }
    }

    pub(super) async fn claim_and_start(
        &self,
        record: BoxRecord,
        expected_state: ManagedExecutionState,
    ) -> ExecutionManagerResult<ExecutionLease> {
        super::record::validate_record_health(&record)?;
        let claimed = match self
            .transition(
                &record,
                expected_state,
                ManagedExecutionState::Starting,
                RuntimeUpdate::None,
            )
            .await
        {
            Ok(claimed) => claimed,
            Err(ExecutionManagerError::Conflict { .. }) => {
                let id = execution_id(&record)?;
                let current = self
                    .get(&id)
                    .await?
                    .ok_or_else(|| ExecutionManagerError::NotFound(id.clone()))?;
                return self.ensure_started_after_lost_claim(current).await;
            }
            Err(error) => return Err(error),
        };

        let execution_id = execution_id(&claimed)?;
        match self.backend.start(&claimed).await {
            Ok(handle) => {
                handle.validate(&execution_id)?;
                let running = self
                    .complete_with_handle(
                        &claimed,
                        ManagedExecutionState::Starting,
                        ManagedExecutionState::Running,
                        handle,
                    )
                    .await?;
                lease_from_record(&running)
            }
            Err(error) => self.resolve_start_error(claimed, error).await,
        }
    }

    async fn ensure_started_after_lost_claim(
        &self,
        record: BoxRecord,
    ) -> ExecutionManagerResult<ExecutionLease> {
        match managed_state(&record)? {
            ManagedExecutionState::Running => lease_from_record(&record),
            ManagedExecutionState::Creating | ManagedExecutionState::Created => {
                Err(ExecutionManagerError::Unavailable(format!(
                    "execution {} startup claim was released; retry the request",
                    execution_id(&record)?
                )))
            }
            ManagedExecutionState::Starting => {
                let id = execution_id(&record)?;
                match self.backend.inspect(&record).await {
                    Ok(observation) if observation.state == ExecutionState::Running => {
                        observation.validate(&id)?;
                        let running = self
                            .complete_with_handle(
                                &record,
                                ManagedExecutionState::Starting,
                                ManagedExecutionState::Running,
                                required_handle(&observation, &id)?,
                            )
                            .await?;
                        lease_from_record(&running)
                    }
                    Ok(_) | Err(ExecutionManagerError::NotFound(_)) => {
                        Err(ExecutionManagerError::Unavailable(format!(
                            "execution {id} startup is owned by another caller"
                        )))
                    }
                    Err(error) => Err(error),
                }
            }
            state => Err(ExecutionManagerError::Conflict {
                execution_id: execution_id(&record)?,
                message: format!("creation claim moved to {state}"),
            }),
        }
    }

    async fn resolve_start_error(
        &self,
        claimed: BoxRecord,
        start_error: ExecutionManagerError,
    ) -> ExecutionManagerResult<ExecutionLease> {
        let id = execution_id(&claimed)?;
        match self.backend.inspect(&claimed).await {
            Ok(observation) => {
                observation.validate(&id)?;
                match observation.state {
                    ExecutionState::Running => {
                        let running = self
                            .complete_with_handle(
                                &claimed,
                                ManagedExecutionState::Starting,
                                ManagedExecutionState::Running,
                                required_handle(&observation, &id)?,
                            )
                            .await?;
                        lease_from_record(&running)
                    }
                    ExecutionState::Stopped | ExecutionState::Failed => {
                        if let Err(error) = self.release_execution_resources(&claimed).await {
                            return Err(startup_reconciliation_error(
                                &id,
                                &start_error,
                                "release execution resources",
                                error,
                            ));
                        }
                        let terminal_state =
                            startup_terminal_state(observation.state, observation.exit_code);
                        if let Err(error) = self
                            .complete_transition(
                                &claimed,
                                ManagedExecutionState::Starting,
                                terminal_state,
                                RuntimeUpdate::Terminal(observation.exit_code),
                            )
                            .await
                        {
                            return Err(startup_reconciliation_error(
                                &id,
                                &start_error,
                                "persist terminal state",
                                error,
                            ));
                        }
                        Err(start_error)
                    }
                    ExecutionState::Created | ExecutionState::Creating | ExecutionState::Paused => {
                        Err(start_error)
                    }
                }
            }
            Err(ExecutionManagerError::NotFound(_)) => {
                if let Err(error) = self
                    .complete_transition(
                        &claimed,
                        ManagedExecutionState::Starting,
                        ManagedExecutionState::Failed,
                        RuntimeUpdate::Terminal(None),
                    )
                    .await
                {
                    return Err(startup_reconciliation_error(
                        &id,
                        &start_error,
                        "persist provider loss",
                        error,
                    ));
                }
                Err(start_error)
            }
            Err(error) => Err(startup_reconciliation_error(
                &id,
                &start_error,
                "inspect backend state",
                error,
            )),
        }
    }
}

fn startup_reconciliation_error(
    execution_id: &a3s_box_core::ExecutionId,
    start_error: &ExecutionManagerError,
    action: &str,
    reconciliation_error: ExecutionManagerError,
) -> ExecutionManagerError {
    ExecutionManagerError::Unavailable(format!(
        "execution {execution_id} failed during startup: {start_error}; failed to {action} during reconciliation: {reconciliation_error}"
    ))
}

pub(super) fn startup_terminal_state(
    backend_state: ExecutionState,
    exit_code: Option<i32>,
) -> ManagedExecutionState {
    if backend_state == ExecutionState::Stopped && exit_code.is_some() {
        ManagedExecutionState::Stopped
    } else {
        ManagedExecutionState::Failed
    }
}
