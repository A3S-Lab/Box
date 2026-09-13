//! Identity-fenced startup and reuse of the Linux KVM OCI qualification owner.
//!
//! Qualification-only: this mirrors native Linux Sandbox owner fencing for the
//! `box-kvm-qualification-service` Host. It does not claim MicroVM production
//! cutover, B2 close, or default omit→OCI routing.

use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use a3s_box_core::{ExecutionIsolation, ExecutionManagerError, ExecutionManagerResult};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{OciLifecycleAdapter, OciRuntimeEndpoint};
use crate::file_lock::FileLock;

const OWNER_RECORD_SCHEMA: &str = "a3s.box.linux-kvm-oci-owner.v1";
const OWNER_RECORD_NAME: &str = "box-owner.json";
const OWNER_LOCK_TARGET: &str = "box-kvm-owner";
const OWNER_SOCKET_NAME: &str = "runtime.sock";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Certified artifacts required to spawn or reuse a Box-owned KVM qualification Host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinuxKvmOwnerArtifacts {
    pub runtime_path: PathBuf,
    pub runtime_sha256: String,
    pub shim_path: PathBuf,
    pub shim_sha256: String,
    pub system_image_manifest: PathBuf,
    pub system_image_manifest_sha256: String,
}

impl LinuxKvmOwnerArtifacts {
    pub(crate) fn certify(
        runtime_path: impl Into<PathBuf>,
        shim_path: impl Into<PathBuf>,
        system_image_manifest: impl Into<PathBuf>,
    ) -> ExecutionManagerResult<Self> {
        let runtime_path = runtime_path.into();
        let shim_path = shim_path.into();
        let system_image_manifest = system_image_manifest.into();
        for (label, path) in [
            ("runtime", runtime_path.as_path()),
            ("shim", shim_path.as_path()),
            ("system-image manifest", system_image_manifest.as_path()),
        ] {
            if !path.is_absolute() {
                return Err(ExecutionManagerError::InvalidRequest(format!(
                    "Linux KVM OCI owner {label} path must be absolute: {}",
                    path.display()
                )));
            }
        }
        Ok(Self {
            runtime_sha256: sha256_file(&runtime_path)?,
            shim_sha256: sha256_file(&shim_path)?,
            system_image_manifest_sha256: sha256_file(&system_image_manifest)?,
            runtime_path,
            shim_path,
            system_image_manifest,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LinuxKvmOwnerRecord {
    schema: String,
    pid: u32,
    pid_start_time: u64,
    runtime_path: PathBuf,
    runtime_sha256: String,
    shim_path: PathBuf,
    shim_sha256: String,
    system_image_manifest: PathBuf,
    system_image_manifest_sha256: String,
    socket_path: PathBuf,
}

impl LinuxKvmOwnerRecord {
    fn new(
        pid: u32,
        pid_start_time: u64,
        artifacts: &LinuxKvmOwnerArtifacts,
        socket_path: PathBuf,
    ) -> Self {
        Self {
            schema: OWNER_RECORD_SCHEMA.to_string(),
            pid,
            pid_start_time,
            runtime_path: artifacts.runtime_path.clone(),
            runtime_sha256: artifacts.runtime_sha256.clone(),
            shim_path: artifacts.shim_path.clone(),
            shim_sha256: artifacts.shim_sha256.clone(),
            system_image_manifest: artifacts.system_image_manifest.clone(),
            system_image_manifest_sha256: artifacts.system_image_manifest_sha256.clone(),
            socket_path,
        }
    }

    fn validate(&self, expected_socket: &Path) -> ExecutionManagerResult<()> {
        if self.schema != OWNER_RECORD_SCHEMA
            || self.pid == 0
            || self.pid_start_time == 0
            || self.socket_path != expected_socket
            || !self.runtime_path.is_absolute()
            || !self.shim_path.is_absolute()
            || !self.system_image_manifest.is_absolute()
            || !is_sha256_hex(&self.runtime_sha256)
            || !is_sha256_hex(&self.shim_sha256)
            || !is_sha256_hex(&self.system_image_manifest_sha256)
        {
            return Err(ExecutionManagerError::Internal(
                "Linux KVM OCI owner record is malformed or belongs to another endpoint"
                    .to_string(),
            ));
        }
        Ok(())
    }

    fn artifacts_match(&self, expected: &LinuxKvmOwnerArtifacts) -> bool {
        self.runtime_path == expected.runtime_path
            && self.runtime_sha256 == expected.runtime_sha256
            && self.shim_path == expected.shim_path
            && self.shim_sha256 == expected.shim_sha256
            && self.system_image_manifest == expected.system_image_manifest
            && self.system_image_manifest_sha256 == expected.system_image_manifest_sha256
    }

    fn is_alive(&self) -> bool {
        crate::process::is_process_running_with_identity(self.pid, Some(self.pid_start_time))
    }
}

/// Ensure a Box-owned Linux KVM qualification Host is identity-fenced and ready.
pub(crate) async fn ensure_linux_kvm_oci_owner(
    service_root: &Path,
    artifacts: &LinuxKvmOwnerArtifacts,
) -> ExecutionManagerResult<OciRuntimeEndpoint> {
    validate_service_root(service_root)?;
    let root = service_root.to_path_buf();
    tokio::task::spawn_blocking(move || prepare_service_root(&root))
        .await
        .map_err(|error| {
            ExecutionManagerError::Internal(format!(
                "Linux KVM OCI owner root task failed: {error}"
            ))
        })??;

    let lock_target = service_root.join(OWNER_LOCK_TARGET);
    let lock = tokio::task::spawn_blocking(move || FileLock::acquire(&lock_target))
        .await
        .map_err(|error| {
            ExecutionManagerError::Internal(format!(
                "Linux KVM OCI owner lock task failed: {error}"
            ))
        })?
        .map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to acquire Linux KVM OCI owner lock: {error}"
            ))
        })?;

    let socket_path = service_root.join(OWNER_SOCKET_NAME);
    let endpoint = OciRuntimeEndpoint::unix_socket(socket_path.clone())?;
    let record_path = service_root.join(OWNER_RECORD_NAME);
    if let Some(record) = load_owner_record(&record_path, &socket_path)? {
        if record.is_alive() {
            if !record.artifacts_match(artifacts) {
                return Err(ExecutionManagerError::Unavailable(
                    "the live Linux KVM OCI owner uses different runtime artifacts; stop it explicitly before changing artifacts"
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
            "refusing to reclaim unowned Linux KVM OCI socket {} without an identity record",
            socket_path.display()
        )));
    }

    let mut child = spawn_owner(service_root, artifacts)?;
    let launch_pid = child.id();
    let launch_start_time = crate::process::pid_start_time(launch_pid).ok_or_else(|| {
        let _ = child.kill();
        let _ = child.wait();
        ExecutionManagerError::Unavailable(format!(
            "could not capture Linux KVM OCI owner launch identity for PID {launch_pid}"
        ))
    })?;
    let provisional = LinuxKvmOwnerRecord::new(
        launch_pid,
        launch_start_time,
        artifacts,
        socket_path.clone(),
    );
    if let Err(error) = write_owner_record(&record_path, &provisional) {
        let _ = child.kill();
        let _ = child.wait();
        reclaim_dead_owner_socket(&socket_path)?;
        return Err(error);
    }

    let ready = wait_until_ready(&endpoint, Some(&mut child)).await;
    if let Err(error) = ready {
        let detail = read_owner_log_tail(service_root);
        let _ = child.kill();
        let _ = child.wait();
        let _ = remove_record_if_same(&record_path, &provisional);
        let _ = reclaim_dead_owner_socket(&socket_path);
        return Err(match error {
            ExecutionManagerError::Unavailable(message) if !detail.is_empty() => {
                ExecutionManagerError::Unavailable(format!("{message}{detail}"))
            }
            other => other,
        });
    }
    let _record = match resolve_owner_identity_from_socket(&socket_path, artifacts) {
        Ok(record) => {
            if let Err(error) = write_owner_record(&record_path, &record) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = remove_record_if_same(&record_path, &provisional);
                let _ = reclaim_dead_owner_socket(&socket_path);
                return Err(error);
            }
            record
        }
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = remove_record_if_same(&record_path, &provisional);
            let _ = reclaim_dead_owner_socket(&socket_path);
            return Err(error);
        }
    };
    std::thread::Builder::new()
        .name("a3s-kvm-oci-owner-reaper".to_string())
        .spawn(move || {
            let _ = child.wait();
        })
        .map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to start Linux KVM OCI owner reaper: {error}"
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
    loop {
        if let Some(child) = child.as_deref_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    return Err(ExecutionManagerError::Unavailable(format!(
                        "Linux KVM OCI owner exited during startup with {status}"
                    )));
                }
                Ok(None) => {}
                Err(error) => {
                    return Err(ExecutionManagerError::Unavailable(format!(
                        "failed to inspect Linux KVM OCI owner startup: {error}"
                    )));
                }
            }
        }
        let last_error = match OciLifecycleAdapter::connect(endpoint.clone()).await {
            Ok(adapter) => match adapter.require_isolation(ExecutionIsolation::Microvm).await {
                Ok(_) => return Ok(()),
                Err(error) => error.to_string(),
            },
            Err(error) => error.to_string(),
        };
        if Instant::now() >= deadline {
            return Err(ExecutionManagerError::Unavailable(format!(
                "Linux KVM OCI owner did not publish a launch-ready SDK endpoint within {} ms: {}",
                STARTUP_TIMEOUT.as_millis(),
                last_error
            )));
        }
        tokio::time::sleep(STARTUP_POLL_INTERVAL).await;
    }
}

fn resolve_owner_identity_from_socket(
    socket_path: &Path,
    artifacts: &LinuxKvmOwnerArtifacts,
) -> ExecutionManagerResult<LinuxKvmOwnerRecord> {
    use std::mem::{size_of, MaybeUninit};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    let stream = UnixStream::connect(socket_path).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to authenticate Linux KVM OCI owner socket {}: {error}",
            socket_path.display()
        ))
    })?;
    let mut credentials = MaybeUninit::<libc::ucred>::zeroed();
    let mut value_length =
        libc::socklen_t::try_from(size_of::<libc::ucred>()).map_err(|error| {
            ExecutionManagerError::Internal(format!(
                "failed to represent SO_PEERCRED value size: {error}"
            ))
        })?;
    // SAFETY: the stream owns a connected Unix descriptor and the output
    // storage is valid for one ucred structure.
    let status = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut value_length,
        )
    };
    if status != 0 {
        return Err(ExecutionManagerError::Unavailable(format!(
            "failed to read SO_PEERCRED for Linux KVM OCI owner {}: {}",
            socket_path.display(),
            std::io::Error::last_os_error()
        )));
    }
    if usize::try_from(value_length).ok() != Some(size_of::<libc::ucred>()) {
        return Err(ExecutionManagerError::Unavailable(format!(
            "SO_PEERCRED returned {value_length} bytes for Linux KVM OCI owner {}",
            socket_path.display()
        )));
    }
    // SAFETY: getsockopt reported a full ucred write into credentials.
    let credentials = unsafe { credentials.assume_init() };
    let pid = credentials.pid as u32;
    if pid == 0 {
        return Err(ExecutionManagerError::Unavailable(
            "Linux KVM OCI owner socket returned an invalid peer PID".to_string(),
        ));
    }
    let pid_start_time = crate::process::pid_start_time(pid).ok_or_else(|| {
        ExecutionManagerError::Unavailable(format!(
            "could not capture Linux KVM OCI owner identity for peer PID {pid}"
        ))
    })?;
    Ok(LinuxKvmOwnerRecord::new(
        pid,
        pid_start_time,
        artifacts,
        socket_path.to_path_buf(),
    ))
}

fn spawn_owner(
    service_root: &Path,
    artifacts: &LinuxKvmOwnerArtifacts,
) -> ExecutionManagerResult<Child> {
    let stdout = open_owner_log(&service_root.join("owner.stdout.log"))?;
    let stderr = open_owner_log(&service_root.join("owner.stderr.log"))?;
    let mut command = Command::new(&artifacts.runtime_path);
    command
        .arg("box-kvm-qualification-service")
        .arg("--root")
        .arg(service_root)
        .arg("--shim")
        .arg(&artifacts.shim_path)
        .arg("--system-image-manifest")
        .arg(&artifacts.system_image_manifest)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    // Live qualification exports A3S_OCI_KVM_SESSION_OWNER=1; preserve it so
    // ensure-spawned Hosts retain session-owner create behavior.
    if let Some(value) = std::env::var_os("A3S_OCI_KVM_SESSION_OWNER") {
        command.env("A3S_OCI_KVM_SESSION_OWNER", value);
    }
    // SAFETY: setsid only; no shared Rust state between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to spawn Linux KVM OCI owner {}: {error}",
            artifacts.runtime_path.display()
        ))
    })
}

fn open_owner_log(path: &Path) -> ExecutionManagerResult<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to open Linux KVM OCI owner log {}: {error}",
                path.display()
            ))
        })?;
    chown_to_owner_fs(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to protect Linux KVM OCI owner log {}: {error}",
            path.display()
        ))
    })?;
    Ok(file)
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
            "Linux KVM OCI service root must be an absolute normalized non-root path: {}",
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
            "failed to create Linux KVM OCI service root {}: {error}",
            path.display()
        ))
    })?;
    chown_to_owner_fs(path)?;
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to inspect Linux KVM OCI service root {}: {error}",
            path.display()
        ))
    })?;
    let (owner_uid, _) = owner_fs_ids();
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != owner_uid
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(ExecutionManagerError::Unavailable(format!(
            "Linux KVM OCI service root {} must be a real UID {owner_uid}-owned directory with mode 0700",
            path.display()
        )));
    }
    let canonical = std::fs::canonicalize(path).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to canonicalize Linux KVM OCI service root {}: {error}",
            path.display()
        ))
    })?;
    if canonical != path {
        return Err(ExecutionManagerError::Unavailable(format!(
            "Linux KVM OCI service root resolves through an alias: {} -> {}",
            path.display(),
            canonical.display()
        )));
    }
    Ok(())
}

fn load_owner_record(
    path: &Path,
    expected_socket: &Path,
) -> ExecutionManagerResult<Option<LinuxKvmOwnerRecord>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ExecutionManagerError::Unavailable(format!(
                "failed to inspect Linux KVM OCI owner record {}: {error}",
                path.display()
            )))
        }
    };
    let (owner_uid, _) = owner_fs_ids();
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != owner_uid
        || metadata.mode() & 0o777 != 0o600
        || metadata.len() > 16 * 1024
    {
        return Err(ExecutionManagerError::Unavailable(format!(
            "Linux KVM OCI owner record {} is not a protected bounded regular file",
            path.display()
        )));
    }
    let bytes = std::fs::read(path).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to read Linux KVM OCI owner record {}: {error}",
            path.display()
        ))
    })?;
    let record: LinuxKvmOwnerRecord = serde_json::from_slice(&bytes).map_err(|error| {
        ExecutionManagerError::Internal(format!(
            "Linux KVM OCI owner record {} is invalid: {error}",
            path.display()
        ))
    })?;
    record.validate(expected_socket)?;
    Ok(Some(record))
}

fn write_owner_record(path: &Path, record: &LinuxKvmOwnerRecord) -> ExecutionManagerResult<()> {
    let bytes = serde_json::to_vec_pretty(record).map_err(|error| {
        ExecutionManagerError::Internal(format!(
            "failed to encode Linux KVM OCI owner record: {error}"
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
            "failed to persist Linux KVM OCI owner record {}: {error}",
            path.display()
        )));
    }
    chown_to_owner_fs(path)?;
    Ok(())
}

fn remove_record_if_same(
    path: &Path,
    expected: &LinuxKvmOwnerRecord,
) -> ExecutionManagerResult<()> {
    let current = load_owner_record(path, &expected.socket_path)?;
    if current.as_ref() == Some(expected) {
        std::fs::remove_file(path).map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to remove failed Linux KVM OCI owner record {}: {error}",
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
                "failed to inspect stale Linux KVM OCI socket {}: {error}",
                path.display()
            )))
        }
    };
    let (owner_uid, _) = owner_fs_ids();
    if !metadata.file_type().is_socket() || metadata.uid() != owner_uid {
        return Err(ExecutionManagerError::Unavailable(format!(
            "refusing to remove stale Linux KVM OCI path {} because it is not a socket owned by UID {owner_uid}",
            path.display()
        )));
    }
    std::fs::remove_file(path).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to remove stale Linux KVM OCI socket {}: {error}",
            path.display()
        ))
    })
}

fn path_exists_no_follow(path: &Path) -> ExecutionManagerResult<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(ExecutionManagerError::Unavailable(format!(
            "failed to inspect Linux KVM OCI path {}: {error}",
            path.display()
        ))),
    }
}

fn owner_fs_ids() -> (u32, u32) {
    // SAFETY: credential queries have no pointer arguments or failure results.
    let (ruid, rgid, euid, egid) = unsafe {
        (
            libc::getuid(),
            libc::getgid(),
            libc::geteuid(),
            libc::getegid(),
        )
    };
    if euid == 0 && ruid != 0 {
        (ruid, rgid)
    } else {
        (euid, egid)
    }
}

fn chown_to_owner_fs(path: &Path) -> ExecutionManagerResult<()> {
    use std::os::unix::ffi::OsStrExt;
    let (uid, gid) = owner_fs_ids();
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        ExecutionManagerError::Unavailable(format!(
            "Linux KVM OCI path contains an interior NUL: {}",
            path.display()
        ))
    })?;
    // SAFETY: chown takes a NUL-terminated path owned for the duration of the call.
    let rc = unsafe { libc::chown(c_path.as_ptr(), uid, gid) };
    if rc != 0 {
        return Err(ExecutionManagerError::Unavailable(format!(
            "failed to assign Linux KVM OCI path {} to UID {uid}: {}",
            path.display(),
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> ExecutionManagerResult<String> {
    let mut file = std::fs::File::open(path).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to open Linux KVM OCI artifact {}: {error}",
            path.display()
        ))
    })?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to hash Linux KVM OCI artifact {}: {error}",
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
    use std::time::{Duration, Instant};

    use super::*;

    fn sample_artifacts() -> LinuxKvmOwnerArtifacts {
        LinuxKvmOwnerArtifacts {
            runtime_path: PathBuf::from("/opt/a3s/a3s-oci"),
            runtime_sha256: "a".repeat(64),
            shim_path: PathBuf::from("/opt/a3s/a3s-oci-kvm-shim"),
            shim_sha256: "b".repeat(64),
            system_image_manifest: PathBuf::from("/opt/a3s/system-image.json"),
            system_image_manifest_sha256: "c".repeat(64),
        }
    }

    #[test]
    fn owner_record_fences_endpoint_and_artifacts() {
        let artifacts = sample_artifacts();
        let socket = PathBuf::from("/tmp/a3s-kvm-owner/runtime.sock");
        let record = LinuxKvmOwnerRecord::new(42, 7, &artifacts, socket.clone());
        record.validate(&socket).unwrap();
        assert!(record.artifacts_match(&artifacts));
        assert!(record.validate(Path::new("/tmp/other.sock")).is_err());

        let mut drifted = artifacts.clone();
        drifted.shim_sha256 = "d".repeat(64);
        assert!(!record.artifacts_match(&drifted));
    }

    #[test]
    fn service_root_rejects_relative_and_parent_paths() {
        assert!(validate_service_root(Path::new("relative")).is_err());
        assert!(validate_service_root(Path::new("/tmp/a/../b")).is_err());
        assert!(validate_service_root(Path::new("/")).is_err());
    }

    #[test]
    fn completed_zombie_owner_is_not_reused() {
        let artifacts = sample_artifacts();
        let mut child = Command::new("/bin/true")
            .spawn()
            .expect("spawn completed owner fixture");
        let pid = child.id();
        let start_time = crate::process::pid_start_time(pid).expect("capture owner identity");
        let deadline = Instant::now() + Duration::from_secs(2);
        while crate::process::is_process_running_with_identity(pid, Some(start_time))
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            crate::process::is_process_alive_with_identity(pid, Some(start_time)),
            "unreaped fixture must remain addressable as a zombie"
        );

        let record = LinuxKvmOwnerRecord::new(
            pid,
            start_time,
            &artifacts,
            PathBuf::from("/tmp/a3s-kvm-owner/runtime.sock"),
        );
        assert!(!record.is_alive(), "a zombie owner must be reclaimed");
        child.wait().expect("reap completed owner fixture");
    }
}
