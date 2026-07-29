//! Durable filesystem baseline used by `a3s-box diff`.

use std::collections::HashMap;
use std::path::{Component, Path};

use a3s_box_core::error::{BoxError, Result};
use serde::{Deserialize, Serialize};

/// Filename stored beside each box rootfs.
pub const DIFF_BASELINE_FILE: &str = "rootfs_snapshot.json";

/// Minimal file metadata needed to classify filesystem changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootfsFileInfo {
    pub size: u64,
    pub mode: u32,
    pub is_dir: bool,
}

/// Capture a rootfs tree while excluding runtime-owned control files.
pub fn walk_rootfs(root: &Path) -> Result<HashMap<String, RootfsFileInfo>> {
    let mut entries = HashMap::new();
    walk_recursive(root, root, &mut entries)?;
    Ok(entries)
}

/// Persist the first pristine baseline for a box without ever replacing it.
///
/// Runtime callers invoke this after all host-side rootfs preparation and before
/// the workload starts. Later starts and monitor recovery therefore preserve the
/// original generation's baseline instead of racing a fast container command.
pub fn create_diff_baseline_if_absent(box_dir: &Path, rootfs: &Path) -> Result<()> {
    let destination = box_dir.join(DIFF_BASELINE_FILE);
    if destination.exists() {
        return Ok(());
    }

    let entries = walk_rootfs(rootfs)?;
    let encoded = serde_json::to_vec(&entries).map_err(|error| {
        BoxError::BuildError(format!("failed to encode rootfs baseline: {error}"))
    })?;
    let temporary = box_dir.join(format!(
        ".{DIFF_BASELINE_FILE}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&temporary, encoded).map_err(BoxError::IoError)?;

    // hard_link installs the fully-written file only when the destination is
    // still absent. A concurrent lifecycle helper that won the race remains the
    // authority; unlike rename, this never overwrites its earlier baseline.
    let install = std::fs::hard_link(&temporary, &destination);
    let _ = std::fs::remove_file(&temporary);
    match install {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(BoxError::IoError(error)),
    }
}

fn walk_recursive(
    root: &Path,
    current: &Path,
    entries: &mut HashMap<String, RootfsFileInfo>,
) -> Result<()> {
    let directory = match std::fs::read_dir(current) {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(error) => return Err(BoxError::IoError(error)),
    };

    for entry in directory {
        let entry = entry.map_err(BoxError::IoError)?;
        let path = entry.path();
        let relative = match path.strip_prefix(root) {
            Ok(relative) => relative,
            Err(_) => continue,
        };
        if a3s_box_core::rootfs_metadata::is_runtime_internal_rootfs_path(relative) {
            continue;
        }

        // Do not follow symlinks while walking a container-controlled tree.
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => continue,
            Err(error) => return Err(BoxError::IoError(error)),
        };

        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::MetadataExt;
            metadata.mode()
        };
        #[cfg(not(unix))]
        let mode = 0;

        entries.insert(
            rootfs_path_string(relative),
            RootfsFileInfo {
                size: metadata.len(),
                mode,
                is_dir: metadata.is_dir(),
            },
        );
        if metadata.is_dir() {
            walk_recursive(root, &path, entries)?;
        }
    }
    Ok(())
}

fn rootfs_path_string(relative: &Path) -> String {
    let segments = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(segment) => Some(segment.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>();
    format!("/{}", segments.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_is_first_writer_wins_and_excludes_runtime_control_files() {
        let directory = tempfile::tempdir().unwrap();
        let box_dir = directory.path().join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(rootfs.join("root")).unwrap();
        std::fs::write(rootfs.join("root/original.txt"), b"original").unwrap();
        std::fs::write(rootfs.join(".a3s-box-exec.json"), b"runtime").unwrap();

        create_diff_baseline_if_absent(&box_dir, &rootfs).unwrap();
        std::fs::write(rootfs.join("root/later.txt"), b"later").unwrap();
        create_diff_baseline_if_absent(&box_dir, &rootfs).unwrap();

        let encoded = std::fs::read(box_dir.join(DIFF_BASELINE_FILE)).unwrap();
        let baseline: HashMap<String, RootfsFileInfo> = serde_json::from_slice(&encoded).unwrap();
        assert!(baseline.contains_key("/root/original.txt"));
        assert!(!baseline.contains_key("/root/later.txt"));
        assert!(!baseline.contains_key("/.a3s-box-exec.json"));
    }

    #[cfg(unix)]
    #[test]
    fn walk_does_not_follow_rootfs_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let outside = directory.path().join("outside");
        let rootfs = directory.path().join("rootfs");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::write(outside.join("secret"), b"secret").unwrap();
        std::os::unix::fs::symlink(&outside, rootfs.join("escape")).unwrap();

        let entries = walk_rootfs(&rootfs).unwrap();
        assert!(entries.contains_key("/escape"));
        assert!(!entries.contains_key("/escape/secret"));
    }
}
