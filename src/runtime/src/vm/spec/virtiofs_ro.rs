//! Host-enforced read-only virtio-fs shares for MicroVM `:ro` volumes.
//!
//! Guest `MS_RDONLY` alone is not host write denial: libkrun's virtio-fs path
//! shares the host directory writable. Linux stages a private bind remounted
//! `MS_RDONLY` (same honesty contract as SandboxViaOci attachment aliases) and
//! points virtio-fs at that alias. Non-Linux refuses `:ro` until a native host
//! denial exists (guest-honor-only is not production-honest).

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};

/// Subdirectory under `.filemounts` that owns MicroVM `:ro` bind aliases.
#[cfg(target_os = "linux")]
const RO_ALIAS_DIR: &str = "ro-aliases";

#[cfg(target_os = "linux")]
fn ro_alias_root(filemounts_dir: &Path) -> PathBuf {
    filemounts_dir.join(RO_ALIAS_DIR)
}

/// Replace a `:ro` volume host path with a Box-owned read-only bind alias.
///
/// Callers must pass a canonical plain directory (single-file binds are staged
/// into a directory before this runs). Existing aliases for this index are
/// detached first so restarts never reuse a stale mount.
#[cfg(target_os = "linux")]
pub(super) fn stage_virtiofs_ro_share(
    source: &Path,
    filemounts_dir: &Path,
    index: usize,
) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;

    if !source.is_dir() {
        return Err(BoxError::ConfigError(format!(
            "MicroVM :ro virtio-fs share requires a directory host path: {}",
            source.display()
        )));
    }

    let root = ro_alias_root(filemounts_dir);
    std::fs::create_dir_all(&root).map_err(BoxError::IoError)?;
    let target = root.join(index.to_string());
    detach_if_mounted(&target)?;
    if target.exists() {
        std::fs::remove_dir_all(&target).map_err(BoxError::IoError)?;
    }
    std::fs::create_dir(&target).map_err(BoxError::IoError)?;

    let source_c = std::ffi::CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        BoxError::ConfigError(format!(
            "MicroVM :ro volume source contains NUL: {}",
            source.display()
        ))
    })?;
    let target_c = std::ffi::CString::new(target.as_os_str().as_bytes()).map_err(|_| {
        BoxError::ConfigError(format!(
            "MicroVM :ro volume alias contains NUL: {}",
            target.display()
        ))
    })?;

    let mounted = unsafe {
        libc::mount(
            source_c.as_ptr(),
            target_c.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    };
    if mounted != 0 {
        let _ = std::fs::remove_dir(&target);
        return Err(BoxError::BoxBootError {
            message: format!(
                "Failed to bind MicroVM :ro volume {} at {}: {}",
                source.display(),
                target.display(),
                std::io::Error::last_os_error()
            ),
            hint: Some("Host CAP_SYS_ADMIN (or equivalent) is required for :ro virtio-fs write denial".into()),
        });
    }

    let remount_flags =
        libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV;
    let remounted = unsafe {
        libc::mount(
            std::ptr::null(),
            target_c.as_ptr(),
            std::ptr::null(),
            remount_flags,
            std::ptr::null(),
        )
    };
    if remounted != 0 {
        let error = std::io::Error::last_os_error();
        let _ = detach_if_mounted(&target);
        let _ = std::fs::remove_dir(&target);
        return Err(BoxError::BoxBootError {
            message: format!(
                "Failed to remount MicroVM :ro volume alias {} read-only: {error}",
                target.display()
            ),
            hint: None,
        });
    }

    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").map_err(BoxError::IoError)?;
    if !mount_is_read_only(&mountinfo, &target) {
        let _ = detach_if_mounted(&target);
        let _ = std::fs::remove_dir(&target);
        return Err(BoxError::BoxBootError {
            message: format!(
                "MicroVM :ro volume alias did not become read-only: {}",
                target.display()
            ),
            hint: None,
        });
    }

    Ok(target)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn stage_virtiofs_ro_share(
    source: &Path,
    _filemounts_dir: &Path,
    _index: usize,
) -> Result<PathBuf> {
    // Guest MS_RDONLY alone is not host write denial. Refuse MicroVM :ro until
    // a native virtio-fs share flag exists (same honesty class as Sandbox
    // attachment aliases requiring Linux host RO enforcement).
    Err(BoxError::ConfigError(format!(
        "MicroVM :ro volume requires Linux host-enforced virtio-fs write denial; \
         refusing guest-honor-only attach for {}",
        source.display()
    )))
}

/// Detach every MicroVM `:ro` alias under `.filemounts` before deleting the box.
#[cfg(target_os = "linux")]
pub(crate) fn cleanup_virtiofs_ro_shares(box_dir: &Path) -> Result<()> {
    let root = box_dir.join(".filemounts").join(RO_ALIAS_DIR);
    if !root.exists() {
        return Ok(());
    }
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").map_err(|error| {
        BoxError::StateError(format!(
            "Failed to inspect MicroVM :ro volume mounts: {error}"
        ))
    })?;
    let mut mounted = mountinfo
        .lines()
        .filter_map(|line| {
            let mut parts = line.split(' ');
            let _ = parts.next()?; // mount id
            let _ = parts.next()?; // parent
            let _ = parts.next()?; // major:minor
            let _ = parts.next()?; // root
            let mount_point = parts.next()?;
            Some(PathBuf::from(mount_point))
        })
        .filter(|path| path.starts_with(&root))
        .collect::<Vec<_>>();
    mounted.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for path in mounted {
        detach_if_mounted(&path)?;
    }
    if root.exists() {
        std::fs::remove_dir_all(&root).map_err(BoxError::IoError)?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn cleanup_virtiofs_ro_shares(_box_dir: &Path) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn detach_if_mounted(target: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    if !target.exists() {
        return Ok(());
    }
    let target_c = std::ffi::CString::new(target.as_os_str().as_bytes()).map_err(|_| {
        BoxError::ConfigError(format!(
            "MicroVM :ro volume alias contains NUL: {}",
            target.display()
        ))
    })?;
    let rc = unsafe { libc::umount2(target_c.as_ptr(), libc::MNT_DETACH) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINVAL) || err.raw_os_error() == Some(libc::ENOENT) {
            return Ok(());
        }
        return Err(BoxError::IoError(err));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn mount_is_read_only(mountinfo: &str, target: &Path) -> bool {
    let target = target.to_string_lossy();
    for line in mountinfo.lines() {
        let mut parts = line.split(' ');
        let _ = parts.next();
        let _ = parts.next();
        let _ = parts.next();
        let _ = parts.next();
        let Some(mount_point) = parts.next() else {
            continue;
        };
        if mount_point != target {
            continue;
        }
        // Optional fields … `-` fs_type source super_opts
        let rest: Vec<_> = parts.collect();
        if let Some(dash) = rest.iter().position(|p| *p == "-") {
            if let Some(super_opts) = rest.get(dash + 3) {
                return super_opts.split(',').any(|opt| opt == "ro");
            }
        }
        // Fallback: mount options field immediately after mount point.
        if let Some(opts) = rest.first() {
            return opts.split(',').any(|opt| opt == "ro");
        }
    }
    false
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn can_mount() -> bool {
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("mnt");
        std::fs::create_dir(&target).unwrap();
        let source = dir.path().join("src");
        std::fs::create_dir(&source).unwrap();
        let source_c = std::ffi::CString::new(source.as_os_str().as_bytes()).unwrap();
        let target_c = std::ffi::CString::new(target.as_os_str().as_bytes()).unwrap();
        let mounted = unsafe {
            libc::mount(
                source_c.as_ptr(),
                target_c.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND,
                std::ptr::null(),
            )
        };
        if mounted == 0 {
            let _ = unsafe { libc::umount2(target_c.as_ptr(), 0) };
            true
        } else {
            false
        }
    }

    #[test]
    fn stages_read_only_virtiofs_alias_when_privileged() {
        if !can_mount() {
            return;
        }
        let fixture = tempfile::tempdir().unwrap();
        let box_dir = fixture.path();
        let source = box_dir.join("vol");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("x"), b"data").unwrap();
        let filemounts = box_dir.join(".filemounts");
        std::fs::create_dir_all(&filemounts).unwrap();

        let alias = stage_virtiofs_ro_share(&source, &filemounts, 0).unwrap();
        assert_eq!(std::fs::read(alias.join("x")).unwrap(), b"data");
        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
        assert!(mount_is_read_only(&mountinfo, &alias));
        assert!(std::fs::write(alias.join("y"), b"no").is_err());

        cleanup_virtiofs_ro_shares(box_dir).unwrap();
        assert!(!ro_alias_root(&filemounts).exists());
        assert_eq!(std::fs::read(source.join("x")).unwrap(), b"data");
    }
}
