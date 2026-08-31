//! Retained rootfs and snapshot cache markers and pruning.

use super::*;

#[cfg(target_os = "macos")]
pub(super) fn prune_apfs_rootfs_cache(
    cache_dir: &Path,
    max_entries: usize,
    max_allocated_bytes: u64,
    protected_key: &str,
) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    struct Entry {
        path: PathBuf,
        key: String,
        modified: std::time::SystemTime,
        allocated_bytes: u64,
    }

    let mut entries = Vec::new();
    for item in std::fs::read_dir(cache_dir)? {
        let item = item?;
        let path = item.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(key) = name.strip_suffix(".sparseimage") else {
            continue;
        };
        if key.starts_with('.') || !item.file_type()?.is_file() {
            continue;
        }
        let key = key.to_string();
        let metadata = item.metadata()?;
        entries.push(Entry {
            path,
            key,
            modified: metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
            // `len()` is the sparse image's 64 GiB virtual capacity. `blocks()`
            // reflects physical 512-byte blocks and is the bounded resource.
            allocated_bytes: metadata.blocks().saturating_mul(512),
        });
    }

    entries.sort_by_key(|entry| entry.modified);
    let mut count = entries.len();
    let mut allocated: u64 = entries.iter().map(|entry| entry.allocated_bytes).sum();
    for entry in entries {
        if count <= max_entries && allocated <= max_allocated_bytes {
            break;
        }
        if entry.key == protected_key {
            continue;
        }
        match std::fs::remove_file(&entry.path) {
            Ok(()) => {
                count = count.saturating_sub(1);
                allocated = allocated.saturating_sub(entry.allocated_bytes);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                count = count.saturating_sub(1);
                allocated = allocated.saturating_sub(entry.allocated_bytes);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Read the snapshot-restore copy-on-write overlay lower marker, if present and
/// non-empty. `snapshot restore` writes the snapshot's stored rootfs path here;
/// the runtime mounts it as a read-only overlay lower instead of copying the
/// rootfs, so all forks share one pristine lower and each writes to its own upper.
pub(super) fn snapshot_lower_dir(box_dir: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(box_dir.join(".snapshot-lower")).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

pub(super) fn retained_rootfs_cache_key(box_dir: &Path) -> Result<Option<String>> {
    let marker = box_dir.join(".rootfs-cache-key");
    let value = match std::fs::read_to_string(&marker) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(BoxError::StateError(format!(
                "Failed to read retained rootfs cache marker {}: {error}",
                marker.display()
            )))
        }
    };
    let key = value.trim();
    if key.len() != 64 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(BoxError::StateError(format!(
            "Retained rootfs cache marker is invalid for {}",
            box_dir.display()
        )));
    }
    Ok(Some(key.to_ascii_lowercase()))
}

#[cfg(any(unix, test))]
pub(super) fn require_snapshot_restore_rootfs(
    cache_key: Option<&str>,
    cached_path: Option<PathBuf>,
) -> Result<(&str, PathBuf)> {
    let cache_key = cache_key.ok_or_else(|| {
        BoxError::StateError(
            "snapshot restore is missing its exact rootfs cache identity".to_string(),
        )
    })?;
    let cached_path = cached_path.ok_or_else(|| {
        BoxError::StateError(format!(
            "snapshot restore rootfs cache entry {cache_key} is unavailable"
        ))
    })?;
    Ok((cache_key, cached_path))
}
