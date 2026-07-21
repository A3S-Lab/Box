//! `a3s-box pause` command — Pause one or more running boxes.
//!
//! Uses the durable execution manager for managed boxes and SIGSTOP only for
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
pub struct PauseArgs {
    /// Box name(s) or ID(s)
    #[arg(required = true)]
    pub boxes: Vec<String>,
}

pub async fn execute(args: PauseArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let mut errors: Vec<String> = Vec::new();

    for query in &args.boxes {
        if let Err(e) = pause_one(&state, query).await {
            errors.push(format!("{query}: {e}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n").into())
    }
}

async fn pause_one(state: &StateFile, query: &str) -> Result<(), Box<dyn std::error::Error>> {
    let box_id = resolve::resolve(state, query)?.id.clone();
    let lifecycle_lock = lifecycle::acquire_box_lifecycle_lock(&box_id).await?;
    let current_state = StateFile::load_default()?;
    let record = select_pause_record(&current_state, &box_id)
        .map_err(|error| format!("Box {query} changed while waiting to pause: {error}"))?;
    drop(current_state);

    if let PausePlan::Managed {
        execution_id,
        generation,
    } = pause_plan(&record)?
    {
        // The managed lifecycle uses the same cross-process lock. Release the
        // command guard before entering it so Sandbox pause is performed by
        // crun and the durable generation advances exactly once.
        drop(lifecycle_lock);
        let home = a3s_box_core::dirs_home();
        let manager = LocalExecutionManager::with_vm_backend(home.join("boxes.json"), &home);
        manager.pause(&execution_id, generation, true).await?;
        println!("{}", record.name);
        return Ok(());
    }

    let pid = lifecycle::require_live_pid(&record, "pause")?;

    #[cfg(windows)]
    {
        let _ = pid;
        return Err(crate::platform::unsupported_command(
            "pause",
            "host process suspension support",
        ));
    }

    #[cfg(unix)]
    {
        let name = record.name.clone();

        process::send_signal(pid, libc::SIGSTOP)
            .map_err(|err| format!("Failed to pause box {name} with SIGSTOP: {err}"))?;

        // The lifecycle lock remains held through the state write, preventing a
        // concurrent start/restart from publishing a new PID that this status
        // transition would mislabel as paused.
        let expected_pid_start_time = record.pid_start_time;
        let persisted = StateFile::modify(|s| {
            let updated = match s.find_by_id_mut(&box_id) {
                Some(record)
                    if lifecycle::matches_execution(record, pid, expected_pid_start_time) =>
                {
                    record.status = "paused".to_string();
                    true
                }
                _ => false,
            };
            Ok::<bool, std::io::Error>(updated)
        })?;
        if !persisted {
            return Err(format!(
                "Box {name} changed execution while it was pausing; did not overwrite the replacement state"
            )
            .into());
        }

        println!("{name}");
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        Err("'pause' requires host process suspension support".into())
    }
}

fn select_pause_record(
    state: &StateFile,
    query: &str,
) -> Result<crate::state::BoxRecord, Box<dyn std::error::Error>> {
    let record = resolve::resolve(state, query)?;

    pause_plan(record)?;
    Ok(record.clone())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PausePlan {
    Legacy,
    Managed {
        execution_id: ExecutionId,
        generation: ExecutionGeneration,
    },
}

fn pause_plan(record: &crate::state::BoxRecord) -> Result<PausePlan, Box<dyn std::error::Error>> {
    if let Some(metadata) = record.managed_execution.as_ref() {
        let state = record
            .managed_state()?
            .ok_or_else(|| format!("Box {} lost managed lifecycle metadata", record.name))?;
        if state != ManagedExecutionState::Running {
            return Err(format!(
                "Cannot pause box {} because it is {state}. Use `a3s-box ps -a` to inspect state.",
                record.name
            )
            .into());
        }
        return Ok(PausePlan::Managed {
            execution_id: ExecutionId::new(record.id.clone())?,
            generation: metadata.generation,
        });
    }

    if record.status != "running" {
        return Err(format!(
            "Cannot pause box {} because it is {}. Use `a3s-box start {}` to start it or `a3s-box ps -a` to inspect state.",
            record.name, record.status, record.name
        )
        .into());
    }
    lifecycle::require_live_pid(record, "pause")?;
    Ok(PausePlan::Legacy)
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
    fn test_pause_rejects_non_running() {
        let (_tmp, state) = setup_state(vec![make_record("id-1", "stopped_box", "stopped", None)]);
        let result = select_pause_record(&state, "stopped_box");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Cannot pause"));
    }

    #[test]
    fn test_pause_rejects_created() {
        let (_tmp, state) = setup_state(vec![make_record("id-1", "created_box", "created", None)]);
        let result = select_pause_record(&state, "created_box");
        assert!(result.is_err());
    }

    #[test]
    fn test_pause_rejects_already_paused() {
        let (_tmp, state) = setup_state(vec![make_record(
            "id-1",
            "paused_box",
            "paused",
            Some(99999),
        )]);
        let result = select_pause_record(&state, "paused_box");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Cannot pause"));
    }

    #[test]
    fn test_pause_rejects_running_without_pid() {
        let (_tmp, state) = setup_state(vec![make_record("id-1", "running_box", "running", None)]);

        let result = select_pause_record(&state, "running_box");

        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(error.contains("no recorded PID"));
        assert!(error.contains("a3s-box ps"));
        assert_eq!(
            state.find_by_id("id-1").unwrap().status,
            "running",
            "stale PID failures must not mark the box paused"
        );
    }

    #[test]
    fn test_pause_not_found() {
        let (_tmp, state) = setup_state(vec![]);
        let result = select_pause_record(&state, "nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn managed_pause_uses_the_execution_manager_generation() {
        assert_eq!(
            pause_plan(&managed_record(ManagedExecutionState::Running)).unwrap(),
            PausePlan::Managed {
                execution_id: ExecutionId::new("11111111-1111-4111-8111-111111111111").unwrap(),
                generation: ExecutionGeneration::INITIAL,
            }
        );
    }

    #[test]
    fn managed_pause_rejects_non_running_state_without_using_a_host_pid() {
        let error = pause_plan(&managed_record(ManagedExecutionState::Paused))
            .unwrap_err()
            .to_string();

        assert!(error.contains("because it is paused"));
    }
}
