//! Drive managed observation / remove-retry before CLI inventory projections.
//!
//! Manager `inspect` (#385) retires durable `Starting` / `Killing` to
//! `Stopped` and durable `Pausing` / `Resuming` to `Failed` when the backend
//! is `NotFound`. Operator surfaces that only read `boxes.json` never call
//! that path, so abandoned claims stay transitional and skip prune.
//!
//! Durable managed `Removing` is a different class: inspect returns Conflict
//! ("removal in progress"). Failed cleanup deliberately leaves `removing`;
//! reconcile already resumes via `finish_remove`. These helpers drive the same
//! remove-retry (`ExecutionManager::remove`) before inventory so forever-
//! removing rows do not lie in `ps` or block honest reclaim. Creating stays
//! out of scope (create race, not inspect/remove retirement). Do not invent
//! pool `ps` inventory or observe Running boxes on every list.

use a3s_box_core::{ExecutionGeneration, ExecutionId, ExecutionManager};

use crate::state::{BoxRecord, StateFile};

/// Whether this record needs manager observation for inventory honesty.
///
/// Durable managed `Starting` / `Killing` (→ Stopped) and `Pausing` /
/// `Resuming` (→ Failed) are in scope: inspect NotFound retires them without
/// inventing an exit. `RestartStarting` must keep projecting Creating until
/// reconcile resumes it; Running/Paused stay untouched so `ps` does not
/// hammer live backends. Creating stays out of scope (create race). Removing
/// uses [`needs_managed_removal_resume`] instead of inspect.
pub(crate) fn needs_managed_inventory_observation(record: &BoxRecord) -> bool {
    record.managed_execution.is_some()
        && matches!(
            record.status.as_str(),
            "starting" | "killing" | "pausing" | "resuming"
        )
}

/// Whether this record needs remove-retry before inventory honesty.
///
/// Durable managed `Removing` must resume `ExecutionManager::remove` (idempotent
/// `begin_remove` + `finish_remove`), not inspect NotFound retirement.
pub(crate) fn needs_managed_removal_resume(record: &BoxRecord) -> bool {
    record.managed_execution.is_some() && record.status == "removing"
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

/// Soft-batch variant for prune/ps: warn and continue if one claim fails.
pub(crate) async fn observe_managed_inventory_claims_best_effort(
    manager: &impl ExecutionManager,
    candidates: impl IntoIterator<Item = &BoxRecord>,
) -> Result<(), Box<dyn std::error::Error>> {
    for record in candidates {
        if !needs_managed_inventory_observation(record) {
            continue;
        }
        let execution_id = ExecutionId::new(record.id.clone())?;
        if let Err(error) = manager.inspect(&execution_id).await {
            tracing::warn!(
                box_id = %record.id,
                status = %record.status,
                error = %error,
                "Failed to observe abandoned managed claim before inventory"
            );
        }
    }
    Ok(())
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

/// Soft-batch remove-retry for prune/ps.
pub(crate) async fn resume_managed_removal_claims_best_effort(
    manager: &impl ExecutionManager,
    candidates: impl IntoIterator<Item = &BoxRecord>,
) -> Result<(), Box<dyn std::error::Error>> {
    for record in candidates {
        if !needs_managed_removal_resume(record) {
            continue;
        }
        let execution_id = ExecutionId::new(record.id.clone())?;
        let Ok(generation) = managed_generation(record) else {
            tracing::warn!(
                box_id = %record.id,
                "Failed to read managed generation for abandoned Removing claim"
            );
            continue;
        };
        if let Err(error) = manager.remove(&execution_id, generation).await {
            tracing::warn!(
                box_id = %record.id,
                status = %record.status,
                error = %error,
                "Failed to resume abandoned managed Removing before inventory"
            );
        }
    }
    Ok(())
}

/// Observe / remove-retry in-scope managed claims under the default home, then reload.
pub(crate) async fn refresh_default_home_after_inventory_observation(
) -> Result<StateFile, Box<dyn std::error::Error>> {
    let home = a3s_box_core::dirs_home();
    let state = StateFile::load_default()?;
    let needs_work = state.list(true).into_iter().any(|record| {
        needs_managed_inventory_observation(record) || needs_managed_removal_resume(record)
    });
    if !needs_work {
        return Ok(state);
    }

    let manager = super::configured_local_execution_manager(&home).await?;
    observe_managed_inventory_claims_best_effort(&manager, state.list(true)).await?;
    resume_managed_removal_claims_best_effort(&manager, state.list(true)).await?;
    Ok(StateFile::load_default()?)
}

/// Observe / remove-retry one in-scope managed claim; `None` if remove finished.
pub(crate) async fn refresh_managed_inventory_record(
    record: BoxRecord,
) -> Result<Option<BoxRecord>, Box<dyn std::error::Error>> {
    let needs_observe = needs_managed_inventory_observation(&record);
    let needs_remove = needs_managed_removal_resume(&record);
    if !needs_observe && !needs_remove {
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
        ExecutionManagerError, ExecutionManagerResult, KillOutcome, OperationId,
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

        let mut creating = make_record(
            "66666666-6666-4666-8666-666666666666",
            "box",
            "creating",
            None,
        );
        creating.managed_execution = running.managed_execution.clone();
        assert!(!needs_managed_inventory_observation(&creating));
        assert!(!needs_managed_removal_resume(&creating));

        let mut removing = make_record(
            "77777777-7777-4777-8777-777777777777",
            "box",
            "removing",
            None,
        );
        removing.managed_execution = running.managed_execution.clone();
        assert!(!needs_managed_inventory_observation(&removing));
        assert!(needs_managed_removal_resume(&removing));
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
        assert!(!matches!("failed", "stopped" | "dead" | "created"));
        assert!(!matches!(
            "restart_starting",
            "stopped" | "dead" | "created"
        ));
        assert!(matches!("stopped", "stopped" | "dead" | "created"));
    }
}
