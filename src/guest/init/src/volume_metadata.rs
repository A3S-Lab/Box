//! Capture and replay uid, gid, and mode for virtio-fs shares.
//!
//! The manifest lives on the Box-owned rootfs. Replay uses chmod and lchown,
//! which Windows virtio-fs stores in its inode table. This module does not
//! write ownership onto the host filesystem itself.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use a3s_box_core::rootfs_metadata::{RootfsEntryKind, RootfsMetadataEntry};
use a3s_box_core::volume_posix::{
    validate_guest_mount, VolumePosixManifest, VolumePosixMount, VOLUME_POSIX_METADATA_ENV,
    VOLUME_POSIX_METADATA_FILE, VOLUME_POSIX_METADATA_MAX_BYTES, VOLUME_POSIX_METADATA_MAX_ENTRIES,
    VOLUME_POSIX_METADATA_PENDING_FILE, VOLUME_POSIX_METADATA_PENDING_TEMP_FILE,
    VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE, VOLUME_POSIX_METADATA_TEMP_FILE,
};
use base64::Engine as _;

/// Record workspace and `BOX_VOL_*` shares mounted on `root`.
pub fn persist_configured_volume_posix_metadata(
    root: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut mounts = Vec::new();
    let workspace = root.join("workspace");
    if workspace.symlink_metadata().is_ok() {
        mounts.push((String::from("/workspace"), workspace));
    }
    let mut index = 0usize;
    loop {
        let key = format!("BOX_VOL_{index}");
        let Ok(value) = std::env::var(&key) else {
            break;
        };
        let guest_path = guest_path_from_volume_spec(&value)?;
        validate_guest_mount(&guest_path)?;
        let relative = guest_path.trim_start_matches('/').to_string();
        mounts.push((guest_path, root.join(relative)));
        index += 1;
    }
    persist_volume_posix_metadata(root, &mounts)
}

/// Write one manifest for the given `(guest path, filesystem path)` shares.
pub fn persist_volume_posix_metadata(
    root: &Path,
    mounts: &[(String, PathBuf)],
) -> Result<(), Box<dyn std::error::Error>> {
    let previous = read_published_manifest(root);
    let mut recorded = Vec::with_capacity(mounts.len());
    let mut total = 0usize;
    let filesystem_roots: Vec<&Path> = mounts.iter().map(|(_, path)| path.as_path()).collect();
    for (guest_path, filesystem_path) in mounts {
        validate_guest_mount(guest_path)?;
        let mut entries = Vec::new();
        let total_before = total;
        let complete = collect_share(
            filesystem_path,
            Path::new("."),
            &mut entries,
            &mut total,
            &filesystem_roots,
        )?;
        if !complete {
            total = total_before;
            if let Some(mount) = previous.as_ref().and_then(|manifest| {
                manifest
                    .mounts
                    .iter()
                    .find(|mount| mount.guest_path == *guest_path)
            }) {
                recorded.push(VolumePosixMount::retained(
                    guest_path.clone(),
                    mount.entries.clone(),
                ));
            }
            continue;
        }
        if entries.is_empty() {
            continue;
        }
        entries.sort_by(|left, right| left.path_base64.cmp(&right.path_base64));
        recorded.push(VolumePosixMount {
            guest_path: guest_path.clone(),
            entries,
            retained: false,
        });
    }
    recorded.sort_by(|left, right| left.guest_path.cmp(&right.guest_path));
    retain_unapplied_entries(root, &mut recorded);
    write_manifest(root, &VolumePosixManifest::new(recorded))?;
    remove_pending_node(&root.join(VOLUME_POSIX_METADATA_PENDING_FILE))?;
    remove_pending_node(&root.join(VOLUME_POSIX_METADATA_PENDING_TEMP_FILE))?;
    sync_directory(root)
}

/// Replay a previously published manifest, then remove it.
///
/// A missing manifest is the first boot and a crash before clean shutdown.
/// A missing path, a kind mismatch, a symlink parent, a path whose parent
/// is not a directory, or an owner/mode restore that the guest cannot apply
/// is skipped so one stale entry does not fail PID 1. Later entries are
/// still applied. An entry whose
/// owner/mode could not be applied is recorded beside the manifest. The
/// published manifest stays in place until the next capture, so a stop
/// before that capture does not shrink a sidecar to the failed entries.
/// The next capture keeps the unapplied uid, gid, and mode.
pub fn restore_volume_posix_metadata(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    restore_volume_posix_metadata_with_shares(root, &configured_guest_paths(root))
}

fn configured_guest_paths(root: &Path) -> Vec<String> {
    let mut paths = Vec::new();
    if root.join("workspace").symlink_metadata().is_ok() {
        paths.push(String::from("/workspace"));
    }
    let mut index = 0usize;
    loop {
        let key = format!("BOX_VOL_{index}");
        let Ok(value) = std::env::var(&key) else {
            break;
        };
        index += 1;
        let Ok(guest_path) = guest_path_from_volume_spec(&value) else {
            continue;
        };
        if validate_guest_mount(&guest_path).is_err() {
            continue;
        }
        if paths.iter().any(|path| path == &guest_path) {
            continue;
        }
        paths.push(guest_path);
    }
    paths
}

fn restore_volume_posix_metadata_with_shares(
    root: &Path,
    share_paths: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(manifest) = newest_valid_capture(root)? else {
        return Ok(());
    };
    let mut pending_mounts = Vec::new();
    for mount in &manifest.mounts {
        let pending = apply_mount(root, mount, &manifest.mounts, share_paths)?;
        if !pending.is_empty() {
            pending_mounts.push(VolumePosixMount {
                guest_path: mount.guest_path.clone(),
                entries: pending,
                retained: false,
            });
        }
    }
    if pending_mounts.is_empty() {
        // A newer temp or publish file is the capture harvest still has to
        // read. An older copy is removed first, so a stop after this replay
        // does not replace the replayed mode with that copy.
        let keep_temp = replayed_temp_is_the_remaining_capture(root)?;
        let keep_publish =
            file_is_remaining_capture(root, VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE)?;
        if !keep_temp {
            remove_pending_node(&root.join(VOLUME_POSIX_METADATA_TEMP_FILE))?;
        }
        if !keep_publish {
            remove_pending_node(&root.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE))?;
        }
        remove_pending_node(&root.join(VOLUME_POSIX_METADATA_FILE))?;
        remove_pending_node(&root.join(VOLUME_POSIX_METADATA_PENDING_FILE))?;
        remove_pending_node(&root.join(VOLUME_POSIX_METADATA_PENDING_TEMP_FILE))?;
        sync_directory(root)?;
        return Ok(());
    }
    write_named_manifest(
        root,
        VOLUME_POSIX_METADATA_PENDING_FILE,
        &VolumePosixManifest::new(pending_mounts),
    )
}

fn write_manifest(
    root: &Path,
    manifest: &VolumePosixManifest,
) -> Result<(), Box<dyn std::error::Error>> {
    write_named_manifest(root, VOLUME_POSIX_METADATA_FILE, manifest)
}

fn write_named_manifest(
    root: &Path,
    file_name: &str,
    manifest: &VolumePosixManifest,
) -> Result<(), Box<dyn std::error::Error>> {
    manifest.validate()?;
    let encoded = serde_json::to_vec(manifest)?;
    if encoded.len() as u64 > VOLUME_POSIX_METADATA_MAX_BYTES {
        return Err(format!(
            "volume posix metadata exceeds {VOLUME_POSIX_METADATA_MAX_BYTES} bytes"
        )
        .into());
    }

    let destination = root.join(file_name);
    // The durable temp and a synced publish are captures whose rename did
    // not finish. Publishing the next manifest must not truncate either
    // file until the new manifest is in place. Pending replay keeps its
    // own scratch.
    let preserve_temp =
        file_name == VOLUME_POSIX_METADATA_FILE && replayed_temp_is_the_remaining_capture(root)?;
    let preserve_publish = file_name == VOLUME_POSIX_METADATA_FILE
        && file_is_remaining_capture(root, VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE)?;
    let preferred = if preserve_publish {
        root.join(VOLUME_POSIX_METADATA_PENDING_TEMP_FILE)
    } else if file_name == VOLUME_POSIX_METADATA_FILE && !preserve_temp {
        root.join(VOLUME_POSIX_METADATA_TEMP_FILE)
    } else if file_name == VOLUME_POSIX_METADATA_FILE {
        root.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE)
    } else {
        root.join(VOLUME_POSIX_METADATA_PENDING_TEMP_FILE)
    };
    let mut candidates = vec![preferred];
    for extra_name in [
        VOLUME_POSIX_METADATA_TEMP_FILE,
        VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE,
        VOLUME_POSIX_METADATA_PENDING_TEMP_FILE,
    ] {
        if preserve_temp && extra_name == VOLUME_POSIX_METADATA_TEMP_FILE {
            continue;
        }
        if preserve_publish && extra_name == VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE {
            continue;
        }
        if extra_name == file_name {
            continue;
        }
        let extra = root.join(extra_name);
        if !candidates.iter().any(|candidate| candidate == &extra) {
            candidates.push(extra);
        }
    }
    let mut chosen = None;
    for candidate in &candidates {
        if prepare_metadata_scratch(candidate)? {
            chosen = Some(candidate.clone());
            break;
        }
    }
    let Some(temporary) = chosen else {
        return Err("volume posix metadata scratch is not a writable file".into());
    };
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)?;
        file.write_all(&encoded)?;
        file.sync_all()?;
    }
    place_manifest(
        &temporary,
        &destination,
        root,
        preserve_temp,
        preserve_publish,
    )?;
    // The preserved temp or publish is retired only after the committed
    // name itself holds the new manifest. A planted directory there keeps
    // those captures.
    if destination.is_file() && preserve_temp {
        remove_if_present(&root.join(VOLUME_POSIX_METADATA_TEMP_FILE))?;
    }
    if destination.is_file() && preserve_publish {
        remove_if_present(&root.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE))?;
    }
    sync_directory(root)?;
    Ok(())
}

fn place_manifest(
    temporary: &Path,
    destination: &Path,
    root: &Path,
    preserve_temp: bool,
    preserve_publish: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // A nonempty directory at the committed name is a planted tree. The
    // capture is renamed beside it. An empty directory is removed so the
    // committed name can hold the file.
    if nonempty_real_directory(destination)? {
        for name in [
            VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE,
            VOLUME_POSIX_METADATA_TEMP_FILE,
            VOLUME_POSIX_METADATA_PENDING_TEMP_FILE,
        ] {
            if preserve_publish && name == VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE {
                continue;
            }
            if preserve_temp && name == VOLUME_POSIX_METADATA_TEMP_FILE {
                continue;
            }
            let candidate = root.join(name);
            if candidate == temporary || nonempty_real_directory(&candidate)? {
                continue;
            }
            if real_directory(&candidate)? {
                std::fs::remove_dir(&candidate)?;
            }
            std::fs::rename(temporary, &candidate)?;
            return Ok(());
        }
        return Err("volume posix metadata destination is not a writable file".into());
    }
    if real_directory(destination)? {
        std::fs::remove_dir(destination)?;
    }
    std::fs::rename(temporary, destination)?;
    Ok(())
}

fn real_directory(path: &Path) -> Result<bool, Box<dyn std::error::Error>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn nonempty_real_directory(path: &Path) -> Result<bool, Box<dyn std::error::Error>> {
    if !real_directory(path)? {
        return Ok(false);
    }
    let mut entries = std::fs::read_dir(path)?;
    Ok(entries.next().transpose()?.is_some())
}

fn read_published_manifest(root: &Path) -> Option<VolumePosixManifest> {
    newest_valid_capture(root).ok().flatten()
}

/// The newest valid manifest or durable temp capture.
///
/// A temp file is the capture whose rename did not finish. A publish scratch
/// is that capture when the durable temp had to be kept. An older copy does
/// not replace a newer one. A corrupt copy does not hide a valid manifest,
/// and a corrupt manifest still fails when nothing valid remains. An
/// oversized committed manifest does not hide a valid temp or publish
/// capture. The size error remains when nothing valid is left. The oversized
/// bytes are not read.
fn newest_valid_capture(
    root: &Path,
) -> Result<Option<VolumePosixManifest>, Box<dyn std::error::Error>> {
    let mut newest: Option<(std::time::SystemTime, VolumePosixManifest)> = None;
    let mut saw_invalid_manifest = false;
    let mut size_error: Option<String> = None;
    let mut non_regular_error: Option<String> = None;
    for file_name in [
        VOLUME_POSIX_METADATA_FILE,
        VOLUME_POSIX_METADATA_TEMP_FILE,
        VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE,
    ] {
        let path = root.join(file_name);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() {
            if file_name == VOLUME_POSIX_METADATA_FILE {
                non_regular_error = Some(format!("{} is not a regular file", path.display()));
                saw_invalid_manifest = true;
            }
            continue;
        }
        let length = metadata.len();
        if length > VOLUME_POSIX_METADATA_MAX_BYTES {
            if file_name == VOLUME_POSIX_METADATA_FILE {
                size_error = Some(format!(
                    "volume posix metadata exceeds {VOLUME_POSIX_METADATA_MAX_BYTES} bytes"
                ));
                saw_invalid_manifest = true;
            }
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let parsed = serde_json::from_slice::<VolumePosixManifest>(&bytes)
            .ok()
            .filter(|manifest| manifest.validate().is_ok());
        let Some(manifest) = parsed else {
            if file_name == VOLUME_POSIX_METADATA_FILE {
                saw_invalid_manifest = true;
            }
            continue;
        };
        let modified = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let replace = newest
            .as_ref()
            .map(|(time, _)| modified > *time)
            .unwrap_or(true);
        if replace {
            newest = Some((modified, manifest));
        }
    }
    if newest.is_none() {
        if let Some(error) = size_error {
            return Err(error.into());
        }
        if let Some(error) = non_regular_error {
            return Err(error.into());
        }
    }
    if newest.is_none() && saw_invalid_manifest {
        return Err("volume posix metadata is not a valid manifest".into());
    }
    Ok(newest.map(|(_, manifest)| manifest))
}

fn replayed_temp_is_the_remaining_capture(root: &Path) -> Result<bool, Box<dyn std::error::Error>> {
    file_is_remaining_capture(root, VOLUME_POSIX_METADATA_TEMP_FILE)
}

fn file_is_remaining_capture(
    root: &Path,
    file_name: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    let temp_path = root.join(file_name);
    let temp_metadata = match std::fs::symlink_metadata(&temp_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !temp_metadata.is_file() || temp_metadata.len() > VOLUME_POSIX_METADATA_MAX_BYTES {
        return Ok(false);
    }
    let temp_bytes = match std::fs::read(&temp_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let Ok(temp) = serde_json::from_slice::<VolumePosixManifest>(&temp_bytes) else {
        return Ok(false);
    };
    if temp.validate().is_err() {
        return Ok(false);
    }
    let temp_mtime = std::fs::metadata(&temp_path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    let manifest_path = root.join(VOLUME_POSIX_METADATA_FILE);
    let manifest_metadata = match std::fs::symlink_metadata(&manifest_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
    };
    if !manifest_metadata.is_file() || manifest_metadata.len() > VOLUME_POSIX_METADATA_MAX_BYTES {
        return Ok(true);
    }
    let manifest_bytes = match std::fs::read(&manifest_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
    };
    let manifest_valid = serde_json::from_slice::<VolumePosixManifest>(&manifest_bytes)
        .ok()
        .is_some_and(|manifest| manifest.validate().is_ok());
    if !manifest_valid {
        return Ok(true);
    }
    let manifest_mtime = std::fs::metadata(&manifest_path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    Ok(temp_mtime > manifest_mtime)
}

fn prepare_metadata_scratch(path: &Path) -> Result<bool, Box<dyn std::error::Error>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        let result = if metadata.is_dir() {
            std::fs::remove_dir(path)
        } else {
            std::fs::remove_file(path)
        };
        return result.map(|()| true).map_err(Into::into);
    }
    if metadata.is_file() {
        return Ok(true);
    }
    if metadata.is_dir() {
        let mut entries = std::fs::read_dir(path)?;
        if entries.next().transpose()?.is_some() {
            return Ok(false);
        }
        std::fs::remove_dir(path)?;
        return Ok(true);
    }
    Err(format!("{} is not a writable metadata scratch", path.display()).into())
}

fn remove_if_present(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Drop a node after a successful persist or replay.
///
/// A regular file or symlink is unlinked and not followed. An empty
/// directory is removed. A nonempty directory stays, so a planted tree at
/// the capture name does not discard the capture that already landed.
fn remove_pending_node(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        let result = if metadata.is_dir() {
            std::fs::remove_dir(path)
        } else {
            std::fs::remove_file(path)
        };
        return result.map_err(Into::into);
    }
    if metadata.is_dir() {
        let mut entries = std::fs::read_dir(path)?;
        if entries.next().transpose()?.is_some() {
            return Ok(());
        }
        return std::fs::remove_dir(path).map_err(Into::into);
    }
    std::fs::remove_file(path).map_err(Into::into)
}

fn retain_unapplied_entries(root: &Path, recorded: &mut [VolumePosixMount]) {
    let path = root.join(VOLUME_POSIX_METADATA_PENDING_FILE);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(_) => return,
    };
    // A symlink would overlay modes from outside this root. An oversized
    // file is not a capture. Neither is read.
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > VOLUME_POSIX_METADATA_MAX_BYTES
    {
        return;
    }
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    let Ok(pending) = serde_json::from_slice::<VolumePosixManifest>(&bytes) else {
        return;
    };
    if pending.validate().is_err() {
        return;
    }
    for mount in recorded.iter_mut() {
        let Some(previous) = pending
            .mounts
            .iter()
            .find(|item| item.guest_path == mount.guest_path)
        else {
            continue;
        };
        for entry in &mut mount.entries {
            if let Some(kept) = previous
                .entries
                .iter()
                .find(|item| same_relative_entry(&item.path_base64, &entry.path_base64))
            {
                *entry = kept.clone();
            }
        }
    }
}

pub fn volume_posix_metadata_enabled() -> bool {
    std::env::var(VOLUME_POSIX_METADATA_ENV).as_deref() == Ok("1")
}

fn guest_path_from_volume_spec(value: &str) -> Result<String, Box<dyn std::error::Error>> {
    let mut parts = value.split(':');
    let _tag = parts
        .next()
        .filter(|tag| !tag.is_empty())
        .ok_or("volume spec is missing a virtio-fs tag")?;
    let guest_path = parts
        .next()
        .filter(|path| !path.is_empty())
        .ok_or("volume spec is missing a guest path")?;
    Ok(guest_path.to_string())
}

fn collect_share(
    source: &Path,
    relative: &Path,
    entries: &mut Vec<RootfsMetadataEntry>,
    total: &mut usize,
    share_roots: &[&Path],
) -> Result<bool, Box<dyn std::error::Error>> {
    let metadata = match std::fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(_) => return Ok(false),
    };
    let file_type = metadata.file_type();
    let (kind, link_target_base64) = if file_type.is_dir() {
        (RootfsEntryKind::Directory, None)
    } else if file_type.is_file() {
        (RootfsEntryKind::Regular, None)
    } else if file_type.is_symlink() {
        let target = match std::fs::read_link(source) {
            Ok(target) => target,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(_) => return Ok(false),
        };
        (
            RootfsEntryKind::Symlink,
            Some(base64::engine::general_purpose::STANDARD.encode(os_bytes(&target))),
        )
    } else {
        return Ok(true);
    };
    *total = total
        .checked_add(1)
        .ok_or("volume posix metadata entry count overflowed")?;
    if *total > VOLUME_POSIX_METADATA_MAX_ENTRIES {
        return Err(format!(
            "volume posix metadata exceeds {VOLUME_POSIX_METADATA_MAX_ENTRIES} entries"
        )
        .into());
    }
    #[cfg(unix)]
    let (mode, uid, gid, mtime) = {
        use std::os::unix::fs::MetadataExt;
        (
            metadata.mode(),
            metadata.uid() as u64,
            metadata.gid() as u64,
            metadata.mtime().max(0) as u64,
        )
    };
    #[cfg(not(unix))]
    let (mode, uid, gid, mtime) = (0, 0, 0, 0);
    entries.push(RootfsMetadataEntry {
        path_base64: base64::engine::general_purpose::STANDARD.encode(os_bytes(relative)),
        kind,
        mode,
        uid,
        gid,
        mtime,
        size: metadata.len(),
        link_target_base64,
    });
    if !file_type.is_dir() {
        return Ok(true);
    }
    let mut children = Vec::new();
    let listing = match std::fs::read_dir(source) {
        Ok(listing) => listing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(_) => return Ok(false),
    };
    for entry in listing {
        match entry {
            Ok(child) => children.push(child),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Ok(false),
        }
    }
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        let child_path = child.path();
        if share_roots.iter().any(|root| *root == child_path) {
            continue;
        }
        if !collect_share(
            &child_path,
            &relative.join(child.file_name()),
            entries,
            total,
            share_roots,
        )? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn apply_mount(
    root: &Path,
    mount: &VolumePosixMount,
    mounts: &[VolumePosixMount],
    share_paths: &[String],
) -> Result<Vec<RootfsMetadataEntry>, Box<dyn std::error::Error>> {
    let relative_mount = mount.guest_path.trim_start_matches('/');
    let share = root.join(relative_mount);
    let mut guest_paths: Vec<&str> = mounts
        .iter()
        .map(|other| other.guest_path.as_str())
        .collect();
    for path in share_paths {
        if guest_paths
            .iter()
            .any(|existing| *existing == path.as_str())
        {
            continue;
        }
        guest_paths.push(path.as_str());
    }
    let mut pending = Vec::new();
    for entry in &mount.entries {
        let relative = match decode_relative_path(&entry.path_base64) {
            Ok(relative) => relative,
            Err(_) => continue,
        };
        if a3s_box_core::volume_posix::entry_covers_nested_share(
            &mount.guest_path,
            &relative,
            &guest_paths,
        ) {
            continue;
        }
        let Ok(target) = resolve_without_symlink_parent(&share, &relative) else {
            continue;
        };
        let metadata = match std::fs::symlink_metadata(&target) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        let actual_kind = if metadata.file_type().is_dir() {
            RootfsEntryKind::Directory
        } else if metadata.file_type().is_file() {
            RootfsEntryKind::Regular
        } else if metadata.file_type().is_symlink() {
            RootfsEntryKind::Symlink
        } else {
            continue;
        };
        if actual_kind != entry.kind {
            continue;
        }
        if apply_owner_and_mode(&target, entry, metadata.file_type().is_symlink()).is_err() {
            pending.push(entry.clone());
        }
    }
    Ok(pending)
}

fn apply_owner_and_mode(
    path: &Path,
    entry: &RootfsMetadataEntry,
    symlink: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::PermissionsExt;

        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| format!("volume posix path contains NUL: {}", path.display()))?;
        let rc = if symlink {
            unsafe {
                libc::lchown(
                    c_path.as_ptr(),
                    entry.uid as libc::uid_t,
                    entry.gid as libc::gid_t,
                )
            }
        } else {
            unsafe {
                libc::chown(
                    c_path.as_ptr(),
                    entry.uid as libc::uid_t,
                    entry.gid as libc::gid_t,
                )
            }
        };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            return Err(format!(
                "failed to restore volume posix owner at {}: {error}",
                path.display()
            )
            .into());
        }
        if !symlink {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(entry.mode & 0o7777))
                .map_err(|error| {
                    format!(
                        "failed to restore volume posix mode at {}: {error}",
                        path.display()
                    )
                })?;
        }
        return Ok(());
    }
    #[cfg(not(unix))]
    {
        let _ = (path, entry, symlink);
        Err("volume posix replay requires a Unix guest".into())
    }
}

fn same_relative_entry(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    matches!(
        (decode_relative_path(left), decode_relative_path(right)),
        (Ok(left), Ok(right)) if left == right
    )
}

fn decode_relative_path(encoded: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| format!("invalid volume posix path encoding: {error}"))?;
    let relative = path_from_bytes(&raw)?;
    let mut clean = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => clean.push(name),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("unsafe volume posix entry path".into());
            }
        }
    }
    Ok(clean)
}

fn resolve_without_symlink_parent(
    root: &Path,
    relative: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut current = root.to_path_buf();
    let components: Vec<_> = relative.components().collect();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            continue;
        };
        current.push(name);
        if index + 1 < components.len()
            && std::fs::symlink_metadata(&current)?
                .file_type()
                .is_symlink()
        {
            return Err(
                format!("symlink parent in volume posix path: {}", current.display()).into(),
            );
        }
    }
    if !current.starts_with(root) {
        return Err("volume posix entry escaped its share".into());
    }
    Ok(current)
}

#[cfg(unix)]
fn os_bytes(path: &Path) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes()
}

#[cfg(not(unix))]
fn os_bytes(path: &Path) -> &[u8] {
    path.to_str().unwrap_or("").as_bytes()
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf, Box<dyn std::error::Error>> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec())))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let text = std::str::from_utf8(bytes)?;
    Ok(PathBuf::from(text))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[test]
    fn volume_posix_metadata_roundtrip_restores_mode_without_writing_into_the_share() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let file = volume.join("note.txt");
        std::fs::write(&file, b"note").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
        std::fs::set_permissions(&volume, std::fs::Permissions::from_mode(0o750)).unwrap();

        persist_volume_posix_metadata(root.path(), &[(String::from("/data"), volume.clone())])
            .unwrap();
        let manifest = root.path().join(VOLUME_POSIX_METADATA_FILE);
        assert!(manifest.is_file());
        assert!(!volume.join(VOLUME_POSIX_METADATA_FILE).exists());

        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(&volume, std::fs::Permissions::from_mode(0o755)).unwrap();

        restore_volume_posix_metadata(root.path()).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&file).unwrap().mode() & 0o7777,
            0o640
        );
        assert_eq!(
            std::fs::symlink_metadata(&volume).unwrap().mode() & 0o7777,
            0o750
        );
        assert!(!manifest.exists());
        restore_volume_posix_metadata(root.path()).unwrap();
    }

    #[test]
    fn volume_posix_metadata_skips_parent_entry_paths() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir_all(&volume).unwrap();
        let note = volume.join("note.txt");
        let outside = root.path().join("outside");
        std::fs::write(&note, b"note").unwrap();
        std::fs::write(&outside, b"outside").unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o777)).unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o700)).unwrap();
        let owner = std::fs::symlink_metadata(&note).unwrap();
        let encode = |path: &str| base64::engine::general_purpose::STANDARD.encode(path);
        let manifest = VolumePosixManifest::new(vec![VolumePosixMount {
            guest_path: "/data".to_string(),
            entries: vec![
                RootfsMetadataEntry {
                    path_base64: encode("../outside"),
                    kind: RootfsEntryKind::Regular,
                    mode: 0o600,
                    uid: owner.uid() as u64,
                    gid: owner.gid() as u64,
                    mtime: 0,
                    size: 7,
                    link_target_base64: None,
                },
                RootfsMetadataEntry {
                    path_base64: encode("note.txt"),
                    kind: RootfsEntryKind::Regular,
                    mode: 0o640,
                    uid: owner.uid() as u64,
                    gid: owner.gid() as u64,
                    mtime: 0,
                    size: 4,
                    link_target_base64: None,
                },
            ],
            retained: false,
        }]);
        let manifest_path = root.path().join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

        restore_volume_posix_metadata(root.path()).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&outside).unwrap().mode() & 0o7777,
            0o700
        );
        assert_eq!(
            std::fs::symlink_metadata(&note).unwrap().mode() & 0o7777,
            0o640
        );
        assert!(!manifest_path.exists());
    }

    #[test]
    fn nested_volume_is_omitted_from_the_parent_share() {
        let root = tempfile::TempDir::new().unwrap();
        let workspace = root.path().join("workspace");
        let cache = workspace.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let note = workspace.join("note.txt");
        let secret = cache.join("secret.txt");
        std::fs::write(&note, b"note").unwrap();
        std::fs::write(&secret, b"secret").unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();

        persist_volume_posix_metadata(
            root.path(),
            &[
                (String::from("/workspace"), workspace.clone()),
                (String::from("/workspace/cache"), cache.clone()),
            ],
        )
        .unwrap();
        let manifest: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(root.path().join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        let parent = manifest
            .mounts
            .iter()
            .find(|mount| mount.guest_path == "/workspace")
            .unwrap();
        let nested = manifest
            .mounts
            .iter()
            .find(|mount| mount.guest_path == "/workspace/cache")
            .unwrap();
        assert!(parent.entries.iter().all(|entry| {
            let relative = decode_relative_path(&entry.path_base64).unwrap();
            !a3s_box_core::volume_posix::entry_covers_nested_share(
                "/workspace",
                &relative,
                &manifest
                    .mounts
                    .iter()
                    .map(|mount| mount.guest_path.as_str())
                    .collect::<Vec<_>>(),
            )
        }));
        assert!(nested.entries.iter().any(|entry| {
            decode_relative_path(&entry.path_base64).unwrap() == Path::new("secret.txt")
        }));

        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o777)).unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o777)).unwrap();
        restore_volume_posix_metadata(root.path()).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&secret).unwrap().mode() & 0o7777,
            0o600
        );
        assert_eq!(
            std::fs::symlink_metadata(&note).unwrap().mode() & 0o7777,
            0o644
        );
    }

    #[test]
    fn stale_parent_manifest_does_not_chmod_a_nested_volume() {
        let root = tempfile::TempDir::new().unwrap();
        let cache = root.path().join("workspace").join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let secret = cache.join("secret.txt");
        std::fs::write(&secret, b"secret").unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o777)).unwrap();
        std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o755)).unwrap();
        let owner = std::fs::symlink_metadata(&cache).unwrap();
        let encode = |path: &str| base64::engine::general_purpose::STANDARD.encode(path);
        let manifest = VolumePosixManifest::new(vec![
            VolumePosixMount {
                guest_path: "/workspace".to_string(),
                entries: vec![RootfsMetadataEntry {
                    path_base64: encode("cache/secret.txt"),
                    kind: RootfsEntryKind::Regular,
                    mode: 0o700,
                    uid: owner.uid() as u64,
                    gid: owner.gid() as u64,
                    mtime: 0,
                    size: 6,
                    link_target_base64: None,
                }],
                retained: false,
            },
            VolumePosixMount {
                guest_path: "/workspace/cache".to_string(),
                entries: vec![RootfsMetadataEntry {
                    path_base64: encode("."),
                    kind: RootfsEntryKind::Directory,
                    mode: 0o750,
                    uid: owner.uid() as u64,
                    gid: owner.gid() as u64,
                    mtime: 0,
                    size: 0,
                    link_target_base64: None,
                }],
                retained: false,
            },
        ]);
        std::fs::write(
            root.path().join(VOLUME_POSIX_METADATA_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        restore_volume_posix_metadata(root.path()).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&secret).unwrap().mode() & 0o7777,
            0o777
        );
        assert_eq!(
            std::fs::symlink_metadata(&cache).unwrap().mode() & 0o7777,
            0o750
        );
    }

    #[test]
    fn restore_skips_a_parent_entry_inside_a_share_the_manifest_omits() {
        let root = tempfile::TempDir::new().unwrap();
        let cache = root.path().join("workspace").join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let secret = cache.join("secret.txt");
        std::fs::write(&secret, b"secret").unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o777)).unwrap();
        let owner = std::fs::symlink_metadata(&secret).unwrap();
        let encode = |path: &str| base64::engine::general_purpose::STANDARD.encode(path);
        let manifest = VolumePosixManifest::new(vec![VolumePosixMount {
            guest_path: "/workspace".to_string(),
            entries: vec![RootfsMetadataEntry {
                path_base64: encode("cache/secret.txt"),
                kind: RootfsEntryKind::Regular,
                mode: 0o700,
                uid: owner.uid() as u64,
                gid: owner.gid() as u64,
                mtime: 0,
                size: 6,
                link_target_base64: None,
            }],
            retained: false,
        }]);
        std::fs::write(
            root.path().join(VOLUME_POSIX_METADATA_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        restore_volume_posix_metadata_with_shares(root.path(), &["/workspace/cache".to_string()])
            .unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&secret).unwrap().mode() & 0o7777,
            0o777
        );
    }

    #[test]
    fn missing_volume_posix_entry_does_not_fail_replay() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let note = volume.join("note.txt");
        let keep = volume.join("keep.txt");
        std::fs::write(&note, b"note").unwrap();
        std::fs::write(&keep, b"keep").unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o640)).unwrap();
        std::fs::set_permissions(&keep, std::fs::Permissions::from_mode(0o640)).unwrap();

        persist_volume_posix_metadata(root.path(), &[(String::from("/data"), volume.clone())])
            .unwrap();
        std::fs::remove_file(&note).unwrap();
        std::fs::set_permissions(&keep, std::fs::Permissions::from_mode(0o600)).unwrap();
        let manifest = root.path().join(VOLUME_POSIX_METADATA_FILE);

        restore_volume_posix_metadata(root.path()).unwrap();
        assert!(!note.exists());
        assert_eq!(
            std::fs::symlink_metadata(&keep).unwrap().mode() & 0o7777,
            0o640
        );
        assert!(!manifest.exists());
    }

    #[test]
    fn volume_posix_kind_mismatch_does_not_fail_replay() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let note = volume.join("note.txt");
        let keep = volume.join("keep.txt");
        std::fs::write(&note, b"note").unwrap();
        std::fs::write(&keep, b"keep").unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o640)).unwrap();
        std::fs::set_permissions(&keep, std::fs::Permissions::from_mode(0o640)).unwrap();

        persist_volume_posix_metadata(root.path(), &[(String::from("/data"), volume.clone())])
            .unwrap();
        std::fs::remove_file(&note).unwrap();
        std::fs::create_dir(&note).unwrap();
        std::fs::set_permissions(&keep, std::fs::Permissions::from_mode(0o600)).unwrap();
        let manifest = root.path().join(VOLUME_POSIX_METADATA_FILE);

        restore_volume_posix_metadata(root.path()).unwrap();
        assert!(note.is_dir());
        assert_eq!(
            std::fs::symlink_metadata(&keep).unwrap().mode() & 0o7777,
            0o640
        );
        assert!(!manifest.exists());
    }

    #[test]
    fn file_parent_entry_does_not_fail_replay() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let notadir = volume.join("notadir");
        let keep = volume.join("keep.txt");
        std::fs::write(&notadir, b"file").unwrap();
        std::fs::write(&keep, b"keep").unwrap();
        std::fs::set_permissions(&keep, std::fs::Permissions::from_mode(0o600)).unwrap();
        let owner = std::fs::symlink_metadata(&keep).unwrap();
        let encode = |path: &str| base64::engine::general_purpose::STANDARD.encode(path);
        let entry = |path: &str, mode: u32| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: RootfsEntryKind::Regular,
            mode,
            uid: owner.uid() as u64,
            gid: owner.gid() as u64,
            mtime: 0,
            size: 4,
            link_target_base64: None,
        };
        let manifest = VolumePosixManifest::new(vec![VolumePosixMount {
            guest_path: "/data".to_string(),
            entries: vec![entry("notadir/child.txt", 0o640), entry("keep.txt", 0o640)],
            retained: false,
        }]);
        std::fs::write(
            root.path().join(VOLUME_POSIX_METADATA_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        restore_volume_posix_metadata(root.path()).unwrap();
        assert!(notadir.is_file());
        assert_eq!(
            std::fs::symlink_metadata(&keep).unwrap().mode() & 0o7777,
            0o640
        );
        assert!(!root.path().join(VOLUME_POSIX_METADATA_FILE).exists());
        assert!(!root
            .path()
            .join(VOLUME_POSIX_METADATA_PENDING_FILE)
            .exists());
    }

    #[test]
    fn symlink_parent_entry_does_not_fail_replay() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&volume).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let keep = volume.join("keep.txt");
        let secret = outside.join("secret.txt");
        std::fs::write(&keep, b"keep").unwrap();
        std::fs::write(&secret, b"secret").unwrap();
        std::fs::set_permissions(&keep, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&outside, volume.join("link")).unwrap();
        let owner = std::fs::symlink_metadata(&keep).unwrap();
        let encode = |path: &str| base64::engine::general_purpose::STANDARD.encode(path);
        let entry = |path: &str, mode: u32| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: RootfsEntryKind::Regular,
            mode,
            uid: owner.uid() as u64,
            gid: owner.gid() as u64,
            mtime: 0,
            size: 4,
            link_target_base64: None,
        };
        let manifest = VolumePosixManifest::new(vec![VolumePosixMount {
            guest_path: "/data".to_string(),
            entries: vec![entry("link/secret.txt", 0o600), entry("keep.txt", 0o640)],
            retained: false,
        }]);
        let manifest_path = root.path().join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

        restore_volume_posix_metadata(root.path()).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&secret).unwrap().mode() & 0o7777,
            0o700
        );
        assert_eq!(
            std::fs::symlink_metadata(&keep).unwrap().mode() & 0o7777,
            0o640
        );
        assert!(!manifest_path.exists());
    }

    #[test]
    fn owner_restore_failure_does_not_fail_replay() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let blocked = volume.join("blocked.txt");
        let keep = volume.join("keep.txt");
        std::fs::write(&blocked, b"blocked").unwrap();
        std::fs::write(&keep, b"keep").unwrap();
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&keep, std::fs::Permissions::from_mode(0o600)).unwrap();
        let owner = std::fs::symlink_metadata(&keep).unwrap();
        let encode = |path: &str| base64::engine::general_purpose::STANDARD.encode(path);
        let entry = |path: &str, mode: u32, uid: u64, gid: u64| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: RootfsEntryKind::Regular,
            mode,
            uid,
            gid,
            mtime: 0,
            size: 4,
            link_target_base64: None,
        };
        let manifest = VolumePosixManifest::new(vec![VolumePosixMount {
            guest_path: "/data".to_string(),
            entries: vec![
                entry("blocked.txt", 0o600, 0, 0),
                entry("keep.txt", 0o640, owner.uid() as u64, owner.gid() as u64),
            ],
            retained: false,
        }]);
        let manifest_path = root.path().join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

        restore_volume_posix_metadata(root.path()).unwrap();
        if owner.uid() != 0 {
            assert_eq!(
                std::fs::symlink_metadata(&blocked).unwrap().mode() & 0o7777,
                0o700
            );
        }
        assert_eq!(
            std::fs::symlink_metadata(&keep).unwrap().mode() & 0o7777,
            0o640
        );
        if owner.uid() == 0 {
            assert!(!manifest_path.exists());
            return;
        }
        let published: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(published.mounts[0].entries.len(), 2);
        let pending_path = root.path().join(VOLUME_POSIX_METADATA_PENDING_FILE);
        let pending: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&pending_path).unwrap()).unwrap();
        assert_eq!(pending.mounts.len(), 1);
        assert_eq!(pending.mounts[0].entries.len(), 1);
        assert_eq!(
            pending.mounts[0].entries[0].path_base64,
            encode("blocked.txt")
        );
        assert_eq!(pending.mounts[0].entries[0].mode, 0o600);
        assert_eq!(pending.mounts[0].entries[0].uid, 0);

        std::fs::set_permissions(&keep, std::fs::Permissions::from_mode(0o755)).unwrap();
        persist_volume_posix_metadata(root.path(), &[(String::from("/data"), volume)]).unwrap();
        assert!(!pending_path.exists());
        let captured: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        let blocked = captured.mounts[0]
            .entries
            .iter()
            .find(|item| item.path_base64 == encode("blocked.txt"))
            .unwrap();
        let kept = captured.mounts[0]
            .entries
            .iter()
            .find(|item| same_relative_entry(&item.path_base64, &encode("keep.txt")))
            .unwrap();
        assert_eq!(blocked.mode, 0o600);
        assert_eq!(blocked.uid, 0);
        assert_eq!(kept.mode & 0o7777, 0o755);
    }

    #[test]
    fn unreadable_share_is_omitted_without_dropping_other_shares() {
        let root = tempfile::TempDir::new().unwrap();
        let data = root.path().join("data");
        let cache = root.path().join("cache");
        std::fs::create_dir(&data).unwrap();
        std::fs::create_dir(&cache).unwrap();
        let note = data.join("note.txt");
        std::fs::write(&note, b"note").unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o640)).unwrap();
        let locked = data.join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("secret.txt"), b"secret").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let _restore_locked = LockedDir(&locked);
        let ok = cache.join("ok.txt");
        std::fs::write(&ok, b"ok").unwrap();
        std::fs::set_permissions(&ok, std::fs::Permissions::from_mode(0o644)).unwrap();

        persist_volume_posix_metadata(
            root.path(),
            &[
                (String::from("/data"), data),
                (String::from("/cache"), cache),
            ],
        )
        .unwrap();
        let manifest: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(root.path().join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        let cache_mount = manifest
            .mounts
            .iter()
            .find(|mount| mount.guest_path == "/cache")
            .unwrap();
        let ok_entry = cache_mount
            .entries
            .iter()
            .find(|entry| same_relative_entry(&entry.path_base64, &encode_path("ok.txt")))
            .unwrap();
        assert_eq!(ok_entry.mode & 0o7777, 0o644);
        let data_mount = manifest
            .mounts
            .iter()
            .find(|mount| mount.guest_path == "/data");
        if std::fs::symlink_metadata(&note).unwrap().uid() == 0 {
            assert!(data_mount.is_some());
            return;
        }
        assert!(data_mount.is_none());
    }

    #[test]
    fn unreadable_share_keeps_its_previous_capture() {
        let root = tempfile::TempDir::new().unwrap();
        let data = root.path().join("data");
        let cache = root.path().join("cache");
        std::fs::create_dir(&data).unwrap();
        std::fs::create_dir(&cache).unwrap();
        let note = data.join("note.txt");
        std::fs::write(&note, b"note").unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o640)).unwrap();
        let ok = cache.join("ok.txt");
        std::fs::write(&ok, b"ok").unwrap();
        std::fs::set_permissions(&ok, std::fs::Permissions::from_mode(0o644)).unwrap();
        let shares = [
            (String::from("/data"), data.clone()),
            (String::from("/cache"), cache.clone()),
        ];
        persist_volume_posix_metadata(root.path(), &shares).unwrap();

        let locked = data.join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("secret.txt"), b"secret").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let _restore_locked = LockedDir(&locked);
        std::fs::set_permissions(&ok, std::fs::Permissions::from_mode(0o600)).unwrap();
        persist_volume_posix_metadata(root.path(), &shares).unwrap();

        let manifest: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(root.path().join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        let data_mount = manifest
            .mounts
            .iter()
            .find(|mount| mount.guest_path == "/data")
            .unwrap();
        let cache_mount = manifest
            .mounts
            .iter()
            .find(|mount| mount.guest_path == "/cache")
            .unwrap();
        let ok_entry = cache_mount
            .entries
            .iter()
            .find(|entry| same_relative_entry(&entry.path_base64, &encode_path("ok.txt")))
            .unwrap();
        assert!(!cache_mount.retained);
        assert_eq!(ok_entry.mode & 0o7777, 0o600);
        if std::fs::symlink_metadata(&note).unwrap().uid() == 0 {
            assert!(!data_mount.retained);
            return;
        }
        assert!(data_mount.retained);
        let note_entry = data_mount
            .entries
            .iter()
            .find(|entry| same_relative_entry(&entry.path_base64, &encode_path("note.txt")))
            .unwrap();
        assert_eq!(note_entry.mode & 0o7777, 0o640);
    }

    #[test]
    fn restore_prefers_a_newer_durable_temp_capture() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let file = volume.join("note.txt");
        std::fs::write(&file, b"note").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let owner = std::fs::symlink_metadata(&file).unwrap();
        let encode = |path: &str| base64::engine::general_purpose::STANDARD.encode(path);
        let entry = |mode: u32| RootfsMetadataEntry {
            path_base64: encode("note.txt"),
            kind: RootfsEntryKind::Regular,
            mode,
            uid: owner.uid() as u64,
            gid: owner.gid() as u64,
            mtime: 0,
            size: 4,
            link_target_base64: None,
        };
        let write_mode = |name: &str, mode: u32| {
            let manifest = VolumePosixManifest::new(vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(mode)],
                retained: false,
            }]);
            let path = root.path().join(name);
            std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
            path
        };
        let manifest = write_mode(VOLUME_POSIX_METADATA_FILE, 0o644);
        let temp = write_mode(VOLUME_POSIX_METADATA_TEMP_FILE, 0o640);
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(
                    std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs),
                )
                .unwrap();
        };
        set_mtime(&manifest, 10);
        set_mtime(&temp, 50);
        let scratch = root.path().join(VOLUME_POSIX_METADATA_PENDING_TEMP_FILE);
        std::fs::write(&scratch, b"scratch").unwrap();

        restore_volume_posix_metadata(root.path()).unwrap();
        assert!(!scratch.exists());
        assert_eq!(
            std::fs::symlink_metadata(&file).unwrap().mode() & 0o7777,
            0o640
        );
        assert!(!manifest.exists());
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&temp).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
    }

    #[test]
    fn restore_prefers_a_newer_publish_capture() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let file = volume.join("note.txt");
        std::fs::write(&file, b"note").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let owner = std::fs::symlink_metadata(&file).unwrap();
        let encode = |path: &str| base64::engine::general_purpose::STANDARD.encode(path);
        let entry = |mode: u32| RootfsMetadataEntry {
            path_base64: encode("note.txt"),
            kind: RootfsEntryKind::Regular,
            mode,
            uid: owner.uid() as u64,
            gid: owner.gid() as u64,
            mtime: 0,
            size: 4,
            link_target_base64: None,
        };
        let write_mode = |name: &str, mode: u32| {
            let manifest = VolumePosixManifest::new(vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(mode)],
                retained: false,
            }]);
            let path = root.path().join(name);
            std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
            path
        };
        let manifest = write_mode(VOLUME_POSIX_METADATA_FILE, 0o644);
        let temp = write_mode(VOLUME_POSIX_METADATA_TEMP_FILE, 0o640);
        let published = write_mode(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE, 0o755);
        write_mode(VOLUME_POSIX_METADATA_PENDING_TEMP_FILE, 0o777);
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(
                    std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs),
                )
                .unwrap();
        };
        set_mtime(&manifest, 10);
        set_mtime(&temp, 50);
        set_mtime(&published, 80);
        set_mtime(
            &root.path().join(VOLUME_POSIX_METADATA_PENDING_TEMP_FILE),
            90,
        );

        restore_volume_posix_metadata(root.path()).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&file).unwrap().mode() & 0o7777,
            0o755
        );
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&published).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o755);
    }

    #[test]
    fn restore_reads_a_valid_publish_beside_an_oversized_manifest() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let file = volume.join("note.txt");
        std::fs::write(&file, b"note").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let owner = std::fs::symlink_metadata(&file).unwrap();
        let encode = |path: &str| base64::engine::general_purpose::STANDARD.encode(path);
        let manifest = VolumePosixManifest::new(vec![VolumePosixMount {
            guest_path: "/data".to_string(),
            entries: vec![RootfsMetadataEntry {
                path_base64: encode("note.txt"),
                kind: RootfsEntryKind::Regular,
                mode: 0o640,
                uid: owner.uid() as u64,
                gid: owner.gid() as u64,
                mtime: 0,
                size: 4,
                link_target_base64: None,
            }],
            retained: false,
        }]);
        let published = root.path().join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
        std::fs::write(&published, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let committed = root.path().join(VOLUME_POSIX_METADATA_FILE);
        let oversized = std::fs::File::create(&committed).unwrap();
        oversized
            .set_len(VOLUME_POSIX_METADATA_MAX_BYTES + 1)
            .unwrap();
        drop(oversized);

        restore_volume_posix_metadata(root.path()).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&file).unwrap().mode() & 0o7777,
            0o640
        );
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&published).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        let committed_len = std::fs::symlink_metadata(&committed)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        assert!(committed_len <= VOLUME_POSIX_METADATA_MAX_BYTES);
    }

    #[test]
    fn restore_rejects_an_oversized_manifest_when_nothing_valid_remains() {
        let root = tempfile::TempDir::new().unwrap();
        let committed = root.path().join(VOLUME_POSIX_METADATA_FILE);
        let oversized = std::fs::File::create(&committed).unwrap();
        oversized
            .set_len(VOLUME_POSIX_METADATA_MAX_BYTES + 1)
            .unwrap();
        drop(oversized);

        let error = restore_volume_posix_metadata(root.path()).unwrap_err();
        assert!(error.to_string().contains("exceeds"), "{error}");
        assert!(committed.is_file());
    }

    #[test]
    fn restore_reads_a_valid_publish_beside_a_directory() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let file = volume.join("note.txt");
        std::fs::write(&file, b"note").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let owner = std::fs::symlink_metadata(&file).unwrap();
        let encode = |path: &str| base64::engine::general_purpose::STANDARD.encode(path);
        let manifest = VolumePosixManifest::new(vec![VolumePosixMount {
            guest_path: "/data".to_string(),
            entries: vec![RootfsMetadataEntry {
                path_base64: encode("note.txt"),
                kind: RootfsEntryKind::Regular,
                mode: 0o640,
                uid: owner.uid() as u64,
                gid: owner.gid() as u64,
                mtime: 0,
                size: 4,
                link_target_base64: None,
            }],
            retained: false,
        }]);
        let published = root.path().join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
        std::fs::write(&published, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let committed = root.path().join(VOLUME_POSIX_METADATA_FILE);
        std::fs::create_dir(&committed).unwrap();

        restore_volume_posix_metadata(root.path()).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&file).unwrap().mode() & 0o7777,
            0o640
        );
        assert!(!committed.exists());
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&published).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
    }

    #[test]
    fn restore_keeps_a_nonempty_committed_manifest_directory() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let file = volume.join("note.txt");
        std::fs::write(&file, b"note").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let owner = std::fs::symlink_metadata(&file).unwrap();
        let encode = |path: &str| base64::engine::general_purpose::STANDARD.encode(path);
        let manifest = VolumePosixManifest::new(vec![VolumePosixMount {
            guest_path: "/data".to_string(),
            entries: vec![RootfsMetadataEntry {
                path_base64: encode("note.txt"),
                kind: RootfsEntryKind::Regular,
                mode: 0o640,
                uid: owner.uid() as u64,
                gid: owner.gid() as u64,
                mtime: 0,
                size: 4,
                link_target_base64: None,
            }],
            retained: false,
        }]);
        let published = root.path().join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
        std::fs::write(&published, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let committed = root.path().join(VOLUME_POSIX_METADATA_FILE);
        std::fs::create_dir(&committed).unwrap();
        let marker = committed.join("keep.txt");
        std::fs::write(&marker, b"keep").unwrap();

        restore_volume_posix_metadata(root.path()).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&file).unwrap().mode() & 0o7777,
            0o640
        );
        assert!(committed.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&published).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
    }

    #[test]
    fn restore_rejects_a_directory_when_nothing_valid_remains() {
        let root = tempfile::TempDir::new().unwrap();
        let committed = root.path().join(VOLUME_POSIX_METADATA_FILE);
        std::fs::create_dir(&committed).unwrap();

        let error = restore_volume_posix_metadata(root.path()).unwrap_err();
        assert!(
            error.to_string().contains("is not a regular file"),
            "{error}"
        );
        assert!(committed.is_dir());
    }

    #[test]
    fn persist_does_not_follow_a_symlink_pending_capture() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let note = volume.join("note.txt");
        std::fs::write(&note, b"note").unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o640)).unwrap();
        let owner = std::fs::symlink_metadata(&note).unwrap();
        let encode = |path: &str| base64::engine::general_purpose::STANDARD.encode(path);
        let decoy = VolumePosixManifest::new(vec![VolumePosixMount {
            guest_path: "/data".to_string(),
            entries: vec![RootfsMetadataEntry {
                path_base64: encode("note.txt"),
                kind: RootfsEntryKind::Regular,
                mode: 0o777,
                uid: owner.uid() as u64,
                gid: owner.gid() as u64,
                mtime: 0,
                size: 4,
                link_target_base64: None,
            }],
            retained: false,
        }]);
        let decoy_path = root.path().join("decoy.json");
        std::fs::write(&decoy_path, serde_json::to_vec(&decoy).unwrap()).unwrap();
        let pending = root.path().join(VOLUME_POSIX_METADATA_PENDING_FILE);
        std::os::unix::fs::symlink(&decoy_path, &pending).unwrap();

        persist_volume_posix_metadata(root.path(), &[(String::from("/data"), volume)]).unwrap();
        let captured: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(root.path().join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        let kept = captured.mounts[0]
            .entries
            .iter()
            .find(|entry| same_relative_entry(&entry.path_base64, &encode("note.txt")))
            .unwrap();
        assert_eq!(kept.mode & 0o7777, 0o640);
    }

    #[test]
    fn persist_writes_when_the_metadata_scratch_directory_is_empty() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let note = volume.join("note.txt");
        std::fs::write(&note, b"note").unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o640)).unwrap();
        let scratch = root.path().join(VOLUME_POSIX_METADATA_TEMP_FILE);
        std::fs::create_dir(&scratch).unwrap();

        persist_volume_posix_metadata(root.path(), &[(String::from("/data"), volume)]).unwrap();
        let captured: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(root.path().join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        let encode = |path: &str| base64::engine::general_purpose::STANDARD.encode(path);
        let kept = captured.mounts[0]
            .entries
            .iter()
            .find(|entry| same_relative_entry(&entry.path_base64, &encode("note.txt")))
            .unwrap();
        assert_eq!(kept.mode & 0o7777, 0o640);
        assert!(!scratch.exists());
    }

    #[test]
    fn persist_writes_when_the_metadata_scratch_directory_is_not_empty() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let note = volume.join("note.txt");
        std::fs::write(&note, b"note").unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o640)).unwrap();
        let scratch = root.path().join(VOLUME_POSIX_METADATA_TEMP_FILE);
        std::fs::create_dir(&scratch).unwrap();
        let marker = scratch.join("keep.txt");
        std::fs::write(&marker, b"keep").unwrap();

        persist_volume_posix_metadata(root.path(), &[(String::from("/data"), volume)]).unwrap();
        let captured: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(root.path().join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        let encode = |path: &str| base64::engine::general_purpose::STANDARD.encode(path);
        let kept = captured.mounts[0]
            .entries
            .iter()
            .find(|entry| same_relative_entry(&entry.path_base64, &encode("note.txt")))
            .unwrap();
        assert_eq!(kept.mode & 0o7777, 0o640);
        assert!(scratch.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
    }

    #[test]
    fn persist_writes_when_the_committed_manifest_directory_is_not_empty() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let note = volume.join("note.txt");
        std::fs::write(&note, b"note").unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o640)).unwrap();
        let committed = root.path().join(VOLUME_POSIX_METADATA_FILE);
        std::fs::create_dir(&committed).unwrap();
        let marker = committed.join("keep.txt");
        std::fs::write(&marker, b"keep").unwrap();

        persist_volume_posix_metadata(root.path(), &[(String::from("/data"), volume)]).unwrap();
        assert!(committed.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
        let encode = |path: &str| base64::engine::general_purpose::STANDARD.encode(path);
        let captured_path = [
            VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE,
            VOLUME_POSIX_METADATA_TEMP_FILE,
        ]
        .into_iter()
        .map(|name| root.path().join(name))
        .find(|path| path.is_file())
        .expect("capture published beside the directory");
        let captured: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&captured_path).unwrap()).unwrap();
        let kept = captured.mounts[0]
            .entries
            .iter()
            .find(|entry| same_relative_entry(&entry.path_base64, &encode("note.txt")))
            .unwrap();
        assert_eq!(kept.mode & 0o7777, 0o640);
    }

    #[test]
    fn persist_writes_when_the_committed_manifest_directory_is_empty() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let note = volume.join("note.txt");
        std::fs::write(&note, b"note").unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o640)).unwrap();
        let committed = root.path().join(VOLUME_POSIX_METADATA_FILE);
        std::fs::create_dir(&committed).unwrap();

        persist_volume_posix_metadata(root.path(), &[(String::from("/data"), volume)]).unwrap();
        assert!(!committed.is_dir());
        let captured: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&committed).unwrap()).unwrap();
        let encode = |path: &str| base64::engine::general_purpose::STANDARD.encode(path);
        let kept = captured.mounts[0]
            .entries
            .iter()
            .find(|entry| same_relative_entry(&entry.path_base64, &encode("note.txt")))
            .unwrap();
        assert_eq!(kept.mode & 0o7777, 0o640);
    }

    #[test]
    fn persist_writes_when_the_pending_scratch_directory_is_empty() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let note = volume.join("note.txt");
        std::fs::write(&note, b"note").unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o640)).unwrap();
        let scratch = root.path().join(VOLUME_POSIX_METADATA_PENDING_TEMP_FILE);
        std::fs::create_dir(&scratch).unwrap();

        persist_volume_posix_metadata(root.path(), &[(String::from("/data"), volume)]).unwrap();
        let captured: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(root.path().join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        let encode = |path: &str| base64::engine::general_purpose::STANDARD.encode(path);
        let kept = captured.mounts[0]
            .entries
            .iter()
            .find(|entry| same_relative_entry(&entry.path_base64, &encode("note.txt")))
            .unwrap();
        assert_eq!(kept.mode & 0o7777, 0o640);
        assert!(!scratch.exists());
    }

    #[test]
    fn persist_writes_when_the_pending_scratch_directory_is_not_empty() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let note = volume.join("note.txt");
        std::fs::write(&note, b"note").unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o640)).unwrap();
        let scratch = root.path().join(VOLUME_POSIX_METADATA_PENDING_TEMP_FILE);
        std::fs::create_dir(&scratch).unwrap();
        let marker = scratch.join("keep.txt");
        std::fs::write(&marker, b"keep").unwrap();

        persist_volume_posix_metadata(root.path(), &[(String::from("/data"), volume)]).unwrap();
        let captured: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(root.path().join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        let encode = |path: &str| base64::engine::general_purpose::STANDARD.encode(path);
        let kept = captured.mounts[0]
            .entries
            .iter()
            .find(|entry| same_relative_entry(&entry.path_base64, &encode("note.txt")))
            .unwrap();
        assert_eq!(kept.mode & 0o7777, 0o640);
        assert!(scratch.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
    }

    #[test]
    fn restore_removes_an_empty_pending_scratch_directory() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let note = volume.join("note.txt");
        std::fs::write(&note, b"note").unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o640)).unwrap();
        persist_volume_posix_metadata(root.path(), &[(String::from("/data"), volume.clone())])
            .unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o600)).unwrap();
        let scratch = root.path().join(VOLUME_POSIX_METADATA_PENDING_TEMP_FILE);
        std::fs::create_dir(&scratch).unwrap();

        restore_volume_posix_metadata(root.path()).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&note).unwrap().mode() & 0o7777,
            0o640
        );
        assert!(!scratch.exists());
    }

    #[test]
    fn restore_keeps_a_nonempty_pending_scratch_directory() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let note = volume.join("note.txt");
        std::fs::write(&note, b"note").unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o640)).unwrap();
        persist_volume_posix_metadata(root.path(), &[(String::from("/data"), volume.clone())])
            .unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o600)).unwrap();
        let scratch = root.path().join(VOLUME_POSIX_METADATA_PENDING_TEMP_FILE);
        std::fs::create_dir(&scratch).unwrap();
        let marker = scratch.join("keep.txt");
        std::fs::write(&marker, b"keep").unwrap();

        restore_volume_posix_metadata(root.path()).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&note).unwrap().mode() & 0o7777,
            0o640
        );
        assert!(scratch.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
    }

    #[test]
    fn restore_removes_an_empty_metadata_scratch_directory() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let note = volume.join("note.txt");
        std::fs::write(&note, b"note").unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o640)).unwrap();
        persist_volume_posix_metadata(root.path(), &[(String::from("/data"), volume.clone())])
            .unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o600)).unwrap();
        let scratch = root.path().join(VOLUME_POSIX_METADATA_TEMP_FILE);
        std::fs::create_dir(&scratch).unwrap();

        restore_volume_posix_metadata(root.path()).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&note).unwrap().mode() & 0o7777,
            0o640
        );
        assert!(!scratch.exists());
    }

    #[test]
    fn restore_keeps_a_nonempty_metadata_scratch_directory() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let note = volume.join("note.txt");
        std::fs::write(&note, b"note").unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o640)).unwrap();
        persist_volume_posix_metadata(root.path(), &[(String::from("/data"), volume.clone())])
            .unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o600)).unwrap();
        let scratch = root.path().join(VOLUME_POSIX_METADATA_TEMP_FILE);
        std::fs::create_dir(&scratch).unwrap();
        let marker = scratch.join("keep.txt");
        std::fs::write(&marker, b"keep").unwrap();

        restore_volume_posix_metadata(root.path()).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&note).unwrap().mode() & 0o7777,
            0o640
        );
        assert!(scratch.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
    }

    #[test]
    fn restore_removes_an_empty_publish_scratch_directory() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let note = volume.join("note.txt");
        std::fs::write(&note, b"note").unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o640)).unwrap();
        persist_volume_posix_metadata(root.path(), &[(String::from("/data"), volume.clone())])
            .unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o600)).unwrap();
        let scratch = root.path().join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
        std::fs::create_dir(&scratch).unwrap();

        restore_volume_posix_metadata(root.path()).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&note).unwrap().mode() & 0o7777,
            0o640
        );
        assert!(!scratch.exists());
    }

    #[test]
    fn restore_keeps_a_nonempty_publish_scratch_directory() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let note = volume.join("note.txt");
        std::fs::write(&note, b"note").unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o640)).unwrap();
        persist_volume_posix_metadata(root.path(), &[(String::from("/data"), volume.clone())])
            .unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o600)).unwrap();
        let scratch = root.path().join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
        std::fs::create_dir(&scratch).unwrap();
        let marker = scratch.join("keep.txt");
        std::fs::write(&marker, b"keep").unwrap();

        restore_volume_posix_metadata(root.path()).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&note).unwrap().mode() & 0o7777,
            0o640
        );
        assert!(scratch.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
    }

    #[test]
    fn restore_drops_an_older_publish_capture() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let file = volume.join("note.txt");
        std::fs::write(&file, b"note").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let owner = std::fs::symlink_metadata(&file).unwrap();
        let encode = |path: &str| base64::engine::general_purpose::STANDARD.encode(path);
        let entry = |mode: u32| RootfsMetadataEntry {
            path_base64: encode("note.txt"),
            kind: RootfsEntryKind::Regular,
            mode,
            uid: owner.uid() as u64,
            gid: owner.gid() as u64,
            mtime: 0,
            size: 4,
            link_target_base64: None,
        };
        let write_mode = |name: &str, mode: u32| {
            let manifest = VolumePosixManifest::new(vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(mode)],
                retained: false,
            }]);
            let path = root.path().join(name);
            std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
            path
        };
        let manifest = write_mode(VOLUME_POSIX_METADATA_FILE, 0o644);
        let temp = write_mode(VOLUME_POSIX_METADATA_TEMP_FILE, 0o640);
        let published = write_mode(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE, 0o600);
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(
                    std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs),
                )
                .unwrap();
        };
        set_mtime(&manifest, 100);
        set_mtime(&temp, 50);
        set_mtime(&published, 80);

        restore_volume_posix_metadata(root.path()).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&file).unwrap().mode() & 0o7777,
            0o644
        );
        assert!(!published.exists());
        assert!(!temp.exists());
    }

    #[test]
    fn partial_replay_keeps_the_newer_temp_capture() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let note = volume.join("note.txt");
        let blocked = volume.join("blocked.txt");
        std::fs::write(&note, b"note").unwrap();
        std::fs::write(&blocked, b"stop").unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700)).unwrap();
        let owner = std::fs::symlink_metadata(&note).unwrap();
        let encode = |path: &str| base64::engine::general_purpose::STANDARD.encode(path);
        let entry = |path: &str, mode: u32, uid: u64| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: RootfsEntryKind::Regular,
            mode,
            uid,
            gid: owner.gid() as u64,
            mtime: 0,
            size: 4,
            link_target_base64: None,
        };
        let manifest = VolumePosixManifest::new(vec![VolumePosixMount {
            guest_path: "/data".to_string(),
            entries: vec![entry("note.txt", 0o644, owner.uid() as u64)],
            retained: false,
        }]);
        let temp_manifest = VolumePosixManifest::new(vec![VolumePosixMount {
            guest_path: "/data".to_string(),
            entries: vec![
                entry("blocked.txt", 0o600, 0),
                entry("note.txt", 0o640, owner.uid() as u64),
            ],
            retained: false,
        }]);
        let manifest_path = root.path().join(VOLUME_POSIX_METADATA_FILE);
        let temp = root.path().join(VOLUME_POSIX_METADATA_TEMP_FILE);
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        std::fs::write(&temp, serde_json::to_vec(&temp_manifest).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(
                    std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs),
                )
                .unwrap();
        };
        set_mtime(&manifest_path, 10);
        set_mtime(&temp, 50);

        restore_volume_posix_metadata(root.path()).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&note).unwrap().mode() & 0o7777,
            0o640
        );
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&temp).unwrap()).unwrap();
        let kept_note = kept.mounts[0]
            .entries
            .iter()
            .find(|item| item.path_base64 == encode("note.txt"))
            .unwrap();
        assert_eq!(kept_note.mode, 0o640);
        if owner.uid() == 0 {
            return;
        }
        assert_eq!(
            std::fs::symlink_metadata(&blocked).unwrap().mode() & 0o7777,
            0o700
        );
        let pending: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(root.path().join(VOLUME_POSIX_METADATA_PENDING_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            pending.mounts[0].entries[0].path_base64,
            encode("blocked.txt")
        );
    }

    #[test]
    fn persist_keeps_a_newer_temp_when_the_manifest_cannot_be_replaced() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let note = volume.join("note.txt");
        std::fs::write(&note, b"note").unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o755)).unwrap();
        let owner = std::fs::symlink_metadata(&note).unwrap();
        let encode = |path: &str| base64::engine::general_purpose::STANDARD.encode(path);
        let captured = VolumePosixManifest::new(vec![VolumePosixMount {
            guest_path: "/data".to_string(),
            entries: vec![RootfsMetadataEntry {
                path_base64: encode("note.txt"),
                kind: RootfsEntryKind::Regular,
                mode: 0o640,
                uid: owner.uid() as u64,
                gid: owner.gid() as u64,
                mtime: 0,
                size: 4,
                link_target_base64: None,
            }],
            retained: false,
        }]);
        let temp = root.path().join(VOLUME_POSIX_METADATA_TEMP_FILE);
        std::fs::write(&temp, serde_json::to_vec(&captured).unwrap()).unwrap();
        let committed = root.path().join(VOLUME_POSIX_METADATA_FILE);
        std::fs::create_dir(&committed).unwrap();
        let marker = committed.join("keep.txt");
        std::fs::write(&marker, b"keep").unwrap();

        persist_volume_posix_metadata(root.path(), &[(String::from("/data"), volume)]).unwrap();
        assert!(committed.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&temp).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
    }

    #[test]
    fn persist_keeps_a_newer_publish_capture_when_the_manifest_cannot_be_replaced() {
        let root = tempfile::TempDir::new().unwrap();
        let volume = root.path().join("data");
        std::fs::create_dir(&volume).unwrap();
        let note = volume.join("note.txt");
        std::fs::write(&note, b"note").unwrap();
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o755)).unwrap();
        let owner = std::fs::symlink_metadata(&note).unwrap();
        if owner.uid() == 0 {
            return;
        }
        let encode = |path: &str| base64::engine::general_purpose::STANDARD.encode(path);
        let entry = |mode: u32| RootfsMetadataEntry {
            path_base64: encode("note.txt"),
            kind: RootfsEntryKind::Regular,
            mode,
            uid: owner.uid() as u64,
            gid: owner.gid() as u64,
            mtime: 0,
            size: 4,
            link_target_base64: None,
        };
        let write_mode = |name: &str, mode: u32| {
            let manifest = VolumePosixManifest::new(vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(mode)],
                retained: false,
            }]);
            let path = root.path().join(name);
            std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
            path
        };
        let manifest = write_mode(VOLUME_POSIX_METADATA_FILE, 0o644);
        let temp = write_mode(VOLUME_POSIX_METADATA_TEMP_FILE, 0o640);
        let published = write_mode(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE, 0o600);
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(
                    std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs),
                )
                .unwrap();
        };
        set_mtime(&manifest, 10);
        set_mtime(&temp, 50);
        set_mtime(&published, 80);
        struct ResetMode<'a>(&'a std::path::Path);
        impl Drop for ResetMode<'_> {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o755));
            }
        }
        let _reset = ResetMode(root.path());
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o555)).unwrap();

        let error = persist_volume_posix_metadata(root.path(), &[(String::from("/data"), volume)])
            .unwrap_err();
        assert!(error.to_string().len() > 0);
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&published).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o600);
        let durable: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&temp).unwrap()).unwrap();
        assert_eq!(durable.mounts[0].entries[0].mode, 0o640);
    }

    struct LockedDir<'a>(&'a std::path::Path);

    impl Drop for LockedDir<'_> {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o755));
        }
    }

    fn encode_path(path: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(path)
    }
}
