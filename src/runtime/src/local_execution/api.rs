use a3s_box_core::{
    CreateExecutionRequest, ExecutionGeneration, ExecutionId, ExecutionLease, ExecutionManager,
    ExecutionManagerError, ExecutionManagerResult, ExecutionReservation, ExecutionSnapshot,
    ExecutionSnapshotId, ExecutionState, ExecutionStatus, KillOutcome, OperationId,
    ReconcileOutcome, RestartExecutionOptions,
};
use async_trait::async_trait;

use super::support::{managed_state, outcome_from_record, require_generation, state_conflict};
use super::{
    build_managed_record, status_from_record, LocalExecutionManager, ManagedExecutionState,
    RuntimeUpdate,
};

#[async_trait]
impl ExecutionManager for LocalExecutionManager {
    async fn create(
        &self,
        request: CreateExecutionRequest,
        operation_id: &OperationId,
    ) -> ExecutionManagerResult<ExecutionReservation> {
        let execution_id = ExecutionId::new(uuid::Uuid::new_v4().to_string())?;
        let record = build_managed_record(
            &self.home_dir,
            &execution_id,
            operation_id.clone(),
            request,
            chrono::Utc::now(),
        )?;
        let reservation = self.reserve(record).await?;
        super::record::reservation_from_record(reservation.record())
    }

    async fn start(
        &self,
        execution_id: &ExecutionId,
        expected_generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<ExecutionLease> {
        let _lifecycle_lock =
            super::lifecycle_lock::acquire(&self.home_dir, execution_id.as_str()).await?;
        let record = self
            .get(execution_id)
            .await?
            .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
        super::record::validate_record_health(&record)?;
        require_generation(&record, execution_id, expected_generation)?;
        self.ensure_started(record).await
    }

    async fn inspect(&self, execution_id: &ExecutionId) -> ExecutionManagerResult<ExecutionStatus> {
        let record = self
            .get(execution_id)
            .await?
            .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
        let record = self.stabilize_snapshot(record).await?;
        let (record, state) = self.observe_record(record).await?;
        status_from_record(&record, state)
    }

    async fn read_logs(
        &self,
        execution_id: &ExecutionId,
        expected_generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<Vec<a3s_box_core::log::LogEntry>> {
        self.read_structured_logs(execution_id, expected_generation)
            .await
    }

    async fn create_filesystem_snapshot(
        &self,
        execution_id: &ExecutionId,
        expected_generation: ExecutionGeneration,
        snapshot_id: &ExecutionSnapshotId,
    ) -> ExecutionManagerResult<ExecutionSnapshot> {
        let _lifecycle_lock =
            super::lifecycle_lock::acquire(&self.home_dir, execution_id.as_str()).await?;
        self.create_snapshot(execution_id, expected_generation, snapshot_id)
            .await
    }

    async fn filesystem_snapshot_size(
        &self,
        snapshot_id: &ExecutionSnapshotId,
    ) -> ExecutionManagerResult<Option<u64>> {
        self.snapshot_size(snapshot_id).await
    }

    async fn delete_filesystem_snapshot(
        &self,
        snapshot_id: &ExecutionSnapshotId,
    ) -> ExecutionManagerResult<bool> {
        self.delete_snapshot(snapshot_id).await
    }

    async fn pause(
        &self,
        execution_id: &ExecutionId,
        expected_generation: ExecutionGeneration,
        keep_memory: bool,
    ) -> ExecutionManagerResult<ExecutionLease> {
        let record = self
            .get(execution_id)
            .await?
            .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
        let record = self.stabilize_snapshot(record).await?;
        require_generation(&record, execution_id, expected_generation)?;
        if managed_state(&record)? != ManagedExecutionState::Running {
            return Err(state_conflict(&record, execution_id, "pause"));
        }
        let claimed = self
            .transition(
                &record,
                ManagedExecutionState::Running,
                ManagedExecutionState::Pausing,
                RuntimeUpdate::PauseClaim(keep_memory),
            )
            .await?;
        self.finish_pause(claimed).await
    }

    async fn resume(
        &self,
        execution_id: &ExecutionId,
        expected_generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<ExecutionLease> {
        let record = self
            .get(execution_id)
            .await?
            .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
        let record = self.stabilize_snapshot(record).await?;
        require_generation(&record, execution_id, expected_generation)?;
        if managed_state(&record)? != ManagedExecutionState::Paused {
            return Err(state_conflict(&record, execution_id, "resume"));
        }
        let claimed = self
            .transition(
                &record,
                ManagedExecutionState::Paused,
                ManagedExecutionState::Resuming,
                RuntimeUpdate::None,
            )
            .await?;
        self.finish_resume(claimed).await
    }

    async fn restart_with_options(
        &self,
        execution_id: &ExecutionId,
        expected_generation: ExecutionGeneration,
        operation_id: &OperationId,
        options: RestartExecutionOptions,
    ) -> ExecutionManagerResult<ExecutionLease> {
        let _lifecycle_lock =
            super::lifecycle_lock::acquire(&self.home_dir, execution_id.as_str()).await?;
        let record = self
            .get(execution_id)
            .await?
            .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
        super::record::validate_record_health(&record)?;
        self.restart_record(record, expected_generation, operation_id, options)
            .await
    }

    async fn kill(
        &self,
        execution_id: &ExecutionId,
        expected_generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<KillOutcome> {
        let record = self
            .get(execution_id)
            .await?
            .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
        let record = self.stabilize_snapshot(record).await?;
        require_generation(&record, execution_id, expected_generation)?;
        let state = managed_state(&record)?;
        if state.is_terminal() {
            return Ok(KillOutcome::AlreadyStopped);
        }
        if matches!(
            state,
            ManagedExecutionState::RestartStopping | ManagedExecutionState::RestartStarting
        ) {
            return Err(state_conflict(&record, execution_id, "kill"));
        }
        let claimed = if state == ManagedExecutionState::Killing {
            record
        } else {
            self.transition(
                &record,
                state,
                ManagedExecutionState::Killing,
                RuntimeUpdate::None,
            )
            .await?
        };
        self.finish_kill(claimed).await
    }

    async fn reconcile(
        &self,
        operation_id: &OperationId,
    ) -> ExecutionManagerResult<ReconcileOutcome> {
        let Some(initial_record) = self.get_by_operation(operation_id).await? else {
            return Ok(ReconcileOutcome::Absent);
        };
        let _lifecycle_lock =
            super::lifecycle_lock::acquire(&self.home_dir, &initial_record.id).await?;
        let Some(record) = self.get_by_operation(operation_id).await? else {
            return Ok(ReconcileOutcome::Absent);
        };
        if record.id != initial_record.id {
            return Err(ExecutionManagerError::Unavailable(format!(
                "operation {operation_id} changed execution identity while waiting for its lifecycle lock"
            )));
        }
        super::record::validate_record_health(&record)?;
        match managed_state(&record)? {
            ManagedExecutionState::Creating | ManagedExecutionState::Created => Ok(
                ReconcileOutcome::Created(super::record::reservation_from_record(&record)?),
            ),
            ManagedExecutionState::Starting => self.recover_start(record).await,
            ManagedExecutionState::Pausing => {
                let (record, state) = self.observe_record(record).await?;
                if managed_state(&record)? == ManagedExecutionState::Pausing
                    && state == ExecutionState::Running
                {
                    return self.finish_pause(record).await.map(ReconcileOutcome::Ready);
                }
                outcome_from_record(record, state)
            }
            ManagedExecutionState::Resuming => {
                let (record, state) = self.observe_record(record).await?;
                if managed_state(&record)? == ManagedExecutionState::Resuming
                    && state == ExecutionState::Paused
                {
                    return self
                        .finish_resume(record)
                        .await
                        .map(ReconcileOutcome::Ready);
                }
                outcome_from_record(record, state)
            }
            ManagedExecutionState::Snapshotting => self
                .recover_snapshot(record)
                .await
                .map(ReconcileOutcome::Ready),
            ManagedExecutionState::Killing => {
                self.finish_kill(record).await?;
                Ok(ReconcileOutcome::Failed)
            }
            ManagedExecutionState::Removing => {
                self.finish_remove(record).await?;
                Ok(ReconcileOutcome::Absent)
            }
            ManagedExecutionState::RestartStopping | ManagedExecutionState::RestartStarting => self
                .resume_restart(record)
                .await
                .map(ReconcileOutcome::Ready),
            _ => {
                let (record, state) = self.observe_record(record).await?;
                outcome_from_record(record, state)
            }
        }
    }
}
