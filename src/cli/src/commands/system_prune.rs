//! `a3s-box system-prune` command — Remove all unused data.
//!
//! Removes stopped boxes and unused images in one operation.

use std::collections::HashSet;
use std::path::Path;

use clap::Args;

use crate::image_usage::{self, ImagePruneMode, ImageReferenceScope};
use crate::output;
use crate::state::StateFile;

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
        println!("  - all created, stopped, and dead boxes");
        println!("  - all networks not used by at least one box");
        if args.all {
            println!("  - all images not used by active boxes");
            println!("  - all rootfs cache entries not used by active boxes");
            println!("  - interrupted image pull working directories");
        } else {
            println!("  - all dangling images");
        }
        println!();
        println!("Use --force to skip this prompt.");
        return Ok(());
    }

    let mut boxes_removed: usize = 0;
    let mut images_removed: usize = 0;
    let mut networks_removed: usize = 0;
    let mut rootfs_cache_entries_removed: usize = 0;
    let mut pull_temp_dirs_removed: usize = 0;
    let mut space_freed: u64 = 0;

    // Phase 1: Remove stopped/dead boxes
    let mut state = StateFile::load_default()?;
    let all_boxes = state.list(true);

    let to_remove: Vec<crate::state::BoxRecord> = all_boxes
        .iter()
        .filter(|r| is_prunable_box(r))
        .map(|record| (*record).clone())
        .collect();

    for record in &to_remove {
        if let Err(error) = crate::cleanup::cleanup_removed_box(record) {
            tracing::warn!(
                box_id = %record.id,
                error = %error,
                "Failed to clean system-pruned Box resources; preserving its state"
            );
            continue;
        }
        if state.remove(&record.id).is_ok() {
            boxes_removed += 1;
            println!("Removed box: {}", record.name);
        }
    }

    // Phase 2: Remove unused images
    // Reload state to get current active boxes after removal.
    let state = StateFile::load_default()?;
    let protected_images = active_image_references(&state);
    let prune_mode = image_prune_mode(args.all);

    let images_dir = super::images_dir();
    if images_dir.exists() {
        if let Ok(store) = super::open_image_store() {
            let all_images = store.list().await;
            let image_size_before = store.total_size().await;

            for image in &all_images {
                if image_usage::is_prunable_reference(
                    &image.reference,
                    &protected_images,
                    prune_mode,
                ) && store.remove(&image.reference).await.is_ok()
                {
                    images_removed += 1;
                    println!("Removed image: {}", image.reference);
                }
            }
            // Multiple references can share one content directory.  Account
            // for the actual content delta, not one image size per tag.
            space_freed = space_freed
                .saturating_add(image_size_before.saturating_sub(store.total_size().await));
        }
    }

    // Phase 3: Remove unused networks (mirrors `docker system prune`).
    // Reload state so freshly-removed boxes no longer count as network users.
    let state = StateFile::load_default()?;
    if let Ok(network_store) = a3s_box_runtime::NetworkStore::default_path() {
        let (removed, _errors) = super::network::prune_unused_networks(&network_store, &state);
        for name in &removed {
            networks_removed += 1;
            println!("Removed network: {name}");
        }
    }

    // Phase 4: `--all` also reclaims runtime caches that are not represented by
    // image-store records. Preserve any rootfs lower referenced by a remaining
    // live box and any pull directory whose owner process is still running.
    if args.all {
        let state = StateFile::load_default()?;
        let protected_rootfs = referenced_rootfs_cache_keys(&state);
        let cache_root = a3s_box_core::dirs_home().join("cache");
        let rootfs_result = prune_rootfs_caches(&cache_root, &protected_rootfs)?;
        rootfs_cache_entries_removed = rootfs_result.entries_removed;
        space_freed = space_freed.saturating_add(rootfs_result.bytes_freed);

        let pull_result = a3s_box_runtime::prune_stale_pull_temp_dirs(&images_dir)?;
        pull_temp_dirs_removed = pull_result.directories_removed;
        space_freed = space_freed.saturating_add(pull_result.bytes_freed);
    }

    println!();
    println!(
        "Removed {} box(es), {} image(s), {} network(s), {} rootfs cache entry(ies), {} interrupted pull dir(s), freed {}",
        boxes_removed,
        images_removed,
        networks_removed,
        rootfs_cache_entries_removed,
        pull_temp_dirs_removed,
        output::format_bytes(space_freed)
    );

    Ok(())
}

fn is_prunable_box(record: &crate::state::BoxRecord) -> bool {
    matches!(record.status.as_str(), "stopped" | "dead" | "created")
}

fn active_image_references(state: &StateFile) -> std::collections::HashSet<String> {
    image_usage::referenced_images(state, ImageReferenceScope::ActiveBoxes)
}

fn image_prune_mode(all: bool) -> ImagePruneMode {
    if all {
        ImagePruneMode::Unused
    } else {
        ImagePruneMode::Dangling
    }
}

fn referenced_rootfs_cache_keys(state: &StateFile) -> HashSet<String> {
    state
        .records()
        .iter()
        .filter_map(|record| {
            std::fs::read_to_string(record.box_dir.join(".rootfs-cache-key"))
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .collect()
}

fn prune_rootfs_caches(
    cache_root: &Path,
    protected: &HashSet<String>,
) -> Result<a3s_box_runtime::cache::RootfsPruneResult, Box<dyn std::error::Error>> {
    let mut result = a3s_box_runtime::cache::RootfsPruneResult::default();
    let directory_cache = cache_root.join("rootfs");
    if directory_cache.exists() {
        result.merge(
            a3s_box_runtime::cache::RootfsCache::new(&directory_cache)?
                .prune_all_protecting(protected)?,
        );
    }
    result.merge(a3s_box_runtime::cache::prune_apfs_rootfs_cache_all(
        &cache_root.join("rootfs-apfs-v2"),
        protected,
    )?);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::{make_record, setup_state};

    #[test]
    fn test_is_prunable_box_keeps_active_boxes() {
        assert!(!is_prunable_box(&make_record(
            "id-1",
            "running",
            "running",
            Some(1)
        )));
        assert!(!is_prunable_box(&make_record(
            "id-2",
            "paused",
            "paused",
            Some(1)
        )));
        assert!(is_prunable_box(&make_record(
            "id-3", "created", "created", None
        )));
        assert!(is_prunable_box(&make_record(
            "id-4", "stopped", "stopped", None
        )));
        assert!(is_prunable_box(&make_record("id-5", "dead", "dead", None)));
    }

    #[test]
    fn test_active_image_references_include_paused() {
        let mut running = make_record("id-1", "running", "running", Some(1));
        running.image = "alpine:latest".to_string();
        let mut paused = make_record("id-2", "paused", "paused", Some(1));
        paused.image = "redis:latest".to_string();
        let mut stopped = make_record("id-3", "stopped", "stopped", None);
        stopped.image = "nginx:latest".to_string();
        let (_tmp, state) = setup_state(vec![running, paused, stopped]);

        let used_images = active_image_references(&state);

        assert!(used_images.contains("alpine:latest"));
        assert!(used_images.contains("docker.io/library/alpine:latest"));
        assert!(used_images.contains("redis:latest"));
        assert!(!used_images.contains("nginx:latest"));
    }

    #[test]
    fn test_image_prune_mode_defaults_to_dangling() {
        assert_eq!(image_prune_mode(false), ImagePruneMode::Dangling);
        assert_eq!(image_prune_mode(true), ImagePruneMode::Unused);
    }

    #[test]
    fn test_rootfs_cache_references_follow_remaining_box_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let box_dir = tmp.path().join("boxes").join("active");
        std::fs::create_dir_all(&box_dir).unwrap();
        std::fs::write(box_dir.join(".rootfs-cache-key"), " live-key\n").unwrap();
        let mut record = make_record("id-1", "active", "running", Some(std::process::id()));
        record.box_dir = box_dir;
        let (_state_dir, state) = setup_state(vec![record]);

        assert_eq!(
            referenced_rootfs_cache_keys(&state),
            ["live-key".to_string()].into_iter().collect()
        );
    }

    #[test]
    fn test_prune_rootfs_caches_covers_directory_and_apfs_backends() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_root = tmp.path().join("cache");
        let directory_cache = cache_root.join("rootfs");
        let source = tmp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("payload"), b"rootfs").unwrap();
        let cache = a3s_box_runtime::cache::RootfsCache::new(&directory_cache).unwrap();
        cache.put("live", &source, "live").unwrap();
        cache.put("unused", &source, "unused").unwrap();

        let apfs_cache = cache_root.join("rootfs-apfs-v2");
        std::fs::create_dir_all(&apfs_cache).unwrap();
        std::fs::write(apfs_cache.join("live.sparseimage"), b"live").unwrap();
        std::fs::write(apfs_cache.join("unused.sparseimage"), b"unused").unwrap();

        let protected = ["live".to_string()].into_iter().collect();
        let result = prune_rootfs_caches(&cache_root, &protected).unwrap();

        assert_eq!(result.entries_removed, 2);
        assert!(result.bytes_freed > 0);
        assert!(cache.get("live").unwrap().is_some());
        assert!(cache.get("unused").unwrap().is_none());
        assert!(apfs_cache.join("live.sparseimage").exists());
        assert!(!apfs_cache.join("unused.sparseimage").exists());
    }
}
