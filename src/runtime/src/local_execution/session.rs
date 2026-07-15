//! Generation-fenced command, PTY, and file sessions.

use std::sync::Arc;

use a3s_box_core::pty::PtyRequest;
use a3s_box_core::{
    BoxError, ExecEvent, ExecOutput, ExecRequest, ExecutionGeneration, ExecutionId,
    ExecutionManagerError, ExecutionManagerResult, ExecutionProcess, ExecutionProcessInput,
    ExecutionProcessStream, ExecutionSessionManager, FileRequest, FileResponse,
};
use async_trait::async_trait;

use super::LocalExecutionManager;
use crate::{
    BoxRecord, ExecClient, PtyClient, StreamingExec, StreamingExecInput, StreamingPty,
    StreamingPtyInput,
};

#[async_trait]
impl ExecutionSessionManager for LocalExecutionManager {
    async fn execute(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        mut request: ExecRequest,
    ) -> ExecutionManagerResult<ExecOutput> {
        request.streaming = false;
        let (client, stream) = self.bind_exec(execution_id, generation).await?;
        client
            .exec_command_on_stream(stream, &request)
            .await
            .map_err(|error| session_error(execution_id, "execute command", error))
    }

    async fn start_process(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        request: ExecRequest,
    ) -> ExecutionManagerResult<ExecutionProcess> {
        let (client, stream) = self.bind_exec(execution_id, generation).await?;
        let stream = client
            .exec_stream_on_stream(stream, &request)
            .await
            .map_err(|error| session_error(execution_id, "start command", error))?;
        let input: Arc<dyn ExecutionProcessInput> = Arc::new(ExecInput {
            execution_id: execution_id.clone(),
            input: stream.input(),
        });
        Ok(Box::new(ExecStream { stream, input }))
    }

    async fn start_pty(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        request: PtyRequest,
    ) -> ExecutionManagerResult<ExecutionProcess> {
        let record = self
            .require_running_record(execution_id, generation)
            .await?;
        let socket_path = record.exec_socket_path.with_file_name("pty.sock");
        let client = PtyClient::connect(&socket_path)
            .await
            .map_err(|error| session_error(execution_id, "connect PTY", error))?;
        self.require_same_runtime(&record, execution_id, generation)
            .await?;
        let stream = client
            .start_stream(&request)
            .await
            .map_err(|error| session_error(execution_id, "start PTY", error))?;
        let input: Arc<dyn ExecutionProcessInput> = Arc::new(PtyInput {
            execution_id: execution_id.clone(),
            input: stream.input(),
        });
        Ok(Box::new(PtyStream { stream, input }))
    }

    async fn transfer_file(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        request: FileRequest,
    ) -> ExecutionManagerResult<FileResponse> {
        let (client, stream) = self.bind_exec(execution_id, generation).await?;
        client
            .file_transfer_on_stream(stream, &request)
            .await
            .map_err(|error| session_error(execution_id, "transfer file", error))
    }
}

impl LocalExecutionManager {
    async fn bind_exec(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<(ExecClient, tokio::net::UnixStream)> {
        let record = self
            .require_running_record(execution_id, generation)
            .await?;
        let client = ExecClient::for_socket(&record.exec_socket_path);
        let stream = client
            .open_stream()
            .await
            .map_err(|error| session_error(execution_id, "connect exec", error))?;
        self.require_same_runtime(&record, execution_id, generation)
            .await?;
        Ok((client, stream))
    }

    async fn require_same_runtime(
        &self,
        bound: &BoxRecord,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<()> {
        let current = self
            .require_running_record(execution_id, generation)
            .await?;
        if current.pid != bound.pid
            || current.pid_start_time != bound.pid_start_time
            || current.exec_socket_path != bound.exec_socket_path
        {
            return Err(ExecutionManagerError::Conflict {
                execution_id: execution_id.clone(),
                message: "runtime generation changed while binding its execution session"
                    .to_string(),
            });
        }
        Ok(())
    }
}

struct ExecInput {
    execution_id: ExecutionId,
    input: StreamingExecInput,
}

#[async_trait]
impl ExecutionProcessInput for ExecInput {
    async fn write_stdin(&self, data: &[u8]) -> ExecutionManagerResult<()> {
        self.input
            .write_stdin(data)
            .await
            .map_err(|error| session_error(&self.execution_id, "write command stdin", error))
    }

    async fn close_stdin(&self) -> ExecutionManagerResult<()> {
        self.input
            .close_stdin()
            .await
            .map_err(|error| session_error(&self.execution_id, "close command stdin", error))
    }

    async fn cancel(&self) -> ExecutionManagerResult<()> {
        self.input
            .cancel()
            .await
            .map_err(|error| session_error(&self.execution_id, "cancel command", error))
    }
}

struct ExecStream {
    stream: StreamingExec,
    input: Arc<dyn ExecutionProcessInput>,
}

#[async_trait]
impl ExecutionProcessStream for ExecStream {
    fn input(&self) -> Arc<dyn ExecutionProcessInput> {
        self.input.clone()
    }

    async fn next_event(&mut self) -> ExecutionManagerResult<Option<ExecEvent>> {
        self.stream
            .next_event()
            .await
            .map_err(|error| ExecutionManagerError::Unavailable(error.to_string()))
    }
}

struct PtyInput {
    execution_id: ExecutionId,
    input: StreamingPtyInput,
}

#[async_trait]
impl ExecutionProcessInput for PtyInput {
    async fn write_stdin(&self, data: &[u8]) -> ExecutionManagerResult<()> {
        self.input
            .write_stdin(data)
            .await
            .map_err(|error| session_error(&self.execution_id, "write PTY stdin", error))
    }

    async fn close_stdin(&self) -> ExecutionManagerResult<()> {
        self.cancel().await
    }

    async fn cancel(&self) -> ExecutionManagerResult<()> {
        self.input
            .close()
            .await
            .map_err(|error| session_error(&self.execution_id, "close PTY", error))
    }

    async fn resize_pty(&self, cols: u16, rows: u16) -> ExecutionManagerResult<()> {
        self.input
            .resize(cols, rows)
            .await
            .map_err(|error| session_error(&self.execution_id, "resize PTY", error))
    }
}

struct PtyStream {
    stream: StreamingPty,
    input: Arc<dyn ExecutionProcessInput>,
}

#[async_trait]
impl ExecutionProcessStream for PtyStream {
    fn input(&self) -> Arc<dyn ExecutionProcessInput> {
        self.input.clone()
    }

    async fn next_event(&mut self) -> ExecutionManagerResult<Option<ExecEvent>> {
        self.stream
            .next_event()
            .await
            .map_err(|error| ExecutionManagerError::Unavailable(error.to_string()))
    }
}

fn session_error(
    execution_id: &ExecutionId,
    operation: &str,
    error: BoxError,
) -> ExecutionManagerError {
    ExecutionManagerError::Unavailable(format!(
        "failed to {operation} for execution {execution_id}: {error}"
    ))
}
