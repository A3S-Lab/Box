//! OCI layer extraction utilities.
//!
//! Handles extraction of OCI image layers (gzip, zstd, or uncompressed tar).

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::rootfs_metadata::{
    RootfsEntryKind, RootfsMetadataEntry, RootfsMetadataManifest, IMAGE_ROOTFS_METADATA_PATH,
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

    let entries = archive
        .entries()
        .map_err(|e| BoxError::OciImageError(format!("Failed to read layer entries: {e}")))?;

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
        if track_metadata && normalized == image_metadata_relative_path() {
            return Err(BoxError::OciImageError(format!(
                "OCI layer contains reserved internal path {}",
                IMAGE_ROOTFS_METADATA_PATH
            )));
        }

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
            if track_metadata && victim == image_metadata_relative_path() {
                return Err(BoxError::OciImageError(format!(
                    "OCI layer whiteouts reserved internal path {}",
                    IMAGE_ROOTFS_METADATA_PATH
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
        let unpacked = entry.unpack_in(target_dir).map_err(|e| {
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
        if track_metadata && unpacked {
            if let Some(desired) = desired {
                if desired.kind != RootfsEntryKind::Directory {
                    remove_metadata_descendants(&mut metadata, &normalized, false);
                }
                metadata.insert(normalized, desired);
            }
        }
    }

    if track_metadata {
        finalize_image_metadata(target_dir, &mut metadata)?;
    }

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
            Some(
                base64::engine::general_purpose::STANDARD
                    .encode(target.as_os_str().as_encoded_bytes()),
            ),
        )
    } else if entry_type.is_file() || entry_type.is_hard_link() {
        (RootfsEntryKind::Regular, None)
    } else {
        return Err(BoxError::OciImageError(format!(
            "Unsupported OCI layer entry type at {}",
            path.display()
        )));
    };
    let path_base64 = base64::engine::general_purpose::STANDARD
        .encode(archive_metadata_path(path).as_os_str().as_encoded_bytes());
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

fn archive_metadata_path(path: &Path) -> PathBuf {
    if path.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        Path::new(".").join(path)
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

fn load_image_metadata(target_dir: &Path) -> Result<BTreeMap<PathBuf, RootfsMetadataEntry>> {
    let path = target_dir.join(image_metadata_relative_path());
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
        if relative == image_metadata_relative_path() || result.insert(relative, entry).is_some() {
            return Err(BoxError::OciImageError(
                "Duplicate or reserved path in image metadata".to_string(),
            ));
        }
    }
    Ok(result)
}

pub(crate) fn finalize_rootfs_metadata(target_dir: &Path) -> Result<()> {
    let mut metadata = load_image_metadata(target_dir)?;
    finalize_image_metadata(target_dir, &mut metadata)?;
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
        // cannot update it on a 0444/0555 host-owned inode, so make only the
        // owner's write bit temporarily available. Guest-init restores the
        // exact manifest mode before launching any container process.
        for (relative, entry) in metadata {
            if entry.kind == RootfsEntryKind::Symlink || entry.mode & 0o200 != 0 {
                continue;
            }
            let target = target_dir.join(relative);
            let current = std::fs::symlink_metadata(&target).map_err(|error| {
                BoxError::OciImageError(format!(
                    "Failed to prepare metadata replay for {}: {error}",
                    target.display()
                ))
            })?;
            std::fs::set_permissions(
                &target,
                std::fs::Permissions::from_mode((current.mode() & 0o7777) | 0o200),
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

fn finalize_image_metadata(
    target_dir: &Path,
    metadata: &mut BTreeMap<PathBuf, RootfsMetadataEntry>,
) -> Result<()> {
    let mut final_entries = BTreeMap::new();
    collect_final_metadata(
        target_dir,
        target_dir,
        Path::new(""),
        metadata,
        &mut final_entries,
    )?;
    let manifest = RootfsMetadataManifest::new(final_entries.into_values().collect());
    let destination = target_dir.join(image_metadata_relative_path());
    let temporary = destination.with_extension("json.tmp");
    let bytes = serde_json::to_vec(&manifest).map_err(|error| {
        BoxError::OciImageError(format!("Failed to encode image metadata: {error}"))
    })?;
    std::fs::write(&temporary, bytes).map_err(|error| {
        BoxError::OciImageError(format!(
            "Failed to write image metadata {}: {error}",
            temporary.display()
        ))
    })?;
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
    output: &mut BTreeMap<PathBuf, RootfsMetadataEntry>,
) -> Result<()> {
    if relative == image_metadata_relative_path()
        || relative == Path::new(".a3s_image_metadata_v1.json.tmp")
    {
        return Ok(());
    }
    let filesystem = std::fs::symlink_metadata(source).map_err(|error| {
        BoxError::OciImageError(format!("Failed to inspect {}: {error}", source.display()))
    })?;
    let file_type = filesystem.file_type();
    let (kind, link_target_base64) = if file_type.is_dir() {
        (RootfsEntryKind::Directory, None)
    } else if file_type.is_file() {
        (RootfsEntryKind::Regular, None)
    } else if file_type.is_symlink() {
        let target = std::fs::read_link(source).map_err(|error| {
            BoxError::OciImageError(format!("Failed to read {}: {error}", source.display()))
        })?;
        (
            RootfsEntryKind::Symlink,
            Some(
                base64::engine::general_purpose::STANDARD
                    .encode(target.as_os_str().as_encoded_bytes()),
            ),
        )
    } else {
        return Ok(());
    };
    let previous = desired.get(relative);
    #[cfg(unix)]
    let (mode, mtime, size) = {
        use std::os::unix::fs::MetadataExt;
        (
            filesystem.mode(),
            filesystem.mtime().max(0) as u64,
            filesystem.size(),
        )
    };
    #[cfg(not(unix))]
    let (mode, mtime, size) = previous
        .map(|entry| (entry.mode, entry.mtime, entry.size))
        .unwrap_or_else(|| {
            (
                if file_type.is_dir() { 0o755 } else { 0o644 },
                0,
                filesystem.len(),
            )
        });
    let entry = RootfsMetadataEntry {
        path_base64: base64::engine::general_purpose::STANDARD.encode(
            archive_metadata_path(relative)
                .as_os_str()
                .as_encoded_bytes(),
        ),
        kind,
        mode,
        uid: previous.map_or(0, |entry| entry.uid),
        gid: previous.map_or(0, |entry| entry.gid),
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
                output,
            )?;
        }
    }
    let _ = root;
    Ok(())
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
    fn tracked_metadata_rejects_reserved_image_path() {
        let temp_dir = TempDir::new().unwrap();
        let layer = temp_dir.path().join("reserved.tar.gz");
        let target = temp_dir.path().join("rootfs");
        create_test_layer(&layer, &[(".a3s_image_metadata_v1.json", b"forged")]);

        let error = extract_layer_with_metadata(&layer, &target).unwrap_err();
        assert!(error.to_string().contains("reserved internal path"));
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
