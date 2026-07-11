//! Guest rootfs management module.
//!
//! This module handles preparation and management of guest rootfs for MicroVM instances.
//! The rootfs contains the minimal filesystem required to boot the guest agent.
//!
//! Two rootfs providers are available:
//! - `CopyProvider` — full recursive copy (works everywhere)
//! - `OverlayProvider` — Linux overlayfs mount (near-instant CoW)

mod builder;
mod layout;
pub(crate) mod overlay;
mod provider;

pub use builder::RootfsBuilder;
pub use layout::{GuestLayout, GUEST_WORKDIR};
pub use provider::{default_provider, CopyProvider, OverlayProvider, RootfsProvider};

use std::path::Path;

/// A temporarily attached persistent rootfs.
///
/// Dropping this guard detaches only mounts created by
/// [`attach_persistent_rootfs`]. An already mounted rootfs is left untouched.
pub struct AttachedRootfs {
    path: std::path::PathBuf,
    detach_on_drop: bool,
}

impl AttachedRootfs {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for AttachedRootfs {
    fn drop(&mut self) {
        if self.detach_on_drop {
            unmount_box_rootfs(&self.path);
        }
    }
}

/// Attach an existing platform-backed persistent rootfs for offline access.
///
/// Returns `None` when the box has no platform-specific backing image. This
/// never creates a new image, so callers cannot accidentally commit an empty
/// filesystem when a backing image is missing.
pub fn attach_persistent_rootfs(
    box_dir: &Path,
) -> a3s_box_core::error::Result<Option<AttachedRootfs>> {
    #[cfg(target_os = "macos")]
    {
        let image = box_dir.join("rootfs-apfs-v2.sparseimage");
        if !image.is_file() {
            return Ok(None);
        }
        let rootfs = box_dir.join("rootfs");
        let was_mounted = is_mountpoint(&rootfs);
        let path = provider::CaseSensitiveApfsProvider.prepare_empty(box_dir)?;
        Ok(Some(AttachedRootfs {
            path,
            detach_on_drop: !was_mounted,
        }))
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = box_dir;
        Ok(None)
    }
}

/// Unmount a box's overlayfs `merged` view — best-effort and idempotent.
///
/// Box teardown must release this mount BEFORE removing the box dir, or
/// `remove_dir_all` deletes *into* the live mount and fails with "Stale file
/// handle", leaking the mount. A restart re-mounts without unmounting first, so
/// the overlay can be stacked (mounted 2–3×); unmount in a bounded loop until
/// `merged` is no longer a mountpoint. No-op if it was never mounted.
pub fn unmount_box_overlay(merged: &Path) {
    for _ in 0..8 {
        if !is_mountpoint(merged) {
            break;
        }
        if overlay::overlay_unmount(merged).is_err() {
            break;
        }
    }
}

/// True if `path` is a mountpoint (its device id differs from its parent's).
#[cfg(unix)]
pub(crate) fn is_mountpoint(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(path), std::fs::metadata(path.join(".."))) {
        (Ok(here), Ok(parent)) => here.dev() != parent.dev(),
        _ => false,
    }
}

#[cfg(not(unix))]
pub(crate) fn is_mountpoint(_path: &Path) -> bool {
    false
}

/// Unmount a platform-specific writable rootfs mount.
pub fn unmount_box_rootfs(rootfs: &Path) {
    #[cfg(target_os = "macos")]
    {
        // The case-sensitive provider returns `<mount>/.a3s-rootfs`, keeping
        // APFS-created volume metadata outside the Linux tree. Accept either
        // that data path or the mountpoint itself at cleanup call sites.
        let mountpoint = if rootfs.file_name().is_some_and(|name| name == ".a3s-rootfs") {
            rootfs.parent().unwrap_or(rootfs)
        } else {
            rootfs
        };
        if !is_mountpoint(mountpoint) {
            return;
        }
        match std::process::Command::new("hdiutil")
            .arg("detach")
            .arg("-quiet")
            .arg(mountpoint)
            .status()
        {
            Ok(status) if status.success() => {}
            Ok(status) => tracing::warn!(
                path = %mountpoint.display(),
                ?status,
                "Failed to detach case-sensitive rootfs image"
            ),
            Err(error) => tracing::warn!(
                path = %mountpoint.display(),
                %error,
                "Failed to run hdiutil detach"
            ),
        }
    }

    #[cfg(not(target_os = "macos"))]
    let _ = rootfs;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_path_is_not_mountpoint() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");

        assert!(!is_mountpoint(&missing));
    }

    #[test]
    fn unmount_overlay_noops_for_non_mountpoint() {
        let temp = tempfile::tempdir().unwrap();
        let merged = temp.path().join("merged");
        std::fs::create_dir(&merged).unwrap();

        unmount_box_overlay(&merged);

        assert!(merged.exists());
    }
}
