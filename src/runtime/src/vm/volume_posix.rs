//! Windows VolumeStore POSIX sidecars.
//!
//! Guest-init writes `/.a3s_volume_metadata_v1.json` on the box rootfs. Stop
//! copies each managed-volume mount into a sibling sidecar under `volumes/`.
//! The next box stages those entries onto a fresh rootfs and rewrites the
//! guest path to the current mount.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use a3s_box_core::error::BoxError;
use a3s_box_core::volume_posix::{
    managed_volume_directory, read_volume_posix_sidecar, write_volume_posix_sidecar,
    VolumePosixBinding, VolumePosixBindings, VolumePosixManifest, VolumePosixMount,
    VolumePosixSidecar, VOLUME_POSIX_BINDINGS_FILE, VOLUME_POSIX_METADATA_FILE,
    VOLUME_POSIX_METADATA_MAX_BYTES, VOLUME_POSIX_METADATA_PENDING_FILE,
    VOLUME_POSIX_METADATA_PENDING_TEMP_FILE, VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE,
    VOLUME_POSIX_METADATA_TEMP_FILE,
};

/// Record this boot's managed mounts and stage VolumeStore sidecars.
///
/// A guest path that the previous boot bound to a volume, and that this boot
/// no longer mounts, is removed from the rootfs manifest. Otherwise replay
/// would chmod that path on the container rootfs.
pub(crate) fn sync_managed_volume_posix(
    box_dir: &Path,
    rootfs: &Path,
    mounts: &[(String, PathBuf)],
) -> Result<(), BoxError> {
    sync_volume_posix(box_dir, rootfs, mounts, &[])
}

/// Stage managed-volume sidecars and keep metadata for every share mounted
/// this boot.
///
/// `replay_mounts` are caller binds mounted this boot. Their entries stay on
/// the box rootfs. The binding records the host path so a later managed
/// volume at the same guest path does not inherit them. A guest path outside
/// this set, other than `/workspace`, is dropped.
pub(crate) fn sync_volume_posix(
    box_dir: &Path,
    rootfs: &Path,
    mounts: &[(String, PathBuf)],
    replay_mounts: &[(String, PathBuf)],
) -> Result<(), BoxError> {
    let path = box_dir.join(VOLUME_POSIX_BINDINGS_FILE);
    let previous = read_bindings(box_dir)?;
    stage_volume_posix_sidecars(rootfs, mounts, previous.as_ref(), replay_mounts)?;
    let mut binding_mounts: Vec<VolumePosixBinding> = mounts
        .iter()
        .map(|(guest_path, host_path)| VolumePosixBinding {
            guest_path: guest_path.clone(),
            host_path: host_path.clone(),
        })
        .collect();
    for (guest_path, host_path) in replay_mounts {
        if binding_mounts
            .iter()
            .any(|binding| binding.guest_path == *guest_path)
        {
            continue;
        }
        binding_mounts.push(VolumePosixBinding {
            guest_path: guest_path.clone(),
            host_path: host_path.clone(),
        });
    }
    if binding_mounts.is_empty() {
        return clear_bindings_when_unmounted(&path);
    }
    let bindings = VolumePosixBindings::new(binding_mounts);
    bindings.validate().map_err(BoxError::ConfigError)?;
    #[cfg(windows)]
    {
        let mut prefix = PathBuf::new();
        for component in box_dir.components() {
            prefix.push(component);
            crate::vm::refuse_directory_reparse(&prefix)?;
        }
    }
    std::fs::create_dir_all(box_dir).map_err(BoxError::IoError)?;
    // An empty directory is removed and replaced. A nonempty directory stays,
    // so this boot does not delete its contents or follow a link.
    if !prepare_bindings_write(&path)? {
        return Ok(());
    }
    let bytes = serde_json::to_vec(&bindings).map_err(|error| {
        BoxError::IoError(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    })?;
    let tmp = path.with_extension("json.tmp");
    // The scratch is not a capture. An empty directory is removed. A
    // nonempty directory stays, and this boot still keeps the manifest.
    if !prepare_bindings_write(&tmp)? {
        return Ok(());
    }
    a3s_box_core::fs_atomic::write_durable(&tmp, &path, &bytes).map_err(BoxError::IoError)
}

/// Copy a clean-shutdown guest manifest into VolumeStore sidecars.
///
/// Mounts that are not direct children of `volumes_dir` are skipped. A missing
/// manifest is a crash or a box with nothing to capture. A guest manifest
/// older than the sidecar does not replace that sidecar. An unreadable
/// manifest in one root does not hide a valid manifest in another.
pub fn harvest_volume_posix_sidecars(box_dir: &Path, volumes_dir: &Path) -> Result<(), BoxError> {
    harvest_volume_posix_sidecars_with_paths(box_dir, volumes_dir, &[])
}

fn harvest_volume_posix_sidecars_with_paths(
    box_dir: &Path,
    volumes_dir: &Path,
    share_paths: &[String],
) -> Result<(), BoxError> {
    let Some(bindings) = read_bindings(box_dir)? else {
        return Ok(());
    };
    let Some((manifest_mtime, manifest)) = read_guest_manifest(box_dir)? else {
        return Ok(());
    };
    let mut mounts = manifest.mounts;
    include_binding_guest_paths(&mut mounts, &bindings.mounts);
    include_guest_paths(&mut mounts, share_paths);
    let _ = a3s_box_core::volume_posix::strip_entries_covered_by_nested_mounts(&mut mounts);
    // A second guest path of the same host is not a prefix of the nested
    // share, so the strip above leaves that copy in place. Drop it before
    // the captures are compared or written.
    let host_mounts: Vec<(String, PathBuf)> = bindings
        .mounts
        .iter()
        .map(|binding| (binding.guest_path.clone(), binding.host_path.clone()))
        .collect();
    let _ = strip_shared_host_nested_entries(&mut mounts, &host_mounts);
    let volume_dir_of = |guest_path: &str| -> Option<PathBuf> {
        let binding = bindings
            .mounts
            .iter()
            .find(|binding| binding.guest_path == guest_path)?;
        managed_volume_directory(volumes_dir, &binding.host_path)
    };
    let mut retained_entries: Vec<(PathBuf, Vec<_>)> = Vec::new();
    let mut blocked: Vec<PathBuf> = Vec::new();
    for mount in &mounts {
        if !mount.retained || mount.entries.is_empty() {
            continue;
        }
        let Some(volume_dir) = volume_dir_of(&mount.guest_path) else {
            continue;
        };
        // A store reached through a link is not this box's directory. Do not
        // record it, or a later write follows the link onto the outside tree.
        if crate::volume::managed_volume_ancestor_is_link(&volume_dir) {
            continue;
        }
        if let Some((_, previous)) = retained_entries
            .iter()
            .find(|(dir, _)| same_volume_host(dir, &volume_dir))
        {
            if previous != &mount.entries {
                blocked.push(volume_dir);
            }
        } else {
            retained_entries.push((volume_dir, mount.entries.clone()));
        }
    }
    let guest_paths: Vec<String> = mounts
        .iter()
        .map(|mount| mount.guest_path.clone())
        .collect();
    let mut chosen: Vec<(PathBuf, (String, Vec<_>))> = Vec::new();
    for mount in mounts {
        let Some(volume_dir) = volume_dir_of(&mount.guest_path) else {
            continue;
        };
        if crate::volume::managed_volume_ancestor_is_link(&volume_dir) {
            continue;
        }
        if mount.entries.is_empty() || blocked.iter().any(|dir| same_volume_host(dir, &volume_dir))
        {
            strip_sidecar_entries_inside_nested_shares(
                &volume_dir,
                &mount.guest_path,
                &guest_paths,
                &host_mounts,
            )?;
            continue;
        }
        if let Some((_, retained)) = retained_entries
            .iter()
            .find(|(dir, _)| same_volume_host(dir, &volume_dir))
        {
            if retained != &mount.entries {
                blocked.push(volume_dir.clone());
                chosen.retain(|(dir, _)| !same_volume_host(dir, &volume_dir));
                continue;
            }
        }
        if mount.retained {
            strip_sidecar_entries_inside_nested_shares(
                &volume_dir,
                &mount.guest_path,
                &guest_paths,
                &host_mounts,
            )?;
            continue;
        }
        if let Some((previous_guest, previous_entries)) = chosen
            .iter()
            .find(|(dir, _)| same_volume_host(dir, &volume_dir))
            .map(|(_, captured)| captured)
        {
            if previous_entries != &mount.entries {
                // The captures stay out of the sidecar. A nested share's
                // file is not one of those modes, so it is still removed.
                strip_sidecar_entries_inside_nested_shares(
                    &volume_dir,
                    &mount.guest_path,
                    &guest_paths,
                    &host_mounts,
                )?;
                return Err(BoxError::ConfigError(format!(
                    "volume posix captures for {} disagree between {previous_guest} and {}",
                    volume_dir.display(),
                    mount.guest_path
                )));
            }
            continue;
        }
        chosen.push((volume_dir, (mount.guest_path, mount.entries)));
    }
    let mut blocked_scratch = false;
    for (volume_dir, (guest_path, entries)) in chosen {
        // A directory or link is not a sidecar. Skip it so one planted path
        // does not stop the other volumes, and do not follow or remove it.
        if sidecar_is_planted(&volume_dir)? {
            continue;
        }
        if sidecar_mtime(&volume_dir)?
            .map(|mtime| mtime > manifest_mtime)
            .unwrap_or(false)
        {
            // The newer sidecar keeps its modes. Entries that belong to
            // another share are still removed.
            strip_sidecar_entries_inside_nested_shares(
                &volume_dir,
                &guest_path,
                &guest_paths,
                &host_mounts,
            )?;
            continue;
        }
        // The scratch is not a capture. An empty directory is removed so the
        // write can proceed. A nonempty directory stays. The other volumes
        // are still written, and the harvest then fails so the blocked
        // sidecar is not reported as updated.
        if let Some(sidecar_path) =
            a3s_box_core::volume_posix::volume_posix_sidecar_path(&volume_dir)
        {
            if !prepare_bindings_write(&sidecar_path.with_extension("json.tmp"))? {
                blocked_scratch = true;
                continue;
            }
        }
        write_volume_posix_sidecar(&volume_dir, &VolumePosixSidecar::new(entries))
            .map_err(BoxError::IoError)?;
    }
    if blocked_scratch {
        return Err(BoxError::IoError(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "volume posix sidecar scratch is not a writable file",
        )));
    }
    Ok(())
}

/// Sidecar modes stay in place. Entries that fall inside another share are
/// not this volume's modes, so they are removed.
fn strip_sidecar_entries_inside_nested_shares(
    volume_dir: &Path,
    guest_path: &str,
    guest_paths: &[String],
    host_mounts: &[(String, PathBuf)],
) -> Result<(), BoxError> {
    if sidecar_is_planted(volume_dir)? {
        return Ok(());
    }
    let Some(sidecar) = read_volume_posix_sidecar(volume_dir).map_err(BoxError::IoError)? else {
        return Ok(());
    };
    let mut recorded = vec![VolumePosixMount {
        guest_path: guest_path.to_string(),
        entries: sidecar.entries.clone(),
        retained: true,
    }];
    include_guest_paths(&mut recorded, guest_paths);
    let _ = a3s_box_core::volume_posix::strip_entries_covered_by_nested_mounts(&mut recorded);
    // The chosen guest path may be another mount of this host. A nested
    // share under a sibling path still names these files.
    let _ = strip_shared_host_nested_entries(&mut recorded, host_mounts);
    let Some(mount) = recorded
        .into_iter()
        .find(|mount| mount.guest_path == guest_path)
    else {
        return Ok(());
    };
    if mount.entries == sidecar.entries {
        return Ok(());
    }
    if let Some(sidecar_path) = a3s_box_core::volume_posix::volume_posix_sidecar_path(volume_dir) {
        if !prepare_bindings_write(&sidecar_path.with_extension("json.tmp"))? {
            return Err(BoxError::IoError(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "volume posix sidecar scratch is not a writable file",
            )));
        }
    }
    write_volume_posix_sidecar(volume_dir, &VolumePosixSidecar::new(mount.entries))
        .map_err(BoxError::IoError)
}

fn stage_volume_posix_sidecars(
    rootfs: &Path,
    mounts: &[(String, PathBuf)],
    previous: Option<&VolumePosixBindings>,
    replay_mounts: &[(String, PathBuf)],
) -> Result<(), BoxError> {
    let mut share_paths = Vec::new();
    let mut host_mounts = Vec::new();
    for (guest_path, host_path) in mounts.iter().chain(replay_mounts.iter()) {
        if share_paths.iter().any(|path: &String| path == guest_path) {
            continue;
        }
        share_paths.push(guest_path.clone());
        host_mounts.push((guest_path.clone(), host_path.clone()));
    }
    let destination = rootfs.join(VOLUME_POSIX_METADATA_FILE);
    let loaded = if rootfs
        .parent()
        .is_some_and(|parent| parent.join("rootfs") == rootfs)
    {
        let box_dir = rootfs.parent().expect("rootfs parent");
        read_guest_manifest(box_dir)?.map(|(mtime, manifest)| (Some(mtime), manifest))
    } else {
        match read_manifest_file(&destination)? {
            Some(manifest) => Some((file_mtime(&destination), manifest)),
            None if !regular_file_exists(&destination)? => None,
            None => {
                return Err(BoxError::ConfigError(format!(
                    "volume posix metadata at {} is not a valid manifest",
                    destination.display()
                )))
            }
        }
    };
    let (manifest_mtime, existing) = match loaded {
        Some((mtime, manifest)) => (mtime, Some(manifest)),
        None => (None, None),
    };
    let had_manifest = existing.is_some();
    // The guest reads this path before any temp or publish capture. An
    // oversized file or a directory at that name hides a valid manifest
    // that was already loaded. Nothing valid left still fails in
    // `read_guest_manifest`. An empty directory is removed. A nonempty
    // directory stays, and the capture is published beside it. A link is
    // unlinked and not followed.
    if guest_root_manifest_blocks_replay(&destination)? {
        remove_unless_nonempty_directory(&destination)?;
    }
    let mut staged = existing.map(|manifest| manifest.mounts).unwrap_or_default();
    let mut changed = !had_manifest;
    if discard_unusable_mounts(&mut staged, previous, &share_paths) {
        changed = true;
    }
    // A nested file listed on only one alias is the same host file. Remove
    // it before disagreement drops the rest of that volume's capture.
    if strip_shared_host_nested_entries(&mut staged, &host_mounts) {
        changed = true;
    }
    if drop_disagreeing_captures(&mut staged, &host_mounts) {
        changed = true;
    }
    if retarget_retired_volume_mounts(&mut staged, previous, &host_mounts) {
        changed = true;
    }
    let mut sidecar_size_error: Option<BoxError> = None;
    for (guest_path, host_path) in mounts {
        // An oversized sidecar is not read. It does not replace a manifest
        // that is already valid. The size error remains when that sidecar is
        // the only capture. Read before the mtime check: an oversized file
        // has no capture mtime, and skipping it here would drop the error.
        let sidecar = match read_sidecar(host_path) {
            Ok(Some(sidecar)) => sidecar,
            Ok(None) => continue,
            Err(error) if is_sidecar_size_limit(&error) => {
                if sidecar_size_error.is_none() {
                    sidecar_size_error = Some(error);
                }
                continue;
            }
            Err(error) => return Err(error),
        };
        let Some(sidecar_mtime) = sidecar_mtime(host_path)? else {
            continue;
        };
        let sidecar_is_newer = manifest_mtime
            .map(|mtime| sidecar_mtime > mtime)
            .unwrap_or(true);
        let already_staged = staged
            .iter()
            .any(|current| current.guest_path == *guest_path);
        let host_already_staged = mounts.iter().any(|(other_guest, other_host)| {
            same_volume_host(other_host, host_path)
                && staged
                    .iter()
                    .any(|current| current.guest_path == *other_guest)
        });
        // An older sidecar still applies when this boot's volume is not the
        // one captured in the rootfs manifest. The manifest mtime belongs to
        // the previous owner of the path. A second guest path of a volume
        // that is already staged must not take that older sidecar instead.
        if had_manifest && !sidecar_is_newer && (already_staged || host_already_staged) {
            continue;
        }
        let mount = VolumePosixMount {
            guest_path: guest_path.clone(),
            entries: sidecar.entries,
            retained: false,
        };
        if let Some(current) = staged
            .iter_mut()
            .find(|current| current.guest_path == *guest_path)
        {
            if *current != mount {
                *current = mount;
                changed = true;
            }
        } else {
            staged.push(mount);
            changed = true;
        }
    }
    if fan_out_shared_host_entries(&mut staged, &host_mounts) {
        changed = true;
    }
    // Fan-out copies one entry set onto every guest path of that host. A
    // nested share is only a prefix of some of those paths, so the copy can
    // put the nested file back on another path, including a replay alias.
    // Drop it from every alias.
    if strip_shared_host_nested_entries(&mut staged, &host_mounts) {
        changed = true;
    }
    if discard_unusable_mounts(&mut staged, previous, &share_paths) {
        changed = true;
    }
    if drop_guest_paths_outside_this_boot(&mut staged, mounts, replay_mounts) {
        changed = true;
    }
    if !had_manifest && staged.is_empty() {
        if let Some(error) = sidecar_size_error {
            return Err(error);
        }
    }
    // A volume this boot no longer mounts can still have a newer mode in the
    // on-disk capture. Copy that mode to its sidecar before a later write
    // drops the guest path. `/workspace` staying behind must not skip this.
    flush_dropped_volume_sidecars(rootfs, &staged, &share_paths)?;
    // The loaded capture may live in another root. The guest reads this
    // rootfs file, so a matching capture elsewhere still has to be published.
    // An older copy in another root still hides this file in the overlay view.
    if !changed && destination_has_staged_manifest(&destination, &staged)? {
        return clear_shadowing_manifests(rootfs, &staged, &share_paths, &host_mounts);
    }
    // Nothing left to replay. Copy the retired capture into VolumeStore
    // sidecars first, then drop guest-readable copies so the next guest
    // does not chmod an unmounted path. A nonempty directory at the
    // committed name stays.
    if staged.is_empty() {
        flush_retired_manifest(rootfs, &share_paths)?;
        remove_unless_nonempty_directory(&destination)?;
        remove_unless_nonempty_directory(&rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE))?;
        remove_unless_nonempty_directory(&rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE))?;
        return clear_shadowing_manifests(rootfs, &staged, &share_paths, &host_mounts);
    }
    // A newer temp or publish file is already the capture the guest reads.
    // Copying it onto the manifest and deleting it would let replay retire
    // the only remaining copy before harvest.
    if staged_bytes_already_in_remaining_capture(rootfs, &staged)? {
        return clear_shadowing_manifests(rootfs, &staged, &share_paths, &host_mounts);
    }
    // A newer capture in merged or upper is not on the guest root. Copy it
    // beside the committed manifest. Replacing that manifest would let
    // replay delete the only copy harvest can still read.
    if other_root_capture_is_authoritative(rootfs, mounts)? {
        write_guest_root_publish_capture(rootfs, &staged)?;
        return clear_shadowing_manifests(rootfs, &staged, &share_paths, &host_mounts);
    }
    // A rootfs temp or publish may still hold an unharvested mount after a
    // newer sidecar replaces only another mount. Folding the result onto
    // the committed manifest would delete that capture before harvest.
    if remaining_capture_keeps_unharvested_mount(rootfs, mounts)? {
        write_guest_root_publish_capture(rootfs, &staged)?;
        return clear_shadowing_manifests(rootfs, &staged, &share_paths, &host_mounts);
    }
    // Caller paths such as `/workspace` live only on the manifest. Replacing
    // the committed file and then letting replay delete it drops those
    // entries. Keep a newer copy beside that file.
    if staged_has_manifest_only_entries(&staged, mounts)? {
        publish_manifest(&destination, VolumePosixManifest::new(staged.clone()))?;
        write_guest_root_publish_capture(rootfs, &staged)?;
        mark_publish_newer_than_committed(rootfs)?;
        return clear_shadowing_manifests(rootfs, &staged, &share_paths, &host_mounts);
    }
    publish_manifest(&destination, VolumePosixManifest::new(staged.clone()))?;
    clear_shadowing_manifests(rootfs, &staged, &share_paths, &host_mounts)
}

/// Remove manifest copies that would hide the staged rootfs file.
///
/// The guest reads the overlay view, where `upper` wins over `rootfs`.
/// A copy left behind replays the pre-stage capture. A pending replay
/// beside a retired manifest is removed as well: the next capture would
/// otherwise overlay those entries onto a fresh listing.
fn destination_has_staged_manifest(
    destination: &Path,
    staged: &[VolumePosixMount],
) -> Result<bool, BoxError> {
    match read_manifest_file(destination) {
        Ok(Some(manifest)) => Ok(manifest.mounts == staged),
        Ok(None) if !regular_file_exists(destination)? => Ok(staged.is_empty()),
        Ok(None) => Ok(false),
        // A directory left in place is not the staged manifest. The capture
        // is published beside it.
        Err(error) if is_not_regular_metadata(&error) || is_metadata_size_limit(&error) => {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn staged_bytes_already_in_remaining_capture(
    rootfs: &Path,
    staged: &[VolumePosixMount],
) -> Result<bool, BoxError> {
    let destination = rootfs.join(VOLUME_POSIX_METADATA_FILE);
    for file_name in [
        VOLUME_POSIX_METADATA_TEMP_FILE,
        VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE,
    ] {
        let path = rootfs.join(file_name);
        if !durable_temp_is_the_remaining_capture(&destination, &path)? {
            continue;
        }
        if read_manifest_file(&path)?.is_some_and(|manifest| manifest.mounts == staged) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn other_root_capture_is_authoritative(
    rootfs: &Path,
    mounts: &[(String, PathBuf)],
) -> Result<bool, BoxError> {
    let destination = rootfs.join(VOLUME_POSIX_METADATA_FILE);
    let dest_mtime = file_mtime(&destination);
    let Some(box_dir) = rootfs
        .parent()
        .filter(|parent| parent.join("rootfs") == rootfs)
    else {
        return Ok(false);
    };
    for root_name in ["merged", "upper"] {
        for file_name in [
            VOLUME_POSIX_METADATA_FILE,
            VOLUME_POSIX_METADATA_TEMP_FILE,
            VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE,
        ] {
            let path = box_dir.join(root_name).join(file_name);
            let manifest = match read_manifest_file(&path) {
                Ok(Some(manifest)) => manifest,
                Ok(None) => continue,
                Err(error) if is_metadata_size_limit(&error) || is_not_regular_metadata(&error) => {
                    continue;
                }
                Err(error) => return Err(error),
            };
            let mtime = file_mtime(&path).unwrap_or(SystemTime::UNIX_EPOCH);
            let dest_valid = match read_manifest_file(&destination) {
                Ok(manifest) => manifest.is_some(),
                Err(error) if is_not_regular_metadata(&error) || is_metadata_size_limit(&error) => {
                    false
                }
                Err(error) => return Err(error),
            };
            // An invalid committed manifest still has a mtime. The valid
            // capture in another root is what the guest and harvest read.
            let newer_than_dest = if !dest_valid {
                true
            } else {
                match dest_mtime {
                    Some(dest) => mtime > dest,
                    None => true,
                }
            };
            if !newer_than_dest {
                continue;
            }
            // A newer sidecar replaces only its own mount. Another mount in
            // this capture is still the copy harvest has to read after replay
            // retires the committed manifest.
            let mut keeps_unharvested_mount = false;
            for mount in &manifest.mounts {
                let host_path = mounts
                    .iter()
                    .find(|(guest_path, _)| guest_path == &mount.guest_path)
                    .map(|(_, host_path)| host_path);
                let replaced = match host_path {
                    Some(host_path) => {
                        sidecar_mtime(host_path)?.is_some_and(|sidecar| sidecar > mtime)
                    }
                    None => false,
                };
                if !replaced {
                    keeps_unharvested_mount = true;
                    break;
                }
            }
            if keeps_unharvested_mount {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn remaining_capture_keeps_unharvested_mount(
    rootfs: &Path,
    mounts: &[(String, PathBuf)],
) -> Result<bool, BoxError> {
    let destination = rootfs.join(VOLUME_POSIX_METADATA_FILE);
    for file_name in [
        VOLUME_POSIX_METADATA_TEMP_FILE,
        VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE,
    ] {
        let path = rootfs.join(file_name);
        if !durable_temp_is_the_remaining_capture(&destination, &path)? {
            continue;
        }
        let Some(manifest) = read_manifest_file(&path)? else {
            continue;
        };
        let mtime = file_mtime(&path).unwrap_or(SystemTime::UNIX_EPOCH);
        for mount in &manifest.mounts {
            let host_path = mounts
                .iter()
                .find(|(guest_path, _)| guest_path == &mount.guest_path)
                .map(|(_, host_path)| host_path);
            let replaced = match host_path {
                Some(host_path) => sidecar_mtime(host_path)?.is_some_and(|sidecar| sidecar > mtime),
                None => false,
            };
            if !replaced {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn staged_has_manifest_only_entries(
    staged: &[VolumePosixMount],
    mounts: &[(String, PathBuf)],
) -> Result<bool, BoxError> {
    for mount in staged {
        let Some((_, host_path)) = mounts
            .iter()
            .find(|(guest_path, _)| guest_path == &mount.guest_path)
        else {
            return Ok(true);
        };
        let sidecar = match read_sidecar(host_path) {
            Ok(Some(sidecar)) => sidecar,
            Ok(None) => return Ok(true),
            Err(error) if is_sidecar_size_limit(&error) => return Ok(true),
            Err(error) => return Err(error),
        };
        if sidecar.entries != mount.entries {
            return Ok(true);
        }
    }
    Ok(false)
}

fn mark_publish_newer_than_committed(rootfs: &Path) -> Result<(), BoxError> {
    let destination = rootfs.join(VOLUME_POSIX_METADATA_FILE);
    let publish = rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
    let Some(dest_mtime) = file_mtime(&destination) else {
        return Ok(());
    };
    if file_mtime(&publish).is_some_and(|mtime| mtime > dest_mtime) {
        return Ok(());
    }
    let Some(marked) = dest_mtime.checked_add(std::time::Duration::from_secs(1)) else {
        return Ok(());
    };
    std::fs::File::options()
        .write(true)
        .open(&publish)
        .map_err(BoxError::IoError)?
        .set_modified(marked)
        .map_err(BoxError::IoError)
}

fn write_guest_root_publish_capture(
    rootfs: &Path,
    staged: &[VolumePosixMount],
) -> Result<(), BoxError> {
    let manifest = VolumePosixManifest::new(staged.to_vec());
    manifest.validate().map_err(BoxError::ConfigError)?;
    let bytes = serde_json::to_vec(&manifest).map_err(|error| {
        BoxError::IoError(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    })?;
    if bytes.len() as u64 > VOLUME_POSIX_METADATA_MAX_BYTES {
        return Err(BoxError::ConfigError(
            "volume posix metadata exceeds the size limit".to_string(),
        ));
    }
    let destination = rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
    let pending = rootfs.join(VOLUME_POSIX_METADATA_PENDING_TEMP_FILE);
    let temp = rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE);
    let committed = rootfs.join(VOLUME_POSIX_METADATA_FILE);
    let mut candidates = vec![pending];
    if !durable_temp_is_the_remaining_capture(&committed, &temp)? {
        candidates.push(temp);
    }
    let mut chosen = None;
    for candidate in &candidates {
        if prepare_bindings_write(candidate)? {
            chosen = Some(candidate.clone());
            break;
        }
    }
    let Some(scratch) = chosen else {
        return Err(BoxError::IoError(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "volume posix publish scratch is not a writable file",
        )));
    };
    a3s_box_core::fs_atomic::write_durable(&scratch, &destination, &bytes)
        .map_err(BoxError::IoError)
}

/// Write the on-disk capture into sidecars before its guest copies are removed.
fn flush_retired_manifest(rootfs: &Path, share_paths: &[String]) -> Result<(), BoxError> {
    let Some(box_dir) = rootfs
        .parent()
        .filter(|parent| parent.join("rootfs") == rootfs)
    else {
        return Ok(());
    };
    let Some(bindings) = read_bindings(box_dir)? else {
        return Ok(());
    };
    let Some(volumes_dir) = bindings.mounts.iter().find_map(|binding| {
        let parent = a3s_box_core::volume_posix::volume_directory_parent(&binding.host_path)?;
        managed_volume_directory(&parent, &binding.host_path)?;
        Some(parent)
    }) else {
        return Ok(());
    };
    harvest_volume_posix_sidecars_with_paths(box_dir, &volumes_dir, share_paths)
}

/// Copy modes for volumes this boot dropped before their guest path is removed.
fn flush_dropped_volume_sidecars(
    rootfs: &Path,
    staged: &[VolumePosixMount],
    share_paths: &[String],
) -> Result<(), BoxError> {
    let Some(box_dir) = rootfs
        .parent()
        .filter(|parent| parent.join("rootfs") == rootfs)
    else {
        return Ok(());
    };
    let Some(bindings) = read_bindings(box_dir)? else {
        return Ok(());
    };
    let dropped = bindings.mounts.iter().any(|binding| {
        let managed = a3s_box_core::volume_posix::volume_directory_parent(&binding.host_path)
            .is_some_and(|parent| managed_volume_directory(&parent, &binding.host_path).is_some());
        managed
            && staged
                .iter()
                .all(|mount| mount.guest_path != binding.guest_path)
    });
    if !dropped {
        return Ok(());
    }
    // The newest capture can be a partial publish that already dropped this
    // guest path. An older file may still hold the mode harvest would skip.
    recover_dropped_volumes_omitted_by_newest(rootfs, staged, share_paths)?;
    flush_retired_manifest(rootfs, share_paths)
}

/// Copy a dropped volume from an older capture when the newest file omits it.
fn recover_dropped_volumes_omitted_by_newest(
    rootfs: &Path,
    staged: &[VolumePosixMount],
    share_paths: &[String],
) -> Result<(), BoxError> {
    let Some(box_dir) = rootfs
        .parent()
        .filter(|parent| parent.join("rootfs") == rootfs)
    else {
        return Ok(());
    };
    let Some(bindings) = read_bindings(box_dir)? else {
        return Ok(());
    };
    let Some((_, newest)) = read_guest_manifest(box_dir)? else {
        return Ok(());
    };
    for binding in &bindings.mounts {
        let Some(parent) = a3s_box_core::volume_posix::volume_directory_parent(&binding.host_path)
        else {
            continue;
        };
        if managed_volume_directory(&parent, &binding.host_path).is_none() {
            continue;
        }
        if staged
            .iter()
            .any(|mount| mount.guest_path == binding.guest_path)
        {
            continue;
        }
        if newest
            .mounts
            .iter()
            .any(|mount| mount.guest_path == binding.guest_path)
        {
            continue;
        }
        write_omitted_dropped_volume(
            box_dir,
            &binding.guest_path,
            &binding.host_path,
            &bindings.mounts,
            share_paths,
        )?;
    }
    Ok(())
}

fn write_omitted_dropped_volume(
    box_dir: &Path,
    guest_path: &str,
    volume_dir: &Path,
    bindings: &[VolumePosixBinding],
    share_paths: &[String],
) -> Result<(), BoxError> {
    if sidecar_is_planted(volume_dir)? {
        return Ok(());
    }
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for root_name in ["merged", "rootfs", "upper"] {
        let root = box_dir.join(root_name);
        for name in [
            VOLUME_POSIX_METADATA_FILE,
            VOLUME_POSIX_METADATA_TEMP_FILE,
            VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE,
        ] {
            let path = root.join(name);
            let manifest = match read_manifest_file(&path) {
                Ok(manifest) => manifest,
                Err(error) if is_not_regular_metadata(&error) || is_metadata_size_limit(&error) => {
                    continue;
                }
                Err(error) => return Err(error),
            };
            let Some(manifest) = manifest else {
                continue;
            };
            let Some(mount) = manifest
                .mounts
                .iter()
                .find(|mount| mount.guest_path == guest_path)
            else {
                continue;
            };
            if mount.retained || mount.entries.is_empty() {
                continue;
            }
            let mtime = file_mtime(&path).unwrap_or(SystemTime::UNIX_EPOCH);
            let replace = best.as_ref().map(|(time, _)| mtime > *time).unwrap_or(true);
            if replace {
                best = Some((mtime, path));
            }
        }
    }
    let Some((mtime, path)) = best else {
        return Ok(());
    };
    if sidecar_mtime(volume_dir)?.is_some_and(|sidecar| sidecar >= mtime) {
        return Ok(());
    }
    let Some(manifest) = read_manifest_file(&path)? else {
        return Ok(());
    };
    let mut mounts = manifest.mounts;
    include_binding_guest_paths(&mut mounts, bindings);
    include_guest_paths(&mut mounts, share_paths);
    let _ = a3s_box_core::volume_posix::strip_entries_covered_by_nested_mounts(&mut mounts);
    let host_mounts: Vec<(String, PathBuf)> = bindings
        .iter()
        .map(|binding| (binding.guest_path.clone(), binding.host_path.clone()))
        .collect();
    let _ = strip_shared_host_nested_entries(&mut mounts, &host_mounts);
    let Some(mount) = mounts
        .into_iter()
        .find(|mount| mount.guest_path == guest_path)
    else {
        return Ok(());
    };
    if mount.entries.is_empty() {
        return Ok(());
    }
    write_volume_posix_sidecar(volume_dir, &VolumePosixSidecar::new(mount.entries))
        .map_err(BoxError::IoError)
}

fn clear_shadowing_manifests(
    rootfs: &Path,
    staged: &[VolumePosixMount],
    guest_paths: &[String],
    host_mounts: &[(String, PathBuf)],
) -> Result<(), BoxError> {
    let destination = rootfs.join(VOLUME_POSIX_METADATA_FILE);
    let temp = rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE);
    let publish = rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
    // A committed manifest that still names a dropped path must not stay the
    // newest capture. Remove it before deciding which temp to keep, so the
    // staged publish written earlier remains the capture the guest reads.
    if capture_includes_unstaged_path(&destination, staged)?
        || capture_covers_bound_nested_share(rootfs, &destination, guest_paths, host_mounts)?
    {
        remove_unless_nonempty_directory(&destination)?;
    }
    let keep_temp = durable_temp_is_the_remaining_capture(&destination, &temp)?;
    let keep_publish = durable_temp_is_the_remaining_capture(&destination, &publish)?;
    let Some(box_dir) = rootfs
        .parent()
        .filter(|parent| parent.join("rootfs") == rootfs)
    else {
        remove_unless_nonempty_directory(&rootfs.join(VOLUME_POSIX_METADATA_PENDING_FILE))?;
        remove_unless_nonempty_directory(&rootfs.join(VOLUME_POSIX_METADATA_PENDING_TEMP_FILE))?;
        if !keep_publish
            || capture_includes_unstaged_path(&publish, staged)?
            || capture_covers_bound_nested_share(rootfs, &publish, guest_paths, host_mounts)?
        {
            remove_unless_nonempty_directory(&publish)?;
        }
        if !keep_temp
            || capture_includes_unstaged_path(&temp, staged)?
            || capture_covers_bound_nested_share(rootfs, &temp, guest_paths, host_mounts)?
        {
            remove_unless_nonempty_directory(&temp)?;
        }
        return Ok(());
    };
    // A nonempty directory at the committed name in another root wins the
    // overlay and hides the guest-root file. Leave the directory and publish
    // that file beside it so the guest still reads the capture.
    if other_root_committed_directory_shadows(&box_dir)? {
        publish_committed_manifest_beside_directory_shadow(rootfs)?;
    }
    let keep_temp = durable_temp_is_the_remaining_capture(&destination, &temp)?;
    let keep_publish = durable_temp_is_the_remaining_capture(&destination, &publish)?;
    for root_name in ["merged", "upper"] {
        let dir = box_dir.join(root_name);
        remove_unless_nonempty_directory(&dir.join(VOLUME_POSIX_METADATA_FILE))?;
        remove_unless_nonempty_directory(&dir.join(VOLUME_POSIX_METADATA_PENDING_FILE))?;
        remove_unless_nonempty_directory(&dir.join(VOLUME_POSIX_METADATA_PENDING_TEMP_FILE))?;
        remove_unless_nonempty_directory(&dir.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE))?;
        remove_unless_nonempty_directory(&dir.join(VOLUME_POSIX_METADATA_TEMP_FILE))?;
    }
    remove_unless_nonempty_directory(&rootfs.join(VOLUME_POSIX_METADATA_PENDING_FILE))?;
    remove_unless_nonempty_directory(&rootfs.join(VOLUME_POSIX_METADATA_PENDING_TEMP_FILE))?;
    if !keep_publish
        || capture_includes_unstaged_path(&publish, staged)?
        || capture_covers_bound_nested_share(rootfs, &publish, guest_paths, host_mounts)?
    {
        remove_unless_nonempty_directory(&publish)?;
    }
    if !keep_temp
        || capture_includes_unstaged_path(&temp, staged)?
        || capture_covers_bound_nested_share(rootfs, &temp, guest_paths, host_mounts)?
    {
        remove_unless_nonempty_directory(&temp)?;
    }
    Ok(())
}

/// A capture that still names a guest path this boot dropped must not stay
/// newer than the staged manifest. The guest would replay that path.
fn capture_includes_unstaged_path(
    path: &Path,
    staged: &[VolumePosixMount],
) -> Result<bool, BoxError> {
    let manifest = match read_manifest_file(path) {
        Ok(manifest) => manifest,
        Err(error) if is_not_regular_metadata(&error) || is_metadata_size_limit(&error) => {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    let Some(manifest) = manifest else {
        return Ok(false);
    };
    Ok(manifest.mounts.iter().any(|mount| {
        staged
            .iter()
            .all(|kept| kept.guest_path != mount.guest_path)
    }))
}

/// A capture that still lists a file inside a nested volume must not stay
/// newer than the staged manifest. The guest would chmod that nested share.
/// This boot's mounts count even when the binding file has not been written.
fn capture_covers_bound_nested_share(
    rootfs: &Path,
    path: &Path,
    guest_paths: &[String],
    host_mounts: &[(String, PathBuf)],
) -> Result<bool, BoxError> {
    let manifest = match read_manifest_file(path) {
        Ok(manifest) => manifest,
        Err(error) if is_not_regular_metadata(&error) || is_metadata_size_limit(&error) => {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    let Some(manifest) = manifest else {
        return Ok(false);
    };
    let original = manifest.mounts.clone();
    let mut mounts = manifest.mounts;
    let mut known_hosts = host_mounts.to_vec();
    if let Some(box_dir) = rootfs
        .parent()
        .filter(|parent| parent.join("rootfs") == rootfs)
    {
        if let Some(bindings) = read_bindings(box_dir)? {
            include_binding_guest_paths(&mut mounts, &bindings.mounts);
            for binding in &bindings.mounts {
                if known_hosts
                    .iter()
                    .any(|(guest_path, _)| guest_path == &binding.guest_path)
                {
                    continue;
                }
                known_hosts.push((binding.guest_path.clone(), binding.host_path.clone()));
            }
        }
    }
    include_guest_paths(&mut mounts, guest_paths);
    let _ = a3s_box_core::volume_posix::strip_entries_covered_by_nested_mounts(&mut mounts);
    let _ = strip_shared_host_nested_entries(&mut mounts, &known_hosts);
    Ok(original.iter().any(|mount| {
        mounts
            .iter()
            .find(|stripped| stripped.guest_path == mount.guest_path)
            .is_some_and(|stripped| stripped.entries.len() != mount.entries.len())
    }))
}

fn other_root_committed_directory_shadows(box_dir: &Path) -> Result<bool, BoxError> {
    for root_name in ["merged", "upper"] {
        let path = box_dir.join(root_name).join(VOLUME_POSIX_METADATA_FILE);
        if nonempty_directory(&path)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn nonempty_directory(path: &Path) -> Result<bool, BoxError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !is_reparse_or_symlink(&metadata) => {
            let mut entries = std::fs::read_dir(path).map_err(BoxError::IoError)?;
            Ok(entries
                .next()
                .transpose()
                .map_err(BoxError::IoError)?
                .is_some())
        }
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(BoxError::IoError(error)),
    }
}

fn empty_real_directory(path: &Path) -> Result<bool, BoxError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !is_reparse_or_symlink(&metadata) => {
            let mut entries = std::fs::read_dir(path).map_err(BoxError::IoError)?;
            Ok(entries
                .next()
                .transpose()
                .map_err(BoxError::IoError)?
                .is_none())
        }
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(BoxError::IoError(error)),
    }
}

fn publish_committed_manifest_beside_directory_shadow(rootfs: &Path) -> Result<(), BoxError> {
    let destination = rootfs.join(VOLUME_POSIX_METADATA_FILE);
    let manifest = match read_manifest_file(&destination) {
        Ok(Some(manifest)) => manifest,
        Ok(None) => return Ok(()),
        Err(error) if is_not_regular_metadata(&error) || is_metadata_size_limit(&error) => {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let publish = rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
    let publish_is_newer = match (file_mtime(&publish), file_mtime(&destination)) {
        (Some(publish_mtime), Some(dest_mtime)) => publish_mtime > dest_mtime,
        (Some(_), None) => true,
        _ => false,
    };
    if publish_is_newer
        && read_manifest_file(&publish)?.is_some_and(|current| current.mounts == manifest.mounts)
    {
        return Ok(());
    }
    write_guest_root_publish_capture(rootfs, &manifest.mounts)?;
    mark_publish_newer_than_committed(rootfs)
}

/// Keep captured entries with the host volume that produced them.
///
/// A volume mounted at a new guest path keeps its entries. A guest path whose
/// host volume changed does not keep the previous volume's entries. A volume
/// this boot does not mount is dropped. Paths that were never a volume
/// binding, such as `/workspace`, stay.
fn retarget_retired_volume_mounts(
    staged: &mut Vec<VolumePosixMount>,
    previous: Option<&VolumePosixBindings>,
    mounts: &[(String, PathBuf)],
) -> bool {
    let Some(previous) = previous else {
        return false;
    };
    let mut current_host_by_guest = std::collections::BTreeMap::<&str, &Path>::new();
    let mut current_guest_by_host: Vec<(&Path, &str)> = Vec::new();
    for (guest_path, host_path) in mounts {
        current_host_by_guest.insert(guest_path.as_str(), host_path.as_path());
        remember_current_guest(&mut current_guest_by_host, host_path.as_path(), guest_path);
    }
    let mut entries_by_host: Vec<(&Path, Vec<_>, bool)> = Vec::new();
    for binding in &previous.mounts {
        if let Some(mount) = staged
            .iter()
            .find(|mount| mount.guest_path == binding.guest_path)
        {
            store_case_insensitive_host_capture(
                &mut entries_by_host,
                binding.host_path.as_path(),
                mount.entries.clone(),
                mount.retained,
            );
        }
    }

    let before = staged.clone();
    let mut next = Vec::with_capacity(staged.len());
    for mount in staged.iter() {
        let guest = mount.guest_path.as_str();
        if let Some(current_host) = current_host_by_guest.get(guest) {
            if let Some((_, entries, retained)) = entries_by_host
                .iter()
                .find(|(host, _, _)| same_volume_host(host, current_host))
            {
                next.push(VolumePosixMount {
                    guest_path: mount.guest_path.clone(),
                    entries: entries.clone(),
                    retained: *retained,
                });
            } else if !previous
                .mounts
                .iter()
                .any(|binding| binding.guest_path == mount.guest_path)
            {
                // This boot's path has no previous owner. Keep the capture
                // that already names it. A path whose host changed still
                // drops that previous owner's entries.
                next.push(mount.clone());
            }
            continue;
        }
        let Some(binding) = previous
            .mounts
            .iter()
            .find(|binding| binding.guest_path == mount.guest_path)
        else {
            next.push(mount.clone());
            continue;
        };
        let Some(new_guest) = current_guest_by_host
            .iter()
            .find(|(host, _)| same_volume_host(host, &binding.host_path))
            .map(|(_, guest)| *guest)
        else {
            continue;
        };
        if staged
            .iter()
            .any(|existing| existing.guest_path == *new_guest)
        {
            continue;
        }
        next.push(VolumePosixMount {
            guest_path: (*new_guest).to_string(),
            entries: mount.entries.clone(),
            retained: mount.retained,
        });
    }
    if next == before {
        return false;
    }
    *staged = next;
    true
}

/// Guest paths from bindings that this capture omitted still cover nested entries.
fn include_binding_guest_paths(
    mounts: &mut Vec<VolumePosixMount>,
    bindings: &[VolumePosixBinding],
) {
    for binding in bindings {
        if mounts
            .iter()
            .any(|mount| mount.guest_path == binding.guest_path)
        {
            continue;
        }
        mounts.push(VolumePosixMount {
            guest_path: binding.guest_path.clone(),
            entries: Vec::new(),
            retained: false,
        });
    }
}

fn include_guest_paths(mounts: &mut Vec<VolumePosixMount>, guest_paths: &[String]) {
    for guest_path in guest_paths {
        if mounts.iter().any(|mount| mount.guest_path == *guest_path) {
            continue;
        }
        mounts.push(VolumePosixMount {
            guest_path: guest_path.clone(),
            entries: Vec::new(),
            retained: false,
        });
    }
}

/// Remove entries that must not be replayed, then drop mounts with nothing left.
///
/// An empty result does not count as a capture. The previous sidecar can
/// still apply, and harvest will not replace it with an empty file.
/// Bindings for a nested volume stay in the strip set when this capture
/// omitted that mount. Guest paths mounted this boot do too, including a
/// nested volume that has no binding yet.
fn discard_unusable_mounts(
    staged: &mut Vec<VolumePosixMount>,
    previous: Option<&VolumePosixBindings>,
    guest_paths: &[String],
) -> bool {
    let before = staged.clone();
    if let Some(previous) = previous {
        include_binding_guest_paths(staged, &previous.mounts);
    }
    include_guest_paths(staged, guest_paths);
    let _ = a3s_box_core::volume_posix::strip_entries_covered_by_nested_mounts(staged);
    staged.retain(|mount| !mount.entries.is_empty());
    *staged != before
}

/// Drop captures of a host volume whose guest paths disagree.
///
/// Fresh captures that disagree are removed, and a retained sibling of a
/// fresh capture stays. Retained captures that disagree with each other are
/// not collapsed into one side: every capture of that volume is removed so
/// the previous sidecar applies.
fn drop_disagreeing_captures(
    staged: &mut Vec<VolumePosixMount>,
    mounts: &[(String, PathBuf)],
) -> bool {
    let mut fresh: Vec<(&Path, Vec<_>)> = Vec::new();
    let mut retained: Vec<(&Path, Vec<_>)> = Vec::new();
    let mut fresh_conflicts: Vec<&Path> = Vec::new();
    let mut retained_conflicts: Vec<&Path> = Vec::new();
    for (guest_path, host_path) in mounts {
        let Some(mount) = staged.iter().find(|mount| mount.guest_path == *guest_path) else {
            continue;
        };
        let seen = if mount.retained {
            &mut retained
        } else {
            &mut fresh
        };
        let conflicts = if mount.retained {
            &mut retained_conflicts
        } else {
            &mut fresh_conflicts
        };
        if let Some((_, previous)) = seen
            .iter()
            .find(|(host, _)| same_volume_host(host, host_path))
        {
            if previous != &mount.entries
                && !conflicts
                    .iter()
                    .any(|host| same_volume_host(host, host_path))
            {
                conflicts.push(host_path.as_path());
            }
        } else {
            seen.push((host_path.as_path(), mount.entries.clone()));
        }
    }
    if fresh_conflicts.is_empty() && retained_conflicts.is_empty() {
        return false;
    }
    let before = staged.len();
    staged.retain(|mount| {
        let Some((_, host_path)) = mounts
            .iter()
            .find(|(guest_path, _)| guest_path == &mount.guest_path)
        else {
            return true;
        };
        if retained_conflicts
            .iter()
            .any(|host| same_volume_host(host, host_path))
        {
            return false;
        }
        mount.retained
            || !fresh_conflicts
                .iter()
                .any(|host| same_volume_host(host, host_path))
    });
    staged.len() != before
}

/// Keep `/workspace`, managed volumes, and this boot's other shares.
///
/// A guest path outside that set would chmod the container rootfs on replay.
fn drop_guest_paths_outside_this_boot(
    staged: &mut Vec<VolumePosixMount>,
    mounts: &[(String, PathBuf)],
    replay_mounts: &[(String, PathBuf)],
) -> bool {
    let before = staged.len();
    staged.retain(|mount| {
        mount.guest_path == "/workspace"
            || mounts
                .iter()
                .any(|(guest_path, _)| guest_path == &mount.guest_path)
            || replay_mounts
                .iter()
                .any(|(guest_path, _)| guest_path == &mount.guest_path)
    });
    staged.len() != before
}

/// Windows volume paths name one directory when only ASCII case differs.
/// A verbatim `\\?\` prefix or a `\\.\` drive prefix names that same
/// directory. `.`, a `..` that steps back through a normal component, and a
/// trailing dot or space on a non-verbatim name do too.
fn same_volume_host(left: &Path, right: &Path) -> bool {
    a3s_box_core::volume_posix::same_windows_directory(left, right)
}

/// Drop a nested share's files from every guest path of the same host volume.
///
/// `entry_covers_nested_share` only sees the entry's own guest path. Another
/// path of that host still names the same file, and replay would chmod the
/// nested share through it.
fn strip_shared_host_nested_entries(
    staged: &mut [VolumePosixMount],
    mounts: &[(String, PathBuf)],
) -> bool {
    let mut guest_paths: Vec<String> = staged
        .iter()
        .map(|mount| mount.guest_path.clone())
        .collect();
    for (guest_path, _) in mounts {
        if guest_paths.iter().any(|path| path == guest_path) {
            continue;
        }
        guest_paths.push(guest_path.clone());
    }
    let guest_refs: Vec<&str> = guest_paths.iter().map(String::as_str).collect();
    let mut changed = false;
    for mount in staged.iter_mut() {
        let Some((_, host_path)) = mounts
            .iter()
            .find(|(guest_path, _)| guest_path == &mount.guest_path)
        else {
            continue;
        };
        let aliases: Vec<&str> = mounts
            .iter()
            .filter(|(_, host)| same_volume_host(host, host_path))
            .map(|(guest_path, _)| guest_path.as_str())
            .collect();
        let before = mount.entries.len();
        mount.entries.retain(|entry| {
            !aliases.iter().any(|alias| {
                a3s_box_core::volume_posix::encoded_entry_covered_by_nested_share(
                    alias,
                    &entry.path_base64,
                    &guest_refs,
                )
            })
        });
        if mount.entries.len() != before {
            changed = true;
        }
    }
    changed
}

/// Every current guest path of one host volume uses that volume's entries.
fn fan_out_shared_host_entries(
    staged: &mut Vec<VolumePosixMount>,
    mounts: &[(String, PathBuf)],
) -> bool {
    let mut entries_by_host: Vec<(&Path, Vec<_>, bool)> = Vec::new();
    for (guest_path, host_path) in mounts {
        if let Some(mount) = staged.iter().find(|mount| mount.guest_path == *guest_path) {
            store_case_insensitive_host_capture(
                &mut entries_by_host,
                host_path.as_path(),
                mount.entries.clone(),
                mount.retained,
            );
        }
    }
    let mut changed = false;
    for (guest_path, host_path) in mounts {
        let Some((_, entries, retained)) = entries_by_host
            .iter()
            .find(|(host, _, _)| same_volume_host(host, host_path))
        else {
            continue;
        };
        if let Some(current) = staged
            .iter_mut()
            .find(|mount| mount.guest_path == *guest_path)
        {
            if current.entries != *entries || current.retained != *retained {
                current.entries = entries.clone();
                current.retained = *retained;
                changed = true;
            }
        } else {
            staged.push(VolumePosixMount {
                guest_path: guest_path.clone(),
                entries: entries.clone(),
                retained: *retained,
            });
            changed = true;
        }
    }
    changed
}

fn store_case_insensitive_host_capture<'a, T>(
    map: &mut Vec<(&'a Path, Vec<T>, bool)>,
    host: &'a Path,
    entries: Vec<T>,
    retained: bool,
) {
    if let Some((_, existing, existing_retained)) = map
        .iter_mut()
        .find(|(key, _, _)| same_volume_host(key, host))
    {
        if *existing_retained && !retained {
            return;
        }
        *existing = entries;
        *existing_retained = retained;
        return;
    }
    map.push((host, entries, retained));
}

fn remember_current_guest<'a>(map: &mut Vec<(&'a Path, &'a str)>, host: &'a Path, guest: &'a str) {
    if let Some((_, existing)) = map.iter_mut().find(|(key, _)| same_volume_host(key, host)) {
        *existing = guest;
        return;
    }
    map.push((host, guest));
}

fn publish_manifest(destination: &Path, manifest: VolumePosixManifest) -> Result<(), BoxError> {
    manifest.validate().map_err(BoxError::ConfigError)?;
    let bytes = serde_json::to_vec(&manifest).map_err(|error| {
        BoxError::IoError(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    })?;
    if bytes.len() as u64 > VOLUME_POSIX_METADATA_MAX_BYTES {
        return Err(BoxError::ConfigError(
            "volume posix metadata exceeds the size limit".to_string(),
        ));
    }
    if let Some(parent) = destination.parent() {
        #[cfg(windows)]
        {
            let mut prefix = PathBuf::new();
            for component in parent.components() {
                prefix.push(component);
                crate::vm::refuse_directory_reparse(&prefix)?;
            }
        }
        std::fs::create_dir_all(parent).map_err(BoxError::IoError)?;
    }
    let tmp = destination.with_file_name(VOLUME_POSIX_METADATA_TEMP_FILE);
    let publish = destination.with_file_name(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
    // Either file may be the capture whose rename did not finish. Do not
    // truncate that capture until the new manifest is in place. The pending
    // scratch is only the write buffer when the publish file is that capture.
    let preserve_temp = durable_temp_is_the_remaining_capture(destination, &tmp)?;
    let preserve_publish = durable_temp_is_the_remaining_capture(destination, &publish)?;
    let pending = destination.with_file_name(VOLUME_POSIX_METADATA_PENDING_TEMP_FILE);
    let preferred = if preserve_publish {
        pending.clone()
    } else if preserve_temp {
        publish.clone()
    } else {
        tmp.clone()
    };
    let mut candidates = vec![preferred];
    for extra in [tmp.clone(), publish.clone(), pending] {
        if preserve_temp && extra == tmp {
            continue;
        }
        if preserve_publish && extra == publish {
            continue;
        }
        if !candidates.iter().any(|candidate| candidate == &extra) {
            candidates.push(extra);
        }
    }
    let mut chosen = None;
    for candidate in &candidates {
        if prepare_bindings_write(candidate)? {
            chosen = Some(candidate.clone());
            break;
        }
    }
    let Some(mut scratch) = chosen else {
        return Err(BoxError::IoError(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "volume posix metadata scratch is not a writable file",
        )));
    };
    if nonempty_directory(destination)? {
        let target = if !preserve_publish {
            publish.clone()
        } else if !preserve_temp {
            tmp.clone()
        } else {
            return Err(BoxError::IoError(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "volume posix metadata destination is not a writable file",
            )));
        };
        if scratch == target {
            let pending = destination.with_file_name(VOLUME_POSIX_METADATA_PENDING_TEMP_FILE);
            if !prepare_bindings_write(&pending)? || nonempty_directory(&pending)? {
                return Err(BoxError::IoError(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "volume posix metadata destination is not a writable file",
                )));
            }
            scratch = pending;
        }
        if nonempty_directory(&target)? {
            return Err(BoxError::IoError(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "volume posix metadata destination is not a writable file",
            )));
        }
        if empty_real_directory(&target)? {
            std::fs::remove_dir(&target).map_err(BoxError::IoError)?;
        }
        a3s_box_core::fs_atomic::write_durable(&scratch, &target, &bytes)
            .map_err(BoxError::IoError)?;
        return Ok(());
    }
    if empty_real_directory(destination)? {
        std::fs::remove_dir(destination).map_err(BoxError::IoError)?;
    }
    reject_planted_file(destination)?;
    a3s_box_core::fs_atomic::write_durable(&scratch, destination, &bytes)
        .map_err(BoxError::IoError)?;
    if preserve_temp {
        remove_regular_file(&tmp)?;
    }
    if preserve_publish {
        remove_regular_file(&publish)?;
    }
    Ok(())
}

fn durable_temp_is_the_remaining_capture(
    destination: &Path,
    temp: &Path,
) -> Result<bool, BoxError> {
    let temp_manifest = match read_manifest_file(temp) {
        Ok(manifest) => manifest,
        Err(error) if is_not_regular_metadata(&error) || is_metadata_size_limit(&error) => {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    if temp_manifest.is_none() {
        return Ok(false);
    }
    let temp_mtime = file_mtime(temp).unwrap_or(SystemTime::UNIX_EPOCH);
    match read_manifest_file(destination) {
        Ok(Some(_)) => Ok(temp_mtime > file_mtime(destination).unwrap_or(SystemTime::UNIX_EPOCH)),
        Ok(None) => Ok(true),
        Err(error) if is_not_regular_metadata(&error) || is_metadata_size_limit(&error) => Ok(true),
        Err(error) => Err(error),
    }
}

fn read_manifest_file(path: &Path) -> Result<Option<VolumePosixManifest>, BoxError> {
    let bytes = match read_bounded_regular_file(path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(None),
        Err(error) if is_scratch_capture(path) && is_metadata_size_limit(&error) => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let manifest: VolumePosixManifest = match serde_json::from_slice(&bytes) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(None),
    };
    if manifest.validate().is_err() {
        return Ok(None);
    }
    Ok(Some(manifest))
}

fn sidecar_is_planted(volume_dir: &Path) -> Result<bool, BoxError> {
    let Some(path) = a3s_box_core::volume_posix::volume_posix_sidecar_path(volume_dir) else {
        return Ok(false);
    };
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if is_regular_file(&metadata) => Ok(false),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(BoxError::IoError(error)),
    }
}

fn sidecar_mtime(volume_dir: &Path) -> Result<Option<SystemTime>, BoxError> {
    let Some(path) = a3s_box_core::volume_posix::volume_posix_sidecar_path(volume_dir) else {
        return Ok(None);
    };
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if is_regular_file(&metadata) => {
            // An oversized sidecar is not a capture. Its mtime must not block
            // harvest from writing the manifest that is still readable.
            if metadata.len() > VOLUME_POSIX_METADATA_MAX_BYTES as u64 {
                return Ok(None);
            }
            Ok(file_mtime_of(&metadata))
        }
        // A directory or link is not a sidecar. Do not follow it, and do not
        // fail boot while a rootfs manifest is still readable.
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(BoxError::IoError(error)),
    }
}

fn is_sidecar_size_limit(error: &BoxError) -> bool {
    match error {
        BoxError::IoError(inner) => inner.to_string().contains("exceeds the size limit"),
        _ => false,
    }
}

fn read_sidecar(volume_dir: &Path) -> Result<Option<VolumePosixSidecar>, BoxError> {
    match read_volume_posix_sidecar(volume_dir) {
        Ok(sidecar) => Ok(sidecar),
        Err(error)
            if error.kind() == std::io::ErrorKind::InvalidData
                && error.to_string().contains("not a regular file") =>
        {
            Ok(None)
        }
        Err(error) => Err(BoxError::IoError(error)),
    }
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::symlink_metadata(path)
        .ok()
        .as_ref()
        .and_then(file_mtime_of)
}

fn file_mtime_of(metadata: &std::fs::Metadata) -> Option<SystemTime> {
    metadata.modified().ok()
}

fn read_bindings(box_dir: &Path) -> Result<Option<VolumePosixBindings>, BoxError> {
    let path = box_dir.join(VOLUME_POSIX_BINDINGS_FILE);
    let bytes = match read_bounded_regular_file(&path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(None),
        // A directory, link, or oversized file is not the previous mount map.
        // Do not read it, and do not fail boot while the rootfs manifest is
        // still readable. This boot rewrites a regular oversized file from
        // the mounts it was given.
        Err(error) if is_not_regular_metadata(&error) || is_metadata_size_limit(&error) => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let Ok(bindings) = serde_json::from_slice::<VolumePosixBindings>(&bytes) else {
        // Bytes that are not a mount map are not the previous boot. This boot
        // rewrites the regular file from the mounts it was given.
        return Ok(None);
    };
    if bindings.validate().is_err() {
        return Ok(None);
    }
    Ok(Some(bindings))
}

fn read_guest_manifest(
    box_dir: &Path,
) -> Result<Option<(SystemTime, VolumePosixManifest)>, BoxError> {
    let mut newest: Option<(SystemTime, VolumePosixManifest)> = None;
    let mut saw_invalid = false;
    let mut blocking_error: Option<BoxError> = None;
    for root_name in ["merged", "rootfs", "upper"] {
        for file_name in [
            VOLUME_POSIX_METADATA_FILE,
            VOLUME_POSIX_METADATA_TEMP_FILE,
            VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE,
        ] {
            let path = box_dir.join(root_name).join(file_name);
            let bytes = match read_bounded_regular_file(&path) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => continue,
                // A temp or publish scratch that grew past the limit, or a
                // directory at a capture name, is not a capture. One root
                // does not hide a valid manifest in another. The original
                // error remains when nothing valid is left. The bytes and
                // the directory are not followed.
                Err(error) if is_metadata_size_limit(&error) || is_not_regular_metadata(&error) => {
                    if file_name == VOLUME_POSIX_METADATA_FILE && blocking_error.is_none() {
                        blocking_error = Some(error);
                        saw_invalid = true;
                    }
                    continue;
                }
                Err(error) => return Err(error),
            };
            let manifest = match serde_json::from_slice::<VolumePosixManifest>(&bytes) {
                Ok(manifest) if manifest.validate().is_ok() => manifest,
                _ => {
                    if file_name == VOLUME_POSIX_METADATA_FILE {
                        saw_invalid = true;
                    }
                    continue;
                }
            };
            let modified = file_mtime(&path).unwrap_or(SystemTime::UNIX_EPOCH);
            let replace = newest
                .as_ref()
                .map(|(time, _)| modified > *time)
                .unwrap_or(true);
            if replace {
                newest = Some((modified, manifest));
            }
        }
    }
    if newest.is_none() {
        if let Some(error) = blocking_error {
            return Err(error);
        }
    }
    if newest.is_none() && saw_invalid {
        return Err(BoxError::ConfigError(
            "volume posix metadata is not a valid manifest".to_string(),
        ));
    }
    Ok(newest)
}

fn guest_root_manifest_blocks_replay(path: &Path) -> Result<bool, BoxError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if is_regular_file(&metadata) => {
            Ok(metadata.len() > VOLUME_POSIX_METADATA_MAX_BYTES)
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(BoxError::IoError(error)),
    }
}

fn is_not_regular_metadata(error: &BoxError) -> bool {
    match error {
        BoxError::IoError(inner) => {
            inner.kind() == std::io::ErrorKind::InvalidData
                && inner.to_string().contains("is not a regular file")
        }
        _ => false,
    }
}

fn is_reparse_or_symlink(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x0000_0400 != 0 {
            return true;
        }
    }
    false
}

/// Remove a capture node. A directory at the capture name is removed.
/// A symlink or reparse point is unlinked and not followed.
fn remove_metadata_node(path: &Path) -> Result<(), BoxError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if is_regular_file(&metadata) => {
            std::fs::remove_file(path).map_err(BoxError::IoError)
        }
        Ok(metadata) if is_reparse_or_symlink(&metadata) => {
            let result = if metadata.is_dir() {
                std::fs::remove_dir(path)
            } else {
                std::fs::remove_file(path)
            };
            result.map_err(BoxError::IoError)
        }
        Ok(metadata) if metadata.is_dir() => {
            std::fs::remove_dir_all(path).map_err(BoxError::IoError)
        }
        Ok(_) => Err(BoxError::IoError(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not a regular file", path.display()),
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(BoxError::IoError(error)),
    }
}

fn is_scratch_capture(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        name == VOLUME_POSIX_METADATA_TEMP_FILE || name == VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE
    })
}

fn is_metadata_size_limit(error: &BoxError) -> bool {
    matches!(error, BoxError::ConfigError(message) if message.contains("exceeds the volume posix metadata size limit"))
}

fn read_bounded_regular_file(path: &Path) -> Result<Option<Vec<u8>>, BoxError> {
    #[cfg(windows)]
    {
        let mut current = PathBuf::new();
        for component in path.components() {
            current.push(component);
            crate::vm::refuse_directory_reparse(&current)?;
        }
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) if is_regular_file(&metadata) => metadata,
        Ok(_) => {
            return Err(BoxError::IoError(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} is not a regular file", path.display()),
            )))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(BoxError::IoError(error)),
    };
    if metadata.len() > VOLUME_POSIX_METADATA_MAX_BYTES {
        return Err(BoxError::ConfigError(format!(
            "{} exceeds the volume posix metadata size limit",
            path.display()
        )));
    }
    std::fs::read(path).map(Some).map_err(BoxError::IoError)
}

fn regular_file_exists(path: &Path) -> Result<bool, BoxError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if is_regular_file(&metadata) => Ok(true),
        Ok(_) => Err(BoxError::IoError(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not a regular file", path.display()),
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(BoxError::IoError(error)),
    }
}

fn remove_unless_nonempty_directory(path: &Path) -> Result<(), BoxError> {
    #[cfg(windows)]
    {
        let mut current = PathBuf::new();
        for component in path.components() {
            current.push(component);
            crate::vm::refuse_directory_reparse(&current)?;
        }
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !is_reparse_or_symlink(&metadata) => {
            let mut entries = std::fs::read_dir(path).map_err(BoxError::IoError)?;
            if entries
                .next()
                .transpose()
                .map_err(BoxError::IoError)?
                .is_some()
            {
                return Ok(());
            }
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(BoxError::IoError(error)),
    }
    remove_metadata_node(path)
}

fn prepare_bindings_write(path: &Path) -> Result<bool, BoxError> {
    #[cfg(windows)]
    {
        let mut current = PathBuf::new();
        for component in path.components() {
            current.push(component);
            crate::vm::refuse_directory_reparse(&current)?;
        }
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if is_regular_file(&metadata) => Ok(true),
        Ok(metadata) if is_reparse_or_symlink(&metadata) => {
            if metadata.is_dir() {
                std::fs::remove_dir(path).map_err(BoxError::IoError)?;
            } else {
                std::fs::remove_file(path).map_err(BoxError::IoError)?;
            }
            Ok(true)
        }
        Ok(metadata) if metadata.is_dir() => {
            let mut entries = std::fs::read_dir(path).map_err(BoxError::IoError)?;
            if entries
                .next()
                .transpose()
                .map_err(BoxError::IoError)?
                .is_some()
            {
                return Ok(false);
            }
            std::fs::remove_dir(path).map_err(BoxError::IoError)?;
            Ok(true)
        }
        Ok(_) => Err(BoxError::IoError(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not a regular file", path.display()),
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(BoxError::IoError(error)),
    }
}

fn clear_bindings_when_unmounted(path: &Path) -> Result<(), BoxError> {
    if !prepare_bindings_write(path)? {
        return Ok(());
    }
    remove_regular_file(path)
}

fn reject_planted_file(path: &Path) -> Result<(), BoxError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if is_regular_file(&metadata) => Ok(()),
        Ok(_) => Err(BoxError::IoError(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not a regular file", path.display()),
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(BoxError::IoError(error)),
    }
}

fn remove_regular_file(path: &Path) -> Result<(), BoxError> {
    #[cfg(windows)]
    {
        let mut current = PathBuf::new();
        for component in path.components() {
            current.push(component);
            crate::vm::refuse_directory_reparse(&current)?;
        }
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if is_regular_file(&metadata) => {
            std::fs::remove_file(path).map_err(BoxError::IoError)
        }
        Ok(_) => Err(BoxError::IoError(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not a regular file", path.display()),
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(BoxError::IoError(error)),
    }
}

fn is_regular_file(metadata: &std::fs::Metadata) -> bool {
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x0000_0400 != 0 {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::rootfs_metadata::{RootfsEntryKind, RootfsMetadataEntry};
    use a3s_box_core::volume_posix::{
        volume_posix_sidecar_path, write_volume_posix_sidecar, VolumePosixSidecar,
        VOLUME_POSIX_METADATA_SCHEMA,
    };

    #[cfg(windows)]
    #[test]
    fn sync_does_not_create_a_box_directory_through_an_ancestor_junction() {
        use std::os::windows::process::CommandExt;

        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"keep").unwrap();
        let source = tmp.path().join("volume");
        std::fs::create_dir_all(&source).unwrap();
        let rootfs = tmp.path().join("guest");
        std::fs::create_dir_all(&rootfs).unwrap();
        let parent = tmp.path().join("parent");
        std::fs::create_dir_all(&parent).unwrap();
        let link = parent.join("link");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());

        let synced = sync_volume_posix(
            &link.join("box"),
            &rootfs,
            &[("/data".to_string(), source)],
            &[],
        );
        assert!(
            synced.is_err(),
            "sync followed an ancestor junction: {synced:?}"
        );
        assert!(
            !outside.join("box").exists(),
            "box directory was created through the junction"
        );
        assert_eq!(std::fs::read(outside.join("secret.txt")).unwrap(), b"keep");
    }

    #[cfg(windows)]
    #[test]
    fn read_bindings_does_not_read_through_an_ancestor_junction() {
        use std::os::windows::process::CommandExt;

        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        let box_dir = outside.join("box");
        std::fs::create_dir_all(&box_dir).unwrap();
        let bindings = VolumePosixBindings::new(vec![VolumePosixBinding {
            guest_path: "/secret-bind".to_string(),
            host_path: outside.join("volume"),
        }]);
        std::fs::write(
            box_dir.join(VOLUME_POSIX_BINDINGS_FILE),
            serde_json::to_vec(&bindings).unwrap(),
        )
        .unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret-bindings").unwrap();
        let parent = tmp.path().join("parent");
        std::fs::create_dir_all(&parent).unwrap();
        let link = parent.join("link");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());

        let read = read_bindings(&link.join("box"));
        let error = match read {
            Ok(found) => panic!("volume bindings followed an ancestor junction: {found:?}"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains("junction"),
            "volume bindings error did not name the junction: {error}"
        );
        assert_eq!(
            std::fs::read(outside.join("secret.txt")).unwrap(),
            b"secret-bindings"
        );
    }

    #[cfg(windows)]
    #[test]
    fn remove_unless_nonempty_directory_does_not_delete_through_an_ancestor_junction() {
        use std::os::windows::process::CommandExt;

        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let secret = outside.join("secret.txt");
        std::fs::write(&secret, b"secret-remove").unwrap();
        let parent = tmp.path().join("parent");
        std::fs::create_dir_all(&parent).unwrap();
        let link = parent.join("link");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());

        let removed = remove_unless_nonempty_directory(&link.join("secret.txt"));
        let error = match removed {
            Ok(()) => panic!("metadata removal followed an ancestor junction"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains("junction"),
            "metadata removal error did not name the junction: {error}"
        );
        assert_eq!(std::fs::read(&secret).unwrap(), b"secret-remove");
    }

    #[test]
    fn remove_unless_nonempty_directory_removes_a_real_file() {
        let tmp = tempfile::tempdir().unwrap();
        let secret = tmp.path().join("secret.txt");
        std::fs::write(&secret, b"remove-me").unwrap();
        remove_unless_nonempty_directory(&secret).expect("real metadata file is removable");
        assert!(!secret.exists());
    }

    #[cfg(windows)]
    #[test]
    fn remove_regular_file_does_not_delete_through_an_ancestor_junction() {
        use std::os::windows::process::CommandExt;

        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let secret = outside.join("secret.txt");
        std::fs::write(&secret, b"secret-regular").unwrap();
        let parent = tmp.path().join("parent");
        std::fs::create_dir_all(&parent).unwrap();
        let link = parent.join("link");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());

        let removed = remove_regular_file(&link.join("secret.txt"));
        let error = match removed {
            Ok(()) => panic!("regular file removal followed an ancestor junction"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains("junction"),
            "regular file removal error did not name the junction: {error}"
        );
        assert_eq!(std::fs::read(&secret).unwrap(), b"secret-regular");
    }

    #[test]
    fn remove_regular_file_removes_a_real_file() {
        let tmp = tempfile::tempdir().unwrap();
        let secret = tmp.path().join("secret.txt");
        std::fs::write(&secret, b"remove-me").unwrap();
        remove_regular_file(&secret).expect("real regular file is removable");
        assert!(!secret.exists());
    }

    #[cfg(windows)]
    #[test]
    fn prepare_bindings_write_does_not_delete_an_empty_directory_through_an_ancestor_junction() {
        use std::os::windows::process::CommandExt;

        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        let empty = outside.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret-bindings-write").unwrap();
        let parent = tmp.path().join("parent");
        std::fs::create_dir_all(&parent).unwrap();
        let link = parent.join("link");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());

        let prepared = prepare_bindings_write(&link.join("empty"));
        let error = match prepared {
            Ok(ready) => panic!("bindings write followed an ancestor junction: {ready}"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains("junction"),
            "bindings write error did not name the junction: {error}"
        );
        assert!(
            empty.is_dir(),
            "empty directory was deleted through the junction"
        );
        assert_eq!(
            std::fs::read(outside.join("secret.txt")).unwrap(),
            b"secret-bindings-write"
        );
    }

    #[test]
    fn prepare_bindings_write_removes_a_real_empty_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let empty = tmp.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert!(prepare_bindings_write(&empty).expect("real empty directory"));
        assert!(!empty.exists());
    }

    #[cfg(windows)]
    #[test]
    fn sync_creates_a_missing_box_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("volume");
        std::fs::create_dir_all(&source).unwrap();
        let rootfs = tmp.path().join("guest");
        std::fs::create_dir_all(&rootfs).unwrap();
        let box_dir = tmp.path().join("box");
        sync_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), source)], &[])
            .expect("sync creates a real box directory");
        assert!(box_dir.is_dir());
    }

    #[cfg(windows)]
    #[test]
    fn publish_does_not_create_a_parent_through_an_ancestor_junction() {
        use std::os::windows::process::CommandExt;

        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"keep").unwrap();
        let parent = tmp.path().join("parent");
        std::fs::create_dir_all(&parent).unwrap();
        let link = parent.join("link");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());

        let published = publish_manifest(
            &link.join("guest").join(VOLUME_POSIX_METADATA_FILE),
            VolumePosixManifest::new(Vec::new()),
        );
        assert!(
            published.is_err(),
            "publish followed an ancestor junction: {published:?}"
        );
        assert!(
            !outside.join("guest").exists(),
            "manifest parent was created through the junction"
        );
        assert_eq!(std::fs::read(outside.join("secret.txt")).unwrap(), b"keep");
    }

    #[test]
    fn publish_creates_a_missing_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let destination = tmp.path().join("guest").join(VOLUME_POSIX_METADATA_FILE);
        publish_manifest(&destination, VolumePosixManifest::new(Vec::new()))
            .expect("publish creates a real parent");
        assert!(destination.is_file());
    }

    fn entry(mode: u32) -> RootfsMetadataEntry {
        RootfsMetadataEntry {
            path_base64: "Lg==".to_string(),
            kind: RootfsEntryKind::Directory,
            mode,
            uid: 1000,
            gid: 1000,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        }
    }

    #[test]
    fn volume_posix_sidecar_survives_rootfs_deletion_and_guest_path_change() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        let bind = home.path().join("caller-bind");
        std::fs::create_dir_all(&volume).unwrap();
        std::fs::create_dir_all(&bind).unwrap();

        let old_box = home.path().join("boxes").join("old");
        let old_rootfs = old_box.join("rootfs");
        std::fs::create_dir_all(&old_rootfs).unwrap();
        let mounts = vec![
            ("/data".to_string(), volume.clone()),
            ("/workspace".to_string(), bind.clone()),
        ];
        sync_managed_volume_posix(&old_box, &old_rootfs, &mounts).unwrap();

        let manifest = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o750)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
            ],
        };
        std::fs::write(
            old_rootfs.join(VOLUME_POSIX_METADATA_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        harvest_volume_posix_sidecars(&old_box, &volumes).unwrap();

        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        assert_eq!(sidecar.parent(), Some(volumes.as_path()));
        assert!(sidecar.is_file());
        let bind_sidecar = volume_posix_sidecar_path(&bind).unwrap();
        assert!(!bind_sidecar.exists());
        std::fs::remove_dir_all(&old_box).unwrap();
        assert!(sidecar.is_file());

        let new_box = home.path().join("boxes").join("new");
        let new_rootfs = new_box.join("rootfs");
        std::fs::create_dir_all(&new_rootfs).unwrap();
        let remounted = vec![("/var/lib/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&new_box, &new_rootfs, &remounted).unwrap();
        let staged: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(new_rootfs.join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(staged.mounts.len(), 1);
        assert_eq!(staged.mounts[0].guest_path, "/var/lib/data");
        assert_eq!(staged.mounts[0].entries[0].mode, 0o750);

        let manifest_path = new_rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&manifest_path, b"{\"schema\":\"stale\"}").unwrap();
        assert!(sync_managed_volume_posix(&new_box, &new_rootfs, &remounted).is_err());
        assert_eq!(
            std::fs::read(&manifest_path).unwrap(),
            b"{\"schema\":\"stale\"}"
        );
    }

    #[test]
    fn newer_volume_posix_sidecar_replaces_the_rootfs_mount() {
        let home = tempfile::tempdir().unwrap();
        let volume = home.path().join("volumes").join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let older = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&older).unwrap()).unwrap();
        let older_mtime = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        let newer_mtime = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(20);
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(older_mtime)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&sidecar)
            .unwrap()
            .set_modified(newer_mtime)
            .unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), volume.clone())])
            .unwrap();
        let staged: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(staged.mounts.len(), 2);
        assert_eq!(staged.mounts[0].guest_path, "/data");
        assert_eq!(staged.mounts[0].entries[0].mode, 0o640);
        assert_eq!(staged.mounts[1].guest_path, "/workspace");
        assert_eq!(staged.mounts[1].entries[0].mode, 0o700);

        std::fs::File::options()
            .write(true)
            .open(&sidecar)
            .unwrap()
            .set_modified(older_mtime)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(newer_mtime)
            .unwrap();
        let before = std::fs::read(&manifest_path).unwrap();
        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), volume)]).unwrap();
        assert_eq!(std::fs::read(&manifest_path).unwrap(), before);
    }

    #[test]
    fn stage_keeps_a_caller_path_after_replay_retires_the_manifest() {
        let home = tempfile::tempdir().unwrap();
        let volume = home.path().join("volumes").join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let older = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&older).unwrap()).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10))
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&sidecar)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(20))
            .unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), volume)]).unwrap();
        let published = rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
        let newer_than_manifest = |path: &std::path::Path| {
            path.is_file()
                && path
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    > manifest_path
                        .metadata()
                        .and_then(|metadata| metadata.modified())
                        .ok()
        };
        if !newer_than_manifest(&rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE)) {
            let _ = std::fs::remove_file(rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE));
        }
        if !newer_than_manifest(&published) {
            let _ = std::fs::remove_file(&published);
        }
        let _ = std::fs::remove_file(&manifest_path);

        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&published).unwrap()).unwrap();
        assert_eq!(
            kept.mounts
                .iter()
                .find(|mount| mount.guest_path == "/data")
                .unwrap()
                .entries[0]
                .mode,
            0o640
        );
        assert_eq!(
            kept.mounts
                .iter()
                .find(|mount| mount.guest_path == "/workspace")
                .unwrap()
                .entries[0]
                .mode,
            0o700
        );
    }

    #[test]
    fn moved_volume_guest_path_is_not_replayed_on_the_rootfs() {
        let home = tempfile::tempdir().unwrap();
        let volume = home.path().join("volumes").join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&captured).unwrap()).unwrap();
        let older_mtime = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        let newer_mtime = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(20);
        std::fs::File::options()
            .write(true)
            .open(&sidecar)
            .unwrap()
            .set_modified(older_mtime)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(newer_mtime)
            .unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), volume.clone())])
            .unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[("/var/lib/data".to_string(), volume.clone())],
        )
        .unwrap();
        let moved: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(moved.mounts.len(), 2);
        assert_eq!(moved.mounts[0].guest_path, "/var/lib/data");
        assert_eq!(moved.mounts[0].entries[0].mode, 0o755);
        assert_eq!(moved.mounts[1].guest_path, "/workspace");
        assert!(moved.mounts.iter().all(|mount| mount.guest_path != "/data"));

        sync_managed_volume_posix(&box_dir, &rootfs, &[]).unwrap();
        let published = rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
        let capture_path = if published.is_file()
            && published
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                > manifest_path
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .ok()
        {
            published
        } else {
            manifest_path.clone()
        };
        let remaining: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
        assert_eq!(remaining.mounts.len(), 1);
        assert_eq!(remaining.mounts[0].guest_path, "/workspace");
        assert!(!box_dir.join(VOLUME_POSIX_BINDINGS_FILE).exists());
    }

    #[test]
    fn moved_volume_keeps_entries_when_host_path_differs_by_case() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        let alias = volumes.join("Data");
        assert_ne!(volume, alias);
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&captured).unwrap()).unwrap();
        let older_mtime = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        let newer_mtime = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(20);
        std::fs::File::options()
            .write(true)
            .open(&sidecar)
            .unwrap()
            .set_modified(older_mtime)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(newer_mtime)
            .unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), volume)]).unwrap();
        sync_managed_volume_posix(&box_dir, &rootfs, &[("/var/lib/data".to_string(), alias)])
            .unwrap();
        let moved: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(moved.mounts.len(), 2);
        assert_eq!(moved.mounts[0].guest_path, "/var/lib/data");
        assert_eq!(moved.mounts[0].entries[0].mode, 0o755);
        assert_eq!(moved.mounts[1].guest_path, "/workspace");
        assert!(moved.mounts.iter().all(|mount| mount.guest_path != "/data"));
    }

    #[test]
    fn different_volume_at_the_same_guest_path_keeps_its_own_sidecar() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let previous = volumes.join("previous");
        let replacement = volumes.join("replacement");
        std::fs::create_dir_all(&previous).unwrap();
        std::fs::create_dir_all(&replacement).unwrap();
        write_volume_posix_sidecar(&replacement, &VolumePosixSidecar::new(vec![entry(0o640)]))
            .unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[("/data".to_string(), previous.clone())],
        )
        .unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&captured).unwrap()).unwrap();
        let older = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        let newer = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(20);
        let sidecar = volume_posix_sidecar_path(&replacement).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&sidecar)
            .unwrap()
            .set_modified(older)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(newer)
            .unwrap();

        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[("/data".to_string(), replacement.clone())],
        )
        .unwrap();
        let staged: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        let data = staged
            .mounts
            .iter()
            .find(|mount| mount.guest_path == "/data")
            .unwrap();
        assert_eq!(data.entries[0].mode, 0o640);
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/workspace")
                .unwrap()
                .entries[0]
                .mode,
            0o700
        );
    }

    #[test]
    fn swapped_volume_guest_paths_follow_the_host_volume() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let alpha = volumes.join("alpha");
        let beta = volumes.join("beta");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/a".to_string(), alpha.clone()),
                ("/b".to_string(), beta.clone()),
            ],
        )
        .unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/a".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/b".to_string(),
                    entries: vec![entry(0o640)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&captured).unwrap()).unwrap();

        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/b".to_string(), alpha.clone()),
                ("/a".to_string(), beta.clone()),
            ],
        )
        .unwrap();
        let staged: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/b")
                .unwrap()
                .entries[0]
                .mode,
            0o755
        );
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/a")
                .unwrap()
                .entries[0]
                .mode,
            0o640
        );
    }

    #[test]
    fn same_host_volume_at_two_guest_paths_shares_entries() {
        let home = tempfile::tempdir().unwrap();
        let volume = home.path().join("volumes").join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), volume.clone())])
            .unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&captured).unwrap()).unwrap();
        let older = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        let newer = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(20);
        std::fs::File::options()
            .write(true)
            .open(&sidecar)
            .unwrap()
            .set_modified(older)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(newer)
            .unwrap();

        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/data".to_string(), volume.clone()),
                ("/mnt/data".to_string(), volume),
            ],
        )
        .unwrap();
        let staged: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        for guest_path in ["/data", "/mnt/data"] {
            assert_eq!(
                staged
                    .mounts
                    .iter()
                    .find(|mount| mount.guest_path == guest_path)
                    .unwrap()
                    .entries[0]
                    .mode,
                0o755,
                "{guest_path}"
            );
        }
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/workspace")
                .unwrap()
                .entries[0]
                .mode,
            0o700
        );
    }

    #[test]
    fn same_host_volume_shares_entries_when_paths_differ_by_case() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        let alias = volumes.join("Data");
        assert_ne!(volume, alias);
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), volume.clone())])
            .unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&captured).unwrap()).unwrap();
        let older = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        let newer = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(20);
        std::fs::File::options()
            .write(true)
            .open(&sidecar)
            .unwrap()
            .set_modified(older)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(newer)
            .unwrap();

        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/data".to_string(), volume),
                ("/mnt/data".to_string(), alias),
            ],
        )
        .unwrap();
        let staged: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        for guest_path in ["/data", "/mnt/data"] {
            assert_eq!(
                staged
                    .mounts
                    .iter()
                    .find(|mount| mount.guest_path == guest_path)
                    .unwrap()
                    .entries[0]
                    .mode,
                0o755,
                "{guest_path}"
            );
        }
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/workspace")
                .unwrap()
                .entries[0]
                .mode,
            0o700
        );
    }

    #[test]
    fn same_directory_keeps_entries_when_host_path_is_verbatim() {
        let home = tempfile::tempdir().unwrap();
        let volume = home.path().join("volumes").join("data");
        std::fs::create_dir_all(&volume).unwrap();
        let verbatim = std::fs::canonicalize(&volume).unwrap();
        assert_ne!(volume, verbatim);
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&captured).unwrap()).unwrap();
        let older = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        let newer = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(20);
        std::fs::File::options()
            .write(true)
            .open(&sidecar)
            .unwrap()
            .set_modified(older)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(newer)
            .unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), volume)]).unwrap();
        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), verbatim)]).unwrap();
        let staged: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/data")
                .unwrap()
                .entries[0]
                .mode,
            0o755
        );
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/workspace")
                .unwrap()
                .entries[0]
                .mode,
            0o700
        );
    }

    #[test]
    fn same_directory_keeps_entries_when_host_path_has_parent_segment() {
        let home = tempfile::tempdir().unwrap();
        let volume = home.path().join("volumes").join("data");
        std::fs::create_dir_all(&volume).unwrap();
        let alias = volume.join("..").join("data");
        assert_ne!(volume, alias);
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&captured).unwrap()).unwrap();
        let older = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        let newer = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(20);
        std::fs::File::options()
            .write(true)
            .open(&sidecar)
            .unwrap()
            .set_modified(older)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(newer)
            .unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), volume)]).unwrap();
        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), alias)]).unwrap();
        let staged: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/data")
                .unwrap()
                .entries[0]
                .mode,
            0o755
        );
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/workspace")
                .unwrap()
                .entries[0]
                .mode,
            0o700
        );
    }

    #[test]
    fn same_directory_keeps_entries_when_host_path_has_trailing_dot() {
        let home = tempfile::tempdir().unwrap();
        let volume = home.path().join("volumes").join("data");
        std::fs::create_dir_all(&volume).unwrap();
        let alias = volume.parent().unwrap().join("data.");
        assert_ne!(volume, alias);
        assert_eq!(
            std::fs::canonicalize(&volume).unwrap(),
            std::fs::canonicalize(&alias).unwrap()
        );
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&captured).unwrap()).unwrap();
        let older = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        let newer = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(20);
        std::fs::File::options()
            .write(true)
            .open(&sidecar)
            .unwrap()
            .set_modified(older)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(newer)
            .unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), volume)]).unwrap();
        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), alias)]).unwrap();
        let staged: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/data")
                .unwrap()
                .entries[0]
                .mode,
            0o755
        );
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/workspace")
                .unwrap()
                .entries[0]
                .mode,
            0o700
        );
    }

    #[test]
    fn same_directory_keeps_entries_when_host_path_has_trailing_space() {
        let home = tempfile::tempdir().unwrap();
        let volume = home.path().join("volumes").join("data");
        std::fs::create_dir_all(&volume).unwrap();
        let alias = volume.parent().unwrap().join("data ");
        assert_ne!(volume, alias);
        assert_eq!(
            std::fs::canonicalize(&volume).unwrap(),
            std::fs::canonicalize(&alias).unwrap()
        );
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&captured).unwrap()).unwrap();
        let older = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        let newer = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(20);
        std::fs::File::options()
            .write(true)
            .open(&sidecar)
            .unwrap()
            .set_modified(older)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(newer)
            .unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), volume)]).unwrap();
        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), alias)]).unwrap();
        let staged: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/data")
                .unwrap()
                .entries[0]
                .mode,
            0o755
        );
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/workspace")
                .unwrap()
                .entries[0]
                .mode,
            0o700
        );
    }

    #[test]
    fn same_directory_keeps_entries_when_host_path_uses_device_namespace() {
        let home = tempfile::tempdir().unwrap();
        let volume = home.path().join("volumes").join("data");
        std::fs::create_dir_all(&volume).unwrap();
        let alias = PathBuf::from(format!(r"\\.\{}", volume.display()));
        assert_ne!(volume, alias);
        assert_eq!(
            std::fs::canonicalize(&volume).unwrap(),
            std::fs::canonicalize(&alias).unwrap()
        );
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&captured).unwrap()).unwrap();
        let older = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        let newer = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(20);
        std::fs::File::options()
            .write(true)
            .open(&sidecar)
            .unwrap()
            .set_modified(older)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(newer)
            .unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), volume)]).unwrap();
        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), alias)]).unwrap();
        let staged: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/data")
                .unwrap()
                .entries[0]
                .mode,
            0o755
        );
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/workspace")
                .unwrap()
                .entries[0]
                .mode,
            0o700
        );
    }

    #[test]
    fn trailing_dot_host_path_reads_the_volume_sidecar() {
        let home = tempfile::tempdir().unwrap();
        let volume = home.path().join("volumes").join("data");
        std::fs::create_dir_all(&volume).unwrap();
        let alias = volume.parent().unwrap().join("data.");
        assert_ne!(volume, alias);
        assert_eq!(
            std::fs::canonicalize(&volume).unwrap(),
            std::fs::canonicalize(&alias).unwrap()
        );
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), alias)]).unwrap();
        let staged: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(rootfs.join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/data")
                .unwrap()
                .entries[0]
                .mode,
            0o640
        );
    }

    #[test]
    fn trailing_space_host_path_reads_the_volume_sidecar() {
        let home = tempfile::tempdir().unwrap();
        let volume = home.path().join("volumes").join("data");
        std::fs::create_dir_all(&volume).unwrap();
        let alias = volume.parent().unwrap().join("data ");
        assert_ne!(volume, alias);
        assert_eq!(
            std::fs::canonicalize(&volume).unwrap(),
            std::fs::canonicalize(&alias).unwrap()
        );
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), alias)]).unwrap();
        let staged: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(rootfs.join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/data")
                .unwrap()
                .entries[0]
                .mode,
            0o640
        );
    }

    #[test]
    fn parent_component_host_path_reads_the_volume_sidecar() {
        let home = tempfile::tempdir().unwrap();
        let volume = home.path().join("volumes").join("data");
        let decoy = volume.join("foo");
        std::fs::create_dir_all(&decoy).unwrap();
        let alias = decoy.join("..");
        assert_ne!(volume, alias);
        assert_eq!(
            std::fs::canonicalize(&volume).unwrap(),
            std::fs::canonicalize(&alias).unwrap()
        );
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), alias)]).unwrap();
        let staged: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(rootfs.join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/data")
                .unwrap()
                .entries[0]
                .mode,
            0o640
        );
    }

    #[test]
    fn verbatim_parent_component_does_not_read_the_volume_sidecar() {
        let home = tempfile::tempdir().unwrap();
        let volume = home.path().join("volumes").join("data");
        std::fs::create_dir_all(volume.join("foo")).unwrap();
        let canon = std::fs::canonicalize(&volume).unwrap();
        let alias = PathBuf::from(format!(r"{}\foo\..", canon.display()));
        assert_ne!(canon, alias);
        assert!(std::fs::canonicalize(&alias).is_err());
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), alias)]).unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        if let Ok(bytes) = std::fs::read(&manifest_path) {
            let staged: VolumePosixManifest = serde_json::from_slice(&bytes).unwrap();
            assert!(staged
                .mounts
                .iter()
                .all(|mount| mount.guest_path != "/data"));
        }
    }

    #[test]
    fn short_name_host_path_reads_the_volume_sidecar() {
        let home = tempfile::tempdir().unwrap();
        let volume = home.path().join("volumes").join("LongVolumeName");
        std::fs::create_dir_all(&volume).unwrap();
        let alias = home.path().join("volumes").join("LONGVO~1");
        assert_ne!(volume, alias);
        assert_eq!(
            std::fs::canonicalize(&volume).unwrap(),
            std::fs::canonicalize(&alias).unwrap()
        );
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), alias)]).unwrap();
        let staged: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(rootfs.join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/data")
                .unwrap()
                .entries[0]
                .mode,
            0o640
        );
    }

    #[test]
    fn short_name_with_trailing_dot_reads_the_volume_sidecar() {
        let home = tempfile::tempdir().unwrap();
        let volume = home.path().join("volumes").join("LongVolumeName");
        std::fs::create_dir_all(&volume).unwrap();
        let alias = home.path().join("volumes").join("LONGVO~1.");
        assert_ne!(volume, alias);
        assert_eq!(
            std::fs::canonicalize(&volume).unwrap(),
            std::fs::canonicalize(&alias).unwrap()
        );
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), alias)]).unwrap();
        let staged: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(rootfs.join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/data")
                .unwrap()
                .entries[0]
                .mode,
            0o640
        );
    }

    #[test]
    fn verbatim_short_name_host_path_reads_the_volume_sidecar() {
        let home = tempfile::tempdir().unwrap();
        let volume = home.path().join("volumes").join("LongVolumeName");
        std::fs::create_dir_all(&volume).unwrap();
        let short = home.path().join("volumes").join("LONGVO~1");
        let canon = std::fs::canonicalize(&volume).unwrap();
        let alias = PathBuf::from(format!(r"{}\LONGVO~1", canon.parent().unwrap().display()));
        assert_ne!(canon, alias);
        assert_eq!(std::fs::canonicalize(&short).unwrap(), canon);
        assert_eq!(std::fs::canonicalize(&alias).unwrap(), canon);
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), alias)]).unwrap();
        let staged: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(rootfs.join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/data")
                .unwrap()
                .entries[0]
                .mode,
            0o640
        );
    }

    #[test]
    fn short_name_with_trailing_space_reads_the_volume_sidecar() {
        let home = tempfile::tempdir().unwrap();
        let volume = home.path().join("volumes").join("LongVolumeName");
        std::fs::create_dir_all(&volume).unwrap();
        let alias = home.path().join("volumes").join("LONGVO~1 ");
        assert_ne!(volume, alias);
        assert_eq!(
            std::fs::canonicalize(&volume).unwrap(),
            std::fs::canonicalize(&alias).unwrap()
        );
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), alias)]).unwrap();
        let staged: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(rootfs.join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/data")
                .unwrap()
                .entries[0]
                .mode,
            0o640
        );
    }

    #[test]
    fn verbatim_short_name_with_trailing_dot_does_not_read_the_volume_sidecar() {
        let home = tempfile::tempdir().unwrap();
        let volume = home.path().join("volumes").join("LongVolumeName");
        std::fs::create_dir_all(&volume).unwrap();
        let canon = std::fs::canonicalize(&volume).unwrap();
        let alias = PathBuf::from(format!(r"{}\LONGVO~1.", canon.parent().unwrap().display()));
        assert_ne!(canon, alias);
        assert!(std::fs::canonicalize(&alias).is_err());
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), alias)]).unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        if let Ok(bytes) = std::fs::read(&manifest_path) {
            let staged: VolumePosixManifest = serde_json::from_slice(&bytes).unwrap();
            assert!(staged
                .mounts
                .iter()
                .all(|mount| mount.guest_path != "/data"));
        }
    }

    #[test]
    fn unicode_short_name_host_path_reads_the_volume_sidecar() {
        let home = tempfile::tempdir().unwrap();
        let volume = home.path().join("volumes").join("项目数据目录名");
        std::fs::create_dir_all(&volume).unwrap();
        let alias = home.path().join("volumes").join("项目数~1");
        assert_ne!(volume, alias);
        assert_eq!(
            std::fs::canonicalize(&volume).unwrap(),
            std::fs::canonicalize(&alias).unwrap()
        );
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), alias)]).unwrap();
        let staged: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(rootfs.join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/data")
                .unwrap()
                .entries[0]
                .mode,
            0o640
        );
    }

    #[test]
    fn shared_host_volume_drops_nested_entries_on_every_guest_path() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let workspace = volumes.join("workspace");
        let cache = volumes.join("cache");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let encode =
            |path: &str| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, path);
        let entry = |path: &str, mode: u32| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: a3s_box_core::rootfs_metadata::RootfsEntryKind::Regular,
            mode,
            uid: 1000,
            gid: 1000,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        };
        let manifest = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry("note.txt", 0o644)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace/cache".to_string(),
                    entries: vec![entry("secret.txt", 0o600)],
                    retained: false,
                },
            ],
        };
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        write_volume_posix_sidecar(
            &workspace,
            &VolumePosixSidecar::new(vec![
                entry("note.txt", 0o644),
                entry("cache/secret.txt", 0o700),
            ]),
        )
        .unwrap();
        let sidecar = volume_posix_sidecar_path(&workspace).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&sidecar)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(4_000_000_000))
            .unwrap();

        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/workspace".to_string(), workspace.clone()),
                ("/backup".to_string(), workspace),
                ("/workspace/cache".to_string(), cache),
            ],
        )
        .unwrap();
        let staged: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        for guest_path in ["/workspace", "/backup"] {
            let mount = staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == guest_path)
                .unwrap();
            assert!(
                mount
                    .entries
                    .iter()
                    .any(|item| item.path_base64 == encode("note.txt")),
                "{guest_path}"
            );
            assert!(
                mount
                    .entries
                    .iter()
                    .all(|item| item.path_base64 != encode("cache/secret.txt")),
                "{guest_path}"
            );
        }
        let nested = staged
            .mounts
            .iter()
            .find(|mount| mount.guest_path == "/workspace/cache")
            .unwrap();
        assert_eq!(nested.entries[0].path_base64, encode("secret.txt"));
        assert_eq!(nested.entries[0].mode, 0o600);
    }

    #[test]
    fn harvest_drops_nested_entries_listed_on_another_guest_path() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let workspace = volumes.join("workspace");
        let cache = volumes.join("cache");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/workspace".to_string(), workspace.clone()),
                ("/backup".to_string(), workspace.clone()),
                ("/workspace/cache".to_string(), cache.clone()),
            ],
        )
        .unwrap();
        let encode =
            |path: &str| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, path);
        let entry = |path: &str, mode: u32| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: a3s_box_core::rootfs_metadata::RootfsEntryKind::Regular,
            mode,
            uid: 1000,
            gid: 1000,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        };
        let manifest = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/backup".to_string(),
                    entries: vec![entry("note.txt", 0o644), entry("cache/secret.txt", 0o700)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace/cache".to_string(),
                    entries: vec![entry("secret.txt", 0o600)],
                    retained: false,
                },
            ],
        };
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(4_000_000_000))
            .unwrap();

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let parent = read_volume_posix_sidecar(&workspace).unwrap().unwrap();
        assert!(parent
            .entries
            .iter()
            .any(|item| item.path_base64 == encode("note.txt") && item.mode == 0o644));
        assert!(parent
            .entries
            .iter()
            .all(|item| item.path_base64 != encode("cache/secret.txt")));
        let nested = read_volume_posix_sidecar(&cache).unwrap().unwrap();
        assert_eq!(nested.entries[0].path_base64, encode("secret.txt"));
        assert_eq!(nested.entries[0].mode, 0o600);
    }

    #[test]
    fn harvest_drops_nested_entries_from_a_newer_sidecar_listed_on_another_guest_path() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let workspace = volumes.join("workspace");
        let cache = volumes.join("cache");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/workspace".to_string(), workspace.clone()),
                ("/backup".to_string(), workspace.clone()),
                ("/workspace/cache".to_string(), cache.clone()),
            ],
        )
        .unwrap();
        let encode =
            |path: &str| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, path);
        let entry = |path: &str, mode: u32| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: a3s_box_core::rootfs_metadata::RootfsEntryKind::Regular,
            mode,
            uid: 1000,
            gid: 1000,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        };
        let manifest = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/backup".to_string(),
                    entries: vec![entry("note.txt", 0o640), entry("cache/secret.txt", 0o700)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry("note.txt", 0o640), entry("cache/secret.txt", 0o700)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace/cache".to_string(),
                    entries: vec![entry("secret.txt", 0o600)],
                    retained: false,
                },
            ],
        };
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(3_000_000_000))
            .unwrap();
        write_volume_posix_sidecar(
            &workspace,
            &VolumePosixSidecar::new(vec![
                entry("note.txt", 0o644),
                entry("cache/secret.txt", 0o700),
            ]),
        )
        .unwrap();
        write_volume_posix_sidecar(
            &cache,
            &VolumePosixSidecar::new(vec![entry("secret.txt", 0o600)]),
        )
        .unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(
            &volume_posix_sidecar_path(&workspace).unwrap(),
            4_000_000_000,
        );
        set_mtime(&volume_posix_sidecar_path(&cache).unwrap(), 5_000_000_000);

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let parent = read_volume_posix_sidecar(&workspace).unwrap().unwrap();
        assert!(
            parent
                .entries
                .iter()
                .any(|item| item.path_base64 == encode("note.txt") && item.mode == 0o644),
            "{parent:?}"
        );
        assert!(parent
            .entries
            .iter()
            .all(|item| item.path_base64 != encode("cache/secret.txt")));
        let nested = read_volume_posix_sidecar(&cache).unwrap().unwrap();
        assert_eq!(nested.entries[0].path_base64, encode("secret.txt"));
        assert_eq!(nested.entries[0].mode, 0o600);
    }

    #[test]
    fn harvest_drops_nested_entries_when_alias_captures_disagree() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let workspace = volumes.join("workspace");
        let cache = volumes.join("cache");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/workspace".to_string(), workspace.clone()),
                ("/backup".to_string(), workspace.clone()),
                ("/workspace/cache".to_string(), cache.clone()),
            ],
        )
        .unwrap();
        let encode =
            |path: &str| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, path);
        let entry = |path: &str, mode: u32| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: a3s_box_core::rootfs_metadata::RootfsEntryKind::Regular,
            mode,
            uid: 1000,
            gid: 1000,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        };
        let manifest = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry("note.txt", 0o755), entry("cache/secret.txt", 0o700)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/backup".to_string(),
                    entries: vec![entry("note.txt", 0o700), entry("cache/secret.txt", 0o700)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace/cache".to_string(),
                    entries: vec![entry("secret.txt", 0o600)],
                    retained: false,
                },
            ],
        };
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(4_000_000_000))
            .unwrap();
        write_volume_posix_sidecar(
            &workspace,
            &VolumePosixSidecar::new(vec![
                entry("note.txt", 0o644),
                entry("cache/secret.txt", 0o700),
            ]),
        )
        .unwrap();
        write_volume_posix_sidecar(
            &cache,
            &VolumePosixSidecar::new(vec![entry("secret.txt", 0o600)]),
        )
        .unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(
            &volume_posix_sidecar_path(&workspace).unwrap(),
            3_000_000_000,
        );
        set_mtime(&volume_posix_sidecar_path(&cache).unwrap(), 5_000_000_000);

        let error = harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap_err();
        assert!(error.to_string().contains("disagree"), "{error}");
        let parent = read_volume_posix_sidecar(&workspace).unwrap().unwrap();
        assert!(
            parent
                .entries
                .iter()
                .any(|item| item.path_base64 == encode("note.txt") && item.mode == 0o644),
            "{parent:?}"
        );
        assert!(parent
            .entries
            .iter()
            .all(|item| item.path_base64 != encode("cache/secret.txt")));
        let nested = read_volume_posix_sidecar(&cache).unwrap().unwrap();
        assert_eq!(nested.entries[0].path_base64, encode("secret.txt"));
        assert_eq!(nested.entries[0].mode, 0o600);
    }

    #[test]
    fn harvest_refuses_disagreeing_captures_of_one_volume() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let before = std::fs::read(&sidecar).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/data".to_string(), volume.clone()),
                ("/mnt/data".to_string(), volume.clone()),
            ],
        )
        .unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/mnt/data".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(
            rootfs.join(VOLUME_POSIX_METADATA_FILE),
            serde_json::to_vec(&captured).unwrap(),
        )
        .unwrap();

        let error = harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap_err();
        assert!(error.to_string().contains("disagree"), "{error}");
        assert_eq!(std::fs::read(&sidecar).unwrap(), before);
    }

    #[test]
    fn harvest_refuses_disagreeing_captures_when_host_paths_differ_by_unicode_case() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("Größe");
        std::fs::create_dir_all(&volume).unwrap();
        let alias = volumes.join("GrÖße");
        assert_ne!(volume, alias);
        assert_eq!(
            std::fs::canonicalize(&volume).unwrap(),
            std::fs::canonicalize(&alias).unwrap()
        );
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let before = std::fs::read(&sidecar).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/data".to_string(), volume.clone()),
                ("/mnt/data".to_string(), alias),
            ],
        )
        .unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/mnt/data".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(
            rootfs.join(VOLUME_POSIX_METADATA_FILE),
            serde_json::to_vec(&captured).unwrap(),
        )
        .unwrap();

        let error = harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap_err();
        assert!(error.to_string().contains("disagree"), "{error}");
        assert_eq!(std::fs::read(&sidecar).unwrap(), before);
    }

    #[test]
    fn harvest_refuses_disagreeing_captures_when_host_paths_differ_by_case() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        let alias = volumes.join("Data");
        assert_ne!(volume, alias);
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let before = std::fs::read(&sidecar).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/data".to_string(), volume.clone()),
                ("/mnt/data".to_string(), alias),
            ],
        )
        .unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/mnt/data".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(
            rootfs.join(VOLUME_POSIX_METADATA_FILE),
            serde_json::to_vec(&captured).unwrap(),
        )
        .unwrap();

        let error = harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap_err();
        assert!(error.to_string().contains("disagree"), "{error}");
        assert_eq!(std::fs::read(&sidecar).unwrap(), before);
    }

    #[test]
    fn harvest_refuses_disagreeing_captures_when_volumes_dir_differs_by_case() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        let alias = home.path().join("Volumes").join("data");
        assert_ne!(volume.parent(), alias.parent());
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let before = std::fs::read(&sidecar).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/data".to_string(), volume.clone()),
                ("/mnt/data".to_string(), alias),
            ],
        )
        .unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/mnt/data".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(
            rootfs.join(VOLUME_POSIX_METADATA_FILE),
            serde_json::to_vec(&captured).unwrap(),
        )
        .unwrap();

        let error = harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap_err();
        assert!(error.to_string().contains("disagree"), "{error}");
        assert_eq!(std::fs::read(&sidecar).unwrap(), before);
    }

    #[test]
    fn harvest_refuses_disagreeing_captures_when_host_path_is_verbatim() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        let verbatim = std::fs::canonicalize(&volume).unwrap();
        assert_ne!(volume, verbatim);
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let before = std::fs::read(&sidecar).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/data".to_string(), volume.clone()),
                ("/mnt/data".to_string(), verbatim),
            ],
        )
        .unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/mnt/data".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(
            rootfs.join(VOLUME_POSIX_METADATA_FILE),
            serde_json::to_vec(&captured).unwrap(),
        )
        .unwrap();

        let error = harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap_err();
        assert!(error.to_string().contains("disagree"), "{error}");
        assert_eq!(std::fs::read(&sidecar).unwrap(), before);
    }

    #[test]
    fn harvest_refuses_disagreeing_captures_when_host_path_has_parent_segment() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        let alias = volume.join("..").join("data");
        assert_ne!(volume, alias);
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let before = std::fs::read(&sidecar).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/data".to_string(), volume.clone()),
                ("/mnt/data".to_string(), alias),
            ],
        )
        .unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/mnt/data".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(
            rootfs.join(VOLUME_POSIX_METADATA_FILE),
            serde_json::to_vec(&captured).unwrap(),
        )
        .unwrap();

        let error = harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap_err();
        assert!(error.to_string().contains("disagree"), "{error}");
        assert_eq!(std::fs::read(&sidecar).unwrap(), before);
    }

    #[test]
    fn harvest_refuses_disagreeing_captures_when_host_path_has_trailing_dot() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        let alias = volumes.join("data.");
        assert_ne!(volume, alias);
        assert_eq!(
            std::fs::canonicalize(&volume).unwrap(),
            std::fs::canonicalize(&alias).unwrap()
        );
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let before = std::fs::read(&sidecar).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/data".to_string(), volume.clone()),
                ("/mnt/data".to_string(), alias),
            ],
        )
        .unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/mnt/data".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(
            rootfs.join(VOLUME_POSIX_METADATA_FILE),
            serde_json::to_vec(&captured).unwrap(),
        )
        .unwrap();

        let error = harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap_err();
        assert!(error.to_string().contains("disagree"), "{error}");
        assert_eq!(std::fs::read(&sidecar).unwrap(), before);
    }

    #[test]
    fn harvest_refuses_disagreeing_captures_when_host_path_uses_a_short_name() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("LongVolumeName");
        std::fs::create_dir_all(&volume).unwrap();
        let alias = volumes.join("LONGVO~1");
        assert_ne!(volume, alias);
        assert_eq!(
            std::fs::canonicalize(&volume).unwrap(),
            std::fs::canonicalize(&alias).unwrap()
        );
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let before = std::fs::read(&sidecar).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/data".to_string(), volume.clone()),
                ("/mnt/data".to_string(), alias),
            ],
        )
        .unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/mnt/data".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(
            rootfs.join(VOLUME_POSIX_METADATA_FILE),
            serde_json::to_vec(&captured).unwrap(),
        )
        .unwrap();

        let error = harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap_err();
        assert!(error.to_string().contains("disagree"), "{error}");
        assert_eq!(std::fs::read(&sidecar).unwrap(), before);
    }

    #[test]
    fn harvest_refuses_disagreeing_captures_when_short_name_has_trailing_dot() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("LongVolumeName");
        std::fs::create_dir_all(&volume).unwrap();
        let alias = volumes.join("LONGVO~1.");
        assert_ne!(volume, alias);
        assert_eq!(
            std::fs::canonicalize(&volume).unwrap(),
            std::fs::canonicalize(&alias).unwrap()
        );
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let before = std::fs::read(&sidecar).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/data".to_string(), volume.clone()),
                ("/mnt/data".to_string(), alias),
            ],
        )
        .unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/mnt/data".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(
            rootfs.join(VOLUME_POSIX_METADATA_FILE),
            serde_json::to_vec(&captured).unwrap(),
        )
        .unwrap();

        let error = harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap_err();
        assert!(error.to_string().contains("disagree"), "{error}");
        assert_eq!(std::fs::read(&sidecar).unwrap(), before);
    }

    #[test]
    fn harvest_refuses_disagreeing_captures_when_unicode_short_name_is_used() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("项目数据目录名");
        std::fs::create_dir_all(&volume).unwrap();
        let alias = volumes.join("项目数~1");
        assert_ne!(volume, alias);
        assert_eq!(
            std::fs::canonicalize(&volume).unwrap(),
            std::fs::canonicalize(&alias).unwrap()
        );
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let before = std::fs::read(&sidecar).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/data".to_string(), volume.clone()),
                ("/mnt/data".to_string(), alias),
            ],
        )
        .unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/mnt/data".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(
            rootfs.join(VOLUME_POSIX_METADATA_FILE),
            serde_json::to_vec(&captured).unwrap(),
        )
        .unwrap();

        let error = harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap_err();
        assert!(error.to_string().contains("disagree"), "{error}");
        assert_eq!(std::fs::read(&sidecar).unwrap(), before);
    }

    #[test]
    fn harvest_refuses_disagreeing_captures_when_host_path_uses_device_unc() {
        let home = tempfile::tempdir().unwrap();
        let volume = home.path().join("volumes").join("data");
        std::fs::create_dir_all(&volume).unwrap();
        let canon = std::fs::canonicalize(&volume).unwrap();
        let text = canon.to_string_lossy();
        let after = text.trim_start_matches(r"\\?\");
        let mut chars = after.chars();
        let drive = chars.next().unwrap();
        assert_eq!(chars.next(), Some(':'));
        assert_eq!(chars.next(), Some('\\'));
        let tail: String = chars.collect();
        let unc = PathBuf::from(format!(r"\\localhost\{drive}$\{tail}"));
        let alias = PathBuf::from(format!(r"\\.\UNC\localhost\{drive}$\{tail}"));
        assert_ne!(unc, alias);
        assert_eq!(
            std::fs::canonicalize(&unc).unwrap(),
            std::fs::canonicalize(&alias).unwrap()
        );
        write_volume_posix_sidecar(&unc, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&unc).unwrap();
        let before = std::fs::read(&sidecar).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/data".to_string(), unc.clone()),
                ("/mnt/data".to_string(), alias),
            ],
        )
        .unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/mnt/data".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(
            rootfs.join(VOLUME_POSIX_METADATA_FILE),
            serde_json::to_vec(&captured).unwrap(),
        )
        .unwrap();
        let volumes = unc.parent().unwrap();

        let error = harvest_volume_posix_sidecars(&box_dir, volumes).unwrap_err();
        assert!(error.to_string().contains("disagree"), "{error}");
        assert_eq!(std::fs::read(&sidecar).unwrap(), before);
    }

    #[test]
    fn harvest_refuses_disagreeing_captures_when_host_path_steps_through_a_child() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        let decoy = volume.join("foo");
        std::fs::create_dir_all(&decoy).unwrap();
        let alias = decoy.join("..");
        assert_ne!(volume, alias);
        assert_eq!(
            std::fs::canonicalize(&volume).unwrap(),
            std::fs::canonicalize(&alias).unwrap()
        );
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let before = std::fs::read(&sidecar).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/data".to_string(), volume.clone()),
                ("/mnt/data".to_string(), alias),
            ],
        )
        .unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/mnt/data".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(
            rootfs.join(VOLUME_POSIX_METADATA_FILE),
            serde_json::to_vec(&captured).unwrap(),
        )
        .unwrap();

        let error = harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap_err();
        assert!(error.to_string().contains("disagree"), "{error}");
        assert_eq!(std::fs::read(&sidecar).unwrap(), before);
    }

    #[test]
    fn harvest_refuses_disagreeing_captures_when_host_path_has_trailing_space() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        let alias = volumes.join("data ");
        assert_ne!(volume, alias);
        assert_eq!(
            std::fs::canonicalize(&volume).unwrap(),
            std::fs::canonicalize(&alias).unwrap()
        );
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let before = std::fs::read(&sidecar).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/data".to_string(), volume.clone()),
                ("/mnt/data".to_string(), alias),
            ],
        )
        .unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/mnt/data".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(
            rootfs.join(VOLUME_POSIX_METADATA_FILE),
            serde_json::to_vec(&captured).unwrap(),
        )
        .unwrap();

        let error = harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap_err();
        assert!(error.to_string().contains("disagree"), "{error}");
        assert_eq!(std::fs::read(&sidecar).unwrap(), before);
    }

    #[test]
    fn harvest_refuses_disagreeing_captures_when_host_path_uses_device_namespace() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        let alias = PathBuf::from(format!(r"\\.\{}", volume.display()));
        assert_ne!(volume, alias);
        assert_eq!(
            std::fs::canonicalize(&volume).unwrap(),
            std::fs::canonicalize(&alias).unwrap()
        );
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let before = std::fs::read(&sidecar).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/data".to_string(), volume.clone()),
                ("/mnt/data".to_string(), alias),
            ],
        )
        .unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/mnt/data".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(
            rootfs.join(VOLUME_POSIX_METADATA_FILE),
            serde_json::to_vec(&captured).unwrap(),
        )
        .unwrap();

        let error = harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap_err();
        assert!(error.to_string().contains("disagree"), "{error}");
        assert_eq!(std::fs::read(&sidecar).unwrap(), before);
    }

    #[test]
    fn stage_and_harvest_drop_entries_inside_a_nested_volume() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let workspace = volumes.join("workspace");
        let cache = volumes.join("cache");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = vec![
            ("/workspace".to_string(), workspace.clone()),
            ("/workspace/cache".to_string(), cache.clone()),
        ];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let encode =
            |path: &str| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, path);
        let entry = |path: &str, mode: u32| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: a3s_box_core::rootfs_metadata::RootfsEntryKind::Regular,
            mode,
            uid: 1000,
            gid: 1000,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        };
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry("note.txt", 0o644), entry("cache/secret.txt", 0o700)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace/cache".to_string(),
                    entries: vec![entry("secret.txt", 0o600)],
                    retained: false,
                },
            ],
        };
        std::fs::write(
            rootfs.join(VOLUME_POSIX_METADATA_FILE),
            serde_json::to_vec(&captured).unwrap(),
        )
        .unwrap();

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let parent = read_volume_posix_sidecar(&workspace).unwrap().unwrap();
        let nested = read_volume_posix_sidecar(&cache).unwrap().unwrap();
        assert!(parent
            .entries
            .iter()
            .all(|entry| entry.path_base64 != encode("cache/secret.txt")));
        assert_eq!(nested.entries[0].path_base64, encode("secret.txt"));
        assert_eq!(nested.entries[0].mode, 0o600);

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let staged: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(rootfs.join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        let staged_parent = staged
            .mounts
            .iter()
            .find(|mount| mount.guest_path == "/workspace")
            .unwrap();
        assert!(staged_parent
            .entries
            .iter()
            .all(|entry| entry.path_base64 != encode("cache/secret.txt")));
    }

    #[test]
    fn discarded_manifest_entries_fall_back_to_the_sidecar() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        let encode =
            |path: &str| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, path);
        let entry = |path: &str, mode: u32| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: a3s_box_core::rootfs_metadata::RootfsEntryKind::Regular,
            mode,
            uid: 1000,
            gid: 1000,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        };
        write_volume_posix_sidecar(
            &volume,
            &VolumePosixSidecar::new(vec![entry("note.txt", 0o755)]),
        )
        .unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let sidecar_bytes = std::fs::read(&sidecar).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let poison = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry("../outside", 0o600)],
                retained: false,
            }],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&poison).unwrap()).unwrap();
        let older = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        let newer = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(20);
        std::fs::File::options()
            .write(true)
            .open(&sidecar)
            .unwrap()
            .set_modified(older)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(newer)
            .unwrap();

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        assert_eq!(std::fs::read(&sidecar).unwrap(), sidecar_bytes);

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let staged: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(staged.mounts.len(), 1);
        assert_eq!(staged.mounts[0].guest_path, "/data");
        assert_eq!(staged.mounts[0].entries[0].path_base64, encode("note.txt"));
        assert_eq!(staged.mounts[0].entries[0].mode, 0o755);
    }

    #[test]
    fn older_guest_manifest_does_not_replace_a_newer_sidecar() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let stale = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&stale).unwrap()).unwrap();
        let older = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        let newer = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(20);
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(older)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&sidecar)
            .unwrap()
            .set_modified(newer)
            .unwrap();
        let sidecar_bytes = std::fs::read(&sidecar).unwrap();

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        assert_eq!(std::fs::read(&sidecar).unwrap(), sidecar_bytes);

        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(newer)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&sidecar)
            .unwrap()
            .set_modified(older)
            .unwrap();
        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let updated = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(updated.entries[0].mode, 0o640);
    }

    #[test]
    fn invalid_manifest_root_does_not_hide_a_valid_capture() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let merged = box_dir.join("merged");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&merged).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&captured).unwrap()).unwrap();
        let corrupt = merged.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&corrupt, b"{not-json").unwrap();
        let older = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        let middle = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(20);
        let newest = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(30);
        let set_mtime = |path: &std::path::Path, time: SystemTime| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(time)
                .unwrap();
        };
        set_mtime(&sidecar, older);
        set_mtime(&manifest_path, middle);
        set_mtime(&corrupt, newest);

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let updated = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(updated.entries[0].mode, 0o640);

        std::fs::remove_file(&manifest_path).unwrap();
        let error = harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap_err();
        assert!(
            error.to_string().contains("not a valid manifest"),
            "{error}"
        );
        assert_eq!(
            read_volume_posix_sidecar(&volume).unwrap().unwrap().entries[0].mode,
            0o640
        );
    }

    #[test]
    fn unmounted_guest_path_is_not_staged_for_replay() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume)];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/etc".to_string(),
                    entries: vec![entry(0o600)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&captured).unwrap()).unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let staged: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        let paths: Vec<&str> = staged
            .mounts
            .iter()
            .map(|mount| mount.guest_path.as_str())
            .collect();
        assert_eq!(paths, vec!["/data", "/workspace"]);
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/data")
                .unwrap()
                .entries[0]
                .mode,
            0o755
        );
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/workspace")
                .unwrap()
                .entries[0]
                .mode,
            0o700
        );
    }

    #[test]
    fn caller_bind_guest_path_stays_on_the_rootfs_manifest() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/src".to_string(),
                    entries: vec![entry(0o750)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/etc".to_string(),
                    entries: vec![entry(0o600)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&captured).unwrap()).unwrap();

        let caller = home.path().join("caller-src");
        std::fs::create_dir_all(&caller).unwrap();
        sync_volume_posix(&box_dir, &rootfs, &mounts, &[("/src".to_string(), caller)]).unwrap();
        let staged: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        let mut paths: Vec<&str> = staged
            .mounts
            .iter()
            .map(|mount| mount.guest_path.as_str())
            .collect();
        paths.sort_unstable();
        assert_eq!(paths, vec!["/data", "/src", "/workspace"]);
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/src")
                .unwrap()
                .entries[0]
                .mode,
            0o750
        );

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let sidecar = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(sidecar.entries[0].mode, 0o755);
    }

    #[test]
    fn managed_volume_does_not_inherit_caller_bind_entries() {
        let home = tempfile::tempdir().unwrap();
        let caller = home.path().join("caller-src");
        std::fs::create_dir_all(&caller).unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o750)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&captured).unwrap()).unwrap();
        let older = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        let newer = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(20);
        std::fs::File::options()
            .write(true)
            .open(&sidecar)
            .unwrap()
            .set_modified(older)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(newer)
            .unwrap();

        sync_volume_posix(&box_dir, &rootfs, &[], &[("/data".to_string(), caller)]).unwrap();
        sync_volume_posix(
            &box_dir,
            &rootfs,
            &[("/data".to_string(), volume.clone())],
            &[],
        )
        .unwrap();
        let staged: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        let data = staged
            .mounts
            .iter()
            .find(|mount| mount.guest_path == "/data")
            .unwrap();
        assert_eq!(data.entries[0].mode, 0o640);
        assert_eq!(
            staged
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/workspace")
                .unwrap()
                .entries[0]
                .mode,
            0o700
        );
    }

    #[test]
    fn harvest_ignores_unapplied_replay_pending_file() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let older = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        std::fs::File::options()
            .write(true)
            .open(&sidecar_path)
            .unwrap()
            .set_modified(older)
            .unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();

        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let published = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o755)],
                retained: false,
            }],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&published).unwrap()).unwrap();
        let pending = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o600)],
                retained: false,
            }],
        };
        std::fs::write(
            rootfs.join(VOLUME_POSIX_METADATA_PENDING_FILE),
            serde_json::to_vec(&pending).unwrap(),
        )
        .unwrap();
        let newer = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(50);
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(newer)
            .unwrap();

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let sidecar = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(sidecar.entries[0].mode, 0o755);
    }

    #[test]
    fn harvest_does_not_write_a_retained_capture() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let older = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        std::fs::File::options()
            .write(true)
            .open(&sidecar_path)
            .unwrap()
            .set_modified(older)
            .unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let published = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount::retained(
                "/data".to_string(),
                vec![entry(0o640)],
            )],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&published).unwrap()).unwrap();
        let newer = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(50);
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(newer)
            .unwrap();

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let sidecar = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(sidecar.entries[0].mode, 0o755);
    }

    #[test]
    fn harvest_does_not_write_a_sidecar_through_a_link_above_the_volume_store() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside");
        let outside_volume = outside.join("store").join("data");
        std::fs::create_dir_all(&outside_volume).unwrap();
        std::fs::write(outside_volume.join("secret.txt"), b"secret").unwrap();
        let link = root.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.path().join("outside"), &link).unwrap();
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let mut command = std::process::Command::new("cmd");
            command.raw_arg(format!(
                "/C mklink /J \"{}\" \"{}\"",
                link.display(),
                outside.display()
            ));
            let status = command.status().expect("mklink");
            assert!(status.success(), "mklink /J failed: {status}");
        }
        let volumes = link.join("store");
        let volume = volumes.join("data");
        let box_dir = root.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), volume.clone())])
            .unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o755)],
                retained: false,
            }],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&captured).unwrap()).unwrap();
        let newer = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(50);
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(newer)
            .unwrap();

        let _ = harvest_volume_posix_sidecars(&box_dir, &volumes);
        let outside_sidecar = outside.join("store").join("data.a3s-volume-posix.v1.json");
        assert!(
            !outside_sidecar.exists(),
            "harvest wrote a sidecar through the link"
        );
        assert_eq!(
            std::fs::read(outside_volume.join("secret.txt")).unwrap(),
            b"secret"
        );
    }

    #[test]
    fn harvest_drops_nested_entries_from_a_retained_parent_sidecar() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let workspace = volumes.join("workspace");
        let cache = volumes.join("cache");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/workspace".to_string(), workspace.clone()),
                ("/workspace/cache".to_string(), cache.clone()),
            ],
        )
        .unwrap();
        let encode =
            |path: &str| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, path);
        let entry = |path: &str, mode: u32| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: a3s_box_core::rootfs_metadata::RootfsEntryKind::Regular,
            mode,
            uid: 1000,
            gid: 1000,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        };
        write_volume_posix_sidecar(
            &workspace,
            &VolumePosixSidecar::new(vec![
                entry("note.txt", 0o644),
                entry("cache/secret.txt", 0o700),
            ]),
        )
        .unwrap();
        let manifest = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount::retained(
                    "/workspace".to_string(),
                    vec![entry("note.txt", 0o640), entry("cache/secret.txt", 0o700)],
                ),
                VolumePosixMount {
                    guest_path: "/workspace/cache".to_string(),
                    entries: vec![entry("secret.txt", 0o600)],
                    retained: false,
                },
            ],
        };
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&volume_posix_sidecar_path(&workspace).unwrap(), 5);
        set_mtime(&manifest_path, 4_000_000_000);

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let parent = read_volume_posix_sidecar(&workspace).unwrap().unwrap();
        let nested = read_volume_posix_sidecar(&cache).unwrap().unwrap();
        assert!(parent
            .entries
            .iter()
            .any(|item| item.path_base64 == encode("note.txt") && item.mode == 0o644));
        assert!(parent
            .entries
            .iter()
            .all(|item| item.path_base64 != encode("cache/secret.txt")));
        assert_eq!(nested.entries[0].path_base64, encode("secret.txt"));
        assert_eq!(nested.entries[0].mode, 0o600);
    }

    #[test]
    fn harvest_drops_nested_entries_from_a_newer_parent_sidecar() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let workspace = volumes.join("workspace");
        let cache = volumes.join("cache");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/workspace".to_string(), workspace.clone()),
                ("/workspace/cache".to_string(), cache.clone()),
            ],
        )
        .unwrap();
        let encode =
            |path: &str| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, path);
        let entry = |path: &str, mode: u32| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: a3s_box_core::rootfs_metadata::RootfsEntryKind::Regular,
            mode,
            uid: 1000,
            gid: 1000,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        };
        write_volume_posix_sidecar(
            &workspace,
            &VolumePosixSidecar::new(vec![
                entry("note.txt", 0o644),
                entry("cache/secret.txt", 0o700),
            ]),
        )
        .unwrap();
        let manifest = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry("note.txt", 0o640)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace/cache".to_string(),
                    entries: vec![entry("secret.txt", 0o600)],
                    retained: false,
                },
            ],
        };
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        let cache_sidecar = volume_posix_sidecar_path(&cache).unwrap();
        if cache_sidecar.is_file() {
            set_mtime(&cache_sidecar, 5);
        }
        set_mtime(&manifest_path, 3_000_000_000);
        set_mtime(
            &volume_posix_sidecar_path(&workspace).unwrap(),
            4_000_000_000,
        );

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let parent = read_volume_posix_sidecar(&workspace).unwrap().unwrap();
        let nested = read_volume_posix_sidecar(&cache).unwrap().unwrap();
        assert!(parent
            .entries
            .iter()
            .any(|item| item.path_base64 == encode("note.txt") && item.mode == 0o644));
        assert!(parent
            .entries
            .iter()
            .all(|item| item.path_base64 != encode("cache/secret.txt")));
        assert_eq!(nested.entries[0].path_base64, encode("secret.txt"));
        assert_eq!(nested.entries[0].mode, 0o600);
    }

    #[test]
    fn harvest_drops_nested_entries_when_the_parent_capture_only_lists_them() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let workspace = volumes.join("workspace");
        let cache = volumes.join("cache");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/workspace".to_string(), workspace.clone()),
                ("/workspace/cache".to_string(), cache.clone()),
            ],
        )
        .unwrap();
        let encode =
            |path: &str| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, path);
        let entry = |path: &str, mode: u32| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: a3s_box_core::rootfs_metadata::RootfsEntryKind::Regular,
            mode,
            uid: 1000,
            gid: 1000,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        };
        write_volume_posix_sidecar(
            &workspace,
            &VolumePosixSidecar::new(vec![
                entry("note.txt", 0o644),
                entry("cache/secret.txt", 0o700),
            ]),
        )
        .unwrap();
        let manifest = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry("cache/secret.txt", 0o700)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace/cache".to_string(),
                    entries: vec![entry("secret.txt", 0o600)],
                    retained: false,
                },
            ],
        };
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        let cache_sidecar = volume_posix_sidecar_path(&cache).unwrap();
        if cache_sidecar.is_file() {
            set_mtime(&cache_sidecar, 5);
        }
        set_mtime(&manifest_path, 3_000_000_000);
        set_mtime(
            &volume_posix_sidecar_path(&workspace).unwrap(),
            4_000_000_000,
        );

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let parent = read_volume_posix_sidecar(&workspace).unwrap().unwrap();
        let nested = read_volume_posix_sidecar(&cache).unwrap().unwrap();
        assert!(parent
            .entries
            .iter()
            .any(|item| item.path_base64 == encode("note.txt") && item.mode == 0o644));
        assert!(parent
            .entries
            .iter()
            .all(|item| item.path_base64 != encode("cache/secret.txt")));
        assert_eq!(nested.entries[0].path_base64, encode("secret.txt"));
        assert_eq!(nested.entries[0].mode, 0o600);
    }

    #[test]
    fn restage_keeps_the_retained_mark() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let older = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        std::fs::File::options()
            .write(true)
            .open(&sidecar_path)
            .unwrap()
            .set_modified(older)
            .unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), volume.clone())])
            .unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let published = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount::retained(
                "/data".to_string(),
                vec![entry(0o640)],
            )],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&published).unwrap()).unwrap();
        let newer = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(50);
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(newer)
            .unwrap();

        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/data".to_string(), volume.clone()),
                ("/var/lib/data".to_string(), volume.clone()),
            ],
        )
        .unwrap();
        let staged: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert!(staged.mounts.iter().all(|mount| mount.retained));
        assert!(staged
            .mounts
            .iter()
            .all(|mount| mount.entries[0].mode == 0o640));
        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let sidecar = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(sidecar.entries[0].mode, 0o755);
    }

    #[test]
    fn retained_capture_wins_over_a_sibling_partial() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let older = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        std::fs::File::options()
            .write(true)
            .open(&sidecar_path)
            .unwrap()
            .set_modified(older)
            .unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/data".to_string(), volume.clone()),
                ("/var/lib/data".to_string(), volume.clone()),
            ],
        )
        .unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let published = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount::retained("/data".to_string(), vec![entry(0o640)]),
                VolumePosixMount {
                    guest_path: "/var/lib/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&published).unwrap()).unwrap();
        let newer = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(50);
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(newer)
            .unwrap();

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let sidecar = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(sidecar.entries[0].mode, 0o640);

        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/var/lib/data".to_string(), volume.clone()),
                ("/data".to_string(), volume.clone()),
            ],
        )
        .unwrap();
        let staged: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert!(staged.mounts.iter().all(|mount| mount.retained));
        assert!(staged
            .mounts
            .iter()
            .all(|mount| mount.entries[0].mode == 0o640));
        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let sidecar = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(sidecar.entries[0].mode, 0o640);
    }

    #[test]
    fn disagreeing_fresh_captures_fall_back_to_the_sidecar() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let older = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        std::fs::File::options()
            .write(true)
            .open(&sidecar_path)
            .unwrap()
            .set_modified(older)
            .unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/data".to_string(), volume.clone()),
                ("/var/lib/data".to_string(), volume.clone()),
            ],
        )
        .unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let published = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/var/lib/data".to_string(),
                    entries: vec![entry(0o600)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&published).unwrap()).unwrap();
        let newer = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(50);
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(newer)
            .unwrap();

        let error = harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap_err();
        assert!(error.to_string().contains("disagree"), "{error}");
        let sidecar = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(sidecar.entries[0].mode, 0o640);

        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/var/lib/data".to_string(), volume.clone()),
                ("/data".to_string(), volume.clone()),
            ],
        )
        .unwrap();
        let staged: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert!(staged
            .mounts
            .iter()
            .all(|mount| !mount.retained && mount.entries[0].mode == 0o640));
        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let sidecar = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(sidecar.entries[0].mode, 0o640);
    }

    #[test]
    fn disagreeing_fresh_captures_fall_back_when_host_paths_differ_by_case() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        let alias = volumes.join("Data");
        assert_ne!(volume, alias);
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let older = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        std::fs::File::options()
            .write(true)
            .open(&sidecar_path)
            .unwrap()
            .set_modified(older)
            .unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/data".to_string(), volume.clone()),
                ("/var/lib/data".to_string(), volume.clone()),
            ],
        )
        .unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let published = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/var/lib/data".to_string(),
                    entries: vec![entry(0o600)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&published).unwrap()).unwrap();
        let newer = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(50);
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(newer)
            .unwrap();

        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/data".to_string(), volume),
                ("/var/lib/data".to_string(), alias),
            ],
        )
        .unwrap();
        let staged: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert!(staged
            .mounts
            .iter()
            .all(|mount| !mount.retained && mount.entries[0].mode == 0o640));
    }

    #[test]
    fn disagreeing_retained_captures_fall_back_to_the_sidecar() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let older = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        std::fs::File::options()
            .write(true)
            .open(&sidecar_path)
            .unwrap()
            .set_modified(older)
            .unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/data".to_string(), volume.clone()),
                ("/var/lib/data".to_string(), volume.clone()),
            ],
        )
        .unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let published = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount::retained("/data".to_string(), vec![entry(0o755)]),
                VolumePosixMount::retained("/var/lib/data".to_string(), vec![entry(0o600)]),
            ],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&published).unwrap()).unwrap();
        let newer = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(50);
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(newer)
            .unwrap();

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let sidecar = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(sidecar.entries[0].mode, 0o640);

        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/var/lib/data".to_string(), volume.clone()),
                ("/data".to_string(), volume.clone()),
            ],
        )
        .unwrap();
        let staged: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert!(staged
            .mounts
            .iter()
            .all(|mount| !mount.retained && mount.entries[0].mode == 0o640));
        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let sidecar = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(sidecar.entries[0].mode, 0o640);
    }

    #[test]
    fn fresh_listing_confirms_a_retained_sibling() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let older = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        std::fs::File::options()
            .write(true)
            .open(&sidecar_path)
            .unwrap()
            .set_modified(older)
            .unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/data".to_string(), volume.clone()),
                ("/var/lib/data".to_string(), volume.clone()),
            ],
        )
        .unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let published = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount::retained("/data".to_string(), vec![entry(0o640)]),
                VolumePosixMount {
                    guest_path: "/var/lib/data".to_string(),
                    entries: vec![entry(0o640)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&published).unwrap()).unwrap();
        let newer = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(50);
        std::fs::File::options()
            .write(true)
            .open(&manifest_path)
            .unwrap()
            .set_modified(newer)
            .unwrap();

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let sidecar = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(sidecar.entries[0].mode, 0o640);
    }

    #[test]
    fn stage_keeps_a_newer_capture_from_another_root() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), volume.clone())])
            .unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        let upper_manifest = upper.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&upper_manifest, serde_json::to_vec(&captured).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&sidecar_path, 5);
        set_mtime(&manifest_path, 10);
        set_mtime(&upper_manifest, 50);

        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/data".to_string(), volume.clone()),
                ("/var/lib/data".to_string(), volume.clone()),
            ],
        )
        .unwrap();
        let published = rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
        let staged: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&published).unwrap()).unwrap();
        assert!(staged
            .mounts
            .iter()
            .all(|mount| mount.entries[0].mode == 0o640));
        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let sidecar = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(sidecar.entries[0].mode, 0o640);
    }

    #[test]
    fn stage_removes_a_shadowing_manifest_in_another_root() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o600)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), volume.clone())])
            .unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        let upper_manifest = upper.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&upper_manifest, serde_json::to_vec(&captured).unwrap()).unwrap();
        let manifest_path = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&manifest_path, 10);
        set_mtime(&upper_manifest, 50);
        set_mtime(&sidecar_path, 80);

        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), volume.clone())])
            .unwrap();
        let staged: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(staged.mounts[0].entries[0].mode, 0o600);
        assert!(!upper_manifest.exists());
    }

    #[test]
    fn stage_publishes_the_authoritative_capture_onto_the_guest_root() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let upper_manifest = upper.join(VOLUME_POSIX_METADATA_FILE);
        let write_mode = |path: &std::path::Path, mode: u32| {
            let manifest = VolumePosixManifest {
                schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
                mounts: vec![VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(mode)],
                    retained: false,
                }],
            };
            std::fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        };
        write_mode(&guest_root, 0o755);
        write_mode(&upper_manifest, 0o640);
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&sidecar_path, 5);
        set_mtime(&guest_root, 10);
        set_mtime(&upper_manifest, 50);

        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), volume)]).unwrap();
        let published = rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
        let staged: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&published).unwrap()).unwrap();
        assert_eq!(staged.mounts[0].entries[0].mode, 0o640);
        assert!(!upper_manifest.exists());
    }

    #[test]
    fn stage_keeps_another_roots_capture_after_replay_retires_the_manifest() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let upper_manifest = upper.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        std::fs::write(&upper_manifest, serde_json::to_vec(&captured).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&sidecar_path, 5);
        set_mtime(&guest_root, 10);
        set_mtime(&upper_manifest, 50);

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let published = rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&published).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        assert!(!upper_manifest.exists());
        let newer_than_manifest = |path: &std::path::Path| {
            path.is_file()
                && path
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    > guest_root
                        .metadata()
                        .and_then(|metadata| metadata.modified())
                        .ok()
        };
        if !newer_than_manifest(&rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE)) {
            let _ = std::fs::remove_file(rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE));
        }
        if !newer_than_manifest(&published) {
            let _ = std::fs::remove_file(&published);
        }
        let _ = std::fs::remove_file(&guest_root);

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let sidecar = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(sidecar.entries[0].mode, 0o640);
    }

    #[test]
    fn stage_keeps_a_valid_other_root_capture_when_the_manifest_is_invalid() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&guest_root, b"not-json").unwrap();
        let upper_manifest = upper.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        std::fs::write(&upper_manifest, serde_json::to_vec(&captured).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&sidecar_path, 5);
        set_mtime(&guest_root, 100);
        set_mtime(&upper_manifest, 50);

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let published = rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&published).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        assert!(!upper_manifest.exists());
        let _ = std::fs::remove_file(&guest_root);

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let sidecar = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(sidecar.entries[0].mode, 0o640);
    }

    #[test]
    fn stage_keeps_one_volumes_capture_when_another_sidecar_is_newer() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let data = volumes.join("data");
        let logs = volumes.join("logs");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&logs).unwrap();
        write_volume_posix_sidecar(&data, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        write_volume_posix_sidecar(&logs, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let data_sidecar = volume_posix_sidecar_path(&data).unwrap();
        let logs_sidecar = volume_posix_sidecar_path(&logs).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        let mounts = [
            ("/data".to_string(), data.clone()),
            ("/logs".to_string(), logs.clone()),
        ];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let upper_manifest = upper.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o640)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/logs".to_string(),
                    entries: vec![entry(0o640)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&upper_manifest, serde_json::to_vec(&captured).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&data_sidecar, 5);
        set_mtime(&logs_sidecar, 90);
        set_mtime(&guest_root, 10);
        set_mtime(&upper_manifest, 50);

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let published = rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&published).unwrap()).unwrap();
        let data_mode = kept
            .mounts
            .iter()
            .find(|mount| mount.guest_path == "/data")
            .unwrap()
            .entries[0]
            .mode;
        assert_eq!(data_mode, 0o640);
        assert!(!upper_manifest.exists());
        let newer_than_manifest = |path: &std::path::Path| {
            path.is_file()
                && path
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    > guest_root
                        .metadata()
                        .and_then(|metadata| metadata.modified())
                        .ok()
        };
        if !newer_than_manifest(&rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE)) {
            let _ = std::fs::remove_file(rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE));
        }
        if !newer_than_manifest(&published) {
            let _ = std::fs::remove_file(&published);
        }
        let _ = std::fs::remove_file(&guest_root);

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let data_kept = read_volume_posix_sidecar(&data).unwrap().unwrap();
        assert_eq!(data_kept.entries[0].mode, 0o640);
        let logs_kept = read_volume_posix_sidecar(&logs).unwrap().unwrap();
        assert_eq!(logs_kept.entries[0].mode, 0o755);
    }

    #[test]
    fn stage_keeps_a_rootfs_temp_when_another_sidecar_is_newer() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let data = volumes.join("data");
        let logs = volumes.join("logs");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&logs).unwrap();
        write_volume_posix_sidecar(&data, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        write_volume_posix_sidecar(&logs, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let data_sidecar = volume_posix_sidecar_path(&data).unwrap();
        let logs_sidecar = volume_posix_sidecar_path(&logs).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [
            ("/data".to_string(), data.clone()),
            ("/logs".to_string(), logs.clone()),
        ];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let temp = rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o640)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/logs".to_string(),
                    entries: vec![entry(0o640)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&temp, serde_json::to_vec(&captured).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&data_sidecar, 5);
        set_mtime(&logs_sidecar, 90);
        set_mtime(&guest_root, 10);
        set_mtime(&temp, 50);

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let published = rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&published).unwrap()).unwrap();
        let data_mode = kept
            .mounts
            .iter()
            .find(|mount| mount.guest_path == "/data")
            .unwrap()
            .entries[0]
            .mode;
        assert_eq!(data_mode, 0o640);
        let newer_than_manifest = |path: &std::path::Path| {
            path.is_file()
                && path
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    > guest_root
                        .metadata()
                        .and_then(|metadata| metadata.modified())
                        .ok()
        };
        if !newer_than_manifest(&temp) {
            let _ = std::fs::remove_file(&temp);
        }
        if !newer_than_manifest(&published) {
            let _ = std::fs::remove_file(&published);
        }
        let _ = std::fs::remove_file(&guest_root);

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let data_kept = read_volume_posix_sidecar(&data).unwrap().unwrap();
        assert_eq!(data_kept.entries[0].mode, 0o640);
        let logs_kept = read_volume_posix_sidecar(&logs).unwrap().unwrap();
        assert_eq!(logs_kept.entries[0].mode, 0o755);
    }

    #[test]
    fn stage_removes_an_older_shadow_when_the_guest_root_is_already_current() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        let merged = box_dir.join("merged");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        std::fs::create_dir_all(&merged).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let upper_manifest = upper.join(VOLUME_POSIX_METADATA_FILE);
        let merged_manifest = merged.join(VOLUME_POSIX_METADATA_FILE);
        let write_mode = |path: &std::path::Path, mode: u32| {
            let manifest = VolumePosixManifest {
                schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
                mounts: vec![VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(mode)],
                    retained: false,
                }],
            };
            std::fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        };
        write_mode(&guest_root, 0o640);
        write_mode(&upper_manifest, 0o755);
        write_mode(&merged_manifest, 0o755);
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&sidecar_path, 5);
        set_mtime(&upper_manifest, 10);
        set_mtime(&merged_manifest, 20);
        set_mtime(&guest_root, 50);

        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), volume)]).unwrap();
        let staged: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&guest_root).unwrap()).unwrap();
        assert_eq!(staged.mounts[0].entries[0].mode, 0o640);
        assert!(!upper_manifest.exists());
        assert!(!merged_manifest.exists());
    }

    #[test]
    fn stage_removes_a_pending_replay_when_the_manifest_is_retired() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), volume)]).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let root_pending = rootfs.join(VOLUME_POSIX_METADATA_PENDING_FILE);
        let upper_pending = upper.join(VOLUME_POSIX_METADATA_PENDING_FILE);
        std::fs::write(&root_pending, b"pending").unwrap();
        std::fs::write(&upper_pending, b"pending").unwrap();
        let root_scratch = rootfs.join(VOLUME_POSIX_METADATA_PENDING_TEMP_FILE);
        let upper_scratch = upper.join(VOLUME_POSIX_METADATA_PENDING_TEMP_FILE);
        std::fs::write(&root_scratch, b"scratch").unwrap();
        std::fs::write(&upper_scratch, b"scratch").unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &[]).unwrap();
        assert!(!guest_root.exists());
        assert!(!root_pending.exists());
        assert!(!upper_pending.exists());
        assert!(!root_scratch.exists());
        assert!(!upper_scratch.exists());
    }

    #[test]
    fn stage_publishes_a_newer_durable_temp_capture() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume)];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let temp = rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        std::fs::write(&temp, serde_json::to_vec(&captured).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&sidecar_path, 5);
        set_mtime(&guest_root, 10);
        set_mtime(&temp, 50);

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&temp).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
    }

    #[test]
    fn stage_leaves_a_newer_temp_for_harvest_after_replay_retires_the_manifest() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let temp = rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        std::fs::write(&temp, serde_json::to_vec(&captured).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&sidecar_path, 5);
        set_mtime(&guest_root, 10);
        set_mtime(&temp, 50);

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let newer_than_manifest = |path: &std::path::Path| {
            path.is_file()
                && path
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    > guest_root
                        .metadata()
                        .and_then(|metadata| metadata.modified())
                        .ok()
        };
        if !newer_than_manifest(&temp) {
            let _ = std::fs::remove_file(&temp);
        }
        let published = rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
        if !newer_than_manifest(&published) {
            let _ = std::fs::remove_file(&published);
        }
        let _ = std::fs::remove_file(&guest_root);

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let sidecar = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(sidecar.entries[0].mode, 0o640);
    }

    #[test]
    fn stage_keeps_a_newer_temp_when_the_manifest_cannot_be_replaced() {
        use std::os::windows::fs::OpenOptionsExt;

        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let temp = rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE);
        let write_mode = |path: &std::path::Path, mode: u32| {
            let manifest = VolumePosixManifest {
                schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
                mounts: vec![VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(mode)],
                    retained: false,
                }],
            };
            std::fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        };
        write_mode(&guest_root, 0o644);
        write_mode(&temp, 0o640);
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&guest_root, 10);
        set_mtime(&temp, 50);
        set_mtime(&sidecar_path, 100);
        let _hold = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&guest_root)
            .unwrap();

        let error = sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), volume)])
            .unwrap_err();
        assert!(!error.to_string().is_empty());
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&temp).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
    }

    #[test]
    fn stage_keeps_a_newer_publish_capture_when_the_manifest_cannot_be_replaced() {
        use std::os::windows::fs::OpenOptionsExt;

        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let temp = rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE);
        let published = rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
        let write_mode = |path: &std::path::Path, mode: u32| {
            let manifest = VolumePosixManifest {
                schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
                mounts: vec![VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(mode)],
                    retained: false,
                }],
            };
            std::fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        };
        write_mode(&guest_root, 0o644);
        write_mode(&temp, 0o640);
        write_mode(&published, 0o600);
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&guest_root, 10);
        set_mtime(&temp, 50);
        set_mtime(&published, 80);
        set_mtime(&sidecar_path, 100);
        let _hold = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&guest_root)
            .unwrap();

        let error = sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), volume)])
            .unwrap_err();
        assert!(!error.to_string().is_empty());
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&published).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o600);
        let durable: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&temp).unwrap()).unwrap();
        assert_eq!(durable.mounts[0].entries[0].mode, 0o640);
    }

    #[test]
    fn harvest_reads_a_valid_manifest_beside_an_oversized_publish_capture() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        std::fs::write(
            rootfs.join(VOLUME_POSIX_METADATA_FILE),
            serde_json::to_vec(&captured).unwrap(),
        )
        .unwrap();
        let publish = rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
        let file = std::fs::File::create(&publish).unwrap();
        file.set_len(VOLUME_POSIX_METADATA_MAX_BYTES + 1).unwrap();
        drop(file);

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let sidecar = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(sidecar.entries[0].mode, 0o640);
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
    }

    #[test]
    fn harvest_reads_a_valid_manifest_beside_an_oversized_file_in_another_root() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        std::fs::write(
            rootfs.join(VOLUME_POSIX_METADATA_FILE),
            serde_json::to_vec(&captured).unwrap(),
        )
        .unwrap();
        let upper_manifest = upper.join(VOLUME_POSIX_METADATA_FILE);
        let file = std::fs::File::create(&upper_manifest).unwrap();
        file.set_len(VOLUME_POSIX_METADATA_MAX_BYTES + 1).unwrap();
        drop(file);

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let sidecar = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(sidecar.entries[0].mode, 0o640);
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
    }

    #[test]
    fn stage_replaces_an_oversized_manifest_when_another_root_is_valid() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let _ = std::fs::remove_file(&guest_root);
        let oversized = std::fs::File::create(&guest_root).unwrap();
        oversized
            .set_len(VOLUME_POSIX_METADATA_MAX_BYTES + 1)
            .unwrap();
        drop(oversized);
        let upper_manifest = upper.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        std::fs::write(&upper_manifest, serde_json::to_vec(&captured).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&sidecar_path, 5);
        set_mtime(&upper_manifest, 50);

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let guest_len = guest_root
            .metadata()
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        assert!(guest_len <= VOLUME_POSIX_METADATA_MAX_BYTES);
        let readable = if guest_root.is_file() && guest_len > 0 {
            guest_root
        } else {
            rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE)
        };
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&readable).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
    }

    #[test]
    fn harvest_reads_a_valid_manifest_beside_a_directory_in_another_root() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        std::fs::write(
            rootfs.join(VOLUME_POSIX_METADATA_FILE),
            serde_json::to_vec(&captured).unwrap(),
        )
        .unwrap();
        std::fs::create_dir(upper.join(VOLUME_POSIX_METADATA_FILE)).unwrap();

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let sidecar = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(sidecar.entries[0].mode, 0o640);
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        assert!(!upper.join(VOLUME_POSIX_METADATA_FILE).exists());
    }

    #[test]
    fn stage_removes_a_non_regular_manifest_when_another_root_is_valid() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::remove_file(&guest_root).unwrap();
        std::fs::create_dir(&guest_root).unwrap();
        let upper_manifest = upper.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        std::fs::write(&upper_manifest, serde_json::to_vec(&captured).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&sidecar_path, 5);
        set_mtime(&upper_manifest, 50);

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        assert!(!guest_root.is_dir());
        let readable = if guest_root.is_file() {
            guest_root
        } else {
            rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE)
        };
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&readable).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
    }

    #[test]
    fn stage_publishes_beside_a_nonempty_committed_manifest_directory() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();

        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let previous = std::fs::read(&guest_root).unwrap();
        std::fs::remove_file(&guest_root).unwrap();
        std::fs::create_dir(&guest_root).unwrap();
        let marker = guest_root.join("keep.txt");
        std::fs::write(&marker, b"keep").unwrap();
        let temp = rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE);
        std::fs::write(&temp, previous).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&temp, 10);
        set_mtime(&sidecar_path, 50);

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        assert!(guest_root.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
        let durable: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&temp).unwrap()).unwrap();
        assert_eq!(durable.mounts[0].entries[0].mode, 0o755);
        let published: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(published.mounts[0].entries[0].mode, 0o640);
    }

    #[test]
    fn stage_keeps_a_nonempty_committed_manifest_directory() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::remove_file(&guest_root).unwrap();
        std::fs::create_dir(&guest_root).unwrap();
        let marker = guest_root.join("keep.txt");
        std::fs::write(&marker, b"keep").unwrap();
        let upper_manifest = upper.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        std::fs::write(&upper_manifest, serde_json::to_vec(&captured).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&sidecar_path, 5);
        set_mtime(&upper_manifest, 50);

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        assert!(guest_root.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
        let published = rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&published).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
    }

    #[test]
    fn stage_retires_the_manifest_beside_a_nonempty_committed_directory() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        let mounts = [("/data".to_string(), volume)];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::remove_file(&guest_root).unwrap();
        std::fs::create_dir(&guest_root).unwrap();
        let marker = guest_root.join("keep.txt");
        std::fs::write(&marker, b"keep").unwrap();
        let upper_manifest = upper.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        std::fs::write(&upper_manifest, serde_json::to_vec(&captured).unwrap()).unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &[]).unwrap();
        assert!(guest_root.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
        assert!(!upper_manifest.exists());
        assert!(!rootfs
            .join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE)
            .exists());
        assert!(!rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE).exists());
        assert!(!box_dir.join(VOLUME_POSIX_BINDINGS_FILE).exists());
    }

    #[test]
    fn stage_flushes_a_retired_temp_capture_beside_a_nonempty_committed_directory() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::remove_file(&guest_root).unwrap();
        std::fs::create_dir(&guest_root).unwrap();
        let marker = guest_root.join("keep.txt");
        std::fs::write(&marker, b"keep").unwrap();
        let temp = rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        std::fs::write(&temp, serde_json::to_vec(&captured).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&sidecar_path, 5);
        set_mtime(&temp, 50);

        sync_managed_volume_posix(&box_dir, &rootfs, &[]).unwrap();
        assert!(guest_root.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
        assert!(!temp.exists());
        assert!(!rootfs
            .join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE)
            .exists());
        let sidecar = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(sidecar.entries[0].mode, 0o640);
    }

    #[test]
    fn stage_drops_a_retired_path_from_a_newer_temp_capture() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let temp = rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o640)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/etc".to_string(),
                    entries: vec![entry(0o600)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&temp, serde_json::to_vec(&captured).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&sidecar_path, 5);
        set_mtime(&guest_root, 10);
        set_mtime(&temp, 50);

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let mut saw_data = false;
        for path in [
            &guest_root,
            &temp,
            &rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE),
        ] {
            if !path.is_file() {
                continue;
            }
            let manifest: VolumePosixManifest =
                serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            assert!(
                manifest
                    .mounts
                    .iter()
                    .all(|mount| mount.guest_path != "/etc"),
                "{}",
                path.display()
            );
            if manifest.mounts.iter().any(|mount| {
                mount.guest_path == "/data"
                    && mount
                        .entries
                        .first()
                        .is_some_and(|entry| entry.mode == 0o640)
            }) {
                saw_data = true;
            }
        }
        assert!(saw_data);
    }

    #[test]
    fn stage_drops_a_retired_path_from_a_newer_committed_manifest() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o640)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/etc".to_string(),
                    entries: vec![entry(0o600)],
                    retained: false,
                },
            ],
        };
        let bytes = serde_json::to_vec(&captured).unwrap();
        std::fs::write(&guest_root, &bytes).unwrap();
        let temp = rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE);
        std::fs::write(&temp, &bytes).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&sidecar_path, 5);
        set_mtime(&guest_root, 4_000_000_000);
        set_mtime(&temp, 4_000_000_010);

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let mut newest: Option<(SystemTime, VolumePosixManifest)> = None;
        for name in [
            VOLUME_POSIX_METADATA_FILE,
            VOLUME_POSIX_METADATA_TEMP_FILE,
            VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE,
        ] {
            let path = rootfs.join(name);
            if !path.is_file() {
                continue;
            }
            let manifest: VolumePosixManifest =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let modified = path.metadata().unwrap().modified().unwrap();
            let replace = newest
                .as_ref()
                .map(|(time, _)| modified > *time)
                .unwrap_or(true);
            if replace {
                newest = Some((modified, manifest));
            }
        }
        let manifest = newest.unwrap().1;
        assert!(
            manifest
                .mounts
                .iter()
                .all(|mount| mount.guest_path != "/etc"),
            "{manifest:?}"
        );
        assert_eq!(
            manifest
                .mounts
                .iter()
                .find(|mount| mount.guest_path == "/data")
                .unwrap()
                .entries[0]
                .mode,
            0o640
        );
    }

    #[test]
    fn stage_flushes_an_unmounted_volume_when_the_host_path_steps_through_a_child() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        let decoy = volume.join("foo");
        std::fs::create_dir_all(&decoy).unwrap();
        let alias = decoy.join("..");
        assert_ne!(volume, alias);
        assert_eq!(
            std::fs::canonicalize(&volume).unwrap(),
            std::fs::canonicalize(&alias).unwrap()
        );
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(&box_dir, &rootfs, &[("/data".to_string(), alias)]).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o640)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&guest_root, serde_json::to_vec(&captured).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&sidecar_path, 5);
        set_mtime(&guest_root, 50);

        sync_managed_volume_posix(&box_dir, &rootfs, &[]).unwrap();
        let sidecar = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(sidecar.entries[0].mode, 0o640);
    }

    #[test]
    fn stage_flushes_an_unmounted_volume_when_workspace_stays() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o640)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&guest_root, serde_json::to_vec(&captured).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&sidecar_path, 5);
        set_mtime(&guest_root, 50);

        sync_managed_volume_posix(&box_dir, &rootfs, &[]).unwrap();
        let sidecar = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(sidecar.entries[0].mode, 0o640);
        let mut newest: Option<(SystemTime, VolumePosixManifest)> = None;
        for name in [
            VOLUME_POSIX_METADATA_FILE,
            VOLUME_POSIX_METADATA_TEMP_FILE,
            VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE,
        ] {
            let path = rootfs.join(name);
            if !path.is_file() {
                continue;
            }
            let manifest: VolumePosixManifest =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let modified = path.metadata().unwrap().modified().unwrap();
            let replace = newest
                .as_ref()
                .map(|(time, _)| modified > *time)
                .unwrap_or(true);
            if replace {
                newest = Some((modified, manifest));
            }
        }
        let manifest = newest.unwrap().1;
        let paths: Vec<&str> = manifest
            .mounts
            .iter()
            .map(|mount| mount.guest_path.as_str())
            .collect();
        assert_eq!(paths, vec!["/workspace"]);
    }

    #[test]
    fn stage_recovers_an_unmounted_volume_from_an_older_capture() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o640)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry(0o700)],
                    retained: false,
                },
            ],
        };
        std::fs::write(&guest_root, serde_json::to_vec(&captured).unwrap()).unwrap();
        let workspace_only = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/workspace".to_string(),
                entries: vec![entry(0o700)],
                retained: false,
            }],
        };
        let temp = rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE);
        std::fs::write(&temp, serde_json::to_vec(&workspace_only).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&sidecar_path, 5);
        set_mtime(&guest_root, 50);
        set_mtime(&temp, 100);

        sync_managed_volume_posix(&box_dir, &rootfs, &[]).unwrap();
        let sidecar = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(sidecar.entries[0].mode, 0o640);
        let mut newest: Option<(SystemTime, VolumePosixManifest)> = None;
        for name in [
            VOLUME_POSIX_METADATA_FILE,
            VOLUME_POSIX_METADATA_TEMP_FILE,
            VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE,
        ] {
            let path = rootfs.join(name);
            if !path.is_file() {
                continue;
            }
            let manifest: VolumePosixManifest =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let modified = path.metadata().unwrap().modified().unwrap();
            let replace = newest
                .as_ref()
                .map(|(time, _)| modified > *time)
                .unwrap_or(true);
            if replace {
                newest = Some((modified, manifest));
            }
        }
        let manifest = newest.unwrap().1;
        let paths: Vec<&str> = manifest
            .mounts
            .iter()
            .map(|mount| mount.guest_path.as_str())
            .collect();
        assert_eq!(paths, vec!["/workspace"]);
    }

    #[test]
    fn stage_recovers_a_nested_volume_without_parent_entries() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let workspace = volumes.join("workspace");
        let cache = volumes.join("cache");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = vec![
            ("/workspace".to_string(), workspace.clone()),
            ("/workspace/cache".to_string(), cache.clone()),
        ];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let encode =
            |path: &str| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, path);
        let entry = |path: &str, mode: u32| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: a3s_box_core::rootfs_metadata::RootfsEntryKind::Regular,
            mode,
            uid: 1000,
            gid: 1000,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        };
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry("note.txt", 0o644), entry("cache/secret.txt", 0o700)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace/cache".to_string(),
                    entries: vec![entry("secret.txt", 0o600)],
                    retained: false,
                },
            ],
        };
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&guest_root, serde_json::to_vec(&captured).unwrap()).unwrap();
        let omitted = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: Vec::new(),
        };
        let temp = rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE);
        std::fs::write(&temp, serde_json::to_vec(&omitted).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        for volume in [&workspace, &cache] {
            let sidecar = volume_posix_sidecar_path(volume).unwrap();
            if sidecar.is_file() {
                set_mtime(&sidecar, 5);
            }
        }
        set_mtime(&guest_root, 50);
        set_mtime(&temp, 100);

        sync_managed_volume_posix(&box_dir, &rootfs, &[]).unwrap();
        let parent = read_volume_posix_sidecar(&workspace).unwrap().unwrap();
        let nested = read_volume_posix_sidecar(&cache).unwrap().unwrap();
        assert!(parent
            .entries
            .iter()
            .any(|entry| entry.path_base64 == encode("note.txt")));
        assert!(parent
            .entries
            .iter()
            .all(|entry| entry.path_base64 != encode("cache/secret.txt")));
        assert_eq!(nested.entries[0].path_base64, encode("secret.txt"));
        assert_eq!(nested.entries[0].mode, 0o600);
    }

    #[test]
    fn stage_drops_an_unbound_newer_temp_that_lists_nested_entries_on_an_alias() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let workspace = volumes.join("workspace");
        let cache = volumes.join("cache");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        assert!(!box_dir.join(VOLUME_POSIX_BINDINGS_FILE).exists());
        let encode =
            |path: &str| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, path);
        let file = |path: &str, mode: u32| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: a3s_box_core::rootfs_metadata::RootfsEntryKind::Regular,
            mode,
            uid: 1000,
            gid: 1000,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        };
        let committed = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![file("note.txt", 0o644)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/backup".to_string(),
                    entries: vec![file("note.txt", 0o644)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace/cache".to_string(),
                    entries: vec![file("secret.txt", 0o600)],
                    retained: false,
                },
            ],
        };
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&guest_root, serde_json::to_vec(&committed).unwrap()).unwrap();
        let newer = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![file("note.txt", 0o644)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/backup".to_string(),
                    entries: vec![file("note.txt", 0o644), file("cache/secret.txt", 0o700)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace/cache".to_string(),
                    entries: vec![file("secret.txt", 0o600)],
                    retained: false,
                },
            ],
        };
        let temp = rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE);
        std::fs::write(&temp, serde_json::to_vec(&newer).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&guest_root, 3_000_000_000);
        set_mtime(&temp, 4_000_000_000);
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/workspace".to_string(), workspace.clone()),
                ("/backup".to_string(), workspace.clone()),
                ("/workspace/cache".to_string(), cache.clone()),
            ],
        )
        .unwrap();
        let (_, manifest) = read_guest_manifest(&box_dir).unwrap().unwrap();
        for guest_path in ["/workspace", "/backup"] {
            let mount = manifest
                .mounts
                .iter()
                .find(|mount| mount.guest_path == guest_path)
                .unwrap();
            assert!(
                mount
                    .entries
                    .iter()
                    .any(|item| item.path_base64 == encode("note.txt")),
                "{guest_path}"
            );
            assert!(
                mount
                    .entries
                    .iter()
                    .all(|item| item.path_base64 != encode("cache/secret.txt")),
                "{guest_path} {mount:?}"
            );
        }
        let nested = manifest
            .mounts
            .iter()
            .find(|mount| mount.guest_path == "/workspace/cache")
            .unwrap();
        assert_eq!(nested.entries[0].path_base64, encode("secret.txt"));
        assert_eq!(nested.entries[0].mode, 0o600);
    }

    #[test]
    fn stage_drops_nested_entries_when_alias_host_paths_differ_by_case() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let workspace = volumes.join("workspace");
        let cache = volumes.join("cache");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let backup_host = volumes.join("Workspace");
        assert_ne!(workspace, backup_host);
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let encode =
            |path: &str| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, path);
        let file = |path: &str, mode: u32| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: a3s_box_core::rootfs_metadata::RootfsEntryKind::Regular,
            mode,
            uid: 1000,
            gid: 1000,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        };
        let newer = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![file("note.txt", 0o644)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/backup".to_string(),
                    entries: vec![file("note.txt", 0o644), file("cache/secret.txt", 0o700)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace/cache".to_string(),
                    entries: vec![file("secret.txt", 0o600)],
                    retained: false,
                },
            ],
        };
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let temp = rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE);
        std::fs::write(&guest_root, serde_json::to_vec(&newer).unwrap()).unwrap();
        std::fs::write(&temp, serde_json::to_vec(&newer).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&guest_root, 3_000_000_000);
        set_mtime(&temp, 4_000_000_000);
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/workspace".to_string(), workspace),
                ("/backup".to_string(), backup_host),
                ("/workspace/cache".to_string(), cache),
            ],
        )
        .unwrap();
        let (_, manifest) = read_guest_manifest(&box_dir).unwrap().unwrap();
        for guest_path in ["/workspace", "/backup"] {
            let mount = manifest
                .mounts
                .iter()
                .find(|mount| mount.guest_path == guest_path)
                .unwrap();
            assert!(
                mount
                    .entries
                    .iter()
                    .any(|item| item.path_base64 == encode("note.txt")),
                "{guest_path}"
            );
            assert!(
                mount
                    .entries
                    .iter()
                    .all(|item| item.path_base64 != encode("cache/secret.txt")),
                "{guest_path} {mount:?}"
            );
        }
        let nested = manifest
            .mounts
            .iter()
            .find(|mount| mount.guest_path == "/workspace/cache")
            .unwrap();
        assert_eq!(nested.entries[0].path_base64, encode("secret.txt"));
        assert_eq!(nested.entries[0].mode, 0o600);
    }

    #[test]
    fn stage_drops_a_newer_temp_that_lists_nested_entries_on_a_replay_alias() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let workspace = volumes.join("workspace");
        let cache = volumes.join("cache");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let managed = [("/workspace/cache".to_string(), cache.clone())];
        let replay = [
            ("/workspace".to_string(), workspace.clone()),
            ("/backup".to_string(), workspace.clone()),
        ];
        sync_volume_posix(&box_dir, &rootfs, &managed, &replay).unwrap();
        let encode =
            |path: &str| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, path);
        let file = |path: &str, mode: u32| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: a3s_box_core::rootfs_metadata::RootfsEntryKind::Regular,
            mode,
            uid: 1000,
            gid: 1000,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        };
        let committed = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![file("note.txt", 0o644)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/backup".to_string(),
                    entries: vec![file("note.txt", 0o644)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace/cache".to_string(),
                    entries: vec![file("secret.txt", 0o600)],
                    retained: false,
                },
            ],
        };
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&guest_root, serde_json::to_vec(&committed).unwrap()).unwrap();
        let newer = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![file("note.txt", 0o644)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/backup".to_string(),
                    entries: vec![file("note.txt", 0o644), file("cache/secret.txt", 0o700)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace/cache".to_string(),
                    entries: vec![file("secret.txt", 0o600)],
                    retained: false,
                },
            ],
        };
        let temp = rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE);
        std::fs::write(&temp, serde_json::to_vec(&newer).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&guest_root, 3_000_000_000);
        set_mtime(&temp, 4_000_000_000);
        sync_volume_posix(&box_dir, &rootfs, &managed, &replay).unwrap();
        let (_, manifest) = read_guest_manifest(&box_dir).unwrap().unwrap();
        for guest_path in ["/workspace", "/backup"] {
            let mount = manifest
                .mounts
                .iter()
                .find(|mount| mount.guest_path == guest_path)
                .unwrap();
            assert!(
                mount
                    .entries
                    .iter()
                    .any(|item| item.path_base64 == encode("note.txt")),
                "{guest_path}"
            );
            assert!(
                mount
                    .entries
                    .iter()
                    .all(|item| item.path_base64 != encode("cache/secret.txt")),
                "{guest_path} {mount:?}"
            );
        }
        let nested = manifest
            .mounts
            .iter()
            .find(|mount| mount.guest_path == "/workspace/cache")
            .unwrap();
        assert_eq!(nested.entries[0].path_base64, encode("secret.txt"));
        assert_eq!(nested.entries[0].mode, 0o600);
    }

    #[test]
    fn stage_drops_a_newer_temp_that_lists_nested_entries_on_an_alias() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let workspace = volumes.join("workspace");
        let cache = volumes.join("cache");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/workspace".to_string(), workspace.clone()),
                ("/backup".to_string(), workspace.clone()),
                ("/workspace/cache".to_string(), cache.clone()),
            ],
        )
        .unwrap();
        let encode =
            |path: &str| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, path);
        let file = |path: &str, mode: u32| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: a3s_box_core::rootfs_metadata::RootfsEntryKind::Regular,
            mode,
            uid: 1000,
            gid: 1000,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        };
        let committed = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![file("note.txt", 0o644)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/backup".to_string(),
                    entries: vec![file("note.txt", 0o644)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace/cache".to_string(),
                    entries: vec![file("secret.txt", 0o600)],
                    retained: false,
                },
            ],
        };
        let newer = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/backup".to_string(),
                    entries: vec![file("note.txt", 0o644), file("cache/secret.txt", 0o700)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![file("note.txt", 0o644)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace/cache".to_string(),
                    entries: vec![file("secret.txt", 0o600)],
                    retained: false,
                },
            ],
        };
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let temp = rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE);
        std::fs::write(&guest_root, serde_json::to_vec(&committed).unwrap()).unwrap();
        std::fs::write(&temp, serde_json::to_vec(&newer).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&guest_root, 3_000_000_000);
        set_mtime(&temp, 4_000_000_000);

        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/workspace".to_string(), workspace.clone()),
                ("/backup".to_string(), workspace),
                ("/workspace/cache".to_string(), cache),
            ],
        )
        .unwrap();
        let (_, manifest) = read_guest_manifest(&box_dir).unwrap().unwrap();
        for guest_path in ["/workspace", "/backup"] {
            let mount = manifest
                .mounts
                .iter()
                .find(|mount| mount.guest_path == guest_path)
                .unwrap();
            assert!(
                mount
                    .entries
                    .iter()
                    .any(|item| item.path_base64 == encode("note.txt")),
                "{guest_path}"
            );
            assert!(
                mount
                    .entries
                    .iter()
                    .all(|item| item.path_base64 != encode("cache/secret.txt")),
                "{guest_path} {mount:?}"
            );
        }
        let nested = manifest
            .mounts
            .iter()
            .find(|mount| mount.guest_path == "/workspace/cache")
            .unwrap();
        assert_eq!(nested.entries[0].path_base64, encode("secret.txt"));
        assert_eq!(nested.entries[0].mode, 0o600);
    }

    #[test]
    fn stage_drops_nested_entries_when_recovering_a_dropped_alias() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let workspace = volumes.join("workspace");
        let cache = volumes.join("cache");
        let data = volumes.join("data");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/workspace".to_string(), workspace.clone()),
                ("/backup".to_string(), workspace.clone()),
                ("/workspace/cache".to_string(), cache.clone()),
                ("/data".to_string(), data.clone()),
                ("/mnt/data".to_string(), data.clone()),
            ],
        )
        .unwrap();
        let encode =
            |path: &str| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, path);
        let file = |path: &str, mode: u32| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: a3s_box_core::rootfs_metadata::RootfsEntryKind::Regular,
            mode,
            uid: 1000,
            gid: 1000,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        };
        let newest = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![file("note.txt", 0o755)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/mnt/data".to_string(),
                    entries: vec![file("note.txt", 0o700)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![file("note.txt", 0o644)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace/cache".to_string(),
                    entries: vec![file("secret.txt", 0o600)],
                    retained: false,
                },
            ],
        };
        let older = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/backup".to_string(),
                entries: vec![file("note.txt", 0o644), file("cache/secret.txt", 0o700)],
                retained: false,
            }],
        };
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let temp = rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE);
        std::fs::write(&guest_root, serde_json::to_vec(&newest).unwrap()).unwrap();
        std::fs::write(&temp, serde_json::to_vec(&older).unwrap()).unwrap();
        write_volume_posix_sidecar(
            &workspace,
            &VolumePosixSidecar::new(vec![file("note.txt", 0o640)]),
        )
        .unwrap();
        write_volume_posix_sidecar(
            &cache,
            &VolumePosixSidecar::new(vec![file("secret.txt", 0o600)]),
        )
        .unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&volume_posix_sidecar_path(&workspace).unwrap(), 10);
        set_mtime(&volume_posix_sidecar_path(&cache).unwrap(), 5_000_000_000);
        set_mtime(&temp, 3_000_000_000);
        set_mtime(&guest_root, 4_000_000_000);

        let error = sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[
                ("/workspace".to_string(), workspace.clone()),
                ("/workspace/cache".to_string(), cache.clone()),
                ("/data".to_string(), data.clone()),
                ("/mnt/data".to_string(), data),
            ],
        )
        .unwrap_err();
        assert!(error.to_string().contains("disagree"), "{error}");
        let parent = read_volume_posix_sidecar(&workspace).unwrap().unwrap();
        assert!(
            parent
                .entries
                .iter()
                .any(|item| item.path_base64 == encode("note.txt") && item.mode == 0o644),
            "{parent:?}"
        );
        assert!(parent
            .entries
            .iter()
            .all(|item| item.path_base64 != encode("cache/secret.txt")));
        let nested = read_volume_posix_sidecar(&cache).unwrap().unwrap();
        assert_eq!(nested.entries[0].path_base64, encode("secret.txt"));
        assert_eq!(nested.entries[0].mode, 0o600);
    }

    #[test]
    fn stage_recovers_a_dropped_parent_without_a_newly_mounted_nested_volume() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let workspace = volumes.join("workspace");
        let cache = volumes.join("cache");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[("/workspace".to_string(), workspace.clone())],
        )
        .unwrap();
        let encode =
            |path: &str| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, path);
        let entry = |path: &str, mode: u32| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: a3s_box_core::rootfs_metadata::RootfsEntryKind::Regular,
            mode,
            uid: 1000,
            gid: 1000,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        };
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/workspace".to_string(),
                entries: vec![entry("note.txt", 0o644), entry("cache/secret.txt", 0o700)],
                retained: false,
            }],
        };
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&guest_root, serde_json::to_vec(&captured).unwrap()).unwrap();
        let nested_only = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/workspace/cache".to_string(),
                entries: vec![entry("secret.txt", 0o600)],
                retained: false,
            }],
        };
        let temp = rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE);
        std::fs::write(&temp, serde_json::to_vec(&nested_only).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        let sidecar = volume_posix_sidecar_path(&workspace).unwrap();
        if sidecar.is_file() {
            set_mtime(&sidecar, 5);
        }
        set_mtime(&guest_root, 50);
        set_mtime(&temp, 100);

        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[("/workspace/cache".to_string(), cache.clone())],
        )
        .unwrap();
        let parent = read_volume_posix_sidecar(&workspace).unwrap().unwrap();
        assert!(parent
            .entries
            .iter()
            .any(|entry| entry.path_base64 == encode("note.txt")));
        assert!(parent
            .entries
            .iter()
            .all(|entry| entry.path_base64 != encode("cache/secret.txt")));
    }

    #[test]
    fn stage_drops_nested_entries_when_harvesting_a_dropped_parent() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let workspace = volumes.join("workspace");
        let cache = volumes.join("cache");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[("/workspace".to_string(), workspace.clone())],
        )
        .unwrap();
        let encode =
            |path: &str| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, path);
        let entry = |path: &str, mode: u32| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: a3s_box_core::rootfs_metadata::RootfsEntryKind::Regular,
            mode,
            uid: 1000,
            gid: 1000,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        };
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/workspace".to_string(),
                entries: vec![entry("note.txt", 0o644), entry("cache/secret.txt", 0o700)],
                retained: false,
            }],
        };
        let temp = rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE);
        std::fs::write(&temp, serde_json::to_vec(&captured).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        let sidecar = volume_posix_sidecar_path(&workspace).unwrap();
        if sidecar.is_file() {
            set_mtime(&sidecar, 5);
        }
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        if guest_root.is_file() {
            set_mtime(&guest_root, 50);
        }
        set_mtime(&temp, 4_000_000_000);

        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[("/workspace/cache".to_string(), cache.clone())],
        )
        .unwrap();
        let parent = read_volume_posix_sidecar(&workspace).unwrap().unwrap();
        assert!(parent
            .entries
            .iter()
            .any(|entry| entry.path_base64 == encode("note.txt")));
        assert!(parent
            .entries
            .iter()
            .all(|entry| entry.path_base64 != encode("cache/secret.txt")));
    }

    #[test]
    fn stage_drops_nested_entries_when_the_newest_capture_omits_the_nested_mount() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let workspace = volumes.join("workspace");
        let cache = volumes.join("cache");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = vec![
            ("/workspace".to_string(), workspace.clone()),
            ("/workspace/cache".to_string(), cache.clone()),
        ];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let encode =
            |path: &str| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, path);
        let entry = |path: &str, mode: u32| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: a3s_box_core::rootfs_metadata::RootfsEntryKind::Regular,
            mode,
            uid: 1000,
            gid: 1000,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        };
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry("note.txt", 0o644), entry("cache/secret.txt", 0o700)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace/cache".to_string(),
                    entries: vec![entry("secret.txt", 0o600)],
                    retained: false,
                },
            ],
        };
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&guest_root, serde_json::to_vec(&captured).unwrap()).unwrap();
        let parent_only = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/workspace".to_string(),
                entries: vec![entry("note.txt", 0o644), entry("cache/secret.txt", 0o700)],
                retained: false,
            }],
        };
        let temp = rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE);
        std::fs::write(&temp, serde_json::to_vec(&parent_only).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        for volume in [&workspace, &cache] {
            let sidecar = volume_posix_sidecar_path(volume).unwrap();
            if sidecar.is_file() {
                set_mtime(&sidecar, 5);
            }
        }
        set_mtime(&guest_root, 50);
        set_mtime(&temp, 100);

        sync_managed_volume_posix(&box_dir, &rootfs, &[]).unwrap();
        let parent = read_volume_posix_sidecar(&workspace).unwrap().unwrap();
        let nested = read_volume_posix_sidecar(&cache).unwrap().unwrap();
        assert!(parent
            .entries
            .iter()
            .any(|entry| entry.path_base64 == encode("note.txt")));
        assert!(parent
            .entries
            .iter()
            .all(|entry| entry.path_base64 != encode("cache/secret.txt")));
        assert_eq!(nested.entries[0].path_base64, encode("secret.txt"));
        assert_eq!(nested.entries[0].mode, 0o600);
        for name in [
            VOLUME_POSIX_METADATA_FILE,
            VOLUME_POSIX_METADATA_TEMP_FILE,
            VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE,
        ] {
            let path = rootfs.join(name);
            if !path.is_file() {
                continue;
            }
            let manifest: VolumePosixManifest =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            for mount in &manifest.mounts {
                assert!(mount
                    .entries
                    .iter()
                    .all(|entry| entry.path_base64 != encode("cache/secret.txt")));
            }
        }
    }

    #[test]
    fn stage_drops_nested_entries_from_a_newer_caller_capture() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let cache = volumes.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let caller = home.path().join("caller-workspace");
        std::fs::create_dir_all(&caller).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        sync_managed_volume_posix(
            &box_dir,
            &rootfs,
            &[("/workspace/cache".to_string(), cache.clone())],
        )
        .unwrap();
        let encode =
            |path: &str| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, path);
        let entry = |path: &str, mode: u32| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: a3s_box_core::rootfs_metadata::RootfsEntryKind::Regular,
            mode,
            uid: 1000,
            gid: 1000,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        };
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/workspace".to_string(),
                    entries: vec![entry("note.txt", 0o644), entry("cache/secret.txt", 0o700)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/workspace/cache".to_string(),
                    entries: vec![entry("secret.txt", 0o600)],
                    retained: false,
                },
            ],
        };
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&guest_root, serde_json::to_vec(&captured).unwrap()).unwrap();
        let caller_only = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/workspace".to_string(),
                entries: vec![entry("note.txt", 0o644), entry("cache/secret.txt", 0o700)],
                retained: false,
            }],
        };
        let temp = rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE);
        std::fs::write(&temp, serde_json::to_vec(&caller_only).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        let sidecar = volume_posix_sidecar_path(&cache).unwrap();
        if sidecar.is_file() {
            set_mtime(&sidecar, 5);
        }
        set_mtime(&guest_root, 50);
        set_mtime(&temp, 4_000_000_000);

        sync_volume_posix(
            &box_dir,
            &rootfs,
            &[],
            &[("/workspace".to_string(), caller)],
        )
        .unwrap();
        let nested = read_volume_posix_sidecar(&cache).unwrap().unwrap();
        assert_eq!(nested.entries[0].path_base64, encode("secret.txt"));
        assert_eq!(nested.entries[0].mode, 0o600);
        let mut newest: Option<(SystemTime, VolumePosixManifest)> = None;
        for name in [
            VOLUME_POSIX_METADATA_FILE,
            VOLUME_POSIX_METADATA_TEMP_FILE,
            VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE,
        ] {
            let path = rootfs.join(name);
            if !path.is_file() {
                continue;
            }
            let manifest: VolumePosixManifest =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let modified = path.metadata().unwrap().modified().unwrap();
            let replace = newest
                .as_ref()
                .map(|(time, _)| modified > *time)
                .unwrap_or(true);
            if replace {
                newest = Some((modified, manifest));
            }
        }
        let guest = newest.unwrap().1;
        let workspace_mount = guest
            .mounts
            .iter()
            .find(|mount| mount.guest_path == "/workspace")
            .unwrap();
        assert!(workspace_mount
            .entries
            .iter()
            .any(|entry| entry.path_base64 == encode("note.txt")));
        assert!(workspace_mount
            .entries
            .iter()
            .all(|entry| entry.path_base64 != encode("cache/secret.txt")));
    }

    #[test]
    fn stage_drops_nested_entries_when_the_nested_volume_is_first_mounted() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let cache = volumes.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let caller = home.path().join("caller-workspace");
        std::fs::create_dir_all(&caller).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let encode =
            |path: &str| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, path);
        let entry = |path: &str, mode: u32| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: a3s_box_core::rootfs_metadata::RootfsEntryKind::Regular,
            mode,
            uid: 1000,
            gid: 1000,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        };
        let caller_only = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/workspace".to_string(),
                entries: vec![entry("note.txt", 0o644), entry("cache/secret.txt", 0o700)],
                retained: false,
            }],
        };
        let temp = rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE);
        std::fs::write(&temp, serde_json::to_vec(&caller_only).unwrap()).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&temp)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(4_000_000_000))
            .unwrap();

        sync_volume_posix(
            &box_dir,
            &rootfs,
            &[("/workspace/cache".to_string(), cache)],
            &[("/workspace".to_string(), caller)],
        )
        .unwrap();
        let mut newest: Option<(SystemTime, VolumePosixManifest)> = None;
        for name in [
            VOLUME_POSIX_METADATA_FILE,
            VOLUME_POSIX_METADATA_TEMP_FILE,
            VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE,
        ] {
            let path = rootfs.join(name);
            if !path.is_file() {
                continue;
            }
            let manifest: VolumePosixManifest =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let modified = path.metadata().unwrap().modified().unwrap();
            let replace = newest
                .as_ref()
                .map(|(time, _)| modified > *time)
                .unwrap_or(true);
            if replace {
                newest = Some((modified, manifest));
            }
        }
        let guest = newest.unwrap().1;
        let workspace_mount = guest
            .mounts
            .iter()
            .find(|mount| mount.guest_path == "/workspace")
            .unwrap();
        assert!(workspace_mount
            .entries
            .iter()
            .any(|entry| entry.path_base64 == encode("note.txt")));
        assert!(workspace_mount
            .entries
            .iter()
            .all(|entry| entry.path_base64 != encode("cache/secret.txt")));
    }

    #[test]
    fn non_regular_committed_manifest_is_still_an_error() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let manifest = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::remove_file(&manifest).unwrap();
        std::fs::create_dir(&manifest).unwrap();

        let error = harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap_err();
        assert!(
            error.to_string().contains("is not a regular file"),
            "{error}"
        );
        let staged = sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap_err();
        assert!(
            staged.to_string().contains("is not a regular file"),
            "{staged}"
        );
        assert!(manifest.is_dir());
    }

    #[test]
    fn stage_rewrites_an_empty_bindings_directory() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&guest_root, serde_json::to_vec(&captured).unwrap()).unwrap();
        let bindings = box_dir.join(VOLUME_POSIX_BINDINGS_FILE);
        std::fs::remove_file(&bindings).unwrap();
        std::fs::create_dir(&bindings).unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&guest_root).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        let written: VolumePosixBindings =
            serde_json::from_slice(&std::fs::read(&bindings).unwrap()).unwrap();
        assert_eq!(written.mounts[0].guest_path, "/data");
    }

    #[test]
    fn stage_replaces_an_oversized_bindings_file() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&guest_root, serde_json::to_vec(&captured).unwrap()).unwrap();
        let bindings = box_dir.join(VOLUME_POSIX_BINDINGS_FILE);
        let file = std::fs::File::create(&bindings).unwrap();
        file.set_len(VOLUME_POSIX_METADATA_MAX_BYTES + 1).unwrap();
        drop(file);

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&guest_root).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        let written: VolumePosixBindings =
            serde_json::from_slice(&std::fs::read(&bindings).unwrap()).unwrap();
        assert_eq!(written.mounts[0].guest_path, "/data");
        assert!(bindings.metadata().unwrap().len() <= VOLUME_POSIX_METADATA_MAX_BYTES);
    }

    #[test]
    fn stage_replaces_bindings_that_are_not_a_mount_map() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&guest_root, serde_json::to_vec(&captured).unwrap()).unwrap();
        let bindings = box_dir.join(VOLUME_POSIX_BINDINGS_FILE);
        std::fs::write(&bindings, b"not-json").unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&guest_root).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        let written: VolumePosixBindings =
            serde_json::from_slice(&std::fs::read(&bindings).unwrap()).unwrap();
        assert_eq!(written.mounts[0].guest_path, "/data");
    }

    #[test]
    fn stage_replaces_bindings_with_the_wrong_schema() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&guest_root, serde_json::to_vec(&captured).unwrap()).unwrap();
        let bindings = box_dir.join(VOLUME_POSIX_BINDINGS_FILE);
        std::fs::write(&bindings, br#"{"schema":"nope","mounts":[]}"#).unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&guest_root).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        let written: VolumePosixBindings =
            serde_json::from_slice(&std::fs::read(&bindings).unwrap()).unwrap();
        assert_eq!(written.mounts[0].guest_path, "/data");
    }

    #[test]
    fn stage_replaces_an_empty_bindings_scratch_directory() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&guest_root, serde_json::to_vec(&captured).unwrap()).unwrap();
        let bindings = box_dir.join(VOLUME_POSIX_BINDINGS_FILE);
        let scratch = bindings.with_extension("json.tmp");
        std::fs::create_dir(&scratch).unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&guest_root).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        let written: VolumePosixBindings =
            serde_json::from_slice(&std::fs::read(&bindings).unwrap()).unwrap();
        assert_eq!(written.mounts[0].guest_path, "/data");
        assert!(!scratch.exists());
    }

    #[test]
    fn stage_keeps_a_manifest_when_the_bindings_scratch_is_not_empty() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&guest_root, serde_json::to_vec(&captured).unwrap()).unwrap();
        let bindings = box_dir.join(VOLUME_POSIX_BINDINGS_FILE);
        let scratch = bindings.with_extension("json.tmp");
        std::fs::create_dir(&scratch).unwrap();
        let marker = scratch.join("keep.txt");
        std::fs::write(&marker, b"keep").unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&guest_root).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        let written: VolumePosixBindings =
            serde_json::from_slice(&std::fs::read(&bindings).unwrap()).unwrap();
        assert_eq!(written.mounts[0].guest_path, "/data");
        assert!(scratch.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
    }

    #[test]
    fn stage_keeps_a_manifest_when_the_bindings_directory_is_not_empty() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&guest_root, serde_json::to_vec(&captured).unwrap()).unwrap();
        let bindings = box_dir.join(VOLUME_POSIX_BINDINGS_FILE);
        std::fs::remove_file(&bindings).unwrap();
        std::fs::create_dir(&bindings).unwrap();
        let marker = bindings.join("keep.txt");
        std::fs::write(&marker, b"keep").unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&guest_root).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        assert!(bindings.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
    }

    #[test]
    fn stage_keeps_a_manifest_when_the_sidecar_is_not_a_regular_file() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&guest_root, serde_json::to_vec(&captured).unwrap()).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        std::fs::remove_file(&sidecar).unwrap();
        std::fs::create_dir(&sidecar).unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&guest_root).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        assert!(sidecar.is_dir());
    }

    #[test]
    fn stage_keeps_a_manifest_beside_an_oversized_sidecar() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&guest_root, serde_json::to_vec(&captured).unwrap()).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let file = std::fs::File::create(&sidecar).unwrap();
        file.set_len(VOLUME_POSIX_METADATA_MAX_BYTES + 1).unwrap();
        drop(file);

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&guest_root).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        assert!(sidecar.metadata().unwrap().len() > VOLUME_POSIX_METADATA_MAX_BYTES);
    }

    #[test]
    fn harvest_replaces_an_oversized_sidecar_with_the_manifest() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&guest_root, serde_json::to_vec(&captured).unwrap()).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let file = std::fs::File::create(&sidecar).unwrap();
        file.set_len(VOLUME_POSIX_METADATA_MAX_BYTES + 1).unwrap();
        drop(file);
        let manifest_mtime = std::fs::metadata(&guest_root).unwrap().modified().unwrap();
        std::fs::File::options()
            .write(true)
            .open(&sidecar)
            .unwrap()
            .set_modified(manifest_mtime + std::time::Duration::from_secs(50))
            .unwrap();

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let kept = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(kept.entries[0].mode, 0o640);
        assert!(sidecar.metadata().unwrap().len() <= VOLUME_POSIX_METADATA_MAX_BYTES);
    }

    #[test]
    fn harvest_skips_a_planted_sidecar_and_writes_the_sibling() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let data = volumes.join("data");
        let logs = volumes.join("logs");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&logs).unwrap();
        write_volume_posix_sidecar(&data, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        write_volume_posix_sidecar(&logs, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [
            ("/data".to_string(), data.clone()),
            ("/logs".to_string(), logs.clone()),
        ];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o640)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/logs".to_string(),
                    entries: vec![entry(0o640)],
                    retained: false,
                },
            ],
        };
        std::fs::write(
            rootfs.join(VOLUME_POSIX_METADATA_FILE),
            serde_json::to_vec(&captured).unwrap(),
        )
        .unwrap();
        let planted = volume_posix_sidecar_path(&data).unwrap();
        std::fs::remove_file(&planted).unwrap();
        std::fs::create_dir(&planted).unwrap();

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let logs_sidecar = read_volume_posix_sidecar(&logs).unwrap().unwrap();
        assert_eq!(logs_sidecar.entries[0].mode, 0o640);
        assert!(planted.is_dir());
        assert!(read_volume_posix_sidecar(&data).is_err());
    }

    #[test]
    fn harvest_replaces_an_empty_sidecar_scratch_directory() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        std::fs::write(
            rootfs.join(VOLUME_POSIX_METADATA_FILE),
            serde_json::to_vec(&captured).unwrap(),
        )
        .unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let scratch = sidecar.with_extension("json.tmp");
        std::fs::create_dir(&scratch).unwrap();

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let kept = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(kept.entries[0].mode, 0o640);
        assert!(!scratch.exists());
    }

    #[test]
    fn harvest_skips_a_nonempty_sidecar_scratch_and_writes_the_sibling() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let data = volumes.join("data");
        let logs = volumes.join("logs");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&logs).unwrap();
        write_volume_posix_sidecar(&data, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        write_volume_posix_sidecar(&logs, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [
            ("/data".to_string(), data.clone()),
            ("/logs".to_string(), logs.clone()),
        ];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![
                VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(0o640)],
                    retained: false,
                },
                VolumePosixMount {
                    guest_path: "/logs".to_string(),
                    entries: vec![entry(0o640)],
                    retained: false,
                },
            ],
        };
        std::fs::write(
            rootfs.join(VOLUME_POSIX_METADATA_FILE),
            serde_json::to_vec(&captured).unwrap(),
        )
        .unwrap();
        let sidecar = volume_posix_sidecar_path(&data).unwrap();
        let scratch = sidecar.with_extension("json.tmp");
        std::fs::create_dir(&scratch).unwrap();
        let marker = scratch.join("keep.txt");
        std::fs::write(&marker, b"keep").unwrap();

        let error = harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap_err();
        assert!(error.to_string().contains("not a writable file"), "{error}");
        let logs_sidecar = read_volume_posix_sidecar(&logs).unwrap().unwrap();
        assert_eq!(logs_sidecar.entries[0].mode, 0o640);
        let data_sidecar = read_volume_posix_sidecar(&data).unwrap().unwrap();
        assert_eq!(data_sidecar.entries[0].mode, 0o755);
        assert!(scratch.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
    }

    #[test]
    fn stage_publishes_when_the_metadata_scratch_directory_is_empty() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let scratch = rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE);
        std::fs::create_dir(&scratch).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let manifest_mtime = std::fs::metadata(&guest_root).unwrap().modified().unwrap();
        std::fs::File::options()
            .write(true)
            .open(volume_posix_sidecar_path(&volume).unwrap())
            .unwrap()
            .set_modified(manifest_mtime + std::time::Duration::from_secs(50))
            .unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&guest_root).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        assert!(!scratch.exists());
    }

    #[test]
    fn stage_publishes_when_the_metadata_scratch_directory_is_not_empty() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let scratch = rootfs.join(VOLUME_POSIX_METADATA_TEMP_FILE);
        std::fs::create_dir(&scratch).unwrap();
        let marker = scratch.join("keep.txt");
        std::fs::write(&marker, b"keep").unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let manifest_mtime = std::fs::metadata(&guest_root).unwrap().modified().unwrap();
        std::fs::File::options()
            .write(true)
            .open(volume_posix_sidecar_path(&volume).unwrap())
            .unwrap()
            .set_modified(manifest_mtime + std::time::Duration::from_secs(50))
            .unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&guest_root).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        assert!(scratch.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
    }

    #[test]
    fn stage_keeps_the_manifest_when_every_metadata_scratch_is_not_empty() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let mut markers = Vec::new();
        for name in [
            VOLUME_POSIX_METADATA_TEMP_FILE,
            VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE,
            VOLUME_POSIX_METADATA_PENDING_TEMP_FILE,
        ] {
            let scratch = rootfs.join(name);
            std::fs::create_dir(&scratch).unwrap();
            let marker = scratch.join("keep.txt");
            std::fs::write(&marker, b"keep").unwrap();
            markers.push((scratch, marker));
        }
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let manifest_mtime = std::fs::metadata(&guest_root).unwrap().modified().unwrap();
        std::fs::File::options()
            .write(true)
            .open(volume_posix_sidecar_path(&volume).unwrap())
            .unwrap()
            .set_modified(manifest_mtime + std::time::Duration::from_secs(50))
            .unwrap();

        let error = sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap_err();
        assert!(error.to_string().contains("not a writable file"), "{error}");
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&guest_root).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o755);
        for (scratch, marker) in markers {
            assert!(scratch.is_dir());
            assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
        }
    }

    #[test]
    fn stage_removes_an_empty_publish_scratch_directory() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume)];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let scratch = rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
        std::fs::create_dir(&scratch).unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(rootfs.join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        assert!(!scratch.exists());
    }

    #[test]
    fn stage_keeps_a_nonempty_publish_scratch_directory() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume)];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let scratch = rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
        std::fs::create_dir(&scratch).unwrap();
        let marker = scratch.join("keep.txt");
        std::fs::write(&marker, b"keep").unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(rootfs.join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        assert!(scratch.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
    }

    #[test]
    fn stage_removes_an_empty_pending_capture_directory() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume)];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let pending = rootfs.join(VOLUME_POSIX_METADATA_PENDING_FILE);
        std::fs::create_dir(&pending).unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(rootfs.join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        assert!(!pending.exists());
    }

    #[test]
    fn stage_keeps_a_nonempty_pending_capture_directory() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume)];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let pending = rootfs.join(VOLUME_POSIX_METADATA_PENDING_FILE);
        std::fs::create_dir(&pending).unwrap();
        let marker = pending.join("keep.txt");
        std::fs::write(&marker, b"keep").unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(rootfs.join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        assert!(pending.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
    }

    #[test]
    fn stage_removes_an_empty_pending_capture_directory_in_another_root() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        let mounts = [("/data".to_string(), volume)];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let pending = upper.join(VOLUME_POSIX_METADATA_PENDING_FILE);
        std::fs::create_dir(&pending).unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(rootfs.join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        assert!(!pending.exists());
    }

    #[test]
    fn stage_removes_an_empty_committed_manifest_directory_in_another_root() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        let mounts = [("/data".to_string(), volume)];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let planted = upper.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::create_dir(&planted).unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(rootfs.join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        assert!(!planted.exists());
    }

    #[test]
    fn stage_keeps_a_nonempty_committed_manifest_directory_in_another_root() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        let mounts = [("/data".to_string(), volume)];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let planted = upper.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::create_dir(&planted).unwrap();
        let marker = planted.join("keep.txt");
        std::fs::write(&marker, b"keep").unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(rootfs.join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        assert!(planted.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
        let published: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(published.mounts[0].entries[0].mode, 0o640);
    }

    #[test]
    fn stage_keeps_a_nonempty_pending_capture_directory_in_another_root() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        let mounts = [("/data".to_string(), volume)];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let pending = upper.join(VOLUME_POSIX_METADATA_PENDING_FILE);
        std::fs::create_dir(&pending).unwrap();
        let marker = pending.join("keep.txt");
        std::fs::write(&marker, b"keep").unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(rootfs.join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        assert!(pending.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
    }

    #[test]
    fn stage_removes_an_empty_pending_scratch_directory_in_another_root() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        let mounts = [("/data".to_string(), volume)];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let scratch = upper.join(VOLUME_POSIX_METADATA_PENDING_TEMP_FILE);
        std::fs::create_dir(&scratch).unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(rootfs.join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        assert!(!scratch.exists());
    }

    #[test]
    fn stage_keeps_a_nonempty_pending_scratch_directory_in_another_root() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        let mounts = [("/data".to_string(), volume)];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let scratch = upper.join(VOLUME_POSIX_METADATA_PENDING_TEMP_FILE);
        std::fs::create_dir(&scratch).unwrap();
        let marker = scratch.join("keep.txt");
        std::fs::write(&marker, b"keep").unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(rootfs.join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        assert!(scratch.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
    }

    #[test]
    fn stage_removes_an_empty_publish_scratch_directory_in_another_root() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        let mounts = [("/data".to_string(), volume)];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let scratch = upper.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
        std::fs::create_dir(&scratch).unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(rootfs.join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        assert!(!scratch.exists());
    }

    #[test]
    fn stage_keeps_a_nonempty_publish_scratch_directory_in_another_root() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        let mounts = [("/data".to_string(), volume)];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let scratch = upper.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
        std::fs::create_dir(&scratch).unwrap();
        let marker = scratch.join("keep.txt");
        std::fs::write(&marker, b"keep").unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(rootfs.join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        assert!(scratch.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
    }

    #[test]
    fn stage_removes_an_empty_metadata_scratch_directory_in_another_root() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        let mounts = [("/data".to_string(), volume)];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let scratch = upper.join(VOLUME_POSIX_METADATA_TEMP_FILE);
        std::fs::create_dir(&scratch).unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(rootfs.join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        assert!(!scratch.exists());
    }

    #[test]
    fn stage_keeps_a_nonempty_metadata_scratch_directory_in_another_root() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o640)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        let mounts = [("/data".to_string(), volume)];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let scratch = upper.join(VOLUME_POSIX_METADATA_TEMP_FILE);
        std::fs::create_dir(&scratch).unwrap();
        let marker = scratch.join("keep.txt");
        std::fs::write(&marker, b"keep").unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let kept: VolumePosixManifest = serde_json::from_slice(
            &std::fs::read(rootfs.join(VOLUME_POSIX_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        assert!(scratch.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
    }

    #[test]
    fn stage_copies_another_root_when_the_publish_scratch_directory_is_empty() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&guest_root, b"not-json").unwrap();
        let upper_manifest = upper.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        std::fs::write(&upper_manifest, serde_json::to_vec(&captured).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&sidecar_path, 5);
        set_mtime(&guest_root, 100);
        set_mtime(&upper_manifest, 50);
        let scratch = rootfs.join(VOLUME_POSIX_METADATA_PENDING_TEMP_FILE);
        std::fs::create_dir(&scratch).unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let published = rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&published).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        assert!(!scratch.exists());
    }

    #[test]
    fn stage_copies_another_root_when_the_publish_scratch_directory_is_not_empty() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        let upper = box_dir.join("upper");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&upper).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::write(&guest_root, b"not-json").unwrap();
        let upper_manifest = upper.join(VOLUME_POSIX_METADATA_FILE);
        let captured = VolumePosixManifest {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts: vec![VolumePosixMount {
                guest_path: "/data".to_string(),
                entries: vec![entry(0o640)],
                retained: false,
            }],
        };
        std::fs::write(&upper_manifest, serde_json::to_vec(&captured).unwrap()).unwrap();
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&sidecar_path, 5);
        set_mtime(&guest_root, 100);
        set_mtime(&upper_manifest, 50);
        let scratch = rootfs.join(VOLUME_POSIX_METADATA_PENDING_TEMP_FILE);
        std::fs::create_dir(&scratch).unwrap();
        let marker = scratch.join("keep.txt");
        std::fs::write(&marker, b"keep").unwrap();

        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let published = rootfs.join(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE);
        let kept: VolumePosixManifest =
            serde_json::from_slice(&std::fs::read(&published).unwrap()).unwrap();
        assert_eq!(kept.mounts[0].entries[0].mode, 0o640);
        assert!(scratch.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
    }

    #[test]
    fn oversized_sidecar_is_still_a_size_error_when_nothing_valid_remains() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let guest_root = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        std::fs::remove_file(&guest_root).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        let file = std::fs::File::create(&sidecar).unwrap();
        file.set_len(VOLUME_POSIX_METADATA_MAX_BYTES + 1).unwrap();
        drop(file);

        let error = sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap_err();
        assert!(
            error.to_string().contains("exceeds the size limit"),
            "{error}"
        );
        assert!(!guest_root.exists());
        assert!(sidecar.metadata().unwrap().len() > VOLUME_POSIX_METADATA_MAX_BYTES);
    }

    #[test]
    fn oversized_committed_manifest_is_still_a_size_error() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let manifest = rootfs.join(VOLUME_POSIX_METADATA_FILE);
        let _ = std::fs::remove_file(&manifest);
        let file = std::fs::File::create(&manifest).unwrap();
        file.set_len(VOLUME_POSIX_METADATA_MAX_BYTES + 1).unwrap();
        drop(file);

        let error = harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("exceeds the volume posix metadata size limit"),
            "{error}"
        );
        let staged = sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap_err();
        assert!(
            staged
                .to_string()
                .contains("exceeds the volume posix metadata size limit"),
            "{staged}"
        );
    }

    #[test]
    fn harvest_reads_a_newer_publish_capture_beside_the_durable_temp() {
        let home = tempfile::tempdir().unwrap();
        let volumes = home.path().join("volumes");
        let volume = volumes.join("data");
        std::fs::create_dir_all(&volume).unwrap();
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry(0o755)])).unwrap();
        let sidecar_path = volume_posix_sidecar_path(&volume).unwrap();
        let box_dir = home.path().join("boxes").join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = [("/data".to_string(), volume.clone())];
        sync_managed_volume_posix(&box_dir, &rootfs, &mounts).unwrap();
        let write_mode = |name: &str, mode: u32| {
            let manifest = VolumePosixManifest {
                schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
                mounts: vec![VolumePosixMount {
                    guest_path: "/data".to_string(),
                    entries: vec![entry(mode)],
                    retained: false,
                }],
            };
            let path = rootfs.join(name);
            std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
            path
        };
        let guest_root = write_mode(VOLUME_POSIX_METADATA_FILE, 0o644);
        let temp = write_mode(VOLUME_POSIX_METADATA_TEMP_FILE, 0o640);
        let published = write_mode(VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE, 0o600);
        let pending_scratch = write_mode(VOLUME_POSIX_METADATA_PENDING_TEMP_FILE, 0o777);
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&sidecar_path, 5);
        set_mtime(&guest_root, 10);
        set_mtime(&temp, 50);
        set_mtime(&published, 80);
        set_mtime(&pending_scratch, 90);

        harvest_volume_posix_sidecars(&box_dir, &volumes).unwrap();
        let sidecar = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(sidecar.entries[0].mode, 0o600);
    }
}
