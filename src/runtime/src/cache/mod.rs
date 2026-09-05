//! Cache module for cold start optimization.
//!
//! Provides two caching layers:
//! - `LayerCache`: Content-addressed cache for extracted OCI layers
//! - `RootfsCache`: Cache for fully-built rootfs directories

use std::path::{Component, Path};

use a3s_box_core::error::{BoxError, Result};

pub mod layer_cache;
pub mod rootfs_cache;

pub use layer_cache::LayerCache;
pub use rootfs_cache::{prune_apfs_rootfs_cache_all, RootfsCache, RootfsPruneResult};

/// Validate a caller-provided cache key before it is appended to a host path.
///
/// Cache keys are identifiers, not relative paths. Keeping this check in one
/// place prevents a malformed OCI digest or API key from escaping the cache
/// directory through `..`, path separators, or platform-specific prefixes.
pub(crate) fn validate_cache_key(key: &str, kind: &str) -> Result<()> {
    if key.is_empty() || key.contains('\0') || key.contains('/') || key.contains('\\') {
        return Err(BoxError::CacheError(format!(
            "Invalid {kind} cache key: expected a single path component"
        )));
    }

    let mut components = Path::new(key).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(BoxError::CacheError(format!(
            "Invalid {kind} cache key: expected a single path component"
        )));
    }

    Ok(())
}

/// Return whether `path` is an actual directory, rather than a symlink to one.
///
/// Cache paths are later handed to mount/copy code, so following a symlink here
/// would turn a local cache entry into an arbitrary host path.
pub(crate) fn is_real_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
        .unwrap_or(false)
}

/// Return whether `path` is an actual regular file, rather than a symlink.
pub(crate) fn is_regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
        .unwrap_or(false)
}

/// Remove one cache path without following a symlink at the path itself.
pub(crate) fn remove_path_no_follow(path: &Path) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(BoxError::CacheError(format!(
                "Failed to inspect cached path {}: {error}",
                path.display()
            )))
        }
    };
    let removed = if metadata.is_dir() && !metadata.file_type().is_symlink() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match removed {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(BoxError::CacheError(format!(
            "Failed to remove cached path {}: {error}",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_keys_must_be_single_path_components() {
        for key in ["", ".", "..", "../escape", "nested/key", "nested\\key"] {
            assert!(validate_cache_key(key, "test").is_err(), "key={key:?}");
        }

        for key in ["sha256_abc123", "rootfs-key", "键"] {
            assert!(validate_cache_key(key, "test").is_ok(), "key={key:?}");
        }
    }
}
