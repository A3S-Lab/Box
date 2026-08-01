//! Injectable process/runtime boundary for local execution orchestration.

use std::path::PathBuf;

use a3s_box_core::{
    pty::PtyRequest, ExecOutput, ExecRequest, ExecutionEventBatch, ExecutionEventsRequest,
    ExecutionId, ExecutionManagerError, ExecutionManagerResult, ExecutionProcess,
    ExecutionProcessInventory, ExecutionResourceUpdate, ExecutionState, ExecutionStats,
    FileRequest, FileResponse, FilesystemRequest, FilesystemResponse, KillOutcome, OperationId,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::local_execution::OciRuntimeBinding;
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
    /// Exact A3S OCI identity when lifecycle ownership is delegated through
    /// the public SDK rather than a Box-owned VM process.
    pub oci_runtime: Option<OciRuntimeBinding>,
}

impl LocalExecutionHandle {
    pub(crate) fn validate(&self, execution_id: &ExecutionId) -> ExecutionManagerResult<()> {
        if self.pid.is_none() && self.pid_start_time.is_some() {
            return Err(ExecutionManagerError::Internal(format!(
                "backend returned a PID start time without a PID for {execution_id}"
            )));
        }
        if let Some(binding) = &self.oci_runtime {
            binding.validate_for(execution_id)?;
        }
        if self.exec_socket_path.as_os_str().is_empty() && self.oci_runtime.is_none() {
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

/// Result of a terminal backend operation, including authoritative status when
/// the runtime was able to observe it before teardown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalExecutionTermination {
    pub outcome: KillOutcome,
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
    /// Reject an unsupported execution before its durable reservation is
    /// published. Backends must repeat mutable capability checks at launch.
    async fn preflight(&self, _record: &BoxRecord) -> ExecutionManagerResult<()> {
        Ok(())
    }

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

    /// Validate a complete live resource contract before a durable mutation claim.
    async fn preflight_resource_update(
        &self,
        _record: &BoxRecord,
        _update: &ExecutionResourceUpdate,
    ) -> ExecutionManagerResult<()> {
        Err(ExecutionManagerError::Unavailable(
            "this execution backend does not support live resource updates".to_string(),
        ))
    }

    /// Apply the exact persisted resource update operation.
    async fn update_resources(
        &self,
        _record: &BoxRecord,
        _operation_id: &OperationId,
        _update: &ExecutionResourceUpdate,
    ) -> ExecutionManagerResult<()> {
        Err(ExecutionManagerError::Unavailable(
            "this execution backend does not support live resource updates".to_string(),
        ))
    }

    /// Return live init and exec processes for the record's exact runtime target.
    async fn list_processes(
        &self,
        _record: &BoxRecord,
    ) -> ExecutionManagerResult<ExecutionProcessInventory> {
        Err(ExecutionManagerError::Unavailable(
            "this execution backend does not expose process inventory".to_string(),
        ))
    }

    /// Return normalized counters for the record's exact runtime target.
    async fn stats(&self, _record: &BoxRecord) -> ExecutionManagerResult<ExecutionStats> {
        Err(ExecutionManagerError::Unavailable(
            "this execution backend does not expose runtime stats".to_string(),
        ))
    }

    /// Poll ordered events for the record's exact runtime target.
    async fn events(
        &self,
        _record: &BoxRecord,
        _request: ExecutionEventsRequest,
    ) -> ExecutionManagerResult<ExecutionEventBatch> {
        Err(ExecutionManagerError::Unavailable(
            "this execution backend does not expose runtime events".to_string(),
        ))
    }

    /// Execute one captured process through a backend-owned, generation-fenced
    /// session boundary. The legacy VM backend retains its socket transport;
    /// SDK-backed implementations override this method.
    async fn execute(
        &self,
        _record: &BoxRecord,
        _request: ExecRequest,
    ) -> ExecutionManagerResult<ExecOutput> {
        Err(ExecutionManagerError::Unavailable(
            "this execution backend does not expose captured process sessions".to_string(),
        ))
    }

    /// Start one streaming non-terminal process through the backend boundary.
    async fn start_process(
        &self,
        _record: &BoxRecord,
        _request: ExecRequest,
    ) -> ExecutionManagerResult<ExecutionProcess> {
        Err(ExecutionManagerError::Unavailable(
            "this execution backend does not expose streaming process sessions".to_string(),
        ))
    }

    /// Start one interactive terminal process through the backend boundary.
    async fn start_pty(
        &self,
        _record: &BoxRecord,
        _request: PtyRequest,
    ) -> ExecutionManagerResult<ExecutionProcess> {
        Err(ExecutionManagerError::Unavailable(
            "this execution backend does not expose PTY sessions".to_string(),
        ))
    }

    /// Transfer one file through the backend's exact-generation session.
    async fn transfer_file(
        &self,
        _record: &BoxRecord,
        _request: FileRequest,
    ) -> ExecutionManagerResult<FileResponse> {
        Err(ExecutionManagerError::Unavailable(
            "this execution backend does not expose file transfer sessions".to_string(),
        ))
    }

    /// Inspect or mutate the exact generation's workload filesystem.
    async fn filesystem(
        &self,
        _record: &BoxRecord,
        _request: FilesystemRequest,
    ) -> ExecutionManagerResult<FilesystemResponse> {
        Err(ExecutionManagerError::Unavailable(
            "this execution backend does not expose filesystem sessions".to_string(),
        ))
    }

    /// Make a stopped, storage-retained rootfs available for a filesystem
    /// snapshot without starting the execution runtime.
    async fn prepare_quiescent_rootfs(&self, _record: &BoxRecord) -> ExecutionManagerResult<()> {
        Ok(())
    }

    /// Release any transient rootfs mount created by
    /// [`Self::prepare_quiescent_rootfs`] while retaining guest data.
    async fn cleanup_quiescent_rootfs(&self, _record: &BoxRecord) -> ExecutionManagerResult<()> {
        Ok(())
    }

    /// Stop the current runtime while preserving execution-owned storage for
    /// the replacement generation.
    async fn stop_for_restart(
        &self,
        record: &BoxRecord,
        _timeout_secs: Option<u64>,
    ) -> ExecutionManagerResult<KillOutcome> {
        self.kill(record).await
    }

    async fn kill(&self, record: &BoxRecord) -> ExecutionManagerResult<KillOutcome>;

    /// Kill the current execution and return its exact terminal status when the
    /// backend provides one. Implementations that cannot observe status retain
    /// the legacy `kill` behavior through this default.
    async fn kill_with_status(
        &self,
        record: &BoxRecord,
    ) -> ExecutionManagerResult<LocalExecutionTermination> {
        Ok(LocalExecutionTermination {
            outcome: self.kill(record).await?,
            exit_code: None,
        })
    }
}
