//! `a3s-box rm` command — Remove one or more boxes.

use clap::Args;

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
    let _lifecycle_lock = crate::lifecycle::acquire_box_lifecycle_lock(&box_id).await?;
    // Re-resolve after waiting for start/restart/commit. The lock then remains
    // held through termination, cleanup, and removal so no new execution can be
    // published for a record whose resources are being deleted.
    let current_state = StateFile::load_default()?;
    let record = current_state
        .find_by_id(&box_id)
        .ok_or_else(|| format!("Box {query} was removed while waiting to remove it"))?
        .clone();
    drop(current_state);

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
}
