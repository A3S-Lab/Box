//! Cross-process advisory file lock for load-modify-save persistence.
//!
//! Several JSON stores (`networks.json`, the OCI `index.json`) are mutated by a
//! read-modify-write: load the whole map, change one entry, write it back. Two
//! processes doing this concurrently lose each other's writes (and, for the
//! network store, allocate duplicate IPs). An atomic tmp+rename only prevents a
//! torn read; it does nothing for a lost update. This lock serializes the whole
//! load → mutate → save across processes.

use std::path::{Path, PathBuf};

/// RAII exclusive advisory lock keyed on `<target>.lock`.
///
/// The lock lives on a sibling `<target>.lock` file, never on `target` itself
/// (whose atomic tmp+rename would swap the inode out from under a held lock).
/// Unix uses `flock`; Windows holds the file open with sharing disabled. Both
/// locks are released automatically when the holder drops or crashes, so a
/// killed process never leaves a stale lock.
///
/// This lock is non-reentrant. Do not acquire it twice for the same file within
/// one process or task: the second acquisition blocks on the first. Hold one
/// guard across the entire load → mutate → save operation.
pub(crate) struct FileLock {
    #[cfg(any(unix, windows))]
    _file: std::fs::File,
}

impl FileLock {
    /// Acquire a blocking exclusive advisory lock on Unix.
    #[cfg(unix)]
    pub(crate) fn acquire(target: &Path) -> std::io::Result<Self> {
        use std::os::unix::io::AsRawFd;

        let lock_path = lock_path(target);
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { _file: file })
    }

    /// Try to acquire an exclusive advisory lock without waiting on Unix.
    ///
    /// `Ok(None)` means another process or file descriptor currently owns the
    /// lock. Every other I/O error remains fail-closed.
    #[cfg(unix)]
    pub(crate) fn try_acquire(target: &Path) -> std::io::Result<Option<Self>> {
        use std::os::unix::io::AsRawFd;

        let lock_path = lock_path(target);
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(Self { _file: file }));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        Err(error)
    }

    /// Acquire the Windows lock by opening the sibling file without sharing.
    ///
    /// `CreateFileW` reports a sharing violation instead of blocking, so retry
    /// until the current owner closes its handle.
    #[cfg(windows)]
    pub(crate) fn acquire(target: &Path) -> std::io::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;
        use std::time::Duration;

        const ERROR_SHARING_VIOLATION: i32 = 32;
        const ERROR_LOCK_VIOLATION: i32 = 33;

        let lock_path = lock_path(target);
        if let Some(parent) = lock_path.parent() {
            refuse_lock_parent(parent)?;
            std::fs::create_dir_all(parent)?;
        }

        loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .share_mode(0)
                .open(&lock_path)
            {
                Ok(file) => return Ok(Self { _file: file }),
                Err(error)
                    if matches!(
                        error.raw_os_error(),
                        Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
                    ) =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Try to acquire the Windows lock without retrying a sharing violation.
    #[cfg(windows)]
    pub(crate) fn try_acquire(target: &Path) -> std::io::Result<Option<Self>> {
        use std::os::windows::fs::OpenOptionsExt;

        const ERROR_SHARING_VIOLATION: i32 = 32;
        const ERROR_LOCK_VIOLATION: i32 = 33;

        let lock_path = lock_path(target);
        if let Some(parent) = lock_path.parent() {
            refuse_lock_parent(parent)?;
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .open(&lock_path)
        {
            Ok(file) => Ok(Some(Self { _file: file })),
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// Fallback for platforms without a native implementation.
    #[cfg(not(any(unix, windows)))]
    pub(crate) fn acquire(_target: &Path) -> std::io::Result<Self> {
        Ok(Self {})
    }

    #[cfg(not(any(unix, windows)))]
    pub(crate) fn try_acquire(_target: &Path) -> std::io::Result<Option<Self>> {
        Ok(Some(Self {}))
    }
}

#[cfg(windows)]
fn refuse_lock_parent(parent: &Path) -> std::io::Result<()> {
    let mut prefix = PathBuf::new();
    for component in parent.components() {
        prefix.push(component);
        crate::vm::refuse_directory_reparse(&prefix)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
    }
    Ok(())
}

fn lock_path(target: &Path) -> PathBuf {
    let mut path = target.as_os_str().to_os_string();
    path.push(".lock");
    PathBuf::from(path)
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn acquire_creates_sibling_lock_file_and_releases_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("networks.json");
        let lock_path = tmp.path().join("networks.json.lock");

        let guard = FileLock::acquire(&target).unwrap();
        assert!(lock_path.exists());
        drop(guard);

        let _guard = FileLock::acquire(&target).unwrap();
    }

    #[test]
    fn exclusive_lock_blocks_other_file_descriptors_until_released() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("index.json");
        let guard = FileLock::acquire(&target).unwrap();
        let thread_target = target.clone();
        let (tx, rx) = mpsc::channel();

        let waiter = std::thread::spawn(move || {
            let _guard = FileLock::acquire(&thread_target).unwrap();
            tx.send(()).unwrap();
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "second lock acquisition should block while the first guard is alive"
        );

        drop(guard);
        rx.recv_timeout(Duration::from_secs(2))
            .expect("second lock acquisition should proceed after drop");
        waiter.join().unwrap();
    }

    #[test]
    fn try_acquire_reports_live_owner_without_blocking() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("operation.json");
        let guard = FileLock::acquire(&target).unwrap();

        assert!(FileLock::try_acquire(&target).unwrap().is_none());
        drop(guard);
        assert!(FileLock::try_acquire(&target).unwrap().is_some());
    }

    #[cfg(windows)]
    #[test]
    fn acquire_does_not_create_a_lock_through_a_directory_junction() {
        use std::os::windows::process::CommandExt;

        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
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

        let target = link.join("networks.json");
        let acquired = FileLock::acquire(&target);
        let acquired_debug = match &acquired {
            Ok(_) => "Ok".to_string(),
            Err(error) => error.to_string(),
        };
        assert!(
            !outside.join("networks.json.lock").exists(),
            "file lock was created through the directory junction: {acquired_debug}"
        );
        assert_eq!(
            std::fs::read(outside.join("secret.txt")).unwrap(),
            b"secret"
        );
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
    }
}
