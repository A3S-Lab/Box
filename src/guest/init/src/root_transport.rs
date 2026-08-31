//! Guest-side selection of the MicroVM root filesystem transport.
//!
//! libkrun boots directory roots directly through virtio-fs. Block roots are
//! different: `init.krun` starts from a private, empty virtio-fs bootstrap,
//! mounts the configured block device, and switches to it before executing A3S
//! guest-init. The bootstrap share still uses the `/dev/root` virtio-fs tag, so
//! probing that tag after a block-root switch would incorrectly pivot back into
//! the empty bootstrap filesystem.

use std::ffi::OsString;

use a3s_box_core::vmm::GUEST_EXT4_ROOT_DEVICE;

/// Kernel-command-line environment imported by libkrun's `init.krun` when a
/// block device owns the guest root filesystem.
pub const LIBKRUN_BLOCK_ROOT_DEVICE_ENV: &str = "KRUN_BLOCK_ROOT_DEVICE";

#[derive(Debug, Clone, PartialEq, Eq)]
enum RootTransport {
    VirtioFs,
    Block { device: String },
}

impl RootTransport {
    fn from_block_device(value: Option<OsString>) -> Result<Self, String> {
        let Some(value) = value else {
            return Ok(Self::VirtioFs);
        };
        let device = value
            .into_string()
            .map_err(|_| format!("{LIBKRUN_BLOCK_ROOT_DEVICE_ENV} must contain valid UTF-8"))?;
        if device != GUEST_EXT4_ROOT_DEVICE {
            return Err(format!(
                "unsupported {LIBKRUN_BLOCK_ROOT_DEVICE_ENV} value {device:?}; expected {GUEST_EXT4_ROOT_DEVICE:?}"
            ));
        }
        Ok(Self::Block { device })
    }

    fn from_env() -> Result<Self, String> {
        Self::from_block_device(std::env::var_os(LIBKRUN_BLOCK_ROOT_DEVICE_ENV))
    }
}

/// Flush and remount a guest-owned block root read-only before PID 1 exits.
///
/// libkrun stops the VM when PID 1 returns. Without this explicit transition,
/// ext4 retains its journal `needs_recovery` bit even after application data was
/// synced, so the next host-side structural validation cannot distinguish a
/// clean persistent generation from a crashed one.
pub fn quiesce_for_handoff() -> Result<bool, Box<dyn std::error::Error>> {
    let transport = RootTransport::from_env()?;

    #[cfg(target_os = "linux")]
    match transport {
        RootTransport::VirtioFs => Ok(false),
        RootTransport::Block { device } => {
            sync_root_filesystem()?;
            remount_block_root_read_only()?;
            sync_root_filesystem()?;
            tracing::info!(device, "Quiesced guest-owned block root for host handoff");
            Ok(true)
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = transport;
        Ok(false)
    }
}

#[cfg(target_os = "linux")]
fn sync_root_filesystem() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::fd::AsRawFd;

    let root = std::fs::File::open("/")?;
    // SAFETY: `root` owns a valid descriptor for the duration of syncfs.
    if unsafe { libc::syncfs(root.as_raw_fd()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn remount_block_root_read_only() -> Result<(), Box<dyn std::error::Error>> {
    use nix::mount::{mount, MsFlags};

    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
        None::<&str>,
    )?;
    Ok(())
}

/// Ensure guest-init is running on the workload root filesystem.
///
/// Block roots have already been selected by `init.krun`; touching `/dev/root`
/// in that mode would re-enter libkrun's empty bootstrap share. Directory roots
/// retain the legacy virtio-fs pivot for compatibility.
pub fn prepare_current_root() -> Result<(), Box<dyn std::error::Error>> {
    let transport = RootTransport::from_env()?;

    #[cfg(target_os = "linux")]
    match transport {
        RootTransport::Block { device } => {
            tracing::info!(
                device,
                "Block root already selected by init.krun; skipping virtio-fs root pivot"
            );
        }
        RootTransport::VirtioFs => prepare_virtiofs_root()?,
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = transport;
        tracing::info!("Skipping root transport setup on non-Linux platform");
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn prepare_virtiofs_root() -> Result<(), Box<dyn std::error::Error>> {
    use nix::mount::{mount, MsFlags};

    tracing::info!("Checking for root filesystem virtio-fs device");
    std::fs::create_dir_all("/mnt/newroot")?;

    match mount(
        Some("/dev/root"),
        "/mnt/newroot",
        Some("virtiofs"),
        MsFlags::empty(),
        None::<&str>,
    ) {
        Ok(()) => switch_to_virtiofs_root("/mnt/newroot"),
        Err(error) => {
            tracing::warn!(
                %error,
                "No /dev/root virtio-fs device found; keeping the current root"
            );
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
fn switch_to_virtiofs_root(new_root: &str) -> Result<(), Box<dyn std::error::Error>> {
    use nix::mount::{mount, MsFlags};

    tracing::info!(new_root, "Mounted virtio-fs root; switching guest root");
    for directory in ["proc", "sys", "dev"] {
        std::fs::create_dir_all(format!("{new_root}/{directory}"))?;
    }

    // MS_MOVE requires the source mounts not to propagate through a shared
    // parent. A failed move is recoverable because the filesystem can be
    // mounted again after the root switch.
    let _ = mount(
        Some(""),
        "/proc",
        None::<&str>,
        MsFlags::MS_PRIVATE,
        None::<&str>,
    );
    let _ = mount(
        Some(""),
        "/sys",
        None::<&str>,
        MsFlags::MS_PRIVATE | MsFlags::MS_REC,
        None::<&str>,
    );
    let _ = mount(
        Some(""),
        "/dev",
        None::<&str>,
        MsFlags::MS_PRIVATE,
        None::<&str>,
    );

    let proc_moved = move_mount("/proc", &format!("{new_root}/proc"));
    let sys_moved = move_mount("/sys", &format!("{new_root}/sys"));
    let dev_moved = move_mount("/dev", &format!("{new_root}/dev"));

    if let Err(error) = pivot_to_rootfs(new_root) {
        tracing::warn!(
            %error,
            "Failed to pivot to virtio-fs root; falling back to chroot"
        );
        use nix::unistd::{chdir, chroot};
        chroot(new_root)?;
        chdir("/")?;
    }

    if !proc_moved {
        remount_after_root_switch("proc", "/proc", "proc")?;
    }
    if !sys_moved {
        remount_after_root_switch("sysfs", "/sys", "sysfs")?;
    }
    if !dev_moved {
        remount_after_root_switch("devtmpfs", "/dev", "devtmpfs")?;
    }

    tracing::info!("Successfully switched to virtio-fs root filesystem");
    Ok(())
}

#[cfg(target_os = "linux")]
fn move_mount(source: &str, target: &str) -> bool {
    use nix::mount::{mount, MsFlags};

    match mount(
        Some(source),
        target,
        None::<&str>,
        MsFlags::MS_MOVE,
        None::<&str>,
    ) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(source, target, %error, "Failed to move filesystem mount");
            false
        }
    }
}

#[cfg(target_os = "linux")]
fn remount_after_root_switch(
    source: &str,
    target: &str,
    filesystem_type: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use nix::mount::{mount, MsFlags};

    match mount(
        Some(source),
        target,
        Some(filesystem_type),
        MsFlags::empty(),
        None::<&str>,
    ) {
        Ok(()) => Ok(()),
        Err(error) => {
            tracing::warn!(source, target, filesystem_type, %error, "Failed to remount filesystem after root switch");
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
fn pivot_to_rootfs(new_root: &str) -> Result<(), Box<dyn std::error::Error>> {
    use nix::mount::{mount, umount2, MntFlags, MsFlags};
    use nix::unistd::chdir;
    use std::ffi::CString;

    let put_old = format!("{new_root}/.a3s-old-root");
    std::fs::create_dir_all(&put_old)?;

    mount(
        Some(""),
        "/",
        None::<&str>,
        MsFlags::MS_PRIVATE | MsFlags::MS_REC,
        None::<&str>,
    )?;

    let new_root = CString::new(new_root)?;
    let put_old_path = CString::new(put_old.as_str())?;
    // SAFETY: both C strings are valid NUL-terminated paths for the duration
    // of the syscall. nix 0.29 does not expose pivot_root.
    let result = unsafe {
        libc::syscall(
            libc::SYS_pivot_root,
            new_root.as_ptr(),
            put_old_path.as_ptr(),
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        let _ = std::fs::remove_dir(&put_old);
        return Err(error.into());
    }

    chdir("/")?;
    if let Err(error) = umount2("/.a3s-old-root", MntFlags::MNT_DETACH) {
        tracing::warn!(%error, "Failed to detach old root after pivot_root");
    }
    if let Err(error) = std::fs::remove_dir("/.a3s-old-root") {
        tracing::warn!(%error, "Failed to remove old root mount point after pivot_root");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_block_device_selects_virtiofs() {
        assert_eq!(
            RootTransport::from_block_device(None).unwrap(),
            RootTransport::VirtioFs
        );
    }

    #[test]
    fn canonical_block_device_selects_prepared_block_root() {
        assert_eq!(
            RootTransport::from_block_device(Some(OsString::from(GUEST_EXT4_ROOT_DEVICE))).unwrap(),
            RootTransport::Block {
                device: GUEST_EXT4_ROOT_DEVICE.to_string()
            }
        );
    }

    #[test]
    fn unexpected_block_device_fails_closed() {
        let error = RootTransport::from_block_device(Some(OsString::from("/dev/vdb"))).unwrap_err();
        assert!(error.contains("unsupported KRUN_BLOCK_ROOT_DEVICE"));
    }
}
