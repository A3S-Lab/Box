//! `a3s-box df` command — Show disk usage.
//!
//! Displays disk usage for images and boxes, similar to `docker system df`.

use clap::Args;

use crate::output;
use crate::state::StateFile;

#[derive(Args)]
pub struct DfArgs {
    /// Show detailed per-item usage
    #[arg(short, long)]
    pub verbose: bool,
}

pub async fn execute(args: DfArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = super::open_image_store()?;
    let state = StateFile::load_default()?;

    // Image stats
    let images = store.list().await;
    let image_count = images.len();
    let image_total_size = store.total_size().await;

    // Box stats
    let boxes = state.list(true);
    let box_count = boxes.len();
    let active_boxes = boxes.iter().filter(|b| b.status == "running").count();
    let box_total_size: u64 = boxes.iter().map(|b| dir_size(&b.box_dir)).sum();

    // Reclaimable: stopped/dead boxes + unused images
    let reclaimable_boxes: u64 = boxes
        .iter()
        .filter(|b| b.status != "running")
        .map(|b| dir_size(&b.box_dir))
        .sum();

    // Summary table
    let mut table = output::new_table(&["TYPE", "TOTAL", "ACTIVE", "SIZE", "RECLAIMABLE"]);

    table.add_row([
        "Images",
        &image_count.to_string(),
        &image_count.to_string(),
        &output::format_bytes(image_total_size),
        &format!("{} (0%)", output::format_bytes(0)),
    ]);

    let reclaim_pct = if box_total_size > 0 {
        (reclaimable_boxes as f64 / box_total_size as f64 * 100.0) as u64
    } else {
        0
    };

    table.add_row([
        "Boxes",
        &box_count.to_string(),
        &active_boxes.to_string(),
        &output::format_bytes(box_total_size),
        &format!(
            "{} ({reclaim_pct}%)",
            output::format_bytes(reclaimable_boxes)
        ),
    ]);

    let total_size = image_total_size + box_total_size;
    let total_reclaimable = reclaimable_boxes;
    let total_pct = if total_size > 0 {
        (total_reclaimable as f64 / total_size as f64 * 100.0) as u64
    } else {
        0
    };

    table.add_row([
        "Total",
        "",
        "",
        &output::format_bytes(total_size),
        &format!("{} ({total_pct}%)", output::format_bytes(total_reclaimable)),
    ]);

    println!("{table}");

    // Verbose: per-item details
    if args.verbose {
        println!();
        println!("Images:");
        let mut img_table = output::new_table(&["REPOSITORY", "SIZE"]);
        for image in &images {
            img_table.add_row([&image.reference, &output::format_bytes(image.size_bytes)]);
        }
        println!("{img_table}");

        println!();
        println!("Boxes:");
        let mut box_table = output::new_table(&["NAME", "STATUS", "SIZE"]);
        for b in &boxes {
            let size = dir_size(&b.box_dir);
            box_table.add_row([&b.name, &b.status, &output::format_bytes(size)]);
        }
        println!("{box_table}");
    }

    Ok(())
}

/// Calculate total size of a directory recursively.
fn dir_size(path: &std::path::Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                total += dir_size(&p);
            } else if let Ok(meta) = p.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_dir_size_empty() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(dir_size(tmp.path()), 0);
    }

    #[test]
    fn test_dir_size_files() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), "hello").unwrap(); // 5
        fs::write(tmp.path().join("b.txt"), "world!").unwrap(); // 6
        assert_eq!(dir_size(tmp.path()), 11);
    }

    #[test]
    fn test_dir_size_nested() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), "aaa").unwrap(); // 3
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("sub").join("b.txt"), "bb").unwrap(); // 2
        assert_eq!(dir_size(tmp.path()), 5);
    }

    #[test]
    fn test_dir_size_nonexistent() {
        let path = std::path::Path::new("/nonexistent/a3s_test_12345");
        assert_eq!(dir_size(path), 0);
    }
}
