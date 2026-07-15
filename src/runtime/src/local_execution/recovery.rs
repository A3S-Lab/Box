use a3s_box_core::{
    ExecutionManagerError, ExecutionManagerResult, ExecutionState, ReconcileOutcome,
};

use super::record::{execution_id, lease_from_record};
use super::support::{
    managed_state, outcome_from_record, pending_restart_source_state, required_handle,
};
use super::{BoxRecord, LocalExecutionManager, ManagedExecutionState, RuntimeUpdate};

impl LocalExecutionManager {
    pub(super) async fn observe_record(
        &self,
        record: BoxRecord,
    ) -> ExecutionManagerResult<(BoxRecord, ExecutionState)> {
        let internal = managed_state(&record)?;
        match internal {
            ManagedExecutionState::Creating | ManagedExecutionState::Created => {
                return Ok((record, ExecutionState::Created));
            }
            ManagedExecutionState::Stopped => return Ok((record, ExecutionState::Stopped)),
            ManagedExecutionState::Failed => return Ok((record, ExecutionState::Failed)),
            ManagedExecutionState::RestartStopping => {
                let id = execution_id(&record)?;
                if matches!(
                    pending_restart_source_state(&record, &id)?,
                    ManagedExecutionState::Created
                        | ManagedExecutionState::Stopped
                        | ManagedExecutionState::Failed
                ) {
                    return Ok((record, ExecutionState::Creating));
                }
            }
            _ => {}
        }
        let id = execution_id(&record)?;
        let observation = match self.backend.inspect(&record).await {
            Ok(observation) => observation,
            Err(ExecutionManagerError::NotFound(_)) => {
                if matches!(
                    internal,
                    ManagedExecutionState::Starting
                        | ManagedExecutionState::RestartStopping
                        | ManagedExecutionState::RestartStarting
                ) {
                    return Ok((record, ExecutionState::Creating));
                }
                let terminal = if internal == ManagedExecutionState::Killing {
                    ManagedExecutionState::Stopped
                } else {
                    ManagedExecutionState::Failed
                };
                self.release_execution_resources(&record).await?;
                let record = self
                    .transition(&record, internal, terminal, RuntimeUpdate::Terminal(None))
                    .await?;
                let state = if terminal == ManagedExecutionState::Stopped {
                    ExecutionState::Stopped
                } else {
                    ExecutionState::Failed
                };
                return Ok((record, state));
            }
            Err(error) => return Err(error),
        };
        observation.validate(&id)?;

        match (internal, observation.state) {
            (ManagedExecutionState::Starting, ExecutionState::Running) => {
                let record = self
                    .complete_with_handle(
                        &record,
                        internal,
                        ManagedExecutionState::Running,
                        required_handle(&observation, &id)?,
                    )
                    .await?;
                Ok((record, ExecutionState::Running))
            }
            (ManagedExecutionState::Starting, ExecutionState::Creating) => {
                Ok((record, ExecutionState::Creating))
            }
            (ManagedExecutionState::Pausing, ExecutionState::Paused) => {
                let record = self
                    .complete_with_handle(
                        &record,
                        internal,
                        ManagedExecutionState::Paused,
                        required_handle(&observation, &id)?,
                    )
                    .await?;
                Ok((record, ExecutionState::Paused))
            }
            (ManagedExecutionState::Pausing, ExecutionState::Running)
            | (ManagedExecutionState::Pausing, ExecutionState::Creating) => {
                Ok((record, ExecutionState::Running))
            }
            (ManagedExecutionState::Resuming, ExecutionState::Running) => {
                let record = self
                    .complete_with_handle(
                        &record,
                        internal,
                        ManagedExecutionState::Running,
                        required_handle(&observation, &id)?,
                    )
                    .await?;
                Ok((record, ExecutionState::Running))
            }
            (ManagedExecutionState::Resuming, ExecutionState::Paused)
            | (ManagedExecutionState::Resuming, ExecutionState::Creating) => {
                Ok((record, ExecutionState::Paused))
            }
            (ManagedExecutionState::Killing, ExecutionState::Running) => {
                Ok((record, ExecutionState::Running))
            }
            (ManagedExecutionState::Killing, ExecutionState::Paused) => {
                Ok((record, ExecutionState::Paused))
            }
            (ManagedExecutionState::Killing, ExecutionState::Creating) => {
                Ok((record, ExecutionState::Creating))
            }
            (
                ManagedExecutionState::RestartStopping,
                state @ (ExecutionState::Running | ExecutionState::Paused),
            ) => Ok((record, state)),
            (
                ManagedExecutionState::RestartStopping,
                ExecutionState::Created
                | ExecutionState::Creating
                | ExecutionState::Stopped
                | ExecutionState::Failed,
            ) => Ok((record, ExecutionState::Creating)),
            (ManagedExecutionState::RestartStarting, ExecutionState::Running) => {
                self.complete_restart_with_handle(&record, required_handle(&observation, &id)?)
                    .await?;
                let current = self
                    .get(&id)
                    .await?
                    .ok_or_else(|| ExecutionManagerError::NotFound(id.clone()))?;
                Ok((current, ExecutionState::Running))
            }
            (
                ManagedExecutionState::RestartStarting,
                ExecutionState::Created | ExecutionState::Creating,
            ) => Ok((record, ExecutionState::Creating)),
            (
                ManagedExecutionState::RestartStarting,
                ExecutionState::Stopped | ExecutionState::Failed,
            ) => {
                let record = self
                    .transition(
                        &record,
                        ManagedExecutionState::RestartStarting,
                        ManagedExecutionState::Failed,
                        RuntimeUpdate::RestartFailed(observation.exit_code),
                    )
                    .await?;
                Ok((record, ExecutionState::Failed))
            }
            (ManagedExecutionState::Running, ExecutionState::Running) => {
                Ok((record, ExecutionState::Running))
            }
            (ManagedExecutionState::Paused, ExecutionState::Paused) => {
                Ok((record, ExecutionState::Paused))
            }
            (_, ExecutionState::Stopped) | (_, ExecutionState::Failed) => {
                let target = if observation.state == ExecutionState::Stopped {
                    ManagedExecutionState::Stopped
                } else {
                    ManagedExecutionState::Failed
                };
                self.release_execution_resources(&record).await?;
                let record = self
                    .transition(
                        &record,
                        internal,
                        target,
                        RuntimeUpdate::Terminal(observation.exit_code),
                    )
                    .await?;
                Ok((record, observation.state))
            }
            _ => Err(ExecutionManagerError::Internal(format!(
                "persisted state {internal} disagrees with backend state {:?} for {id}",
                observation.state
            ))),
        }
    }

    pub(super) async fn recover_start(
        &self,
        record: BoxRecord,
    ) -> ExecutionManagerResult<ReconcileOutcome> {
        let state = managed_state(&record)?;
        let record = if state == ManagedExecutionState::Starting {
            match self.backend.inspect(&record).await {
                Err(ExecutionManagerError::NotFound(_)) => {
                    self.transition(
                        &record,
                        ManagedExecutionState::Starting,
                        ManagedExecutionState::Created,
                        RuntimeUpdate::None,
                    )
                    .await?
                }
                Ok(observation) => {
                    let id = execution_id(&record)?;
                    observation.validate(&id)?;
                    if observation.state == ExecutionState::Running {
                        let running = self
                            .complete_with_handle(
                                &record,
                                ManagedExecutionState::Starting,
                                ManagedExecutionState::Running,
                                required_handle(&observation, &id)?,
                            )
                            .await?;
                        return Ok(ReconcileOutcome::Ready(lease_from_record(&running)?));
                    }
                    if observation.state == ExecutionState::Creating {
                        return Ok(ReconcileOutcome::Creating);
                    }
                    let failed = self
                        .transition(
                            &record,
                            ManagedExecutionState::Starting,
                            ManagedExecutionState::Failed,
                            RuntimeUpdate::Terminal(observation.exit_code),
                        )
                        .await?;
                    return outcome_from_record(failed, ExecutionState::Failed);
                }
                Err(error) => return Err(error),
            }
        } else {
            record
        };
        self.claim_and_start(record, ManagedExecutionState::Created)
            .await
            .map(ReconcileOutcome::Ready)
    }
}
