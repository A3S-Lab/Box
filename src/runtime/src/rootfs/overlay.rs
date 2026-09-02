//! Overlayfs mount/unmount operations.
//!
//! Provides host-side overlayfs mounts for CoW rootfs. On Linux 5.11+,
//! unprivileged overlayfs is available in user namespaces. Falls back to
//! `mount(2)` syscall or `mount` command.

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};

/// Directory and marker names used for a bounded Linux Sandbox writable layer.
///
/// The layer itself is a private, size-limited tmpfs. `upper` and `work` are
/// bind-mounted aliases kept at the historical Box paths so metadata and
/// lifecycle code do not need a second rootfs layout.
pub(crate) const WRITABLE_LAYER_DIR_NAME: &str = ".writable-layer";
pub(crate) const WRITABLE_LAYER_MARKER_NAME: &str = ".writable-layer-bytes";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MountInfo {
    pub root: PathBuf,
    pub mount_point: PathBuf,
    pub mount_options: String,
    pub filesystem: String,
    pub source: String,
    pub super_options: String,
}

/// Read one mountinfo record for an exact mount point.
///
/// `/proc/self/mountinfo` is used instead of comparing device IDs: bind mounts
/// intentionally retain the source device and therefore look like ordinary
/// directories to `stat(2)`.
#[cfg(target_os = "linux")]
pub(crate) fn mount_info(path: &Path) -> Option<MountInfo> {
    let wanted = path.to_string_lossy();
    let contents = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    contents
        .lines()
        .filter_map(parse_mountinfo_line)
        // A crashed/restarted owner can briefly leave stacked mounts at the
        // same path. The last record is the top-most mount and is the one a
        // subsequent unmount or validation will observe first.
        .rfind(|info| info.mount_point.to_string_lossy() == wanted)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn mount_info(_path: &Path) -> Option<MountInfo> {
    None
}

#[cfg(target_os = "linux")]
fn parse_mountinfo_line(line: &str) -> Option<MountInfo> {
    let (left, right) = line.split_once(" - ")?;
    let left = left.split_whitespace().collect::<Vec<_>>();
    let right = right.split_whitespace().collect::<Vec<_>>();
    // mountinfo fields 4 and 5 are root, mount point, and the per-mount
    // options respectively. The post-separator fields start with fstype.
    if left.len() < 6 || right.len() < 3 {
        return None;
    }
    Some(MountInfo {
        root: PathBuf::from(unescape_mountinfo(left[3])),
        mount_point: PathBuf::from(unescape_mountinfo(left[4])),
        mount_options: left[5].to_string(),
        filesystem: right[0].to_string(),
        source: unescape_mountinfo(right[1]).to_string(),
        super_options: right[2].to_string(),
    })
}

#[cfg(target_os = "linux")]
fn unescape_mountinfo(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

/// True when `path` is a mount point, including a bind mount.
pub(crate) fn is_mountpoint(path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        if mount_info(path).is_some() {
            return true;
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(path), std::fs::metadata(path.join(".."))) {
            (Ok(here), Ok(parent)) => here.dev() != parent.dev(),
            _ => false,
        }
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// Mount an overlayfs at `merged` with `lower` (read-only), `upper` (writes), `work`.
///
/// Tries in order:
/// 1. `mount(2)` syscall (requires CAP_SYS_ADMIN or unprivileged overlay)
/// 2. `mount` command as fallback
pub fn overlay_mount(lower: &Path, upper: &Path, work: &Path, merged: &Path) -> Result<()> {
    // overlayfs mount options are comma-delimited with no escaping, so a comma in
    // any path would be parsed as an option boundary and silently corrupt the
    // mount. Refuse instead. Box dirs are UUID-based today, so this never trips in
    // practice — it's a guard for any future user-controllable cache/box path.
    for path in [lower, upper, work] {
        if path.to_string_lossy().contains(',') {
            return Err(BoxError::BuildError(format!(
                "overlay path contains a comma, which overlayfs options cannot express: {}",
                path.display()
            )));
        }
    }

    let base_options = overlay_options(lower, upper, work, false);

    // Try mount(2) syscall first
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;

        // Metadata-only copy-up avoids copying every executable's contents
        // when Sandbox ownership is shifted for its user namespace. Restrict
        // it to the initial root namespace: rootless overlay uses user.*
        // private xattrs that an untrusted workload could forge. OCI ingestion
        // separately rejects both trusted.overlay.* and user.overlay.* xattrs.
        let metadata_options = overlay_options(lower, upper, work, true);
        let options = if unsafe { libc::geteuid() } == 0 {
            vec![(&metadata_options, true), (&base_options, false)]
        } else {
            vec![(&base_options, false)]
        };
        let source = CString::new("overlay").unwrap();
        let target = CString::new(merged.to_string_lossy().as_ref())
            .map_err(|e| BoxError::BuildError(format!("Invalid merged path for mount: {}", e)))?;
        let fstype = CString::new("overlay").unwrap();
        let mut failures = Vec::new();

        for &(options, metadata_copy) in &options {
            let data = CString::new(options.as_str()).map_err(|error| {
                BoxError::BuildError(format!("Invalid overlay mount options: {error}"))
            })?;
            let ret = unsafe {
                libc::mount(
                    source.as_ptr(),
                    target.as_ptr(),
                    fstype.as_ptr(),
                    0,
                    data.as_ptr() as *const libc::c_void,
                )
            };
            if ret == 0 {
                tracing::debug!(
                    lower = %lower.display(),
                    merged = %merged.display(),
                    metadata_copy,
                    "Overlay mounted via mount(2)"
                );
                return Ok(());
            }
            failures.push(format!(
                "mount(2), metacopy={metadata_copy}: {}",
                std::io::Error::last_os_error()
            ));
        }

        tracing::debug!(
            errors = ?failures,
            "mount(2) failed, trying mount command"
        );

        for &(options, metadata_copy) in &options {
            match std::process::Command::new("mount")
                .args(["-t", "overlay", "overlay", "-o", options])
                .arg(merged)
                .status()
            {
                Ok(status) if status.success() => {
                    tracing::debug!(
                        lower = %lower.display(),
                        merged = %merged.display(),
                        metadata_copy,
                        "Overlay mounted via mount command"
                    );
                    return Ok(());
                }
                Ok(status) => {
                    failures.push(format!("mount command, metacopy={metadata_copy}: {status}"))
                }
                Err(error) => {
                    failures.push(format!("mount command, metacopy={metadata_copy}: {error}"))
                }
            }
        }

        Err(BoxError::BuildError(format!(
            "Failed to mount overlayfs at {}: {}",
            merged.display(),
            failures.join("; ")
        )))
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (lower, upper, work, merged, base_options);
        Err(BoxError::BuildError(
            "Overlayfs is only supported on Linux".to_string(),
        ))
    }
}

fn overlay_options(lower: &Path, upper: &Path, work: &Path, metadata_copy: bool) -> String {
    let mut options = format!(
        "lowerdir={},upperdir={},workdir={}",
        lower.display(),
        upper.display(),
        work.display()
    );
    if metadata_copy {
        options.push_str(",metacopy=on");
    }
    options
}

/// Unmount an overlayfs at `merged`.
pub fn overlay_unmount(merged: &Path) -> Result<()> {
    overlay_unmount_with_mode(merged, true)
}

/// Synchronously unmount an overlayfs before reusing its writable layer.
///
/// A lazy detach is appropriate when a box is being discarded, but it can
/// leave the old mount alive through open namespace references. Reusing the
/// same upper directory before that mount is gone violates overlayfs' single
/// writer expectation and can hide writes from the replacement generation.
pub(crate) fn overlay_unmount_for_reuse(merged: &Path) -> Result<()> {
    overlay_unmount_with_mode(merged, false)
}

fn overlay_unmount_with_mode(merged: &Path, lazy: bool) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;

        let target = CString::new(merged.to_string_lossy().as_ref())
            .map_err(|e| BoxError::BuildError(format!("Invalid path for umount: {}", e)))?;

        let flags = if lazy { libc::MNT_DETACH } else { 0 };
        let ret = unsafe { libc::umount2(target.as_ptr(), flags) };

        if ret == 0 {
            tracing::debug!(path = %merged.display(), lazy, "Overlay unmounted");
            return Ok(());
        }

        let errno = std::io::Error::last_os_error();

        // Fallback: try `umount` command
        let mut command = std::process::Command::new("umount");
        if lazy {
            command.arg("-l");
        }
        let status = command
            .arg(merged)
            .status()
            .map_err(|e| BoxError::BuildError(format!("Failed to run umount command: {}", e)))?;

        if status.success() {
            tracing::debug!(path = %merged.display(), lazy, "Overlay unmounted via umount command");
            return Ok(());
        }

        Err(BoxError::BuildError(format!(
            "Failed to {}unmount overlayfs at {}: umount2 returned {}, umount command exited with {}",
            if lazy { "lazily " } else { "synchronously " },
            merged.display(),
            errno,
            status
        )))
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (merged, lazy);
        Ok(())
    }
}

/// Host-side mount for a bounded writable layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundedWritableLayer {
    /// The private tmpfs mount which owns both directories.
    pub mount: PathBuf,
}

/// Prepare a size-limited writable layer for the Linux Sandbox provider.
///
/// A tmpfs parent owns `upper` and `work`; bind aliases preserve the existing
/// Box layout (`box_dir/upper` and `box_dir/work`). This gives overlayfs a
/// genuinely bounded filesystem rather than merely recording a requested
/// number in metadata. Existing mounts are reused only when their recorded
/// quota matches exactly; a changed quota fails closed.
pub(crate) fn prepare_bounded_writable_layer(
    box_dir: &Path,
    bytes: u64,
) -> Result<BoundedWritableLayer> {
    if bytes == 0 {
        return Err(BoxError::ConfigError(
            "Sandbox writable-layer quota must be greater than zero".into(),
        ));
    }
    if !writable_layer_quota_supported() {
        return Err(BoxError::ConfigError(
            "Linux Sandbox writable-layer quotas require root tmpfs and overlayfs support".into(),
        ));
    }

    std::fs::create_dir_all(box_dir).map_err(|error| {
        BoxError::BuildError(format!(
            "Failed to create Box directory {} for bounded writable layer: {error}",
            box_dir.display()
        ))
    })?;

    let mount = box_dir.join(WRITABLE_LAYER_DIR_NAME);
    let marker = box_dir.join(WRITABLE_LAYER_MARKER_NAME);
    let upper_source = mount.join("upper");
    let work_source = mount.join("work");
    let upper = box_dir.join("upper");
    let work = box_dir.join("work");

    let marker_present = validate_existing_quota_marker(&marker, bytes, &mount)?;
    ensure_directory_path(&mount, "bounded writable-layer mount")?;

    if is_mountpoint(&mount) {
        let info = mount_info(&mount).ok_or_else(|| {
            BoxError::StateError(format!(
                "Cannot inspect bounded writable-layer mount {}",
                mount.display()
            ))
        })?;
        validate_tmpfs_size(&info, bytes, &mount)?;
    } else {
        if marker_present {
            return Err(BoxError::StateError(format!(
                "Writable-layer quota marker {} exists but its tmpfs mount {} is absent; refusing to recreate a potentially lost persistent generation",
                marker.display(),
                mount.display()
            )));
        }
        let mut entries = std::fs::read_dir(&mount).map_err(|error| {
            BoxError::BuildError(format!(
                "Failed to inspect bounded writable-layer directory {}: {error}",
                mount.display()
            ))
        })?;
        if entries.next().is_some() {
            return Err(BoxError::StateError(format!(
                "Unmanaged files remain in {} where a bounded tmpfs would be mounted",
                mount.display()
            )));
        }
        mount_tmpfs(&mount, bytes)?;
    }

    ensure_directory_path(&upper_source, "bounded writable-layer upper")?;
    ensure_directory_path(&work_source, "bounded writable-layer work")?;
    ensure_directory_path(&upper, "overlay upper alias")?;
    ensure_directory_path(&work, "overlay work alias")?;

    bind_alias(&upper_source, &upper)?;
    if let Err(error) = bind_alias(&work_source, &work) {
        let _ = unmount_path(&upper, true);
        return Err(error);
    }

    // The marker lives outside tmpfs so a process restart can validate the
    // quota before touching the mount. Write it only after all mounts succeed.
    write_quota_marker(&marker, bytes)?;

    Ok(BoundedWritableLayer { mount })
}

/// Release a bounded writable layer. Persistent boxes retain the tmpfs and
/// aliases so their overlay contents survive stop/start; non-persistent boxes
/// synchronously detach all mounts before their directory is removed.
pub(crate) fn cleanup_bounded_writable_layer(box_dir: &Path, persistent: bool) -> Result<()> {
    let mount = box_dir.join(WRITABLE_LAYER_DIR_NAME);
    let marker = box_dir.join(WRITABLE_LAYER_MARKER_NAME);
    let upper = box_dir.join("upper");
    let work = box_dir.join("work");

    let managed =
        marker.exists() || is_mountpoint(&mount) || is_mountpoint(&upper) || is_mountpoint(&work);
    if !managed {
        return Ok(());
    }
    if persistent {
        // The mounted tmpfs is the durable writable generation for this host.
        // Keeping it mounted avoids copying potentially large data out of the
        // quota filesystem and lets the next boot reattach the same aliases.
        return Ok(());
    }

    let mut first_error = None;
    // Overlay must already be detached by the provider. Detach aliases before
    // the tmpfs parent so the source directories remain reachable during
    // unmount.
    for path in [&work, &upper, &mount] {
        for _ in 0..8 {
            if !is_mountpoint(path) {
                break;
            }
            match unmount_path(path, false) {
                Ok(()) => continue,
                Err(synchronous) => {
                    // A discarded box has no future writer; a lazy detach is
                    // a safe last resort, but retain the synchronous failure
                    // if both paths fail so the caller can retry instead of
                    // claiming clean removal.
                    if unmount_path(path, true).is_err() {
                        if first_error.is_none() {
                            first_error = Some(synchronous);
                        }
                        break;
                    }
                }
            }
        }
        if is_mountpoint(path) && first_error.is_none() {
            first_error = Some(BoxError::StateError(format!(
                "Writable-layer mount {} remained attached after cleanup",
                path.display()
            )));
        }
    }

    // Keep the marker when any mount is still attached: it is the durable
    // signal that cleanup must be retried and prevents a later boot from
    // treating an orphaned tmpfs as an untracked directory.
    if ![&work, &upper, &mount]
        .iter()
        .any(|path| is_mountpoint(path))
    {
        if let Err(error) = std::fs::remove_file(&marker) {
            if error.kind() != std::io::ErrorKind::NotFound && first_error.is_none() {
                first_error = Some(BoxError::BuildError(format!(
                    "Failed to remove writable-layer quota marker {}: {error}",
                    marker.display()
                )));
            }
        }
    }
    if let Some(error) = first_error {
        return Err(error);
    }
    Ok(())
}

fn validate_existing_quota_marker(marker: &Path, bytes: u64, mount: &Path) -> Result<bool> {
    let metadata = match std::fs::symlink_metadata(marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(BoxError::StateError(format!(
                "Failed to inspect writable-layer quota marker {}: {error}",
                marker.display()
            )))
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(BoxError::StateError(format!(
            "Writable-layer quota marker {} is not a regular file",
            marker.display()
        )));
    }
    let contents = match std::fs::read_to_string(marker) {
        Ok(contents) => contents,
        Err(error) => {
            return Err(BoxError::StateError(format!(
                "Failed to read writable-layer quota marker {}: {error}",
                marker.display()
            )))
        }
    };
    let recorded = contents.trim().parse::<u64>().map_err(|error| {
        BoxError::StateError(format!(
            "Invalid writable-layer quota marker {}: {error}",
            marker.display()
        ))
    })?;
    if recorded != bytes {
        return Err(BoxError::ConfigError(format!(
            "Writable-layer quota for {} is retained at {recorded} bytes; requested {bytes} bytes",
            mount.display()
        )));
    }
    Ok(true)
}

fn write_quota_marker(marker: &Path, bytes: u64) -> Result<()> {
    let temporary = marker.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, format!("{bytes}\n")).map_err(|error| {
        BoxError::BuildError(format!(
            "Failed to write writable-layer quota marker {}: {error}",
            temporary.display()
        ))
    })?;
    if let Err(error) = std::fs::rename(&temporary, marker) {
        let _ = std::fs::remove_file(&temporary);
        return Err(BoxError::BuildError(format!(
            "Failed to publish writable-layer quota marker {}: {error}",
            marker.display()
        )));
    }
    Ok(())
}

fn ensure_directory_path(path: &Path, label: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => Err(
            BoxError::StateError(format!("{label} {} is not a directory", path.display())),
        ),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir_all(path)
            .map_err(|error| {
                BoxError::BuildError(format!(
                    "Failed to create {label} {}: {error}",
                    path.display()
                ))
            }),
        Err(error) => Err(BoxError::BuildError(format!(
            "Failed to inspect {label} {}: {error}",
            path.display()
        ))),
    }
}

fn validate_tmpfs_size(info: &MountInfo, bytes: u64, mount: &Path) -> Result<()> {
    if info.filesystem != "tmpfs" {
        return Err(BoxError::StateError(format!(
            "Bounded writable-layer path {} is mounted as {}, not tmpfs",
            mount.display(),
            info.filesystem
        )));
    }
    let recorded_size = info
        .super_options
        .split(',')
        .chain(info.mount_options.split(','))
        .find_map(|option| option.strip_prefix("size="))
        .and_then(parse_size_bytes);
    if recorded_size != Some(bytes) {
        return Err(BoxError::StateError(format!(
            "Bounded writable-layer tmpfs {} has size {:?}, expected {bytes} bytes",
            mount.display(),
            recorded_size
        )));
    }
    Ok(())
}

fn parse_size_bytes(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let (number, multiplier) = match value.as_bytes().last().copied() {
        Some(b'k' | b'K') => (&value[..value.len() - 1], 1024),
        Some(b'm' | b'M') => (&value[..value.len() - 1], 1024 * 1024),
        Some(b'g' | b'G') => (&value[..value.len() - 1], 1024 * 1024 * 1024),
        Some(b't' | b'T') => (&value[..value.len() - 1], 1024_u64.pow(4)),
        _ => (value, 1),
    };
    number.parse::<u64>().ok()?.checked_mul(multiplier)
}

#[cfg(target_os = "linux")]
fn mount_tmpfs(target: &Path, bytes: u64) -> Result<()> {
    use std::ffi::CString;

    let source = CString::new("tmpfs").unwrap();
    let target_string = target.to_string_lossy();
    let target_c = CString::new(target_string.as_ref())
        .map_err(|error| BoxError::BuildError(format!("Invalid tmpfs target path: {error}")))?;
    let fstype = CString::new("tmpfs").unwrap();
    let data = CString::new(format!("size={bytes},mode=0700,nosuid,nodev"))
        .map_err(|error| BoxError::BuildError(format!("Invalid tmpfs mount options: {error}")))?;
    let flags = libc::MS_NOSUID | libc::MS_NODEV;
    let ret = unsafe {
        libc::mount(
            source.as_ptr(),
            target_c.as_ptr(),
            fstype.as_ptr(),
            flags,
            data.as_ptr() as *const libc::c_void,
        )
    };
    if ret == 0 {
        return Ok(());
    }
    let syscall_error = std::io::Error::last_os_error();
    let status = std::process::Command::new("mount")
        .args(["-t", "tmpfs", "tmpfs", "-o"])
        .arg(format!("size={bytes},mode=0700,nosuid,nodev"))
        .arg(target)
        .status()
        .map_err(|error| {
            BoxError::BuildError(format!("Failed to run tmpfs mount command: {error}"))
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(BoxError::BuildError(format!(
            "Failed to mount bounded writable-layer tmpfs at {}: mount(2) returned {}; mount command exited with {}",
            target.display(), syscall_error, status
        )))
    }
}

#[cfg(not(target_os = "linux"))]
fn mount_tmpfs(_target: &Path, _bytes: u64) -> Result<()> {
    Err(BoxError::ConfigError(
        "Bounded writable-layer tmpfs is only supported on Linux".into(),
    ))
}

fn bind_alias(source: &Path, target: &Path) -> Result<()> {
    if is_mountpoint(target) {
        let info = mount_info(target).ok_or_else(|| {
            BoxError::StateError(format!(
                "Cannot inspect existing overlay alias mount {}",
                target.display()
            ))
        })?;
        if info.filesystem != "tmpfs" {
            return Err(BoxError::StateError(format!(
                "Overlay alias {} is mounted as {}, expected tmpfs",
                target.display(),
                info.filesystem
            )));
        }
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        let source_c = CString::new(source.to_string_lossy().as_ref()).map_err(|error| {
            BoxError::BuildError(format!("Invalid writable-layer source path: {error}"))
        })?;
        let target_c = CString::new(target.to_string_lossy().as_ref()).map_err(|error| {
            BoxError::BuildError(format!("Invalid writable-layer alias path: {error}"))
        })?;
        let ret = unsafe {
            libc::mount(
                source_c.as_ptr(),
                target_c.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND,
                std::ptr::null(),
            )
        };
        if ret == 0 {
            return Ok(());
        }
        let syscall_error = std::io::Error::last_os_error();
        let status = std::process::Command::new("mount")
            .arg("--bind")
            .arg(source)
            .arg(target)
            .status()
            .map_err(|error| {
                BoxError::BuildError(format!("Failed to run bind mount command: {error}"))
            })?;
        if status.success() {
            Ok(())
        } else {
            Err(BoxError::BuildError(format!(
                "Failed to bind writable-layer alias {} -> {}: mount(2) returned {}; mount command exited with {}",
                target.display(),
                source.display(),
                syscall_error,
                status
            )))
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (source, target);
        Err(BoxError::ConfigError(
            "Bounded writable-layer aliases are only supported on Linux".into(),
        ))
    }
}

fn unmount_path(path: &Path, lazy: bool) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        let target = CString::new(path.to_string_lossy().as_ref()).map_err(|error| {
            BoxError::BuildError(format!("Invalid path for writable-layer unmount: {error}"))
        })?;
        let flags = if lazy { libc::MNT_DETACH } else { 0 };
        let ret = unsafe { libc::umount2(target.as_ptr(), flags) };
        if ret == 0 {
            return Ok(());
        }
        let syscall_error = std::io::Error::last_os_error();
        let mut command = std::process::Command::new("umount");
        if lazy {
            command.arg("-l");
        }
        let status = command.arg(path).status().map_err(|error| {
            BoxError::BuildError(format!(
                "Failed to run writable-layer umount command: {error}"
            ))
        })?;
        if status.success() {
            Ok(())
        } else {
            Err(BoxError::BuildError(format!(
                "Failed to {}unmount writable-layer path {}: umount2 returned {}; umount command exited with {}",
                if lazy { "lazily " } else { "synchronously " },
                path.display(),
                syscall_error,
                status
            )))
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (path, lazy);
        Ok(())
    }
}

/// Check if overlayfs is supported on this system.
///
/// Always returns `false` on non-Linux platforms (compile-time).
#[cfg(target_os = "linux")]
pub(crate) fn is_overlay_supported() -> bool {
    static OVERLAY_SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

    cached_overlay_support(&OVERLAY_SUPPORTED, probe_overlay_support)
}

#[cfg(target_os = "linux")]
fn cached_overlay_support(cache: &std::sync::OnceLock<bool>, probe: impl FnOnce() -> bool) -> bool {
    *cache.get_or_init(probe)
}

#[cfg(target_os = "linux")]
fn probe_overlay_support() -> bool {
    // Check /proc/filesystems for overlay support
    if let Ok(fs_list) = std::fs::read_to_string("/proc/filesystems") {
        if !fs_list.contains("overlay") {
            tracing::debug!("Overlay not listed in /proc/filesystems");
            return false;
        }
    } else {
        return false;
    }

    // Try a test mount in a tempdir to verify we have permission
    let tmp = match tempfile::TempDir::new() {
        Ok(t) => t,
        Err(_) => return false,
    };

    let lower = tmp.path().join("lower");
    let upper = tmp.path().join("upper");
    let work = tmp.path().join("work");
    let merged = tmp.path().join("merged");

    for dir in [&lower, &upper, &work, &merged] {
        if std::fs::create_dir_all(dir).is_err() {
            return false;
        }
    }

    let ok = overlay_mount(&lower, &upper, &work, &merged).is_ok();
    if ok {
        let _ = overlay_unmount(&merged);
    }
    ok
}

/// Check if overlayfs is supported on this system.
///
/// Always returns `false` on non-Linux platforms.
#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
pub(crate) fn is_overlay_supported() -> bool {
    false
}

/// Check whether this host can enforce a byte-precise Sandbox writable-layer
/// quota. The probe requires the same privileges and filesystems used by the
/// production path; callers must not advertise the capability on a mere
/// kernel-version guess.
pub(crate) fn writable_layer_quota_supported() -> bool {
    #[cfg(target_os = "linux")]
    {
        static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *SUPPORTED.get_or_init(probe_writable_layer_quota_support)
    }

    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(target_os = "linux")]
fn probe_writable_layer_quota_support() -> bool {
    if unsafe { libc::geteuid() } != 0 || !is_overlay_supported() {
        return false;
    }

    let temp = match tempfile::TempDir::new() {
        Ok(temp) => temp,
        Err(_) => return false,
    };
    let mount = temp.path().join("quota");
    let lower = temp.path().join("lower");
    let merged = temp.path().join("merged");
    if std::fs::create_dir_all(&mount).is_err()
        || std::fs::create_dir_all(&lower).is_err()
        || std::fs::create_dir_all(&merged).is_err()
    {
        return false;
    }
    // Exercise the exact layout used by production, not only an isolated
    // tmpfs mount. OverlayFS requires upperdir and workdir to resolve through
    // the same mount; this catches kernels that reject a bounded layer before
    // the provider advertises EphemeralStorage.
    let bytes = 4 * 1024 * 1024;
    if mount_tmpfs(&mount, bytes).is_err() {
        return false;
    }
    let upper = mount.join("upper");
    let work = mount.join("work");
    let layout_ready =
        std::fs::create_dir_all(&upper).is_ok() && std::fs::create_dir_all(&work).is_ok();
    let overlay_mounted = layout_ready && overlay_mount(&lower, &upper, &work, &merged).is_ok();
    let overlay_sync_ok = if overlay_mounted {
        // The parent tmpfs is torn down immediately below. A lazy detach can
        // leave the overlay mount alive through an open namespace reference,
        // making a healthy host look unsupported because the tmpfs is still
        // busy. Use the same synchronous path required before layer reuse.
        let sync_ok = overlay_unmount_for_reuse(&merged).is_ok();
        if !sync_ok {
            // Do not leave a probe mount behind if the strict cleanup path
            // fails. The result remains false so callers fail closed.
            let _ = overlay_unmount(&merged);
        }
        sync_ok && !is_mountpoint(&merged)
    } else {
        false
    };
    let tmpfs_sync_ok = unmount_path(&mount, false).is_ok();
    if !tmpfs_sync_ok {
        let _ = unmount_path(&mount, true);
    }
    let tmpfs_unmounted = !is_mountpoint(&mount);
    overlay_mounted && overlay_sync_ok && tmpfs_sync_ok && tmpfs_unmounted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_overlay_supported_returns_bool() {
        // Just verify it doesn't panic
        let _supported = is_overlay_supported();
    }

    #[test]
    fn quota_size_parser_accepts_binary_suffixes() {
        assert_eq!(parse_size_bytes("4096"), Some(4096));
        assert_eq!(parse_size_bytes("4k"), Some(4096));
        assert_eq!(parse_size_bytes("4M"), Some(4 * 1024 * 1024));
        assert_eq!(parse_size_bytes("2g"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_size_bytes("bad"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mountinfo_parser_decodes_paths_and_filesystem() {
        let line =
            "42 1 0:44 /source\\040dir /target\\040dir rw,relatime - tmpfs tmpfs rw,size=4096";
        let info = parse_mountinfo_line(line).unwrap();
        assert_eq!(info.root, PathBuf::from("/source dir"));
        assert_eq!(info.mount_point, PathBuf::from("/target dir"));
        assert_eq!(info.filesystem, "tmpfs");
        assert_eq!(info.super_options, "rw,size=4096");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn concurrent_overlay_support_queries_probe_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Barrier, OnceLock};

        const THREADS: usize = 8;
        let cache = Arc::new(OnceLock::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(THREADS));

        std::thread::scope(|scope| {
            let handles = (0..THREADS)
                .map(|_| {
                    let cache = cache.clone();
                    let calls = calls.clone();
                    let barrier = barrier.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        cached_overlay_support(&cache, || {
                            calls.fetch_add(1, Ordering::SeqCst);
                            std::thread::sleep(std::time::Duration::from_millis(20));
                            true
                        })
                    })
                })
                .collect::<Vec<_>>();

            for handle in handles {
                assert!(handle.join().unwrap());
            }
        });

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn test_overlay_not_supported_on_non_linux() {
        assert!(!is_overlay_supported());
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn test_overlay_mount_fails_on_non_linux() {
        let tmp = tempfile::TempDir::new().unwrap();
        let result = overlay_mount(
            &tmp.path().join("l"),
            &tmp.path().join("u"),
            &tmp.path().join("w"),
            &tmp.path().join("m"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_overlay_mount_rejects_comma_in_mount_option_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        let lower = tmp.path().join("lower,with-comma");
        let upper = tmp.path().join("upper");
        let work = tmp.path().join("work");
        let merged = tmp.path().join("merged");

        let err = overlay_mount(&lower, &upper, &work, &merged).unwrap_err();

        assert!(err.to_string().contains("contains a comma"));
        assert!(err.to_string().contains("lower,with-comma"));
    }

    #[test]
    fn metadata_copy_option_is_explicit() {
        let lower = Path::new("/cache/lower");
        let upper = Path::new("/box/upper");
        let work = Path::new("/box/work");

        assert_eq!(
            overlay_options(lower, upper, work, false),
            "lowerdir=/cache/lower,upperdir=/box/upper,workdir=/box/work"
        );
        assert_eq!(
            overlay_options(lower, upper, work, true),
            "lowerdir=/cache/lower,upperdir=/box/upper,workdir=/box/work,metacopy=on"
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn test_overlay_unmount_noop_on_non_linux() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(overlay_unmount(tmp.path()).is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_overlay_mount_and_unmount() {
        if !is_overlay_supported() {
            // Skip in environments without overlay support
            return;
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let lower = tmp.path().join("lower");
        let upper = tmp.path().join("upper");
        let work = tmp.path().join("work");
        let merged = tmp.path().join("merged");

        for dir in [&lower, &upper, &work, &merged] {
            std::fs::create_dir_all(dir).unwrap();
        }

        // Create a file in lower
        std::fs::write(lower.join("hello.txt"), "from lower").unwrap();

        // Mount
        overlay_mount(&lower, &upper, &work, &merged).unwrap();

        // Verify lower file visible in merged
        assert_eq!(
            std::fs::read_to_string(merged.join("hello.txt")).unwrap(),
            "from lower"
        );

        // Write to merged — should go to upper
        std::fs::write(merged.join("new.txt"), "from upper").unwrap();
        assert!(upper.join("new.txt").exists());

        // Unmount
        overlay_unmount(&merged).unwrap();
    }
}
