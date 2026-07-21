//! `a3s-box stop` command — Graceful stop of one or more boxes.

use clap::Args;

use a3s_box_core::{
    vmm::parse_signal_name, ExecutionGeneration, ExecutionId, ExecutionManager,
    KillExecutionOptions,
};
use a3s_box_runtime::{LocalExecutionManager, ManagedExecutionState};

use crate::cleanup;
use crate::lifecycle;
use crate::process;
use crate::resolve;
use crate::state::StateFile;
use crate::status;

#[derive(Args)]
pub struct StopArgs {
    /// Box name(s) or ID(s)
    #[arg(required = true)]
    pub boxes: Vec<String>,

    /// Seconds to wait before force-killing (overrides per-box stop-timeout)
    #[arg(short = 't', long)]
    pub timeout: Option<u64>,
}

pub async fn execute(args: StopArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let mut errors: Vec<String> = Vec::new();

    for query in &args.boxes {
        if let Err(e) = stop_one(&state, query, args.timeout).await {
            errors.push(format!("{query}: {e}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n").into())
    }
}

async fn stop_one(
    state: &StateFile,
    query: &str,
    timeout: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let box_id = resolve::resolve(state, query)?.id.clone();
    let lifecycle_lock = lifecycle::acquire_box_lifecycle_lock(&box_id).await?;
    // Reload only after acquiring the shared per-box lock. Otherwise a start or
    // restart can publish a new PID while this command is waiting on the old
    // process, and the final stopped write would erase the new execution.
    let current_state = StateFile::load_default()?;
    let record = current_state
        .find_by_id(&box_id)
        .ok_or_else(|| format!("Box {query} was removed while waiting to stop"))?
        .clone();
    drop(current_state);

    if let StopPlan::Managed {
        execution_id,
        generation,
        options,
    } = stop_plan(&record, timeout)?
    {
        let name = record.name.clone();
        let auto_remove = record.auto_remove;
        drop(lifecycle_lock);
        let home = a3s_box_core::dirs_home();
        let manager = LocalExecutionManager::with_vm_backend(home.join("boxes.json"), &home);
        manager
            .kill_with_options(&execution_id, generation, options)
            .await?;
        if auto_remove {
            manager.remove_execution(&execution_id, generation).await?;
            println!("{name} (auto-removed)");
            return Ok(());
        }
        crate::audit::record(
            a3s_box_core::audit::AuditAction::BoxStop,
            a3s_box_core::audit::AuditOutcome::Success,
            &box_id,
            &format!("stopped box {name}"),
        );
        println!("{name}");
        return Ok(());
    }

    status::require_active(&record, "stop")
        .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
    let pid = lifecycle::require_live_pid(&record, "stop")
        .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;

    let name = record.name.clone();
    let auto_remove = record.auto_remove;
    let record_snapshot = record.clone();
    let previous_exit_code = record.exit_code;

    // Resolve stop signal: CLI --stop-signal > BoxRecord.stop_signal > SIGTERM
    let stop_signal = record
        .stop_signal
        .as_deref()
        .map(parse_signal_name)
        .unwrap_or(15); // SIGTERM = 15

    // Resolve timeout: CLI -t > BoxRecord.stop_timeout > 10s
    let effective_timeout = timeout.or(record.stop_timeout).unwrap_or(10);

    // Exec socket used to deliver the stop signal inside the guest.
    let exec_socket = crate::socket_paths::exec(&record);

    // Deliver the stop signal to the container (honouring its STOPSIGNAL), then
    // wait for the VM to exit; SIGKILL the shim after the timeout.
    lifecycle::resume_paused_for_termination(&record, pid, "stop")
        .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
    let stop_outcome = Some(
        process::graceful_stop_via_guest(pid, &exec_socket, stop_signal, effective_timeout).await,
    );

    if auto_remove {
        cleanup::cleanup_removed_box(&record_snapshot)?;
        StateFile::remove_record(&box_id)?;
        println!("{name} (auto-removed)");
        return Ok(());
    }

    cleanup::cleanup_stopped_box(&record_snapshot)?;

    // Apply the status change atomically (load-fresh + mutate + save under the
    // state lock) so it cannot clobber a concurrent run/monitor/compose write
    // with our pre-await snapshot.
    let new_exit_code = stopped_exit_code(previous_exit_code, stop_outcome, stop_signal);
    let expected_pid_start_time = record.pid_start_time;
    let persisted = StateFile::modify(|s| {
        let updated = match s.find_by_id_mut(&box_id) {
            Some(record) if lifecycle::matches_execution(record, pid, expected_pid_start_time) => {
                record.status = "stopped".to_string();
                record.pid = None;
                record.stopped_by_user = true;
                record.exit_code = new_exit_code;
                record.health_status = "none".to_string();
                record.health_retries = 0;
                true
            }
            _ => false,
        };
        Ok::<bool, std::io::Error>(updated)
    })?;
    if !persisted {
        return Err(format!(
            "Box {name} changed execution while it was stopping; did not overwrite the replacement state"
        )
        .into());
    }
    crate::audit::record(
        a3s_box_core::audit::AuditAction::BoxStop,
        a3s_box_core::audit::AuditOutcome::Success,
        &box_id,
        &format!("stopped box {name}"),
    );
    println!("{name}");

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StopPlan {
    Legacy,
    Managed {
        execution_id: ExecutionId,
        generation: ExecutionGeneration,
        options: KillExecutionOptions,
    },
}

fn stop_plan(
    record: &crate::state::BoxRecord,
    timeout: Option<u64>,
) -> Result<StopPlan, Box<dyn std::error::Error>> {
    let Some(metadata) = record.managed_execution.as_ref() else {
        return Ok(StopPlan::Legacy);
    };
    let state = record
        .managed_state()?
        .ok_or_else(|| format!("Box {} lost managed lifecycle metadata", record.name))?;
    if !matches!(
        state,
        ManagedExecutionState::Running
            | ManagedExecutionState::Paused
            | ManagedExecutionState::Killing
    ) {
        return Err(format!(
            "Cannot stop box {} because it is {state}. Use `a3s-box ps -a` to inspect state.",
            record.name
        )
        .into());
    }
    let signal = record
        .stop_signal
        .as_deref()
        .map(parse_signal_name)
        .unwrap_or(15);
    Ok(StopPlan::Managed {
        execution_id: ExecutionId::new(record.id.clone())?,
        generation: metadata.generation,
        options: KillExecutionOptions {
            signal: Some(signal),
            timeout_secs: Some(timeout.or(record.stop_timeout).unwrap_or(10)),
        },
    })
}

fn stopped_exit_code(
    previous_exit_code: Option<i32>,
    outcome: Option<process::StopOutcome>,
    stop_signal: i32,
) -> Option<i32> {
    outcome
        .and_then(|outcome| outcome.inferred_exit_code(stop_signal))
        .or(previous_exit_code)
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
    fn test_stopped_exit_code_uses_graceful_signal_code() {
        assert_eq!(
            stopped_exit_code(None, Some(process::StopOutcome::GracefulExit), 15),
            Some(143)
        );
    }

    #[test]
    fn test_stopped_exit_code_uses_forced_kill_code() {
        assert_eq!(
            stopped_exit_code(Some(7), Some(process::StopOutcome::ForceKilled), 15),
            Some(137)
        );
    }

    #[test]
    fn test_stopped_exit_code_preserves_previous_when_already_exited() {
        assert_eq!(
            stopped_exit_code(Some(7), Some(process::StopOutcome::AlreadyExited), 15),
            Some(7)
        );
    }

    #[test]
    fn test_stop_accepts_paused_status_as_active() {
        let record = make_record("id", "box", "paused", Some(1));

        assert!(status::require_active(&record, "stop").is_ok());
    }

    #[test]
    fn managed_stop_uses_generation_and_cli_timeout_without_a_host_pid() {
        let mut record = managed_record(ManagedExecutionState::Paused);
        record.stop_signal = Some("SIGINT".to_string());
        record.stop_timeout = Some(12);

        assert_eq!(
            stop_plan(&record, Some(3)).unwrap(),
            StopPlan::Managed {
                execution_id: ExecutionId::new("11111111-1111-4111-8111-111111111111").unwrap(),
                generation: ExecutionGeneration::INITIAL,
                options: KillExecutionOptions {
                    signal: Some(2),
                    timeout_secs: Some(3),
                },
            }
        );
    }

    #[test]
    fn managed_stop_rejects_non_active_stable_state() {
        let error = stop_plan(&managed_record(ManagedExecutionState::Stopped), None)
            .unwrap_err()
            .to_string();

        assert!(error.contains("because it is stopped"));
    }
}
