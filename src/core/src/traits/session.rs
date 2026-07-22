//! Backend-neutral command, PTY, and file access for managed executions.

use std::sync::Arc;

use async_trait::async_trait;

use crate::exec::{
    ExecEvent, ExecOutput, ExecRequest, FileRequest, FileResponse, FilesystemRequest,
    FilesystemResponse,
};
use crate::pty::PtyRequest;

use super::execution::{
    ExecutionGeneration, ExecutionId, ExecutionManagerError, ExecutionManagerResult,
};

/// Signals supported by the backend-neutral managed process channel.
///
/// A3S workloads always execute in a Linux guest or Linux OCI Sandbox, so the
/// numeric values are stable even when the host itself is macOS or Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionProcessSignal {
    Terminate,
    Kill,
}

impl ExecutionProcessSignal {
    pub const fn linux_number(self) -> i32 {
        match self {
            Self::Terminate => 15,
            Self::Kill => 9,
        }
    }
}

/// Cloneable input/control side of one running execution process.
#[async_trait]
pub trait ExecutionProcessInput: Send + Sync {
    async fn write_stdin(&self, data: &[u8]) -> ExecutionManagerResult<()>;

    async fn close_stdin(&self) -> ExecutionManagerResult<()>;

    async fn cancel(&self) -> ExecutionManagerResult<()>;

    async fn send_signal(&self, signal: ExecutionProcessSignal) -> ExecutionManagerResult<()> {
        match signal {
            ExecutionProcessSignal::Kill => self.cancel().await,
            ExecutionProcessSignal::Terminate => Err(ExecutionManagerError::InvalidRequest(
                "process transport does not support graceful termination".to_string(),
            )),
        }
    }

    async fn resize_pty(&self, cols: u16, rows: u16) -> ExecutionManagerResult<()> {
        let _ = (cols, rows);
        Err(ExecutionManagerError::InvalidRequest(
            "process does not have a PTY".to_string(),
        ))
    }
}

/// Event side of one running execution process.
#[async_trait]
pub trait ExecutionProcessStream: Send {
    fn input(&self) -> Arc<dyn ExecutionProcessInput>;

    async fn next_event(&mut self) -> ExecutionManagerResult<Option<ExecEvent>>;
}

pub type ExecutionProcess = Box<dyn ExecutionProcessStream>;

/// Generation-fenced process and filesystem access shared by compatibility
/// services and native SDK adapters.
///
/// Implementations must bind the underlying runtime endpoint before their
/// final generation check. A generation change may fail an operation, but it
/// must never redirect the operation to the replacement runtime.
#[async_trait]
pub trait ExecutionSessionManager: Send + Sync {
    async fn execute(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        request: ExecRequest,
    ) -> ExecutionManagerResult<ExecOutput>;

    async fn start_process(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        request: ExecRequest,
    ) -> ExecutionManagerResult<ExecutionProcess>;

    async fn start_pty(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        request: PtyRequest,
    ) -> ExecutionManagerResult<ExecutionProcess>;

    async fn transfer_file(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        request: FileRequest,
    ) -> ExecutionManagerResult<FileResponse>;

    async fn filesystem(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
        _request: FilesystemRequest,
    ) -> ExecutionManagerResult<FilesystemResponse> {
        Err(ExecutionManagerError::Unavailable(
            "this execution session does not support filesystem metadata operations".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::ExecutionProcessSignal;

    #[test]
    fn managed_process_signals_use_linux_guest_numbers() {
        assert_eq!(ExecutionProcessSignal::Terminate.linux_number(), 15);
        assert_eq!(ExecutionProcessSignal::Kill.linux_number(), 9);
    }
}
