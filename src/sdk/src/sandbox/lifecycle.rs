use a3s_box_core::log::LogEntry;
use a3s_box_core::{ExecutionState, OperationId, RestartExecutionOptions};

use super::Sandbox;
use crate::{BoxStatsSummary, ClientError, Result};

/// Typed controls for an idempotent Sandbox restart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxRestartOptions {
    pub operation_id: Option<OperationId>,
    pub stop_timeout_seconds: Option<u64>,
}

impl SandboxRestartOptions {
    pub fn operation_id(mut self, operation_id: OperationId) -> Self {
        self.operation_id = Some(operation_id);
        self
    }

    pub const fn stop_timeout_seconds(mut self, seconds: u64) -> Self {
        self.stop_timeout_seconds = Some(seconds);
        self
    }
}

/// Controls for one bounded structured-log snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxLogOptions {
    pub tail: usize,
}

impl SandboxLogOptions {
    pub const fn tail(tail: usize) -> Self {
        Self { tail }
    }

    pub(crate) fn validate(self) -> Result<Self> {
        if self.tail == 0 {
            return Err(ClientError::Validation(
                "sandbox log tail must be greater than zero".to_string(),
            ));
        }
        if self.tail > 10_000 {
            return Err(ClientError::Validation(
                "sandbox log tail cannot exceed 10000 entries".to_string(),
            ));
        }
        Ok(self)
    }
}

impl Default for SandboxLogOptions {
    fn default() -> Self {
        Self { tail: 100 }
    }
}

impl Sandbox {
    /// Stop the current generation without removing its durable record.
    pub async fn stop(&self) -> Result<()> {
        let state = self.inner.state();
        if state.closed
            || matches!(
                state.state,
                ExecutionState::Stopped | ExecutionState::Failed
            )
        {
            return Ok(());
        }
        self.inner
            .client
            .kill_execution(&self.inner.execution_id, state.generation)
            .await?;
        let status = self
            .inner
            .client
            .inspect_execution(&self.inner.execution_id)
            .await?;
        self.inner.update(status.generation, status.state);
        Ok(())
    }

    /// Restart this Sandbox using a durable idempotency identity.
    pub async fn restart(&self, options: SandboxRestartOptions) -> Result<()> {
        let state = self.inner.state();
        if state.closed {
            return Err(ClientError::Validation(format!(
                "sandbox {} has been removed",
                self.id()
            )));
        }
        let operation_id = match options.operation_id {
            Some(operation_id) => operation_id,
            None => OperationId::new(format!("sdk-restart-{}", uuid::Uuid::new_v4()))
                .map_err(ClientError::Execution)?,
        };
        let lease = self
            .inner
            .client
            .restart_execution(
                &self.inner.execution_id,
                state.generation,
                &operation_id,
                RestartExecutionOptions {
                    stop_timeout_secs: options.stop_timeout_seconds,
                },
            )
            .await?;
        self.inner.update(lease.generation, ExecutionState::Running);
        Ok(())
    }

    /// Remove a created or terminal Sandbox and all runtime-owned resources.
    pub async fn remove(&self) -> Result<()> {
        let state = self.inner.state();
        if state.closed {
            return Ok(());
        }
        if !matches!(
            state.state,
            ExecutionState::Created | ExecutionState::Stopped | ExecutionState::Failed
        ) {
            return Err(ClientError::Validation(format!(
                "sandbox {} must be stopped before removal",
                self.id()
            )));
        }
        self.inner
            .client
            .remove_execution(&self.inner.execution_id, state.generation)
            .await?;
        self.inner.close(state.generation);
        Ok(())
    }

    /// Read a bounded tail of structured stdout/stderr entries.
    pub async fn logs(&self, options: SandboxLogOptions) -> Result<Vec<LogEntry>> {
        let options = options.validate()?;
        let state = self.inner.state();
        if state.closed {
            return Err(ClientError::Validation(format!(
                "sandbox {} has been removed",
                self.id()
            )));
        }
        let mut entries = self
            .inner
            .client
            .read_execution_logs(&self.inner.execution_id, state.generation)
            .await?;
        if entries.len() > options.tail {
            entries.drain(..entries.len() - options.tail);
        }
        Ok(entries)
    }

    /// Read one bounded host resource-usage snapshot when locally available.
    pub async fn stats(&self) -> Result<Option<BoxStatsSummary>> {
        let state = self.inner.state();
        if state.closed {
            return Ok(None);
        }
        let status = self
            .inner
            .client
            .inspect_execution(&self.inner.execution_id)
            .await?;
        self.inner.update(status.generation, status.state);
        self.inner.client.get_box_stats(self.id())
    }
}
