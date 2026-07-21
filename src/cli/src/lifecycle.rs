//! Shared lifecycle validation helpers for commands backed by a host process.

use std::path::Path;

use crate::process;
use crate::state::BoxRecord;

/// Cross-process per-box lifecycle lock.
///
/// Start/restart/monitor boot, stop/kill/pause/remove/update, compose teardown,
/// and stopped-box commit hold the same lock. This serializes PID selection,
/// signals, cleanup, boot, and the corresponding state transition for one box.
pub struct BoxLifecycleLock {
    #[cfg(any(unix, windows))]
    _file: std::fs::File,
}

impl BoxLifecycleLock {
    fn acquire(box_id: &str) -> std::io::Result<Self> {
        if box_id.is_empty()
            || !box_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "box ID is not safe for a lifecycle lock filename",
            ));
        }
        Self::acquire_in(&a3s_box_core::dirs_home().join("locks"), box_id)
    }

    #[cfg(unix)]
    fn acquire_in(directory: &Path, box_id: &str) -> std::io::Result<Self> {
        use std::os::unix::io::AsRawFd;

        std::fs::create_dir_all(directory)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join(format!("{box_id}.lifecycle.lock")))?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { _file: file })
    }

    #[cfg(windows)]
    fn acquire_in(directory: &Path, box_id: &str) -> std::io::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;
        use std::time::Duration;

        const ERROR_SHARING_VIOLATION: i32 = 32;
        const ERROR_LOCK_VIOLATION: i32 = 33;

        std::fs::create_dir_all(directory)?;
        let path = directory.join(format!("{box_id}.lifecycle.lock"));
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
    fn acquire_in(_directory: &Path, _box_id: &str) -> std::io::Result<Self> {
        Ok(Self {})
    }
}

/// Acquire a lifecycle lock without blocking the async executor thread.
pub async fn acquire_box_lifecycle_lock(box_id: &str) -> std::io::Result<BoxLifecycleLock> {
    let box_id = box_id.to_string();
    tokio::task::spawn_blocking(move || BoxLifecycleLock::acquire(&box_id))
        .await
        .map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("lifecycle lock task failed: {error}"),
            )
        })?
}

/// Invalidate the previous terminal completion marker before launching a box.
///
/// The old manifest is atomically moved to a one-shot replay path. If it was
/// already staged, the replay file is retained so a boot that failed before
/// guest replay can retry. Commit reads only the canonical marker, so a forced
/// stop or metadata-write failure still fails closed.
pub fn stage_box_terminal_rootfs_metadata(
    box_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    a3s_box_runtime::rootfs::stage_box_terminal_rootfs_metadata(box_dir)?;
    Ok(())
}

/// Require a record to point at a currently live host process.
pub fn require_live_pid(record: &BoxRecord, action: &str) -> Result<u32, String> {
    match record.pid {
        Some(pid) if process::is_process_alive_with_identity(pid, record.pid_start_time) => Ok(pid),
        Some(pid) => Err(format!(
            "Cannot {action} box {} because its recorded PID {pid} is not running. The box state may be stale; run `a3s-box ps` to reconcile state, then `a3s-box restart {}` if it should still be running.",
            record.name, record.name
        )),
        None => Err(format!(
            "Cannot {action} box {} because it has no recorded PID. The box state may be stale; run `a3s-box ps` to reconcile state, then `a3s-box restart {}` if it should still be running.",
            record.name, record.name
        )),
    }
}

/// Return whether a freshly loaded record still describes the execution whose
/// PID was selected before an asynchronous signal/wait operation.
pub fn matches_execution(record: &BoxRecord, pid: u32, pid_start_time: Option<u64>) -> bool {
    record.pid == Some(pid) && record.pid_start_time == pid_start_time
}

/// Resume a paused process before sending a terminating lifecycle signal.
pub fn resume_paused_for_termination(
    record: &BoxRecord,
    pid: u32,
    action: &str,
) -> Result<(), String> {
    if record.status != "paused" {
        return Ok(());
    }

    #[cfg(unix)]
    {
        process::send_signal(pid, libc::SIGCONT).map_err(|err| {
            format!(
                "Failed to resume paused box {} before {action}: {err}",
                record.name
            )
        })
    }
    #[cfg(windows)]
    {
        let _ = pid;
        Err(crate::platform::unsupported_command(
            action,
            "resuming a paused host process before termination",
        )
        .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::make_record;

    #[test]
    fn test_require_live_pid_accepts_current_process() {
        let record = make_record("id", "box", "running", Some(std::process::id()));

        assert_eq!(
            require_live_pid(&record, "pause").unwrap(),
            std::process::id()
        );
    }

    #[test]
    fn test_require_live_pid_rejects_missing_pid_with_guidance() {
        let record = make_record("id", "box", "running", None);

        let error = require_live_pid(&record, "pause").unwrap_err();

        assert!(error.contains("no recorded PID"));
        assert!(error.contains("a3s-box ps"));
        assert!(error.contains("a3s-box restart box"));
    }

    #[test]
    fn test_resume_paused_for_termination_noops_for_running() {
        let record = make_record("id", "box", "running", Some(std::process::id()));

        assert!(resume_paused_for_termination(&record, std::process::id(), "stop").is_ok());
    }

    #[test]
    fn execution_identity_rejects_a_replacement_pid() {
        let mut record = make_record("id", "box", "running", Some(101));
        record.pid_start_time = Some(1);
        assert!(matches_execution(&record, 101, Some(1)));

        record.pid = Some(202);
        record.pid_start_time = Some(2);
        assert!(!matches_execution(&record, 101, Some(1)));
    }

    // A live PID whose recorded start-time identity does NOT match (a reused PID
    // after a crash/reboot) must be rejected so stop/kill/pause never signals an
    // unrelated host process. Without the identity check this returns Ok(pid).
    #[cfg(target_os = "linux")]
    #[test]
    fn test_require_live_pid_rejects_reused_pid_via_identity_mismatch() {
        let mut record = make_record("id", "box", "running", Some(std::process::id()));
        record.pid_start_time = Some(u64::MAX);

        let error = require_live_pid(&record, "stop").unwrap_err();
        assert!(error.contains("is not running"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_require_live_pid_accepts_matching_identity() {
        let mut record = make_record("id", "box", "running", Some(std::process::id()));
        record.pid_start_time = crate::process::pid_start_time(std::process::id());

        assert_eq!(
            require_live_pid(&record, "stop").unwrap(),
            std::process::id()
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn lifecycle_lock_serializes_same_box() {
        use std::sync::mpsc;
        use std::time::Duration;

        let directory = tempfile::tempdir().unwrap();
        let first = BoxLifecycleLock::acquire_in(directory.path(), "same-box").unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let lock_dir = directory.path().to_path_buf();
        let waiter = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let _second = BoxLifecycleLock::acquire_in(&lock_dir, "same-box").unwrap();
            acquired_tx.send(()).unwrap();
        });

        started_rx.recv().unwrap();
        assert!(acquired_rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(first);
        acquired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        waiter.join().unwrap();
    }
}
