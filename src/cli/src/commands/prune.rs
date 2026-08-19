//! `a3s-box prune` command — Remove all stopped boxes.
//!
//! The box-only counterpart to `system-prune` (which also removes images and
//! networks): removes every created/stopped/dead box in one call, mirroring
//! `docker container prune`. Running boxes are never touched.

use clap::Args;

use crate::state::StateFile;

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

    let mut state = StateFile::load_default()?;
    let to_remove: Vec<crate::state::BoxRecord> = state
        .list(true)
        .iter()
        .filter(|r| is_prunable_box(r))
        .map(|record| (*record).clone())
        .collect();

    let mut removed: usize = 0;
    for record in &to_remove {
        if let Err(error) = crate::cleanup::cleanup_removed_box(record) {
            tracing::warn!(
                box_id = %record.id,
                error = %error,
                "Failed to clean pruned Box resources; preserving its state"
            );
            continue;
        }
        if state.remove(&record.id).is_ok() {
            removed += 1;
            println!("Removed box: {}", record.name);
        }
    }

    println!();
    println!("Removed {removed} box(es)");
    Ok(())
}

/// A box is prunable when it is not actively running or paused.
fn is_prunable_box(record: &crate::state::BoxRecord) -> bool {
    matches!(record.status.as_str(), "stopped" | "dead" | "created")
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
}
