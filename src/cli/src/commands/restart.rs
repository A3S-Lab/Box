//! `a3s-box restart` command — Restart one or more boxes.
//!
//! Managed records use the durable lifecycle manager. Legacy records retain
//! the equivalent of `a3s-box stop` followed by `a3s-box start`.

use a3s_box_core::{
    ExecutionGeneration, ExecutionId, ExecutionManager, OperationId, RestartExecutionOptions,
};
use a3s_box_runtime::{LocalExecutionManager, ManagedExecutionOperation, ManagedExecutionState};
use clap::Args;

use crate::boot;
use crate::lifecycle;
use crate::process;
use crate::resolve;
use crate::state::StateFile;
use crate::status;

#[derive(Args)]
pub struct RestartArgs {
    /// Box name(s) or ID(s)
    #[arg(required = true)]
    pub boxes: Vec<String>,

    /// Seconds to wait for stop before force-killing
    #[arg(short = 't', long, default_value = "10")]
    pub timeout: u64,
}

pub async fn execute(args: RestartArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let mut errors: Vec<String> = Vec::new();

    for query in &args.boxes {
        if let Err(e) = restart_one(&state, query, args.timeout).await {
            errors.push(format!("{query}: {e}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n").into())
    }
}

async fn restart_one(
    state: &StateFile,
    query: &str,
    timeout: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let record = resolve::resolve(state, query)?;

    let box_id = record.id.clone();
    let name = record.name.clone();
    let restart_plan = restart_plan(record, timeout)?;
    let box_dir = record.box_dir.clone();
    let exec_socket_path = record.exec_socket_path.clone();

    if let RestartPlan::Managed {
        execution_id,
        generation,
        operation_id,
        stop_timeout_secs,
    } = restart_plan
    {
        let operation_id = match operation_id {
            Some(operation_id) => operation_id,
            None => OperationId::new(format!("cli-restart-{}", uuid::Uuid::new_v4()))?,
        };
        let home = a3s_box_core::dirs_home();
        let manager = LocalExecutionManager::with_vm_backend(home.join("boxes.json"), &home);
        manager
            .restart_with_options(
                &execution_id,
                generation,
                &operation_id,
                RestartExecutionOptions { stop_timeout_secs },
            )
            .await?;
        create_baseline_snapshot(&box_id, &box_dir).await;

        let current = StateFile::load_default()?;
        let record = current
            .find_by_id(&box_id)
            .ok_or_else(|| format!("{name} was removed during restart"))?;
        crate::health::spawn_detached_health_checker(record)
            .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
        println!("{name}");
        return Ok(());
    }

    // Phase 1: Stop the box if it is active.
    if restart_plan == RestartPlan::LegacyStopThenStart {
        let pid = lifecycle::require_live_pid(record, "restart")
            .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
        let stop_signal = record
            .stop_signal
            .as_deref()
            .map(a3s_box_core::vmm::parse_signal_name)
            .unwrap_or(libc::SIGTERM);
        let effective_timeout = record.stop_timeout.unwrap_or(timeout);
        lifecycle::resume_paused_for_termination(record, pid, "restart")
            .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
        // Deliver the stop signal inside the guest so the container honours its
        // STOPSIGNAL and runs its own shutdown (then the VM halts cleanly), as
        // `stop` does. Signalling the host shim never reaches the container and
        // kills the VM abruptly; graceful_stop_via_guest falls back to that only
        // when no guest exec server is reachable.
        process::graceful_stop_via_guest(pid, &exec_socket_path, stop_signal, effective_timeout)
            .await;

        // Update state to stopped — atomically (load-fresh + mutate + save under
        // the lock) so the post-await write cannot clobber a concurrent
        // monitor/run/command writer with our pre-await snapshot.
        crate::cleanup::cleanup_external_socket_dir(&box_dir, &exec_socket_path);
        StateFile::modify(|s| {
            if let Some(record) = s.find_by_id_mut(&box_id) {
                record.status = "stopped".to_string();
                record.pid = None;
            }
            Ok::<(), std::io::Error>(())
        })?;
    }

    // Phase 2: Start the box using shared boot logic. The record's boot config
    // (image, cmd, dirs) is immutable across the stop, so the in-memory handle is
    // fine to boot from; only the post-boot status write must be atomic.
    let record = resolve::resolve(state, &box_id)?;
    // Boot + persist under a per-box boot lock (see boot::boot_and_record): if the
    // monitor (or another concurrent restart) already brought this box back, skip
    // rather than boot a duplicate VM that orphans one of the two.
    match boot::boot_and_record(record, boot::RestartCountUpdate::Preserve).await? {
        boot::BootOutcome::Restarted { .. } => println!("{name}"),
        boot::BootOutcome::AlreadyRunning => println!("{name} (already started)"),
        boot::BootOutcome::RemovedDuringBoot => {
            return Err(format!("{name} was removed during restart").into());
        }
    }
    let current = StateFile::load_default()?;
    if let Some(record) = current.find_by_id(&box_id) {
        crate::health::spawn_detached_health_checker(record)
            .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
    }
    Ok(())
}

async fn create_baseline_snapshot(box_id: &str, box_dir: &std::path::Path) {
    let baseline_box_dir = box_dir.to_path_buf();
    let baseline_box_id = box_id.to_string();
    match tokio::task::spawn_blocking(move || {
        crate::commands::diff::create_box_baseline_snapshot(&baseline_box_dir)
            .map_err(|error| error.to_string())
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(
                box_id = %baseline_box_id,
                %error,
                "Failed to create rootfs diff baseline snapshot after restart"
            );
        }
        Err(error) => {
            tracing::warn!(
                box_id = %baseline_box_id,
                %error,
                "Rootfs diff baseline task failed after restart"
            );
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RestartPlan {
    LegacyStopThenStart,
    LegacyStartOnly,
    Managed {
        execution_id: ExecutionId,
        generation: ExecutionGeneration,
        operation_id: Option<OperationId>,
        stop_timeout_secs: Option<u64>,
    },
}

fn restart_plan(record: &crate::state::BoxRecord, timeout: u64) -> Result<RestartPlan, String> {
    if let Some(metadata) = record.managed_execution.as_ref() {
        let execution_id =
            ExecutionId::new(record.id.clone()).map_err(|error| error.to_string())?;
        let state = record
            .managed_state()
            .map_err(|error| format!("Invalid managed state for box {}: {error}", record.name))?
            .ok_or_else(|| format!("Box {} lost managed lifecycle metadata", record.name))?;
        return match state {
            ManagedExecutionState::Created
            | ManagedExecutionState::Running
            | ManagedExecutionState::Paused
            | ManagedExecutionState::Stopped
            | ManagedExecutionState::Failed => Ok(RestartPlan::Managed {
                execution_id,
                generation: metadata.generation,
                operation_id: None,
                stop_timeout_secs: Some(record.stop_timeout.unwrap_or(timeout)),
            }),
            ManagedExecutionState::RestartStopping | ManagedExecutionState::RestartStarting => {
                match metadata.pending_operation.as_ref() {
                    Some(ManagedExecutionOperation::Restart {
                        operation_id,
                        source_generation,
                        stop_timeout_secs,
                        ..
                    }) => Ok(RestartPlan::Managed {
                        execution_id,
                        generation: *source_generation,
                        operation_id: Some(operation_id.clone()),
                        stop_timeout_secs: *stop_timeout_secs,
                    }),
                    _ => Err(format!(
                        "Box {} has no persisted managed restart intent",
                        record.name
                    )),
                }
            }
            other => Err(format!("Cannot restart box in state: {other}")),
        };
    }
    if status::is_active(record) {
        return Ok(RestartPlan::LegacyStopThenStart);
    }

    match record.status.as_str() {
        "created" | "stopped" | "dead" => Ok(RestartPlan::LegacyStartOnly),
        other => Err(format!("Cannot restart box in state: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::{BoxConfig, CreateExecutionRequest, ExecutionIsolation};
    use a3s_box_runtime::ManagedExecutionMetadata;
    use std::collections::BTreeMap;

    use crate::test_helpers::fixtures::make_record;

    fn managed_record(state: ManagedExecutionState) -> crate::state::BoxRecord {
        let id = "11111111-1111-4111-8111-111111111111";
        let mut record = make_record(id, "managed", state.as_status(), None);
        record.isolation = ExecutionIsolation::Sandbox;
        let mut metadata = ManagedExecutionMetadata::new(
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
            },
        )
        .unwrap();
        if matches!(
            state,
            ManagedExecutionState::RestartStopping | ManagedExecutionState::RestartStarting
        ) {
            metadata.pending_operation = Some(ManagedExecutionOperation::Restart {
                operation_id: OperationId::new("operation-restart").unwrap(),
                source_generation: ExecutionGeneration::INITIAL,
                source_state: ManagedExecutionState::Running,
                stop_timeout_secs: Some(7),
            });
        }
        if state == ManagedExecutionState::RestartStarting {
            metadata.generation = ExecutionGeneration::new(2).unwrap();
        }
        record.managed_execution = Some(metadata);
        record
    }

    #[test]
    fn test_restart_plan_stops_running_and_paused_first() {
        assert_eq!(
            restart_plan(&make_record("id-1", "running", "running", Some(1)), 10).unwrap(),
            RestartPlan::LegacyStopThenStart
        );
        assert_eq!(
            restart_plan(&make_record("id-2", "paused", "paused", Some(1)), 10).unwrap(),
            RestartPlan::LegacyStopThenStart
        );
    }

    #[test]
    fn test_restart_plan_starts_inactive_boxes_directly() {
        assert_eq!(
            restart_plan(&make_record("id-1", "created", "created", None), 10).unwrap(),
            RestartPlan::LegacyStartOnly
        );
        assert_eq!(
            restart_plan(&make_record("id-2", "stopped", "stopped", None), 10).unwrap(),
            RestartPlan::LegacyStartOnly
        );
        assert_eq!(
            restart_plan(&make_record("id-3", "dead", "dead", None), 10).unwrap(),
            RestartPlan::LegacyStartOnly
        );
    }

    #[test]
    fn managed_restart_plan_uses_the_current_generation_for_stable_states() {
        for state in [
            ManagedExecutionState::Created,
            ManagedExecutionState::Running,
            ManagedExecutionState::Paused,
            ManagedExecutionState::Stopped,
            ManagedExecutionState::Failed,
        ] {
            assert_eq!(
                restart_plan(&managed_record(state), 10).unwrap(),
                RestartPlan::Managed {
                    execution_id: ExecutionId::new("11111111-1111-4111-8111-111111111111").unwrap(),
                    generation: ExecutionGeneration::INITIAL,
                    operation_id: None,
                    stop_timeout_secs: Some(10),
                }
            );
        }
    }

    #[test]
    fn managed_restart_plan_recovers_the_persisted_operation() {
        for state in [
            ManagedExecutionState::RestartStopping,
            ManagedExecutionState::RestartStarting,
        ] {
            assert_eq!(
                restart_plan(&managed_record(state), 10).unwrap(),
                RestartPlan::Managed {
                    execution_id: ExecutionId::new("11111111-1111-4111-8111-111111111111").unwrap(),
                    generation: ExecutionGeneration::INITIAL,
                    operation_id: Some(OperationId::new("operation-restart").unwrap()),
                    stop_timeout_secs: Some(7),
                }
            );
        }
    }

    #[test]
    fn managed_restart_plan_uses_record_stop_timeout_before_cli_fallback() {
        let mut record = managed_record(ManagedExecutionState::Running);
        record.stop_timeout = Some(3);

        let RestartPlan::Managed {
            stop_timeout_secs, ..
        } = restart_plan(&record, 10).unwrap()
        else {
            panic!("expected managed restart plan");
        };
        assert_eq!(stop_timeout_secs, Some(3));
    }
}
