//! Ephemeral container `/dev` setup inside the guest mount namespace.

use nix::mount::{mount, MsFlags};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use tracing::warn;

const STANDARD_DEVICES: [&str; 6] = ["null", "zero", "full", "random", "urandom", "tty"];

/// Populate a container rootfs with guest-native standard devices.
///
/// Image rootfs directories can be transported over VirtioFS. Character device
/// numbers are host-ABI metadata and cannot be persisted safely through a macOS
/// host filesystem: a Linux `mknod(1, 3)` otherwise reappears as `0:0` in the
/// guest and opening `/dev/null` fails with `ENXIO`. Mount an ephemeral `/dev`
/// in the guest namespace and bind the guest's real devices instead. Nothing is
/// written into the image layer, and repeated execs reuse the existing mount.
pub(crate) fn ensure_container_dev_nodes(rootfs: &str) {
    let rootfs = Path::new(rootfs);
    let dev = rootfs.join("dev");
    if let Err(error) = ensure_dev_directory(&dev) {
        warn!(path = %dev.display(), %error, "Failed to prepare container /dev");
        return;
    }

    if !mount_ephemeral_dev_if_needed(rootfs, &dev) {
        return;
    }

    for directory in ["pts", "shm"] {
        let path = dev.join(directory);
        if let Err(error) = std::fs::create_dir_all(&path) {
            warn!(path = %path.display(), %error, "Failed to create container device directory");
        }
    }

    for name in STANDARD_DEVICES {
        bind_standard_device(&dev, name);
    }

    for (link, target) in [
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
        ("fd", "/proc/self/fd"),
    ] {
        let path = dev.join(link);
        if std::fs::symlink_metadata(&path).is_ok() {
            continue;
        }
        if let Err(error) = std::os::unix::fs::symlink(target, &path) {
            warn!(path = %path.display(), %error, "Failed to create container device symlink");
        }
    }
}

fn ensure_dev_directory(dev: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(dev) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "container /dev is not a directory",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir_all(dev),
        Err(error) => Err(error),
    }
}

fn mount_ephemeral_dev_if_needed(rootfs: &Path, dev: &Path) -> bool {
    let root_device = match std::fs::metadata(rootfs).map(|metadata| metadata.dev()) {
        Ok(device) => device,
        Err(error) => {
            warn!(path = %rootfs.display(), %error, "Failed to inspect container rootfs device");
            return false;
        }
    };
    let dev_device = match std::fs::metadata(dev).map(|metadata| metadata.dev()) {
        Ok(device) => device,
        Err(error) => {
            warn!(path = %dev.display(), %error, "Failed to inspect container /dev device");
            return false;
        }
    };
    if root_device != dev_device {
        return true;
    }

    match mount(
        Some("tmpfs"),
        dev,
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
        Some("mode=755,size=65536k"),
    ) {
        Ok(()) => true,
        Err(error) => {
            warn!(path = %dev.display(), %error, "Failed to mount ephemeral container /dev");
            false
        }
    }
}

fn bind_standard_device(dev: &Path, name: &str) {
    let source = PathBuf::from("/dev").join(name);
    let target = dev.join(name);
    if devices_match(&source, &target) {
        return;
    }

    let target_file = match prepare_bind_target(&target) {
        Ok(file) => file,
        Err(error) => {
            warn!(path = %target.display(), %error, "Failed to prepare container device target");
            return;
        }
    };
    let target_fd_path = format!("/proc/self/fd/{}", target_file.as_raw_fd());
    if let Err(error) = mount(
        Some(source.as_path()),
        target_fd_path.as_str(),
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    ) {
        warn!(source = %source.display(), path = %target.display(), %error, "Failed to bind container device");
        return;
    }

    if !devices_match(&source, &target) {
        warn!(source = %source.display(), path = %target.display(), "Container device bind did not expose the expected device");
    }
}

fn prepare_bind_target(target: &Path) -> std::io::Result<File> {
    match std::fs::symlink_metadata(target) {
        Ok(_) => std::fs::remove_file(target)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o666)
        .open(target)
}

fn devices_match(source: &Path, target: &Path) -> bool {
    let Ok(source) = std::fs::metadata(source) else {
        return false;
    };
    let Ok(target) = std::fs::metadata(target) else {
        return false;
    };
    source.file_type().is_char_device()
        && target.file_type().is_char_device()
        && source.rdev() == target.rdev()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_non_directory_dev_without_following_it() {
        let root = tempfile::tempdir().unwrap();
        let dev = root.path().join("dev");
        std::fs::write(&dev, "not a directory").unwrap();

        let error = ensure_dev_directory(&dev).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn rejects_a_symlinked_dev() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let dev = root.path().join("dev");
        std::os::unix::fs::symlink(outside.path(), &dev).unwrap();

        let error = ensure_dev_directory(&dev).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn creates_a_missing_dev_directory() {
        let root = tempfile::tempdir().unwrap();
        let dev = root.path().join("dev");

        ensure_dev_directory(&dev).unwrap();

        assert!(dev.is_dir());
    }
}
