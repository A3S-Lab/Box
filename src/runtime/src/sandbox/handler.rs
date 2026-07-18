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
const LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(5);
const LIFECYCLE_POLL_INTERVAL: Duration = Duration::from_millis(25);

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
    log_worker: Option<Child>,
    log_worker_pid: Option<u32>,
    log_worker_pid_start_time: Option<u64>,
    metrics_sys: Mutex<System>,
    exit_code: Option<i32>,
    bundle_dir: PathBuf,
    runtime_record: PathBuf,
    cleaned: bool,
}

#[cfg(target_os = "linux")]
pub(crate) struct CrunHandlerSpec {
    runtime_path: PathBuf,
    runtime_root: PathBuf,
    container_id: String,
    init_pid: u32,
    bundle_dir: PathBuf,
    runtime_record: PathBuf,
}

#[cfg(target_os = "linux")]
impl CrunHandlerSpec {
    pub(crate) fn new(
        runtime_path: PathBuf,
        runtime_root: PathBuf,
        container_id: String,
        init_pid: u32,
        bundle_dir: PathBuf,
        runtime_record: PathBuf,
    ) -> Self {
        Self {
            runtime_path,
            runtime_root,
            container_id,
            init_pid,
            bundle_dir,
            runtime_record,
        }
    }
}

impl CrunHandler {
    #[cfg(target_os = "linux")]
    pub(crate) fn from_child(
        spec: CrunHandlerSpec,
        process: Child,
        log_worker: Child,
        log_worker_pid_start_time: u64,
    ) -> Self {
        let log_worker_pid = log_worker.id();
        Self {
            runtime_path: spec.runtime_path,
            runtime_root: spec.runtime_root,
            container_id: spec.container_id,
            init_pid: spec.init_pid,
            process: Some(process),
            log_worker: Some(log_worker),
            log_worker_pid: Some(log_worker_pid),
            log_worker_pid_start_time: Some(log_worker_pid_start_time),
            metrics_sys: Mutex::new(System::new()),
            exit_code: None,
            bundle_dir: spec.bundle_dir,
            runtime_record: spec.runtime_record,
            cleaned: false,
        }
    }

    #[cfg(all(target_os = "linux", feature = "vm"))]
    pub(crate) fn from_recorded_runtime(
        spec: CrunHandlerSpec,
        log_worker_pid: Option<u32>,
        log_worker_pid_start_time: Option<u64>,
    ) -> Self {
        Self {
            runtime_path: spec.runtime_path,
            runtime_root: spec.runtime_root,
            container_id: spec.container_id,
            init_pid: spec.init_pid,
            process: None,
            log_worker: None,
            log_worker_pid,
            log_worker_pid_start_time,
            metrics_sys: Mutex::new(System::new()),
            exit_code: None,
            bundle_dir: spec.bundle_dir,
            runtime_record: spec.runtime_record,
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

    pub(crate) fn pause_at(
        runtime_path: &Path,
        runtime_root: &Path,
        container_id: &str,
    ) -> Result<()> {
        Self::transition_state_at(
            runtime_path,
            runtime_root,
            container_id,
            "pause",
            &["created", "running"],
            "paused",
        )
    }

    pub(crate) fn resume_at(
        runtime_path: &Path,
        runtime_root: &Path,
        container_id: &str,
    ) -> Result<()> {
        Self::transition_state_at(
            runtime_path,
            runtime_root,
            container_id,
            "resume",
            &["paused"],
            "running",
        )
    }

    fn transition_state_at(
        runtime_path: &Path,
        runtime_root: &Path,
        container_id: &str,
        operation: &str,
        source_states: &[&str],
        target_state: &str,
    ) -> Result<()> {
        let state =
            Self::query_state_at(runtime_path, runtime_root, container_id)?.ok_or_else(|| {
                BoxError::StateError(format!(
                    "Sandbox runtime {container_id} does not exist for {operation}"
                ))
            })?;
        if state.status == target_state {
            return Ok(());
        }
        if !source_states.contains(&state.status.as_str()) {
            return Err(BoxError::StateError(format!(
                "Cannot {operation} Sandbox runtime {container_id} in state {}",
                state.status
            )));
        }

        let output = Command::new(runtime_path)
            .arg("--root")
            .arg(runtime_root)
            .arg(operation)
            .arg(container_id)
            .env("LC_ALL", "C")
            .output()
            .map_err(|error| {
                BoxError::ExecError(format!("Failed to run crun {operation}: {error}"))
            })?;
        if !output.status.success() {
            if Self::query_state_at(runtime_path, runtime_root, container_id)?
                .is_some_and(|state| state.status == target_state)
            {
                return Ok(());
            }
            return Err(runtime_failure(&format!("crun {operation}"), &output));
        }

        let deadline = Instant::now() + LIFECYCLE_TIMEOUT;
        loop {
            match Self::query_state_at(runtime_path, runtime_root, container_id)? {
                Some(state) if state.status == target_state => return Ok(()),
                Some(state) if state.status == "stopped" => {
                    return Err(BoxError::StateError(format!(
                        "Sandbox runtime {container_id} stopped while waiting for {operation}"
                    )))
                }
                None => {
                    return Err(BoxError::StateError(format!(
                        "Sandbox runtime {container_id} disappeared while waiting for {operation}"
                    )))
                }
                Some(_) if Instant::now() < deadline => {
                    std::thread::sleep(LIFECYCLE_POLL_INTERVAL);
                }
                Some(state) => {
                    return Err(BoxError::StateError(format!(
                        "Timed out waiting for Sandbox runtime {container_id} to enter {target_state}; current state is {}",
                        state.status
                    )))
                }
            }
        }
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

    fn reap_log_worker(&mut self) {
        const LOG_WORKER_EXIT_TIMEOUT: Duration = Duration::from_secs(2);
        const LOG_WORKER_EXIT_POLL: Duration = Duration::from_millis(10);

        if let Some(mut worker) = self.log_worker.take() {
            let deadline = Instant::now() + LOG_WORKER_EXIT_TIMEOUT;
            loop {
                match worker.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(LOG_WORKER_EXIT_POLL);
                    }
                    Ok(None) => break,
                    Err(error) => {
                        tracing::warn!(
                            container_id = %self.container_id,
                            %error,
                            "Failed to poll Sandbox log worker before reaping"
                        );
                        break;
                    }
                }
            }
            tracing::warn!(
                container_id = %self.container_id,
                "Sandbox log worker did not exit after crun; terminating it"
            );
            let _ = worker.kill();
            let _ = worker.wait();
            return;
        }

        let (Some(pid), Some(start_time)) = (self.log_worker_pid, self.log_worker_pid_start_time)
        else {
            return;
        };
        let deadline = Instant::now() + LOG_WORKER_EXIT_TIMEOUT;
        while crate::process::is_process_running_with_identity(pid, Some(start_time))
            && Instant::now() < deadline
        {
            std::thread::sleep(LOG_WORKER_EXIT_POLL);
        }
        if crate::process::is_process_running_with_identity(pid, Some(start_time)) {
            tracing::warn!(
                container_id = %self.container_id,
                log_worker_pid = pid,
                "Recovered Sandbox log worker did not exit after crun; terminating it"
            );
            // The start-time token was revalidated immediately before the
            // signal, so a reused PID cannot be targeted.
            #[cfg(target_os = "linux")]
            if let Ok(pid) = i32::try_from(pid) {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
        #[cfg(target_os = "linux")]
        if !crate::process::wait_for_process_exit_with_identity(
            pid,
            start_time,
            LOG_WORKER_EXIT_TIMEOUT,
        ) {
            tracing::warn!(
                container_id = %self.container_id,
                log_worker_pid = pid,
                "Recovered Sandbox log worker remained present after cleanup"
            );
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
        // Reap the wrapper first: its inherited stdout/stderr descriptors must
        // close before the worker treats EOF as final. Then wait for the worker
        // to drain both streams before removing durable generation artifacts.
        self.reap_child();
        self.reap_log_worker();
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
            .is_some_and(|state| matches!(state.status.as_str(), "created" | "running" | "paused"))
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

    fn lifecycle_runtime(temporary: &tempfile::TempDir) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let runtime_root = temporary.path().join("runtime");
        std::fs::create_dir(&runtime_root).unwrap();
        std::fs::write(runtime_root.join("state"), "running\n").unwrap();
        let runtime = temporary.path().join("crun-fixture");
        std::fs::write(
            &runtime,
            r#"#!/bin/sh
root="$2"
operation="$3"
case "$operation" in
  state)
    status="$(cat "$root/state")"
    printf '{"status":"%s","pid":42}\n' "$status"
    ;;
  pause)
    printf 'paused\n' > "$root/state"
    ;;
  resume)
    printf 'running\n' > "$root/state"
    ;;
  *)
    exit 64
    ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        (runtime, runtime_root)
    }

    #[test]
    fn crun_pause_and_resume_are_state_checked_and_idempotent() {
        let temporary = tempfile::tempdir().unwrap();
        let (runtime, runtime_root) = lifecycle_runtime(&temporary);

        CrunHandler::pause_at(&runtime, &runtime_root, "sandbox-1").unwrap();
        CrunHandler::pause_at(&runtime, &runtime_root, "sandbox-1").unwrap();
        assert_eq!(
            CrunHandler::query_state_at(&runtime, &runtime_root, "sandbox-1")
                .unwrap()
                .unwrap()
                .status,
            "paused"
        );

        CrunHandler::resume_at(&runtime, &runtime_root, "sandbox-1").unwrap();
        CrunHandler::resume_at(&runtime, &runtime_root, "sandbox-1").unwrap();
        assert_eq!(
            CrunHandler::query_state_at(&runtime, &runtime_root, "sandbox-1")
                .unwrap()
                .unwrap()
                .status,
            "running"
        );
    }

    #[test]
    fn crun_pause_rejects_a_terminal_runtime() {
        let temporary = tempfile::tempdir().unwrap();
        let (runtime, runtime_root) = lifecycle_runtime(&temporary);
        std::fs::write(runtime_root.join("state"), "stopped\n").unwrap();

        let error = CrunHandler::pause_at(&runtime, &runtime_root, "sandbox-1").unwrap_err();

        assert!(error.to_string().contains("state stopped"));
    }

    #[cfg(feature = "vm")]
    #[test]
    fn recorded_runtime_handler_attaches_without_owning_a_wrapper_process() {
        let temporary = tempfile::tempdir().unwrap();
        let runtime_path = PathBuf::from("/bin/true");
        let runtime_root = temporary.path().join("runtime");
        let bundle_dir = temporary.path().join("bundle");
        let runtime_record = temporary.path().join("runtime.json");

        let handler = CrunHandler::from_recorded_runtime(
            CrunHandlerSpec::new(
                runtime_path.clone(),
                runtime_root.clone(),
                "recorded-test".to_string(),
                42,
                bundle_dir.clone(),
                runtime_record.clone(),
            ),
            None,
            None,
        );

        assert_eq!(handler.runtime_path, runtime_path);
        assert_eq!(handler.runtime_root, runtime_root);
        assert_eq!(handler.container_id, "recorded-test");
        assert_eq!(handler.pid(), 42);
        assert!(handler.process.is_none());
        assert!(handler.log_worker.is_none());
        assert!(handler.log_worker_pid.is_none());
        assert_eq!(handler.bundle_dir, bundle_dir);
        assert_eq!(handler.runtime_record, runtime_record);
        assert!(!handler.cleaned);
    }

    #[test]
    fn dropping_handler_detaches_from_live_runtime_process() {
        let temporary = tempfile::tempdir().unwrap();
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let log_worker = Command::new("sleep").arg("30").spawn().unwrap();
        let log_worker_pid = log_worker.id();
        let log_worker_pid_start_time = crate::process::pid_start_time(log_worker_pid).unwrap();
        let handler = CrunHandler::from_child(
            CrunHandlerSpec::new(
                PathBuf::from("/bin/true"),
                temporary.path().join("runtime"),
                "detached-test".to_string(),
                pid,
                temporary.path().join("bundle"),
                temporary.path().join("runtime.json"),
            ),
            child,
            log_worker,
            log_worker_pid_start_time,
        );

        drop(handler);
        let remained_alive = unsafe { libc::kill(pid as i32, 0) == 0 };
        let log_worker_remained_alive = unsafe { libc::kill(log_worker_pid as i32, 0) == 0 };

        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
            let mut status = 0;
            libc::waitpid(pid as i32, &mut status, 0);
            libc::kill(log_worker_pid as i32, libc::SIGKILL);
            libc::waitpid(log_worker_pid as i32, &mut status, 0);
        }
        assert!(
            remained_alive,
            "dropping a runtime handle must not destroy a detached Sandbox"
        );
        assert!(
            log_worker_remained_alive,
            "dropping a runtime handle must not destroy its detached log worker"
        );
    }
}
