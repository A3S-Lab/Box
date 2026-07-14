//! Injectable process/runtime boundary for local execution orchestration.

use std::path::PathBuf;

use a3s_box_core::{
    ExecutionId, ExecutionManagerError, ExecutionManagerResult, ExecutionState, KillOutcome,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::BoxRecord;

/// Runtime evidence persisted after an execution becomes ready.
#[derive(Debug, Clone)]
pub struct LocalExecutionHandle {
    pub started_at: DateTime<Utc>,
    pub pid: Option<u32>,
    pub pid_start_time: Option<u64>,
    pub exec_socket_path: PathBuf,
    pub console_log: PathBuf,
    pub anonymous_volumes: Vec<String>,
}

impl LocalExecutionHandle {
    pub(crate) fn validate(&self, execution_id: &ExecutionId) -> ExecutionManagerResult<()> {
        if self.pid.is_none() && self.pid_start_time.is_some() {
            return Err(ExecutionManagerError::Internal(format!(
                "backend returned a PID start time without a PID for {execution_id}"
            )));
        }
        if self.exec_socket_path.as_os_str().is_empty() {
            return Err(ExecutionManagerError::Internal(format!(
                "backend returned an empty exec socket path for {execution_id}"
            )));
        }
        if self.console_log.as_os_str().is_empty() {
            return Err(ExecutionManagerError::Internal(format!(
                "backend returned an empty console log path for {execution_id}"
            )));
        }
        Ok(())
    }
}

/// One backend observation used during inspection and restart recovery.
#[derive(Debug, Clone)]
pub struct LocalExecutionObservation {
    pub state: ExecutionState,
    pub handle: Option<LocalExecutionHandle>,
    pub exit_code: Option<i32>,
}

impl LocalExecutionObservation {
    pub(crate) fn validate(&self, execution_id: &ExecutionId) -> ExecutionManagerResult<()> {
        match self.state {
            ExecutionState::Running | ExecutionState::Paused => {
                self.handle.as_ref().ok_or_else(|| {
                    ExecutionManagerError::Internal(format!(
                        "backend returned {:?} without runtime evidence for {execution_id}",
                        self.state
                    ))
                })?;
            }
            ExecutionState::Created
            | ExecutionState::Creating
            | ExecutionState::Stopped
            | ExecutionState::Failed => {}
        }
        if let Some(handle) = &self.handle {
            handle.validate(execution_id)?;
        }
        Ok(())
    }
}

/// Backend operations invoked outside the durable state lock.
///
/// Implementations must key all host/runtime paths by [`BoxRecord::id`]. The
/// external sandbox ID in managed metadata is an untrusted diagnostic label.
#[async_trait]
pub trait LocalExecutionBackend: Send + Sync {
    async fn start(&self, record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle>;

    async fn inspect(
        &self,
        record: &BoxRecord,
    ) -> ExecutionManagerResult<LocalExecutionObservation>;

    async fn pause(
        &self,
        record: &BoxRecord,
        keep_memory: bool,
    ) -> ExecutionManagerResult<LocalExecutionHandle>;

    async fn resume(&self, record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle>;

    async fn kill(&self, record: &BoxRecord) -> ExecutionManagerResult<KillOutcome>;
}
