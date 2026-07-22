//! Guest-side rootfs tar creation.
//!
//! Tar headers must be generated from guest-visible Linux metadata. Reading the
//! virtio-fs backing directory on macOS can expose different uid/gid/mode values.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use a3s_box_core::rootfs_metadata::IMAGE_ROOTFS_METADATA_PATH;
use a3s_box_core::rootfs_metadata::{
    is_runtime_internal_rootfs_path, runtime_managed_rootfs_mode,
    stage_terminal_rootfs_metadata_for_boot, RootfsEntryKind, RootfsMetadataEntry,
    RootfsMetadataManifest, PREVIOUS_ROOTFS_METADATA_PATH, ROOTFS_METADATA_PATH,
};
use base64::Engine;

/// Write a tar stream for `root`, excluding nested mount points such as procfs,
/// sysfs, tmpfs, and user volumes.
pub(crate) fn write_rootfs_archive(
    root: &Path,
    output: &mut impl Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let excluded_mounts = nested_mount_points(root)?;
    let mut builder = tar::Builder::new(output);
    builder.follow_symlinks(false);
    append_tree(&mut builder, root, root, Path::new("."), &excluded_mounts)?;
    builder.finish()?;
    Ok(())
}

/// Persist a compact terminal metadata snapshot for a stopped persistent box.
pub fn persist_rootfs_metadata(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let excluded_mounts = nested_mount_points(root)?;
    let mut entries = Vec::new();
    collect_metadata(root, root, Path::new("."), &excluded_mounts, &mut entries)?;
    entries.sort_by(|left, right| left.path_base64.cmp(&right.path_base64));
    let manifest = RootfsMetadataManifest::new(entries);
    let destination = root.join(ROOTFS_METADATA_PATH.trim_start_matches('/'));
    let temporary = destination.with_extension("json.tmp");
    let bytes = serde_json::to_vec(&manifest)?;
    {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(temporary, destination)?;
    sync_rootfs_directory(root)?;
    Ok(())
}

#[cfg(unix)]
fn sync_rootfs_directory(root: &Path) -> std::io::Result<()> {
    std::fs::File::open(root)?.sync_all()
}

#[cfg(not(unix))]
fn sync_rootfs_directory(_root: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Replay rootless-host OCI ownership and the last persistent guest generation.
/// This must run before procfs, workspace, or user volumes are mounted.
#[cfg(target_os = "linux")]
pub fn restore_rootfs_metadata(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    restore_rootfs_metadata_excluding(root, &HashSet::new())
}

/// Replay rootfs metadata after an OCI runtime has installed procfs, tmpfs,
/// and user bind mounts. Entries at or below a live nested mount are skipped so
/// replay can never chmod/chown an attached host path.
#[cfg(target_os = "linux")]
pub fn restore_rootfs_metadata_around_mounts(
    root: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let excluded_mounts = nested_mount_points(root)?;
    restore_rootfs_metadata_excluding(root, &excluded_mounts)
}

#[cfg(target_os = "linux")]
fn restore_rootfs_metadata_excluding(
    root: &Path,
    excluded_mounts: &HashSet<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    // The host normally stages this before launching the VM so commit observes
    // the missing canonical marker immediately. Repeat it here for runtimes that
    // launch guest-init directly: replay is one-shot, and only a subsequent
    // clean shutdown may create a new terminal completion marker.
    stage_terminal_rootfs_metadata_for_boot(root)?;
    // Runtime may update generated files such as resolv.conf after the image
    // rootfs cache is composed, so image replay validates type and symlink
    // identity but not regular-file size. The terminal snapshot was captured
    // after all container writes and remains strict.
    apply_metadata_manifest(root, IMAGE_ROOTFS_METADATA_PATH, false, excluded_mounts)?;
    apply_metadata_manifest(root, PREVIOUS_ROOTFS_METADATA_PATH, true, excluded_mounts)?;
    match std::fs::remove_file(root.join(PREVIOUS_ROOTFS_METADATA_PATH.trim_start_matches('/'))) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    // Fence even a not-found retry: a prior delete may have succeeded before
    // its directory sync reported an error. Never exec the workload until that
    // one-shot deletion is durably ordered.
    sync_rootfs_directory(root)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn apply_metadata_manifest(
    root: &Path,
    manifest_path: &str,
    strict_content: bool,
    excluded_mounts: &HashSet<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let source = root.join(manifest_path.trim_start_matches('/'));
    let source_metadata = match std::fs::symlink_metadata(&source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !source_metadata.file_type().is_file() {
        return Err(format!("rootfs metadata is not a plain file: {}", source.display()).into());
    }
    let bytes = std::fs::read(&source)?;
    let manifest: RootfsMetadataManifest = serde_json::from_slice(&bytes)?;
    manifest.validate()?;
    let mut decoded = Vec::with_capacity(manifest.entries.len());
    let mut unique = HashSet::with_capacity(manifest.entries.len());
    for entry in manifest.entries {
        if entry.uid > u32::MAX as u64 || entry.gid > u32::MAX as u64 {
            return Err("rootfs metadata uid/gid exceeds Linux range".into());
        }
        let raw = base64::engine::general_purpose::STANDARD.decode(&entry.path_base64)?;
        let relative = PathBuf::from(std::ffi::OsString::from_vec(raw));
        let relative = safe_relative_path(&relative)?;
        if is_runtime_internal_rootfs_path(&relative) || !unique.insert(relative.clone()) {
            return Err("duplicate or reserved rootfs metadata path".into());
        }
        let unresolved_target = root.join(&relative);
        if excluded_mounts
            .iter()
            .any(|mount| unresolved_target == *mount || unresolved_target.starts_with(mount))
        {
            continue;
        }
        let target = resolve_without_symlink_parent(root, &relative)?;
        let metadata = std::fs::symlink_metadata(&target)?;
        let actual_kind = if metadata.file_type().is_dir() {
            RootfsEntryKind::Directory
        } else if metadata.file_type().is_file() {
            RootfsEntryKind::Regular
        } else if metadata.file_type().is_symlink() {
            RootfsEntryKind::Symlink
        } else {
            return Err(format!("unsupported rootfs entry at {}", target.display()).into());
        };
        if actual_kind != entry.kind
            || (strict_content
                && runtime_managed_rootfs_mode(&relative).is_none()
                && actual_kind == RootfsEntryKind::Regular
                && metadata.size() != entry.size)
        {
            return Err(format!("rootfs metadata mismatch at {}", target.display()).into());
        }
        if actual_kind == RootfsEntryKind::Symlink {
            let expected = entry
                .link_target_base64
                .as_ref()
                .ok_or("symlink metadata is missing its target")?;
            let expected = base64::engine::general_purpose::STANDARD.decode(expected)?;
            if std::fs::read_link(&target)?.as_os_str().as_bytes() != expected {
                return Err(format!("rootfs symlink mismatch at {}", target.display()).into());
            }
        }
        decoded.push((
            entry,
            target,
            metadata.uid() as u64,
            metadata.gid() as u64,
            metadata.mode(),
        ));
    }

    for (entry, target, current_uid, current_gid, current_mode) in &decoded {
        if entry.uid == *current_uid && entry.gid == *current_gid {
            continue;
        }
        if entry.kind != RootfsEntryKind::Symlink && current_mode & 0o200 == 0 {
            std::fs::set_permissions(
                target,
                std::fs::Permissions::from_mode((current_mode & 0o7777) | 0o200),
            )
            .map_err(|error| {
                format!(
                    "failed to make {} writable for ownership replay: {error}",
                    target.display()
                )
            })?;
        }
        let path = std::ffi::CString::new(target.as_os_str().as_bytes())?;
        if unsafe { libc::lchown(path.as_ptr(), entry.uid as u32, entry.gid as u32) } != 0 {
            return Err(format!(
                "failed to restore ownership at {} from {}:{} to {}:{}: {}",
                target.display(),
                current_uid,
                current_gid,
                entry.uid,
                entry.gid,
                std::io::Error::last_os_error()
            )
            .into());
        }
    }
    decoded.sort_by_key(|(_, path, _, _, _)| std::cmp::Reverse(path.components().count()));
    for (entry, target, _, _, _) in &decoded {
        if entry.kind != RootfsEntryKind::Symlink {
            let current_mode = std::fs::symlink_metadata(target)?.mode() & 0o7777;
            let relative = target.strip_prefix(root)?;
            let desired_mode = runtime_managed_rootfs_mode(relative).unwrap_or(entry.mode & 0o7777);
            if current_mode == desired_mode {
                continue;
            }
            std::fs::set_permissions(target, std::fs::Permissions::from_mode(desired_mode))
                .map_err(|error| {
                    format!(
                        "failed to restore mode at {} to {:o}: {error}",
                        target.display(),
                        desired_mode
                    )
                })?;
        }
    }
    match std::fs::remove_file(source) {
        Ok(()) => {}
        // A host-prepared read-only OCI rootfs has already passed every type,
        // ownership, mode, size, and symlink check above. Keeping the internal
        // manifest is safe when the mount itself prevents its removal.
        Err(error) if error.raw_os_error() == Some(libc::EROFS) => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn safe_relative_path(path: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    use std::path::Component;
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => result.push(name),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("unsafe rootfs metadata path".into())
            }
        }
    }
    Ok(result)
}

#[cfg(target_os = "linux")]
fn resolve_without_symlink_parent(
    root: &Path,
    relative: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut current = root.to_path_buf();
    let components: Vec<_> = relative.components().collect();
    for (index, component) in components.iter().enumerate() {
        let std::path::Component::Normal(name) = component else {
            continue;
        };
        current.push(name);
        if index + 1 < components.len()
            && std::fs::symlink_metadata(&current)?
                .file_type()
                .is_symlink()
        {
            return Err(format!(
                "symlink parent in rootfs metadata path: {}",
                current.display()
            )
            .into());
        }
    }
    Ok(current)
}

fn append_tree<W: Write>(
    builder: &mut tar::Builder<W>,
    root: &Path,
    source: &Path,
    archive_path: &Path,
    excluded_mounts: &HashSet<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    if should_skip(root, source, excluded_mounts) {
        return Ok(());
    }

    let metadata = std::fs::symlink_metadata(source)?;
    let file_type = metadata.file_type();
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if file_type.is_socket() {
            return Ok(());
        }
    }

    if file_type.is_dir() {
        builder.append_dir(archive_path, source)?;
        let mut entries: Vec<_> = std::fs::read_dir(source)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            append_tree(
                builder,
                root,
                &entry.path(),
                &archive_path.join(entry.file_name()),
                excluded_mounts,
            )?;
        }
    } else {
        builder.append_path_with_name(source, archive_path)?;
    }
    Ok(())
}

fn collect_metadata(
    root: &Path,
    source: &Path,
    archive_path: &Path,
    excluded_mounts: &HashSet<PathBuf>,
    entries: &mut Vec<RootfsMetadataEntry>,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    if should_skip(root, source, excluded_mounts) {
        return Ok(());
    }
    let metadata = std::fs::symlink_metadata(source)?;
    let file_type = metadata.file_type();
    let (kind, link_target_base64) = if file_type.is_dir() {
        (RootfsEntryKind::Directory, None)
    } else if file_type.is_file() {
        (RootfsEntryKind::Regular, None)
    } else if file_type.is_symlink() {
        let target = std::fs::read_link(source)?;
        (
            RootfsEntryKind::Symlink,
            Some(base64::engine::general_purpose::STANDARD.encode(target.as_os_str().as_bytes())),
        )
    } else {
        return Ok(());
    };
    entries.push(RootfsMetadataEntry {
        path_base64: base64::engine::general_purpose::STANDARD
            .encode(archive_path.as_os_str().as_bytes()),
        kind,
        mode: metadata.mode(),
        uid: metadata.uid() as u64,
        gid: metadata.gid() as u64,
        mtime: metadata.mtime().max(0) as u64,
        size: metadata.size(),
        link_target_base64,
    });

    if file_type.is_dir() {
        let mut children: Vec<_> = std::fs::read_dir(source)?.collect::<Result<_, _>>()?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            collect_metadata(
                root,
                &child.path(),
                &archive_path.join(child.file_name()),
                excluded_mounts,
                entries,
            )?;
        }
    }
    Ok(())
}

fn should_skip(root: &Path, source: &Path, excluded_mounts: &HashSet<PathBuf>) -> bool {
    if source != root && excluded_mounts.contains(source) {
        return true;
    }
    let Ok(relative) = source.strip_prefix(root) else {
        return true;
    };
    is_runtime_internal_rootfs_path(relative)
}

fn nested_mount_points(root: &Path) -> Result<HashSet<PathBuf>, std::io::Error> {
    #[cfg(target_os = "linux")]
    {
        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")?;
        Ok(parse_nested_mount_points(root, &mountinfo))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = root;
        Ok(HashSet::new())
    }
}

fn parse_nested_mount_points(root: &Path, mountinfo: &str) -> HashSet<PathBuf> {
    mountinfo
        .lines()
        .filter_map(|line| line.split_whitespace().nth(4))
        .map(decode_mountinfo_path)
        .map(PathBuf::from)
        .filter(|mount| mount != root && mount.starts_with(root))
        .collect()
}

fn decode_mountinfo_path(path: &str) -> String {
    path.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn directory_sync_errors_are_propagated() {
        let directory = tempfile::tempdir().unwrap();
        assert!(sync_rootfs_directory(&directory.path().join("missing")).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn metadata_replay_normalizes_runtime_managed_files() {
        use base64::Engine;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = tempfile::tempdir().unwrap();
        let etc = directory.path().join("etc");
        std::fs::create_dir(&etc).unwrap();
        let hosts = etc.join("hosts");
        let probe = etc.join("probe");
        let environment = directory.path().join(".a3s-box-env");
        let init = directory.path().join("usr/sbin/init");
        std::fs::create_dir_all(init.parent().unwrap()).unwrap();
        std::fs::write(&hosts, "127.0.0.1 localhost\n").unwrap();
        std::fs::write(&probe, "probe\n").unwrap();
        std::fs::write(&environment, "PATH=L3Vzci9iaW4=\nSMOKE=dHJ1ZQ\n").unwrap();
        for path in [&hosts, &probe, &environment] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        std::fs::write(&init, "guest init\n").unwrap();
        std::fs::set_permissions(&init, std::fs::Permissions::from_mode(0o755)).unwrap();
        let metadata = std::fs::metadata(&hosts).unwrap();
        let entry = |path: &str, size: u64| RootfsMetadataEntry {
            path_base64: base64::engine::general_purpose::STANDARD
                .encode(Path::new(path).as_os_str().as_bytes()),
            kind: RootfsEntryKind::Regular,
            mode: 0o100600,
            uid: metadata.uid() as u64,
            gid: metadata.gid() as u64,
            mtime: 0,
            size,
            link_target_base64: None,
        };
        let manifest = RootfsMetadataManifest::new(vec![
            // Deliberately stale size and mode: both are runtime-owned.
            entry("./etc/hosts", 1),
            entry("./etc/probe", 6),
            entry("./usr/sbin/init", 1),
            entry("./.a3s-box-env", 1),
        ]);
        std::fs::write(
            directory
                .path()
                .join(ROOTFS_METADATA_PATH.trim_start_matches('/')),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        apply_metadata_manifest(
            directory.path(),
            ROOTFS_METADATA_PATH,
            true,
            &HashSet::new(),
        )
        .unwrap();

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
        assert_eq!(
            std::fs::metadata(environment).unwrap().permissions().mode() & 0o7777,
            0o600
        );
    }

    #[test]
    fn archive_preserves_guest_visible_mode_uid_gid_and_symlink() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = tempfile::TempDir::new().unwrap();
        let executable = directory.path().join("executable");
        std::fs::write(&executable, b"payload").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o751)).unwrap();
        std::os::unix::fs::symlink("executable", directory.path().join("link")).unwrap();
        let metadata = std::fs::metadata(&executable).unwrap();

        let mut bytes = Vec::new();
        write_rootfs_archive(directory.path(), &mut bytes).unwrap();
        let mut archive = tar::Archive::new(bytes.as_slice());
        let mut saw_executable = false;
        let mut saw_link = false;
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            let path = entry.path().unwrap();
            let path = path.strip_prefix(".").unwrap_or(path.as_ref());
            match path.to_string_lossy().as_ref() {
                "executable" => {
                    saw_executable = true;
                    assert_eq!(entry.header().mode().unwrap() & 0o7777, 0o751);
                    assert_eq!(entry.header().uid().unwrap(), metadata.uid() as u64);
                    assert_eq!(entry.header().gid().unwrap(), metadata.gid() as u64);
                }
                "link" => {
                    saw_link = true;
                    assert_eq!(entry.link_name().unwrap().unwrap(), Path::new("executable"));
                }
                _ => {}
            }
        }
        assert!(saw_executable);
        assert!(saw_link);
    }

    #[test]
    fn mountinfo_parser_decodes_and_keeps_only_nested_mounts() {
        let mounts = parse_nested_mount_points(
            Path::new("/"),
            "1 0 0:1 / / rw - rootfs rootfs rw\n\
             2 1 0:2 / /proc rw - proc proc rw\n\
             3 1 0:3 / /with\\040space rw - tmpfs tmpfs rw\n",
        );

        assert!(mounts.contains(Path::new("/proc")));
        assert!(mounts.contains(Path::new("/with space")));
        assert!(!mounts.contains(Path::new("/")));
    }

    #[test]
    fn persisted_manifest_records_terminal_metadata_and_excludes_internal_files() {
        use base64::Engine;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::TempDir::new().unwrap();
        let executable = directory.path().join("probe");
        std::fs::write(&executable, b"probe").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(directory.path().join(".a3s_exit_code"), b"0").unwrap();
        std::fs::write(directory.path().join("init-rust.log"), b"runtime log").unwrap();
        std::fs::write(directory.path().join("init.trace.log"), b"runtime trace").unwrap();

        persist_rootfs_metadata(directory.path()).unwrap();
        let manifest_path = directory.path().join(".a3s_rootfs_metadata_v1.json");
        let manifest: RootfsMetadataManifest =
            serde_json::from_slice(&std::fs::read(manifest_path).unwrap()).unwrap();
        manifest.validate().unwrap();
        let probe_path = base64::engine::general_purpose::STANDARD
            .encode(Path::new("./probe").as_os_str().as_bytes());
        let probe = manifest
            .entries
            .iter()
            .find(|entry| entry.path_base64 == probe_path)
            .unwrap();
        assert_eq!(probe.mode & 0o7777, 0o755);
        assert!(!manifest.entries.iter().any(|entry| {
            base64::engine::general_purpose::STANDARD
                .decode(&entry.path_base64)
                .is_ok_and(|path| path.ends_with(b".a3s_exit_code"))
        }));
        assert!(!manifest.entries.iter().any(|entry| {
            base64::engine::general_purpose::STANDARD
                .decode(&entry.path_base64)
                .is_ok_and(|path| path.ends_with(b"init-rust.log"))
        }));
        assert!(!manifest.entries.iter().any(|entry| {
            base64::engine::general_purpose::STANDARD
                .decode(&entry.path_base64)
                .is_ok_and(|path| path.ends_with(b"init.trace.log"))
        }));
    }
}
