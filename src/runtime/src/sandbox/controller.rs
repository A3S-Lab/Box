//! Shared launch utilities for the A3S OCI Runtime Sandbox owner.

#[cfg(target_os = "linux")]
use std::fs::File;
use std::fs::OpenOptions;
#[cfg(target_os = "linux")]
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::{Command, Stdio};
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::execution::ResolvedExecutionPlan;
use a3s_box_core::log::LogConfig;
#[cfg(target_os = "linux")]
use a3s_box_core::log::{SandboxLogWorkerSpec, SANDBOX_LOG_WORKER_SCHEMA};
use oci_spec::runtime::Spec;
use serde::Serialize;

use super::capability::SandboxCapabilitySnapshot;

#[cfg(target_os = "linux")]
pub(crate) const EXEC_LISTENER_FD: i32 = 3;
#[cfg(target_os = "linux")]
pub(crate) const PTY_LISTENER_FD: i32 = 4;
#[cfg(target_os = "linux")]
pub(crate) const INIT_LOG_FD: i32 = 5;
#[cfg(target_os = "linux")]
pub(crate) const START_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(target_os = "linux")]
const START_FAILURE_LOG_LIMIT_BYTES: u64 = 4 * 1024;

/// Files and sockets required to launch a generated OCI bundle.
pub struct SandboxLaunchSpec {
    pub container_id: String,
    pub bundle_dir: PathBuf,
    pub runtime_root: PathBuf,
    pub runtime_record: PathBuf,
    pub exec_socket_path: PathBuf,
    pub pty_socket_path: PathBuf,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    pub init_log_path: PathBuf,
    pub log_config: LogConfig,
    pub log_worker_path: PathBuf,
    pub log_worker_log_path: PathBuf,
    pub log_worker_ready_path: PathBuf,
}

/// Persist generated artifacts without accepting user-supplied OCI JSON.
pub fn write_bundle(
    bundle_dir: &Path,
    spec: &Spec,
    execution_plan: &ResolvedExecutionPlan,
    capabilities: &SandboxCapabilitySnapshot,
) -> Result<()> {
    create_private_dir(bundle_dir)?;
    write_json_atomic(&bundle_dir.join("config.json"), spec)?;
    write_json_atomic(&bundle_dir.join("execution-plan.json"), execution_plan)?;
    write_json_atomic(&bundle_dir.join("capabilities.json"), capabilities)?;
    Ok(())
}

pub(crate) fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let parent = path.parent().ok_or_else(|| {
        BoxError::ConfigError(format!(
            "Sandbox artifact has no parent: {}",
            path.display()
        ))
    })?;
    create_private_dir(parent)?;
    let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| {
        BoxError::SerializationError(format!("Failed to encode Sandbox artifact: {error}"))
    })?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&temporary).map_err(BoxError::IoError)?;
    file.write_all(&bytes).map_err(BoxError::IoError)?;
    file.write_all(b"\n").map_err(BoxError::IoError)?;
    file.sync_all().map_err(BoxError::IoError)?;
    std::fs::rename(&temporary, path).map_err(BoxError::IoError)?;
    Ok(())
}

pub(crate) fn create_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).map_err(BoxError::IoError)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(BoxError::IoError)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn open_log(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    let parent = path.parent().ok_or_else(|| {
        BoxError::ConfigError(format!("Sandbox log has no parent: {}", path.display()))
    })?;
    create_private_dir(parent)?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true).mode(0o600);
    options.open(path).map_err(BoxError::IoError)
}

#[cfg(target_os = "linux")]
pub(crate) fn start_log_worker(
    launch: &SandboxLaunchSpec,
    watched_pid: u32,
    watched_pid_start_time: u64,
) -> Result<std::process::Child> {
    let _ = std::fs::remove_file(&launch.log_worker_ready_path);
    let worker_spec = SandboxLogWorkerSpec {
        schema: SANDBOX_LOG_WORKER_SCHEMA.to_string(),
        box_id: launch.container_id.clone(),
        console_log: launch.stdout_path.clone(),
        log_config: launch.log_config.clone(),
        watched_pid,
        watched_pid_start_time,
        ready_file: launch.log_worker_ready_path.clone(),
    };
    let config = serde_json::to_string(&worker_spec).map_err(|error| {
        BoxError::SerializationError(format!(
            "Failed to encode Sandbox log worker configuration: {error}"
        ))
    })?;
    let stdout = open_log(&launch.log_worker_log_path)?;
    let stderr = stdout.try_clone().map_err(BoxError::IoError)?;
    let mut worker = Command::new(&launch.log_worker_path)
        .arg("--sandbox-log-worker-config")
        .arg(config)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .map_err(|error| BoxError::BoxBootError {
            message: format!("Failed to start Sandbox log worker: {error}"),
            hint: None,
        })?;

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if launch.log_worker_ready_path.is_file() {
            return Ok(worker);
        }
        match worker.try_wait() {
            Ok(Some(status)) => {
                let diagnostics =
                    read_log_tail(&launch.log_worker_log_path, START_FAILURE_LOG_LIMIT_BYTES)
                        .map(|excerpt| format!(": {excerpt}"))
                        .unwrap_or_default();
                return Err(BoxError::BoxBootError {
                    message: format!(
                        "Sandbox log worker exited before readiness with {status}{diagnostics}"
                    ),
                    hint: None,
                });
            }
            Ok(None) => {}
            Err(error) => return Err(BoxError::IoError(error)),
        }
        if Instant::now() >= deadline {
            reap_failed_log_worker(&mut worker);
            return Err(BoxError::BoxBootError {
                message: "Timed out waiting for Sandbox log worker readiness".to_string(),
                hint: None,
            });
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn reap_failed_log_worker(worker: &mut std::process::Child) {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match worker.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => break,
        }
    }
    let _ = worker.kill();
    let _ = worker.wait();
}

#[cfg(target_os = "linux")]
pub(crate) fn bind_control_listener(path: &Path) -> Result<std::os::unix::net::UnixListener> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    let parent = path.parent().ok_or_else(|| {
        BoxError::ConfigError(format!("Sandbox socket has no parent: {}", path.display()))
    })?;
    create_private_dir(parent)?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            std::fs::remove_file(path).map_err(BoxError::IoError)?;
        }
        Ok(_) => {
            return Err(BoxError::BoxBootError {
                message: format!(
                    "Refusing to replace non-socket Sandbox control path {}",
                    path.display()
                ),
                hint: None,
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(BoxError::IoError(error)),
    }
    let listener = std::os::unix::net::UnixListener::bind(path).map_err(BoxError::IoError)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(BoxError::IoError)?;
    Ok(listener)
}

#[cfg(target_os = "linux")]
pub(crate) fn duplicate_for_inheritance(fd: i32) -> Result<std::os::fd::OwnedFd> {
    use std::os::fd::{FromRawFd, OwnedFd};

    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) };
    if duplicate < 0 {
        return Err(BoxError::IoError(std::io::Error::last_os_error()));
    }
    // SAFETY: F_DUPFD_CLOEXEC returned a new descriptor owned by this process.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}

#[cfg(target_os = "linux")]
pub(crate) fn read_log_tail(path: &Path, limit: u64) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let offset = length.saturating_sub(limit);
    file.seek(SeekFrom::Start(offset)).ok()?;

    let mut bytes = Vec::with_capacity((length - offset) as usize);
    file.take(limit).read_to_end(&mut bytes).ok()?;
    let excerpt = String::from_utf8_lossy(&bytes).trim().to_string();
    if excerpt.is_empty() {
        None
    } else if offset > 0 {
        Some(format!("...{excerpt}"))
    } else {
        Some(excerpt)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn startup_log_excerpt_is_bounded_and_keeps_the_tail() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("runtime.stderr.log");
        let mut contents = "x".repeat(START_FAILURE_LOG_LIMIT_BYTES as usize + 512);
        contents.push_str("\nseccomp unknown architecture `NATIVE`\n");
        std::fs::write(&path, contents).unwrap();

        let excerpt = read_log_tail(&path, START_FAILURE_LOG_LIMIT_BYTES).unwrap();
        assert!(excerpt.starts_with("..."));
        assert!(excerpt.contains("seccomp unknown architecture `NATIVE`"));
        assert!(excerpt.len() <= START_FAILURE_LOG_LIMIT_BYTES as usize + 3);
    }
}
