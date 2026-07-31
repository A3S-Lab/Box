//! Box-owned aliases for caller-owned read-only Sandbox attachments.
//!
//! A3S OCI resolves bind sources after entering the workload user namespace.
//! A readable Artifact root below a private provider directory can therefore be
//! inaccessible even though the Box service itself opened it successfully.
//! These aliases pin the already-open source into the Box-owned Sandbox tree;
//! caller permissions and storage ownership remain unchanged.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};

use super::SandboxIdMappingPlan;

#[cfg(target_os = "linux")]
const ATTACHMENTS_DIRECTORY: &str = "sandbox/attachments";

#[cfg(target_os = "linux")]
pub(crate) fn sandbox_mount_alias_root(home_dir: &Path, box_id: &str) -> PathBuf {
    home_dir
        .join("boxes")
        .join(box_id)
        .join(ATTACHMENTS_DIRECTORY)
}

/// Replace each distinct caller-owned source with one read-only bind alias.
///
/// Existing aliases are removed first, so a persistent Sandbox restart never
/// reuses a mount from an older runtime generation. A partial preparation is
/// rolled back before the error is returned.
#[cfg(target_os = "linux")]
pub(crate) fn stage_read_only_mount_aliases(
    home_dir: &Path,
    box_id: &str,
    sources: &[PathBuf],
    id_mappings: &SandboxIdMappingPlan,
) -> Result<HashMap<PathBuf, PathBuf>> {
    cleanup_sandbox_mount_aliases(home_dir, box_id)?;
    if sources.is_empty() {
        return Ok(HashMap::new());
    }

    let root = sandbox_mount_alias_root(home_dir, box_id);
    create_alias_root(&root)?;
    let mut aliases = HashMap::new();

    for source in sources {
        if aliases.contains_key(source) {
            continue;
        }
        let target = root.join(format!("{:04}", aliases.len()));
        if let Err(error) = stage_one_alias(source, &target, id_mappings) {
            let rollback = cleanup_sandbox_mount_aliases(home_dir, box_id);
            return match rollback {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(BoxError::BoxBootError {
                    message: format!(
                        "Failed to stage Sandbox attachment {}: {error}; rollback also failed: {rollback_error}",
                        source.display()
                    ),
                    hint: Some(
                        "Reconcile the Box attachment mounts before retrying the Sandbox".into(),
                    ),
                }),
            };
        }
        aliases.insert(source.clone(), target);
    }

    Ok(aliases)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn stage_read_only_mount_aliases(
    _home_dir: &Path,
    _box_id: &str,
    _sources: &[PathBuf],
    _id_mappings: &SandboxIdMappingPlan,
) -> Result<HashMap<PathBuf, PathBuf>> {
    Err(BoxError::ConfigError(
        "Sandbox attachment aliases require Linux".into(),
    ))
}

/// Unmount and remove every alias owned by one Sandbox generation.
///
/// Mount points are discovered from mountinfo instead of trusting directory
/// entries left by a crashed process. Nothing below the alias root is deleted
/// until every attached mount has been detached.
#[cfg(target_os = "linux")]
pub(crate) fn cleanup_sandbox_mount_aliases(home_dir: &Path, box_id: &str) -> Result<()> {
    let root = sandbox_mount_alias_root(home_dir, box_id);
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").map_err(|error| {
        BoxError::StateError(format!(
            "Failed to inspect Sandbox attachment mounts for {box_id}: {error}"
        ))
    })?;
    let mut mount_points = mount_points_below(&mountinfo, &root);
    mount_points.sort_by(|left, right| {
        right
            .components()
            .count()
            .cmp(&left.components().count())
            .then_with(|| right.cmp(left))
    });
    for mount_point in mount_points {
        detach_mount(&mount_point).map_err(|error| {
            BoxError::StateError(format!(
                "Failed to detach Sandbox attachment alias {}: {error}",
                mount_point.display()
            ))
        })?;
    }

    match std::fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            std::fs::remove_file(&root).map_err(BoxError::IoError)?;
            Err(BoxError::StateError(format!(
                "Sandbox attachment root was an unsafe symlink and was removed: {}",
                root.display()
            )))
        }
        Ok(metadata) if !metadata.is_dir() => Err(BoxError::StateError(format!(
            "Sandbox attachment root is not a directory: {}",
            root.display()
        ))),
        Ok(_) => std::fs::remove_dir_all(&root).map_err(BoxError::IoError),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(BoxError::IoError(error)),
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn cleanup_sandbox_mount_aliases(_home_dir: &Path, _box_id: &str) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn create_alias_root(root: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let sandbox_dir = root.parent().ok_or_else(|| {
        BoxError::StateError(format!(
            "Sandbox attachment root has no managed parent: {}",
            root.display()
        ))
    })?;
    let box_dir = sandbox_dir.parent().ok_or_else(|| {
        BoxError::StateError(format!(
            "Sandbox attachment parent has no managed box directory: {}",
            sandbox_dir.display()
        ))
    })?;
    require_plain_managed_directory(box_dir, "box")?;
    match std::fs::create_dir(sandbox_dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(BoxError::BoxBootError {
                message: format!(
                    "Failed to create Sandbox attachment parent {}: {error}",
                    sandbox_dir.display()
                ),
                hint: None,
            })
        }
    }
    require_plain_managed_directory(sandbox_dir, "attachment parent")?;
    std::fs::create_dir(root).map_err(|error| BoxError::BoxBootError {
        message: format!(
            "Failed to create Sandbox attachment root {}: {error}",
            root.display()
        ),
        hint: None,
    })?;
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o711)).map_err(|error| {
        BoxError::BoxBootError {
            message: format!(
                "Failed to secure Sandbox attachment root {}: {error}",
                root.display()
            ),
            hint: None,
        }
    })
}

#[cfg(target_os = "linux")]
fn require_plain_managed_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| BoxError::BoxBootError {
        message: format!(
            "Failed to inspect Sandbox {label} directory {}: {error}",
            path.display()
        ),
        hint: None,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(BoxError::StateError(format!(
            "Sandbox {label} is not a managed directory: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn stage_one_alias(source: &Path, target: &Path, id_mappings: &SandboxIdMappingPlan) -> Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;

    let source_metadata = std::fs::symlink_metadata(source).map_err(BoxError::IoError)?;
    let canonical_source = source.canonicalize().map_err(BoxError::IoError)?;
    if source_metadata.file_type().is_symlink() || canonical_source != source {
        return Err(BoxError::ConfigError(format!(
            "Sandbox attachment source must be a canonical plain path: {}",
            source.display()
        )));
    }

    let source_c = std::ffi::CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        BoxError::ConfigError(format!(
            "Sandbox attachment source contains NUL: {}",
            source.display()
        ))
    })?;
    let raw_fd = unsafe {
        libc::open(
            source_c.as_ptr(),
            libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if raw_fd < 0 {
        return Err(BoxError::IoError(std::io::Error::last_os_error()));
    }
    let source_file = unsafe { std::fs::File::from_raw_fd(raw_fd) };
    let metadata = source_file.metadata().map_err(BoxError::IoError)?;
    super::rootfs::validate_external_mount_root_metadata(source, &metadata, id_mappings, true)?;

    if metadata.is_dir() {
        std::fs::create_dir(target).map_err(BoxError::IoError)?;
    } else {
        std::fs::File::create(target).map_err(BoxError::IoError)?;
    }

    let proc_source = std::ffi::CString::new(format!("/proc/self/fd/{}", source_file.as_raw_fd()))
        .map_err(|error| {
            BoxError::ConfigError(format!("Invalid attachment descriptor: {error}"))
        })?;
    let target_c = std::ffi::CString::new(target.as_os_str().as_bytes()).map_err(|_| {
        BoxError::ConfigError(format!(
            "Sandbox attachment alias contains NUL: {}",
            target.display()
        ))
    })?;
    let mounted = unsafe {
        libc::mount(
            proc_source.as_ptr(),
            target_c.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    };
    if mounted != 0 {
        return Err(BoxError::BoxBootError {
            message: format!(
                "Failed to bind Sandbox attachment {} at {}: {}",
                source.display(),
                target.display(),
                std::io::Error::last_os_error()
            ),
            hint: Some("Run the Sandbox service with its required mount capability".into()),
        });
    }

    let private = unsafe {
        libc::mount(
            std::ptr::null(),
            target_c.as_ptr(),
            std::ptr::null(),
            libc::MS_PRIVATE | libc::MS_REC,
            std::ptr::null(),
        )
    };
    if private != 0 {
        let error = std::io::Error::last_os_error();
        let _ = detach_mount(target);
        return Err(BoxError::BoxBootError {
            message: format!(
                "Failed to make Sandbox attachment alias {} private: {error}",
                target.display()
            ),
            hint: None,
        });
    }

    let read_only = unsafe {
        libc::mount(
            std::ptr::null(),
            target_c.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV,
            std::ptr::null(),
        )
    };
    if read_only != 0 {
        let error = std::io::Error::last_os_error();
        let _ = detach_mount(target);
        return Err(BoxError::BoxBootError {
            message: format!(
                "Failed to make Sandbox attachment alias {} read-only: {error}",
                target.display()
            ),
            hint: None,
        });
    }

    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").map_err(BoxError::IoError)?;
    if !mount_is_read_only(&mountinfo, target) {
        let _ = detach_mount(target);
        return Err(BoxError::BoxBootError {
            message: format!(
                "Sandbox attachment alias did not become read-only: {}",
                target.display()
            ),
            hint: None,
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn detach_mount(target: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let target = std::ffi::CString::new(target.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "mount path has NUL"))?;
    if unsafe { libc::umount2(target.as_ptr(), 0) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if matches!(
        error.raw_os_error(),
        Some(libc::EINVAL) | Some(libc::ENOENT)
    ) {
        return Ok(());
    }
    if unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) } == 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error())
}

#[cfg(target_os = "linux")]
fn mount_points_below(mountinfo: &str, root: &Path) -> Vec<PathBuf> {
    mountinfo
        .lines()
        .filter_map(|line| line.split_whitespace().nth(4))
        .map(decode_mountinfo_path)
        .map(PathBuf::from)
        .filter(|path| path == root || path.starts_with(root))
        .collect()
}

#[cfg(target_os = "linux")]
fn mount_is_read_only(mountinfo: &str, target: &Path) -> bool {
    mountinfo.lines().any(|line| {
        let mut fields = line.split_whitespace();
        let mount_point = fields.nth(4).map(decode_mountinfo_path);
        let options = fields.next();
        mount_point.as_deref() == target.to_str()
            && options.is_some_and(|options| options.split(',').any(|option| option == "ro"))
    })
}

#[cfg(target_os = "linux")]
fn decode_mountinfo_path(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::sandbox::{validate_external_mount_access, IdMapping};

    fn mappings() -> SandboxIdMappingPlan {
        SandboxIdMappingPlan {
            uid_mappings: vec![IdMapping {
                container_id: 0,
                host_id: 100_000,
                size: 1,
            }],
            gid_mappings: vec![IdMapping {
                container_id: 0,
                host_id: 100_000,
                size: 1,
            }],
            maximum_container_uid: 0,
            maximum_container_gid: 0,
        }
    }

    fn can_mount() -> bool {
        if unsafe { libc::geteuid() } != 0 {
            return false;
        }
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find_map(|line| line.strip_prefix("CapEff:\t"))
                    .and_then(|value| u64::from_str_radix(value, 16).ok())
            })
            .is_some_and(|capabilities| capabilities & (1 << 21) != 0)
    }

    fn set_mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn alias_root_creates_its_missing_managed_sandbox_parent() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("a3s");
        std::fs::create_dir_all(home.join("boxes/execution-1")).unwrap();
        let root = sandbox_mount_alias_root(&home, "execution-1");

        create_alias_root(&root).unwrap();

        assert!(root.is_dir());
        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o7777,
            0o711
        );
        require_plain_managed_directory(root.parent().unwrap(), "attachment parent").unwrap();
    }

    #[test]
    fn alias_root_rejects_a_symlinked_sandbox_parent() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("a3s");
        let box_dir = home.join("boxes/execution-1");
        let outside = fixture.path().join("outside");
        std::fs::create_dir_all(&box_dir).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, box_dir.join("sandbox")).unwrap();
        let root = sandbox_mount_alias_root(&home, "execution-1");

        let error = create_alias_root(&root).unwrap_err();

        assert!(error.to_string().contains("not a managed directory"));
        assert!(!outside.join("attachments").exists());
    }

    #[test]
    fn aliases_a_read_only_source_below_a_private_caller_root() {
        if !can_mount() {
            return;
        }
        let fixture = tempfile::tempdir().unwrap();
        set_mode(fixture.path(), 0o755);
        let private = fixture.path().join("provider");
        let source = private.join("artifact/root");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("model.bin"), b"immutable").unwrap();
        set_mode(&private, 0o700);
        set_mode(&source, 0o755);

        let home = fixture.path().join("a3s");
        std::fs::create_dir_all(home.join("boxes/execution-1/sandbox")).unwrap();
        assert!(validate_external_mount_access(&source, &mappings(), true).is_err());

        let aliases = stage_read_only_mount_aliases(
            &home,
            "execution-1",
            std::slice::from_ref(&source),
            &mappings(),
        )
        .unwrap();
        let alias = aliases.get(&source).unwrap();
        assert_eq!(
            std::fs::read(alias.join("model.bin")).unwrap(),
            b"immutable"
        );
        assert!(validate_external_mount_access(alias, &mappings(), true).is_ok());
        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
        assert!(mount_is_read_only(&mountinfo, alias));
        assert_eq!(
            std::fs::metadata(&private).unwrap().permissions().mode() & 0o7777,
            0o700
        );

        cleanup_sandbox_mount_aliases(&home, "execution-1").unwrap();
        assert!(!sandbox_mount_alias_root(&home, "execution-1").exists());
        assert_eq!(
            std::fs::read(source.join("model.bin")).unwrap(),
            b"immutable"
        );
    }

    #[test]
    fn failed_alias_set_rolls_back_earlier_mounts() {
        if !can_mount() {
            return;
        }
        let fixture = tempfile::tempdir().unwrap();
        set_mode(fixture.path(), 0o755);
        let readable = fixture.path().join("readable");
        let unreadable = fixture.path().join("unreadable");
        std::fs::create_dir(&readable).unwrap();
        std::fs::create_dir(&unreadable).unwrap();
        set_mode(&readable, 0o755);
        set_mode(&unreadable, 0o700);
        let home = fixture.path().join("a3s");
        std::fs::create_dir_all(home.join("boxes/execution-2/sandbox")).unwrap();

        assert!(stage_read_only_mount_aliases(
            &home,
            "execution-2",
            &[readable, unreadable],
            &mappings(),
        )
        .is_err());
        let root = sandbox_mount_alias_root(&home, "execution-2");
        assert!(!root.exists());
        assert!(mount_points_below(
            &std::fs::read_to_string("/proc/self/mountinfo").unwrap(),
            &root
        )
        .is_empty());
    }
}
