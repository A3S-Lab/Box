//! Fail-closed session adapter for hosts without Unix-domain socket support.

use a3s_box_core::pty::PtyRequest;
use a3s_box_core::{
    ExecOutput, ExecRequest, ExecutionGeneration, ExecutionId, ExecutionManagerError,
    ExecutionManagerResult, ExecutionProcess, ExecutionSessionManager, FileRequest, FileResponse,
};
use async_trait::async_trait;

use super::LocalExecutionManager;

#[async_trait]
impl ExecutionSessionManager for LocalExecutionManager {
    async fn execute(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
        _request: ExecRequest,
    ) -> ExecutionManagerResult<ExecOutput> {
        Err(unsupported_session("execute commands"))
    }

    async fn start_process(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
        _request: ExecRequest,
    ) -> ExecutionManagerResult<ExecutionProcess> {
        Err(unsupported_session("start processes"))
    }

    async fn start_pty(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
        _request: PtyRequest,
    ) -> ExecutionManagerResult<ExecutionProcess> {
        Err(unsupported_session("start PTYs"))
    }

    async fn transfer_file(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
        _request: FileRequest,
    ) -> ExecutionManagerResult<FileResponse> {
        Err(unsupported_session("transfer files"))
    }
}

fn unsupported_session(operation: &str) -> ExecutionManagerError {
    ExecutionManagerError::Unavailable(format!(
        "cannot {operation}: managed execution sessions require Unix-domain socket support"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_session_error_is_explicit_and_fail_closed() {
        let error = unsupported_session("execute commands");
        assert!(matches!(error, ExecutionManagerError::Unavailable(_)));
        assert!(error.to_string().contains("Unix-domain socket support"));
    }

    #[test]
    fn local_manager_still_satisfies_the_backend_neutral_contract() {
        fn assert_session_manager<T: ExecutionSessionManager>() {}
        assert_session_manager::<LocalExecutionManager>();
    }
}
