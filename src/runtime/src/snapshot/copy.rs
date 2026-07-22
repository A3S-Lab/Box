//! Filesystem-safe Snapshot tree cloning and payload measurement.

use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::rootfs_metadata::{
    RootfsMetadataManifest, IMAGE_ROOTFS_METADATA_PATH, PREVIOUS_ROOTFS_METADATA_PATH,
    ROOTFS_METADATA_PATH,
};

pub(super) fn install_rootfs_metadata(
    rootfs: &Path,
    metadata: &RootfsMetadataManifest,
) -> Result<()> {
    metadata.validate().map_err(BoxError::OciImageError)?;
    for reserved in [
        IMAGE_ROOTFS_METADATA_PATH,
        ROOTFS_METADATA_PATH,
        PREVIOUS_ROOTFS_METADATA_PATH,
    ] {
        let path = rootfs.join(reserved.trim_start_matches('/'));
        match std::fs::symlink_metadata(&path) {
            Ok(existing) if existing.file_type().is_file() || existing.file_type().is_symlink() => {
                std::fs::remove_file(&path).map_err(|error| {
                    BoxError::CacheError(format!(
                        "Failed to replace snapshot rootfs metadata {}: {error}",
                        path.display()
                    ))
                })?;
            }
            Ok(_) => {
                return Err(BoxError::CacheError(format!(
                    "Snapshot rootfs metadata path is not a regular file: {}",
                    path.display()
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(BoxError::CacheError(format!(
                    "Failed to inspect snapshot rootfs metadata {}: {error}",
                    path.display()
                )))
            }
        }
    }

    let destination = rootfs.join(ROOTFS_METADATA_PATH.trim_start_matches('/'));
    let bytes = serde_json::to_vec(metadata).map_err(|error| {
        BoxError::SerializationError(format!(
            "Failed to encode snapshot rootfs metadata: {error}"
        ))
    })?;
    std::fs::write(&destination, bytes).map_err(|error| {
        BoxError::CacheError(format!(
            "Failed to write snapshot rootfs metadata {}: {error}",
            destination.display()
        ))
    })?;
    Ok(())
}

/// Recursively clone a rootfs without following symlinks. Linux Sandbox
/// snapshots preserve hardlink identity, ownership, modes, timestamps, and
/// xattrs. Unsupported special files fail closed instead of being opened as
/// regular files (which would block forever for a FIFO).
#[cfg(unix)]
pub(super) fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(src).map_err(snapshot_copy_error(src, "inspect"))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(BoxError::CacheError(format!(
            "Snapshot rootfs source is not a directory: {}",
            src.display()
        )));
    }
    let mut state = SnapshotCopyState::default();
    copy_snapshot_directory(src, dst, &metadata, &mut state)
}

#[cfg(unix)]
#[derive(Default)]
struct SnapshotCopyState {
    hardlinks: std::collections::HashMap<(u64, u64), PathBuf>,
}

#[cfg(unix)]
fn copy_snapshot_directory(
    src: &Path,
    dst: &Path,
    metadata: &std::fs::Metadata,
    state: &mut SnapshotCopyState,
) -> Result<()> {
    std::fs::create_dir(dst).map_err(snapshot_copy_error(dst, "create directory"))?;
    let mut entries: Vec<_> = std::fs::read_dir(src)
        .map_err(snapshot_copy_error(src, "read directory"))?
        .collect::<std::result::Result<_, _>>()
        .map_err(snapshot_copy_error(src, "read directory entry"))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        copy_snapshot_entry(&entry.path(), &dst.join(entry.file_name()), state)?;
    }
    finish_snapshot_copy(src, dst, metadata, false)
}

#[cfg(unix)]
fn copy_snapshot_entry(src: &Path, dst: &Path, state: &mut SnapshotCopyState) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::symlink_metadata(src).map_err(snapshot_copy_error(src, "inspect"))?;
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        return copy_snapshot_directory(src, dst, &metadata, state);
    }
    if file_type.is_symlink() {
        let target = std::fs::read_link(src).map_err(snapshot_copy_error(src, "read symlink"))?;
        std::os::unix::fs::symlink(&target, dst)
            .map_err(snapshot_copy_error(dst, "create symlink"))?;
        return finish_snapshot_copy(src, dst, &metadata, true);
    }
    if !file_type.is_file() {
        return Err(BoxError::CacheError(format!(
            "Snapshot rootfs contains unsupported special file {} ({})",
            src.display(),
            special_file_kind(&file_type)
        )));
    }

    let hardlink_key = (metadata.dev(), metadata.ino());
    if metadata.nlink() > 1 {
        if let Some(existing) = state.hardlinks.get(&hardlink_key) {
            std::fs::hard_link(existing, dst)
                .map_err(snapshot_copy_error(dst, "preserve hardlink"))?;
            return Ok(());
        }
    }
    crate::cache::layer_cache::copy_file_cow(src, dst)
        .map_err(snapshot_copy_error(dst, "copy regular file"))?;
    finish_snapshot_copy(src, dst, &metadata, false)?;
    if metadata.nlink() > 1 {
        state.hardlinks.insert(hardlink_key, dst.to_path_buf());
    }
    Ok(())
}

#[cfg(unix)]
fn finish_snapshot_copy(
    src: &Path,
    dst: &Path,
    metadata: &std::fs::Metadata,
    symlink: bool,
) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let current = std::fs::symlink_metadata(dst).map_err(snapshot_copy_error(dst, "inspect"))?;
    if current.uid() != metadata.uid() || current.gid() != metadata.gid() {
        let path = std::ffi::CString::new(dst.as_os_str().as_bytes()).map_err(|_| {
            BoxError::CacheError(format!(
                "Snapshot path contains a NUL byte: {}",
                dst.display()
            ))
        })?;
        if unsafe { libc::lchown(path.as_ptr(), metadata.uid(), metadata.gid()) } != 0 {
            return Err(BoxError::CacheError(format!(
                "Failed to preserve ownership on {}: {}",
                dst.display(),
                std::io::Error::last_os_error()
            )));
        }
    }
    if !symlink {
        std::fs::set_permissions(
            dst,
            std::fs::Permissions::from_mode(metadata.mode() & 0o7777),
        )
        .map_err(snapshot_copy_error(dst, "preserve mode"))?;
        filetime::set_file_times(
            dst,
            filetime::FileTime::from_last_access_time(metadata),
            filetime::FileTime::from_last_modification_time(metadata),
        )
        .map_err(snapshot_copy_error(dst, "preserve timestamps"))?;
    } else {
        filetime::set_symlink_file_times(
            dst,
            filetime::FileTime::from_last_access_time(metadata),
            filetime::FileTime::from_last_modification_time(metadata),
        )
        .map_err(snapshot_copy_error(dst, "preserve symlink timestamps"))?;
    }
    copy_snapshot_xattrs(src, dst)?;
    Ok(())
}

#[cfg(unix)]
fn copy_snapshot_xattrs(src: &Path, dst: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    for name in xattr::list(src).map_err(snapshot_copy_error(src, "list xattrs"))? {
        let raw_name = name.as_bytes();
        if raw_name.starts_with(b"trusted.overlay.") || raw_name.starts_with(b"user.overlay.") {
            return Err(BoxError::CacheError(format!(
                "Snapshot rootfs contains reserved overlay xattr {:?} at {}",
                name,
                src.display()
            )));
        }
        let value = xattr::get(src, &name)
            .map_err(snapshot_copy_error(src, "read xattr"))?
            .ok_or_else(|| {
                BoxError::CacheError(format!(
                    "Snapshot xattr {:?} disappeared from {} while quiesced",
                    name,
                    src.display()
                ))
            })?;
        xattr::set(dst, &name, &value).map_err(snapshot_copy_error(dst, "write xattr"))?;
    }
    Ok(())
}

#[cfg(unix)]
fn special_file_kind(file_type: &std::fs::FileType) -> &'static str {
    use std::os::unix::fs::FileTypeExt;

    if file_type.is_fifo() {
        "fifo"
    } else if file_type.is_socket() {
        "socket"
    } else if file_type.is_char_device() {
        "character device"
    } else if file_type.is_block_device() {
        "block device"
    } else {
        "unknown"
    }
}

#[cfg(unix)]
fn snapshot_copy_error<'a>(
    path: &'a Path,
    operation: &'static str,
) -> impl FnOnce(std::io::Error) -> BoxError + 'a {
    move |error| {
        BoxError::CacheError(format!(
            "Failed to {operation} Snapshot path {}: {error}",
            path.display()
        ))
    }
}

/// Windows keeps the portable snapshot copy because the rootfs cache helper
/// intentionally rejects Windows symlinks. Managed Sandbox snapshots are
/// Linux-only, but the pre-existing VM Snapshot store remains cross-platform.
#[cfg(windows)]
pub(super) fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).map_err(|e| {
        BoxError::CacheError(format!(
            "Failed to create directory {}: {}",
            dst.display(),
            e
        ))
    })?;

    for entry in std::fs::read_dir(src).map_err(|e| {
        BoxError::CacheError(format!("Failed to read directory {}: {}", src.display(), e))
    })? {
        let entry = entry
            .map_err(|e| BoxError::CacheError(format!("Failed to read directory entry: {}", e)))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let file_type = entry.file_type().map_err(|e| {
            BoxError::CacheError(format!(
                "Failed to read file type for {}: {}",
                src_path.display(),
                e
            ))
        })?;

        if file_type.is_symlink() {
            copy_symlink(&src_path, &dst_path)?;
        } else if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path).map_err(|e| {
                BoxError::CacheError(format!(
                    "Failed to copy {} → {}: {}",
                    src_path.display(),
                    dst_path.display(),
                    e
                ))
            })?;
        }
    }

    Ok(())
}

#[cfg(windows)]
fn copy_symlink(src: &Path, dst: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;

    // `Path::metadata` follows the link and cannot classify a dangling link.
    // The directory attribute belongs to the reparse point itself, so this
    // remains valid even when the target is outside the snapshot or missing.
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    let metadata = std::fs::symlink_metadata(src).map_err(|e| {
        BoxError::CacheError(format!(
            "Failed to inspect symlink {} without following it: {}",
            src.display(),
            e
        ))
    })?;
    if !metadata.file_type().is_symlink() {
        return Err(BoxError::CacheError(format!(
            "Snapshot path changed while copying symlink: {}",
            src.display()
        )));
    }

    let target = std::fs::read_link(src).map_err(|e| {
        BoxError::CacheError(format!("Failed to read symlink {}: {}", src.display(), e))
    })?;

    let is_dir = metadata.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0;
    let result = if is_dir {
        std::os::windows::fs::symlink_dir(&target, dst)
    } else {
        std::os::windows::fs::symlink_file(&target, dst)
    };
    result.map_err(|e| {
        BoxError::CacheError(format!(
            "Failed to create symlink {} → {}: {}",
            dst.display(),
            target.display(),
            e
        ))
    })?;

    Ok(())
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::os::windows::fs::MetadataExt;

    const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

    fn symlink_or_skip(target: &Path, link: &Path, directory: bool) -> bool {
        let result = if directory {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        };
        match result {
            Ok(()) => true,
            Err(error) if error.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD) => {
                eprintln!(
                    "skipping Windows snapshot symlink test: creating {} requires symlink privilege",
                    link.display()
                );
                false
            }
            Err(error) => panic!(
                "failed to create Windows test symlink {} -> {}: {error}",
                link.display(),
                target.display()
            ),
        }
    }

    #[test]
    fn copies_external_file_and_dangling_directory_symlinks_without_following_targets() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("src");
        let dst = temp.path().join("dst");
        let outside = temp.path().join("outside-file");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(&outside, b"outside").unwrap();

        let external_target = Path::new(r"..\outside-file");
        let missing_target = Path::new(r"..\missing-directory");
        if !symlink_or_skip(external_target, &src.join("external-file"), false) {
            return;
        }
        if !symlink_or_skip(missing_target, &src.join("missing-dir"), true) {
            return;
        }

        copy_dir_recursive(&src, &dst).unwrap();

        for (name, target, directory) in [
            ("external-file", external_target, false),
            ("missing-dir", missing_target, true),
        ] {
            let copied = dst.join(name);
            let metadata = std::fs::symlink_metadata(&copied).unwrap();
            assert!(metadata.file_type().is_symlink());
            assert_eq!(
                metadata.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0,
                directory,
                "{name} must preserve its file-vs-directory symlink type"
            );
            assert_eq!(std::fs::read_link(copied).unwrap(), target);
        }
        assert_eq!(std::fs::read(outside).unwrap(), b"outside");
    }
}

/// Calculate Snapshot payload bytes without following symlinks or counting one
/// hardlinked inode more than once.
pub(super) fn dir_size(path: &Path) -> Result<u64> {
    #[cfg(unix)]
    {
        let mut seen = std::collections::HashSet::new();
        dir_size_unix(path, &mut seen)
    }
    #[cfg(not(unix))]
    {
        dir_size_portable(path)
    }
}

#[cfg(unix)]
fn dir_size_unix(path: &Path, seen: &mut std::collections::HashSet<(u64, u64)>) -> Result<u64> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::symlink_metadata(path).map_err(snapshot_copy_error(path, "size"))?;
    if metadata.file_type().is_symlink() {
        return Ok(metadata.len());
    }
    if metadata.file_type().is_file() {
        return Ok(if seen.insert((metadata.dev(), metadata.ino())) {
            metadata.len()
        } else {
            0
        });
    }
    if !metadata.file_type().is_dir() {
        return Err(BoxError::CacheError(format!(
            "Snapshot contains unsupported special file while measuring {}",
            path.display()
        )));
    }
    let mut total = 0_u64;
    for entry in std::fs::read_dir(path).map_err(snapshot_copy_error(path, "measure directory"))? {
        total = total.saturating_add(dir_size_unix(
            &entry
                .map_err(snapshot_copy_error(path, "measure directory entry"))?
                .path(),
            seen,
        )?);
    }
    Ok(total)
}

#[cfg(not(unix))]
fn dir_size_portable(path: &Path) -> Result<u64> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        BoxError::CacheError(format!(
            "Failed to inspect Snapshot path {}: {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_symlink() || metadata.file_type().is_file() {
        return Ok(metadata.len());
    }
    if !metadata.file_type().is_dir() {
        return Err(BoxError::CacheError(format!(
            "Snapshot contains unsupported special file while measuring {}",
            path.display()
        )));
    }
    let mut total = 0_u64;
    for entry in std::fs::read_dir(path).map_err(|error| {
        BoxError::CacheError(format!(
            "Failed to measure Snapshot directory {}: {error}",
            path.display()
        ))
    })? {
        total = total.saturating_add(dir_size_portable(
            &entry.map_err(BoxError::IoError)?.path(),
        )?);
    }
    Ok(total)
}
