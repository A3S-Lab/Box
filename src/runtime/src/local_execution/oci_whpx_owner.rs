//! Identity-fenced startup and reuse of the Windows WHPX OCI qualification owner.
//!
//! Qualification-only: mirrors Linux KVM Box-owned Host fencing for
//! `box-whpx-qualification-service`. Does not claim WHPX MicroVM production
//! cutover, B2 close, or default omit→OCI routing.

use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use a3s_box_core::{ExecutionIsolation, ExecutionManagerError, ExecutionManagerResult};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{OciLifecycleAdapter, OciRuntimeEndpoint};
use crate::file_lock::FileLock;

const OWNER_RECORD_SCHEMA: &str = "a3s.box.windows-whpx-oci-owner.v1";
const OWNER_RECORD_NAME: &str = "box-owner.json";
const OWNER_LOCK_TARGET: &str = "box-whpx-owner";
const READY_FILE_NAME: &str = "service-ready.json";
const READY_SCHEMA: &str = "a3s.oci.box-whpx-service-ready.v1";
const AGENT_RELATIVE: &str = r"usr\bin\a3s-oci-agent";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Certified artifacts required to spawn or reuse a Box-owned WHPX qualification Host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WindowsWhpxOwnerArtifacts {
    pub runtime_path: PathBuf,
    pub runtime_sha256: String,
    pub shim_path: PathBuf,
    pub shim_sha256: String,
    pub vm_rootfs: PathBuf,
    pub agent_sha256: String,
}

impl WindowsWhpxOwnerArtifacts {
    pub(crate) fn certify(
        runtime_path: impl Into<PathBuf>,
        shim_path: impl Into<PathBuf>,
        vm_rootfs: impl Into<PathBuf>,
    ) -> ExecutionManagerResult<Self> {
        let runtime_path = runtime_path.into();
        let shim_path = shim_path.into();
        let vm_rootfs = vm_rootfs.into();
        for (label, path) in [
            ("runtime", runtime_path.as_path()),
            ("shim", shim_path.as_path()),
            ("vm-rootfs", vm_rootfs.as_path()),
        ] {
            if !path.is_absolute() {
                return Err(ExecutionManagerError::InvalidRequest(format!(
                    "Windows WHPX OCI owner {label} path must be absolute: {}",
                    path.display()
                )));
            }
        }
        let agent_path = vm_rootfs.join(AGENT_RELATIVE);
        if !agent_path.is_file() {
            return Err(ExecutionManagerError::InvalidRequest(format!(
                "Windows WHPX OCI owner vm-rootfs must contain {}: {}",
                AGENT_RELATIVE,
                agent_path.display()
            )));
        }
        Ok(Self {
            runtime_sha256: sha256_file(&runtime_path)?,
            shim_sha256: sha256_file(&shim_path)?,
            agent_sha256: sha256_file(&agent_path)?,
            runtime_path,
            shim_path,
            vm_rootfs,
        })
    }
}

/// Deterministic named-pipe endpoint for a Box-owned WHPX service root.
pub(crate) fn owned_pipe_name(service_root: &Path) -> ExecutionManagerResult<String> {
    validate_service_root(service_root)?;
    let mut hasher = Sha256::new();
    hasher.update(service_root.to_string_lossy().as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    Ok(format!(r"\\.\pipe\a3s-box-whpx-owner-{}", &digest[..32]))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowsWhpxOwnerRecord {
    schema: String,
    pid: u32,
    pid_start_time: u64,
    runtime_path: PathBuf,
    runtime_sha256: String,
    shim_path: PathBuf,
    shim_sha256: String,
    vm_rootfs: PathBuf,
    agent_sha256: String,
    pipe_name: String,
}

impl WindowsWhpxOwnerRecord {
    fn new(
        pid: u32,
        pid_start_time: u64,
        artifacts: &WindowsWhpxOwnerArtifacts,
        pipe_name: String,
    ) -> Self {
        Self {
            schema: OWNER_RECORD_SCHEMA.to_string(),
            pid,
            pid_start_time,
            runtime_path: artifacts.runtime_path.clone(),
            runtime_sha256: artifacts.runtime_sha256.clone(),
            shim_path: artifacts.shim_path.clone(),
            shim_sha256: artifacts.shim_sha256.clone(),
            vm_rootfs: artifacts.vm_rootfs.clone(),
            agent_sha256: artifacts.agent_sha256.clone(),
            pipe_name,
        }
    }

    fn validate(&self, expected_pipe: &str) -> ExecutionManagerResult<()> {
        if self.schema != OWNER_RECORD_SCHEMA
            || self.pid == 0
            || self.pid_start_time == 0
            || self.pipe_name != expected_pipe
            || !self.runtime_path.is_absolute()
            || !self.shim_path.is_absolute()
            || !self.vm_rootfs.is_absolute()
            || !is_sha256_hex(&self.runtime_sha256)
            || !is_sha256_hex(&self.shim_sha256)
            || !is_sha256_hex(&self.agent_sha256)
        {
            return Err(ExecutionManagerError::Internal(
                "Windows WHPX OCI owner record is malformed or belongs to another endpoint"
                    .to_string(),
            ));
        }
        Ok(())
    }

    fn artifacts_match(&self, expected: &WindowsWhpxOwnerArtifacts) -> bool {
        self.runtime_path == expected.runtime_path
            && self.runtime_sha256 == expected.runtime_sha256
            && self.shim_path == expected.shim_path
            && self.shim_sha256 == expected.shim_sha256
            && self.vm_rootfs == expected.vm_rootfs
            && self.agent_sha256 == expected.agent_sha256
    }

    fn is_alive(&self) -> bool {
        crate::process::is_process_running_with_identity(self.pid, Some(self.pid_start_time))
    }
}

#[derive(Debug, Deserialize)]
struct ServiceReadyEvidence {
    schema_version: String,
    owner_pid: u32,
    endpoint: String,
    runtime_root: PathBuf,
    state_root: PathBuf,
}

/// Ensure a Box-owned Windows WHPX qualification Host is identity-fenced and ready.
pub(crate) async fn ensure_windows_whpx_oci_owner(
    service_root: &Path,
    artifacts: &WindowsWhpxOwnerArtifacts,
) -> ExecutionManagerResult<OciRuntimeEndpoint> {
    validate_service_root(service_root)?;
    let root = service_root.to_path_buf();
    tokio::task::spawn_blocking(move || prepare_service_root(&root))
        .await
        .map_err(|error| {
            ExecutionManagerError::Internal(format!(
                "Windows WHPX OCI owner root task failed: {error}"
            ))
        })??;

    let lock_target = service_root.join(OWNER_LOCK_TARGET);
    let lock = tokio::task::spawn_blocking(move || FileLock::acquire(&lock_target))
        .await
        .map_err(|error| {
            ExecutionManagerError::Internal(format!(
                "Windows WHPX OCI owner lock task failed: {error}"
            ))
        })?
        .map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to acquire Windows WHPX OCI owner lock: {error}"
            ))
        })?;

    let pipe_name = owned_pipe_name(service_root)?;
    let endpoint = OciRuntimeEndpoint::windows_named_pipe(pipe_name.clone())?;
    let record_path = service_root.join(OWNER_RECORD_NAME);
    let ready_path = service_root.join(READY_FILE_NAME);
    let state_root = service_root.join("state");

    if let Some(record) = load_owner_record(&record_path, &pipe_name)? {
        if record.is_alive() {
            if !record.artifacts_match(artifacts) {
                return Err(ExecutionManagerError::Unavailable(
                    "the live Windows WHPX OCI owner uses different runtime artifacts; stop it explicitly before changing artifacts"
                        .to_string(),
                ));
            }
            let result =
                wait_until_ready(&endpoint, &ready_path, service_root, &state_root, None).await;
            drop(lock);
            return result.map(|()| endpoint);
        }
        reclaim_dead_owner_ready(&ready_path)?;
    } else if ready_claims_live_unowned(&ready_path, &pipe_name, service_root, &state_root)? {
        return Err(ExecutionManagerError::Unavailable(format!(
            "refusing to reclaim unowned Windows WHPX OCI pipe {pipe_name} without an identity record"
        )));
    } else {
        reclaim_dead_owner_ready(&ready_path)?;
    }

    let mut child = spawn_owner(
        service_root,
        artifacts,
        &pipe_name,
        &state_root,
        &ready_path,
    )?;
    let launch_pid = child.id();
    let launch_start_time = crate::process::pid_start_time(launch_pid).ok_or_else(|| {
        let _ = child.kill();
        let _ = child.wait();
        ExecutionManagerError::Unavailable(format!(
            "could not capture Windows WHPX OCI owner launch identity for PID {launch_pid}"
        ))
    })?;
    let provisional =
        WindowsWhpxOwnerRecord::new(launch_pid, launch_start_time, artifacts, pipe_name.clone());
    if let Err(error) = write_owner_record(&record_path, &provisional) {
        let _ = child.kill();
        let _ = child.wait();
        reclaim_dead_owner_ready(&ready_path)?;
        return Err(error);
    }

    let ready = wait_until_ready(
        &endpoint,
        &ready_path,
        service_root,
        &state_root,
        Some(&mut child),
    )
    .await;
    if let Err(error) = ready {
        let detail = read_owner_log_tail(service_root);
        let _ = child.kill();
        let _ = child.wait();
        let _ = remove_record_if_same(&record_path, &provisional);
        let _ = reclaim_dead_owner_ready(&ready_path);
        return Err(match error {
            ExecutionManagerError::Unavailable(message) if !detail.is_empty() => {
                ExecutionManagerError::Unavailable(format!("{message}{detail}"))
            }
            other => other,
        });
    }

    let resolved = match resolve_owner_identity_from_ready(
        &ready_path,
        service_root,
        &state_root,
        &pipe_name,
        artifacts,
    ) {
        Ok(record) => {
            if let Err(error) = write_owner_record(&record_path, &record) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = remove_record_if_same(&record_path, &provisional);
                let _ = reclaim_dead_owner_ready(&ready_path);
                return Err(error);
            }
            record
        }
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = remove_record_if_same(&record_path, &provisional);
            let _ = reclaim_dead_owner_ready(&ready_path);
            return Err(error);
        }
    };
    if resolved.pid != launch_pid {
        let _ = child.kill();
        let _ = child.wait();
        let _ = remove_record_if_same(&record_path, &resolved);
        let _ = reclaim_dead_owner_ready(&ready_path);
        return Err(ExecutionManagerError::Unavailable(format!(
            "Windows WHPX OCI ready evidence claimed PID {} but launch PID was {launch_pid}",
            resolved.pid
        )));
    }

    std::thread::Builder::new()
        .name("a3s-whpx-oci-owner-reaper".to_string())
        .spawn(move || {
            let _ = child.wait();
        })
        .map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to start Windows WHPX OCI owner reaper: {error}"
            ))
        })?;
    drop(lock);
    Ok(endpoint)
}

async fn wait_until_ready(
    endpoint: &OciRuntimeEndpoint,
    ready_path: &Path,
    service_root: &Path,
    state_root: &Path,
    mut child: Option<&mut Child>,
) -> ExecutionManagerResult<()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Some(child) = child.as_deref_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    return Err(ExecutionManagerError::Unavailable(format!(
                        "Windows WHPX OCI owner exited during startup with {status}"
                    )));
                }
                Ok(None) => {}
                Err(error) => {
                    return Err(ExecutionManagerError::Unavailable(format!(
                        "failed to inspect Windows WHPX OCI owner startup: {error}"
                    )));
                }
            }
        }

        let ready_ok = match read_ready_evidence(ready_path) {
            Ok(Some(ready)) => {
                ready.schema_version == READY_SCHEMA
                    && ready.endpoint
                        == match endpoint {
                            OciRuntimeEndpoint::WindowsNamedPipe { name } => name.as_str(),
                            OciRuntimeEndpoint::UnixSocket { .. } => "",
                        }
                    && paths_equal(&ready.runtime_root, service_root)
                    && paths_equal(&ready.state_root, state_root)
                    && ready.owner_pid != 0
            }
            Ok(None) => false,
            Err(_) => false,
        };

        let last_error = if ready_ok {
            match OciLifecycleAdapter::connect(endpoint.clone()).await {
                Ok(adapter) => match adapter.require_isolation(ExecutionIsolation::Microvm).await {
                    Ok(_) => return Ok(()),
                    Err(error) => error.to_string(),
                },
                Err(error) => error.to_string(),
            }
        } else {
            "service-ready evidence is absent or mismatched".to_string()
        };

        if Instant::now() >= deadline {
            return Err(ExecutionManagerError::Unavailable(format!(
                "Windows WHPX OCI owner did not publish a launch-ready SDK endpoint within {} ms: {}",
                STARTUP_TIMEOUT.as_millis(),
                last_error
            )));
        }
        tokio::time::sleep(STARTUP_POLL_INTERVAL).await;
    }
}

fn resolve_owner_identity_from_ready(
    ready_path: &Path,
    service_root: &Path,
    state_root: &Path,
    pipe_name: &str,
    artifacts: &WindowsWhpxOwnerArtifacts,
) -> ExecutionManagerResult<WindowsWhpxOwnerRecord> {
    let ready = read_ready_evidence(ready_path)?.ok_or_else(|| {
        ExecutionManagerError::Unavailable(
            "Windows WHPX OCI owner ready evidence disappeared after readiness".to_string(),
        )
    })?;
    if ready.schema_version != READY_SCHEMA
        || ready.endpoint != pipe_name
        || !paths_equal(&ready.runtime_root, service_root)
        || !paths_equal(&ready.state_root, state_root)
        || ready.owner_pid == 0
    {
        return Err(ExecutionManagerError::Unavailable(
            "Windows WHPX OCI owner ready evidence does not match the Box-owned launch".to_string(),
        ));
    }
    let pid_start_time = crate::process::pid_start_time(ready.owner_pid).ok_or_else(|| {
        ExecutionManagerError::Unavailable(format!(
            "could not capture Windows WHPX OCI owner identity for ready PID {}",
            ready.owner_pid
        ))
    })?;
    Ok(WindowsWhpxOwnerRecord::new(
        ready.owner_pid,
        pid_start_time,
        artifacts,
        pipe_name.to_string(),
    ))
}

fn spawn_owner(
    service_root: &Path,
    artifacts: &WindowsWhpxOwnerArtifacts,
    pipe_name: &str,
    state_root: &Path,
    ready_path: &Path,
) -> ExecutionManagerResult<Child> {
    use std::os::windows::process::CommandExt;

    std::fs::create_dir_all(state_root).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to create Windows WHPX OCI state root {}: {error}",
            state_root.display()
        ))
    })?;
    let stdout = open_owner_log(&service_root.join("owner.stdout.log"))?;
    let stderr = open_owner_log(&service_root.join("owner.stderr.log"))?;
    // CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS — survive parent exit without a console.
    const CREATION_FLAGS: u32 = 0x00000200 | 0x00000008;
    let mut command = Command::new(&artifacts.runtime_path);
    command
        .arg("box-whpx-qualification-service")
        .arg("--shim")
        .arg(&artifacts.shim_path)
        .arg("--runtime-root")
        .arg(service_root)
        .arg("--vm-rootfs")
        .arg(&artifacts.vm_rootfs)
        .arg("--state-root")
        .arg(state_root)
        .arg("--pipe")
        .arg(pipe_name)
        .arg("--ready-file")
        .arg(ready_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .creation_flags(CREATION_FLAGS);
    command.spawn().map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to spawn Windows WHPX OCI owner {}: {error}",
            artifacts.runtime_path.display()
        ))
    })
}

fn open_owner_log(path: &Path) -> ExecutionManagerResult<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to open Windows WHPX OCI owner log {}: {error}",
                path.display()
            ))
        })
}

fn read_owner_log_tail(service_root: &Path) -> String {
    let mut parts = Vec::new();
    for name in ["owner.stderr.log", "owner.stdout.log"] {
        let path = service_root.join(name);
        match std::fs::read(&path) {
            Ok(bytes) if !bytes.is_empty() => {
                let text = String::from_utf8_lossy(&bytes);
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    let tail = trimmed
                        .chars()
                        .rev()
                        .take(1200)
                        .collect::<String>()
                        .chars()
                        .rev()
                        .collect::<String>();
                    parts.push(format!("; {name}: {tail}"));
                }
            }
            _ => {}
        }
    }
    parts.concat()
}

fn validate_service_root(path: &Path) -> ExecutionManagerResult<()> {
    if !path.is_absolute()
        || path.parent().is_none()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(ExecutionManagerError::InvalidRequest(format!(
            "Windows WHPX OCI service root must be an absolute normalized non-root path: {}",
            path.display()
        )));
    }
    Ok(())
}

fn prepare_service_root(path: &Path) -> ExecutionManagerResult<()> {
    std::fs::create_dir_all(path).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to create Windows WHPX OCI service root {}: {error}",
            path.display()
        ))
    })?;
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to inspect Windows WHPX OCI service root {}: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(ExecutionManagerError::Unavailable(format!(
            "Windows WHPX OCI service root {} must be a real directory",
            path.display()
        )));
    }
    Ok(())
}

fn load_owner_record(
    path: &Path,
    expected_pipe: &str,
) -> ExecutionManagerResult<Option<WindowsWhpxOwnerRecord>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ExecutionManagerError::Unavailable(format!(
                "failed to inspect Windows WHPX OCI owner record {}: {error}",
                path.display()
            )))
        }
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 16 * 1024 {
        return Err(ExecutionManagerError::Unavailable(format!(
            "Windows WHPX OCI owner record {} is not a protected bounded regular file",
            path.display()
        )));
    }
    let bytes = std::fs::read(path).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to read Windows WHPX OCI owner record {}: {error}",
            path.display()
        ))
    })?;
    let record: WindowsWhpxOwnerRecord = serde_json::from_slice(&bytes).map_err(|error| {
        ExecutionManagerError::Internal(format!(
            "Windows WHPX OCI owner record {} is invalid: {error}",
            path.display()
        ))
    })?;
    record.validate(expected_pipe)?;
    Ok(Some(record))
}

fn write_owner_record(path: &Path, record: &WindowsWhpxOwnerRecord) -> ExecutionManagerResult<()> {
    let bytes = serde_json::to_vec_pretty(record).map_err(|error| {
        ExecutionManagerError::Internal(format!(
            "failed to encode Windows WHPX OCI owner record: {error}"
        ))
    })?;
    let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4().simple()));
    let result = (|| -> std::io::Result<()> {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temporary);
        return Err(ExecutionManagerError::Unavailable(format!(
            "failed to write Windows WHPX OCI owner record {}: {error}",
            path.display()
        )));
    }
    Ok(())
}

fn remove_record_if_same(
    path: &Path,
    expected: &WindowsWhpxOwnerRecord,
) -> ExecutionManagerResult<()> {
    let current = load_owner_record(path, &expected.pipe_name)?;
    if current.as_ref() == Some(expected) {
        std::fs::remove_file(path).map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to remove failed Windows WHPX OCI owner record {}: {error}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

fn reclaim_dead_owner_ready(path: &Path) -> ExecutionManagerResult<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ExecutionManagerError::Unavailable(format!(
            "failed to remove stale Windows WHPX OCI ready file {}: {error}",
            path.display()
        ))),
    }
}

fn ready_claims_live_unowned(
    ready_path: &Path,
    pipe_name: &str,
    service_root: &Path,
    state_root: &Path,
) -> ExecutionManagerResult<bool> {
    let Some(ready) = read_ready_evidence(ready_path)? else {
        return Ok(false);
    };
    if ready.schema_version != READY_SCHEMA
        || ready.endpoint != pipe_name
        || !paths_equal(&ready.runtime_root, service_root)
        || !paths_equal(&ready.state_root, state_root)
        || ready.owner_pid == 0
    {
        return Ok(false);
    }
    // Without a start-time fence we can only refuse a still-running PID.
    Ok(crate::process::is_process_running_with_identity(
        ready.owner_pid,
        None,
    ))
}

fn read_ready_evidence(path: &Path) -> ExecutionManagerResult<Option<ServiceReadyEvidence>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ExecutionManagerError::Unavailable(format!(
                "failed to read Windows WHPX OCI ready file {}: {error}",
                path.display()
            )))
        }
    };
    let ready: ServiceReadyEvidence = serde_json::from_slice(&bytes).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "Windows WHPX OCI ready file {} is invalid: {error}",
            path.display()
        ))
    })?;
    Ok(Some(ready))
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn sha256_file(path: &Path) -> ExecutionManagerResult<String> {
    let mut file = std::fs::File::open(path).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to open Windows WHPX OCI artifact {}: {error}",
            path.display()
        ))
    })?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to hash Windows WHPX OCI artifact {}: {error}",
            path.display()
        ))
    })?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_artifacts() -> WindowsWhpxOwnerArtifacts {
        WindowsWhpxOwnerArtifacts {
            runtime_path: PathBuf::from(r"C:\opt\a3s\a3s-oci.exe"),
            runtime_sha256: "a".repeat(64),
            shim_path: PathBuf::from(r"C:\opt\a3s\a3s-oci-krun-shim.exe"),
            shim_sha256: "b".repeat(64),
            vm_rootfs: PathBuf::from(r"C:\opt\a3s\system"),
            agent_sha256: "c".repeat(64),
        }
    }

    #[test]
    fn owner_record_fences_endpoint_and_artifacts() {
        let artifacts = sample_artifacts();
        let pipe = r"\\.\pipe\a3s-box-whpx-owner-test";
        let record = WindowsWhpxOwnerRecord::new(42, 7, &artifacts, pipe.to_string());
        record.validate(pipe).unwrap();
        assert!(record.artifacts_match(&artifacts));
        assert!(record.validate(r"\\.\pipe\other").is_err());

        let mut drifted = artifacts.clone();
        drifted.shim_sha256 = "d".repeat(64);
        assert!(!record.artifacts_match(&drifted));
    }

    #[test]
    fn service_root_rejects_relative_and_parent_paths() {
        assert!(validate_service_root(Path::new("relative")).is_err());
        assert!(validate_service_root(Path::new(r"C:\tmp\a\..\b")).is_err());
        assert!(validate_service_root(Path::new(r"C:\")).is_err());
    }

    #[test]
    fn owned_pipe_name_is_stable_for_service_root() {
        let root = Path::new(r"C:\absolute\a3s-whpx-service");
        let first = owned_pipe_name(root).unwrap();
        let second = owned_pipe_name(root).unwrap();
        assert_eq!(first, second);
        assert!(first.starts_with(r"\\.\pipe\a3s-box-whpx-owner-"));
        assert_eq!(first.len(), r"\\.\pipe\a3s-box-whpx-owner-".len() + 32);
    }
}
