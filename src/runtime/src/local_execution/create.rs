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
                                self.transition(
                                    &record,
                                    ManagedExecutionState::Starting,
                                    ManagedExecutionState::Failed,
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
                        let _ = self
                            .transition(
                                &claimed,
                                ManagedExecutionState::Starting,
                                ManagedExecutionState::Failed,
                                RuntimeUpdate::Terminal(observation.exit_code),
                            )
                            .await;
                        Err(start_error)
                    }
                    ExecutionState::Created | ExecutionState::Creating | ExecutionState::Paused => {
                        Err(start_error)
                    }
                }
            }
            Err(ExecutionManagerError::NotFound(_)) => {
                let _ = self
                    .transition(
                        &claimed,
                        ManagedExecutionState::Starting,
                        ManagedExecutionState::Failed,
                        RuntimeUpdate::Terminal(None),
                    )
                    .await;
                Err(start_error)
            }
            Err(_) => Err(start_error),
        }
    }
}
