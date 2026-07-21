//! `a3s-box unpause` command — Unpause one or more paused boxes.
//!
//! Sends SIGCONT to the box process and updates the status back to "running".

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
    let _lifecycle_lock = lifecycle::acquire_box_lifecycle_lock(&box_id).await?;
    let current_state = StateFile::load_default()?;
    let record = select_unpause_record(&current_state, &box_id)
        .map_err(|error| format!("Box {query} changed while waiting to unpause: {error}"))?;
    drop(current_state);

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

    if record.status != "paused" {
        return Err(format!(
            "Cannot unpause box {} because it is {}. Use `a3s-box ps -a` to inspect state.",
            record.name, record.status
        )
        .into());
    }

    lifecycle::require_live_pid(record, "unpause")?;
    Ok(record.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::{make_record, setup_state};

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
}
