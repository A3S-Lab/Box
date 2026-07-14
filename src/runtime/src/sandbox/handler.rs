//! Runtime handler for a live `crun` Sandbox container.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::vmm::{VmHandler, VmMetrics};
use serde::Deserialize;
use sysinfo::{Pid, System};

// `crun kill` accepts Linux signal numbers even though this module must also
// type-check on hosts where libc does not expose POSIX signal constants.
const SIGKILL_NUMBER: i32 = 9;

#[derive(Debug, Deserialize)]
pub(crate) struct CrunState {
    pub status: String,
    #[serde(default)]
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub pid: u32,
}

/// Owns both the foreground `crun run` process and the OCI runtime state.
/// Lifecycle operations always target the container ID through `crun`; merely
/// signalling the wrapper process is never treated as cleanup.
/// Dropping the in-process handle deliberately detaches without destroying the
/// workload so short-lived CLI commands can launch persistent boxes. Explicit
/// lifecycle operations and crash reconciliation own runtime cleanup.
pub struct CrunHandler {
    runtime_path: PathBuf,
    runtime_root: PathBuf,
    container_id: String,
    init_pid: u32,
    process: Option<Child>,
    metrics_sys: Mutex<System>,
    exit_code: Option<i32>,
    bundle_dir: PathBuf,
    runtime_record: PathBuf,
    cleaned: bool,
}

impl CrunHandler {
    #[cfg(target_os = "linux")]
    pub(crate) fn from_child(
        runtime_path: PathBuf,
        runtime_root: PathBuf,
        container_id: String,
        init_pid: u32,
        process: Child,
        bundle_dir: PathBuf,
        runtime_record: PathBuf,
    ) -> Self {
        Self {
            runtime_path,
            runtime_root,
            container_id,
            init_pid,
            process: Some(process),
            metrics_sys: Mutex::new(System::new()),
            exit_code: None,
            bundle_dir,
            runtime_record,
            cleaned: false,
        }
    }

    pub(crate) fn query_state_at(
        runtime_path: &Path,
        runtime_root: &Path,
        container_id: &str,
    ) -> Result<Option<CrunState>> {
        let output = Command::new(runtime_path)
            .arg("--root")
            .arg(runtime_root)
            .arg("state")
            .arg(container_id)
            .env("LC_ALL", "C")
            .output()
            .map_err(|error| BoxError::BoxBootError {
                message: format!("Failed to query Sandbox runtime state: {error}"),
                hint: None,
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let normalized = stderr.to_ascii_lowercase();
            if normalized.contains("does not exist")
                || normalized.contains("not found")
                || normalized.contains("no such file or directory")
            {
                return Ok(None);
            }
            return Err(BoxError::BoxBootError {
                message: format!("crun state failed for {container_id}: {}", stderr.trim()),
                hint: None,
            });
        }
        let state =
            serde_json::from_slice(&output.stdout).map_err(|error| BoxError::BoxBootError {
                message: format!("Invalid crun state response: {error}"),
                hint: None,
            })?;
        Ok(Some(state))
    }

    fn runtime_command(&self, operation: &str) -> Command {
        let mut command = Command::new(&self.runtime_path);
        command
            .arg("--root")
            .arg(&self.runtime_root)
            .arg(operation)
            .env("LC_ALL", "C");
        command
    }

    fn signal_container(&self, signal: i32) -> Result<()> {
        let output = self
            .runtime_command("kill")
            .arg(&self.container_id)
            .arg(signal.to_string())
            .output()
            .map_err(|error| BoxError::ExecError(format!("Failed to run crun kill: {error}")))?;
        if output.status.success() {
            return Ok(());
        }
        match self.query_state()? {
            None => return Ok(()),
            Some(state) if state.status == "stopped" => return Ok(()),
            Some(_) => {}
        }
        Err(runtime_failure("crun kill", &output))
    }

    fn query_state(&self) -> Result<Option<CrunState>> {
        Self::query_state_at(&self.runtime_path, &self.runtime_root, &self.container_id)
    }

    fn wait_for_exit(&mut self, timeout_ms: u64) -> Result<bool> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if self.poll_child()?.is_some() || self.query_state()?.is_none() {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn poll_child(&mut self) -> Result<Option<i32>> {
        if self.exit_code.is_some() {
            return Ok(self.exit_code);
        }
        let Some(process) = self.process.as_mut() else {
            return Ok(None);
        };
        match process.try_wait() {
            Ok(Some(status)) => {
                self.exit_code = status.code().or(Some(128));
                Ok(self.exit_code)
            }
            Ok(None) => Ok(None),
            Err(error) => Err(BoxError::ExecError(format!(
                "Failed to poll crun process for {}: {error}",
                self.container_id
            ))),
        }
    }

    fn reap_child(&mut self) {
        let Some(mut process) = self.process.take() else {
            return;
        };
        match process.try_wait() {
            Ok(Some(status)) => {
                self.exit_code = status.code().or(self.exit_code).or(Some(128));
                return;
            }
            Ok(None) => {
                // OCI cleanup already ran before this helper. Killing a stuck
                // wrapper here cannot replace container cleanup; it only
                // guarantees that handler teardown never blocks indefinitely.
                let _ = process.kill();
            }
            Err(error) => {
                tracing::warn!(
                    container_id = %self.container_id,
                    %error,
                    "Failed to poll crun run process before reaping"
                );
                let _ = process.kill();
            }
        }
        match process.wait() {
            Ok(status) => {
                self.exit_code = status.code().or(self.exit_code).or(Some(128));
            }
            Err(error) => {
                tracing::warn!(
                    container_id = %self.container_id,
                    %error,
                    "Failed to reap crun run process"
                );
            }
        }
    }

    fn delete_runtime_state(&mut self) -> Result<()> {
        if self.cleaned {
            return Ok(());
        }
        let output = self
            .runtime_command("delete")
            .arg("--force")
            .arg(&self.container_id)
            .output()
            .map_err(|error| BoxError::ExecError(format!("Failed to run crun delete: {error}")))?;
        if !output.status.success() && self.query_state()?.is_some() {
            return Err(runtime_failure("crun delete --force", &output));
        }
        self.cleaned = true;
        remove_file_if_exists(&self.runtime_record);
        remove_dir_if_exists(&self.bundle_dir);
        remove_dir_if_exists(&self.runtime_root);
        Ok(())
    }
}

impl VmHandler for CrunHandler {
    fn stop(&mut self, signal: i32, timeout_ms: u64) -> Result<()> {
        let mut first_error = None;
        if self.query_state()?.is_some() {
            if let Err(error) = self.signal_container(signal) {
                first_error = Some(error);
            }
            match self.wait_for_exit(timeout_ms) {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!(
                        container_id = %self.container_id,
                        timeout_ms,
                        "Sandbox did not stop gracefully; sending SIGKILL"
                    );
                    if let Err(error) = self.signal_container(SIGKILL_NUMBER) {
                        first_error.get_or_insert(error);
                    }
                    let _ = self.wait_for_exit(2_000);
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                    let _ = self.signal_container(SIGKILL_NUMBER);
                }
            }
        }

        if let Err(error) = self.delete_runtime_state() {
            first_error.get_or_insert(error);
        }
        self.reap_child();
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn metrics(&self) -> VmMetrics {
        let pid = Pid::from_u32(self.init_pid);
        let mut system = match self.metrics_sys.lock() {
            Ok(system) => system,
            Err(error) => {
                tracing::warn!(%error, "Sandbox metrics lock is poisoned");
                return VmMetrics::default();
            }
        };
        system.refresh_process(pid);
        system
            .process(pid)
            .map(|process| VmMetrics {
                cpu_percent: Some(process.cpu_usage()),
                memory_bytes: Some(process.memory()),
            })
            .unwrap_or_default()
    }

    fn is_running(&self) -> bool {
        self.query_state()
            .ok()
            .flatten()
            .is_some_and(|state| state.status == "running" || state.status == "created")
    }

    fn has_exited(&self) -> bool {
        !self.is_running()
    }

    fn pid(&self) -> u32 {
        self.init_pid
    }

    fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    fn try_wait_exit(&mut self) -> Result<Option<i32>> {
        let exit = self.poll_child()?;
        if exit.is_some() {
            self.delete_runtime_state()?;
        }
        Ok(exit)
    }
}

fn runtime_failure(operation: &str, output: &Output) -> BoxError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    BoxError::ExecError(format!(
        "{operation} exited with {}: {}",
        output.status,
        stderr.trim()
    ))
}

fn remove_file_if_exists(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "Failed to remove Sandbox runtime record")
        }
    }
}

fn remove_dir_if_exists(path: &Path) {
    match std::fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "Failed to remove Sandbox runtime directory")
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn dropping_handler_detaches_from_live_runtime_process() {
        let temporary = tempfile::tempdir().unwrap();
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let handler = CrunHandler::from_child(
            PathBuf::from("/bin/true"),
            temporary.path().join("runtime"),
            "detached-test".to_string(),
            pid,
            child,
            temporary.path().join("bundle"),
            temporary.path().join("runtime.json"),
        );

        drop(handler);
        let remained_alive = unsafe { libc::kill(pid as i32, 0) == 0 };

        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
            let mut status = 0;
            libc::waitpid(pid as i32, &mut status, 0);
        }
        assert!(
            remained_alive,
            "dropping a runtime handle must not destroy a detached Sandbox"
        );
    }
}
