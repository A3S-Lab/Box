use a3s_box_core::{
    ExecutionLease, ExecutionManagerError, ExecutionManagerResult, ExecutionState, KillOutcome,
};

use super::record::{execution_id, lease_from_record};
use super::store::RuntimeUpdate;
use super::support::{pending_pause_policy, required_handle};
use super::{BoxRecord, LocalExecutionManager, ManagedExecutionState};

impl LocalExecutionManager {
    pub(super) async fn finish_pause(
        &self,
        record: BoxRecord,
    ) -> ExecutionManagerResult<ExecutionLease> {
        let id = execution_id(&record)?;
        let keep_memory = pending_pause_policy(&record, &id)?;
        match self.backend.pause(&record, keep_memory).await {
            Ok(handle) => {
                handle.validate(&id)?;
                let paused = self
                    .complete_with_handle(
                        &record,
                        ManagedExecutionState::Pausing,
                        ManagedExecutionState::Paused,
                        handle,
                    )
                    .await?;
                lease_from_record(&paused)
            }
            Err(error) => match self.resolve_pause_error(record).await {
                Some(lease) => Ok(lease),
                None => Err(error),
            },
        }
    }

    async fn resolve_pause_error(&self, record: BoxRecord) -> Option<ExecutionLease> {
        let Ok(id) = execution_id(&record) else {
            return None;
        };
        match self.backend.inspect(&record).await {
            Ok(observation) if observation.state == ExecutionState::Paused => {
                if observation.validate(&id).is_ok() {
                    if let Ok(handle) = required_handle(&observation, &id) {
                        let paused = self
                            .complete_with_handle(
                                &record,
                                ManagedExecutionState::Pausing,
                                ManagedExecutionState::Paused,
                                handle,
                            )
                            .await
                            .ok()?;
                        return lease_from_record(&paused).ok();
                    }
                }
            }
            Ok(observation) if observation.state == ExecutionState::Running => {
                let _ = self
                    .transition(
                        &record,
                        ManagedExecutionState::Pausing,
                        ManagedExecutionState::Running,
                        RuntimeUpdate::None,
                    )
                    .await;
            }
            _ => {}
        }
        None
    }

    pub(super) async fn finish_resume(
        &self,
        record: BoxRecord,
    ) -> ExecutionManagerResult<ExecutionLease> {
        let id = execution_id(&record)?;
        match self.backend.resume(&record).await {
            Ok(handle) => {
                handle.validate(&id)?;
                let running = self
                    .complete_with_handle(
                        &record,
                        ManagedExecutionState::Resuming,
                        ManagedExecutionState::Running,
                        handle,
                    )
                    .await?;
                lease_from_record(&running)
            }
            Err(error) => match self.resolve_resume_error(record).await {
                Some(lease) => Ok(lease),
                None => Err(error),
            },
        }
    }

    async fn resolve_resume_error(&self, record: BoxRecord) -> Option<ExecutionLease> {
        let Ok(id) = execution_id(&record) else {
            return None;
        };
        match self.backend.inspect(&record).await {
            Ok(observation) if observation.state == ExecutionState::Running => {
                if observation.validate(&id).is_ok() {
                    if let Ok(handle) = required_handle(&observation, &id) {
                        let running = self
                            .complete_with_handle(
                                &record,
                                ManagedExecutionState::Resuming,
                                ManagedExecutionState::Running,
                                handle,
                            )
                            .await
                            .ok()?;
                        return lease_from_record(&running).ok();
                    }
                }
            }
            Ok(observation) if observation.state == ExecutionState::Paused => {
                let _ = self
                    .transition(
                        &record,
                        ManagedExecutionState::Resuming,
                        ManagedExecutionState::Paused,
                        RuntimeUpdate::None,
                    )
                    .await;
            }
            _ => {}
        }
        None
    }

    pub(super) async fn finish_kill(
        &self,
        record: BoxRecord,
    ) -> ExecutionManagerResult<KillOutcome> {
        match self.backend.kill(&record).await {
            Ok(outcome) => {
                self.release_execution_resources(&record).await?;
                self.transition(
                    &record,
                    ManagedExecutionState::Killing,
                    ManagedExecutionState::Stopped,
                    RuntimeUpdate::Terminal(None),
                )
                .await?;
                Ok(outcome)
            }
            Err(ExecutionManagerError::NotFound(_)) => {
                self.release_execution_resources(&record).await?;
                self.transition(
                    &record,
                    ManagedExecutionState::Killing,
                    ManagedExecutionState::Stopped,
                    RuntimeUpdate::Terminal(None),
                )
                .await?;
                Ok(KillOutcome::AlreadyStopped)
            }
            Err(error) => match self.resolve_kill_error(record).await {
                Some(outcome) => Ok(outcome),
                None => Err(error),
            },
        }
    }

    async fn resolve_kill_error(&self, record: BoxRecord) -> Option<KillOutcome> {
        let terminal = match self.backend.inspect(&record).await {
            Err(ExecutionManagerError::NotFound(_)) => true,
            Ok(observation)
                if matches!(
                    observation.state,
                    ExecutionState::Stopped | ExecutionState::Failed
                ) =>
            {
                true
            }
            _ => false,
        };
        if !terminal {
            return None;
        }
        if self.release_execution_resources(&record).await.is_err() {
            return None;
        }
        self.transition(
            &record,
            ManagedExecutionState::Killing,
            ManagedExecutionState::Stopped,
            RuntimeUpdate::Terminal(None),
        )
        .await
        .ok()?;
        Some(KillOutcome::Killed)
    }
}
