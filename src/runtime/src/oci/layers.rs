//! OCI layer extraction utilities.
//!
//! Handles extraction of OCI image layers (gzip, zstd, or uncompressed tar).

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::rootfs_metadata::{
    RootfsEntryKind, RootfsMetadataEntry, RootfsMetadataManifest, IMAGE_ROOTFS_METADATA_PATH,
    IMAGE_ROOTFS_METADATA_TEMP_PATH, PREVIOUS_ROOTFS_METADATA_PATH, ROOTFS_METADATA_PATH,
    ROOTFS_METADATA_TEMP_PATH,
};
use base64::Engine;
use flate2::read::GzDecoder;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use tar::Archive;

/// Extract a single OCI layer (tar.gz) to target directory.
///
/// # Arguments
///
/// * `layer_path` - Path to the layer tarball (*.tar.gz)
/// * `target_dir` - Directory to extract files into
///
/// # Errors
///
/// Returns error if:
/// - Layer file doesn't exist
/// - Decompression fails
/// - Extraction fails
/// - Target directory cannot be created
pub fn extract_layer(layer_path: &Path, target_dir: &Path) -> Result<()> {
    // Bound total decompressed output so a compression-bomb layer (a few MB that
    // expands to hundreds of GB of zeros) cannot fill the host disk during pull.
    // Generous default; tune with A3S_BOX_MAX_LAYER_BYTES.
    let max_layer_bytes =
        super::limited_reader::cap_from_env("A3S_BOX_MAX_LAYER_BYTES", 16 * 1024 * 1024 * 1024);
    extract_layer_with_cap(layer_path, target_dir, max_layer_bytes, false)
}

/// Extract a layer and retain the Linux ownership encoded in its tar headers.
///
/// Rootless macOS extraction cannot apply arbitrary uid/gid values to APFS.
/// The generated rootfs-private manifest is replayed by guest-init before any
/// nested filesystems are mounted.
pub(crate) fn extract_layer_with_metadata(layer_path: &Path, target_dir: &Path) -> Result<()> {
    let max_layer_bytes =
        super::limited_reader::cap_from_env("A3S_BOX_MAX_LAYER_BYTES", 16 * 1024 * 1024 * 1024);
    extract_layer_with_cap(layer_path, target_dir, max_layer_bytes, true)
}

fn extract_layer_with_cap(
    layer_path: &Path,
    target_dir: &Path,
    max_layer_bytes: u64,
    track_metadata: bool,
) -> Result<()> {
    // Validate layer exists
    if !layer_path.exists() {
        return Err(BoxError::OciImageError(format!(
            "Layer file not found: {}",
            layer_path.display()
        )));
    }

    // Create target directory
    std::fs::create_dir_all(target_dir).map_err(|e| {
        BoxError::OciImageError(format!(
            "Failed to create target directory {}: {}",
            target_dir.display(),
            e
        ))
    })?;

    // Open layer file
    let mut file = File::open(layer_path).map_err(|e| {
        BoxError::OciImageError(format!(
            "Failed to open layer file {}: {}",
            layer_path.display(),
            e
        ))
    })?;

    // Detect the layer's compression from its magic bytes — OCI layers are gzip
    // (1f 8b), zstd (28 b5 2f fd, e.g. buildkit/nerdctl `--compression zstd`), or
    // an uncompressed tar. Peek, rewind, then pick the matching decoder; relying
    // on the media type alone would miss layers stored without one.
    let mut magic = [0u8; 4];
    let read = file.read(&mut magic).map_err(|e| {
        BoxError::OciImageError(format!(
            "Failed to read layer header {}: {e}",
            layer_path.display()
        ))
    })?;
    file.seek(SeekFrom::Start(0)).map_err(|e| {
        BoxError::OciImageError(format!(
            "Failed to rewind layer {}: {e}",
            layer_path.display()
        ))
    })?;

    let decoder: Box<dyn Read> = if read >= 2 && magic[0] == 0x1f && magic[1] == 0x8b {
        Box::new(GzDecoder::new(file))
    } else if read >= 4 && magic == [0x28, 0xb5, 0x2f, 0xfd] {
        Box::new(zstd::stream::read::Decoder::new(file).map_err(|e| {
            BoxError::OciImageError(format!(
                "Failed to init zstd decoder for {}: {e}",
                layer_path.display()
            ))
        })?)
    } else {
        // Uncompressed tar (some registries / `--compression none`).
        Box::new(file)
    };

    let decoder = super::limited_reader::LimitedReader::new(decoder, max_layer_bytes);

    // Extract the tar archive, applying OCI whiteout semantics so files deleted
    // in an upper layer do not reappear from lower layers:
    //   - `.wh.<name>`    deletes the sibling `<name>` already materialized
    //   - `.wh..wh..opq`  clears all prior contents of its parent directory
    // Whiteout markers themselves are never written into the rootfs. Normal
    // entries are delegated to `unpack_in`, preserving the same symlink /
    // hardlink / permission / mtime fidelity that `unpack` provides.
    let mut archive = Archive::new(decoder);
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(true);
    archive.set_overwrite(true);
    #[cfg(unix)]
    {
        archive.set_unpack_xattrs(true);
        // Restore the uid/gid stamped in the layer tar headers so `COPY --chown`
        // ownership (and non-root ownership baked into base-image layers) is
        // preserved in the rootfs instead of collapsing to root. tar performs a
        // chown for this, which only succeeds as root — gate on euid 0 so a
        // non-privileged extraction does not fail with EPERM.
        if unsafe { libc::geteuid() } == 0 {
            archive.set_preserve_ownerships(true);
        }
    }

    let mut metadata = if track_metadata {
        load_image_metadata(target_dir)?
    } else {
        BTreeMap::new()
    };

    // Windows grants administrators SeCreateSymbolicLinkPrivilege but normally
    // leaves it disabled in the process token. `tar` creates OCI links through
    // `std::os::windows::fs::symlink_file`, which otherwise fails even though
    // the service identity already owns the privilege. Keep the process-wide
    // token mutation serialized and scoped to extraction; Developer Mode still
    // works when the token does not contain the privilege.
    #[cfg(windows)]
    let windows_symlink_guard =
        a3s_box_core::windows_symlink::WindowsSymlinkPrivilegeGuard::acquire();
    #[cfg(windows)]
    let windows_symlink_privilege_enabled = windows_symlink_guard.assigned_privilege_enabled();

    let entries = archive
        .entries()
        .map_err(|e| BoxError::OciImageError(format!("Failed to read layer entries: {e}")))?;
    let mut parent_write_guards = LayerParentWriteGuards::default();
    let mut layer_directory_modes = BTreeMap::new();

    for entry in entries {
        let mut entry = entry
            .map_err(|e| BoxError::OciImageError(format!("Failed to read layer entry: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| BoxError::OciImageError(format!("Invalid layer entry path: {e}")))?
            .into_owned();

        // Defensively reject path-traversal entries (`unpack_in` also guards this).
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            tracing::warn!(path = %path.display(), "Skipping layer entry with '..' component");
            continue;
        }

        let normalized = normalize_layer_path(&path).ok_or_else(|| {
            BoxError::OciImageError(format!("Invalid layer entry path: {}", path.display()))
        })?;
        if let Some(reserved) = reserved_metadata_path(&normalized) {
            return Err(BoxError::OciImageError(format!(
                "OCI layer contains reserved internal path {reserved}"
            )));
        }

        // A lower OCI layer may leave a directory read-only (for example Go's
        // module cache uses 0555) while an upper layer adds another child. On a
        // rootless host, temporarily open the nearest existing parent through
        // metadata finalization. The guard then restores the mode encoded by
        // the newest layer entry (or the original mode when this layer did not
        // modify the directory).
        parent_write_guards.prepare(target_dir, &normalized)?;

        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        if file_name == ".wh..wh..opq" {
            // Opaque directory marker: discard everything already extracted into
            // the parent directory from lower layers, keeping the directory.
            // Resolve the parent WITHIN the rootfs first: a malicious layer can
            // extract an absolute symlink as the parent, and following it here
            // would wipe a host directory OUTSIDE the extraction target.
            if let Some(parent) = path.parent() {
                if let Some(dir) = resolve_within(target_dir, parent) {
                    if let Ok(read) = std::fs::read_dir(&dir) {
                        for child in read.flatten() {
                            remove_path(&child.path());
                        }
                    }
                } else {
                    tracing::warn!(parent = %parent.display(), "Skipping opaque whiteout: parent escapes the rootfs");
                }
            }
            if track_metadata {
                let parent = normalize_layer_path(path.parent().unwrap_or_else(|| Path::new("")))
                    .ok_or_else(|| {
                    BoxError::OciImageError("Invalid opaque whiteout path".to_string())
                })?;
                remove_metadata_descendants(&mut metadata, &parent, false);
            }
            continue;
        }

        if let Some(victim_name) = file_name.strip_prefix(".wh.") {
            let victim = normalize_layer_path(
                &path
                    .parent()
                    .unwrap_or_else(|| Path::new(""))
                    .join(victim_name),
            )
            .ok_or_else(|| BoxError::OciImageError("Invalid whiteout path".to_string()))?;
            if let Some(reserved) = reserved_metadata_path(&victim) {
                return Err(BoxError::OciImageError(format!(
                    "OCI layer whiteouts reserved internal path {reserved}"
                )));
            }
            // Whiteout marker: remove the named sibling from a lower layer. Resolve
            // the parent within the rootfs so a symlinked parent cannot redirect the
            // deletion to a host file outside the extraction target.
            if let Some(parent) = path.parent() {
                if let Some(dir) = resolve_within(target_dir, parent) {
                    remove_path(&dir.join(victim_name));
                } else {
                    tracing::warn!(parent = %parent.display(), "Skipping whiteout: parent escapes the rootfs");
                }
            }
            if track_metadata {
                remove_metadata_descendants(&mut metadata, &victim, true);
            }
            continue;
        }

        if entry.header().entry_type() == tar::EntryType::Symlink {
            prepare_symlink_destination(target_dir, &path)?;
        } else if entry.header().entry_type().is_hard_link() {
            prepare_hardlink_destination(target_dir, &path)?;
        }
        reject_overlay_private_xattrs(&mut entry, &path)?;

        let desired = if track_metadata {
            Some(metadata_from_header(&entry, &normalized)?)
        } else {
            None
        };
        let directory_mode = if entry.header().entry_type().is_dir() {
            Some(entry.header().mode().map_err(|error| {
                BoxError::OciImageError(format!(
                    "Invalid directory mode at {}: {error}",
                    path.display()
                ))
            })?)
        } else {
            None
        };
        #[cfg(windows)]
        let entry_is_symlink = entry.header().entry_type().is_symlink();
        let unpacked = entry.unpack_in(target_dir).map_err(|e| {
            #[cfg(windows)]
            if entry_is_symlink && windows_symlink_creation_was_denied(&e) {
                let diagnostic = a3s_box_core::windows_symlink::denial_diagnostic(
                    windows_symlink_privilege_enabled,
                );
                return BoxError::OciImageError(format!(
                    "Failed to extract layer to {}: Windows cannot preserve OCI symlink {}: \
                     {diagnostic}; flattening the link would corrupt the image. See \
                     https://learn.microsoft.com/windows/advanced-settings/developer-mode",
                    target_dir.display(),
                    path.display(),
                ));
            }
            // Surface the underlying cause (e.g. the LimitedReader's size-cap
            // error) — tar's wrapper Display alone would just say "failed to
            // unpack <path>" and hide a decompression-bomb abort from the operator.
            let cause = std::error::Error::source(&e)
                .map(|src| format!("{e}: {src}"))
                .unwrap_or_else(|| e.to_string());
            BoxError::OciImageError(format!(
                "Failed to extract layer to {}: {cause}",
                target_dir.display(),
            ))
        })?;
        if unpacked {
            if let Some(mode) = directory_mode {
                layer_directory_modes.insert(normalized.clone(), mode);
            }
        }
        if track_metadata && unpacked {
            if let Some(desired) = desired {
                // Descendants can only remain when this layer replaces a
                // lower-layer directory with a non-directory. Scanning the
                // entire metadata map for every ordinary file turns large
                // image extraction into quadratic work.
                if metadata_descendant_cleanup_needed(&metadata, &normalized, desired.kind) {
                    remove_metadata_descendants(&mut metadata, &normalized, false);
                }
                metadata.insert(normalized, desired);
            }
        }
    }

    if track_metadata {
        // Metadata collection and activation must traverse every image
        // directory and write the root manifest even when the newest layer
        // leaves one of those directories non-traversable or read-only.
        parent_write_guards.prepare_metadata_directories(target_dir, &metadata)?;
        let mut mode_overrides = parent_write_guards.original_directory_modes();
        mode_overrides.extend(
            layer_directory_modes
                .iter()
                .map(|(path, mode)| (path.clone(), *mode)),
        );
        finalize_image_metadata_with_mode_overrides(target_dir, &mut metadata, &mode_overrides)?;
    }
    let mut directory_modes: BTreeMap<_, _> = metadata
        .iter()
        .filter(|(_, entry)| entry.kind == RootfsEntryKind::Directory)
        .map(|(path, entry)| (path.clone(), entry.mode))
        .collect();
    directory_modes.extend(layer_directory_modes);
    parent_write_guards.restore(&directory_modes)?;

    tracing::debug!(
        layer = %layer_path.display(),
        target = %target_dir.display(),
        "Extracted OCI layer"
    );

    Ok(())
}

#[cfg(unix)]
fn reject_overlay_private_xattrs<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    path: &Path,
) -> Result<()> {
    const PAX_XATTR_PREFIX: &[u8] = b"SCHILY.xattr.";
    let Some(extensions) = entry.pax_extensions().map_err(|error| {
        BoxError::OciImageError(format!(
            "Failed to inspect extended attributes for {}: {error}",
            path.display()
        ))
    })?
    else {
        return Ok(());
    };

    for extension in extensions {
        let extension = extension.map_err(|error| {
            BoxError::OciImageError(format!(
                "Invalid PAX extended attribute for {}: {error}",
                path.display()
            ))
        })?;
        let Some(name) = extension.key_bytes().strip_prefix(PAX_XATTR_PREFIX) else {
            continue;
        };
        if name.starts_with(b"trusted.overlay.") || name.starts_with(b"user.overlay.") {
            return Err(BoxError::OciImageError(format!(
                "OCI layer entry {} contains reserved overlayfs metadata",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn reject_overlay_private_xattrs<R: Read>(
    _entry: &mut tar::Entry<'_, R>,
    _path: &Path,
) -> Result<()> {
    Ok(())
}

fn image_metadata_relative_path() -> PathBuf {
    PathBuf::from(IMAGE_ROOTFS_METADATA_PATH.trim_start_matches('/'))
}

fn terminal_metadata_relative_path() -> PathBuf {
    PathBuf::from(ROOTFS_METADATA_PATH.trim_start_matches('/'))
}

fn previous_metadata_relative_path() -> PathBuf {
    PathBuf::from(PREVIOUS_ROOTFS_METADATA_PATH.trim_start_matches('/'))
}

fn image_metadata_temp_relative_path() -> PathBuf {
    PathBuf::from(IMAGE_ROOTFS_METADATA_TEMP_PATH.trim_start_matches('/'))
}

fn terminal_metadata_temp_relative_path() -> PathBuf {
    PathBuf::from(ROOTFS_METADATA_TEMP_PATH.trim_start_matches('/'))
}

fn reserved_metadata_path(path: &Path) -> Option<&'static str> {
    if path.starts_with(image_metadata_relative_path()) {
        Some(IMAGE_ROOTFS_METADATA_PATH)
    } else if path.starts_with(terminal_metadata_relative_path()) {
        Some(ROOTFS_METADATA_PATH)
    } else if path.starts_with(previous_metadata_relative_path()) {
        Some(PREVIOUS_ROOTFS_METADATA_PATH)
    } else if path.starts_with(image_metadata_temp_relative_path()) {
        Some(IMAGE_ROOTFS_METADATA_TEMP_PATH)
    } else if path.starts_with(terminal_metadata_temp_relative_path()) {
        Some(ROOTFS_METADATA_TEMP_PATH)
    } else {
        None
    }
}

fn normalize_layer_path(path: &Path) -> Option<PathBuf> {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => normalized.push(name),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(normalized)
}

fn metadata_from_header<R: Read>(
    entry: &tar::Entry<'_, R>,
    path: &Path,
) -> Result<RootfsMetadataEntry> {
    let entry_type = entry.header().entry_type();
    let (kind, link_target_base64) = if entry_type.is_dir() {
        (RootfsEntryKind::Directory, None)
    } else if entry_type.is_symlink() {
        let target = entry
            .link_name()
            .map_err(|error| BoxError::OciImageError(format!("Invalid symlink target: {error}")))?
            .ok_or_else(|| BoxError::OciImageError("Missing symlink target".to_string()))?;
        (
            RootfsEntryKind::Symlink,
            Some(base64::engine::general_purpose::STANDARD.encode(guest_path_bytes(&target))),
        )
    } else if entry_type.is_file() || entry_type.is_hard_link() {
        (RootfsEntryKind::Regular, None)
    } else {
        return Err(BoxError::OciImageError(format!(
            "Unsupported OCI layer entry type at {}",
            path.display()
        )));
    };
    let path_base64 =
        base64::engine::general_purpose::STANDARD.encode(archive_metadata_path_bytes(path));
    Ok(RootfsMetadataEntry {
        path_base64,
        kind,
        mode: entry.header().mode().map_err(|error| {
            BoxError::OciImageError(format!("Invalid mode at {}: {error}", path.display()))
        })?,
        uid: entry.header().uid().map_err(|error| {
            BoxError::OciImageError(format!("Invalid uid at {}: {error}", path.display()))
        })?,
        gid: entry.header().gid().map_err(|error| {
            BoxError::OciImageError(format!("Invalid gid at {}: {error}", path.display()))
        })?,
        mtime: entry.header().mtime().map_err(|error| {
            BoxError::OciImageError(format!("Invalid mtime at {}: {error}", path.display()))
        })?,
        size: entry.header().size().map_err(|error| {
            BoxError::OciImageError(format!("Invalid size at {}: {error}", path.display()))
        })?,
        link_target_base64,
    })
}

/// Encode a rootfs path with Linux separators regardless of the host OS.
///
/// The manifest is consumed inside a Linux guest, so serializing a Windows
/// `PathBuf` directly would turn `./usr/bin` into `.\\usr\\bin` and describe a
/// different Linux filename.
fn archive_metadata_path_bytes(path: &Path) -> Vec<u8> {
    let mut encoded = vec![b'.'];
    for component in path.components() {
        if let std::path::Component::Normal(name) = component {
            encoded.push(b'/');
            encoded.extend_from_slice(name.as_encoded_bytes());
        }
    }
    encoded
}

fn guest_path_bytes(path: &Path) -> Vec<u8> {
    let bytes = path.as_os_str().as_encoded_bytes();
    #[cfg(windows)]
    {
        bytes
            .iter()
            .map(|byte| if *byte == b'\\' { b'/' } else { *byte })
            .collect()
    }
    #[cfg(not(windows))]
    {
        bytes.to_vec()
    }
}

fn remove_metadata_descendants(
    metadata: &mut BTreeMap<PathBuf, RootfsMetadataEntry>,
    path: &Path,
    include_path: bool,
) {
    metadata.retain(|candidate, _| {
        !(candidate.starts_with(path) && (include_path || candidate != path))
    });
}

fn metadata_descendant_cleanup_needed(
    metadata: &BTreeMap<PathBuf, RootfsMetadataEntry>,
    path: &Path,
    replacement: RootfsEntryKind,
) -> bool {
    if replacement == RootfsEntryKind::Directory {
        return false;
    }

    metadata
        .get(path)
        .is_some_and(|existing| existing.kind == RootfsEntryKind::Directory)
        || metadata
            .range((
                std::ops::Bound::Excluded(path.to_path_buf()),
                std::ops::Bound::Unbounded,
            ))
            .next()
            .is_some_and(|(candidate, _)| candidate.starts_with(path))
}

fn load_image_metadata(target_dir: &Path) -> Result<BTreeMap<PathBuf, RootfsMetadataEntry>> {
    let path = crate::oci::rootfs::resolve_guest_file_path(
        target_dir,
        IMAGE_ROOTFS_METADATA_PATH.trim_start_matches('/'),
    )?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => {
            return Err(BoxError::OciImageError(format!(
                "Failed to read image metadata {}: {error}",
                path.display()
            )))
        }
    };
    let manifest: RootfsMetadataManifest = serde_json::from_slice(&bytes).map_err(|error| {
        BoxError::OciImageError(format!(
            "Invalid image metadata {}: {error}",
            path.display()
        ))
    })?;
    manifest.validate().map_err(BoxError::OciImageError)?;
    let mut result = BTreeMap::new();
    for entry in manifest.entries {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&entry.path_base64)
            .map_err(|error| BoxError::OciImageError(format!("Invalid metadata path: {error}")))?;
        let archive_path = PathBuf::from(os_string_from_encoded_bytes(raw));
        let relative = normalize_layer_path(&archive_path)
            .ok_or_else(|| BoxError::OciImageError("Unsafe path in image metadata".to_string()))?;
        if reserved_metadata_path(&relative).is_some() || result.insert(relative, entry).is_some() {
            return Err(BoxError::OciImageError(
                "Duplicate or reserved path in image metadata".to_string(),
            ));
        }
    }
    Ok(result)
}

pub(crate) fn finalize_rootfs_metadata(target_dir: &Path) -> Result<()> {
    let mut metadata = load_image_metadata(target_dir)?;
    let mut directory_guards = LayerParentWriteGuards::default();
    directory_guards.prepare_metadata_directories(target_dir, &metadata)?;
    let mode_overrides = directory_guards.original_directory_modes();
    finalize_image_metadata_with_mode_overrides(target_dir, &mut metadata, &mode_overrides)?;
    let directory_modes = metadata
        .iter()
        .filter(|(_, entry)| entry.kind == RootfsEntryKind::Directory)
        .map(|(path, entry)| (path.clone(), entry.mode))
        .collect();
    directory_guards.restore(&directory_modes)?;
    prepare_rootless_metadata_replay(target_dir, &metadata)
}

fn prepare_rootless_metadata_replay(
    target_dir: &Path,
    metadata: &BTreeMap<PathBuf, RootfsMetadataEntry>,
) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        if unsafe { libc::geteuid() } == 0 {
            return Ok(());
        }
        // virtiofs stores synthetic uid/gid as filesystem metadata. A guest
        // cannot update a host-owned read-only inode; directories additionally
        // need execute permission for traversal. Guest-init restores the exact
        // manifest mode before launching any container process.
        for (relative, entry) in metadata {
            let required_mode = if entry.kind == RootfsEntryKind::Directory {
                0o300
            } else {
                0o200
            };
            if entry.kind == RootfsEntryKind::Symlink || entry.mode & required_mode == required_mode
            {
                continue;
            }
            let relative_path = relative.to_str().ok_or_else(|| {
                BoxError::OciImageError(format!(
                    "Metadata replay path is not UTF-8: {}",
                    relative.display()
                ))
            })?;
            let target = if entry.kind == RootfsEntryKind::Directory {
                crate::oci::rootfs::resolve_guest_directory_path(target_dir, relative_path)?
            } else {
                crate::oci::rootfs::resolve_guest_file_path(target_dir, relative_path)?
            };
            let current = std::fs::symlink_metadata(&target).map_err(|error| {
                BoxError::OciImageError(format!(
                    "Failed to prepare metadata replay for {}: {error}",
                    target.display()
                ))
            })?;
            std::fs::set_permissions(
                &target,
                std::fs::Permissions::from_mode(
                    (current.mode() & 0o7777)
                        | if entry.kind == RootfsEntryKind::Directory {
                            0o300
                        } else {
                            0o200
                        },
                ),
            )
            .map_err(|error| {
                BoxError::OciImageError(format!(
                    "Failed to prepare metadata replay for {}: {error}",
                    target.display()
                ))
            })?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (target_dir, metadata);
    }
    Ok(())
}

#[cfg(test)]
fn finalize_image_metadata(
    target_dir: &Path,
    metadata: &mut BTreeMap<PathBuf, RootfsMetadataEntry>,
) -> Result<()> {
    finalize_image_metadata_with_mode_overrides(target_dir, metadata, &BTreeMap::new())
}

fn finalize_image_metadata_with_mode_overrides(
    target_dir: &Path,
    metadata: &mut BTreeMap<PathBuf, RootfsMetadataEntry>,
    mode_overrides: &BTreeMap<PathBuf, u32>,
) -> Result<()> {
    let mut final_entries = BTreeMap::new();
    collect_final_metadata(
        target_dir,
        target_dir,
        Path::new(""),
        metadata,
        mode_overrides,
        &mut final_entries,
    )?;
    let manifest = RootfsMetadataManifest::new(final_entries.into_values().collect());
    let destination_relative = IMAGE_ROOTFS_METADATA_PATH.trim_start_matches('/');
    let temporary_relative = ".a3s_image_metadata_v1.json.tmp";
    let bytes = serde_json::to_vec(&manifest).map_err(|error| {
        BoxError::OciImageError(format!("Failed to encode image metadata: {error}"))
    })?;
    let temporary =
        crate::oci::rootfs::replace_guest_file_no_follow(target_dir, temporary_relative, bytes)?;
    let destination =
        crate::oci::rootfs::remove_guest_entry_no_follow(target_dir, destination_relative)?;
    std::fs::rename(&temporary, &destination).map_err(|error| {
        BoxError::OciImageError(format!(
            "Failed to activate image metadata {}: {error}",
            destination.display()
        ))
    })?;
    *metadata = manifest
        .entries
        .into_iter()
        .filter_map(|entry| decode_metadata_key(&entry).map(|key| (key, entry)))
        .collect();
    Ok(())
}

fn decode_metadata_key(entry: &RootfsMetadataEntry) -> Option<PathBuf> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(&entry.path_base64)
        .ok()?;
    normalize_layer_path(Path::new(&os_string_from_encoded_bytes(raw)))
}

fn os_string_from_encoded_bytes(raw: Vec<u8>) -> std::ffi::OsString {
    // Every metadata manifest is produced and consumed on the same host. The
    // bytes therefore use this platform's `OsStr` encoding and can be restored
    // losslessly, including non-UTF-8 Unix paths and Windows WTF-8 paths.
    unsafe { std::ffi::OsString::from_encoded_bytes_unchecked(raw) }
}

fn collect_final_metadata(
    root: &Path,
    source: &Path,
    relative: &Path,
    desired: &BTreeMap<PathBuf, RootfsMetadataEntry>,
    mode_overrides: &BTreeMap<PathBuf, u32>,
    output: &mut BTreeMap<PathBuf, RootfsMetadataEntry>,
) -> Result<()> {
    if reserved_metadata_path(relative).is_some()
        || relative == Path::new(".a3s_image_metadata_v1.json.tmp")
        || relative == Path::new(".a3s_rootfs_metadata_v1.json.tmp")
        || relative == Path::new(".a3s_rootfs_metadata_v1.previous.json")
    {
        return Ok(());
    }
    let filesystem = std::fs::symlink_metadata(source).map_err(|error| {
        BoxError::OciImageError(format!("Failed to inspect {}: {error}", source.display()))
    })?;
    let file_type = filesystem.file_type();
    let previous = desired.get(relative);
    let (kind, link_target_base64) = if file_type.is_dir() {
        (RootfsEntryKind::Directory, None)
    } else if file_type.is_file() {
        (RootfsEntryKind::Regular, None)
    } else if file_type.is_symlink() {
        let target = std::fs::read_link(source).map_err(|error| {
            BoxError::OciImageError(format!("Failed to read {}: {error}", source.display()))
        })?;
        let target = previous
            .filter(|entry| entry.kind == RootfsEntryKind::Symlink)
            .and_then(|entry| entry.link_target_base64.clone())
            .unwrap_or_else(|| {
                base64::engine::general_purpose::STANDARD.encode(guest_path_bytes(&target))
            });
        (RootfsEntryKind::Symlink, Some(target))
    } else {
        return Ok(());
    };
    let previous_same_kind = previous.filter(|entry| entry.kind == kind);
    #[cfg(unix)]
    let (mode, mtime, size) = {
        use std::os::unix::fs::MetadataExt;
        (
            mode_overrides
                .get(relative)
                .copied()
                .unwrap_or_else(|| filesystem.mode()),
            filesystem.mtime().max(0) as u64,
            filesystem.size(),
        )
    };
    #[cfg(not(unix))]
    let (mode, mtime, size) = previous_same_kind
        .map(|entry| (entry.mode, entry.mtime, entry.size))
        .unwrap_or_else(|| {
            (
                if file_type.is_dir() { 0o755 } else { 0o644 },
                0,
                filesystem.len(),
            )
        });
    let entry = RootfsMetadataEntry {
        path_base64: base64::engine::general_purpose::STANDARD
            .encode(archive_metadata_path_bytes(relative)),
        kind,
        mode,
        uid: previous_same_kind.map_or(0, |entry| entry.uid),
        gid: previous_same_kind.map_or(0, |entry| entry.gid),
        mtime,
        size,
        link_target_base64,
    };
    output.insert(relative.to_path_buf(), entry);
    if file_type.is_dir() {
        let mut children: Vec<_> = std::fs::read_dir(source)
            .map_err(|error| {
                BoxError::OciImageError(format!("Failed to read {}: {error}", source.display()))
            })?
            .collect::<std::result::Result<_, _>>()
            .map_err(|error| BoxError::OciImageError(format!("Failed to read entry: {error}")))?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            collect_final_metadata(
                root,
                &child.path(),
                &relative.join(child.file_name()),
                desired,
                mode_overrides,
                output,
            )?;
        }
    }
    let _ = root;
    Ok(())
}

#[cfg(windows)]
const WINDOWS_SYMLINK_ACCESS_DENIED_ERROR: i32 = 5;

#[cfg(windows)]
const WINDOWS_SYMLINK_PRIVILEGE_ERROR: i32 = 1314;

#[cfg(windows)]
fn windows_symlink_creation_was_denied(error: &std::io::Error) -> bool {
    [
        WINDOWS_SYMLINK_ACCESS_DENIED_ERROR,
        WINDOWS_SYMLINK_PRIVILEGE_ERROR,
    ]
    .into_iter()
    .any(|code| error_chain_has_raw_os_error(error, code))
}

#[cfg(windows)]
fn error_chain_has_raw_os_error(error: &std::io::Error, code: i32) -> bool {
    if error.raw_os_error() == Some(code) {
        return true;
    }
    // tar 0.4 wraps symlink errors in a new `io::Error` containing only a
    // formatted message, so the raw code is no longer available in its source
    // chain. Match the exact rendered OS-code suffix as the final fallback.
    if error.to_string().contains(&format!("(os error {code})")) {
        return true;
    }
    let mut source = std::error::Error::source(error);
    while let Some(current) = source {
        if current
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.raw_os_error() == Some(code))
            || current.to_string().contains(&format!("(os error {code})"))
        {
            return true;
        }
        source = current.source();
    }
    false
}

fn prepare_symlink_destination(target_dir: &Path, path: &Path) -> Result<()> {
    let Some(name) = path.file_name() else {
        return Ok(());
    };
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let Some(parent) = resolve_within_or_base(target_dir, parent) else {
        tracing::warn!(parent = %parent.display(), "Skipping symlink destination preparation: parent escapes the rootfs");
        return Ok(());
    };
    let candidate = parent.join(name);
    let Ok(metadata) = std::fs::symlink_metadata(&candidate) else {
        return Ok(());
    };
    if metadata.is_dir() {
        std::fs::remove_dir_all(&candidate).map_err(|e| {
            BoxError::OciImageError(format!(
                "Failed to replace directory {} with symlink from layer: {}",
                candidate.display(),
                e
            ))
        })?;
    }
    Ok(())
}

fn prepare_hardlink_destination(target_dir: &Path, path: &Path) -> Result<()> {
    let Some(name) = path.file_name() else {
        return Ok(());
    };
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let Some(parent) = resolve_within_or_base(target_dir, parent) else {
        tracing::warn!(parent = %parent.display(), "Skipping hardlink destination preparation: parent escapes the rootfs");
        return Ok(());
    };
    let candidate = parent.join(name);
    let Ok(metadata) = std::fs::symlink_metadata(&candidate) else {
        return Ok(());
    };
    let result = if metadata.is_dir() {
        std::fs::remove_dir_all(&candidate)
    } else {
        std::fs::remove_file(&candidate)
    };
    result.map_err(|error| {
        BoxError::OciImageError(format!(
            "Failed to replace {} with hardlink from layer: {error}",
            candidate.display()
        ))
    })
}

#[derive(Default)]
struct LayerParentWriteGuards {
    restore: BTreeMap<PathBuf, (PathBuf, std::fs::Permissions)>,
}

impl LayerParentWriteGuards {
    fn prepare(&mut self, target_dir: &Path, path: &Path) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            if unsafe { libc::geteuid() } == 0 {
                return Ok(());
            }

            let canonical_target = target_dir.canonicalize().map_err(|error| {
                BoxError::OciImageError(format!(
                    "Failed to resolve layer extraction root {}: {error}",
                    target_dir.display()
                ))
            })?;

            // `unpack_in` creates missing intermediate directories itself. Walk
            // up to the closest parent that already exists, resolving symlinks
            // inside the rootfs and refusing to chmod anything outside it.
            let requested_parent = path.parent().unwrap_or_else(|| Path::new(""));
            'retry_after_opening_ancestor: loop {
                let mut relative = requested_parent;
                loop {
                    if let Some(parent) = resolve_within_or_base(target_dir, relative) {
                        let metadata = std::fs::symlink_metadata(&parent).map_err(|error| {
                            BoxError::OciImageError(format!(
                                "Failed to inspect layer parent {}: {error}",
                                parent.display()
                            ))
                        })?;
                        if !metadata.is_dir() || metadata.permissions().mode() & 0o300 == 0o300 {
                            return Ok(());
                        }

                        let original = metadata.permissions();
                        let physical_relative = parent
                            .strip_prefix(&canonical_target)
                            .map_err(|_| {
                                BoxError::OciImageError(format!(
                                    "Resolved layer parent {} escapes extraction root {}",
                                    parent.display(),
                                    canonical_target.display()
                                ))
                            })?
                            .to_path_buf();
                        self.restore
                            .entry(parent.clone())
                            .or_insert_with(|| (physical_relative, original.clone()));
                        std::fs::set_permissions(
                            &parent,
                            std::fs::Permissions::from_mode((original.mode() & 0o7777) | 0o300),
                        )
                        .map_err(|error| {
                            BoxError::OciImageError(format!(
                                "Failed to prepare layer parent {} for extraction: {error}",
                                parent.display()
                            ))
                        })?;

                        // Opening an ancestor may make a deeper, existing
                        // parent resolvable. Retry from the requested parent;
                        // if the remainder is absent, the now-writable nearest
                        // ancestor is sufficient for `unpack_in` to create it.
                        if relative != requested_parent {
                            continue 'retry_after_opening_ancestor;
                        }
                        return Ok(());
                    }

                    let Some(ancestor) = relative.parent() else {
                        return Ok(());
                    };
                    relative = ancestor;
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (target_dir, path);
            Ok(())
        }
    }

    fn prepare_metadata_directories(
        &mut self,
        target_dir: &Path,
        metadata: &BTreeMap<PathBuf, RootfsMetadataEntry>,
    ) -> Result<()> {
        for (relative, entry) in metadata {
            if entry.kind == RootfsEntryKind::Directory {
                // `prepare` opens a path's parent. A synthetic child therefore
                // asks it to open the directory represented by this manifest
                // entry without ever touching or creating the child itself.
                self.prepare(target_dir, &relative.join(".a3s-metadata-access"))?;
            }
        }
        Ok(())
    }

    fn original_directory_modes(&self) -> BTreeMap<PathBuf, u32> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            self.restore
                .values()
                .map(|(relative, permissions)| (relative.clone(), permissions.mode() & 0o7777))
                .collect()
        }
        #[cfg(not(unix))]
        {
            BTreeMap::new()
        }
    }

    fn restore(&mut self, layer_directory_modes: &BTreeMap<PathBuf, u32>) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut entries = std::mem::take(&mut self.restore)
                .into_iter()
                .collect::<Vec<_>>();
            entries.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
            let mut first_error = None;
            for (path, (relative, original)) in entries {
                let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                    continue;
                };
                if !metadata.is_dir() {
                    continue;
                }
                let permissions = layer_directory_modes
                    .get(&relative)
                    .map(|mode| std::fs::Permissions::from_mode(*mode))
                    .unwrap_or(original);
                if let Err(error) = std::fs::set_permissions(&path, permissions) {
                    first_error.get_or_insert_with(|| {
                        BoxError::OciImageError(format!(
                        "Failed to restore directory permissions after layer extraction at {}: {error}",
                        path.display()
                        ))
                    });
                }
            }
            if let Some(error) = first_error {
                return Err(error);
            }
        }
        #[cfg(not(unix))]
        let _ = layer_directory_modes;
        Ok(())
    }
}

impl Drop for LayerParentWriteGuards {
    fn drop(&mut self) {
        if self.restore.is_empty() {
            return;
        }
        if let Err(error) = self.restore(&BTreeMap::new()) {
            tracing::warn!(
                error = %error,
                "Failed to restore directory permissions after aborted layer extraction"
            );
        }
    }
}

/// Resolve `rel` beneath `target_dir`, following symlinks, returning the real
/// path ONLY if it stays inside `target_dir`.
///
/// A malicious layer can extract an absolute symlink (e.g. `esc -> /etc`) and
/// then a whiteout whose parent is that symlink; without this guard the
/// hand-rolled whiteout deletion would follow it and remove host files OUTSIDE
/// the extraction target. Returns `None` when the parent does not exist or
/// resolves outside the rootfs (caller skips + warns). Intra-rootfs symlinks
/// are allowed — the image may already mutate its own files; only escapes past
/// `target_dir` are blocked.
fn resolve_within(target_dir: &Path, rel: &Path) -> Option<PathBuf> {
    if rel.as_os_str().is_empty() {
        return target_dir.canonicalize().ok();
    }
    resolve_within_or_base(target_dir, rel)
}

fn resolve_within_or_base(target_dir: &Path, rel: &Path) -> Option<PathBuf> {
    let base = target_dir.canonicalize().ok()?;
    if rel.as_os_str().is_empty() {
        return Some(base);
    }
    let resolved = base.join(rel).canonicalize().ok()?;
    resolved.starts_with(&base).then_some(resolved)
}

/// Remove a file or directory tree for an applied whiteout, ignoring a missing
/// target. Uses `symlink_metadata` so a symlink is removed as a link, not
/// followed into a lower layer.
fn remove_path(path: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    let result = if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    if let Err(e) = result {
        tracing::warn!(path = %path.display(), error = %e, "Failed to apply whiteout deletion");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[cfg(windows)]
    #[test]
    fn windows_symlink_denial_classifies_access_and_privilege_errors() {
        for code in [
            WINDOWS_SYMLINK_ACCESS_DENIED_ERROR,
            WINDOWS_SYMLINK_PRIVILEGE_ERROR,
        ] {
            assert!(windows_symlink_creation_was_denied(
                &std::io::Error::from_raw_os_error(code)
            ));
            assert!(windows_symlink_creation_was_denied(&std::io::Error::other(
                format!("wrapped Windows symlink failure (os error {code})")
            )));
        }
        assert!(!windows_symlink_creation_was_denied(
            &std::io::Error::from_raw_os_error(206)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_layer_extraction_temporarily_enables_an_assigned_symlink_privilege() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use tar::Builder;

        let temp_dir = TempDir::new().unwrap();
        let layer = temp_dir.path().join("symlink.tar.gz");
        let target = temp_dir.path().join("rootfs");
        let file = File::create(&layer).unwrap();
        let mut builder = Builder::new(GzEncoder::new(file, Compression::default()));
        let mut target_header = tar::Header::new_gnu();
        target_header.set_size(7);
        target_header.set_mode(0o644);
        target_header.set_uid(0);
        target_header.set_gid(0);
        target_header.set_cksum();
        builder
            .append_data(&mut target_header, "target", b"payload".as_slice())
            .unwrap();
        let mut link_header = tar::Header::new_gnu();
        link_header.set_entry_type(tar::EntryType::Symlink);
        link_header.set_size(0);
        link_header.set_mode(0o777);
        link_header.set_uid(0);
        link_header.set_gid(0);
        builder
            .append_link(&mut link_header, "link", "target")
            .unwrap();
        builder.finish().unwrap();
        drop(builder);

        match extract_layer_with_metadata(&layer, &target) {
            Ok(()) => {
                let link = target.join("link");
                assert!(fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink());
                assert_eq!(fs::read_link(link).unwrap(), PathBuf::from("target"));
            }
            Err(error)
                if error.to_string().contains("ERROR_ACCESS_DENIED (5)")
                    || error
                        .to_string()
                        .contains("ERROR_PRIVILEGE_NOT_HELD (1314)") =>
            {
                eprintln!(
                    "skipping Windows symlink extraction test: the test identity has no symlink capability"
                );
            }
            Err(error) => panic!("Windows layer extraction failed unexpectedly: {error}"),
        }
    }

    #[test]
    fn test_extract_layer_creates_target_directory() {
        let temp_dir = TempDir::new().unwrap();
        let layer_path = temp_dir.path().join("layer.tar.gz");
        let target_dir = temp_dir.path().join("extracted");

        // Create a minimal tar.gz file
        create_test_layer(&layer_path, &[("test.txt", b"hello")]);

        // Extract layer
        extract_layer(&layer_path, &target_dir).unwrap();

        // Verify target directory was created
        assert!(target_dir.exists());
        assert!(target_dir.is_dir());
    }

    #[test]
    fn test_extract_layer_extracts_files() {
        let temp_dir = TempDir::new().unwrap();
        let layer_path = temp_dir.path().join("layer.tar.gz");
        let target_dir = temp_dir.path().join("extracted");

        // Create layer with test files
        create_test_layer(
            &layer_path,
            &[("file1.txt", b"content1"), ("dir/file2.txt", b"content2")],
        );

        // Extract layer
        extract_layer(&layer_path, &target_dir).unwrap();

        // Verify files were extracted
        assert!(target_dir.join("file1.txt").exists());
        assert!(target_dir.join("dir/file2.txt").exists());

        // Verify content
        let content1 = fs::read_to_string(target_dir.join("file1.txt")).unwrap();
        assert_eq!(content1, "content1");

        let content2 = fs::read_to_string(target_dir.join("dir/file2.txt")).unwrap();
        assert_eq!(content2, "content2");
    }

    #[test]
    fn test_extract_layer_nonexistent_file() {
        let temp_dir = TempDir::new().unwrap();
        let layer_path = temp_dir.path().join("nonexistent.tar.gz");
        let target_dir = temp_dir.path().join("extracted");

        // Try to extract non-existent layer
        let result = extract_layer(&layer_path, &target_dir);

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Layer file not found"));
    }

    #[test]
    fn test_extract_layer_multiple_layers_to_same_target() {
        let temp_dir = TempDir::new().unwrap();
        let layer1_path = temp_dir.path().join("layer1.tar.gz");
        let layer2_path = temp_dir.path().join("layer2.tar.gz");
        let target_dir = temp_dir.path().join("extracted");

        // Create two layers
        create_test_layer(&layer1_path, &[("base.txt", b"base content")]);
        create_test_layer(&layer2_path, &[("app.txt", b"app content")]);

        // Extract both layers to same target
        extract_layer(&layer1_path, &target_dir).unwrap();
        extract_layer(&layer2_path, &target_dir).unwrap();

        // Verify both files exist
        assert!(target_dir.join("base.txt").exists());
        assert!(target_dir.join("app.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn tracked_extraction_writes_into_readonly_directory_from_lower_layer() {
        use std::os::unix::fs::PermissionsExt;

        if unsafe { libc::geteuid() } == 0 {
            return;
        }

        let temp_dir = TempDir::new().unwrap();
        let layer1 = temp_dir.path().join("readonly-parent.tar.gz");
        let layer2 = temp_dir.path().join("readonly-child.tar.gz");
        let layer3 = temp_dir.path().join("child-before-parent.tar.gz");
        let target = temp_dir.path().join("rootfs");

        create_directory_entries_test_layer(
            &layer1,
            &[("go/pkg/mod", 0o444), ("go/pkg/mod/example", 0o444)],
        );
        create_test_layer(&layer2, &[("go/pkg/mod/example/child.txt", b"payload")]);
        create_child_then_directory_test_layer(
            &layer3,
            "go/pkg/mod/example",
            "go/pkg/mod/example/later.txt",
            b"later",
            0o555,
        );

        extract_layer_with_metadata(&layer1, &target).unwrap();
        let locked_ancestor = target.join("go/pkg/mod");
        assert_eq!(
            fs::metadata(&locked_ancestor).unwrap().permissions().mode() & 0o777,
            0o444
        );

        extract_layer_with_metadata(&layer2, &target).unwrap();
        assert_eq!(
            fs::metadata(&locked_ancestor).unwrap().permissions().mode() & 0o777,
            0o444,
            "temporary access must restore every restrictive ancestor"
        );

        // A later layer may put a child before an explicit directory entry. The
        // explicit entry is still the final source of truth for the directory.
        extract_layer_with_metadata(&layer3, &target).unwrap();
        let manifest = load_image_metadata(&target).unwrap();
        assert_eq!(manifest[Path::new("go/pkg/mod")].mode & 0o777, 0o444);
        assert_eq!(
            manifest[Path::new("go/pkg/mod/example")].mode & 0o777,
            0o555
        );

        // Open the ancestor only for host-side assertions and TempDir cleanup.
        fs::set_permissions(&locked_ancestor, fs::Permissions::from_mode(0o755)).unwrap();
        let parent = target.join("go/pkg/mod/example");
        assert_eq!(
            fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o555
        );
        assert_eq!(fs::read(parent.join("child.txt")).unwrap(), b"payload");
        assert_eq!(fs::read(parent.join("later.txt")).unwrap(), b"later");

        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn readonly_parent_reached_through_symlink_keeps_physical_metadata_mode() {
        use std::os::unix::fs::PermissionsExt;

        if unsafe { libc::geteuid() } == 0 {
            return;
        }

        let temp_dir = TempDir::new().unwrap();
        let directory_layer = temp_dir.path().join("directory.tar.gz");
        let symlink_layer = temp_dir.path().join("symlink.tar.gz");
        let child_layer = temp_dir.path().join("child.tar.gz");
        let target = temp_dir.path().join("rootfs");
        create_directory_test_layer(&directory_layer, "physical", 0o444);
        create_layer_with_symlink(&symlink_layer, "alias", Path::new("physical"), &[]);
        create_test_layer(&child_layer, &[("alias/child.txt", b"payload")]);

        extract_layer_with_metadata(&directory_layer, &target).unwrap();
        extract_layer_with_metadata(&symlink_layer, &target).unwrap();
        let alias_mode = load_image_metadata(&target).unwrap()[Path::new("alias")].mode;
        extract_layer_with_metadata(&child_layer, &target).unwrap();

        let manifest = load_image_metadata(&target).unwrap();
        assert_eq!(manifest[Path::new("physical")].mode & 0o777, 0o444);
        assert_eq!(manifest[Path::new("alias")].kind, RootfsEntryKind::Symlink);
        assert_eq!(manifest[Path::new("alias")].mode, alias_mode);
        let physical = target.join("physical");
        assert_eq!(
            fs::metadata(&physical).unwrap().permissions().mode() & 0o777,
            0o444
        );
        fs::set_permissions(&physical, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(fs::read(physical.join("child.txt")).unwrap(), b"payload");
    }

    #[cfg(unix)]
    #[test]
    fn final_metadata_replay_opens_nontraversable_directory_without_changing_manifest_mode() {
        use std::os::unix::fs::PermissionsExt;

        if unsafe { libc::geteuid() } == 0 {
            return;
        }

        let temp_dir = TempDir::new().unwrap();
        let layer = temp_dir.path().join("nontraversable.tar.gz");
        let target = temp_dir.path().join("rootfs");
        // Owner-write without owner-execute is still non-traversable.
        create_directory_test_layer(&layer, "private", 0o644);
        extract_layer_with_metadata(&layer, &target).unwrap();

        finalize_rootfs_metadata(&target).unwrap();

        let metadata = load_image_metadata(&target).unwrap();
        assert_eq!(metadata[Path::new("private")].mode & 0o777, 0o644);
        assert_eq!(
            fs::metadata(target.join("private"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o744,
            "host permissions must remain open only for guest metadata replay"
        );
    }

    #[cfg(unix)]
    #[test]
    fn tracked_extraction_can_finalize_a_readonly_root_directory() {
        use std::os::unix::fs::PermissionsExt;

        if unsafe { libc::geteuid() } == 0 {
            return;
        }

        let temp_dir = TempDir::new().unwrap();
        let readonly_root = temp_dir.path().join("readonly-root.tar.gz");
        let empty_upper = temp_dir.path().join("empty-upper.tar.gz");
        let target = temp_dir.path().join("rootfs");
        create_directory_test_layer(&readonly_root, ".", 0o555);
        create_test_layer(&empty_upper, &[]);

        extract_layer_with_metadata(&readonly_root, &target).unwrap();
        // tar intentionally leaves the extraction root's host mode alone;
        // model a lower layer/cache whose backing root is already restrictive.
        fs::set_permissions(&target, fs::Permissions::from_mode(0o555)).unwrap();
        extract_layer_with_metadata(&empty_upper, &target).unwrap();

        let manifest = load_image_metadata(&target).unwrap();
        assert_eq!(manifest[Path::new("")].mode & 0o777, 0o555);
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o555
        );

        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn test_extract_layer_overwrites_existing_files() {
        let temp_dir = TempDir::new().unwrap();
        let layer1_path = temp_dir.path().join("layer1.tar.gz");
        let layer2_path = temp_dir.path().join("layer2.tar.gz");
        let target_dir = temp_dir.path().join("extracted");

        // Create two layers with same filename
        create_test_layer(&layer1_path, &[("file.txt", b"version 1")]);
        create_test_layer(&layer2_path, &[("file.txt", b"version 2")]);

        // Extract first layer
        extract_layer(&layer1_path, &target_dir).unwrap();
        let content1 = fs::read_to_string(target_dir.join("file.txt")).unwrap();
        assert_eq!(content1, "version 1");

        // Extract second layer (should overwrite)
        extract_layer(&layer2_path, &target_dir).unwrap();
        let content2 = fs::read_to_string(target_dir.join("file.txt")).unwrap();
        assert_eq!(content2, "version 2");
    }

    #[test]
    fn test_extract_layer_overwrites_existing_hardlink_destination() {
        let temp_dir = TempDir::new().unwrap();
        let layer1_path = temp_dir.path().join("layer1.tar.gz");
        let layer2_path = temp_dir.path().join("layer2.tar.gz");
        let target_dir = temp_dir.path().join("extracted");

        create_test_layer(
            &layer1_path,
            &[
                ("usr/bin/perl", b"current interpreter"),
                ("usr/bin/perl5.38.2", b"stale interpreter"),
            ],
        );
        create_hardlink_test_layer(&layer2_path, "usr/bin/perl5.38.2", "usr/bin/perl");

        extract_layer(&layer1_path, &target_dir).unwrap();
        extract_layer(&layer2_path, &target_dir).unwrap();

        assert_eq!(
            fs::read(target_dir.join("usr/bin/perl5.38.2")).unwrap(),
            b"current interpreter"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                fs::metadata(target_dir.join("usr/bin/perl")).unwrap().ino(),
                fs::metadata(target_dir.join("usr/bin/perl5.38.2"))
                    .unwrap()
                    .ino(),
            );
        }
    }

    #[test]
    fn test_extract_layer_applies_whiteout() {
        let temp_dir = TempDir::new().unwrap();
        let layer1 = temp_dir.path().join("layer1.tar.gz");
        let layer2 = temp_dir.path().join("layer2.tar.gz");
        let target = temp_dir.path().join("extracted");

        create_test_layer(
            &layer1,
            &[("dir/keep.txt", b"keep"), ("dir/removed.txt", b"bye")],
        );
        // Upper layer whites out dir/removed.txt
        create_test_layer(&layer2, &[("dir/.wh.removed.txt", b"")]);

        extract_layer(&layer1, &target).unwrap();
        assert!(target.join("dir/removed.txt").exists());

        extract_layer(&layer2, &target).unwrap();
        assert!(target.join("dir/keep.txt").exists(), "sibling must survive");
        assert!(
            !target.join("dir/removed.txt").exists(),
            "whiteout must delete the file from the lower layer"
        );
        assert!(
            !target.join("dir/.wh.removed.txt").exists(),
            "whiteout marker must not be written to the rootfs"
        );
    }

    #[test]
    fn test_extract_layer_applies_opaque_directory() {
        let temp_dir = TempDir::new().unwrap();
        let layer1 = temp_dir.path().join("l1.tar.gz");
        let layer2 = temp_dir.path().join("l2.tar.gz");
        let target = temp_dir.path().join("ex");

        create_test_layer(&layer1, &[("d/old1.txt", b"a"), ("d/old2.txt", b"b")]);
        // Opaque marker clears prior dir contents; new.txt is added afterward.
        create_test_layer(&layer2, &[("d/.wh..wh..opq", b""), ("d/new.txt", b"c")]);

        extract_layer(&layer1, &target).unwrap();
        extract_layer(&layer2, &target).unwrap();

        assert!(!target.join("d/old1.txt").exists());
        assert!(!target.join("d/old2.txt").exists());
        assert!(target.join("d/new.txt").exists());
        assert!(!target.join("d/.wh..wh..opq").exists());
    }

    #[test]
    fn tracked_metadata_preserves_header_ownership_and_whiteouts() {
        let temp_dir = TempDir::new().unwrap();
        let layer1 = temp_dir.path().join("metadata-1.tar.gz");
        let layer2 = temp_dir.path().join("metadata-2.tar.gz");
        let target = temp_dir.path().join("rootfs");
        create_owned_test_layer(&layer1, "dir/owned", b"payload", 123, 456, 0o750);
        create_test_layer(&layer2, &[("dir/.wh.owned", b"")]);

        extract_layer_with_metadata(&layer1, &target).unwrap();
        let manifest = read_image_manifest(&target);
        let owned = manifest
            .entries
            .iter()
            .find(|entry| {
                base64::engine::general_purpose::STANDARD
                    .decode(&entry.path_base64)
                    .is_ok_and(|raw| raw == b"./dir/owned")
            })
            .unwrap();
        assert_eq!(
            (owned.uid, owned.gid, owned.mode & 0o7777),
            (123, 456, 0o750)
        );

        extract_layer_with_metadata(&layer2, &target).unwrap();
        let manifest = read_image_manifest(&target);
        assert!(!manifest.entries.iter().any(|entry| {
            base64::engine::general_purpose::STANDARD
                .decode(&entry.path_base64)
                .is_ok_and(|raw| raw.ends_with(b"dir/owned"))
        }));
    }

    #[test]
    fn metadata_descendants_are_scanned_only_when_replacing_a_directory() {
        let path = PathBuf::from("usr/lib/example");
        let mut metadata = BTreeMap::new();
        let entry = |kind| RootfsMetadataEntry {
            path_base64: String::new(),
            kind,
            mode: 0,
            uid: 0,
            gid: 0,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        };

        metadata.insert(path.clone(), entry(RootfsEntryKind::Regular));
        assert!(!metadata_descendant_cleanup_needed(
            &metadata,
            &path,
            RootfsEntryKind::Regular
        ));

        metadata.insert(path.clone(), entry(RootfsEntryKind::Directory));
        assert!(metadata_descendant_cleanup_needed(
            &metadata,
            &path,
            RootfsEntryKind::Regular
        ));
        assert!(!metadata_descendant_cleanup_needed(
            &metadata,
            &path,
            RootfsEntryKind::Directory
        ));

        metadata.clear();
        metadata.insert(path.join("implicit-child"), entry(RootfsEntryKind::Regular));
        assert!(metadata_descendant_cleanup_needed(
            &metadata,
            &path,
            RootfsEntryKind::Regular
        ));

        metadata.clear();
        metadata.insert(
            PathBuf::from("usr/lib/example-other"),
            entry(RootfsEntryKind::Regular),
        );
        assert!(!metadata_descendant_cleanup_needed(
            &metadata,
            &path,
            RootfsEntryKind::Regular
        ));

        metadata.insert(path.join("implicit-child"), entry(RootfsEntryKind::Regular));
        assert!(metadata_descendant_cleanup_needed(
            &metadata,
            &path,
            RootfsEntryKind::Regular
        ));
    }

    #[test]
    fn every_extraction_mode_rejects_reserved_internal_paths() {
        for (index, reserved) in [
            ".a3s_image_metadata_v1.json",
            ".a3s_image_metadata_v1.json/child",
            ".a3s_image_metadata_v1.json.tmp",
            ".a3s_image_metadata_v1.json.tmp/child",
            ".a3s_rootfs_metadata_v1.json",
            ".a3s_rootfs_metadata_v1.json/child",
            ".a3s_rootfs_metadata_v1.json.tmp",
            ".a3s_rootfs_metadata_v1.json.tmp/child",
            ".a3s_rootfs_metadata_v1.previous.json",
            ".a3s_rootfs_metadata_v1.previous.json/child",
        ]
        .into_iter()
        .enumerate()
        {
            for track_metadata in [false, true] {
                let temp_dir = TempDir::new().unwrap();
                let layer = temp_dir
                    .path()
                    .join(format!("reserved-{index}-{track_metadata}.tar.gz"));
                let target = temp_dir.path().join("rootfs");
                create_test_layer(&layer, &[(reserved, b"forged")]);

                let error = if track_metadata {
                    extract_layer_with_metadata(&layer, &target).unwrap_err()
                } else {
                    extract_layer(&layer, &target).unwrap_err()
                };
                assert!(error.to_string().contains("reserved internal path"));
                assert!(!target.join(reserved).exists());
            }
        }
    }

    #[test]
    fn untracked_extraction_rejects_reserved_whiteouts() {
        for victim in [
            ".a3s_rootfs_metadata_v1.previous.json",
            ".a3s_rootfs_metadata_v1.json.tmp",
        ] {
            let temp_dir = TempDir::new().unwrap();
            let layer = temp_dir.path().join("reserved-whiteout.tar.gz");
            let target = temp_dir.path().join("rootfs");
            let whiteout = format!(".wh.{victim}");
            create_test_layer(&layer, &[(&whiteout, b"")]);

            let error = extract_layer(&layer, &target).unwrap_err();
            assert!(error
                .to_string()
                .contains("whiteouts reserved internal path"));
        }
    }

    #[test]
    fn tracked_metadata_rejects_reserved_terminal_symlink() {
        let temp_dir = TempDir::new().unwrap();
        let layer = temp_dir.path().join("reserved-terminal-link.tar.gz");
        let target = temp_dir.path().join("rootfs");
        create_layer_with_symlink(
            &layer,
            ".a3s_rootfs_metadata_v1.json",
            Path::new("/dev/zero"),
            &[],
        );

        let error = extract_layer_with_metadata(&layer, &target).unwrap_err();
        assert!(error.to_string().contains("reserved internal path"));
        assert!(std::fs::symlink_metadata(target.join(".a3s_rootfs_metadata_v1.json")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn metadata_finalization_replaces_temp_symlink_without_touching_outside() {
        let temp_dir = TempDir::new().unwrap();
        let target = temp_dir.path().join("rootfs");
        let outside = temp_dir.path().join("outside");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(&outside, b"host-secret").unwrap();
        std::os::unix::fs::symlink("../outside", target.join(".a3s_image_metadata_v1.json.tmp"))
            .unwrap();

        finalize_rootfs_metadata(&target).unwrap();

        assert_eq!(std::fs::read(&outside).unwrap(), b"host-secret");
        assert!(target.join(image_metadata_relative_path()).is_file());
        assert!(!target.join(".a3s_image_metadata_v1.json.tmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn metadata_collection_uses_real_modes_for_runtime_replacements() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = TempDir::new().unwrap();
        let target = temp_dir.path().join("rootfs");
        std::fs::create_dir_all(&target).unwrap();
        let replacement = target.join("replacement");
        std::fs::write(&replacement, b"regular").unwrap();
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o640)).unwrap();
        let rewritten = target.join("rewritten");
        std::fs::write(&rewritten, b"rewritten").unwrap();
        std::fs::set_permissions(&rewritten, std::fs::Permissions::from_mode(0o644)).unwrap();

        let path = PathBuf::from("replacement");
        let rewritten_path = PathBuf::from("rewritten");
        let desired_entry = |path: &Path, kind, mode| RootfsMetadataEntry {
            path_base64: base64::engine::general_purpose::STANDARD
                .encode(archive_metadata_path_bytes(path)),
            kind,
            mode,
            uid: 123,
            gid: 456,
            mtime: 0,
            size: 0,
            link_target_base64: (kind == RootfsEntryKind::Symlink)
                .then(|| base64::engine::general_purpose::STANDARD.encode(b"old-target")),
        };
        let mut desired = BTreeMap::from([
            (
                path.clone(),
                desired_entry(&path, RootfsEntryKind::Symlink, 0o777),
            ),
            (
                rewritten_path.clone(),
                desired_entry(&rewritten_path, RootfsEntryKind::Regular, 0o600),
            ),
        ]);

        finalize_image_metadata(&target, &mut desired).unwrap();

        let replacement = &desired[&path];
        assert_eq!(replacement.kind, RootfsEntryKind::Regular);
        assert_eq!(replacement.mode & 0o7777, 0o640);
        assert_eq!((replacement.uid, replacement.gid), (0, 0));
        let rewritten = &desired[&rewritten_path];
        assert_eq!(rewritten.kind, RootfsEntryKind::Regular);
        assert_eq!(rewritten.mode & 0o7777, 0o644);
        assert_eq!((rewritten.uid, rewritten.gid), (123, 456));
    }

    #[cfg(unix)]
    #[test]
    fn metadata_loading_rejects_destination_symlink_escape() {
        let temp_dir = TempDir::new().unwrap();
        let target = temp_dir.path().join("rootfs");
        let outside = temp_dir.path().join("outside");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(&outside, b"host-secret").unwrap();
        std::os::unix::fs::symlink("../outside", target.join(image_metadata_relative_path()))
            .unwrap();

        let error = finalize_rootfs_metadata(&target).unwrap_err().to_string();

        assert!(error.contains("escapes rootfs"), "{error}");
        assert_eq!(std::fs::read(&outside).unwrap(), b"host-secret");
    }

    #[cfg(unix)]
    #[test]
    fn extraction_rejects_overlayfs_private_xattrs() {
        let temp_dir = TempDir::new().unwrap();
        for (index, xattr) in ["trusted.overlay.metacopy", "user.overlay.redirect"]
            .into_iter()
            .enumerate()
        {
            let layer = temp_dir
                .path()
                .join(format!("overlay-xattr-{index}.tar.gz"));
            let target = temp_dir.path().join(format!("rootfs-{index}"));
            create_overlay_xattr_test_layer(&layer, xattr);

            let error = extract_layer_with_metadata(&layer, &target).unwrap_err();
            assert!(error
                .to_string()
                .contains("contains reserved overlayfs metadata"));
            assert!(!target.join("payload").exists());
        }
    }

    #[test]
    fn extract_layer_rejects_decompression_bomb_past_cap() {
        let temp_dir = TempDir::new().unwrap();
        let layer = temp_dir.path().join("bomb.tar.gz");
        let target = temp_dir.path().join("out");
        // 64 KiB of zeros — compresses to almost nothing but exceeds a small cap,
        // standing in for a real layer that expands to hundreds of GB.
        let big = vec![0u8; 64 * 1024];
        create_test_layer(&layer, &[("big", &big)]);

        // A 4 KiB cap must abort the extraction...
        let result = extract_layer_with_cap(&layer, &target, 4 * 1024, false);
        assert!(
            result.is_err(),
            "the cap must abort an oversized (bomb) layer, got: {result:?}"
        );
        // ...BEFORE the full 64 KiB member is written to disk.
        let written = std::fs::metadata(target.join("big"))
            .map(|m| m.len())
            .unwrap_or(0);
        assert!(
            written < 64 * 1024,
            "cap must bound bytes written before aborting; wrote {written}"
        );
    }

    #[test]
    fn extract_layer_with_generous_cap_extracts_normally() {
        let temp_dir = TempDir::new().unwrap();
        let layer = temp_dir.path().join("ok.tar.gz");
        let target = temp_dir.path().join("out");
        create_test_layer(&layer, &[("file.txt", b"hello")]);
        // A generous cap must not regress a normal small layer.
        extract_layer_with_cap(&layer, &target, 16 * 1024 * 1024, false).unwrap();
        assert!(target.join("file.txt").exists());
    }

    // Helper function to create a test tar.gz layer
    fn create_test_layer(path: &Path, files: &[(&str, &[u8])]) {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use tar::Builder;

        let file = File::create(path).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = Builder::new(encoder);

        for (name, content) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            // Set uid/gid explicitly: a bare GNU header leaves those octal fields
            // blank, which makes a root-side extraction with preserved ownership
            // fail to parse the uid ("numeric field was not a number"). Real OCI
            // layers always carry valid uid/gid fields.
            header.set_uid(0);
            header.set_gid(0);
            header.set_cksum();

            builder.append_data(&mut header, name, *content).unwrap();
        }

        builder.finish().unwrap();
    }

    fn create_owned_test_layer(
        path: &Path,
        name: &str,
        content: &[u8],
        uid: u64,
        gid: u64,
        mode: u32,
    ) {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use tar::Builder;

        let file = File::create(path).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(mode);
        header.set_uid(uid);
        header.set_gid(gid);
        header.set_cksum();
        builder.append_data(&mut header, name, content).unwrap();
        builder.finish().unwrap();
    }

    fn create_directory_test_layer(path: &Path, name: &str, mode: u32) {
        create_directory_entries_test_layer(path, &[(name, mode)]);
    }

    fn create_directory_entries_test_layer(path: &Path, entries: &[(&str, u32)]) {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use tar::Builder;

        let file = File::create(path).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = Builder::new(encoder);
        for (name, mode) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            header.set_mode(*mode);
            header.set_uid(0);
            header.set_gid(0);
            header.set_cksum();
            builder
                .append_data(&mut header, name, std::io::empty())
                .unwrap();
        }
        builder.finish().unwrap();
    }

    fn create_child_then_directory_test_layer(
        path: &Path,
        directory: &str,
        child: &str,
        content: &[u8],
        directory_mode: u32,
    ) {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use tar::Builder;

        let file = File::create(path).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = Builder::new(encoder);

        let mut child_header = tar::Header::new_gnu();
        child_header.set_size(content.len() as u64);
        child_header.set_mode(0o644);
        child_header.set_uid(0);
        child_header.set_gid(0);
        child_header.set_cksum();
        builder
            .append_data(&mut child_header, child, content)
            .unwrap();

        let mut directory_header = tar::Header::new_gnu();
        directory_header.set_entry_type(tar::EntryType::Directory);
        directory_header.set_size(0);
        directory_header.set_mode(directory_mode);
        directory_header.set_uid(0);
        directory_header.set_gid(0);
        directory_header.set_cksum();
        builder
            .append_data(&mut directory_header, directory, std::io::empty())
            .unwrap();
        builder.finish().unwrap();
    }

    #[cfg(unix)]
    fn create_overlay_xattr_test_layer(path: &Path, xattr: &str) {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use tar::Builder;

        let file = File::create(path).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = Builder::new(encoder);
        let key = format!("SCHILY.xattr.{xattr}");
        builder
            .append_pax_extensions([(key.as_str(), b"".as_slice())])
            .unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_size(7);
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_cksum();
        builder
            .append_data(&mut header, "payload", b"payload".as_slice())
            .unwrap();
        builder.finish().unwrap();
    }

    fn create_hardlink_test_layer(path: &Path, name: &str, target: &str) {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use tar::Builder;

        let file = File::create(path).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Link);
        header.set_size(0);
        header.set_mode(0o755);
        header.set_uid(0);
        header.set_gid(0);
        builder.append_link(&mut header, name, target).unwrap();
        builder.finish().unwrap();
    }

    fn read_image_manifest(target: &Path) -> RootfsMetadataManifest {
        let bytes =
            std::fs::read(target.join(IMAGE_ROOTFS_METADATA_PATH.trim_start_matches('/'))).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn write_test_tar<W: std::io::Write>(writer: W, files: &[(&str, &[u8])]) {
        use tar::Builder;
        let mut builder = Builder::new(writer);
        for (name, content) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_uid(0);
            header.set_gid(0);
            header.set_cksum();
            builder.append_data(&mut header, name, *content).unwrap();
        }
        builder.finish().unwrap();
    }

    /// Build a gzipped layer that first creates a SYMLINK entry, then writes the
    /// given follow-on entries — used to probe symlink-directed escapes (a later
    /// entry / whiteout that resolves THROUGH the symlinked parent).
    fn create_layer_with_symlink(path: &Path, link: &str, target: &Path, then: &[(&str, &[u8])]) {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use tar::Builder;

        let file = File::create(path).unwrap();
        let mut builder = Builder::new(GzEncoder::new(file, Compression::default()));

        let mut sh = tar::Header::new_gnu();
        sh.set_entry_type(tar::EntryType::Symlink);
        sh.set_size(0);
        sh.set_mode(0o777);
        sh.set_uid(0);
        sh.set_gid(0);
        builder.append_link(&mut sh, link, target).unwrap();

        for (name, content) in then {
            let mut h = tar::Header::new_gnu();
            h.set_size(content.len() as u64);
            h.set_mode(0o644);
            h.set_uid(0);
            h.set_gid(0);
            h.set_cksum();
            builder.append_data(&mut h, name, *content).unwrap();
        }
        builder.finish().unwrap();
    }

    // ---- Malicious-image extraction hardening (host-side, occurs during pull) ----
    // A hostile layer must never reach outside the extraction target. These encode
    // the SECURE expectation: a failure here is a real escape, not a flaky test.

    #[test]
    fn whiteout_does_not_delete_through_symlinked_parent() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("rootfs");
        fs::create_dir_all(&target).unwrap();
        // A host file OUTSIDE the target that a malicious image must not delete.
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let victim = outside.join("victim");
        fs::write(&victim, b"keep me").unwrap();

        // esc -> <outside> (absolute symlink target, legal in images), then a
        // whiteout `.wh.victim` whose parent is the symlink.
        let layer = tmp.path().join("evil.tar.gz");
        create_layer_with_symlink(&layer, "esc", &outside, &[("esc/.wh.victim", b"")]);
        let _ = extract_layer(&layer, &target);

        assert!(
            victim.exists(),
            "SECURITY: whiteout followed a symlinked parent and deleted a host file outside the target ({})",
            victim.display()
        );
    }

    #[test]
    fn opaque_whiteout_does_not_wipe_through_symlinked_parent() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("rootfs");
        fs::create_dir_all(&target).unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let a = outside.join("a");
        let b = outside.join("b");
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();

        let layer = tmp.path().join("evil.tar.gz");
        create_layer_with_symlink(&layer, "esc", &outside, &[("esc/.wh..wh..opq", b"")]);
        let _ = extract_layer(&layer, &target);

        assert!(
            a.exists() && b.exists(),
            "SECURITY: opaque whiteout wiped a host directory through a symlinked parent"
        );
    }

    #[test]
    fn layer_entry_cannot_write_through_symlinked_parent() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("rootfs");
        fs::create_dir_all(&target).unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();

        let layer = tmp.path().join("evil.tar.gz");
        create_layer_with_symlink(&layer, "esc", &outside, &[("esc/pwned", b"owned")]);
        let _ = extract_layer(&layer, &target);

        assert!(
            !outside.join("pwned").exists(),
            "SECURITY: a layer wrote through a symlinked parent to outside the target"
        );
    }

    #[test]
    fn test_extract_layer_handles_zstd() {
        let temp_dir = TempDir::new().unwrap();
        let layer_path = temp_dir.path().join("layer.tar.zst");
        let target_dir = temp_dir.path().join("extracted");
        {
            let file = File::create(&layer_path).unwrap();
            let encoder = zstd::stream::write::Encoder::new(file, 0)
                .unwrap()
                .auto_finish();
            write_test_tar(encoder, &[("z.txt", b"zstd-content")]);
        }

        extract_layer(&layer_path, &target_dir).unwrap();
        assert_eq!(
            fs::read_to_string(target_dir.join("z.txt")).unwrap(),
            "zstd-content"
        );
    }

    #[test]
    fn test_extract_layer_handles_uncompressed_tar() {
        let temp_dir = TempDir::new().unwrap();
        let layer_path = temp_dir.path().join("layer.tar");
        let target_dir = temp_dir.path().join("extracted");
        write_test_tar(File::create(&layer_path).unwrap(), &[("p.txt", b"plain")]);

        extract_layer(&layer_path, &target_dir).unwrap();
        assert_eq!(
            fs::read_to_string(target_dir.join("p.txt")).unwrap(),
            "plain"
        );
    }
}
