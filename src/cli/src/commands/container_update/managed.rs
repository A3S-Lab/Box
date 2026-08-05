//! Managed OCI live-resource update routing for `container-update`.

use a3s_box_core::{
    ExecutionGeneration, ExecutionId, ExecutionManager, ExecutionResourceUpdate, OperationId,
};
use a3s_box_runtime::{resize::ResourceUpdate, ManagedExecutionOperation, ManagedExecutionState};

/// Exact managed mutation selected from one durable OCI route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ManagedLiveUpdate {
    box_name: String,
    execution_id: ExecutionId,
    generation: ExecutionGeneration,
    operation_id: OperationId,
    update: ExecutionResourceUpdate,
}

/// Select the canonical managed path without allowing an OCI record to fall
/// back to a compatibility socket after an unavailable or interrupted call.
pub(super) fn resolve(
    record: &crate::state::BoxRecord,
    update: &ResourceUpdate,
) -> Result<Option<ManagedLiveUpdate>, String> {
    let Some(metadata) = record
        .managed_execution
        .as_ref()
        .filter(|metadata| metadata.is_oci_routed())
    else {
        return Ok(None);
    };
    let managed_update = to_execution_resource_update(update);
    let state = record
        .managed_state()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("box {} lost managed execution state", record.name))?;
    let operation_id = match state {
        ManagedExecutionState::Running => {
            if let Some(completed) = metadata.last_resource_update.as_ref().filter(|completed| {
                completed.generation == metadata.generation && completed.update == managed_update
            }) {
                completed.operation_id.clone()
            } else {
                OperationId::new(format!("cli-update-{}", uuid::Uuid::new_v4()))
                    .map_err(|error| error.to_string())?
            }
        }
        ManagedExecutionState::UpdatingResources => match metadata.pending_operation.as_ref() {
            Some(ManagedExecutionOperation::UpdateResources {
                operation_id,
                update: pending_update,
            }) if pending_update == &managed_update => operation_id.clone(),
            Some(ManagedExecutionOperation::UpdateResources { .. }) => {
                return Err(format!(
                    "box {} already has a different live resource update in progress; retry that update before submitting new limits",
                    record.name
                ));
            }
            _ => {
                return Err(format!(
                    "box {} has invalid managed resource-update recovery state",
                    record.name
                ));
            }
        },
        other => {
            return Err(format!(
                "cannot apply a live resource update to box {} while it is {other}; wait for the lifecycle operation to finish and retry",
                record.name
            ));
        }
    };

    Ok(Some(ManagedLiveUpdate {
        box_name: record.name.clone(),
        execution_id: ExecutionId::new(record.id.clone()).map_err(|error| error.to_string())?,
        generation: metadata.generation,
        operation_id,
        update: managed_update,
    }))
}

fn to_execution_resource_update(update: &ResourceUpdate) -> ExecutionResourceUpdate {
    ExecutionResourceUpdate {
        memory_reservation: update.limits.memory_reservation,
        memory_swap: update.limits.memory_swap,
        pids_limit: update.limits.pids_limit,
        cpu_shares: update.limits.cpu_shares,
        cpu_quota: update.limits.cpu_quota,
        cpu_period: update.limits.cpu_period,
        cpuset_cpus: update.limits.cpuset_cpus.clone(),
    }
}

/// Dispatch one exact-generation update through the canonical lifecycle
/// facade. The caller must not hold the per-execution lifecycle lock.
pub(super) async fn apply(
    manager: &dyn ExecutionManager,
    target: &ManagedLiveUpdate,
) -> Result<(), String> {
    manager
        .update_resources(
            &target.execution_id,
            target.generation,
            &target.operation_id,
            target.update.clone(),
        )
        .await
        .map(|_| ())
        .map_err(|error| {
            format!(
                "failed to apply managed live resource update to {}: {error}; no CLI policy changes were persisted and the exact-generation operation can be retried safely",
                target.box_name
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::config::{BoxConfig, ResourceLimits};
    use a3s_box_core::{CreateExecutionRequest, ExecutionLease};
    use a3s_box_runtime::ManagedExecutionMetadata;

    fn managed_oci_record(state: ManagedExecutionState) -> crate::state::BoxRecord {
        let mut record = crate::test_helpers::fixtures::make_record(
            "managed-update-id",
            "managed-update",
            state.as_status(),
            None,
        );
        let mut metadata = ManagedExecutionMetadata::new(
            OperationId::new("managed-update-create").unwrap(),
            ExecutionGeneration::new(7).unwrap(),
            CreateExecutionRequest {
                external_sandbox_id: "managed-update-external".to_string(),
                config: BoxConfig {
                    image: record.image.clone(),
                    ..Default::default()
                },
                labels: Default::default(),
                policy: Default::default(),
                rootfs_snapshot_id: None,
            },
        )
        .unwrap();
        metadata.runtime_route = a3s_box_runtime::ManagedRuntimeRoute::OciSdk;
        record.managed_execution = Some(metadata);
        record
    }

    fn cpu_share_update(value: u64) -> ResourceUpdate {
        ResourceUpdate {
            limits: ResourceLimits {
                cpu_shares: Some(value),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[derive(Clone)]
    struct RecordedUpdate {
        execution_id: ExecutionId,
        generation: ExecutionGeneration,
        operation_id: OperationId,
        update: ExecutionResourceUpdate,
    }

    struct RecordingExecutionManager {
        call: std::sync::Mutex<Option<RecordedUpdate>>,
        lease: ExecutionLease,
    }

    #[async_trait::async_trait]
    impl ExecutionManager for RecordingExecutionManager {
        async fn inspect(
            &self,
            _execution_id: &ExecutionId,
        ) -> a3s_box_core::ExecutionManagerResult<a3s_box_core::ExecutionStatus> {
            Err(a3s_box_core::ExecutionManagerError::Unavailable(
                "inspect is not used by this test".to_string(),
            ))
        }

        async fn update_resources(
            &self,
            execution_id: &ExecutionId,
            generation: ExecutionGeneration,
            operation_id: &OperationId,
            update: ExecutionResourceUpdate,
        ) -> a3s_box_core::ExecutionManagerResult<ExecutionLease> {
            *self.call.lock().unwrap() = Some(RecordedUpdate {
                execution_id: execution_id.clone(),
                generation,
                operation_id: operation_id.clone(),
                update,
            });
            Ok(self.lease.clone())
        }

        async fn pause(
            &self,
            _execution_id: &ExecutionId,
            _generation: ExecutionGeneration,
            _keep_memory: bool,
        ) -> a3s_box_core::ExecutionManagerResult<ExecutionLease> {
            Err(a3s_box_core::ExecutionManagerError::Unavailable(
                "pause is not used by this test".to_string(),
            ))
        }

        async fn resume(
            &self,
            _execution_id: &ExecutionId,
            _generation: ExecutionGeneration,
        ) -> a3s_box_core::ExecutionManagerResult<ExecutionLease> {
            Err(a3s_box_core::ExecutionManagerError::Unavailable(
                "resume is not used by this test".to_string(),
            ))
        }

        async fn kill(
            &self,
            _execution_id: &ExecutionId,
            _generation: ExecutionGeneration,
        ) -> a3s_box_core::ExecutionManagerResult<a3s_box_core::KillOutcome> {
            Err(a3s_box_core::ExecutionManagerError::Unavailable(
                "kill is not used by this test".to_string(),
            ))
        }

        async fn reconcile(
            &self,
            _operation_id: &OperationId,
        ) -> a3s_box_core::ExecutionManagerResult<a3s_box_core::ReconcileOutcome> {
            Err(a3s_box_core::ExecutionManagerError::Unavailable(
                "reconcile is not used by this test".to_string(),
            ))
        }
    }

    #[test]
    fn persisted_oci_route_selects_exact_generation_managed_update() {
        let record = managed_oci_record(ManagedExecutionState::Running);
        let target = resolve(&record, &cpu_share_update(512)).unwrap().unwrap();

        assert_eq!(target.execution_id.as_str(), record.id);
        assert_eq!(target.generation, ExecutionGeneration::new(7).unwrap());
        assert_eq!(target.update.cpu_shares, Some(512));
        assert!(target.operation_id.as_str().starts_with("cli-update-"));
    }

    #[tokio::test]
    async fn managed_update_dispatches_exact_identity_and_partial_intent() {
        let record = managed_oci_record(ManagedExecutionState::Running);
        let target = resolve(&record, &cpu_share_update(512)).unwrap().unwrap();
        let metadata = record.managed_execution.as_ref().unwrap();
        let manager = RecordingExecutionManager {
            call: std::sync::Mutex::new(None),
            lease: ExecutionLease {
                execution_id: target.execution_id.clone(),
                generation: target.generation,
                plan: metadata.plan.clone(),
                resources: metadata.request.config.resources.clone(),
                started_at: chrono::Utc::now(),
            },
        };

        apply(&manager, &target).await.unwrap();

        let call = manager.call.lock().unwrap().clone().unwrap();
        assert_eq!(call.execution_id, target.execution_id);
        assert_eq!(call.generation, target.generation);
        assert_eq!(call.operation_id, target.operation_id);
        assert_eq!(call.update, target.update);
    }

    #[test]
    fn unmanaged_record_keeps_legacy_live_update_route() {
        let record = crate::test_helpers::fixtures::make_record(
            "legacy-update-id",
            "legacy-update",
            "running",
            None,
        );

        assert!(resolve(&record, &cpu_share_update(512)).unwrap().is_none());
    }

    #[test]
    fn matching_pending_managed_update_reuses_operation_identity() {
        let mut record = managed_oci_record(ManagedExecutionState::UpdatingResources);
        let pending_update = to_execution_resource_update(&cpu_share_update(1024));
        let pending_id = OperationId::new("pending-update").unwrap();
        record.managed_execution.as_mut().unwrap().pending_operation =
            Some(ManagedExecutionOperation::UpdateResources {
                operation_id: pending_id.clone(),
                update: pending_update.clone(),
            });

        let target = resolve(&record, &cpu_share_update(1024)).unwrap().unwrap();

        assert_eq!(target.operation_id, pending_id);
        assert_eq!(target.update, pending_update);
    }

    #[test]
    fn different_pending_managed_update_fails_closed() {
        let mut record = managed_oci_record(ManagedExecutionState::UpdatingResources);
        record.managed_execution.as_mut().unwrap().pending_operation =
            Some(ManagedExecutionOperation::UpdateResources {
                operation_id: OperationId::new("pending-update").unwrap(),
                update: to_execution_resource_update(&cpu_share_update(512)),
            });

        let error = resolve(&record, &cpu_share_update(1024)).unwrap_err();

        assert!(error.contains("different live resource update in progress"));
        assert!(error.contains("retry that update"));
    }

    #[test]
    fn completed_managed_update_reuses_operation_identity() {
        let mut record = managed_oci_record(ManagedExecutionState::Running);
        let completed_update = to_execution_resource_update(&cpu_share_update(2048));
        let completed_id = OperationId::new("completed-update").unwrap();
        record
            .managed_execution
            .as_mut()
            .unwrap()
            .last_resource_update = Some(a3s_box_runtime::ManagedResourceUpdateCompletion {
            operation_id: completed_id.clone(),
            generation: ExecutionGeneration::new(7).unwrap(),
            update: completed_update,
        });

        let target = resolve(&record, &cpu_share_update(2048)).unwrap().unwrap();

        assert_eq!(target.operation_id, completed_id);
    }

    #[test]
    fn completed_update_from_another_generation_is_not_reused() {
        let mut record = managed_oci_record(ManagedExecutionState::Running);
        let completed_id = OperationId::new("stale-completed-update").unwrap();
        record
            .managed_execution
            .as_mut()
            .unwrap()
            .last_resource_update = Some(a3s_box_runtime::ManagedResourceUpdateCompletion {
            operation_id: completed_id.clone(),
            generation: ExecutionGeneration::new(6).unwrap(),
            update: to_execution_resource_update(&cpu_share_update(2048)),
        });

        let target = resolve(&record, &cpu_share_update(2048)).unwrap().unwrap();

        assert_ne!(target.operation_id, completed_id);
        assert!(target.operation_id.as_str().starts_with("cli-update-"));
    }

    #[test]
    fn conversion_preserves_every_tier_two_field() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                memory_reservation: Some(64),
                memory_swap: Some(-1),
                pids_limit: Some(32),
                cpu_shares: Some(512),
                cpu_quota: Some(20_000),
                cpu_period: Some(100_000),
                cpuset_cpus: Some("0-1".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(
            to_execution_resource_update(&update),
            ExecutionResourceUpdate {
                memory_reservation: Some(64),
                memory_swap: Some(-1),
                pids_limit: Some(32),
                cpu_shares: Some(512),
                cpu_quota: Some(20_000),
                cpu_period: Some(100_000),
                cpuset_cpus: Some("0-1".to_string()),
            }
        );
    }

    #[test]
    fn paused_managed_update_does_not_fall_back_to_legacy_transport() {
        let record = managed_oci_record(ManagedExecutionState::Paused);

        let error = resolve(&record, &cpu_share_update(512)).unwrap_err();

        assert!(error.contains("while it is paused"));
        assert!(error.contains("retry"));
    }
}
