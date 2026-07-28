//! Identity-fenced shutdown of a native A3S OCI runtime owner.

use std::time::Duration;

use a3s_box_core::error::{BoxError, Result};

const FORCED_EXIT_TIMEOUT: Duration = Duration::from_secs(1);

/// Stop one exact native-service process without risking a reused PID.
pub(crate) fn stop(owner_pid: u32, owner_start_time: u64) -> Result<()> {
    stop_with_timeouts(
        owner_pid,
        owner_start_time,
        Duration::from_millis(super::A3S_OCI_LIFECYCLE_TIMEOUT_MS),
        FORCED_EXIT_TIMEOUT,
    )
}

fn stop_with_timeouts(
    owner_pid: u32,
    owner_start_time: u64,
    graceful_timeout: Duration,
    forced_timeout: Duration,
) -> Result<()> {
    signal_if_running(owner_pid, owner_start_time, libc::SIGTERM)?;
    if crate::process::wait_for_process_stop_with_identity(
        owner_pid,
        owner_start_time,
        graceful_timeout,
    ) {
        return Ok(());
    }

    tracing::warn!(
        owner_pid,
        graceful_timeout_ms = %graceful_timeout.as_millis(),
        "A3S OCI runtime owner exceeded graceful shutdown; forcing exit"
    );
    signal_if_running(owner_pid, owner_start_time, libc::SIGKILL)?;
    if crate::process::wait_for_process_stop_with_identity(
        owner_pid,
        owner_start_time,
        forced_timeout,
    ) {
        Ok(())
    } else {
        Err(BoxError::StateError(format!(
            "A3S OCI runtime owner {owner_pid} did not exit"
        )))
    }
}

fn signal_if_running(owner_pid: u32, owner_start_time: u64, signal: i32) -> Result<()> {
    if !crate::process::is_process_running_with_identity(owner_pid, Some(owner_start_time)) {
        return Ok(());
    }
    let raw_pid = i32::try_from(owner_pid).map_err(|_| {
        BoxError::StateError(format!(
            "A3S OCI runtime owner PID {owner_pid} does not fit i32"
        ))
    })?;
    // SAFETY: the stable start-time check immediately before this call fences
    // the numeric PID, and kill(2) has no memory-safety preconditions.
    if unsafe { libc::kill(raw_pid, signal) } == 0
        || !crate::process::is_process_running_with_identity(owner_pid, Some(owner_start_time))
    {
        Ok(())
    } else {
        Err(BoxError::StateError(format!(
            "Failed to send signal {signal} to A3S OCI runtime owner {owner_pid}: {}",
            std::io::Error::last_os_error()
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::io::BufRead;
    use std::process::{Command, Stdio};

    use super::*;

    #[test]
    fn stop_forces_an_identity_fenced_owner_that_ignores_sigterm() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "trap '' TERM; echo ready; exec sleep 3600"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let start_time = crate::process::pid_start_time(pid).unwrap();
        let mut ready = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");

        let result = stop_with_timeouts(
            pid,
            start_time,
            Duration::from_millis(25),
            Duration::from_secs(1),
        );
        let stopped = !crate::process::is_process_running_with_identity(pid, Some(start_time));
        let _ = child.kill();
        let _ = child.wait();

        result.unwrap();
        assert!(stopped);
    }

    #[test]
    fn stop_does_not_signal_a_reused_pid_identity() {
        let mut child = Command::new("sleep").arg("3600").spawn().unwrap();
        let pid = child.id();
        let start_time = crate::process::pid_start_time(pid).unwrap();

        let result = stop_with_timeouts(
            pid,
            start_time.saturating_add(1),
            Duration::ZERO,
            Duration::ZERO,
        );
        let still_running = crate::process::is_process_running_with_identity(pid, Some(start_time));
        let _ = child.kill();
        let _ = child.wait();

        result.unwrap();
        assert!(still_running);
    }
}
