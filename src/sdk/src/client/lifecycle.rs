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

    /// Atomically capture one running or paused execution filesystem.
    ///
    /// The runtime temporarily quiesces a running execution, publishes the
    /// snapshot under the validated identifier, and restores the prior stable
    /// state before returning.
    pub async fn create_execution_snapshot(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        snapshot_id: &ExecutionSnapshotId,
    ) -> Result<ExecutionSnapshot> {
        Ok(self
            .execution_manager
            .create_filesystem_snapshot(execution_id, generation, snapshot_id)
            .await?)
    }

    /// Return the published size of a runtime-managed filesystem snapshot.
    pub async fn execution_snapshot_size(
        &self,
        snapshot_id: &ExecutionSnapshotId,
    ) -> Result<Option<u64>> {
        Ok(self
            .execution_manager
            .filesystem_snapshot_size(snapshot_id)
            .await?)
    }

    /// Delete a runtime-managed filesystem snapshot.
    ///
    /// The runtime refuses deletion while a live execution still uses the
    /// snapshot as its immutable copy-on-write lower.
    pub async fn delete_execution_snapshot(
        &self,
        snapshot_id: &ExecutionSnapshotId,
    ) -> Result<bool> {
        Ok(self
            .execution_manager
            .delete_filesystem_snapshot(snapshot_id)
            .await?)
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
    pub(crate) async fn remove_local_execution_if_present(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> Result<bool> {
        match self.local_execution_manager.as_ref() {
            Some(manager) => Ok(manager.remove_execution(execution_id, generation).await?),
            None => Ok(false),
        }
    }

    /// Reconcile one idempotent create operation after caller or service restart.
    pub async fn reconcile_operation(
        &self,
        operation_id: &OperationId,
    ) -> Result<ReconcileOutcome> {
        Ok(self.execution_manager.reconcile(operation_id).await?)
    }
}
