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

use std::path::{Path, PathBuf};

/// Read the exit code persisted by guest-init from the active writable rootfs.
///
/// Rootfs providers expose `/.a3s_exit_code` at different host paths: the
/// overlay upper directory on Linux, the copied rootfs fallback, or the private
/// data directory inside the case-sensitive APFS mount on macOS.
pub fn read_persisted_exit_code(box_dir: &Path) -> Option<i32> {
    let candidates = [
        box_dir.join("upper").join(".a3s_exit_code"),
        box_dir
            .join("rootfs")
            .join(".a3s-rootfs")
            .join(".a3s_exit_code"),
        box_dir.join("rootfs").join(".a3s_exit_code"),
    ];

    candidates.into_iter().find_map(|path| {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|contents| contents.trim().parse::<i32>().ok())
    })
}

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

/// Invalidate the last clean-shutdown metadata generation before launching a
/// box, retaining it at the one-shot replay path used by guest-init.
///
/// Overlay providers can expose the same entry through `merged` and `upper`.
/// Staging is idempotent when the canonical marker is already absent: an
/// existing replay marker is retained so a boot that failed before guest replay
/// can be retried safely.
pub fn stage_box_terminal_rootfs_metadata(box_dir: &Path) -> a3s_box_core::error::Result<()> {
    let attached = attach_persistent_rootfs(box_dir)?;
    let mut roots = Vec::<PathBuf>::new();
    if let Some(rootfs) = attached.as_ref() {
        roots.push(rootfs.path().to_path_buf());
    }
    roots.extend([
        box_dir.join("rootfs"),
        box_dir.join("upper"),
        box_dir.join("merged"),
    ]);
    roots.sort();
    roots.dedup();

    let mut existing_roots = Vec::new();
    for root in roots {
        match std::fs::symlink_metadata(&root) {
            Ok(_) => existing_roots.push(root),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    stage_metadata_roots(&existing_roots)?;
    Ok(())
}

fn stage_metadata_roots(roots: &[PathBuf]) -> std::io::Result<()> {
    for root in roots {
        a3s_box_core::rootfs_metadata::stage_terminal_rootfs_metadata_for_boot(root)?;
    }
    Ok(())
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
    fn persisted_exit_code_supports_each_rootfs_provider_layout() {
        for (relative, expected) in [
            ("upper/.a3s_exit_code", 17),
            ("rootfs/.a3s_exit_code", 23),
            ("rootfs/.a3s-rootfs/.a3s_exit_code", 29),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!("{expected}\n")).unwrap();

            assert_eq!(read_persisted_exit_code(temp.path()), Some(expected));
        }
    }

    #[test]
    fn persisted_exit_code_ignores_missing_or_invalid_files() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(read_persisted_exit_code(temp.path()), None);

        let path = temp.path().join("rootfs/.a3s_exit_code");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "not-an-exit-code").unwrap();
        assert_eq!(read_persisted_exit_code(temp.path()), None);
    }

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

    #[test]
    fn staging_is_idempotent_until_guest_replay_succeeds() {
        let root = tempfile::tempdir().unwrap();
        let terminal = root
            .path()
            .join(a3s_box_core::rootfs_metadata::ROOTFS_METADATA_PATH.trim_start_matches('/'));
        let previous = root.path().join(
            a3s_box_core::rootfs_metadata::PREVIOUS_ROOTFS_METADATA_PATH.trim_start_matches('/'),
        );
        std::fs::write(&terminal, b"clean generation").unwrap();

        stage_metadata_roots(&[root.path().to_path_buf()]).unwrap();
        stage_metadata_roots(&[root.path().to_path_buf()]).unwrap();

        assert!(!terminal.exists());
        assert_eq!(std::fs::read(previous).unwrap(), b"clean generation");
    }

    #[test]
    fn staging_one_candidate_never_discards_an_alias_replay() {
        let directory = tempfile::tempdir().unwrap();
        let merged = directory.path().join("merged");
        let upper = directory.path().join("upper");
        std::fs::create_dir_all(&merged).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        let terminal_name =
            a3s_box_core::rootfs_metadata::ROOTFS_METADATA_PATH.trim_start_matches('/');
        let previous_name =
            a3s_box_core::rootfs_metadata::PREVIOUS_ROOTFS_METADATA_PATH.trim_start_matches('/');
        std::fs::write(merged.join(terminal_name), b"clean generation").unwrap();
        // Models the view through `upper` immediately after the same overlay
        // entry was renamed through `merged`.
        std::fs::write(upper.join(previous_name), b"clean generation").unwrap();

        stage_metadata_roots(&[merged.clone(), upper.clone()]).unwrap();

        assert!(merged.join(previous_name).is_file());
        assert!(upper.join(previous_name).is_file());
    }
}
