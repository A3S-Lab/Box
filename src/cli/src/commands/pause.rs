//! `a3s-box pause` command — Pause one or more running boxes.
//!
//! Sends SIGSTOP to the box process and updates the status to "paused".

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
    let _lifecycle_lock = lifecycle::acquire_box_lifecycle_lock(&box_id).await?;
    let current_state = StateFile::load_default()?;
    let record = select_pause_record(&current_state, &box_id)
        .map_err(|error| format!("Box {query} changed while waiting to pause: {error}"))?;
    drop(current_state);

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

    if record.status != "running" {
        return Err(format!(
            "Cannot pause box {} because it is {}. Use `a3s-box start {}` to start it or `a3s-box ps -a` to inspect state.",
            record.name, record.status, record.name
        )
        .into());
    }

    lifecycle::require_live_pid(record, "pause")?;
    Ok(record.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::{make_record, setup_state};

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
}
