//! `a3s-box image-prune` command — remove unused images.

use std::collections::HashSet;

use clap::Args;

use crate::output;
use crate::state::StateFile;

#[derive(Args)]
pub struct ImagePruneArgs {
    /// Remove all unused images, not just dangling ones
    #[arg(short, long)]
    pub all: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub force: bool,
}

pub async fn execute(args: ImagePruneArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = super::open_image_store()?;

    // Collect image references used by existing boxes
    let used_images: HashSet<String> = match StateFile::load_default() {
        Ok(state) => state.records().iter().map(|r| r.image.clone()).collect(),
        Err(_) => HashSet::new(),
    };

    let all_images = store.list().await;

    // Determine which images to remove
    let to_remove: Vec<_> = all_images
        .iter()
        .filter(|img| {
            if args.all {
                // Remove all images not referenced by any box
                !used_images.contains(&img.reference)
            } else {
                // Without --all, only remove images not referenced by any box
                // (same behavior for now — Docker distinguishes dangling vs unused,
                // but our store doesn't track parent/child image relationships)
                !used_images.contains(&img.reference)
            }
        })
        .collect();

    if to_remove.is_empty() {
        println!("No unused images to remove.");
        return Ok(());
    }

    // Show what will be removed
    if !args.force {
        println!("WARNING: This will remove {} image(s):", to_remove.len());
        for img in &to_remove {
            println!(
                "  {} ({})",
                img.reference,
                output::format_bytes(img.size_bytes)
            );
        }
        println!();
        println!("Use --force to skip this prompt.");
        return Ok(());
    }

    let mut freed: u64 = 0;
    let mut count: usize = 0;
    let mut errors: Vec<String> = Vec::new();

    for img in &to_remove {
        match store.remove(&img.reference).await {
            Ok(()) => {
                freed += img.size_bytes;
                count += 1;
            }
            Err(e) => {
                errors.push(format!("{}: {e}", img.reference));
            }
        }
    }

    println!(
        "Removed {} image(s), freed {}",
        count,
        output::format_bytes(freed)
    );

    if !errors.is_empty() {
        eprintln!("\nErrors:");
        for err in &errors {
            eprintln!("  {err}");
        }
    }

    Ok(())
}
