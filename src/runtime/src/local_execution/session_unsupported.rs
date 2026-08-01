//! OCI SDK sessions plus fail-closed legacy handling on hosts without Unix sockets.

use a3s_box_core::pty::PtyRequest;
use a3s_box_core::{
    ExecOutput, ExecRequest, ExecutionGeneration, ExecutionId, ExecutionManagerError,
    ExecutionManagerResult, ExecutionProcess, ExecutionSessionManager, FileRequest, FileResponse,
};
use async_trait::async_trait;

use super::session_support::{
    debug_session_environment, has_oci_runtime, inherit_container_environment,
};
use super::LocalExecutionManager;

#[async_trait]
impl ExecutionSessionManager for LocalExecutionManager {
    async fn execute(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        mut request: ExecRequest,
    ) -> ExecutionManagerResult<ExecOutput> {
        request.streaming = false;
        let record = self
            .require_running_record(execution_id, generation)
            .await?;
        if !has_oci_runtime(&record) {
            return Err(unsupported_session("execute commands"));
        }
        inherit_container_environment(&record.env, &mut request.env);
        debug_session_environment(
            execution_id,
            generation,
            "execute",
            &record.env,
            &request.env,
        );
        self.require_same_runtime(&record, execution_id, generation)
            .await?;
        self.backend.execute(&record, request).await
    }

    async fn start_process(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        mut request: ExecRequest,
    ) -> ExecutionManagerResult<ExecutionProcess> {
        let record = self
            .require_running_record(execution_id, generation)
            .await?;
        if !has_oci_runtime(&record) {
            return Err(unsupported_session("start processes"));
        }
        inherit_container_environment(&record.env, &mut request.env);
        debug_session_environment(
            execution_id,
            generation,
            "start_process",
            &record.env,
            &request.env,
        );
        self.require_same_runtime(&record, execution_id, generation)
            .await?;
        self.backend.start_process(&record, request).await
    }

    async fn start_pty(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        mut request: PtyRequest,
    ) -> ExecutionManagerResult<ExecutionProcess> {
        let record = self
            .require_running_record(execution_id, generation)
            .await?;
        if !has_oci_runtime(&record) {
            return Err(unsupported_session("start PTYs"));
        }
        inherit_container_environment(&record.env, &mut request.env);
        debug_session_environment(
            execution_id,
            generation,
            "start_pty",
            &record.env,
            &request.env,
        );
        self.require_same_runtime(&record, execution_id, generation)
            .await?;
        self.backend.start_pty(&record, request).await
    }

    async fn transfer_file(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        _request: FileRequest,
    ) -> ExecutionManagerResult<FileResponse> {
        let record = self
            .require_running_record(execution_id, generation)
            .await?;
        if has_oci_runtime(&record) {
            return Err(ExecutionManagerError::Unavailable(format!(
                "execution {execution_id} uses the A3S OCI SDK, whose current public contract does not expose Box file sessions"
            )));
        }
        Err(unsupported_session("transfer files"))
    }
}

fn unsupported_session(operation: &str) -> ExecutionManagerError {
    ExecutionManagerError::Unavailable(format!(
        "cannot {operation}: legacy managed execution sessions require Unix-domain socket support"
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
