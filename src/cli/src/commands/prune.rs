//! `a3s-box prune` command — Remove all stopped boxes.
//!
//! The box-only counterpart to `system-prune` (which also removes images and
//! networks): removes every created/stopped/dead box in one call, mirroring
//! `docker container prune`. Running boxes are never touched.

use clap::Args;

#[derive(Args)]
pub struct PruneArgs {
    /// Skip confirmation prompt
    #[arg(short, long)]
    pub force: bool,
}

pub async fn execute(args: PruneArgs) -> Result<(), Box<dyn std::error::Error>> {
    if !args.force {
        println!("WARNING: This will remove all created, stopped, and dead boxes.");
        println!("Running and paused boxes are kept.");
        println!();
        println!("Use --force to skip this prompt.");
        return Ok(());
    }

    // Observe managed Starting/Killing/Pausing/Resuming and resume Removing
    // under the lifecycle lock so #385 NotFound retirement / finish_remove is
    // visible before prune selection (durable transitional claims stay skipped
    // by the existing filter; Removing rows are forgotten by remove-retry).
    let mut state =
        super::observe_inventory::refresh_default_home_after_inventory_observation().await?;
    let to_remove: Vec<crate::state::BoxRecord> = state
        .list(true)
        .iter()
        .filter(|r| is_prunable_box(r))
        .map(|record| (*record).clone())
        .collect();

    let mut removed: usize = 0;
    for record in &to_remove {
        crate::cleanup::cleanup_removed_box(record).map_err(|error| {
            prune_cleanup_error(&record.id, error)
        })?;
        state.remove(&record.id).map_err(|error| {
            format!(
                "Failed to remove pruned Box {} from state after host cleanup: {error}",
                record.id
            )
        })?;
        removed += 1;
        println!("Removed box: {}", record.name);
    }

    println!();
    println!("Removed {removed} box(es)");
    Ok(())
}

/// A box is prunable when it is not actively running or paused.
fn is_prunable_box(record: &crate::state::BoxRecord) -> bool {
    matches!(record.status.as_str(), "stopped" | "dead" | "created")
}

fn prune_cleanup_error(
    box_id: &str,
    error: impl std::fmt::Display,
) -> Box<dyn std::error::Error> {
    format!(
        "Failed to clean pruned Box {box_id}: {error}; preserving its state (refusing prune success)"
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::make_record;

    #[test]
    fn test_is_prunable_box_only_inactive() {
        assert!(!is_prunable_box(&make_record(
            "a",
            "running",
            "running",
            Some(1)
        )));
        assert!(!is_prunable_box(&make_record(
            "b",
            "paused",
            "paused",
            Some(1)
        )));
        assert!(is_prunable_box(&make_record(
            "c", "stopped", "stopped", None
        )));
        assert!(is_prunable_box(&make_record("d", "dead", "dead", None)));
        assert!(is_prunable_box(&make_record(
            "e", "created", "created", None
        )));
    }

    #[test]
    fn prune_cleanup_error_refuses_invented_success() {
        let err = prune_cleanup_error("box-1", "lease teardown refused");
        let message = err.to_string();
        assert!(message.contains("box-1"));
        assert!(message.contains("lease teardown refused"));
        assert!(message.contains("refusing prune success"));
    }
}
