impl A3sBoxClient {
    /// Persist an unstarted execution through the canonical lifecycle facade.
    pub async fn create_box(
        &self,
        request: CreateExecutionRequest,
        operation_id: &OperationId,
    ) -> Result<ExecutionReservation> {
        Ok(self.execution_manager.create(request, operation_id).await?)
    }

    /// Start a previously created execution with generation fencing.
    pub async fn start_box(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> Result<ExecutionLease> {
        Ok(self
            .execution_manager
            .start(execution_id, generation)
            .await?)
    }

    /// Atomically create or recover and then start an execution.
    pub async fn run_box(
        &self,
        request: CreateExecutionRequest,
        operation_id: &OperationId,
    ) -> Result<ExecutionLease> {
        Ok(self
            .execution_manager
            .create_and_start(request, operation_id)
            .await?)
    }

    /// Inspect the generation-fenced state of a managed execution.
    pub async fn inspect_execution(&self, execution_id: &ExecutionId) -> Result<ExecutionStatus> {
        Ok(self.execution_manager.inspect(execution_id).await?)
    }

    /// Pause a managed execution through its resolved backend.
    pub async fn pause_execution(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        keep_memory: bool,
    ) -> Result<ExecutionLease> {
        Ok(self
            .execution_manager
            .pause(execution_id, generation, keep_memory)
            .await?)
    }

    /// Resume a managed execution through its resolved backend.
    pub async fn resume_execution(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> Result<ExecutionLease> {
        Ok(self
            .execution_manager
            .resume(execution_id, generation)
            .await?)
    }

    /// Restart a managed execution under an idempotent operation identity.
    pub async fn restart_execution(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        operation_id: &OperationId,
        options: RestartExecutionOptions,
    ) -> Result<ExecutionLease> {
        Ok(self
            .execution_manager
            .restart_with_options(execution_id, generation, operation_id, options)
            .await?)
    }

    /// Kill a managed execution and release runtime-owned resources.
    pub async fn kill_execution(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> Result<KillOutcome> {
        Ok(self
            .execution_manager
            .kill(execution_id, generation)
            .await?)
    }

    /// Remove one terminal local execution and all runtime-owned paths.
    pub(crate) async fn remove_execution(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> Result<bool> {
        let manager = self.local_execution_manager.as_ref().ok_or_else(|| {
            ClientError::Validation(
                "this client was constructed without a local execution manager".to_string(),
            )
        })?;
        Ok(manager.remove_execution(execution_id, generation).await?)
    }

    /// Reconcile one idempotent create operation after caller or service restart.
    pub async fn reconcile_operation(
        &self,
        operation_id: &OperationId,
    ) -> Result<ReconcileOutcome> {
        Ok(self.execution_manager.reconcile(operation_id).await?)
    }
}
