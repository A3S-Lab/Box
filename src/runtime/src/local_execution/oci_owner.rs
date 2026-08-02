//! Identity-fenced startup and reuse of the long-lived native Linux OCI owner.

use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use a3s_box_core::{ExecutionIsolation, ExecutionManagerError, ExecutionManagerResult};
use serde::{Deserialize, Serialize};

use super::{OciLifecycleAdapter, OciRuntimeEndpoint};
use crate::file_lock::FileLock;
use crate::sandbox::CertifiedA3sOci;

const OWNER_RECORD_SCHEMA: &str = "a3s.box.native-linux-oci-owner.v1";
const OWNER_RECORD_NAME: &str = "box-owner.json";
const OWNER_LOCK_TARGET: &str = "box-owner";
const OWNER_SOCKET_NAME: &str = "runtime.sock";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeLinuxOwnerRecord {
    schema: String,
    pid: u32,
    pid_start_time: u64,
    runtime_path: PathBuf,
    runtime_sha256: String,
    agent_path: PathBuf,
    agent_sha256: String,
    socket_path: PathBuf,
}

impl NativeLinuxOwnerRecord {
    fn new(
        pid: u32,
        pid_start_time: u64,
        artifacts: &CertifiedA3sOci,
        socket_path: PathBuf,
    ) -> Self {
        Self {
            schema: OWNER_RECORD_SCHEMA.to_string(),
            pid,
            pid_start_time,
            runtime_path: artifacts.runtime_path.clone(),
            runtime_sha256: artifacts.runtime_sha256.clone(),
            agent_path: artifacts.agent_path.clone(),
            agent_sha256: artifacts.agent_sha256.clone(),
            socket_path,
        }
    }

    fn validate(&self, expected_socket: &Path) -> ExecutionManagerResult<()> {
        if self.schema != OWNER_RECORD_SCHEMA
            || self.pid == 0
            || self.pid_start_time == 0
            || self.socket_path != expected_socket
            || !self.runtime_path.is_absolute()
            || !self.agent_path.is_absolute()
            || !is_sha256_hex(&self.runtime_sha256)
            || !is_sha256_hex(&self.agent_sha256)
        {
            return Err(ExecutionManagerError::Internal(
                "native Linux OCI owner record is malformed or belongs to another endpoint"
                    .to_string(),
            ));
        }
        Ok(())
    }

    fn artifacts_match(&self, expected: &CertifiedA3sOci) -> bool {
        self.runtime_path == expected.runtime_path
            && self.runtime_sha256 == expected.runtime_sha256
            && self.agent_path == expected.agent_path
            && self.agent_sha256 == expected.agent_sha256
    }

    fn is_alive(&self) -> bool {
        crate::process::is_process_alive_with_identity(self.pid, Some(self.pid_start_time))
    }
}

pub(crate) async fn ensure_native_linux_oci_owner(
    service_root: &Path,
    artifacts: &CertifiedA3sOci,
) -> ExecutionManagerResult<OciRuntimeEndpoint> {
    validate_service_root(service_root)?;
    let root = service_root.to_path_buf();
    tokio::task::spawn_blocking(move || prepare_service_root(&root))
        .await
        .map_err(|error| {
            ExecutionManagerError::Internal(format!(
                "native Linux OCI owner root task failed: {error}"
            ))
        })??;

    let lock_target = service_root.join(OWNER_LOCK_TARGET);
    let lock = tokio::task::spawn_blocking(move || FileLock::acquire(&lock_target))
        .await
        .map_err(|error| {
            ExecutionManagerError::Internal(format!(
                "native Linux OCI owner lock task failed: {error}"
            ))
        })?
        .map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to acquire native Linux OCI owner lock: {error}"
            ))
        })?;

    let socket_path = service_root.join(OWNER_SOCKET_NAME);
    let endpoint = OciRuntimeEndpoint::unix_socket(socket_path.clone())?;
    let record_path = service_root.join(OWNER_RECORD_NAME);
    if let Some(record) = load_owner_record(&record_path, &socket_path)? {
        if record.is_alive() {
            if !record.artifacts_match(artifacts) {
                return Err(ExecutionManagerError::Unavailable(
                    "the live native Linux OCI owner uses different runtime artifacts; stop it explicitly before changing artifacts"
                        .to_string(),
                ));
            }
            let result = wait_until_ready(&endpoint, None).await;
            drop(lock);
            return result.map(|()| endpoint);
        }
        reclaim_dead_owner_socket(&socket_path)?;
    } else if path_exists_no_follow(&socket_path)? {
        return Err(ExecutionManagerError::Unavailable(format!(
            "refusing to reclaim unowned native Linux OCI socket {} without an identity record",
            socket_path.display()
        )));
    }

    let mut child = spawn_owner(service_root, artifacts)?;
    let pid = child.id();
    let pid_start_time = crate::process::pid_start_time(pid).ok_or_else(|| {
        let _ = child.kill();
        let _ = child.wait();
        ExecutionManagerError::Unavailable(format!(
            "could not capture native Linux OCI owner identity for PID {pid}"
        ))
    })?;
    let record = NativeLinuxOwnerRecord::new(pid, pid_start_time, artifacts, socket_path.clone());
    if let Err(error) = write_owner_record(&record_path, &record) {
        let _ = child.kill();
        let _ = child.wait();
        reclaim_dead_owner_socket(&socket_path)?;
        return Err(error);
    }

    let ready = wait_until_ready(&endpoint, Some(&mut child)).await;
    if let Err(error) = ready {
        let _ = child.kill();
        let _ = child.wait();
        let _ = remove_record_if_same(&record_path, &record);
        let _ = reclaim_dead_owner_socket(&socket_path);
        return Err(error);
    }
    // A short-lived CLI will naturally hand the owner to init, while a
    // long-lived embedding process must still reap an owner that later exits.
    // Retaining the Child in this waiter prevents a persistent SDK host from
    // accumulating a zombie without tying the owner's lifetime to the caller.
    std::thread::Builder::new()
        .name("a3s-oci-owner-reaper".to_string())
        .spawn(move || {
            let _ = child.wait();
        })
        .map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to start native Linux OCI owner reaper: {error}"
            ))
        })?;
    drop(lock);
    Ok(endpoint)
}

async fn wait_until_ready(
    endpoint: &OciRuntimeEndpoint,
    mut child: Option<&mut Child>,
) -> ExecutionManagerResult<()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut last_error = None;
    loop {
        if let Some(child) = child.as_deref_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    return Err(ExecutionManagerError::Unavailable(format!(
                        "native Linux OCI owner exited during startup with {status}"
                    )))
                }
                Ok(None) => {}
                Err(error) => {
                    return Err(ExecutionManagerError::Unavailable(format!(
                        "failed to inspect native Linux OCI owner startup: {error}"
                    )))
                }
            }
        }
        match OciLifecycleAdapter::connect(endpoint.clone()).await {
            Ok(adapter) => match adapter.require_isolation(ExecutionIsolation::Sandbox).await {
                Ok(_) => return Ok(()),
                Err(error) => last_error = Some(error.to_string()),
            },
            Err(error) => last_error = Some(error.to_string()),
        }
        if Instant::now() >= deadline {
            return Err(ExecutionManagerError::Unavailable(format!(
                "native Linux OCI owner did not publish a launch-ready SDK endpoint within {} ms: {}",
                STARTUP_TIMEOUT.as_millis(),
                last_error.unwrap_or_else(|| "endpoint unavailable".to_string())
            )));
        }
        tokio::time::sleep(STARTUP_POLL_INTERVAL).await;
    }
}

fn spawn_owner(service_root: &Path, artifacts: &CertifiedA3sOci) -> ExecutionManagerResult<Child> {
    let stdout = open_owner_log(&service_root.join("owner.stdout.log"))?;
    let stderr = open_owner_log(&service_root.join("owner.stderr.log"))?;
    let mut command = Command::new(&artifacts.runtime_path);
    command
        .arg("native-linux-host-service")
        .arg("--root")
        .arg(service_root)
        .arg("--agent")
        .arg(&artifacts.agent_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    // SAFETY: the closure performs only the async-signal-safe setsid syscall
    // between fork and exec and does not access shared Rust state.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    command.spawn().map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to spawn native Linux OCI owner {}: {error}",
            artifacts.runtime_path.display()
        ))
    })
}

fn open_owner_log(path: &Path) -> ExecutionManagerResult<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to open native Linux OCI owner log {}: {error}",
                path.display()
            ))
        })?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to protect native Linux OCI owner log {}: {error}",
            path.display()
        ))
    })?;
    Ok(file)
}

fn validate_service_root(path: &Path) -> ExecutionManagerResult<()> {
    if !path.is_absolute()
        || path.parent().is_none()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(ExecutionManagerError::InvalidRequest(format!(
            "native Linux OCI service root must be an absolute normalized non-root path: {}",
            path.display()
        )));
    }
    Ok(())
}

fn prepare_service_root(path: &Path) -> ExecutionManagerResult<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(path).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to create native Linux OCI service root {}: {error}",
            path.display()
        ))
    })?;
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to inspect native Linux OCI service root {}: {error}",
            path.display()
        ))
    })?;
    // SAFETY: geteuid has no preconditions or failure result.
    let effective_uid = unsafe { libc::geteuid() };
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != effective_uid
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(ExecutionManagerError::Unavailable(format!(
            "native Linux OCI service root {} must be a real UID {effective_uid}-owned directory with mode 0700",
            path.display()
        )));
    }
    let canonical = std::fs::canonicalize(path).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to canonicalize native Linux OCI service root {}: {error}",
            path.display()
        ))
    })?;
    if canonical != path {
        return Err(ExecutionManagerError::Unavailable(format!(
            "native Linux OCI service root resolves through an alias: {} -> {}",
            path.display(),
            canonical.display()
        )));
    }
    Ok(())
}

fn load_owner_record(
    path: &Path,
    expected_socket: &Path,
) -> ExecutionManagerResult<Option<NativeLinuxOwnerRecord>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ExecutionManagerError::Unavailable(format!(
                "failed to inspect native Linux OCI owner record {}: {error}",
                path.display()
            )))
        }
    };
    // SAFETY: geteuid has no preconditions or failure result.
    let effective_uid = unsafe { libc::geteuid() };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != effective_uid
        || metadata.mode() & 0o777 != 0o600
        || metadata.len() > 16 * 1024
    {
        return Err(ExecutionManagerError::Unavailable(format!(
            "native Linux OCI owner record {} is not a protected bounded regular file",
            path.display()
        )));
    }
    let bytes = std::fs::read(path).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to read native Linux OCI owner record {}: {error}",
            path.display()
        ))
    })?;
    let record: NativeLinuxOwnerRecord = serde_json::from_slice(&bytes).map_err(|error| {
        ExecutionManagerError::Internal(format!(
            "native Linux OCI owner record {} is invalid: {error}",
            path.display()
        ))
    })?;
    record.validate(expected_socket)?;
    Ok(Some(record))
}

fn write_owner_record(path: &Path, record: &NativeLinuxOwnerRecord) -> ExecutionManagerResult<()> {
    let bytes = serde_json::to_vec_pretty(record).map_err(|error| {
        ExecutionManagerError::Internal(format!(
            "failed to encode native Linux OCI owner record: {error}"
        ))
    })?;
    let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4().simple()));
    let result = (|| -> std::io::Result<()> {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temporary);
        return Err(ExecutionManagerError::Unavailable(format!(
            "failed to persist native Linux OCI owner record {}: {error}",
            path.display()
        )));
    }
    Ok(())
}

fn remove_record_if_same(
    path: &Path,
    expected: &NativeLinuxOwnerRecord,
) -> ExecutionManagerResult<()> {
    let current = load_owner_record(path, &expected.socket_path)?;
    if current.as_ref() == Some(expected) {
        std::fs::remove_file(path).map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to remove failed native Linux OCI owner record {}: {error}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

fn reclaim_dead_owner_socket(path: &Path) -> ExecutionManagerResult<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(ExecutionManagerError::Unavailable(format!(
                "failed to inspect stale native Linux OCI socket {}: {error}",
                path.display()
            )))
        }
    };
    // SAFETY: geteuid has no preconditions or failure result.
    let effective_uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_socket() || metadata.uid() != effective_uid {
        return Err(ExecutionManagerError::Unavailable(format!(
            "refusing to remove stale native Linux OCI path {} because it is not a socket owned by UID {effective_uid}",
            path.display()
        )));
    }
    std::fs::remove_file(path).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to remove stale native Linux OCI socket {}: {error}",
            path.display()
        ))
    })
}

fn path_exists_no_follow(path: &Path) -> ExecutionManagerResult<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(ExecutionManagerError::Unavailable(format!(
            "failed to inspect native Linux OCI path {}: {error}",
            path.display()
        ))),
    }
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_record_fences_endpoint_and_artifacts() {
        let artifacts = CertifiedA3sOci {
            runtime_path: PathBuf::from("/opt/a3s/a3s-oci"),
            runtime_sha256: "a".repeat(64),
            agent_path: PathBuf::from("/opt/a3s/a3s-oci-agent"),
            agent_sha256: "b".repeat(64),
        };
        let socket = PathBuf::from("/tmp/a3s-owner/runtime.sock");
        let record = NativeLinuxOwnerRecord::new(42, 7, &artifacts, socket.clone());
        record.validate(&socket).unwrap();
        assert!(record.artifacts_match(&artifacts));
        assert!(record.validate(Path::new("/tmp/other.sock")).is_err());
    }

    #[test]
    fn service_root_rejects_relative_and_parent_paths() {
        assert!(validate_service_root(Path::new("relative")).is_err());
        assert!(validate_service_root(Path::new("/tmp/a/../b")).is_err());
        assert!(validate_service_root(Path::new("/")).is_err());
    }
}
