//! ShimHandler — concrete VmHandler for a libkrun shim subprocess.

pub use a3s_box_core::vmm::{VmHandler, VmMetrics, DEFAULT_SHUTDOWN_TIMEOUT_MS};

use a3s_box_core::error::Result;
use std::process::Child;
use std::sync::Mutex;
use sysinfo::{Pid, System};

#[cfg(windows)]
fn wait_then_terminate_attached_process(pid: u32, box_id: &str, timeout_ms: u64) -> Result<()> {
    use a3s_box_core::error::BoxError;
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    };

    const TERMINATION_WAIT_MS: u32 = 5_000;
    const MAX_FINITE_WAIT_MS: u64 = (u32::MAX - 1) as u64;

    let handle = unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid) };
    if handle == 0 {
        let error = std::io::Error::last_os_error();
        if !crate::process::is_process_alive(pid) {
            return Ok(());
        }
        return Err(BoxError::ExecError(format!(
            "Failed to open attached VM process {pid} for box {box_id}: {error}"
        )));
    }

    let result = (|| {
        let graceful_wait_ms = timeout_ms.min(MAX_FINITE_WAIT_MS) as u32;
        match unsafe { WaitForSingleObject(handle, graceful_wait_ms) } {
            WAIT_OBJECT_0 => return Ok(()),
            WAIT_TIMEOUT => tracing::warn!(
                pid,
                box_id,
                timeout_ms,
                "Attached VM process did not exit after the guest stop request; forcing termination"
            ),
            WAIT_FAILED => tracing::warn!(
                pid,
                box_id,
                error = %std::io::Error::last_os_error(),
                "Failed while waiting for attached VM process; forcing termination"
            ),
            status => tracing::warn!(
                pid,
                box_id,
                status,
                "Unexpected attached VM wait status; forcing termination"
            ),
        }

        if unsafe { TerminateProcess(handle, 1) } == 0 {
            let error = std::io::Error::last_os_error();
            if unsafe { WaitForSingleObject(handle, 0) } == WAIT_OBJECT_0 {
                return Ok(());
            }
            return Err(BoxError::ExecError(format!(
                "Failed to terminate attached VM process {pid} for box {box_id}: {error}"
            )));
        }

        match unsafe { WaitForSingleObject(handle, TERMINATION_WAIT_MS) } {
            WAIT_OBJECT_0 => Ok(()),
            WAIT_TIMEOUT => Err(BoxError::ExecError(format!(
                "Attached VM process {pid} for box {box_id} did not exit after termination"
            ))),
            WAIT_FAILED => Err(BoxError::ExecError(format!(
                "Failed to wait for attached VM process {pid} for box {box_id}: {}",
                std::io::Error::last_os_error()
            ))),
            status => Err(BoxError::ExecError(format!(
                "Unexpected wait status {status} for attached VM process {pid} of box {box_id}"
            ))),
        }
    })();
    unsafe {
        CloseHandle(handle);
    }
    result
}

/// Handler for a running VM subprocess (shim process).
///
/// Provides lifecycle operations (stop, metrics, status) for a VM identified by PID.
pub struct ShimHandler {
    pid: u32,
    /// Stable host-process identity captured when the handler is created.
    pid_start_time: Option<u64>,
    box_id: String,
    /// Child process handle for proper lifecycle management.
    /// When we spawn the process, we keep the Child to properly wait() on stop.
    /// When we attach to an existing process, this is None.
    process: Option<Child>,
    /// Shared System instance for CPU metrics calculation across calls.
    /// CPU usage requires comparing snapshots over time, so we must reuse the same System.
    metrics_sys: Mutex<System>,
    /// Exit code of the shim process, set when stop() collects the exit status.
    exit_code: Option<i32>,
}

impl ShimHandler {
    /// Create a handler for a spawned VM with process ownership.
    ///
    /// This constructor takes ownership of the Child process handle for proper
    /// lifecycle management (clean shutdown with wait()).
    pub fn from_child(process: Child, box_id: String) -> Self {
        let pid = process.id();
        Self {
            pid,
            pid_start_time: crate::process::pid_start_time(pid),
            box_id,
            process: Some(process),
            metrics_sys: Mutex::new(System::new()),
            exit_code: None,
        }
    }

    /// Create a handler for an existing VM (attach mode).
    ///
    /// Used when reconnecting to a running box. We don't have a Child handle,
    /// so we manage the process by PID only.
    pub fn from_pid(pid: u32, box_id: String) -> Self {
        Self {
            pid,
            pid_start_time: crate::process::pid_start_time(pid),
            box_id,
            process: None,
            metrics_sys: Mutex::new(System::new()),
            exit_code: None,
        }
    }

    /// Get the box ID.
    pub fn box_id(&self) -> &str {
        &self.box_id
    }
}

impl VmHandler for ShimHandler {
    fn pid(&self) -> u32 {
        self.pid
    }

    #[cfg(unix)]
    fn stop(&mut self, signal: i32, timeout_ms: u64) -> Result<()> {
        // Graceful shutdown: send configured signal first, wait, then SIGKILL if needed.
        // This gives libkrun time to flush its virtio-blk buffers to disk.

        // `try_wait_exit` may already have reaped an owned child. Never signal
        // that old numeric PID after it becomes eligible for reuse.
        if self.exit_code.is_some() {
            self.process.take();
            return Ok(());
        }
        if !self.is_running() {
            return Ok(());
        }

        if let Some(mut process) = self.process.take() {
            // Step 1: Send configured stop signal for graceful shutdown
            let pid = process.id();
            tracing::debug!(pid, box_id = %self.box_id, signal, "Sending stop signal to VM process");
            unsafe {
                libc::kill(pid as i32, signal);
            }

            // Step 2: Wait with timeout for process to exit
            let start = std::time::Instant::now();
            loop {
                match process.try_wait() {
                    Ok(Some(status)) => {
                        tracing::debug!(pid, ?status, "VM process exited gracefully");
                        self.exit_code = status.code();
                        return Ok(());
                    }
                    Ok(None) => {
                        // Still running, check timeout
                        if start.elapsed().as_millis() > timeout_ms as u128 {
                            tracing::warn!(
                                pid,
                                timeout_ms,
                                "VM process did not exit gracefully, sending SIGKILL"
                            );
                            let _ = process.kill();
                            if let Ok(status) = process.wait() {
                                self.exit_code = status.code();
                            }
                            return Ok(());
                        }
                        // Brief sleep before checking again
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    Err(e) => {
                        tracing::warn!(pid, error = %e, "Error checking process status, forcing kill");
                        let _ = process.kill();
                        let _ = process.wait();
                        return Ok(());
                    }
                }
            }
        } else {
            // Attached mode: use configured signal then SIGKILL with polling
            tracing::debug!(pid = self.pid, box_id = %self.box_id, signal, "Sending stop signal to attached VM process");
            unsafe {
                libc::kill(self.pid as i32, signal);
            }

            // Poll for exit with timeout
            let start = std::time::Instant::now();
            loop {
                let mut status: i32 = 0;
                let result = unsafe { libc::waitpid(self.pid as i32, &mut status, libc::WNOHANG) };

                if result > 0 {
                    tracing::debug!(pid = self.pid, "VM process exited gracefully");
                    return Ok(());
                }
                if result < 0 {
                    // Error - process may not be our child (common in attached mode)
                    if !self.is_running() {
                        return Ok(()); // Already dead
                    }
                }

                if start.elapsed().as_millis() > timeout_ms as u128 {
                    if !self.is_running() {
                        return Ok(());
                    }
                    tracing::warn!(
                        pid = self.pid,
                        timeout_ms,
                        "VM process did not exit gracefully, sending SIGKILL"
                    );
                    unsafe {
                        libc::kill(self.pid as i32, libc::SIGKILL);
                    }
                    return Ok(());
                }

                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }

    #[cfg(windows)]
    fn stop(&mut self, _signal: i32, timeout_ms: u64) -> Result<()> {
        // Windows version: use Child::kill() or terminate process by PID
        if let Some(mut process) = self.process.take() {
            tracing::debug!(pid = self.pid, box_id = %self.box_id, "Terminating VM process");

            // Try graceful wait first
            let start = std::time::Instant::now();
            loop {
                match process.try_wait() {
                    Ok(Some(status)) => {
                        tracing::debug!(pid = self.pid, ?status, "VM process exited");
                        self.exit_code = status.code();
                        return Ok(());
                    }
                    Ok(None) => {
                        if start.elapsed().as_millis() > timeout_ms as u128 {
                            tracing::warn!(pid = self.pid, "VM process did not exit, forcing kill");
                            let _ = process.kill();
                            if let Ok(status) = process.wait() {
                                self.exit_code = status.code();
                            }
                            return Ok(());
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    Err(e) => {
                        tracing::warn!(pid = self.pid, error = %e, "Error checking process, forcing kill");
                        let _ = process.kill();
                        let _ = process.wait();
                        return Ok(());
                    }
                }
            }
        } else {
            if !self.is_running() {
                return Ok(());
            }

            // Every CLI invocation reconstructs the managed runtime from its
            // durable PID, so normal `stop` reaches this attached path. The VM
            // manager has already sent the guest stop request. Hold a handle to
            // this exact process object while waiting, then force-terminate it
            // on timeout so PID reuse cannot target an unrelated process.
            tracing::debug!(pid = self.pid, box_id = %self.box_id, timeout_ms, "Waiting for attached VM process to stop");
            wait_then_terminate_attached_process(self.pid, &self.box_id, timeout_ms)
        }
    }

    fn metrics(&self) -> VmMetrics {
        if !self.is_running() {
            return VmMetrics::default();
        }

        let pid = Pid::from_u32(self.pid);

        // Use the shared System instance for stateful CPU tracking
        let mut sys = match self.metrics_sys.lock() {
            Ok(guard) => guard,
            Err(e) => {
                tracing::warn!(error = %e, "metrics_sys lock poisoned");
                return VmMetrics::default();
            }
        };

        // Refresh process info - this updates the internal state for delta calculation
        sys.refresh_process(pid);

        // Try to get process information
        if let Some(proc_info) = sys.process(pid) {
            return VmMetrics {
                cpu_percent: Some(proc_info.cpu_usage()),
                memory_bytes: Some(proc_info.memory()),
            };
        }

        // Process not found or not running - return empty metrics
        VmMetrics::default()
    }

    #[cfg(unix)]
    fn is_running(&self) -> bool {
        crate::process::is_process_alive_with_identity(self.pid, self.pid_start_time)
    }

    #[cfg(windows)]
    fn is_running(&self) -> bool {
        crate::process::is_process_alive_with_identity(self.pid, self.pid_start_time)
    }

    fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    fn try_wait_exit(&mut self) -> Result<Option<i32>> {
        if self.exit_code.is_some() {
            return Ok(self.exit_code);
        }

        let Some(process) = self.process.as_mut() else {
            return Ok(None);
        };

        match process.try_wait() {
            Ok(Some(status)) => {
                self.exit_code = status.code();
                Ok(self.exit_code)
            }
            Ok(None) => Ok(None),
            Err(e) => Err(a3s_box_core::error::BoxError::ExecError(format!(
                "Failed to poll VM process {}: {}",
                self.pid, e
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vm_metrics_default() {
        let m = VmMetrics::default();
        assert!(m.cpu_percent.is_none());
        assert!(m.memory_bytes.is_none());
    }

    #[test]
    fn test_vm_metrics_clone() {
        let m = VmMetrics {
            cpu_percent: Some(50.0),
            memory_bytes: Some(1024 * 1024),
        };
        let cloned = m.clone();
        assert_eq!(cloned.cpu_percent, Some(50.0));
        assert_eq!(cloned.memory_bytes, Some(1024 * 1024));
    }

    #[test]
    fn test_shim_handler_from_pid() {
        let handler = ShimHandler::from_pid(12345, "box-abc".to_string());
        assert_eq!(handler.pid(), 12345);
        assert_eq!(handler.box_id(), "box-abc");
        assert_eq!(handler.exit_code(), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn attached_handler_rejects_a_reused_pid_identity() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let mut handler = ShimHandler::from_pid(child.id(), "box-stale-pid".to_string());
        handler.pid_start_time = Some(u64::MAX);

        assert!(!handler.is_running());
        let metrics = handler.metrics();
        assert!(metrics.cpu_percent.is_none());
        assert!(metrics.memory_bytes.is_none());
        handler.stop(libc::SIGTERM, 0).unwrap();
        assert!(child.try_wait().unwrap().is_none());

        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn test_shim_handler_try_wait_exit_captures_child_exit_code() {
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 7")
            .spawn()
            .unwrap();
        let mut handler = ShimHandler::from_child(child, "box-child-exit".to_string());

        let exit_code = loop {
            if let Some(code) = handler.try_wait_exit().unwrap() {
                break code;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };

        assert_eq!(exit_code, 7);
        assert_eq!(handler.exit_code(), Some(7));
        assert_eq!(handler.try_wait_exit().unwrap(), Some(7));
    }

    #[cfg(unix)]
    #[test]
    fn test_shim_handler_stop_terminates_owned_child() {
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg("trap 'exit 0' TERM; while :; do sleep 0.05; done")
            .spawn()
            .unwrap();
        let mut handler = ShimHandler::from_child(child, "box-child-stop".to_string());

        assert!(handler.is_running());
        handler.stop(libc::SIGTERM, 2_000).unwrap();

        assert!(!handler.is_running());
        let exit_code = handler.exit_code();
        assert!(matches!(exit_code, None | Some(0)));
        assert_eq!(handler.try_wait_exit().unwrap(), exit_code);
    }

    #[cfg(unix)]
    #[test]
    fn test_shim_handler_stop_attached_missing_pid_is_ok() {
        let mut handler = ShimHandler::from_pid(999_999_999, "missing".to_string());

        handler.stop(libc::SIGTERM, 10).unwrap();

        assert!(!handler.is_running());
        assert_eq!(handler.exit_code(), None);
    }

    #[cfg(windows)]
    #[test]
    fn test_shim_handler_stop_terminates_attached_child() {
        let mut child = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"])
            .spawn()
            .unwrap();
        let mut handler = ShimHandler::from_pid(child.id(), "box-attached-stop".to_string());

        assert!(handler.is_running());
        let stop_result = handler.stop(15, 0);
        let exited = child.try_wait().unwrap().is_some();
        if !exited {
            let _ = child.kill();
            let _ = child.wait();
        }

        stop_result.unwrap();
        assert!(exited, "attached Windows process survived handler.stop()");
        assert!(!handler.is_running());
    }

    #[cfg(windows)]
    #[test]
    fn test_shim_handler_stop_allows_attached_child_to_exit_gracefully() {
        let mut child = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-Command",
                "Start-Sleep -Milliseconds 150; exit 0",
            ])
            .spawn()
            .unwrap();
        let mut handler = ShimHandler::from_pid(child.id(), "box-attached-graceful".to_string());

        handler.stop(15, 2_000).unwrap();
        let status = child.wait().unwrap();

        assert!(status.success(), "graceful child was force-terminated");
        assert!(!handler.is_running());
    }

    #[test]
    fn test_shim_handler_is_running_nonexistent_pid() {
        // PID 999999999 should not exist
        let handler = ShimHandler::from_pid(999_999_999, "test".to_string());
        assert!(!handler.is_running());
    }

    #[test]
    fn test_shim_handler_metrics_nonexistent_pid() {
        let handler = ShimHandler::from_pid(999_999_999, "test".to_string());
        let m = handler.metrics();
        // Non-existent process should return default metrics
        assert!(m.cpu_percent.is_none() || m.cpu_percent == Some(0.0));
    }

    #[test]
    fn test_shim_handler_is_running_current_process() {
        // Current process PID should be running
        let pid = std::process::id();
        let handler = ShimHandler::from_pid(pid, "self".to_string());
        assert!(handler.is_running());
    }

    #[test]
    fn test_default_shutdown_timeout() {
        assert_eq!(DEFAULT_SHUTDOWN_TIMEOUT_MS, 10_000);
    }

    #[test]
    fn test_vm_metrics_debug() {
        let m = VmMetrics {
            cpu_percent: Some(25.5),
            memory_bytes: Some(512),
        };
        let debug = format!("{:?}", m);
        assert!(debug.contains("25.5"));
        assert!(debug.contains("512"));
    }
}
