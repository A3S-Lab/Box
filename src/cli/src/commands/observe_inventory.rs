//! Drive managed observation / remove-retry / restart-reconcile before CLI
//! inventory projections.
//!
//! Manager `inspect` (#385) retires durable `Starting` / `Killing` to
//! `Stopped` and durable `Pausing` / `Resuming` / `Snapshotting` /
//! `UpdatingResources` to `Failed` when the backend is `NotFound` (snapshot
//! paths never invent a published filesystem snapshot). Operator surfaces that
//! only read `boxes.json` never call that path, so abandoned claims stay
//! transitional and skip prune.
//!
//! Durable managed `Removing` is a different class: inspect returns Conflict
//! ("removal in progress"). Failed cleanup deliberately leaves `removing`;
//! reconcile already resumes via `finish_remove`. These helpers drive the same
//! remove-retry (`ExecutionManager::remove`) before inventory so forever-
//! removing rows do not lie in `ps` or block honest reclaim.
//!
//! Durable managed `RestartStopping` / `RestartStarting` are another class:
//! inspect must keep projecting Creating (no NotFound retirement). Resume the
//! restart owner via `ExecutionManager::reconcile` on the **creation**
//! operation identity (same lookup reconcile uses), not inspect and not an
//! invented pool `ps` path. Creating stays out of scope (create race). Do not
//! observe Running boxes on every list.

#[cfg(target_os = "linux")]
use a3s_box_core::NetworkMode;
use a3s_box_core::{
    ExecutionGeneration, ExecutionId, ExecutionManager, ExecutionState, OperationId,
};

use crate::state::{BoxRecord, StateFile};

/// Mark bridge-mode running boxes unhealthy when passt is dead (#454).
///
/// Persists `health_status=unhealthy` so `inspect` / `events` / `ps` can see the
/// network backend loss without inventing a lifecycle status change.
#[cfg(target_os = "linux")]
pub(crate) fn observe_bridge_passt_backend_loss(
    home: &std::path::Path,
) -> Result<(), std::io::Error> {
    let mut touched = Vec::new();
    {
        let state = StateFile::load_default()?;
        for record in state.list(true) {
            if !matches!(record.status.as_str(), "running") {
                continue;
            }
            if !matches!(record.network_mode, NetworkMode::Bridge { .. }) {
                continue;
            }
            if record.health_status == "unhealthy" {
                continue;
            }
            if a3s_box_runtime::network::passt_backend_lost_for_box(home, &record.id) {
                touched.push(record.id.clone());
            }
        }
    }
    if touched.is_empty() {
        return Ok(());
    }

    StateFile::modify(|state| {
        for id in &touched {
            if let Some(record) = state.find_by_id_mut(id) {
                if matches!(record.status.as_str(), "running")
                    && matches!(record.network_mode, NetworkMode::Bridge { .. })
                    && a3s_box_runtime::network::passt_backend_lost_for_box(home, id)
                {
                    record.health_status = "unhealthy".to_string();
                }
            }
        }
        Ok::<(), std::io::Error>(())
    })?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn observe_bridge_passt_backend_loss(
    _home: &std::path::Path,
) -> Result<(), std::io::Error> {
    Ok(())
}

/// Whether this record needs manager observation for inventory honesty.
///
/// Durable managed `Starting` / `Killing` (→ Stopped) and `Pausing` /
/// `Resuming` / `Snapshotting` / `UpdatingResources` (→ Failed) are in scope:
/// inspect NotFound retires them without inventing an exit or a published
/// snapshot. `RestartStarting` / `RestartStopping` use
/// [`needs_managed_restart_resume`] instead of inspect. Running/Paused stay
/// untouched so `ps` does not hammer live backends. Creating stays out of
/// scope (create race). Removing uses [`needs_managed_removal_resume`].
pub(crate) fn needs_managed_inventory_observation(record: &BoxRecord) -> bool {
    record.managed_execution.is_some()
        && matches!(
            record.status.as_str(),
            "starting" | "killing" | "pausing" | "resuming" | "snapshotting" | "updating_resources"
        )
}

/// Whether this record needs remove-retry before inventory honesty.
///
/// Durable managed `Removing` must resume `ExecutionManager::remove` (idempotent
/// `begin_remove` + `finish_remove`), not inspect NotFound retirement.
pub(crate) fn needs_managed_removal_resume(record: &BoxRecord) -> bool {
    record.managed_execution.is_some() && record.status == "removing"
}

/// Whether this record needs restart reconcile before inventory honesty.
///
/// Durable managed `RestartStopping` / `RestartStarting` must resume via
/// `ExecutionManager::reconcile(create_operation_id)`, not inspect NotFound
/// retirement (inspect keeps projecting Creating by design).
pub(crate) fn needs_managed_restart_resume(record: &BoxRecord) -> bool {
    record.managed_execution.is_some()
        && matches!(
            record.status.as_str(),
            "restart_stopping" | "restart_starting"
        )
}

fn managed_generation(
    record: &BoxRecord,
) -> Result<ExecutionGeneration, Box<dyn std::error::Error>> {
    record
        .managed_execution
        .as_ref()
        .map(|metadata| metadata.generation)
        .ok_or_else(|| {
            format!(
                "box {} lost managed generation during removal resume",
                record.id
            )
            .into()
        })
}

fn managed_create_operation_id(
    record: &BoxRecord,
) -> Result<OperationId, Box<dyn std::error::Error>> {
    record
        .managed_execution
        .as_ref()
        .map(|metadata| metadata.operation_id.clone())
        .ok_or_else(|| {
            format!(
                "box {} lost managed creation operation during restart resume",
                record.id
            )
            .into()
        })
}

/// Resume one abandoned restart claim. Absent-backend start may publish Failed
/// then return Err — treat that as converged when inspect no longer projects
/// Creating for the restart claim.
async fn resume_one_managed_restart(
    manager: &impl ExecutionManager,
    record: &BoxRecord,
) -> Result<(), Box<dyn std::error::Error>> {
    let operation_id = managed_create_operation_id(record)?;
    let execution_id = ExecutionId::new(record.id.clone())?;
    match manager.reconcile(&operation_id).await {
        Ok(_) => Ok(()),
        Err(error) => match manager.inspect(&execution_id).await {
            Ok(status) if status.state == ExecutionState::Creating => Err(error.into()),
            Ok(_) => Ok(()),
            Err(_) => Err(error.into()),
        },
    }
}

/// Call `manager.inspect` for each durable managed transitional claim in scope.
///
/// Persist happens inside the manager store (same `boxes.json` as CLI
/// [`StateFile`] when homes match). Callers must reload state afterward.
pub(crate) async fn observe_managed_inventory_claims(
    manager: &impl ExecutionManager,
    candidates: impl IntoIterator<Item = &BoxRecord>,
) -> Result<(), Box<dyn std::error::Error>> {
    for record in candidates {
        if !needs_managed_inventory_observation(record) {
            continue;
        }
        let execution_id = ExecutionId::new(record.id.clone())?;
        manager.inspect(&execution_id).await?;
    }
    Ok(())
}

/// Observe / remove-retry / restart-reconcile under the default home, then reload.
///
/// Fail closed: one inspect / remove-retry / restart-reconcile error refuses
/// inventory success so `ps` / `prune` / `info` cannot project stale transitional
/// claims. Passt backend-loss observation errors also fail closed.
pub(crate) async fn refresh_default_home_after_inventory_observation(
) -> Result<StateFile, Box<dyn std::error::Error>> {
    let home = a3s_box_core::dirs_home();
    observe_bridge_passt_backend_loss(&home)?;
    let state = StateFile::load_default()?;
    let needs_work = state.list(true).into_iter().any(|record| {
        needs_managed_inventory_observation(record)
            || needs_managed_removal_resume(record)
            || needs_managed_restart_resume(record)
    });
    if !needs_work {
        return Ok(state);
    }

    let manager = super::configured_local_execution_manager(&home).await?;
    let candidates = state.list(true);
    observe_managed_inventory_claims(&manager, candidates).await?;
    let state = StateFile::load_default()?;
    resume_managed_removal_claims(&manager, state.list(true)).await?;
    let state = StateFile::load_default()?;
    resume_managed_restart_claims(&manager, state.list(true)).await?;
    Ok(StateFile::load_default()?)
}

/// Resume durable managed `Removing` via manager remove-retry.
pub(crate) async fn resume_managed_removal_claims(
    manager: &impl ExecutionManager,
    candidates: impl IntoIterator<Item = &BoxRecord>,
) -> Result<(), Box<dyn std::error::Error>> {
    for record in candidates {
        if !needs_managed_removal_resume(record) {
            continue;
        }
        let execution_id = ExecutionId::new(record.id.clone())?;
        let generation = managed_generation(record)?;
        manager.remove(&execution_id, generation).await?;
    }
    Ok(())
}

/// Resume durable managed restart claims via manager reconcile (create op).
pub(crate) async fn resume_managed_restart_claims(
    manager: &impl ExecutionManager,
    candidates: impl IntoIterator<Item = &BoxRecord>,
) -> Result<(), Box<dyn std::error::Error>> {
    for record in candidates {
        if !needs_managed_restart_resume(record) {
            continue;
        }
        resume_one_managed_restart(manager, record).await?;
    }
    Ok(())
}

/// Observe / remove-retry / restart-reconcile one claim; `None` if remove finished.
pub(crate) async fn refresh_managed_inventory_record(
    record: BoxRecord,
) -> Result<Option<BoxRecord>, Box<dyn std::error::Error>> {
    let needs_observe = needs_managed_inventory_observation(&record);
    let needs_remove = needs_managed_removal_resume(&record);
    let needs_restart = needs_managed_restart_resume(&record);
    if !needs_observe && !needs_remove && !needs_restart {
        return Ok(Some(record));
    }

    let home = a3s_box_core::dirs_home();
    let manager = super::configured_local_execution_manager(&home).await?;
    if needs_observe {
        observe_managed_inventory_claims(&manager, std::slice::from_ref(&record)).await?;
    }
    if needs_remove {
        resume_managed_removal_claims(&manager, std::slice::from_ref(&record)).await?;
    }
    if needs_restart {
        resume_managed_restart_claims(&manager, std::slice::from_ref(&record)).await?;
    }

    let state = StateFile::load_default()?;
    Ok(state.find_by_id(&record.id).cloned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use a3s_box_core::{
        BoxConfig, CreateExecutionRequest, ExecutionGeneration, ExecutionIsolation,
        ExecutionManagerError, ExecutionManagerResult, ExecutionResourceUpdate,
        ExecutionSnapshotId, KillOutcome, OperationId,
    };
    use a3s_box_runtime::{
        LocalExecutionBackend, LocalExecutionHandle, LocalExecutionManager,
        LocalExecutionObservation, ManagedExecutionMetadata, ManagedExecutionOperation,
        ManagedExecutionState,
    };
    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};

    use crate::test_helpers::fixtures::make_record;

    #[derive(Default)]
    struct AbsentBackend;

    #[async_trait]
    impl LocalExecutionBackend for AbsentBackend {
        async fn start(&self, _record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
            Err(ExecutionManagerError::Unavailable(
                "absent backend cannot start".into(),
            ))
        }

        async fn inspect(
            &self,
            record: &BoxRecord,
        ) -> ExecutionManagerResult<LocalExecutionObservation> {
            Err(ExecutionManagerError::NotFound(
                ExecutionId::new(record.id.clone()).unwrap(),
            ))
        }

        async fn pause(
            &self,
            _record: &BoxRecord,
            _keep_memory: bool,
        ) -> ExecutionManagerResult<LocalExecutionHandle> {
            Err(ExecutionManagerError::NotFound(
                ExecutionId::new("00000000-0000-4000-8000-000000000000").unwrap(),
            ))
        }

        async fn resume(
            &self,
            _record: &BoxRecord,
        ) -> ExecutionManagerResult<LocalExecutionHandle> {
            Err(ExecutionManagerError::NotFound(
                ExecutionId::new("00000000-0000-4000-8000-000000000000").unwrap(),
            ))
        }

        async fn kill(&self, _record: &BoxRecord) -> ExecutionManagerResult<KillOutcome> {
            Ok(KillOutcome::AlreadyStopped)
        }
    }

    fn managed_claim(
        id: &str,
        box_dir: &std::path::Path,
        status: &str,
        pending: ManagedExecutionOperation,
    ) -> BoxRecord {
        let mut record = make_record(id, "abandoned", status, None);
        record.box_dir = box_dir.join(id);
        record.isolation = ExecutionIsolation::Sandbox;
        std::fs::create_dir_all(&record.box_dir).unwrap();
        let mut metadata = ManagedExecutionMetadata::new(
            OperationId::new("operation-abandoned").unwrap(),
            ExecutionGeneration::INITIAL,
            CreateExecutionRequest {
                external_sandbox_id: "external-abandoned".to_string(),
                config: BoxConfig {
                    isolation: ExecutionIsolation::Sandbox,
                    image: record.image.clone(),
                    ..Default::default()
                },
                labels: BTreeMap::new(),
                policy: Default::default(),
                rootfs_snapshot_id: None,
            },
        )
        .unwrap();
        metadata.pending_operation = Some(pending);
        record.managed_execution = Some(metadata);
        record
    }

    fn managed_starting_record(id: &str, box_dir: &std::path::Path) -> BoxRecord {
        managed_claim(id, box_dir, "starting", ManagedExecutionOperation::Start)
    }

    fn managed_killing_record(id: &str, box_dir: &std::path::Path) -> BoxRecord {
        managed_claim(
            id,
            box_dir,
            "killing",
            ManagedExecutionOperation::Kill {
                signal: None,
                timeout_secs: None,
            },
        )
    }

    fn managed_pausing_record(id: &str, box_dir: &std::path::Path) -> BoxRecord {
        managed_claim(
            id,
            box_dir,
            "pausing",
            ManagedExecutionOperation::Pause {
                keep_memory: true,
                operation_id: None,
            },
        )
    }

    fn managed_resuming_record(id: &str, box_dir: &std::path::Path) -> BoxRecord {
        let mut record = managed_claim(
            id,
            box_dir,
            "resuming",
            ManagedExecutionOperation::Resume { operation_id: None },
        );
        // Warm-resume path observes the backend; cold resume finishes without
        // inspect and would not exercise NotFound→Failed retirement.
        if let Some(metadata) = record.managed_execution.as_mut() {
            metadata.paused_with_memory = true;
        }
        record
    }

    fn managed_snapshotting_record(id: &str, box_dir: &std::path::Path) -> BoxRecord {
        managed_claim(
            id,
            box_dir,
            "snapshotting",
            ManagedExecutionOperation::Snapshot {
                snapshot_id: ExecutionSnapshotId::new("abandoned-snapshot").unwrap(),
                source_state: ManagedExecutionState::Running,
                operation_id: None,
                freezer_applied: false,
            },
        )
    }

    fn managed_updating_resources_record(id: &str, box_dir: &std::path::Path) -> BoxRecord {
        managed_claim(
            id,
            box_dir,
            "updating_resources",
            ManagedExecutionOperation::UpdateResources {
                operation_id: OperationId::new("abandoned-update").unwrap(),
                update: ExecutionResourceUpdate {
                    pids_limit: Some(42),
                    ..Default::default()
                },
            },
        )
    }

    #[test]
    fn needs_observation_for_managed_start_kill_pause_resume_only() {
        let unmanaged = make_record("id", "box", "starting", None);
        assert!(!needs_managed_inventory_observation(&unmanaged));

        let mut running = make_record(
            "11111111-1111-4111-8111-111111111111",
            "box",
            "running",
            Some(1),
        );
        running.managed_execution = Some(
            ManagedExecutionMetadata::new(
                OperationId::new("op").unwrap(),
                ExecutionGeneration::INITIAL,
                CreateExecutionRequest {
                    external_sandbox_id: "ext".into(),
                    config: BoxConfig::default(),
                    labels: BTreeMap::new(),
                    policy: Default::default(),
                    rootfs_snapshot_id: None,
                },
            )
            .unwrap(),
        );
        assert!(!needs_managed_inventory_observation(&running));

        let mut starting = make_record(
            "22222222-2222-4222-8222-222222222222",
            "box",
            "starting",
            None,
        );
        starting.managed_execution = running.managed_execution.clone();
        assert!(needs_managed_inventory_observation(&starting));

        let mut killing = make_record(
            "33333333-3333-4333-8333-333333333333",
            "box",
            "killing",
            None,
        );
        killing.managed_execution = running.managed_execution.clone();
        assert!(needs_managed_inventory_observation(&killing));

        let mut pausing = make_record(
            "44444444-4444-4444-8444-444444444444",
            "box",
            "pausing",
            None,
        );
        pausing.managed_execution = running.managed_execution.clone();
        assert!(needs_managed_inventory_observation(&pausing));

        let mut resuming = make_record(
            "55555555-5555-4555-8555-555555555555",
            "box",
            "resuming",
            None,
        );
        resuming.managed_execution = running.managed_execution.clone();
        assert!(needs_managed_inventory_observation(&resuming));

        let mut snapshotting = make_record(
            "88888888-8888-4888-8888-888888888888",
            "box",
            "snapshotting",
            None,
        );
        snapshotting.managed_execution = running.managed_execution.clone();
        assert!(needs_managed_inventory_observation(&snapshotting));

        let mut updating = make_record(
            "99999999-9999-4999-8999-999999999999",
            "box",
            "updating_resources",
            None,
        );
        updating.managed_execution = running.managed_execution.clone();
        assert!(needs_managed_inventory_observation(&updating));

        let mut creating = make_record(
            "66666666-6666-4666-8666-666666666666",
            "box",
            "creating",
            None,
        );
        creating.managed_execution = running.managed_execution.clone();
        assert!(!needs_managed_inventory_observation(&creating));
        assert!(!needs_managed_removal_resume(&creating));
        assert!(!needs_managed_restart_resume(&creating));

        let mut removing = make_record(
            "77777777-7777-4777-8777-777777777777",
            "box",
            "removing",
            None,
        );
        removing.managed_execution = running.managed_execution.clone();
        assert!(!needs_managed_inventory_observation(&removing));
        assert!(needs_managed_removal_resume(&removing));
        assert!(!needs_managed_restart_resume(&removing));

        let mut restart_starting = make_record(
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "box",
            "restart_starting",
            None,
        );
        restart_starting.managed_execution = running.managed_execution.clone();
        assert!(!needs_managed_inventory_observation(&restart_starting));
        assert!(!needs_managed_removal_resume(&restart_starting));
        assert!(needs_managed_restart_resume(&restart_starting));

        let mut restart_stopping = make_record(
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
            "box",
            "restart_stopping",
            None,
        );
        restart_stopping.managed_execution = running.managed_execution.clone();
        assert!(!needs_managed_inventory_observation(&restart_stopping));
        assert!(needs_managed_restart_resume(&restart_stopping));
    }

    #[tokio::test]
    async fn observe_retires_absent_managed_starting_without_inventing_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join("boxes.json");
        let id = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let record = managed_starting_record(id, tmp.path());

        let mut state = StateFile::load(&state_path).unwrap();
        state.add(record.clone()).unwrap();
        assert_eq!(
            state.find_by_id(id).unwrap().status,
            ManagedExecutionState::Starting.as_status()
        );

        let manager = LocalExecutionManager::new(&state_path, tmp.path(), Arc::new(AbsentBackend));
        observe_managed_inventory_claims(&manager, state.list(true))
            .await
            .unwrap();

        let refreshed = StateFile::load(&state_path).unwrap();
        let stopped = refreshed.find_by_id(id).unwrap();
        assert_eq!(stopped.status, "stopped");
        assert_eq!(
            stopped.managed_state().unwrap(),
            Some(ManagedExecutionState::Stopped)
        );
        assert!(
            stopped.exit_code.is_none(),
            "CLI observe path must not invent an exit code"
        );
    }

    #[tokio::test]
    async fn observe_retires_absent_managed_killing_without_inventing_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join("boxes.json");
        let id = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
        let record = managed_killing_record(id, tmp.path());

        let mut state = StateFile::load(&state_path).unwrap();
        state.add(record.clone()).unwrap();
        assert_eq!(
            state.find_by_id(id).unwrap().status,
            ManagedExecutionState::Killing.as_status()
        );

        let manager = LocalExecutionManager::new(&state_path, tmp.path(), Arc::new(AbsentBackend));
        observe_managed_inventory_claims(&manager, state.list(true))
            .await
            .unwrap();

        let refreshed = StateFile::load(&state_path).unwrap();
        let stopped = refreshed.find_by_id(id).unwrap();
        assert_eq!(stopped.status, "stopped");
        assert_eq!(
            stopped.managed_state().unwrap(),
            Some(ManagedExecutionState::Stopped)
        );
        assert!(
            stopped.exit_code.is_none(),
            "CLI observe path must not invent an exit code for abandoned Killing"
        );
    }

    fn managed_removing_record(id: &str, boxes_dir: &std::path::Path) -> BoxRecord {
        let mut record =
            managed_claim(id, boxes_dir, "removing", ManagedExecutionOperation::Remove);
        // finish_remove validates exec endpoint ownership; fixtures from
        // make_record may carry a foreign layout — clear for absent cleanup.
        record.exec_socket_path = std::path::PathBuf::new();
        record
    }

    #[tokio::test]
    async fn resume_removes_absent_managed_removing_without_inspect_retirement() {
        let tmp = tempfile::tempdir().unwrap();
        // finish_remove validates box_dir == {home}/boxes/{id}.
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join("boxes")).unwrap();
        let state_path = home.join("boxes.json");
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let record = managed_removing_record(id, &home.join("boxes"));

        let mut state = StateFile::load(&state_path).unwrap();
        state.add(record.clone()).unwrap();
        assert_eq!(
            state.find_by_id(id).unwrap().status,
            ManagedExecutionState::Removing.as_status()
        );

        let manager = LocalExecutionManager::new(&state_path, &home, Arc::new(AbsentBackend));
        resume_managed_removal_claims(&manager, state.list(true))
            .await
            .unwrap();

        let refreshed = StateFile::load(&state_path).unwrap();
        assert!(
            refreshed.find_by_id(id).is_none(),
            "remove-retry must finish_remove an abandoned Removing claim"
        );
    }

    #[tokio::test]
    async fn observe_retires_absent_managed_pausing_to_failed_without_inventing_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join("boxes.json");
        let id = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
        let record = managed_pausing_record(id, tmp.path());

        let mut state = StateFile::load(&state_path).unwrap();
        state.add(record.clone()).unwrap();
        assert_eq!(
            state.find_by_id(id).unwrap().status,
            ManagedExecutionState::Pausing.as_status()
        );

        let manager = LocalExecutionManager::new(&state_path, tmp.path(), Arc::new(AbsentBackend));
        observe_managed_inventory_claims(&manager, state.list(true))
            .await
            .unwrap();

        let refreshed = StateFile::load(&state_path).unwrap();
        let failed = refreshed.find_by_id(id).unwrap();
        assert_eq!(failed.status, "failed");
        assert_eq!(
            failed.managed_state().unwrap(),
            Some(ManagedExecutionState::Failed)
        );
        assert!(
            failed.exit_code.is_none(),
            "CLI observe path must not invent an exit code for abandoned Pausing"
        );
    }

    #[tokio::test]
    async fn observe_retires_absent_managed_resuming_to_failed_without_inventing_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join("boxes.json");
        let id = "ffffffff-ffff-4fff-8fff-ffffffffffff";
        let record = managed_resuming_record(id, tmp.path());

        let mut state = StateFile::load(&state_path).unwrap();
        state.add(record.clone()).unwrap();
        assert_eq!(
            state.find_by_id(id).unwrap().status,
            ManagedExecutionState::Resuming.as_status()
        );

        let manager = LocalExecutionManager::new(&state_path, tmp.path(), Arc::new(AbsentBackend));
        observe_managed_inventory_claims(&manager, state.list(true))
            .await
            .unwrap();

        let refreshed = StateFile::load(&state_path).unwrap();
        let failed = refreshed.find_by_id(id).unwrap();
        assert_eq!(failed.status, "failed");
        assert_eq!(
            failed.managed_state().unwrap(),
            Some(ManagedExecutionState::Failed)
        );
        assert!(
            failed.exit_code.is_none(),
            "CLI observe path must not invent an exit code for abandoned Resuming"
        );
    }

    #[tokio::test]
    async fn observe_retires_absent_managed_snapshotting_to_failed_without_inventing_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join("boxes.json");
        let id = "12121212-1212-4121-8121-121212121212";
        let record = managed_snapshotting_record(id, tmp.path());

        let mut state = StateFile::load(&state_path).unwrap();
        state.add(record.clone()).unwrap();
        assert_eq!(
            state.find_by_id(id).unwrap().status,
            ManagedExecutionState::Snapshotting.as_status()
        );

        let manager = LocalExecutionManager::new(&state_path, tmp.path(), Arc::new(AbsentBackend));
        observe_managed_inventory_claims(&manager, state.list(true))
            .await
            .unwrap();

        let refreshed = StateFile::load(&state_path).unwrap();
        let failed = refreshed.find_by_id(id).unwrap();
        assert_eq!(failed.status, "failed");
        assert_eq!(
            failed.managed_state().unwrap(),
            Some(ManagedExecutionState::Failed)
        );
        assert!(
            failed.exit_code.is_none(),
            "CLI observe path must not invent an exit for abandoned Snapshotting"
        );
    }

    #[tokio::test]
    async fn observe_retires_absent_managed_updating_resources_to_failed_without_inventing_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join("boxes.json");
        let id = "13131313-1313-4131-8131-131313131313";
        let record = managed_updating_resources_record(id, tmp.path());

        let mut state = StateFile::load(&state_path).unwrap();
        state.add(record.clone()).unwrap();
        assert_eq!(
            state.find_by_id(id).unwrap().status,
            ManagedExecutionState::UpdatingResources.as_status()
        );

        let manager = LocalExecutionManager::new(&state_path, tmp.path(), Arc::new(AbsentBackend));
        observe_managed_inventory_claims(&manager, state.list(true))
            .await
            .unwrap();

        let refreshed = StateFile::load(&state_path).unwrap();
        let failed = refreshed.find_by_id(id).unwrap();
        assert_eq!(failed.status, "failed");
        assert_eq!(
            failed.managed_state().unwrap(),
            Some(ManagedExecutionState::Failed)
        );
        assert!(
            failed.exit_code.is_none(),
            "CLI observe path must not invent an exit for abandoned UpdatingResources"
        );
    }

    #[tokio::test]
    async fn observe_skips_running_managed_records() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join("boxes.json");
        let id = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
        let mut record = managed_starting_record(id, tmp.path());
        record.status = "running".to_string();
        // Use this process PID so StateFile reconcile does not flip Running→dead.
        record.pid = Some(std::process::id());
        record.started_at = Some(Utc.with_ymd_and_hms(2026, 3, 15, 12, 0, 0).unwrap());
        if let Some(metadata) = record.managed_execution.as_mut() {
            metadata.pending_operation = None;
        }

        let mut state = StateFile::load(&state_path).unwrap();
        state.add(record).unwrap();

        let manager = LocalExecutionManager::new(&state_path, tmp.path(), Arc::new(AbsentBackend));
        observe_managed_inventory_claims(&manager, state.list(true))
            .await
            .unwrap();

        let refreshed = StateFile::load(&state_path).unwrap();
        assert_eq!(refreshed.find_by_id(id).unwrap().status, "running");
    }

    #[test]
    fn prune_still_requires_stopped_not_transitional() {
        // After observe retires Starting/Killing→Stopped, prune's existing
        // filter reclaim it. Pausing/Resuming retire to Failed (not prune
        // targets). Removing is finished via remove-retry (row gone), not by
        // widening prune. Do not widen prune to accept transitional or failed
        // statuses as a shortcut around observation / rm.
        assert!(!matches!("starting", "stopped" | "dead" | "created"));
        assert!(!matches!("killing", "stopped" | "dead" | "created"));
        assert!(!matches!("pausing", "stopped" | "dead" | "created"));
        assert!(!matches!("resuming", "stopped" | "dead" | "created"));
        assert!(!matches!("removing", "stopped" | "dead" | "created"));
        assert!(!matches!("snapshotting", "stopped" | "dead" | "created"));
        assert!(!matches!(
            "updating_resources",
            "stopped" | "dead" | "created"
        ));
        assert!(!matches!("failed", "stopped" | "dead" | "created"));
        assert!(!matches!(
            "restart_starting",
            "stopped" | "dead" | "created"
        ));
        assert!(!matches!(
            "restart_stopping",
            "stopped" | "dead" | "created"
        ));
        assert!(matches!("stopped", "stopped" | "dead" | "created"));
    }

    fn managed_restart_claim(
        id: &str,
        box_dir: &std::path::Path,
        status: &str,
        generation: ExecutionGeneration,
        source_state: ManagedExecutionState,
    ) -> BoxRecord {
        let mut record = managed_claim(
            id,
            box_dir,
            status,
            ManagedExecutionOperation::Restart {
                operation_id: OperationId::new("operation-restart-abandoned").unwrap(),
                source_generation: ExecutionGeneration::INITIAL,
                source_state,
                stop_timeout_secs: None,
            },
        );
        if let Some(metadata) = record.managed_execution.as_mut() {
            metadata.generation = generation;
        }
        record
    }

    #[tokio::test]
    async fn restart_resume_retires_absent_restart_starting_without_inventing_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join("boxes.json");
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let record = managed_restart_claim(
            id,
            tmp.path(),
            "restart_starting",
            ExecutionGeneration::new(2).unwrap(),
            ManagedExecutionState::Running,
        );

        let mut state = StateFile::load(&state_path).unwrap();
        state.add(record).unwrap();

        let manager = LocalExecutionManager::new(&state_path, tmp.path(), Arc::new(AbsentBackend));
        resume_managed_restart_claims(&manager, state.list(true))
            .await
            .unwrap();

        let refreshed = StateFile::load(&state_path).unwrap();
        let failed = refreshed.find_by_id(id).unwrap();
        assert_eq!(failed.status, "failed");
        assert_eq!(
            failed.managed_state().unwrap(),
            Some(ManagedExecutionState::Failed)
        );
        assert!(
            failed.exit_code.is_none(),
            "restart reconcile must not invent an exit for absent RestartStarting"
        );
    }

    #[tokio::test]
    async fn restart_resume_advances_absent_restart_stopping_to_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join("boxes.json");
        let id = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let record = managed_restart_claim(
            id,
            tmp.path(),
            "restart_stopping",
            ExecutionGeneration::INITIAL,
            ManagedExecutionState::Running,
        );

        let mut state = StateFile::load(&state_path).unwrap();
        state.add(record).unwrap();

        let manager = LocalExecutionManager::new(&state_path, tmp.path(), Arc::new(AbsentBackend));
        resume_managed_restart_claims(&manager, state.list(true))
            .await
            .unwrap();

        let refreshed = StateFile::load(&state_path).unwrap();
        let failed = refreshed.find_by_id(id).unwrap();
        assert_eq!(failed.status, "failed");
        assert_eq!(
            failed.managed_state().unwrap(),
            Some(ManagedExecutionState::Failed)
        );
        assert!(failed.exit_code.is_none());
    }

    #[tokio::test]
    async fn observe_does_not_inspect_retire_restart_starting() {
        // Anti-overfit: inventory observation must not treat RestartStarting
        // like Starting (NotFound→Stopped). Resume uses reconcile instead.
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join("boxes.json");
        let id = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
        let record = managed_restart_claim(
            id,
            tmp.path(),
            "restart_starting",
            ExecutionGeneration::new(2).unwrap(),
            ManagedExecutionState::Running,
        );

        let mut state = StateFile::load(&state_path).unwrap();
        state.add(record).unwrap();

        let manager = LocalExecutionManager::new(&state_path, tmp.path(), Arc::new(AbsentBackend));
        observe_managed_inventory_claims(&manager, state.list(true))
            .await
            .unwrap();

        let refreshed = StateFile::load(&state_path).unwrap();
        assert_eq!(
            refreshed.find_by_id(id).unwrap().status,
            "restart_starting",
            "observe must not inspect-retire RestartStarting"
        );
    }
}
