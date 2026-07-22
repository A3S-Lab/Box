//! `a3s-box rm` command — Remove one or more boxes.

use clap::Args;

use a3s_box_core::{ExecutionGeneration, ExecutionId, ExecutionManager, KillExecutionOptions};
use a3s_box_runtime::{LocalExecutionManager, ManagedExecutionState};

use crate::cleanup;
use crate::resolve;
use crate::state::StateFile;
use crate::status;

#[derive(Args)]
pub struct RmArgs {
    /// Box name(s) or ID(s)
    #[arg(required = true)]
    pub boxes: Vec<String>,

    /// Force removal of active boxes (terminates them first)
    #[arg(short, long)]
    pub force: bool,
}

pub async fn execute(args: RmArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = StateFile::load_default()?;
    let mut errors: Vec<String> = Vec::new();

    for query in &args.boxes {
        if let Err(e) = rm_one(&mut state, query, args.force).await {
            errors.push(format!("{query}: {e}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n").into())
    }
}

async fn rm_one(
    state: &mut StateFile,
    query: &str,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let box_id = resolve::resolve(state, query)?.id.clone();
    let lifecycle_lock = crate::lifecycle::acquire_box_lifecycle_lock(&box_id).await?;
    // Re-resolve after waiting for start/restart/commit. Legacy removal keeps
    // this guard through cleanup; managed removal hands ownership to the
    // execution manager, which acquires the same cross-process lock itself.
    let current_state = StateFile::load_default()?;
    let record = current_state
        .find_by_id(&box_id)
        .ok_or_else(|| format!("Box {query} was removed while waiting to remove it"))?
        .clone();
    drop(current_state);

    if let RmPlan::Managed {
        execution_id,
        generation,
        terminate,
    } = rm_plan(&record, force)?
    {
        let name = record.name.clone();
        drop(lifecycle_lock);
        let home = a3s_box_core::dirs_home();
        let manager = LocalExecutionManager::with_vm_backend(home.join("boxes.json"), &home);
        if terminate {
            manager
                .kill_with_options(
                    &execution_id,
                    generation,
                    KillExecutionOptions {
                        signal: Some(9),
                        timeout_secs: Some(0),
                    },
                )
                .await?;
        }
        manager.remove_execution(&execution_id, generation).await?;
        state.forget(&box_id);
        crate::audit::record(
            a3s_box_core::audit::AuditAction::BoxDestroy,
            a3s_box_core::audit::AuditOutcome::Success,
            &box_id,
            &format!("removed box {name}"),
        );
        println!("{name}");
        return Ok(());
    }

    validate_remove_request(&record, force)?;

    if status::is_active(&record) {
        // Force-kill the active box. A missing PID is treated as stale state;
        // --force still removes metadata and resources below. Only signal a PID
        // whose start-time identity still matches, so a reused PID after a
        // crash/reboot is never killed.
        if let Some(pid) = record.pid {
            if crate::process::is_process_alive_with_identity(pid, record.pid_start_time) {
                crate::process::terminate_process(pid);
            }
        }
    }

    let name = record.name.clone();
    cleanup::cleanup_removed_box(&record)?;

    // Remove from state atomically under the lock (avoids clobbering concurrent
    // monitor/CLI writers that rewrite the whole record vector), then keep this
    // in-memory handle consistent without a second persisting write.
    StateFile::remove_record(&box_id)?;
    state.forget(&box_id);
    crate::audit::record(
        a3s_box_core::audit::AuditAction::BoxDestroy,
        a3s_box_core::audit::AuditOutcome::Success,
        &box_id,
        &format!("removed box {name}"),
    );
    println!("{name}");

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RmPlan {
    Legacy,
    Managed {
        execution_id: ExecutionId,
        generation: ExecutionGeneration,
        terminate: bool,
    },
}

fn rm_plan(
    record: &crate::state::BoxRecord,
    force: bool,
) -> Result<RmPlan, Box<dyn std::error::Error>> {
    let Some(metadata) = record.managed_execution.as_ref() else {
        return Ok(RmPlan::Legacy);
    };
    let state = record
        .managed_state()?
        .ok_or_else(|| format!("Box {} lost managed lifecycle metadata", record.name))?;
    let terminate = match state {
        ManagedExecutionState::Created
        | ManagedExecutionState::Stopped
        | ManagedExecutionState::Failed
        | ManagedExecutionState::Removing => false,
        ManagedExecutionState::Running
        | ManagedExecutionState::Paused
        | ManagedExecutionState::Killing => {
            if !force {
                return Err(format!(
                    "Box {} is {state}. Use --force to remove an active box.",
                    record.name
                )
                .into());
            }
            true
        }
        other => {
            return Err(format!(
                "Cannot remove box {} while its managed lifecycle is {other}",
                record.name
            )
            .into());
        }
    };
    Ok(RmPlan::Managed {
        execution_id: ExecutionId::new(record.id.clone())?,
        generation: metadata.generation,
        terminate,
    })
}

fn validate_remove_request(
    record: &crate::state::BoxRecord,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if status::is_active(record) && !force {
        return Err(format!(
            "Box {} is {}. Use --force to remove an active box.",
            record.name, record.status
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::make_record;
    use a3s_box_core::{BoxConfig, CreateExecutionRequest, ExecutionIsolation, OperationId};
    use a3s_box_runtime::ManagedExecutionMetadata;
    use std::collections::BTreeMap;

    fn managed_record(state: ManagedExecutionState) -> crate::state::BoxRecord {
        let id = "11111111-1111-4111-8111-111111111111";
        let mut record = make_record(id, "managed", state.as_status(), None);
        record.isolation = ExecutionIsolation::Sandbox;
        record.managed_execution = Some(
            ManagedExecutionMetadata::new(
                OperationId::new("operation-create").unwrap(),
                ExecutionGeneration::INITIAL,
                CreateExecutionRequest {
                    external_sandbox_id: "external-1".to_string(),
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
            .unwrap(),
        );
        record
    }

    #[test]
    fn test_rm_rejects_paused_without_force() {
        let record = make_record("id-1", "paused_box", "paused", None);

        let result = validate_remove_request(&record, false);

        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(error.contains("paused"));
        assert!(error.contains("--force"));
    }

    #[test]
    fn test_rm_force_accepts_paused_stale_record() {
        let record = make_record("id-1", "paused_box", "paused", None);

        validate_remove_request(&record, true).unwrap();
    }

    #[test]
    fn managed_rm_routes_active_records_through_the_execution_manager() {
        assert_eq!(
            rm_plan(&managed_record(ManagedExecutionState::Paused), true).unwrap(),
            RmPlan::Managed {
                execution_id: ExecutionId::new("11111111-1111-4111-8111-111111111111").unwrap(),
                generation: ExecutionGeneration::INITIAL,
                terminate: true,
            }
        );
    }

    #[test]
    fn managed_rm_rejects_active_records_without_force() {
        let error = rm_plan(&managed_record(ManagedExecutionState::Running), false)
            .unwrap_err()
            .to_string();

        assert!(error.contains("--force"));
    }

    #[test]
    fn managed_rm_removes_terminal_records_without_a_backend_kill() {
        assert_eq!(
            rm_plan(&managed_record(ManagedExecutionState::Stopped), false).unwrap(),
            RmPlan::Managed {
                execution_id: ExecutionId::new("11111111-1111-4111-8111-111111111111").unwrap(),
                generation: ExecutionGeneration::INITIAL,
                terminate: false,
            }
        );
    }
}
