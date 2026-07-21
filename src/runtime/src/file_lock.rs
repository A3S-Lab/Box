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

    /// Fallback for platforms without a native implementation.
    #[cfg(not(any(unix, windows)))]
    pub(crate) fn acquire(_target: &Path) -> std::io::Result<Self> {
        Ok(Self {})
    }
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
}
