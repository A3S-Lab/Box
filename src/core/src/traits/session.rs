//! Backend-neutral command, PTY, and file access for managed executions.

use std::sync::Arc;

use async_trait::async_trait;

use crate::exec::{ExecEvent, ExecOutput, ExecRequest, FileRequest, FileResponse};
use crate::pty::PtyRequest;

use super::execution::{
    ExecutionGeneration, ExecutionId, ExecutionManagerError, ExecutionManagerResult,
};

/// Cloneable input/control side of one running execution process.
#[async_trait]
pub trait ExecutionProcessInput: Send + Sync {
    async fn write_stdin(&self, data: &[u8]) -> ExecutionManagerResult<()>;

    async fn close_stdin(&self) -> ExecutionManagerResult<()>;

    async fn cancel(&self) -> ExecutionManagerResult<()>;

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
}
