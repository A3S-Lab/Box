//! Guest filesystem and virtio-fs mount lifecycle.

use super::*;

/// Mount essential filesystems (/proc, /sys, /dev).
pub(super) fn mount_essential_filesystems() -> Result<(), Box<dyn std::error::Error>> {
    info!("Mounting essential filesystems");

    // Note: mount() signature differs between Linux and macOS in nix crate
    // On Linux: mount(source, target, fstype, flags, data)
    // On macOS: mount(source, target, flags, data)
    // This code is meant to run on Linux inside the VM

    #[cfg(target_os = "linux")]
    {
        use nix::mount::{mount, MsFlags};

        // Mount /proc (ignore EBUSY — kernel may have already mounted it)
        match mount(
            Some("proc"),
            "/proc",
            Some("proc"),
            MsFlags::empty(),
            None::<&str>,
        ) {
            Ok(()) => {}
            Err(nix::errno::Errno::EBUSY) => {
                info!("/proc already mounted, skipping");
            }
            Err(e) => return Err(e.into()),
        }

        // Mount /sys (ignore EBUSY)
        match mount(
            Some("sysfs"),
            "/sys",
            Some("sysfs"),
            MsFlags::empty(),
            None::<&str>,
        ) {
            Ok(()) => {}
            Err(nix::errno::Errno::EBUSY) => {
                info!("/sys already mounted, skipping");
            }
            Err(e) => return Err(e.into()),
        }

        // Mount /dev (devtmpfs, ignore EBUSY)
        match mount(
            Some("devtmpfs"),
            "/dev",
            Some("devtmpfs"),
            MsFlags::empty(),
            None::<&str>,
        ) {
            Ok(()) => {}
            Err(nix::errno::Errno::EBUSY) => {
                info!("/dev already mounted, skipping");
            }
            Err(e) => return Err(e.into()),
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        // On non-Linux platforms (e.g., macOS for development),
        // skip mounting as this code won't actually run
        info!("Skipping mount on non-Linux platform (development mode)");
    }

    Ok(())
}

/// Mount devpts for guest-side PTY allocation.
#[cfg(target_os = "linux")]
pub(super) fn mount_devpts() -> Result<(), Box<dyn std::error::Error>> {
    use nix::mount::{mount, MsFlags};

    std::fs::create_dir_all("/dev/pts")?;
    match mount(
        Some("devpts"),
        "/dev/pts",
        Some("devpts"),
        MsFlags::empty(),
        Some("mode=0620,ptmxmode=0666"),
    ) {
        Ok(()) => {
            info!("Mounted devpts at /dev/pts");
            Ok(())
        }
        Err(nix::errno::Errno::EBUSY) => {
            info!("/dev/pts already mounted, skipping");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) fn mount_devpts() -> Result<(), Box<dyn std::error::Error>> {
    Ok(())
}

const DEFAULT_SHM_SIZE_BYTES: u64 = 64 * 1024 * 1024;

pub(super) fn default_shm_mount_options() -> String {
    format!("mode=1777,size={DEFAULT_SHM_SIZE_BYTES}")
}

/// Mount the Docker-compatible default shared-memory filesystem.
///
/// devtmpfs exposes `/dev/shm` only as a root-owned 0755 directory. A
/// container expects a 64 MiB tmpfs with sticky world-writable permissions,
/// even when no explicit `--shm-size` was supplied. Explicit volume and
/// tmpfs declarations are mounted afterward and can replace this default.
pub(super) fn mount_default_shm() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "linux")]
    {
        use nix::mount::{mount, MsFlags};

        std::fs::create_dir_all("/dev/shm")?;
        let options = default_shm_mount_options();
        match mount(
            None::<&str>,
            "/dev/shm",
            Some("tmpfs"),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
            Some(options.as_str()),
        ) {
            Ok(()) => info!("Mounted default shared memory at /dev/shm"),
            Err(nix::errno::Errno::EBUSY) => {
                info!("/dev/shm already mounted, keeping existing mount")
            }
            Err(error) => return Err(error.into()),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = default_shm_mount_options();
    }
    Ok(())
}

/// Select the MicroVM root transport and mount the private boot share.
/// Workload-controlled shares are deliberately mounted later, after guest
/// host files have been materialized into the rootfs.
pub(super) fn mount_virtio_fs_shares() -> Result<(), Box<dyn std::error::Error>> {
    info!("Mounting MicroVM root and boot-control shares");

    #[cfg(target_os = "linux")]
    {
        root_transport::prepare_current_root()?;
        if std::env::var(GUEST_ROOTFS_MAINTENANCE_ENV).as_deref() != Ok("1") {
            mount_guest_terminal_control()?;
        }
        mount_guest_boot_control()?;
    }

    #[cfg(not(target_os = "linux"))]
    {
        info!("Skipping virtio-fs mount on non-Linux platform (development mode)");
    }

    Ok(())
}

/// Mount the sole maintenance disk with two independent read-only fences:
/// libkrun exposes the block device read-only, and ext4 is mounted with both
/// `MS_RDONLY` and `noload` so this observational path cannot replay a journal.
#[cfg(target_os = "linux")]
pub(super) fn mount_rootfs_maintenance_device() -> Result<(), Box<dyn std::error::Error>> {
    use a3s_box_core::guest_exec::{
        GUEST_ROOTFS_MAINTENANCE_DEVICE_ENV, GUEST_ROOTFS_MAINTENANCE_MOUNT_PATH,
    };
    use a3s_box_core::vmm::GUEST_EXT4_ROOT_DEVICE;
    use nix::mount::{mount, MsFlags};

    let device = std::env::var(GUEST_ROOTFS_MAINTENANCE_DEVICE_ENV)
        .map_err(|_| format!("missing {GUEST_ROOTFS_MAINTENANCE_DEVICE_ENV}"))?;
    if device != GUEST_EXT4_ROOT_DEVICE {
        return Err(format!(
            "unsupported {GUEST_ROOTFS_MAINTENANCE_DEVICE_ENV} value {device:?}; expected {GUEST_EXT4_ROOT_DEVICE:?}"
        )
        .into());
    }
    std::fs::create_dir_all(GUEST_ROOTFS_MAINTENANCE_MOUNT_PATH)?;
    mount(
        Some(device.as_str()),
        GUEST_ROOTFS_MAINTENANCE_MOUNT_PATH,
        Some("ext4"),
        MsFlags::MS_RDONLY | MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some("ro,noload"),
    )
    .map_err(|error| {
        let devices = std::fs::read_dir("/sys/class/block")
            .map(|entries| {
                let mut names = entries
                    .flatten()
                    .filter_map(|entry| entry.file_name().into_string().ok())
                    .collect::<Vec<_>>();
                names.sort();
                names.join(",")
            })
            .unwrap_or_else(|_| "unavailable".to_string());
        format!(
            "ext4 mount of {device} at {GUEST_ROOTFS_MAINTENANCE_MOUNT_PATH} with ro,noload failed: {error}; guest block devices: {devices}"
        )
    })?;
    info!(
        device,
        path = GUEST_ROOTFS_MAINTENANCE_MOUNT_PATH,
        "Mounted rootfs maintenance disk read-only without journal replay"
    );
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(super) fn mount_rootfs_maintenance_device() -> Result<(), Box<dyn std::error::Error>> {
    Err("rootfs maintenance requires Linux guest-init".into())
}

#[cfg(target_os = "linux")]
pub(super) fn unmount_rootfs_maintenance_device() -> Result<(), Box<dyn std::error::Error>> {
    use a3s_box_core::guest_exec::GUEST_ROOTFS_MAINTENANCE_MOUNT_PATH;
    use nix::mount::{umount2, MntFlags};

    umount2(GUEST_ROOTFS_MAINTENANCE_MOUNT_PATH, MntFlags::empty())?;
    info!(
        path = GUEST_ROOTFS_MAINTENANCE_MOUNT_PATH,
        "Unmounted rootfs maintenance disk"
    );
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(super) fn unmount_rootfs_maintenance_device() -> Result<(), Box<dyn std::error::Error>> {
    Ok(())
}

/// Mount shares that are visible to the workload only after guest-init has
/// finished writing host-controlled rootfs files.
pub(super) fn mount_workload_virtio_fs_shares() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "linux")]
    {
        use nix::mount::MsFlags;

        std::fs::create_dir_all("/workspace")?;
        mount_virtiofs("workspace", "/workspace", MsFlags::empty())?;
        mount_user_volumes()?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn mount_guest_boot_control() -> Result<(), Box<dyn std::error::Error>> {
    use nix::mount::MsFlags;

    let Some(path) = std::env::var_os(GUEST_BOOT_CONFIG_ENV) else {
        return Ok(());
    };
    if path != std::ffi::OsStr::new(GUEST_BOOT_CONFIG_PATH) {
        return Err(format!("unsupported {GUEST_BOOT_CONFIG_ENV} path {:?}", path).into());
    }

    std::fs::create_dir_all(GUEST_BOOT_CONTROL_MOUNT_PATH)?;
    mount_virtiofs(
        GUEST_BOOT_CONTROL_TAG,
        GUEST_BOOT_CONTROL_MOUNT_PATH,
        MsFlags::MS_RDONLY | MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
    )?;
    info!(
        tag = GUEST_BOOT_CONTROL_TAG,
        path = GUEST_BOOT_CONTROL_MOUNT_PATH,
        "Mounted private guest boot control share"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn unmount_guest_boot_control() -> Result<(), Box<dyn std::error::Error>> {
    use nix::mount::{umount2, MntFlags};

    umount2(GUEST_BOOT_CONTROL_MOUNT_PATH, MntFlags::empty())?;
    std::fs::remove_dir(GUEST_BOOT_CONTROL_MOUNT_PATH)?;
    info!(
        path = GUEST_BOOT_CONTROL_MOUNT_PATH,
        "Consumed and unmounted private guest boot control share"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn mount_guest_terminal_control() -> Result<(), Box<dyn std::error::Error>> {
    use nix::mount::{umount2, MntFlags, MsFlags};

    std::fs::create_dir_all(GUEST_TERMINAL_CONTROL_MOUNT_PATH)?;
    mount_virtiofs(
        GUEST_TERMINAL_CONTROL_TAG,
        GUEST_TERMINAL_CONTROL_MOUNT_PATH,
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
    )?;
    let acquire_result = terminal_status::acquire(std::path::Path::new(GUEST_TERMINAL_STATUS_PATH));
    let baseline_result =
        diff_baseline::acquire_if_present(std::path::Path::new(GUEST_DIFF_BASELINE_PATH));
    // virtio-fs may report EBUSY while the retained status descriptor is
    // open. A lazy detach removes the share from the workload namespace
    // immediately while keeping that one already-open handle valid for PID 1.
    let unmount_result = umount2(GUEST_TERMINAL_CONTROL_MOUNT_PATH, MntFlags::MNT_DETACH);
    let remove_result = std::fs::remove_dir(GUEST_TERMINAL_CONTROL_MOUNT_PATH);

    acquire_result?;
    let baseline_enabled = baseline_result?;
    unmount_result?;
    remove_result?;
    info!(
        tag = GUEST_TERMINAL_CONTROL_TAG,
        baseline_enabled, "Opened and unmounted private guest terminal control share"
    );
    Ok(())
}

pub(super) fn virtiofs_mount_options_from_env_value(value: Option<&str>) -> Option<String> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        Some("default") => None,
        Some(mode) => Some(format!("cache={mode}")),
        None => Some("cache=none".to_string()),
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn virtiofs_mount_options() -> Option<String> {
    virtiofs_mount_options_from_env_value(std::env::var("A3S_VIRTIOFS_CACHE").ok().as_deref())
}

#[cfg(target_os = "linux")]
pub(super) fn mount_virtiofs(
    tag: &str,
    target: &str,
    flags: nix::mount::MsFlags,
) -> Result<(), Box<dyn std::error::Error>> {
    use nix::mount::mount;

    if let Some(options) = virtiofs_mount_options() {
        match mount(
            Some(tag),
            target,
            Some("virtiofs"),
            flags,
            Some(options.as_str()),
        ) {
            Ok(()) => return Ok(()),
            Err(error) => {
                warn!(
                    tag = tag,
                    target = target,
                    options = options,
                    error = %error,
                    "virtio-fs mount with explicit cache mode failed; retrying with the kernel default"
                );
            }
        }
    }

    mount(Some(tag), target, Some("virtiofs"), flags, None::<&str>)?;
    Ok(())
}

/// Mount user-defined volumes passed via BOX_VOL_* environment variables.
///
/// Each variable has the format: `<tag>:<guest_path>[:ro]`
#[cfg(target_os = "linux")]
pub(super) fn mount_user_volumes() -> Result<(), Box<dyn std::error::Error>> {
    use nix::mount::{mount, MsFlags};

    let mut index = 0;
    loop {
        let env_key = format!("BOX_VOL_{}", index);
        match std::env::var(&env_key) {
            Ok(value) => {
                let parts: Vec<&str> = value.split(':').collect();
                if parts.len() < 2 {
                    error!("Invalid volume spec in {}: {}", env_key, value);
                    index += 1;
                    continue;
                }

                let tag = parts[0];
                let guest_path = parts[1];
                if guest_mount_path_overlaps_runtime_state(guest_path) {
                    return Err(format!(
                        "guest volume path {guest_path:?} overlaps reserved runtime state /run/a3s-box"
                    )
                    .into());
                }
                // Flags after the guest path may appear in any order: "ro", "file", "copy".
                // The host decides "file" (it can stat the source); the guest obeys.
                let read_only = parts[2..].contains(&"ro");
                let is_file = parts[2..].contains(&"file");
                let copy_up = parts[2..].contains(&"copy");

                let flags = if read_only {
                    MsFlags::MS_RDONLY
                } else {
                    MsFlags::empty()
                };

                if is_file {
                    // Single-file bind mount. The shim shares a temp DIRECTORY
                    // containing the file (virtio-fs cannot share a bare file), so
                    // mount that share at a private location and bind just the file
                    // onto guest_path. This preserves the target's parent directory
                    // (e.g. /etc) instead of clobbering it with the share.
                    let file_name = guest_path.rsplit('/').next().unwrap_or(guest_path);
                    let private_mp = format!("/run/.a3s-filemounts/{}", index);
                    std::fs::create_dir_all(&private_mp)?;
                    mount_virtiofs(tag, private_mp.as_str(), MsFlags::empty())?;

                    let src = format!("{}/{}", private_mp, file_name);
                    if !std::path::Path::new(&src).exists() {
                        warn!("File mount source {} missing in share {}", src, tag);
                    }

                    // Ensure the target parent and an (empty) target file exist so
                    // the bind has somewhere to land.
                    if let Some(last_slash) = guest_path.rfind('/') {
                        let parent = &guest_path[..last_slash];
                        if !parent.is_empty() {
                            std::fs::create_dir_all(parent)?;
                        }
                    }
                    if !std::path::Path::new(guest_path).exists() {
                        std::fs::File::create(guest_path)?;
                    }

                    // Bind the file, then remount read-only if requested (a bind
                    // mount needs a separate MS_REMOUNT pass to apply MS_RDONLY).
                    mount(
                        Some(src.as_str()),
                        guest_path,
                        None::<&str>,
                        MsFlags::MS_BIND,
                        None::<&str>,
                    )?;
                    if read_only {
                        mount(
                            None::<&str>,
                            guest_path,
                            None::<&str>,
                            MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
                            None::<&str>,
                        )?;
                    }
                    info!(
                        tag = tag,
                        guest_path = guest_path,
                        read_only = read_only,
                        "Mounted file volume (bind; parent directory preserved)"
                    );
                } else {
                    // Managed volumes inherit the image directory's contents
                    // and metadata. Bind mounts remain host-authoritative.
                    std::fs::create_dir_all(guest_path)?;
                    let initialized = if copy_up {
                        mount_copy_up_volume(tag, index, guest_path, flags)?
                    } else {
                        mount_virtiofs(tag, guest_path, flags)?;
                        false
                    };
                    info!(
                        tag = tag,
                        guest_path = guest_path,
                        read_only = read_only,
                        initialized = initialized,
                        "Mounted user volume"
                    );
                }

                index += 1;
            }
            Err(_) => break,
        }
    }

    if index > 0 {
        info!("Mounted {} user volume(s)", index);
    }

    Ok(())
}

pub(super) fn guest_mount_path_overlaps_runtime_state(path: &str) -> bool {
    if !path.starts_with('/') || path.split('/').any(|component| component == "..") {
        return true;
    }
    let normalized = format!(
        "/{}",
        path.split('/')
            .filter(|component| !component.is_empty() && *component != ".")
            .collect::<Vec<_>>()
            .join("/")
    );
    normalized == "/"
        || normalized == "/run"
        || normalized == "/run/a3s-box"
        || normalized.starts_with("/run/a3s-box/")
}

/// Mount a managed volume privately, seed it from the image directory when
/// empty, then expose it at the requested destination.
#[cfg(target_os = "linux")]
pub(super) fn mount_copy_up_volume(
    tag: &str,
    index: usize,
    guest_path: &str,
    final_flags: nix::mount::MsFlags,
) -> Result<bool, Box<dyn std::error::Error>> {
    use nix::mount::{umount2, MntFlags, MsFlags};

    // Resolve image symlinks before selecting a private staging mount. The
    // staging path must not be below the seed source or the copy would
    // recursively archive the volume into itself (for example at `/run`).
    let source = std::fs::canonicalize(guest_path)?;
    let staging = volume_staging_path(index, &source)?;
    std::fs::create_dir_all(&staging)?;
    mount_virtiofs(
        tag,
        staging
            .to_str()
            .ok_or("volume staging path is not valid UTF-8")?,
        MsFlags::empty(),
    )?;

    let initialized = volume::initialize_named_volume(&source, &staging);
    let unmounted = umount2(&staging, MntFlags::MNT_DETACH);
    let _ = std::fs::remove_dir(&staging);
    if let Some(parent) = staging.parent() {
        let _ = std::fs::remove_dir(parent);
    }

    let initialized = initialized?;
    unmounted?;
    mount_virtiofs(tag, guest_path, final_flags)?;
    Ok(initialized)
}

#[cfg(target_os = "linux")]
pub(super) fn volume_staging_path(
    index: usize,
    source: &std::path::Path,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    for root in ["/run", "/dev", "/tmp"] {
        let root = std::fs::canonicalize(root)?;
        let candidate = root.join(".a3s-volume-mounts").join(index.to_string());
        if !candidate.starts_with(source) {
            return Ok(candidate);
        }
    }
    Err(format!(
        "managed volume target {} contains every safe staging directory",
        source.display()
    )
    .into())
}

/// Mount tmpfs volumes passed via BOX_TMPFS_* environment variables.
///
/// Each variable has the format: `<path>[:<options>]`
/// Data options are passed directly to mount (e.g., "size=100m"); `ro` and
/// `rw` select the mount access mode.
pub(super) fn mount_tmpfs_volumes() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "linux")]
    {
        use nix::mount::{mount, MsFlags};

        let mut index = 0;
        loop {
            let env_key = format!("BOX_TMPFS_{}", index);
            match std::env::var(&env_key) {
                Ok(value) => {
                    let (path, options, read_only) = parse_tmpfs_mount(&value)?;

                    info!(
                        path = path,
                        options = ?options,
                        read_only = read_only,
                        "Mounting tmpfs"
                    );

                    // Ensure mount point exists
                    std::fs::create_dir_all(path)?;

                    mount(
                        None::<&str>,
                        path,
                        Some("tmpfs"),
                        if read_only {
                            MsFlags::MS_RDONLY
                        } else {
                            MsFlags::empty()
                        },
                        options.as_deref(),
                    )?;

                    index += 1;
                }
                Err(_) => break,
            }
        }

        if index > 0 {
            info!("Mounted {} tmpfs volume(s)", index);
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        info!("Skipping tmpfs mount on non-Linux platform (development mode)");
    }

    Ok(())
}

pub(super) fn parse_tmpfs_mount(value: &str) -> std::io::Result<(&str, Option<String>, bool)> {
    let (path, options) = value
        .split_once(':')
        .map_or((value, None), |(path, options)| (path, Some(options)));
    if path.is_empty() || guest_mount_path_overlaps_runtime_state(path) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid or reserved tmpfs mount path: {path:?}"),
        ));
    }

    let mut data = Vec::new();
    let mut read_only = None;
    for option in options
        .into_iter()
        .flat_map(|options| options.split(','))
        .filter(|option| !option.is_empty())
    {
        match option {
            "ro" | "rw" => {
                let requested = option == "ro";
                if read_only.replace(requested).is_some() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("tmpfs mount has duplicate or conflicting access modes: {value:?}"),
                    ));
                }
            }
            _ => data.push(option),
        }
    }

    Ok((
        path,
        (!data.is_empty()).then(|| data.join(",")),
        read_only.unwrap_or(false),
    ))
}

/// Remount the container rootfs as read-only if `BOX_READONLY=1` is set.
///
/// Called after all filesystem setup (mounts, network config) so that no
/// further writes to `/` are needed before the container process launches.
/// Virtiofs and tmpfs shares are separate mountpoints and remain writable.
#[cfg(target_os = "linux")]
pub(super) fn remount_rootfs_readonly() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("BOX_READONLY").as_deref() != Ok("1") {
        return Ok(());
    }

    use nix::mount::{mount, MsFlags};

    info!("Remounting rootfs as read-only (--read-only)");

    // A direct `MS_REMOUNT|MS_RDONLY` of the virtio-fs root often fails with
    // EBUSY. Fall back to the bind-remount trick (bind / onto itself, then
    // remount that bind read-only), which succeeds where a direct remount
    // cannot. If both fail, log and continue WRITABLE — a non-enforced
    // --read-only is far less harmful than killing the container outright.
    let direct = mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
        None::<&str>,
    );
    if direct.is_ok() {
        info!("Rootfs remounted read-only");
        return Ok(());
    }

    let bind = mount(Some("/"), "/", None::<&str>, MsFlags::MS_BIND, None::<&str>).and_then(|_| {
        mount(
            None::<&str>,
            "/",
            None::<&str>,
            MsFlags::MS_REMOUNT | MsFlags::MS_BIND | MsFlags::MS_RDONLY,
            None::<&str>,
        )
    });
    match bind {
        Ok(()) => info!("Rootfs remounted read-only (via bind)"),
        Err(error) => warn!(
            %error,
            direct_error = ?direct.err(),
            "Could not remount rootfs read-only; container runs writable"
        ),
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(super) fn remount_rootfs_readonly() -> Result<(), Box<dyn std::error::Error>> {
    Ok(())
}
