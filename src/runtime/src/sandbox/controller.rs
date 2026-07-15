//! Durable bundle creation and `crun` process startup.

#[cfg(target_os = "linux")]
use std::fs::File;
use std::fs::OpenOptions;
#[cfg(target_os = "linux")]
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(target_os = "linux")]
use std::process::Stdio;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::execution::ResolvedExecutionPlan;
use a3s_box_core::log::LogConfig;
#[cfg(target_os = "linux")]
use a3s_box_core::log::{SandboxLogWorkerSpec, SANDBOX_LOG_WORKER_SCHEMA};
use oci_spec::runtime::Spec;
use serde::Serialize;

use super::capability::{CertifiedCrun, SandboxCapabilitySnapshot};
use super::handler::CrunHandler;
#[cfg(target_os = "linux")]
use super::handler::{CrunHandlerSpec, CrunState};

#[cfg(target_os = "linux")]
const EXEC_LISTENER_FD: i32 = 3;
#[cfg(target_os = "linux")]
const PTY_LISTENER_FD: i32 = 4;
#[cfg(target_os = "linux")]
const INIT_LOG_FD: i32 = 5;
#[cfg(target_os = "linux")]
const PRESERVED_FD_COUNT: usize = 3;
#[cfg(target_os = "linux")]
const START_TIMEOUT: Duration = Duration::from_secs(10);
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

#[cfg(target_os = "linux")]
#[derive(Debug, Serialize)]
struct SandboxRuntimeRecord<'a> {
    schema: &'static str,
    container_id: &'a str,
    runtime_path: &'a Path,
    runtime_root: &'a Path,
    bundle_dir: &'a Path,
    init_pid: u32,
    log_worker_pid: u32,
    log_worker_pid_start_time: u64,
}

/// Controller pinned to one already-verified `crun` artifact.
pub struct CrunController {
    runtime: CertifiedCrun,
}

impl CrunController {
    pub fn new(runtime: CertifiedCrun) -> Self {
        Self { runtime }
    }

    /// Refuse to overwrite a live runtime generation with the same ID.
    pub fn require_absent(&self, runtime_root: &Path, container_id: &str) -> Result<()> {
        match std::fs::symlink_metadata(runtime_root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(BoxError::BoxBootError {
                    message: format!(
                        "Failed to inspect Sandbox runtime root {}: {error}",
                        runtime_root.display()
                    ),
                    hint: None,
                })
            }
            Ok(metadata) if !metadata.file_type().is_dir() => {
                return Err(BoxError::BoxBootError {
                    message: format!(
                        "Sandbox runtime root is not a directory: {}",
                        runtime_root.display()
                    ),
                    hint: None,
                })
            }
            Ok(_) => {}
        }

        // `crun state --root <missing>` materializes the root even when the
        // container is absent. The metadata gate above keeps this safety probe
        // side-effect free before image pulls and bundle preparation begin.
        match CrunHandler::query_state_at(&self.runtime.path, runtime_root, container_id)? {
            Some(state) if state.status == "stopped" => {
                let output = Command::new(&self.runtime.path)
                    .arg("--root")
                    .arg(runtime_root)
                    .arg("delete")
                    .arg("--force")
                    .arg(container_id)
                    .env("LC_ALL", "C")
                    .output()
                    .map_err(|error| BoxError::BoxBootError {
                        message: format!("Failed to delete stopped Sandbox generation: {error}"),
                        hint: None,
                    })?;
                if !output.status.success() {
                    return Err(BoxError::BoxBootError {
                        message: format!(
                            "Failed to delete stopped Sandbox generation: {}",
                            String::from_utf8_lossy(&output.stderr).trim()
                        ),
                        hint: None,
                    });
                }
                Ok(())
            }
            Some(state) => Err(BoxError::BoxBootError {
                message: format!(
                    "Sandbox runtime ID {container_id} already exists in state {}",
                    state.status
                ),
                hint: Some(
                    "Reconcile or stop the existing Sandbox before restarting it".to_string(),
                ),
            }),
            None => Ok(()),
        }
    }

    #[cfg(target_os = "linux")]
    pub async fn start(&self, launch: SandboxLaunchSpec) -> Result<CrunHandler> {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;

        self.require_absent(&launch.runtime_root, &launch.container_id)?;
        create_private_dir(&launch.runtime_root)?;
        let exec_listener = bind_control_listener(&launch.exec_socket_path)?;
        let pty_listener = bind_control_listener(&launch.pty_socket_path)?;
        let stdout = open_log(&launch.stdout_path)?;
        let stderr = open_log(&launch.stderr_path)?;
        let init_log = open_log(&launch.init_log_path)?;

        let inherited_exec = duplicate_for_inheritance(exec_listener.as_raw_fd())?;
        let inherited_pty = duplicate_for_inheritance(pty_listener.as_raw_fd())?;
        let inherited_log = duplicate_for_inheritance(init_log.as_raw_fd())?;
        let exec_fd = inherited_exec.as_raw_fd();
        let pty_fd = inherited_pty.as_raw_fd();
        let log_fd = inherited_log.as_raw_fd();

        let mut command = Command::new(&self.runtime.path);
        command
            .arg("--root")
            .arg(&launch.runtime_root)
            .arg("run")
            .arg("--bundle")
            .arg(&launch.bundle_dir)
            .arg("--preserve-fds")
            .arg(PRESERVED_FD_COUNT.to_string())
            .arg(&launch.container_id)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));

        // The duplicated source descriptors are all >= 10, so the three dup2
        // operations cannot clobber one another. dup2 clears CLOEXEC on 3/4/5.
        unsafe {
            command.pre_exec(move || {
                for (source, destination) in [
                    (exec_fd, EXEC_LISTENER_FD),
                    (pty_fd, PTY_LISTENER_FD),
                    (log_fd, INIT_LOG_FD),
                ] {
                    if libc::dup2(source, destination) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }

        let mut child = command.spawn().map_err(|error| BoxError::BoxBootError {
            message: format!("Failed to start certified crun runtime: {error}"),
            hint: None,
        })?;
        drop((inherited_exec, inherited_pty, inherited_log));
        // `crun` and the container own duplicated descriptors now. The parent
        // listener copies must close so socket EOF/lifetime is not extended.
        drop((exec_listener, pty_listener, init_log));

        let deadline = Instant::now() + START_TIMEOUT;
        let init_pid = loop {
            let child_status = match child.try_wait() {
                Ok(status) => status,
                Err(error) => {
                    cleanup_failed_start(&self.runtime.path, &launch);
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(BoxError::IoError(error));
                }
            };
            if let Some(status) = child_status {
                let diagnostics = start_failure_diagnostics(&launch);
                cleanup_failed_start(&self.runtime.path, &launch);
                return Err(BoxError::BoxBootError {
                    message: format!(
                        "crun run exited before the Sandbox was running: {status}{diagnostics}"
                    ),
                    hint: None,
                });
            }
            let runtime_state = match CrunHandler::query_state_at(
                &self.runtime.path,
                &launch.runtime_root,
                &launch.container_id,
            ) {
                Ok(state) => state,
                Err(error) => {
                    cleanup_failed_start(&self.runtime.path, &launch);
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
            };
            if let Some(CrunState { status, pid }) = runtime_state {
                if status == "running" && pid > 0 {
                    break pid;
                }
                if status == "stopped" {
                    let diagnostics = start_failure_diagnostics(&launch);
                    cleanup_failed_start(&self.runtime.path, &launch);
                    return Err(BoxError::BoxBootError {
                        message: format!("Sandbox stopped during OCI startup{diagnostics}"),
                        hint: None,
                    });
                }
            }
            if Instant::now() >= deadline {
                let diagnostics = start_failure_diagnostics(&launch);
                cleanup_failed_start(&self.runtime.path, &launch);
                let _ = child.kill();
                let _ = child.wait();
                return Err(BoxError::BoxBootError {
                    message: format!(
                        "Timed out waiting for the Sandbox OCI state to become running{diagnostics}"
                    ),
                    hint: None,
                });
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        };

        let watched_pid = child.id();
        let watched_pid_start_time = match crate::process::pid_start_time(watched_pid) {
            Some(start_time) => start_time,
            None => {
                cleanup_failed_start(&self.runtime.path, &launch);
                let _ = child.kill();
                let _ = child.wait();
                return Err(BoxError::BoxBootError {
                    message: "Failed to capture crun wrapper process identity for Sandbox logs"
                        .to_string(),
                    hint: None,
                });
            }
        };
        let mut log_worker = match start_log_worker(&launch, watched_pid, watched_pid_start_time) {
            Ok(worker) => worker,
            Err(error) => {
                cleanup_failed_start(&self.runtime.path, &launch);
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let log_worker_pid = log_worker.id();
        let log_worker_pid_start_time = match crate::process::pid_start_time(log_worker_pid) {
            Some(start_time) => start_time,
            None => {
                cleanup_failed_start(&self.runtime.path, &launch);
                reap_failed_log_worker(&mut log_worker);
                let _ = child.kill();
                let _ = child.wait();
                return Err(BoxError::BoxBootError {
                    message: "Failed to capture Sandbox log worker process identity".to_string(),
                    hint: None,
                });
            }
        };

        let record = SandboxRuntimeRecord {
            schema: "a3s.box.sandbox-runtime.v1",
            container_id: &launch.container_id,
            runtime_path: &self.runtime.path,
            runtime_root: &launch.runtime_root,
            bundle_dir: &launch.bundle_dir,
            init_pid,
            log_worker_pid,
            log_worker_pid_start_time,
        };
        if let Err(error) = write_json_atomic(&launch.runtime_record, &record) {
            cleanup_failed_start(&self.runtime.path, &launch);
            reap_failed_log_worker(&mut log_worker);
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }

        Ok(CrunHandler::from_child(
            CrunHandlerSpec::new(
                self.runtime.path.clone(),
                launch.runtime_root,
                launch.container_id,
                init_pid,
                launch.bundle_dir,
                launch.runtime_record,
            ),
            child,
            log_worker,
            log_worker_pid_start_time,
        ))
    }

    #[cfg(not(target_os = "linux"))]
    pub async fn start(&self, _launch: SandboxLaunchSpec) -> Result<CrunHandler> {
        Err(BoxError::BoxBootError {
            message: "Sandbox execution requires Linux".to_string(),
            hint: Some("Run this workload on an A3S OS Sandbox host".to_string()),
        })
    }
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

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
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

fn create_private_dir(path: &Path) -> Result<()> {
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
fn open_log(path: &Path) -> Result<File> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let parent = path.parent().ok_or_else(|| {
        BoxError::ConfigError(format!("Sandbox log has no parent: {}", path.display()))
    })?;
    create_private_dir(parent)?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path).map_err(BoxError::IoError)
}

#[cfg(target_os = "linux")]
fn start_log_worker(
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
fn reap_failed_log_worker(worker: &mut std::process::Child) {
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
fn bind_control_listener(path: &Path) -> Result<std::os::unix::net::UnixListener> {
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
            })
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
fn duplicate_for_inheritance(fd: i32) -> Result<std::os::fd::OwnedFd> {
    use std::os::fd::{FromRawFd, OwnedFd};
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) };
    if duplicate < 0 {
        return Err(BoxError::IoError(std::io::Error::last_os_error()));
    }
    // SAFETY: F_DUPFD_CLOEXEC returned a new descriptor owned by this process.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}

#[cfg(target_os = "linux")]
fn start_failure_diagnostics(launch: &SandboxLaunchSpec) -> String {
    let diagnostics = [
        ("crun stderr", &launch.stderr_path),
        ("guest-init log", &launch.init_log_path),
        ("Sandbox log worker", &launch.log_worker_log_path),
    ]
    .into_iter()
    .filter_map(|(label, path)| {
        read_log_tail(path, START_FAILURE_LOG_LIMIT_BYTES)
            .map(|excerpt| format!("{label}: {excerpt}"))
    })
    .collect::<Vec<_>>();

    if diagnostics.is_empty() {
        String::new()
    } else {
        format!(" ({})", diagnostics.join("; "))
    }
}

#[cfg(target_os = "linux")]
fn read_log_tail(path: &Path, limit: u64) -> Option<String> {
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

#[cfg(target_os = "linux")]
fn cleanup_failed_start(runtime_path: &Path, launch: &SandboxLaunchSpec) {
    let _ = Command::new(runtime_path)
        .arg("--root")
        .arg(&launch.runtime_root)
        .arg("delete")
        .arg("--force")
        .arg(&launch.container_id)
        .env("LC_ALL", "C")
        .output();
    let _ = std::fs::remove_file(&launch.runtime_record);
    let _ = std::fs::remove_dir_all(&launch.runtime_root);
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn controller_with_runtime(path: PathBuf) -> CrunController {
        CrunController::new(CertifiedCrun {
            path,
            version: "1.28".to_string(),
            sha256: "test-digest".to_string(),
            features: vec!["+CAP".to_string(), "+SECCOMP".to_string()],
        })
    }

    #[test]
    fn absent_runtime_root_is_not_materialized_by_state_probe() {
        let temporary = tempfile::tempdir().unwrap();
        let runtime_root = temporary.path().join("missing-runtime-root");
        let controller = controller_with_runtime(temporary.path().join("must-not-run"));

        controller
            .require_absent(&runtime_root, "internal-execution-id")
            .unwrap();

        assert!(!runtime_root.exists());
    }

    #[test]
    fn runtime_root_symlink_is_rejected_before_executing_crun() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("target");
        let runtime_root = temporary.path().join("runtime-root");
        std::fs::create_dir(&target).unwrap();
        symlink(&target, &runtime_root).unwrap();
        let controller = controller_with_runtime(temporary.path().join("must-not-run"));

        let error = controller
            .require_absent(&runtime_root, "internal-execution-id")
            .unwrap_err();

        assert!(error.to_string().contains("not a directory"));
        assert!(target.read_dir().unwrap().next().is_none());
    }

    #[test]
    fn startup_log_excerpt_is_bounded_and_keeps_the_tail() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("crun.stderr.log");
        let mut contents = "x".repeat(START_FAILURE_LOG_LIMIT_BYTES as usize + 512);
        contents.push_str("\nseccomp unknown architecture `NATIVE`\n");
        std::fs::write(&path, contents).unwrap();

        let excerpt = read_log_tail(&path, START_FAILURE_LOG_LIMIT_BYTES).unwrap();
        assert!(excerpt.starts_with("..."));
        assert!(excerpt.contains("seccomp unknown architecture `NATIVE`"));
        assert!(excerpt.len() <= START_FAILURE_LOG_LIMIT_BYTES as usize + 3);
    }
}
