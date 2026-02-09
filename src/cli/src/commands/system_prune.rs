//! `a3s-box system-prune` command — Remove all unused data.
//!
//! Removes stopped boxes and unused images in one operation.

use clap::Args;

use crate::output;
use crate::state::StateFile;

use std::collections::HashSet;

#[derive(Args)]
pub struct SystemPruneArgs {
    /// Remove all unused images, not just dangling ones
    #[arg(short, long)]
    pub all: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub force: bool,
}

pub async fn execute(args: SystemPruneArgs) -> Result<(), Box<dyn std::error::Error>> {
    if !args.force {
        println!("WARNING: This will remove:");
        println!("  - all stopped boxes");
        if args.all {
            println!("  - all images not used by running boxes");
        } else {
            println!("  - all dangling images");
        }
        println!();
        println!("Use --force to skip this prompt.");
        return Ok(());
    }

    let mut boxes_removed: usize = 0;
    let mut images_removed: usize = 0;
    let mut space_freed: u64 = 0;

    // Phase 1: Remove stopped/dead boxes
    let mut state = StateFile::load_default()?;
    let all_boxes = state.list(true);

    let to_remove: Vec<(String, String, std::path::PathBuf)> = all_boxes
        .iter()
        .filter(|r| matches!(r.status.as_str(), "stopped" | "dead" | "created"))
        .map(|r| (r.id.clone(), r.name.clone(), r.box_dir.clone()))
        .collect();

    for (box_id, name, box_dir) in &to_remove {
        if box_dir.exists() {
            let _ = std::fs::remove_dir_all(box_dir);
        }
        if state.remove(box_id).is_ok() {
            boxes_removed += 1;
            println!("Removed box: {name}");
        }
    }

    // Phase 2: Remove unused images
    // Reload state to get current running boxes after removal
    let state = StateFile::load_default()?;
    let used_images: HashSet<String> = state
        .list(false) // only running boxes
        .iter()
        .map(|r| r.image.clone())
        .collect();

    let images_dir = super::images_dir();
    if images_dir.exists() {
        if let Ok(store) = super::open_image_store() {
            let all_images = store.list().await;

            for image in &all_images {
                if !used_images.contains(&image.reference)
                    && store.remove(&image.reference).await.is_ok()
                {
                    space_freed += image.size_bytes;
                    images_removed += 1;
                    println!("Removed image: {}", image.reference);
                }
            }
        }
    }

    println!();
    println!(
        "Removed {} box(es), {} image(s), freed {}",
        boxes_removed,
        images_removed,
        output::format_bytes(space_freed)
    );

    Ok(())
}
