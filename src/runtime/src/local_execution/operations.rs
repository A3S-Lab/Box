use a3s_box_core::{
    ExecutionLease, ExecutionManagerError, ExecutionManagerResult, ExecutionState, KillOutcome,
};

use super::record::{execution_id, lease_from_record};
use super::store::RuntimeUpdate;
use super::support::{
    paused_with_memory, pending_kill_options, pending_pause_policy, required_handle,
};
use super::{BoxRecord, LocalExecutionManager, ManagedExecutionState};

impl LocalExecutionManager {
    pub(super) async fn finish_pause(
        &self,
        record: BoxRecord,
    ) -> ExecutionManagerResult<ExecutionLease> {
        let id = execution_id(&record)?;
        let keep_memory = pending_pause_policy(&record, &id)?;
        if !keep_memory {
            return self.finish_cold_pause(record).await;
        }
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

    async fn finish_cold_pause(&self, record: BoxRecord) -> ExecutionManagerResult<ExecutionLease> {
        let id = execution_id(&record)?;
        let stopped = match self
            .backend
            .stop_for_restart(&record, record.stop_timeout)
            .await
        {
            Ok(_) | Err(ExecutionManagerError::NotFound(_)) => true,
            Err(stop_error) => match self.backend.inspect(&record).await {
                Err(ExecutionManagerError::NotFound(_)) => true,
                Ok(observation)
                    if matches!(
                        observation.state,
                        ExecutionState::Stopped | ExecutionState::Failed
                    ) =>
                {
                    true
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
                    return Err(stop_error);
                }
                _ => return Err(stop_error),
            },
        };
        debug_assert!(stopped);
        self.release_execution_resources(&record).await?;
        let paused = self
            .complete_transition(
                &record,
                ManagedExecutionState::Pausing,
                ManagedExecutionState::Paused,
                RuntimeUpdate::ColdPause,
            )
            .await?;
        let lease = lease_from_record(&paused)?;
        debug_assert_eq!(lease.execution_id, id);
        Ok(lease)
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
        if !paused_with_memory(&record, &id)? {
            return self.finish_cold_resume(record).await;
        }
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

    async fn finish_cold_resume(
        &self,
        record: BoxRecord,
    ) -> ExecutionManagerResult<ExecutionLease> {
        let id = execution_id(&record)?;
        match self.backend.inspect(&record).await {
            Ok(observation) if observation.state == ExecutionState::Running => {
                observation.validate(&id)?;
                let running = self
                    .complete_transition(
                        &record,
                        ManagedExecutionState::Resuming,
                        ManagedExecutionState::Running,
                        RuntimeUpdate::StartHandle(required_handle(&observation, &id)?),
                    )
                    .await?;
                return lease_from_record(&running);
            }
            Err(ExecutionManagerError::NotFound(_)) => {}
            Ok(observation)
                if matches!(
                    observation.state,
                    ExecutionState::Stopped | ExecutionState::Failed
                ) => {}
            Ok(observation) => {
                return Err(ExecutionManagerError::Conflict {
                    execution_id: id,
                    message: format!(
                        "filesystem-only resume found unexpected backend state {:?}",
                        observation.state
                    ),
                });
            }
            Err(error) => return Err(error),
        }
        match self.backend.start(&record).await {
            Ok(handle) => {
                handle.validate(&id)?;
                let running = self
                    .complete_transition(
                        &record,
                        ManagedExecutionState::Resuming,
                        ManagedExecutionState::Running,
                        RuntimeUpdate::StartHandle(handle),
                    )
                    .await?;
                lease_from_record(&running)
            }
            Err(start_error) => self.resolve_cold_resume_error(record, start_error).await,
        }
    }

    async fn resolve_cold_resume_error(
        &self,
        record: BoxRecord,
        start_error: ExecutionManagerError,
    ) -> ExecutionManagerResult<ExecutionLease> {
        let id = execution_id(&record)?;
        match self.backend.inspect(&record).await {
            Ok(observation) if observation.state == ExecutionState::Running => {
                observation.validate(&id)?;
                let running = self
                    .complete_transition(
                        &record,
                        ManagedExecutionState::Resuming,
                        ManagedExecutionState::Running,
                        RuntimeUpdate::StartHandle(required_handle(&observation, &id)?),
                    )
                    .await?;
                lease_from_record(&running)
            }
            Err(ExecutionManagerError::NotFound(_)) => {
                self.rollback_cold_resume(&record).await?;
                Err(start_error)
            }
            Ok(observation)
                if matches!(
                    observation.state,
                    ExecutionState::Created
                        | ExecutionState::Paused
                        | ExecutionState::Stopped
                        | ExecutionState::Failed
                ) =>
            {
                self.rollback_cold_resume(&record).await?;
                Err(start_error)
            }
            Ok(_) | Err(_) => Err(start_error),
        }
    }

    async fn rollback_cold_resume(&self, record: &BoxRecord) -> ExecutionManagerResult<()> {
        self.release_execution_resources(record).await?;
        self.transition(
            record,
            ManagedExecutionState::Resuming,
            ManagedExecutionState::Paused,
            RuntimeUpdate::None,
        )
        .await?;
        Ok(())
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
        let execution_id = execution_id(&record)?;
        let options = pending_kill_options(&record, &execution_id)?;
        let mut backend_record = record.clone();
        if let Some(signal) = options.signal {
            backend_record.stop_signal = Some(signal.to_string());
        }
        if let Some(timeout_secs) = options.timeout_secs {
            backend_record.stop_timeout = Some(timeout_secs);
        }
        match self.backend.kill(&backend_record).await {
            Ok(outcome) => {
                self.release_execution_resources(&record).await?;
                self.transition(
                    &record,
                    ManagedExecutionState::Killing,
                    ManagedExecutionState::Stopped,
                    RuntimeUpdate::KillTerminal(None),
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
                    RuntimeUpdate::KillTerminal(None),
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
            RuntimeUpdate::KillTerminal(None),
        )
        .await
        .ok()?;
        Some(KillOutcome::Killed)
    }
}
