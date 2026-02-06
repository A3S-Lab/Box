//! VmHandler - Runtime operations on a running VM.

use a3s_box_core::error::Result;
use std::process::Child;
use std::sync::Mutex;
use sysinfo::{Pid, System};

/// VM resource metrics.
#[derive(Debug, Clone, Default)]
pub struct VmMetrics {
    /// CPU usage percentage (0-100 per core)
    pub cpu_percent: Option<f32>,
    /// Memory usage in bytes
    pub memory_bytes: Option<u64>,
}

/// Trait for runtime operations on a running VM.
///
/// Separates runtime operations (stop, metrics) from spawning operations (VmController).
/// This allows reconnection to existing VMs by creating a handler directly from PID.
pub trait VmHandler: Send + Sync {
    /// Stop the VM.
    fn stop(&mut self) -> Result<()>;

    /// Get VM metrics (CPU, memory usage).
    fn metrics(&self) -> VmMetrics;

    /// Check if the VM is still running.
    fn is_running(&self) -> bool;

    /// Get the process ID of the running VM.
    fn pid(&self) -> u32;
}

/// Handler for a running VM subprocess (shim process).
///
/// Provides lifecycle operations (stop, metrics, status) for a VM identified by PID.
pub struct ShimHandler {
    pid: u32,
    box_id: String,
    /// Child process handle for proper lifecycle management.
    /// When we spawn the process, we keep the Child to properly wait() on stop.
    /// When we attach to an existing process, this is None.
    process: Option<Child>,
    /// Shared System instance for CPU metrics calculation across calls.
    /// CPU usage requires comparing snapshots over time, so we must reuse the same System.
    metrics_sys: Mutex<System>,
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
            box_id,
            process: Some(process),
            metrics_sys: Mutex::new(System::new()),
        }
    }

    /// Create a handler for an existing VM (attach mode).
    ///
    /// Used when reconnecting to a running box. We don't have a Child handle,
    /// so we manage the process by PID only.
    pub fn from_pid(pid: u32, box_id: String) -> Self {
        Self {
            pid,
            box_id,
            process: None,
            metrics_sys: Mutex::new(System::new()),
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

    fn stop(&mut self) -> Result<()> {
        // Graceful shutdown: SIGTERM first, wait, then SIGKILL if needed.
        // This gives libkrun time to flush its virtio-blk buffers to disk.
        const GRACEFUL_SHUTDOWN_TIMEOUT_MS: u64 = 2000;

        if let Some(mut process) = self.process.take() {
            // Step 1: Send SIGTERM for graceful shutdown
            let pid = process.id();
            tracing::debug!(pid, box_id = %self.box_id, "Sending SIGTERM to VM process");
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }

            // Step 2: Wait with timeout for process to exit
            let start = std::time::Instant::now();
            loop {
                match process.try_wait() {
                    Ok(Some(status)) => {
                        tracing::debug!(pid, ?status, "VM process exited gracefully");
                        return Ok(());
                    }
                    Ok(None) => {
                        // Still running, check timeout
                        if start.elapsed().as_millis() > GRACEFUL_SHUTDOWN_TIMEOUT_MS as u128 {
                            tracing::warn!(
                                pid,
                                "VM process did not exit gracefully, sending SIGKILL"
                            );
                            let _ = process.kill();
                            let _ = process.wait();
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
            // Attached mode: use SIGTERM then SIGKILL with polling
            tracing::debug!(pid = self.pid, box_id = %self.box_id, "Sending SIGTERM to attached VM process");
            unsafe {
                libc::kill(self.pid as i32, libc::SIGTERM);
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
                    let exists = unsafe { libc::kill(self.pid as i32, 0) } == 0;
                    if !exists {
                        return Ok(()); // Already dead
                    }
                }

                if start.elapsed().as_millis() > GRACEFUL_SHUTDOWN_TIMEOUT_MS as u128 {
                    tracing::warn!(
                        pid = self.pid,
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

    fn metrics(&self) -> VmMetrics {
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

    fn is_running(&self) -> bool {
        // Check if process exists by sending signal 0
        unsafe { libc::kill(self.pid as i32, 0) == 0 }
    }
}
