//! Host-side rootfs ownership preparation for user-namespace execution.

#[cfg(target_os = "linux")]
use std::collections::HashSet;
use std::io::Read;
#[cfg(target_os = "linux")]
use std::io::Write;
#[cfg(any(target_os = "linux", test))]
use std::path::Component;
use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
#[cfg(target_os = "linux")]
use a3s_box_core::rootfs_metadata::runtime_managed_rootfs_mode;
#[cfg(any(target_os = "linux", test))]
use a3s_box_core::rootfs_metadata::{RootfsEntryKind, RootfsMetadataEntry};
use a3s_box_core::rootfs_metadata::{
    RootfsMetadataManifest, IMAGE_ROOTFS_METADATA_PATH, PREVIOUS_ROOTFS_METADATA_PATH,
    ROOTFS_METADATA_PATH,
};
#[cfg(any(target_os = "linux", test))]
use base64::Engine;

use super::capability::{IdMapping, SandboxIdMappingPlan};

const MAX_ROOTFS_METADATA_BYTES: u64 = 64 * 1024 * 1024;
#[cfg(target_os = "linux")]
const SNAPSHOT_ID_MAPPING_FILE: &str = "sandbox/rootfs-id-mappings.json";
#[cfg(target_os = "linux")]
const MAX_SNAPSHOT_ID_MAPPING_BYTES: u64 = 64 * 1024;

/// Container IDs discovered in the authoritative rootfs metadata manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootfsIdentityRequirements {
    pub maximum_uid: u32,
    pub maximum_gid: u32,
    pub manifest_path: PathBuf,
}

/// Persist the exact user-namespace mapping needed to translate a stopped
/// Sandbox rootfs back to container ownership during filesystem snapshots.
#[cfg(target_os = "linux")]
pub(crate) fn persist_snapshot_id_mappings(
    box_dir: &Path,
    plan: &SandboxIdMappingPlan,
) -> Result<()> {
    let destination = box_dir.join(SNAPSHOT_ID_MAPPING_FILE);
    let parent = destination.parent().ok_or_else(|| {
        BoxError::ConfigError(format!(
            "Sandbox snapshot mapping path has no parent: {}",
            destination.display()
        ))
    })?;
    std::fs::create_dir_all(parent).map_err(BoxError::IoError)?;
    let mut encoded = serde_json::to_vec_pretty(plan).map_err(|error| {
        BoxError::SerializationError(format!(
            "Failed to encode Sandbox snapshot ID mappings: {error}"
        ))
    })?;
    encoded.push(b'\n');
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(BoxError::IoError)?;
    temporary.write_all(&encoded).map_err(BoxError::IoError)?;
    temporary.as_file().sync_all().map_err(BoxError::IoError)?;
    temporary
        .persist(&destination)
        .map_err(|error| BoxError::IoError(error.error))?;
    if let Ok(directory) = std::fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

/// Load a persisted snapshot mapping, rejecting links, oversized artifacts,
/// and malformed JSON before it can influence host ownership translation.
#[cfg(target_os = "linux")]
pub(crate) fn load_snapshot_id_mappings(box_dir: &Path) -> Result<Option<SandboxIdMappingPlan>> {
    let path = box_dir.join(SNAPSHOT_ID_MAPPING_FILE);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(BoxError::IoError(error)),
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_SNAPSHOT_ID_MAPPING_BYTES {
        return Err(BoxError::ConfigError(format!(
            "Sandbox snapshot ID mapping artifact is not a bounded regular file: {}",
            path.display()
        )));
    }
    let mut encoded = Vec::with_capacity(metadata.len() as usize);
    std::fs::File::open(&path)
        .map_err(BoxError::IoError)?
        .take(MAX_SNAPSHOT_ID_MAPPING_BYTES + 1)
        .read_to_end(&mut encoded)
        .map_err(BoxError::IoError)?;
    if encoded.len() as u64 > MAX_SNAPSHOT_ID_MAPPING_BYTES {
        return Err(BoxError::ConfigError(format!(
            "Sandbox snapshot ID mapping artifact exceeds {} bytes: {}",
            MAX_SNAPSHOT_ID_MAPPING_BYTES,
            path.display()
        )));
    }
    serde_json::from_slice(&encoded).map(Some).map_err(|error| {
        BoxError::SerializationError(format!(
            "Failed to decode Sandbox snapshot ID mappings {}: {error}",
            path.display()
        ))
    })
}

/// Host IDs representing container root for one mapping plan.
pub fn mapped_root_ids(plan: &SandboxIdMappingPlan) -> Result<(u32, u32)> {
    Ok((
        map_container_id(&plan.uid_mappings, 0, "UID")?,
        map_container_id(&plan.gid_mappings, 0, "GID")?,
    ))
}

/// Make an A3S-owned workspace or anonymous volume accessible as container
/// root without ever changing an arbitrary caller-provided host tree.
#[cfg(target_os = "linux")]
pub fn prepare_managed_mount_source(path: &Path, plan: &SandboxIdMappingPlan) -> Result<()> {
    ensure_no_nested_mounts(path)?;
    let (root_uid, root_gid) = mapped_root_ids(plan)?;
    prepare_managed_tree(path, plan, root_uid, root_gid)
}

#[cfg(not(target_os = "linux"))]
pub fn prepare_managed_mount_source(_path: &Path, _plan: &SandboxIdMappingPlan) -> Result<()> {
    Err(BoxError::ConfigError(
        "Sandbox mount ownership preparation requires Linux".to_string(),
    ))
}

/// Verify that an external bind source's root is usable by the mapped root
/// identity. The runtime refuses to chown external host data implicitly.
#[cfg(unix)]
pub fn validate_external_mount_access(
    path: &Path,
    plan: &SandboxIdMappingPlan,
    read_only: bool,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let (uid, gid) = mapped_root_ids(plan)?;
    let metadata = std::fs::metadata(path).map_err(BoxError::IoError)?;
    let mode = metadata.mode();
    let permission_bits = if metadata.uid() == uid {
        (mode >> 6) & 0o7
    } else if metadata.gid() == gid {
        (mode >> 3) & 0o7
    } else {
        mode & 0o7
    };
    let required = if metadata.is_dir() {
        if read_only {
            0o5
        } else {
            0o7
        }
    } else if read_only {
        0o4
    } else {
        0o6
    };
    if permission_bits & required != required {
        return Err(BoxError::ConfigError(format!(
            "External Sandbox mount {} is not {} by mapped container root {uid}:{gid}; adjust host ownership/permissions or use an A3S-managed volume",
            path.display(),
            if read_only { "readable" } else { "writable" }
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn validate_external_mount_access(
    _path: &Path,
    _plan: &SandboxIdMappingPlan,
    _read_only: bool,
) -> Result<()> {
    Err(BoxError::ConfigError(
        "Sandbox bind mount validation requires Linux".to_string(),
    ))
}

#[cfg(target_os = "linux")]
struct DecodedEntry {
    metadata: RootfsMetadataEntry,
    relative: PathBuf,
    target: PathBuf,
}

/// Read the terminal persistent manifest when present, otherwise the immutable
/// image manifest. Fresh image generations select the immutable manifest through
/// the preference-aware variant below.
pub fn inspect_rootfs_identity_requirements(root: &Path) -> Result<RootfsIdentityRequirements> {
    inspect_rootfs_identity_requirements_with_preference(root, false)
}

pub(crate) fn inspect_rootfs_identity_requirements_with_preference(
    root: &Path,
    prefer_image_manifest: bool,
) -> Result<RootfsIdentityRequirements> {
    let (manifest_path, manifest) = load_authoritative_manifest(root, prefer_image_manifest)?;
    let mut maximum_uid = 0u32;
    let mut maximum_gid = 0u32;
    for entry in manifest.entries {
        let uid = u32::try_from(entry.uid).map_err(|_| {
            BoxError::OciImageError("rootfs metadata UID exceeds the Linux range".to_string())
        })?;
        let gid = u32::try_from(entry.gid).map_err(|_| {
            BoxError::OciImageError("rootfs metadata GID exceeds the Linux range".to_string())
        })?;
        maximum_uid = maximum_uid.max(uid);
        maximum_gid = maximum_gid.max(gid);
    }
    Ok(RootfsIdentityRequirements {
        maximum_uid,
        maximum_gid,
        manifest_path,
    })
}

/// Prepare one per-box rootfs for the exact user-namespace mapping.
///
/// A root-run service can translate OCI container ownership to subordinate
/// host IDs directly. A non-root service leaves ownership replay to PID 1 from
/// inside the user namespace. Read-only rootfs is rejected for the latter until
/// an idmapped-mount path can guarantee replay before the read-only transition.
#[cfg(target_os = "linux")]
pub fn prepare_rootfs_ownership(
    root: &Path,
    plan: &SandboxIdMappingPlan,
    effective_uid: u32,
    read_only: bool,
) -> Result<()> {
    prepare_rootfs_ownership_with_preference(root, plan, effective_uid, read_only, false)
}

#[cfg(target_os = "linux")]
pub(crate) fn prepare_rootfs_ownership_with_preference(
    root: &Path,
    plan: &SandboxIdMappingPlan,
    effective_uid: u32,
    read_only: bool,
    prefer_image_manifest: bool,
) -> Result<()> {
    if effective_uid != 0 {
        if read_only {
            return Err(BoxError::ConfigError(
                "Sandbox read-only rootfs requires a root-run service until idmapped rootfs preparation is available"
                    .to_string(),
            ));
        }
        return Ok(());
    }

    ensure_no_nested_mounts(root)?;
    let (_, manifest) = load_authoritative_manifest(root, prefer_image_manifest)?;
    let entries = decode_and_validate_entries(root, manifest)?;
    let authoritative_paths: HashSet<PathBuf> =
        entries.iter().map(|entry| entry.relative.clone()).collect();

    for entry in &entries {
        let uid = map_container_id(
            &plan.uid_mappings,
            u32::try_from(entry.metadata.uid).map_err(|_| {
                BoxError::OciImageError("rootfs metadata UID exceeds the Linux range".to_string())
            })?,
            "UID",
        )?;
        let gid = map_container_id(
            &plan.gid_mappings,
            u32::try_from(entry.metadata.gid).map_err(|_| {
                BoxError::OciImageError("rootfs metadata GID exceeds the Linux range".to_string())
            })?,
            "GID",
        )?;
        lchown_if_needed(&entry.target, uid, gid)?;
    }

    // Files written by the runtime after manifest generation (DNS, hostname,
    // env staging, refreshed init, and the manifests themselves) are not all
    // represented in the selected generation. Walk without following symlinks:
    // already-mapped IDs are left untouched, while raw OCI IDs are translated.
    shift_unlisted_entries(root, root, &authoritative_paths, plan)?;

    // chown clears setuid/setgid bits on regular files. Restore exact manifest
    // modes deepest-first after every ownership change.
    let mut modes: Vec<_> = entries
        .iter()
        .filter(|entry| entry.metadata.kind != RootfsEntryKind::Symlink)
        .collect();
    modes.sort_by_key(|entry| std::cmp::Reverse(entry.relative.components().count()));
    for entry in modes {
        use std::os::unix::fs::PermissionsExt;
        let mode =
            runtime_managed_rootfs_mode(&entry.relative).unwrap_or(entry.metadata.mode & 0o7777);
        std::fs::set_permissions(&entry.target, std::fs::Permissions::from_mode(mode)).map_err(
            |error| BoxError::BoxBootError {
                message: format!(
                    "Failed to restore Sandbox rootfs mode at {}: {error}",
                    entry.target.display()
                ),
                hint: None,
            },
        )?;
    }

    Ok(())
}

/// Capture authoritative guest-visible metadata for a quiesced Sandbox rootfs.
///
/// The host sees user-namespace IDs, so every UID/GID is translated back
/// through the exact OCI mappings before the manifest is stored in a
/// filesystem Snapshot. The walk never follows symlinks and rejects special
/// files, preventing a FIFO or device node from entering the copy path.
#[cfg(target_os = "linux")]
pub(crate) fn capture_snapshot_rootfs_metadata(
    root: &Path,
    plan: &SandboxIdMappingPlan,
) -> Result<RootfsMetadataManifest> {
    ensure_no_nested_mounts(root)?;
    let mut entries = Vec::new();
    collect_snapshot_rootfs_metadata(root, root, Path::new("."), plan, &mut entries)?;
    entries.sort_by(|left, right| left.path_base64.cmp(&right.path_base64));
    Ok(RootfsMetadataManifest::new(entries))
}

#[cfg(target_os = "linux")]
fn collect_snapshot_rootfs_metadata(
    root: &Path,
    source: &Path,
    manifest_path: &Path,
    plan: &SandboxIdMappingPlan,
    entries: &mut Vec<RootfsMetadataEntry>,
) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let relative = source
        .strip_prefix(root)
        .map_err(|_| BoxError::OciImageError("Sandbox Snapshot walk escaped its root".into()))?;
    if matches!(
        relative.to_str(),
        Some(".a3s_rootfs_metadata_v1.json")
            | Some(".a3s_rootfs_metadata_v1.json.tmp")
            | Some(".a3s_rootfs_metadata_v1.previous.json")
            | Some(".a3s_image_metadata_v1.json")
            | Some(".a3s_image_metadata_v1.json.tmp")
            | Some(".a3s_exit_code")
            | Some("init.trace.log")
    ) {
        return Ok(());
    }

    let metadata = std::fs::symlink_metadata(source).map_err(BoxError::IoError)?;
    let file_type = metadata.file_type();
    let (kind, link_target_base64) = if file_type.is_dir() {
        (RootfsEntryKind::Directory, None)
    } else if file_type.is_file() {
        (RootfsEntryKind::Regular, None)
    } else if file_type.is_symlink() {
        let target = std::fs::read_link(source).map_err(BoxError::IoError)?;
        (
            RootfsEntryKind::Symlink,
            Some(base64::engine::general_purpose::STANDARD.encode(target.as_os_str().as_bytes())),
        )
    } else {
        let kind = if file_type.is_fifo() {
            "fifo"
        } else if file_type.is_socket() {
            "socket"
        } else if file_type.is_char_device() {
            "character device"
        } else if file_type.is_block_device() {
            "block device"
        } else {
            "unknown"
        };
        return Err(BoxError::OciImageError(format!(
            "Sandbox Snapshot rootfs contains unsupported special file {} ({kind})",
            source.display()
        )));
    };
    entries.push(RootfsMetadataEntry {
        path_base64: base64::engine::general_purpose::STANDARD
            .encode(manifest_path.as_os_str().as_bytes()),
        kind,
        mode: metadata.mode(),
        uid: unmap_host_id(&plan.uid_mappings, metadata.uid(), "UID")? as u64,
        gid: unmap_host_id(&plan.gid_mappings, metadata.gid(), "GID")? as u64,
        mtime: metadata.mtime().max(0) as u64,
        size: metadata.size(),
        link_target_base64,
    });

    if file_type.is_dir() {
        let mut children: Vec<_> = std::fs::read_dir(source)
            .map_err(BoxError::IoError)?
            .collect::<std::result::Result<_, _>>()
            .map_err(BoxError::IoError)?;
        children.sort_by_key(std::fs::DirEntry::file_name);
        for child in children {
            collect_snapshot_rootfs_metadata(
                root,
                &child.path(),
                &manifest_path.join(child.file_name()),
                plan,
                entries,
            )?;
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn unmap_host_id(mappings: &[IdMapping], id: u32, kind: &str) -> Result<u32> {
    for mapping in mappings {
        let Some(end) = mapping.host_id.checked_add(mapping.size) else {
            continue;
        };
        if mapping.host_id <= id && id < end {
            return mapping
                .container_id
                .checked_add(id - mapping.host_id)
                .ok_or_else(|| {
                    BoxError::ConfigError(format!(
                        "Sandbox Snapshot {kind} reverse mapping overflows u32"
                    ))
                });
        }
    }
    Err(BoxError::ConfigError(format!(
        "Sandbox Snapshot host {kind} {id} is outside the OCI mappings"
    )))
}

#[cfg(not(target_os = "linux"))]
pub fn prepare_rootfs_ownership(
    _root: &Path,
    _plan: &SandboxIdMappingPlan,
    _effective_uid: u32,
    _read_only: bool,
) -> Result<()> {
    Err(BoxError::ConfigError(
        "Sandbox rootfs ownership preparation requires Linux".to_string(),
    ))
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn prepare_rootfs_ownership_with_preference(
    root: &Path,
    plan: &SandboxIdMappingPlan,
    effective_uid: u32,
    read_only: bool,
    _prefer_image_manifest: bool,
) -> Result<()> {
    prepare_rootfs_ownership(root, plan, effective_uid, read_only)
}

fn load_authoritative_manifest(
    root: &Path,
    prefer_image_manifest: bool,
) -> Result<(PathBuf, RootfsMetadataManifest)> {
    let terminal = root.join(ROOTFS_METADATA_PATH.trim_start_matches('/'));
    let previous = root.join(PREVIOUS_ROOTFS_METADATA_PATH.trim_start_matches('/'));
    let image = root.join(IMAGE_ROOTFS_METADATA_PATH.trim_start_matches('/'));
    let candidates = if prefer_image_manifest {
        // A freshly composed rootfs must not trust lifecycle markers that an
        // image layer may have baked into the filesystem.
        [image, terminal, previous]
    } else {
        // The CLI moves the clean-shutdown marker to `previous` before boot.
        // Retain support for direct runtime callers that have not staged the
        // legacy terminal marker yet; guest-init performs the one-shot cleanup
        // only after replay succeeds.
        [terminal, previous, image]
    };

    for candidate in &candidates {
        if let Some(manifest) = load_manifest_if_present(candidate)? {
            return Ok((candidate.clone(), manifest));
        }
    }

    Err(BoxError::BoxBootError {
        message: format!(
            "Sandbox rootfs metadata is unavailable at {}, {}, or {}",
            candidates[0].display(),
            candidates[1].display(),
            candidates[2].display()
        ),
        hint: Some("Rebuild the per-box rootfs from its OCI image".to_string()),
    })
}

fn load_manifest_if_present(path: &Path) -> Result<Option<RootfsMetadataManifest>> {
    let mut file = match open_regular_file_no_follow(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(BoxError::BoxBootError {
                message: format!(
                    "Sandbox rootfs metadata is not a safe regular file at {}: {error}",
                    path.display()
                ),
                hint: Some("Rebuild the per-box rootfs from its OCI image".to_string()),
            });
        }
    };
    let length = file.metadata().map_err(BoxError::IoError)?.len();
    if length > MAX_ROOTFS_METADATA_BYTES {
        return Err(BoxError::OciImageError(format!(
            "Sandbox rootfs metadata {} exceeds the {} byte limit",
            path.display(),
            MAX_ROOTFS_METADATA_BYTES
        )));
    }

    let mut bytes = Vec::with_capacity(length as usize);
    Read::by_ref(&mut file)
        .take(MAX_ROOTFS_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| BoxError::BoxBootError {
            message: format!(
                "Failed to read Sandbox rootfs metadata {}: {error}",
                path.display()
            ),
            hint: Some("Rebuild the per-box rootfs from its OCI image".to_string()),
        })?;
    if bytes.len() as u64 > MAX_ROOTFS_METADATA_BYTES {
        return Err(BoxError::OciImageError(format!(
            "Sandbox rootfs metadata {} exceeds the {} byte limit",
            path.display(),
            MAX_ROOTFS_METADATA_BYTES
        )));
    }
    let manifest: RootfsMetadataManifest = serde_json::from_slice(&bytes).map_err(|error| {
        BoxError::OciImageError(format!(
            "Invalid Sandbox rootfs metadata {}: {error}",
            path.display()
        ))
    })?;
    manifest.validate().map_err(BoxError::OciImageError)?;
    Ok(Some(manifest))
}

#[cfg(unix)]
fn open_regular_file_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "rootfs metadata path is not a regular file",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn open_regular_file_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    a3s_box_core::windows_file::open_regular_file(path, None).map(|(file, _)| file)
}

#[cfg(not(any(unix, windows)))]
fn open_regular_file_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "rootfs metadata path is not a regular file",
        ));
    }
    std::fs::File::open(path)
}

#[cfg(target_os = "linux")]
fn decode_and_validate_entries(
    root: &Path,
    manifest: RootfsMetadataManifest,
) -> Result<Vec<DecodedEntry>> {
    let mut decoded = Vec::with_capacity(manifest.entries.len());
    let mut unique = HashSet::with_capacity(manifest.entries.len());
    for metadata in manifest.entries {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&metadata.path_base64)
            .map_err(|error| {
                BoxError::OciImageError(format!("Invalid rootfs metadata path: {error}"))
            })?;
        // Manifests are produced and consumed on the same host, so the encoded
        // platform path bytes can be reconstructed losslessly.
        let encoded = unsafe { std::ffi::OsString::from_encoded_bytes_unchecked(raw) };
        let relative = safe_relative_path(Path::new(&encoded))?;
        if relative == Path::new(IMAGE_ROOTFS_METADATA_PATH.trim_start_matches('/'))
            || relative == Path::new(ROOTFS_METADATA_PATH.trim_start_matches('/'))
            || relative == Path::new(PREVIOUS_ROOTFS_METADATA_PATH.trim_start_matches('/'))
            || !unique.insert(relative.clone())
        {
            return Err(BoxError::OciImageError(
                "Duplicate or reserved Sandbox rootfs metadata path".to_string(),
            ));
        }
        let target = resolve_without_symlink_parent(root, &relative)?;
        let filesystem =
            std::fs::symlink_metadata(&target).map_err(|error| BoxError::BoxBootError {
                message: format!(
                    "Sandbox rootfs metadata target {} is unavailable: {error}",
                    target.display()
                ),
                hint: None,
            })?;
        let actual_kind = if filesystem.file_type().is_dir() {
            RootfsEntryKind::Directory
        } else if filesystem.file_type().is_file() {
            RootfsEntryKind::Regular
        } else if filesystem.file_type().is_symlink() {
            RootfsEntryKind::Symlink
        } else {
            return Err(BoxError::OciImageError(format!(
                "Unsupported rootfs entry at {}",
                target.display()
            )));
        };
        if actual_kind != metadata.kind {
            return Err(BoxError::OciImageError(format!(
                "Sandbox rootfs metadata type mismatch at {}",
                target.display()
            )));
        }
        if actual_kind == RootfsEntryKind::Symlink {
            let expected = metadata.link_target_base64.as_ref().ok_or_else(|| {
                BoxError::OciImageError("Symlink metadata is missing its target".to_string())
            })?;
            let expected = base64::engine::general_purpose::STANDARD
                .decode(expected)
                .map_err(|error| {
                    BoxError::OciImageError(format!("Invalid symlink target metadata: {error}"))
                })?;
            if std::fs::read_link(&target)
                .map_err(BoxError::IoError)?
                .as_os_str()
                .as_encoded_bytes()
                != expected
            {
                return Err(BoxError::OciImageError(format!(
                    "Sandbox rootfs symlink mismatch at {}",
                    target.display()
                )));
            }
        }
        decoded.push(DecodedEntry {
            metadata,
            relative,
            target,
        });
    }
    Ok(decoded)
}

#[cfg(any(target_os = "linux", test))]
fn safe_relative_path(path: &Path) -> Result<PathBuf> {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => result.push(name),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(BoxError::OciImageError(
                    "Unsafe Sandbox rootfs metadata path".to_string(),
                ))
            }
        }
    }
    Ok(result)
}

#[cfg(target_os = "linux")]
fn resolve_without_symlink_parent(root: &Path, relative: &Path) -> Result<PathBuf> {
    let mut current = root.to_path_buf();
    let components: Vec<_> = relative.components().collect();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            continue;
        };
        current.push(name);
        if index + 1 < components.len()
            && std::fs::symlink_metadata(&current)
                .map_err(BoxError::IoError)?
                .file_type()
                .is_symlink()
        {
            return Err(BoxError::OciImageError(format!(
                "Symlink parent in Sandbox rootfs metadata path: {}",
                current.display()
            )));
        }
    }
    Ok(current)
}

#[cfg(target_os = "linux")]
fn shift_unlisted_entries(
    root: &Path,
    source: &Path,
    authoritative: &HashSet<PathBuf>,
    plan: &SandboxIdMappingPlan,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let relative = source
        .strip_prefix(root)
        .map_err(|_| BoxError::OciImageError("Sandbox rootfs walk escaped its root".to_string()))?;
    let metadata = std::fs::symlink_metadata(source).map_err(BoxError::IoError)?;
    if !authoritative.contains(relative) {
        let uid = map_current_or_container_id(&plan.uid_mappings, metadata.uid(), "UID")?;
        let gid = map_current_or_container_id(&plan.gid_mappings, metadata.gid(), "GID")?;
        lchown_if_needed(source, uid, gid)?;
    }
    if metadata.file_type().is_dir() {
        for child in std::fs::read_dir(source).map_err(BoxError::IoError)? {
            shift_unlisted_entries(
                root,
                &child.map_err(BoxError::IoError)?.path(),
                authoritative,
                plan,
            )?;
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn prepare_managed_tree(
    path: &Path,
    plan: &SandboxIdMappingPlan,
    root_uid: u32,
    root_gid: u32,
) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = std::fs::symlink_metadata(path).map_err(BoxError::IoError)?;
    let uid = if id_is_mapped(&plan.uid_mappings, metadata.uid()) {
        metadata.uid()
    } else {
        root_uid
    };
    let gid = if id_is_mapped(&plan.gid_mappings, metadata.gid()) {
        metadata.gid()
    } else {
        root_gid
    };
    let mode = metadata.mode() & 0o7777;
    lchown_if_needed(path, uid, gid)?;
    if !metadata.file_type().is_symlink() {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(BoxError::IoError)?;
    }
    if metadata.file_type().is_dir() {
        for child in std::fs::read_dir(path).map_err(BoxError::IoError)? {
            prepare_managed_tree(
                &child.map_err(BoxError::IoError)?.path(),
                plan,
                root_uid,
                root_gid,
            )?;
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn id_is_mapped(mappings: &[IdMapping], id: u32) -> bool {
    mappings.iter().any(|mapping| {
        mapping
            .host_id
            .checked_add(mapping.size)
            .is_some_and(|end| mapping.host_id <= id && id < end)
    })
}

#[cfg(target_os = "linux")]
fn ensure_no_nested_mounts(root: &Path) -> Result<()> {
    let root = root.canonicalize().map_err(BoxError::IoError)?;
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").map_err(BoxError::IoError)?;
    for mount in mountinfo
        .lines()
        .filter_map(|line| line.split_whitespace().nth(4))
        .map(decode_mountinfo_path)
        .map(PathBuf::from)
    {
        if mount != root && mount.starts_with(&root) {
            return Err(BoxError::BoxBootError {
                message: format!(
                    "Refusing Sandbox ownership preparation across nested mount {} under {}",
                    mount.display(),
                    root.display()
                ),
                hint: Some("Reconcile the stale mount before restarting the Sandbox".to_string()),
            });
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn decode_mountinfo_path(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

#[cfg(target_os = "linux")]
fn map_current_or_container_id(mappings: &[IdMapping], id: u32, kind: &str) -> Result<u32> {
    if mappings.iter().any(|mapping| {
        mapping
            .host_id
            .checked_add(mapping.size)
            .is_some_and(|end| mapping.host_id <= id && id < end)
    }) {
        return Ok(id);
    }
    map_container_id(mappings, id, kind)
}

fn map_container_id(mappings: &[IdMapping], id: u32, kind: &str) -> Result<u32> {
    for mapping in mappings {
        let Some(end) = mapping.container_id.checked_add(mapping.size) else {
            continue;
        };
        if mapping.container_id <= id && id < end {
            return mapping
                .host_id
                .checked_add(id - mapping.container_id)
                .ok_or_else(|| {
                    BoxError::ConfigError(format!("Sandbox {kind} mapping overflows u32"))
                });
        }
    }
    Err(BoxError::ConfigError(format!(
        "Sandbox {kind} mappings do not cover container ID {id}"
    )))
}

#[cfg(target_os = "linux")]
fn lchown_if_needed(path: &Path, uid: u32, gid: u32) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    let current = std::fs::symlink_metadata(path).map_err(BoxError::IoError)?;
    if current.uid() == uid && current.gid() == gid {
        return Ok(());
    }
    let path_bytes = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        BoxError::OciImageError(format!(
            "NUL byte in Sandbox rootfs path {}",
            path.display()
        ))
    })?;
    if unsafe { libc::lchown(path_bytes.as_ptr(), uid, gid) } != 0 {
        return Err(BoxError::BoxBootError {
            message: format!(
                "Failed to map Sandbox rootfs ownership at {} to {uid}:{gid}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ),
            hint: None,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::rootfs_metadata::ROOTFS_METADATA_SCHEMA;

    fn test_manifest(uid: u64, gid: u64) -> RootfsMetadataManifest {
        RootfsMetadataManifest {
            schema: ROOTFS_METADATA_SCHEMA.to_string(),
            entries: vec![RootfsMetadataEntry {
                path_base64: base64::engine::general_purpose::STANDARD.encode("."),
                kind: RootfsEntryKind::Directory,
                mode: 0o755,
                uid,
                gid,
                mtime: 0,
                size: 0,
                link_target_base64: None,
            }],
        }
    }

    #[test]
    fn mapping_translation_is_complete_and_exact() {
        let mappings = vec![
            IdMapping {
                container_id: 0,
                host_id: 100_000,
                size: 10,
            },
            IdMapping {
                container_id: 10,
                host_id: 200_000,
                size: 6,
            },
        ];
        assert_eq!(map_container_id(&mappings, 0, "UID").unwrap(), 100_000);
        assert_eq!(map_container_id(&mappings, 12, "UID").unwrap(), 200_002);
        assert!(map_container_id(&mappings, 16, "UID").is_err());
    }

    #[test]
    fn terminal_manifest_takes_precedence_for_identity_planning() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory
                .path()
                .join(IMAGE_ROOTFS_METADATA_PATH.trim_start_matches('/')),
            serde_json::to_vec(&test_manifest(1, 2)).unwrap(),
        )
        .unwrap();
        std::fs::write(
            directory
                .path()
                .join(ROOTFS_METADATA_PATH.trim_start_matches('/')),
            serde_json::to_vec(&test_manifest(42, 43)).unwrap(),
        )
        .unwrap();

        let requirements = inspect_rootfs_identity_requirements(directory.path()).unwrap();
        assert_eq!(requirements.maximum_uid, 42);
        assert_eq!(requirements.maximum_gid, 43);
        assert!(requirements
            .manifest_path
            .ends_with(".a3s_rootfs_metadata_v1.json"));
    }

    #[test]
    fn staged_previous_manifest_takes_precedence_over_image() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory
                .path()
                .join(IMAGE_ROOTFS_METADATA_PATH.trim_start_matches('/')),
            serde_json::to_vec(&test_manifest(1, 2)).unwrap(),
        )
        .unwrap();
        std::fs::write(
            directory
                .path()
                .join(PREVIOUS_ROOTFS_METADATA_PATH.trim_start_matches('/')),
            serde_json::to_vec(&test_manifest(42, 43)).unwrap(),
        )
        .unwrap();

        let requirements = inspect_rootfs_identity_requirements(directory.path()).unwrap();
        assert_eq!(requirements.maximum_uid, 42);
        assert_eq!(requirements.maximum_gid, 43);
        assert!(requirements
            .manifest_path
            .ends_with(".a3s_rootfs_metadata_v1.previous.json"));
    }

    #[test]
    fn fresh_rootfs_prefers_image_manifest_for_identity_planning() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory
                .path()
                .join(IMAGE_ROOTFS_METADATA_PATH.trim_start_matches('/')),
            serde_json::to_vec(&test_manifest(7, 8)).unwrap(),
        )
        .unwrap();
        std::fs::write(
            directory
                .path()
                .join(ROOTFS_METADATA_PATH.trim_start_matches('/')),
            serde_json::to_vec(&test_manifest(42, 43)).unwrap(),
        )
        .unwrap();
        std::fs::write(
            directory
                .path()
                .join(PREVIOUS_ROOTFS_METADATA_PATH.trim_start_matches('/')),
            serde_json::to_vec(&test_manifest(99, 100)).unwrap(),
        )
        .unwrap();

        let requirements =
            inspect_rootfs_identity_requirements_with_preference(directory.path(), true).unwrap();
        assert_eq!(requirements.maximum_uid, 7);
        assert_eq!(requirements.maximum_gid, 8);
        assert!(requirements
            .manifest_path
            .ends_with(".a3s_image_metadata_v1.json"));
    }

    #[cfg(unix)]
    #[test]
    fn terminal_manifest_symlink_is_rejected_without_following() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let outside = directory.path().join("outside.json");
        let outside_bytes = serde_json::to_vec(&test_manifest(99, 100)).unwrap();
        std::fs::write(&outside, &outside_bytes).unwrap();
        symlink(
            &outside,
            directory
                .path()
                .join(ROOTFS_METADATA_PATH.trim_start_matches('/')),
        )
        .unwrap();

        let error = inspect_rootfs_identity_requirements(directory.path()).unwrap_err();
        assert!(error.to_string().contains("not a safe regular file"));
        assert_eq!(std::fs::read(outside).unwrap(), outside_bytes);
    }

    #[cfg(unix)]
    #[test]
    fn fresh_rootfs_ignores_baked_terminal_manifest_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory
                .path()
                .join(IMAGE_ROOTFS_METADATA_PATH.trim_start_matches('/')),
            serde_json::to_vec(&test_manifest(7, 8)).unwrap(),
        )
        .unwrap();
        let outside = directory.path().join("outside.json");
        std::fs::write(
            &outside,
            serde_json::to_vec(&test_manifest(99, 100)).unwrap(),
        )
        .unwrap();
        symlink(
            &outside,
            directory
                .path()
                .join(ROOTFS_METADATA_PATH.trim_start_matches('/')),
        )
        .unwrap();

        let requirements =
            inspect_rootfs_identity_requirements_with_preference(directory.path(), true).unwrap();
        assert_eq!(requirements.maximum_uid, 7);
        assert_eq!(requirements.maximum_gid, 8);
        assert!(requirements
            .manifest_path
            .ends_with(".a3s_image_metadata_v1.json"));
    }

    #[test]
    fn oversized_rootfs_manifest_is_rejected_before_reading() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory
            .path()
            .join(ROOTFS_METADATA_PATH.trim_start_matches('/'));
        let file = std::fs::File::create(path).unwrap();
        file.set_len(MAX_ROOTFS_METADATA_BYTES + 1).unwrap();

        let error = inspect_rootfs_identity_requirements(directory.path()).unwrap_err();
        assert!(error
            .to_string()
            .contains("exceeds the 67108864 byte limit"));
    }

    #[test]
    fn unsafe_manifest_path_is_rejected() {
        assert!(safe_relative_path(Path::new("../escape")).is_err());
        assert!(safe_relative_path(Path::new("/host")).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ownership_preparation_keeps_runtime_managed_files_readable() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = tempfile::tempdir().unwrap();
        let etc = directory.path().join("etc");
        std::fs::create_dir(&etc).unwrap();
        let hosts = etc.join("hosts");
        let probe = etc.join("probe");
        let init = directory.path().join("usr/sbin/init");
        std::fs::create_dir_all(init.parent().unwrap()).unwrap();
        std::fs::write(&hosts, "127.0.0.1 localhost\n").unwrap();
        std::fs::write(&probe, "probe\n").unwrap();
        for path in [&hosts, &probe] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        std::fs::write(&init, "guest init\n").unwrap();
        std::fs::set_permissions(&init, std::fs::Permissions::from_mode(0o755)).unwrap();

        let owner = std::fs::metadata(directory.path()).unwrap();
        let entry = |path: &str, size: u64| RootfsMetadataEntry {
            path_base64: base64::engine::general_purpose::STANDARD.encode(path),
            kind: RootfsEntryKind::Regular,
            mode: 0o100600,
            uid: 0,
            gid: 0,
            mtime: 0,
            size,
            link_target_base64: None,
        };
        let manifest = RootfsMetadataManifest {
            schema: ROOTFS_METADATA_SCHEMA.to_string(),
            entries: vec![
                entry("./etc/hosts", 20),
                entry("./etc/probe", 6),
                entry("./usr/sbin/init", 1),
            ],
        };
        std::fs::write(
            directory
                .path()
                .join(IMAGE_ROOTFS_METADATA_PATH.trim_start_matches('/')),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let plan = SandboxIdMappingPlan {
            uid_mappings: vec![IdMapping {
                container_id: 0,
                host_id: owner.uid(),
                size: 1,
            }],
            gid_mappings: vec![IdMapping {
                container_id: 0,
                host_id: owner.gid(),
                size: 1,
            }],
            maximum_container_uid: 0,
            maximum_container_gid: 0,
        };

        prepare_rootfs_ownership(directory.path(), &plan, 0, false).unwrap();

        assert_eq!(
            std::fs::metadata(hosts).unwrap().permissions().mode() & 0o7777,
            0o644
        );
        assert_eq!(
            std::fs::metadata(probe).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(init).unwrap().permissions().mode() & 0o7777,
            0o755
        );
    }
}
