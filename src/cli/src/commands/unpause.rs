//! `a3s-box unpause` command — Unpause one or more paused boxes.
//!
//! Uses the durable execution manager for managed boxes and SIGCONT only for
//! legacy state records.

use a3s_box_core::{ExecutionGeneration, ExecutionId, ExecutionManager};
use a3s_box_runtime::{LocalExecutionManager, ManagedExecutionState};
use clap::Args;

use crate::lifecycle;
#[cfg(unix)]
use crate::process;
use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct UnpauseArgs {
    /// Box name(s) or ID(s)
    #[arg(required = true)]
    pub boxes: Vec<String>,
}

pub async fn execute(args: UnpauseArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let mut errors: Vec<String> = Vec::new();

    for query in &args.boxes {
        if let Err(e) = unpause_one(&state, query).await {
            errors.push(format!("{query}: {e}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n").into())
    }
}

async fn unpause_one(state: &StateFile, query: &str) -> Result<(), Box<dyn std::error::Error>> {
    let box_id = resolve::resolve(state, query)?.id.clone();
    let lifecycle_lock = lifecycle::acquire_box_lifecycle_lock(&box_id).await?;
    let current_state = StateFile::load_default()?;
    let record = select_unpause_record(&current_state, &box_id)
        .map_err(|error| format!("Box {query} changed while waiting to unpause: {error}"))?;
    drop(current_state);

    if let UnpausePlan::Managed {
        execution_id,
        generation,
    } = unpause_plan(&record)?
    {
        // The runtime manager owns both crun resume and generation fencing for
        // current records. Keep direct SIGCONT only for legacy state files.
        drop(lifecycle_lock);
        let home = a3s_box_core::dirs_home();
        let manager = LocalExecutionManager::with_vm_backend(home.join("boxes.json"), &home);
        manager.resume(&execution_id, generation).await?;
        println!("{}", record.name);
        return Ok(());
    }

    let pid = lifecycle::require_live_pid(&record, "unpause")?;

    #[cfg(windows)]
    {
        let _ = pid;
        return Err(crate::platform::unsupported_command(
            "unpause",
            "host process resume support",
        ));
    }

    #[cfg(unix)]
    {
        let name = record.name.clone();

        process::send_signal(pid, libc::SIGCONT)
            .map_err(|err| format!("Failed to unpause box {name} with SIGCONT: {err}"))?;

        let expected_pid_start_time = record.pid_start_time;
        let persisted = StateFile::modify(|s| {
            let updated = match s.find_by_id_mut(&box_id) {
                Some(record)
                    if lifecycle::matches_execution(record, pid, expected_pid_start_time) =>
                {
                    record.status = "running".to_string();
                    true
                }
                _ => false,
            };
            Ok::<bool, std::io::Error>(updated)
        })?;
        if !persisted {
            return Err(format!(
                "Box {name} changed execution while it was unpausing; did not overwrite the replacement state"
            )
            .into());
        }

        println!("{name}");
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        Err("'unpause' requires host process resume support".into())
    }
}

fn select_unpause_record(
    state: &StateFile,
    query: &str,
) -> Result<crate::state::BoxRecord, Box<dyn std::error::Error>> {
    let record = resolve::resolve(state, query)?;

    unpause_plan(record)?;
    Ok(record.clone())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum UnpausePlan {
    Legacy,
    Managed {
        execution_id: ExecutionId,
        generation: ExecutionGeneration,
    },
}

fn unpause_plan(
    record: &crate::state::BoxRecord,
) -> Result<UnpausePlan, Box<dyn std::error::Error>> {
    if let Some(metadata) = record.managed_execution.as_ref() {
        let state = record
            .managed_state()?
            .ok_or_else(|| format!("Box {} lost managed lifecycle metadata", record.name))?;
        if state != ManagedExecutionState::Paused {
            return Err(format!(
                "Cannot unpause box {} because it is {state}. Use `a3s-box ps -a` to inspect state.",
                record.name
            )
            .into());
        }
        return Ok(UnpausePlan::Managed {
            execution_id: ExecutionId::new(record.id.clone())?,
            generation: metadata.generation,
        });
    }

    if record.status != "paused" {
        return Err(format!(
            "Cannot unpause box {} because it is {}. Use `a3s-box ps -a` to inspect state.",
            record.name, record.status
        )
        .into());
    }
    lifecycle::require_live_pid(record, "unpause")?;
    Ok(UnpausePlan::Legacy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::{make_record, setup_state};
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
    fn test_unpause_rejects_running() {
        let (_tmp, state) = setup_state(vec![make_record(
            "id-1",
            "running_box",
            "running",
            Some(99999),
        )]);
        let result = select_unpause_record(&state, "running_box");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Cannot unpause"));
    }

    #[test]
    fn test_unpause_rejects_stopped() {
        let (_tmp, state) = setup_state(vec![make_record("id-1", "stopped_box", "stopped", None)]);
        let result = select_unpause_record(&state, "stopped_box");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Cannot unpause"));
    }

    #[test]
    fn test_unpause_rejects_paused_without_pid() {
        let (_tmp, state) = setup_state(vec![make_record("id-1", "paused_box", "paused", None)]);

        let result = select_unpause_record(&state, "paused_box");

        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(error.contains("no recorded PID"));
        assert!(error.contains("a3s-box ps"));
        assert_eq!(
            state.find_by_id("id-1").unwrap().status,
            "paused",
            "stale PID failures must not mark the box running"
        );
    }

    #[test]
    fn test_unpause_not_found() {
        let (_tmp, state) = setup_state(vec![]);
        let result = select_unpause_record(&state, "nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn managed_unpause_uses_the_execution_manager_generation() {
        assert_eq!(
            unpause_plan(&managed_record(ManagedExecutionState::Paused)).unwrap(),
            UnpausePlan::Managed {
                execution_id: ExecutionId::new("11111111-1111-4111-8111-111111111111").unwrap(),
                generation: ExecutionGeneration::INITIAL,
            }
        );
    }

    #[test]
    fn managed_unpause_rejects_non_paused_state_without_using_a_host_pid() {
        let error = unpause_plan(&managed_record(ManagedExecutionState::Running))
            .unwrap_err()
            .to_string();

        assert!(error.contains("because it is running"));
    }
}
