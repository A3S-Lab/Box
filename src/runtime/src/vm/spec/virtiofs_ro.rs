//! Host-enforced read-only virtio-fs shares for MicroVM `:ro` volumes.
//!
//! Guest `MS_RDONLY` alone is not host write denial: libkrun's virtio-fs path
//! shares the host directory writable. Linux stages a private bind remounted
//! `MS_RDONLY` (same honesty contract as SandboxViaOci attachment aliases) and
//! points virtio-fs at that alias. Windows stages a BindFlt read-only mapping
//! with the same live-view / source-stays-writable contract. Other hosts refuse
//! `:ro` until a native host denial exists (guest-honor-only is not
//! production-honest).

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};

/// Subdirectory under `.filemounts` that owns MicroVM `:ro` bind aliases.
#[cfg(any(target_os = "linux", target_os = "windows"))]
const RO_ALIAS_DIR: &str = "ro-aliases";

#[cfg(any(target_os = "linux", target_os = "windows"))]
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
            hint: Some(
                "Host CAP_SYS_ADMIN (or equivalent) is required for :ro virtio-fs write denial"
                    .into(),
            ),
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

/// Windows host-enforced `:ro`: BindFlt read-only mapping over a Box-owned alias.
///
/// Same honesty class as Linux `MS_RDONLY` bind aliases: the virtio-fs share path
/// must deny host writes while the caller's source path stays writable and live.
#[cfg(target_os = "windows")]
pub(super) fn stage_virtiofs_ro_share(
    source: &Path,
    filemounts_dir: &Path,
    index: usize,
) -> Result<PathBuf> {
    if !source.is_dir() {
        return Err(BoxError::ConfigError(format!(
            "MicroVM :ro virtio-fs share requires a directory host path: {}",
            source.display()
        )));
    }

    let root = ro_alias_root(filemounts_dir);
    std::fs::create_dir_all(&root).map_err(BoxError::IoError)?;
    let target = root.join(index.to_string());
    detach_bindflt_mapping(&target)?;
    if target.exists() {
        std::fs::remove_dir_all(&target).map_err(BoxError::IoError)?;
    }
    std::fs::create_dir(&target).map_err(BoxError::IoError)?;

    if let Err(error) = setup_bindflt_read_only(&target, source) {
        let _ = std::fs::remove_dir(&target);
        return Err(error);
    }

    // Prove host write denial on the alias without mutating the caller's source.
    let probe = target.join(".a3s-box-ro-probe");
    match std::fs::write(&probe, b"no") {
        Err(_) => {}
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            let _ = detach_bindflt_mapping(&target);
            let _ = std::fs::remove_dir_all(&target);
            return Err(BoxError::BoxBootError {
                message: format!(
                    "MicroVM :ro volume alias did not deny writes: {}",
                    target.display()
                ),
                hint: Some(
                    "BindFlt read-only mapping is required for Windows :ro virtio-fs write denial"
                        .into(),
                ),
            });
        }
    }

    // Prove the live view still reads through to the backing path.
    let marker = source.join(".a3s-box-ro-marker");
    std::fs::write(&marker, b"ro-ok").map_err(BoxError::IoError)?;
    let seen = std::fs::read(target.join(".a3s-box-ro-marker"));
    let _ = std::fs::remove_file(&marker);
    if seen.map(|bytes| bytes == b"ro-ok").unwrap_or(false) {
        Ok(target)
    } else {
        let _ = detach_bindflt_mapping(&target);
        let _ = std::fs::remove_dir_all(&target);
        Err(BoxError::BoxBootError {
            message: format!(
                "MicroVM :ro volume alias did not mirror source reads: {}",
                target.display()
            ),
            hint: None,
        })
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub(super) fn stage_virtiofs_ro_share(
    source: &Path,
    _filemounts_dir: &Path,
    _index: usize,
) -> Result<PathBuf> {
    // Guest MS_RDONLY alone is not host write denial. Refuse MicroVM :ro until
    // a native virtio-fs share flag exists (macOS still lacks BindFlt / MS_RDONLY
    // bind parity).
    Err(BoxError::ConfigError(format!(
        "MicroVM :ro volume requires host-enforced virtio-fs write denial \
         (Linux RO bind or Windows BindFlt); refusing guest-honor-only attach for {}",
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

#[cfg(target_os = "windows")]
pub(crate) fn cleanup_virtiofs_ro_shares(box_dir: &Path) -> Result<()> {
    let root = box_dir.join(".filemounts").join(RO_ALIAS_DIR);
    if !root.exists() {
        return Ok(());
    }
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(BoxError::IoError(error)),
    };
    let mut aliases = entries
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(BoxError::IoError)?;
    aliases.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for alias in aliases {
        detach_bindflt_mapping(&alias)?;
        if alias.exists() {
            std::fs::remove_dir_all(&alias).map_err(BoxError::IoError)?;
        }
    }
    if root.exists() {
        std::fs::remove_dir_all(&root).map_err(BoxError::IoError)?;
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
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

#[cfg(target_os = "windows")]
const BINDFLT_FLAG_READ_ONLY_MAPPING: u32 = 0x1;
#[cfg(target_os = "windows")]
const HRESULT_FROM_WIN32_FILE_NOT_FOUND: i32 = 0x8007_0002u32 as i32;
#[cfg(target_os = "windows")]
const HRESULT_FROM_WIN32_NOT_FOUND: i32 = 0x8007_0490u32 as i32;

#[cfg(target_os = "windows")]
fn bindflt_absent(hr: i32) -> bool {
    hr == HRESULT_FROM_WIN32_FILE_NOT_FOUND || hr == HRESULT_FROM_WIN32_NOT_FOUND
}

#[cfg(target_os = "windows")]
fn setup_bindflt_read_only(virtual_path: &Path, backing_path: &Path) -> Result<()> {
    let api = bindflt_api()?;
    let virtual_w = wide_path(virtual_path)?;
    let backing_w = wide_path(backing_path)?;
    let hr = unsafe {
        (api.setup_filter)(
            std::ptr::null_mut(),
            BINDFLT_FLAG_READ_ONLY_MAPPING,
            virtual_w.as_ptr(),
            backing_w.as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    if hr < 0 {
        return Err(BoxError::BoxBootError {
            message: format!(
                "Failed to create BindFlt read-only mapping for MicroVM :ro volume {} at {} (hr=0x{hr:08X})",
                backing_path.display(),
                virtual_path.display()
            ),
            hint: Some(
                "Windows BindFlt (bindfltapi.dll) is required for :ro virtio-fs write denial".into(),
            ),
        });
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn detach_bindflt_mapping(virtual_path: &Path) -> Result<()> {
    if !virtual_path.exists() {
        // Mapping may still exist for a deleted directory; still ask BindFlt.
    }
    let api = match bindflt_api() {
        Ok(api) => api,
        Err(_) if !virtual_path.exists() => return Ok(()),
        Err(error) => return Err(error),
    };
    let virtual_w = wide_path(virtual_path)?;
    let hr = unsafe { (api.remove_mapping)(std::ptr::null_mut(), virtual_w.as_ptr()) };
    if hr < 0 && !bindflt_absent(hr) {
        return Err(BoxError::StateError(format!(
            "Failed to detach BindFlt MicroVM :ro alias {} (hr=0x{hr:08X})",
            virtual_path.display()
        )));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn wide_path(path: &Path) -> Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.iter().any(|unit| *unit == 0) {
        return Err(BoxError::ConfigError(format!(
            "MicroVM :ro volume path contains NUL: {}",
            path.display()
        )));
    }
    wide.push(0);
    Ok(wide)
}

#[cfg(target_os = "windows")]
struct BindFltApi {
    // Keep the module loaded for the process lifetime of these function pointers.
    _module: windows_sys::Win32::Foundation::HMODULE,
    setup_filter: unsafe extern "system" fn(
        job: *mut core::ffi::c_void,
        flags: u32,
        virtual_path: *const u16,
        backing_path: *const u16,
        exceptions: *mut *mut u16,
        exception_count: u32,
    ) -> i32,
    remove_mapping:
        unsafe extern "system" fn(job: *mut core::ffi::c_void, virtual_path: *const u16) -> i32,
}

#[cfg(target_os = "windows")]
fn bindflt_api() -> Result<&'static BindFltApi> {
    use std::sync::OnceLock;
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    static API: OnceLock<std::result::Result<BindFltApi, String>> = OnceLock::new();
    let resolved = API.get_or_init(|| {
        let name: Vec<u16> = "bindfltapi.dll\0".encode_utf16().collect();
        let module = unsafe { LoadLibraryW(name.as_ptr()) };
        if module == 0 {
            return Err(format!(
                "LoadLibraryW(bindfltapi.dll) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let setup = unsafe { GetProcAddress(module, b"BfSetupFilter\0".as_ptr()) };
        let remove = unsafe { GetProcAddress(module, b"BfRemoveMapping\0".as_ptr()) };
        let (Some(setup), Some(remove)) = (setup, remove) else {
            return Err("bindfltapi.dll is missing BfSetupFilter/BfRemoveMapping".into());
        };
        Ok(BindFltApi {
            _module: module,
            setup_filter: unsafe { std::mem::transmute(setup) },
            remove_mapping: unsafe { std::mem::transmute(remove) },
        })
    });
    match resolved {
        Ok(api) => Ok(api),
        Err(message) => Err(BoxError::BoxBootError {
            message: format!("Windows MicroVM :ro host denial unavailable: {message}"),
            hint: Some(
                "Install/enable the Windows Bind Filter (bindfltapi.dll) for :ro volumes".into(),
            ),
        }),
    }
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

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    #[test]
    fn stages_read_only_virtiofs_alias_with_bindflt() {
        if bindflt_api().is_err() {
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
        assert!(std::fs::write(alias.join("y"), b"no").is_err());
        std::fs::write(source.join("live"), b"yes").unwrap();
        assert_eq!(std::fs::read(alias.join("live")).unwrap(), b"yes");

        cleanup_virtiofs_ro_shares(box_dir).unwrap();
        assert!(!ro_alias_root(&filemounts).exists());
        assert_eq!(std::fs::read(source.join("x")).unwrap(), b"data");
        assert!(std::fs::write(source.join("after"), b"ok").is_ok());
    }
}
