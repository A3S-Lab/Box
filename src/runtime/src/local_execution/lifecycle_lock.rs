use std::path::Path;

use a3s_box_core::{ExecutionManagerError, ExecutionManagerResult};

/// Cross-process lock shared with the CLI's stopped-commit/start lock.
pub struct ExecutionLifecycleLock {
    #[cfg(any(unix, windows))]
    _file: std::fs::File,
}

/// Acquire the cross-process lifecycle lock from synchronous callers such as
/// the direct SDK. The returned guard holds the lock until it is dropped.
pub fn acquire_blocking(
    home_dir: &Path,
    execution_id: &str,
) -> ExecutionManagerResult<ExecutionLifecycleLock> {
    validate_execution_id(execution_id)?;
    ExecutionLifecycleLock::acquire(&home_dir.join("locks"), execution_id).map_err(|error| {
        ExecutionManagerError::Internal(format!("failed to acquire lifecycle lock: {error}"))
    })
}

fn validate_execution_id(execution_id: &str) -> ExecutionManagerResult<()> {
    if execution_id.is_empty()
        || !execution_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ExecutionManagerError::Internal(
            "execution ID is not safe for a lifecycle lock filename".to_string(),
        ));
    }
    Ok(())
}

impl ExecutionLifecycleLock {
    #[cfg(unix)]
    fn acquire(directory: &Path, execution_id: &str) -> std::io::Result<Self> {
        use std::os::unix::io::AsRawFd;

        std::fs::create_dir_all(directory)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join(format!("{execution_id}.lifecycle.lock")))?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { _file: file })
    }

    #[cfg(windows)]
    fn acquire(directory: &Path, execution_id: &str) -> std::io::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;
        use std::time::Duration;

        const ERROR_SHARING_VIOLATION: i32 = 32;
        const ERROR_LOCK_VIOLATION: i32 = 33;

        std::fs::create_dir_all(directory)?;
        let path = directory.join(format!("{execution_id}.lifecycle.lock"));
        loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .share_mode(0)
                .open(&path)
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

    #[cfg(not(any(unix, windows)))]
    fn acquire(_directory: &Path, _execution_id: &str) -> std::io::Result<Self> {
        Ok(Self {})
    }
}

pub(super) async fn acquire(
    home_dir: &Path,
    execution_id: &str,
) -> ExecutionManagerResult<ExecutionLifecycleLock> {
    validate_execution_id(execution_id)?;
    let directory = home_dir.join("locks");
    let execution_id = execution_id.to_string();
    tokio::task::spawn_blocking(move || ExecutionLifecycleLock::acquire(&directory, &execution_id))
        .await
        .map_err(|error| {
            ExecutionManagerError::Internal(format!("lifecycle lock task failed: {error}"))
        })?
        .map_err(|error| {
            ExecutionManagerError::Internal(format!("failed to acquire lifecycle lock: {error}"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(unix, windows))]
    #[test]
    fn same_execution_is_serialized_across_file_handles() {
        use std::sync::mpsc;
        use std::time::Duration;

        let directory = tempfile::tempdir().unwrap();
        let first = ExecutionLifecycleLock::acquire(directory.path(), "execution-1").unwrap();
        let lock_dir = directory.path().to_path_buf();
        let (started_tx, started_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let _second = ExecutionLifecycleLock::acquire(&lock_dir, "execution-1").unwrap();
            acquired_tx.send(()).unwrap();
        });

        started_rx.recv().unwrap();
        assert!(acquired_rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(first);
        acquired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        waiter.join().unwrap();
    }
}
