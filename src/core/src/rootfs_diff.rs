//! Host-side rootfs snapshots used by `a3s-box diff`.
//!
//! The baseline must be captured after all runtime-owned rootfs preparation and
//! before the guest starts. Keeping the format and walker in core lets the VM
//! runtime establish that ordering while the CLI performs later comparisons.

use std::collections::HashMap;
use std::io;
use std::path::{Component, Path};

/// Minimal file metadata retained in a rootfs diff baseline.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileInfo {
    pub size: u64,
    pub mode: u32,
    pub is_dir: bool,
}

/// Walk a rootfs and collect metadata keyed by guest-absolute path.
pub fn walk_dir(root: &Path) -> io::Result<HashMap<String, FileInfo>> {
    let mut map = HashMap::new();
    walk_recursive(root, root, &mut map)?;
    Ok(map)
}

fn walk_recursive(
    root: &Path,
    current: &Path,
    map: &mut HashMap<String, FileInfo>,
) -> io::Result<()> {
    let entries = match std::fs::read_dir(current) {
        Ok(entries) => entries,
        // A concurrently disappearing or unreadable entry should not make a
        // best-effort baseline prevent the VM from booting.
        Err(_) => return Ok(()),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map(rootfs_path_string)
            .unwrap_or_default();

        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };

        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::MetadataExt;
            metadata.mode()
        };
        #[cfg(not(unix))]
        let mode = 0u32;

        map.insert(
            relative,
            FileInfo {
                size: metadata.len(),
                mode,
                is_dir: metadata.is_dir(),
            },
        );

        if metadata.is_dir() {
            walk_recursive(root, &path, map)?;
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

/// Capture one rootfs baseline in the JSON format consumed by the CLI.
pub fn create_snapshot(rootfs_dir: &Path, snapshot_path: &Path) -> io::Result<()> {
    let map = walk_dir(rootfs_dir)?;
    let json = serde_json::to_vec(&map).map_err(io::Error::other)?;
    std::fs::write(snapshot_path, json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_records_guest_absolute_paths_and_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let rootfs = directory.path().join("rootfs");
        std::fs::create_dir_all(rootfs.join("etc")).unwrap();
        std::fs::write(rootfs.join("etc/hostname"), b"box\n").unwrap();
        let snapshot = directory.path().join("baseline.json");

        create_snapshot(&rootfs, &snapshot).unwrap();

        let decoded: HashMap<String, FileInfo> =
            serde_json::from_slice(&std::fs::read(snapshot).unwrap()).unwrap();
        assert!(decoded["/etc"].is_dir);
        assert_eq!(decoded["/etc/hostname"].size, 4);
    }
}
