use a3s_box_core::{
    ExecutionLease, ExecutionManagerError, ExecutionManagerResult, ExecutionState,
    KillExecutionOptions, KillOutcome,
};

use super::create::startup_terminal_state;
use super::record::{execution_id, lease_from_record};
use super::store::RuntimeUpdate;
use super::support::{
    paused_with_memory, pending_kill_options, pending_pause_policy, pending_resource_update,
    required_handle,
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
                Ok(Some(lease)) => Ok(lease),
                Ok(None) => Err(error),
                Err(resolved) => Err(resolved),
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
                    if observation.state == ExecutionState::Stopped
                        && observation.exit_code.is_none() =>
                {
                    // Clean stop without an authenticated exit: treat as a
                    // successful filesystem-only pause (lost-response safe).
                    true
                }
                Ok(observation)
                    if matches!(
                        observation.state,
                        ExecutionState::Stopped | ExecutionState::Failed
                    ) =>
                {
                    // Terminal evidence (Failed, or Stopped with exit) is not a
                    // successful cold pause. Publish the terminal state and
                    // refuse inventing Paused (which would drop the exit).
                    if self.release_execution_resources(&record).await.is_err() {
                        return Err(stop_error);
                    }
                    let terminal = startup_terminal_state(observation.state, observation.exit_code);
                    self.transition(
                        &record,
                        ManagedExecutionState::Pausing,
                        terminal,
                        RuntimeUpdate::Terminal(observation.exit_code),
                    )
                    .await?;
                    return Err(ExecutionManagerError::Unavailable(format!(
                        "filesystem-only pause observed a terminal generation (state {:?}, exit {:?}); refusing to publish Paused",
                        observation.state, observation.exit_code
                    )));
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

    async fn resolve_pause_error(
        &self,
        record: BoxRecord,
    ) -> ExecutionManagerResult<Option<ExecutionLease>> {
        let Ok(id) = execution_id(&record) else {
            return Ok(None);
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
                            .await?;
                        return Ok(Some(lease_from_record(&paused)?));
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
            Ok(observation)
                if matches!(
                    observation.state,
                    ExecutionState::Stopped | ExecutionState::Failed
                ) =>
            {
                // Terminal evidence after a failed warm pause must not leave
                // the generation stuck in Pausing (or invent Paused later).
                observation.validate(&id)?;
                self.release_execution_resources(&record).await?;
                let terminal = startup_terminal_state(observation.state, observation.exit_code);
                self.transition(
                    &record,
                    ManagedExecutionState::Pausing,
                    terminal,
                    RuntimeUpdate::Terminal(observation.exit_code),
                )
                .await?;
                return Err(ExecutionManagerError::Unavailable(format!(
                    "warm pause observed a terminal generation (state {:?}, exit {:?}); refusing to leave Pausing",
                    observation.state, observation.exit_code
                )));
            }
            _ => {}
        }
        Ok(None)
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
                Ok(Some(lease)) => Ok(lease),
                Ok(None) => Err(error),
                Err(resolved) => Err(resolved),
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
                    ExecutionState::Stopped | ExecutionState::Failed
                ) =>
            {
                // Terminal evidence after a failed cold resume must not invent
                // a retryable Paused generation (drops authenticated exit).
                self.release_execution_resources(&record).await?;
                let terminal = startup_terminal_state(observation.state, observation.exit_code);
                self.transition(
                    &record,
                    ManagedExecutionState::Resuming,
                    terminal,
                    RuntimeUpdate::Terminal(observation.exit_code),
                )
                .await?;
                Err(ExecutionManagerError::Unavailable(format!(
                    "filesystem-only resume observed a terminal generation (state {:?}, exit {:?}); refusing to publish Paused",
                    observation.state, observation.exit_code
                )))
            }
            Ok(observation)
                if matches!(
                    observation.state,
                    ExecutionState::Created | ExecutionState::Paused
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

    async fn resolve_resume_error(
        &self,
        record: BoxRecord,
    ) -> ExecutionManagerResult<Option<ExecutionLease>> {
        let Ok(id) = execution_id(&record) else {
            return Ok(None);
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
                            .await?;
                        return Ok(Some(lease_from_record(&running)?));
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
            Ok(observation)
                if matches!(
                    observation.state,
                    ExecutionState::Stopped | ExecutionState::Failed
                ) =>
            {
                // Terminal evidence after a failed warm resume must not leave
                // the generation stuck in Resuming (drops authenticated exit).
                observation.validate(&id)?;
                self.release_execution_resources(&record).await?;
                let terminal = startup_terminal_state(observation.state, observation.exit_code);
                self.transition(
                    &record,
                    ManagedExecutionState::Resuming,
                    terminal,
                    RuntimeUpdate::Terminal(observation.exit_code),
                )
                .await?;
                return Err(ExecutionManagerError::Unavailable(format!(
                    "warm resume observed a terminal generation (state {:?}, exit {:?}); refusing to leave Resuming",
                    observation.state, observation.exit_code
                )));
            }
            _ => {}
        }
        Ok(None)
    }

    pub(super) async fn finish_resource_update(
        &self,
        record: BoxRecord,
    ) -> ExecutionManagerResult<ExecutionLease> {
        let execution_id = execution_id(&record)?;
        let (operation_id, update) = pending_resource_update(&record, &execution_id)?;
        if let Err(error) = self
            .backend
            .update_resources(&record, &operation_id, &update)
            .await
        {
            let terminal = match self.backend.inspect(&record).await {
                Ok(observation)
                    if matches!(
                        observation.state,
                        ExecutionState::Stopped | ExecutionState::Failed
                    ) =>
                {
                    observation.validate(&execution_id)?;
                    Some((observation.state, observation.exit_code))
                }
                Err(ExecutionManagerError::NotFound(_)) => Some((ExecutionState::Failed, None)),
                _ => None,
            };
            if let Some((state, exit_code)) = terminal {
                self.release_execution_resources(&record).await?;
                let target = if state == ExecutionState::Stopped {
                    ManagedExecutionState::Stopped
                } else {
                    ManagedExecutionState::Failed
                };
                self.transition(
                    &record,
                    ManagedExecutionState::UpdatingResources,
                    target,
                    RuntimeUpdate::Terminal(exit_code),
                )
                .await?;
            }
            return Err(error);
        }
        let running = self.complete_resource_update(&record).await?;
        lease_from_record(&running)
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
        match self.backend.kill_with_status(&backend_record).await {
            Ok(termination) => {
                self.release_execution_resources(&record).await?;
                let exit_code =
                    kill_terminal_exit_code(termination.outcome, options, termination.exit_code);
                self.transition(
                    &record,
                    ManagedExecutionState::Killing,
                    ManagedExecutionState::Stopped,
                    kill_runtime_update(termination.outcome, exit_code),
                )
                .await?;
                Ok(termination.outcome)
            }
            Err(ExecutionManagerError::NotFound(_)) => {
                self.release_execution_resources(&record).await?;
                let exit_code = kill_terminal_exit_code(KillOutcome::AlreadyStopped, options, None);
                self.transition(
                    &record,
                    ManagedExecutionState::Killing,
                    ManagedExecutionState::Stopped,
                    kill_runtime_update(KillOutcome::AlreadyStopped, exit_code),
                )
                .await?;
                Ok(KillOutcome::AlreadyStopped)
            }
            Err(error) => match self.resolve_kill_error(record, options).await {
                Some(outcome) => Ok(outcome),
                None => Err(error),
            },
        }
    }

    async fn resolve_kill_error(
        &self,
        record: BoxRecord,
        options: KillExecutionOptions,
    ) -> Option<KillOutcome> {
        match self.backend.inspect(&record).await {
            Err(ExecutionManagerError::NotFound(_)) => {
                if self.release_execution_resources(&record).await.is_err() {
                    return None;
                }
                let exit_code = kill_terminal_exit_code(KillOutcome::AlreadyStopped, options, None);
                self.transition(
                    &record,
                    ManagedExecutionState::Killing,
                    ManagedExecutionState::Stopped,
                    kill_runtime_update(KillOutcome::AlreadyStopped, exit_code),
                )
                .await
                .ok()?;
                Some(KillOutcome::AlreadyStopped)
            }
            Ok(observation) if observation.state == ExecutionState::Failed => {
                // Crash evidence is not a user stop. Preserve exit without
                // marking stopped_by_user via KillTerminal.
                if self.release_execution_resources(&record).await.is_err() {
                    return None;
                }
                self.transition(
                    &record,
                    ManagedExecutionState::Killing,
                    ManagedExecutionState::Failed,
                    RuntimeUpdate::Terminal(observation.exit_code),
                )
                .await
                .ok()?;
                Some(KillOutcome::AlreadyStopped)
            }
            Ok(observation) if observation.state == ExecutionState::Stopped => {
                if self.release_execution_resources(&record).await.is_err() {
                    return None;
                }
                let exit_code =
                    kill_terminal_exit_code(KillOutcome::Killed, options, observation.exit_code);
                self.transition(
                    &record,
                    ManagedExecutionState::Killing,
                    ManagedExecutionState::Stopped,
                    RuntimeUpdate::KillTerminal(exit_code),
                )
                .await
                .ok()?;
                Some(KillOutcome::Killed)
            }
            _ => None,
        }
    }
}

fn kill_runtime_update(outcome: KillOutcome, exit_code: Option<i32>) -> RuntimeUpdate {
    match outcome {
        // Only a kill that was applied attributes stopped_by_user.
        KillOutcome::Killed => RuntimeUpdate::KillTerminal(exit_code),
        // AlreadyStopped / vanished-runtime cleanup must not invent user-stop.
        KillOutcome::AlreadyStopped => RuntimeUpdate::Terminal(exit_code),
    }
}

fn kill_terminal_exit_code(
    outcome: KillOutcome,
    options: KillExecutionOptions,
    observed_exit_code: Option<i32>,
) -> Option<i32> {
    if let Some(exit_code) = observed_exit_code {
        return Some(exit_code);
    }
    match outcome {
        // Only invent 128+signal when a kill was actually applied and the
        // backend could not reap an authenticated status. AlreadyStopped /
        // vanished-runtime paths must leave exit absent (no fabricated evidence).
        KillOutcome::Killed => options
            .signal
            .and_then(|signal| 128_i32.checked_add(signal)),
        KillOutcome::AlreadyStopped => None,
    }
}
