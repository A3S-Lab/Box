use a3s_box_core::{
    CreateExecutionRequest, ExecutionEventBatch, ExecutionEventsRequest, ExecutionGeneration,
    ExecutionId, ExecutionLease, ExecutionManager, ExecutionManagerError, ExecutionManagerResult,
    ExecutionProcessInventory, ExecutionReservation, ExecutionResourceUpdate, ExecutionSnapshot,
    ExecutionSnapshotId, ExecutionState, ExecutionStats, ExecutionStatus, KillExecutionOptions,
    KillOutcome, OperationId, ReconcileOutcome, RestartExecutionOptions,
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
        self.backend.preflight(&record).await?;
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
        let lifecycle_lock =
            super::lifecycle_lock::acquire(&self.home_dir, execution_id.as_str()).await?;
        let manager = self.clone();
        let execution_id = execution_id.clone();
        let execution_id_label = execution_id.clone();
        tokio::spawn(async move {
            // Inspection can reconcile provider state, release resources, and
            // persist a terminal observation. Once its lifecycle lock has been
            // acquired, finish that projection even if the caller is cancelled;
            // a replay must wait for this same authoritative operation.
            let _lifecycle_lock = lifecycle_lock;
            let record = manager
                .get(&execution_id)
                .await?
                .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
            let record = manager.stabilize_snapshot(record).await?;
            let (record, state) = manager.observe_record(record).await?;
            status_from_record(&record, state)
        })
        .await
        .map_err(|error| {
            ExecutionManagerError::Internal(format!(
                "inspection task failed for {execution_id_label}: {error}"
            ))
        })?
    }

    async fn read_logs(
        &self,
        execution_id: &ExecutionId,
        expected_generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<Vec<a3s_box_core::log::LogEntry>> {
        self.read_structured_logs(execution_id, expected_generation)
            .await
    }

    async fn list_processes(
        &self,
        execution_id: &ExecutionId,
        expected_generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<ExecutionProcessInventory> {
        let record = self
            .require_running_record(execution_id, expected_generation)
            .await?;
        self.require_same_runtime(&record, execution_id, expected_generation)
            .await?;
        let inventory = self.backend.list_processes(&record).await?;
        self.require_same_runtime(&record, execution_id, expected_generation)
            .await?;
        if inventory.execution_id != *execution_id || inventory.generation != expected_generation {
            return Err(ExecutionManagerError::Internal(format!(
                "backend returned process inventory for a different execution generation than {execution_id} generation {}",
                expected_generation.get()
            )));
        }
        inventory.validate()?;
        Ok(inventory)
    }

    async fn stats(
        &self,
        execution_id: &ExecutionId,
        expected_generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<ExecutionStats> {
        let record = self
            .require_running_record(execution_id, expected_generation)
            .await?;
        self.require_same_runtime(&record, execution_id, expected_generation)
            .await?;
        let stats = self.backend.stats(&record).await?;
        self.require_same_runtime(&record, execution_id, expected_generation)
            .await?;
        if stats.execution_id != *execution_id || stats.generation != expected_generation {
            return Err(ExecutionManagerError::Internal(format!(
                "backend returned stats for a different execution generation than {execution_id} generation {}",
                expected_generation.get()
            )));
        }
        stats.validate()?;
        Ok(stats)
    }

    async fn events(
        &self,
        execution_id: &ExecutionId,
        expected_generation: ExecutionGeneration,
        request: ExecutionEventsRequest,
    ) -> ExecutionManagerResult<ExecutionEventBatch> {
        request.validate()?;
        let after_sequence = request.after_sequence;
        let record = self
            .require_running_record(execution_id, expected_generation)
            .await?;
        self.require_same_runtime(&record, execution_id, expected_generation)
            .await?;
        let batch = self.backend.events(&record, request).await?;
        self.require_same_runtime(&record, execution_id, expected_generation)
            .await?;
        if batch.execution_id != *execution_id || batch.generation != expected_generation {
            return Err(ExecutionManagerError::Internal(format!(
                "backend returned events for a different execution generation than {execution_id} generation {}",
                expected_generation.get()
            )));
        }
        batch.validate_after(after_sequence)?;
        Ok(batch)
    }

    async fn update_resources(
        &self,
        execution_id: &ExecutionId,
        expected_generation: ExecutionGeneration,
        operation_id: &OperationId,
        update: ExecutionResourceUpdate,
    ) -> ExecutionManagerResult<ExecutionLease> {
        update.validate()?;
        let _lifecycle_lock =
            super::lifecycle_lock::acquire(&self.home_dir, execution_id.as_str()).await?;
        let record = self
            .get(execution_id)
            .await?
            .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
        let record = self.stabilize_snapshot(record).await?;
        require_generation(&record, execution_id, expected_generation)?;
        let state = managed_state(&record)?;
        if let Some(completed) = record
            .managed_execution
            .as_ref()
            .and_then(|metadata| metadata.last_resource_update.as_ref())
            .filter(|completed| completed.operation_id == *operation_id)
        {
            if completed.generation != expected_generation || completed.update != update {
                return Err(ExecutionManagerError::Conflict {
                    execution_id: execution_id.clone(),
                    message: format!(
                        "resource update operation {operation_id} was already completed with different intent"
                    ),
                });
            }
            if state != ManagedExecutionState::Running {
                return Err(state_conflict(
                    &record,
                    execution_id,
                    "replay resource update",
                ));
            }
            return super::record::lease_from_record(&record);
        }
        if state == ManagedExecutionState::UpdatingResources {
            let pending = record
                .managed_execution
                .as_ref()
                .and_then(|metadata| metadata.pending_operation.as_ref());
            if !matches!(
                pending,
                Some(crate::ManagedExecutionOperation::UpdateResources {
                    operation_id: pending_id,
                    update: pending_update,
                }) if pending_id == operation_id && pending_update == &update
            ) {
                return Err(ExecutionManagerError::Conflict {
                    execution_id: execution_id.clone(),
                    message: "another resource update is already in progress".to_string(),
                });
            }
            return self.finish_resource_update(record).await;
        }
        if state != ManagedExecutionState::Running {
            return Err(state_conflict(&record, execution_id, "update resources"));
        }
        self.backend
            .preflight_resource_update(&record, &update)
            .await?;
        let claimed = self
            .transition(
                &record,
                ManagedExecutionState::Running,
                ManagedExecutionState::UpdatingResources,
                RuntimeUpdate::ResourceUpdateClaim {
                    operation_id: operation_id.clone(),
                    update,
                },
            )
            .await?;
        self.finish_resource_update(claimed).await
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
        let _lifecycle_lock =
            super::lifecycle_lock::acquire(&self.home_dir, execution_id.as_str()).await?;
        let record = self
            .get(execution_id)
            .await?
            .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
        let record = self.stabilize_snapshot(record).await?;
        require_generation(&record, execution_id, expected_generation)?;
        if managed_state(&record)? != ManagedExecutionState::Running {
            return Err(state_conflict(&record, execution_id, "pause"));
        }
        let backend_operation_id =
            OperationId::new(format!("managed-pause-{}", uuid::Uuid::new_v4().simple()))?;
        let claimed = self
            .transition(
                &record,
                ManagedExecutionState::Running,
                ManagedExecutionState::Pausing,
                RuntimeUpdate::PauseClaim {
                    keep_memory,
                    operation_id: backend_operation_id,
                },
            )
            .await?;
        self.finish_pause(claimed).await
    }

    async fn resume(
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
        let record = self.stabilize_snapshot(record).await?;
        require_generation(&record, execution_id, expected_generation)?;
        if managed_state(&record)? != ManagedExecutionState::Paused {
            return Err(state_conflict(&record, execution_id, "resume"));
        }
        let backend_operation_id =
            OperationId::new(format!("managed-resume-{}", uuid::Uuid::new_v4().simple()))?;
        let claimed = self
            .transition(
                &record,
                ManagedExecutionState::Paused,
                ManagedExecutionState::Resuming,
                RuntimeUpdate::ResumeClaim(backend_operation_id),
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
        self.kill_with_options(
            execution_id,
            expected_generation,
            KillExecutionOptions::default(),
        )
        .await
    }

    async fn kill_with_options(
        &self,
        execution_id: &ExecutionId,
        expected_generation: ExecutionGeneration,
        options: KillExecutionOptions,
    ) -> ExecutionManagerResult<KillOutcome> {
        validate_kill_options(options)?;
        let _lifecycle_lock =
            super::lifecycle_lock::acquire(&self.home_dir, execution_id.as_str()).await?;
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
                RuntimeUpdate::KillClaim(options),
            )
            .await?
        };
        self.finish_kill(claimed).await
    }

    async fn remove(
        &self,
        execution_id: &ExecutionId,
        expected_generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<bool> {
        self.remove_execution(execution_id, expected_generation)
            .await
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
            ManagedExecutionState::UpdatingResources => {
                let execution_id = super::record::execution_id(&record)?;
                match self.finish_resource_update(record).await {
                    Ok(lease) => Ok(ReconcileOutcome::Ready(lease)),
                    Err(error) => {
                        let current = self
                            .get(&execution_id)
                            .await?
                            .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
                        match managed_state(&current)? {
                            ManagedExecutionState::Stopped | ManagedExecutionState::Failed => {
                                Ok(ReconcileOutcome::Failed)
                            }
                            _ => Err(error),
                        }
                    }
                }
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

fn validate_kill_options(options: KillExecutionOptions) -> ExecutionManagerResult<()> {
    if options
        .signal
        .is_some_and(|signal| signal <= 0 || 128_i32.checked_add(signal).is_none())
    {
        return Err(ExecutionManagerError::InvalidRequest(
            "kill signal must be positive and representable as a Box exit code".to_string(),
        ));
    }
    if options
        .timeout_secs
        .is_some_and(|timeout| timeout.checked_mul(1_000).is_none())
    {
        return Err(ExecutionManagerError::InvalidRequest(
            "kill timeout is too large".to_string(),
        ));
    }
    Ok(())
}
