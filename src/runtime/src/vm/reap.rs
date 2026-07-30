//! Crash-recovery reaping of orphaned box runtimes.
//!
//! A clean shutdown destroys each VM via its in-memory handle (overlay
//! unmount + box-dir removal). After a crash (`SIGKILL`, OOM, power loss) the
//! CRI process dies but its `a3s-box-shim` microVMs are reparented to `init`
//! and keep running, holding their overlay mounts and box directories. On the
//! next start the CRI has no handle to them, so without this they leak across
//! restarts. [`reap_orphaned_box`] reclaims one such box by id.

#[cfg(target_os = "linux")]
use std::path::Path;

/// Reap an orphaned sandbox microVM left by a previous (crashed) process:
/// kill its `a3s-box-shim`, unmount its overlay, and remove its box directory.
///
/// Idempotent and best-effort: a box with no leftovers (e.g. after a graceful
/// shutdown) is a no-op. Safe to call for every known sandbox id on startup.
#[cfg(target_os = "linux")]
pub fn reap_orphaned_box(box_id: &str) {
    reap_orphaned_box_in(&a3s_box_core::dirs_home(), box_id);
}

/// Delete a durable Sandbox OCI runtime generation without removing its Box
/// rootfs or persisted CLI state.
///
/// Callers must run this before unmounting or deleting Box paths: a failed
/// runtime cleanup may mean a shared-kernel process still uses the rootfs.
#[cfg(target_os = "linux")]
pub fn cleanup_recorded_sandbox_runtime(box_dir: &Path, box_id: &str) -> a3s_box_core::Result<()> {
    cleanup_recorded_sandbox_runtime_in(&a3s_box_core::dirs_home(), box_dir, box_id)
}

/// Wait for a naturally exited Sandbox generation to finish projecting both
/// console streams before a caller archives or reads its final logs.
#[cfg(target_os = "linux")]
pub fn wait_for_recorded_sandbox_log_drain(
    box_dir: &Path,
    box_id: &str,
    timeout: std::time::Duration,
) -> a3s_box_core::Result<bool> {
    let home_dir = a3s_box_core::dirs_home();
    wait_for_recorded_sandbox_log_drain_in(&home_dir, box_dir, box_id, timeout)
}

#[cfg(target_os = "linux")]
fn wait_for_recorded_sandbox_log_drain_in(
    home_dir: &Path,
    box_dir: &Path,
    box_id: &str,
    timeout: std::time::Duration,
) -> a3s_box_core::Result<bool> {
    // Waiting is read-only: it neither executes the recorded runtime nor
    // signals a process. Validate fixed paths and the PID/start-time pair.
    let Some(record) = load_recorded_sandbox_runtime_identity(home_dir, box_dir, box_id)? else {
        return Ok(true);
    };
    Ok(wait_for_log_worker_identity(&record, timeout))
}

#[cfg(target_os = "linux")]
pub(crate) fn cleanup_recorded_sandbox_runtime_in(
    home_dir: &Path,
    box_dir: &Path,
    box_id: &str,
) -> a3s_box_core::Result<()> {
    match reap_recorded_sandbox(home_dir, box_dir, box_id) {
        SandboxReap::NotPresent | SandboxReap::Cleaned => Ok(()),
        SandboxReap::Failed(reason) => Err(a3s_box_core::BoxError::StateError(format!(
            "Failed to clean recorded Sandbox runtime for {box_id}: {reason}; refusing to touch its rootfs"
        ))),
    }
}

/// [`reap_orphaned_box`] against an explicit home directory (for testing).
#[cfg(target_os = "linux")]
fn reap_orphaned_box_in(home_dir: &Path, box_id: &str) {
    let box_dir = home_dir.join("boxes").join(box_id);
    if !box_dir.exists() {
        return;
    }

    let runtime_owned_cgroup = match reap_recorded_sandbox(home_dir, &box_dir, box_id) {
        SandboxReap::NotPresent => false,
        SandboxReap::Cleaned => true,
        SandboxReap::Failed(reason) => {
            // A live shared-kernel process may still be using the rootfs. Never
            // unmount or delete it after an unverified/failed runtime cleanup.
            tracing::error!(box_id, %reason, "Refusing to touch an unreaped Sandbox rootfs");
            return;
        }
    };

    let killed = kill_orphaned_shim(box_id);
    // Wait for the killed shim(s) to actually exit before touching the overlay:
    // they hold the merged rootfs, so unmounting/removing it while they are
    // still alive would race the VM's own files.
    wait_for_exit(&killed, std::time::Duration::from_secs(5));

    if let Err(error) = crate::sandbox::cleanup_sandbox_mount_aliases(home_dir, box_id) {
        tracing::error!(
            box_id,
            %error,
            "Refusing to remove a box with attached Sandbox mount aliases"
        );
        return;
    }

    // Unmount the box overlay; MNT_DETACH (lazy) inside overlay_unmount handles
    // a mount that is somehow still busy.
    let merged = box_dir.join("merged");
    if merged.exists() {
        if let Err(error) = crate::rootfs::overlay::overlay_unmount(&merged) {
            tracing::warn!(
                box_id = %box_id,
                path = %merged.display(),
                error = %error,
                "Failed to unmount orphaned box overlay during crash recovery"
            );
        }
    }

    if let Err(error) = std::fs::remove_dir_all(&box_dir) {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                box_id = %box_id,
                path = %box_dir.display(),
                error = %error,
                "Failed to remove orphaned box directory during crash recovery"
            );
        }
    }

    // A legacy MicroVM shim could leave an empty host cgroup behind. A3S OCI
    // Runtime instead owns and removes the complete Sandbox hierarchy as part
    // of the successful delete above.
    if !runtime_owned_cgroup {
        let _ = std::fs::remove_dir(format!("/sys/fs/cgroup/a3s-box/{box_id}"));
    }

    if !killed.is_empty() {
        tracing::info!(box_id = %box_id, "Reaped orphaned sandbox microVM after CRI restart");
    }
}

/// Validated durable evidence for one live or stopped Sandbox generation.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub(crate) struct RecordedSandboxRuntime {
    pub(crate) runtime_path: std::path::PathBuf,
    pub(crate) runtime_sha256: Option<String>,
    pub(crate) agent_path: Option<std::path::PathBuf>,
    pub(crate) agent_sha256: Option<String>,
    pub(crate) runtime_root: std::path::PathBuf,
    pub(crate) runtime_socket: Option<std::path::PathBuf>,
    pub(crate) bundle_dir: std::path::PathBuf,
    pub(crate) init_pid: u32,
    pub(crate) generation: Option<u64>,
    pub(crate) owner_pid: Option<u32>,
    pub(crate) owner_pid_start_time: Option<u64>,
    pub(crate) log_worker_pid: Option<u32>,
    pub(crate) log_worker_pid_start_time: Option<u64>,
}

/// Load and validate the runtime-owned Sandbox record for one internal box ID.
///
/// Every persisted path is checked against the expected internal layout and
/// the recorded A3S OCI artifact identities are re-certified before use.
#[cfg(target_os = "linux")]
pub(crate) fn load_recorded_sandbox_runtime(
    home_dir: &Path,
    box_dir: &Path,
    box_id: &str,
) -> a3s_box_core::Result<Option<RecordedSandboxRuntime>> {
    let Some(record) = load_recorded_sandbox_runtime_identity(home_dir, box_dir, box_id)? else {
        return Ok(None);
    };
    verify_recorded_a3s_oci_owner(&record, box_id)?;
    Ok(Some(record))
}

#[cfg(target_os = "linux")]
fn load_recorded_sandbox_runtime_identity(
    home_dir: &Path,
    box_dir: &Path,
    box_id: &str,
) -> a3s_box_core::Result<Option<RecordedSandboxRuntime>> {
    let expected_box_dir = home_dir.join("boxes").join(box_id);
    if box_dir != expected_box_dir {
        return Err(a3s_box_core::BoxError::StateError(format!(
            "Sandbox runtime record has an unexpected host directory for {box_id}"
        )));
    }
    let record_path = box_dir.join("sandbox/runtime.json");
    let bytes = match std::fs::read(&record_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(a3s_box_core::BoxError::IoError(error)),
    };
    let record: crate::sandbox::runtime_record::SandboxRuntimeRecord =
        serde_json::from_slice(&bytes).map_err(|error| {
            a3s_box_core::BoxError::StateError(format!(
                "Invalid Sandbox runtime record at {}: {error}",
                record_path.display()
            ))
        })?;
    let expected_runtime_root = crate::vm::sandbox_runtime_root(home_dir, box_id);
    let legacy_runtime_root = crate::vm::legacy_sandbox_runtime_root(home_dir, box_id);
    let expected_bundle = box_dir.join("sandbox/bundle");
    let log_worker_identity_valid = match (record.log_worker_pid, record.log_worker_pid_start_time)
    {
        (None, None) => true,
        (Some(pid), Some(start_time)) => pid > 0 && start_time > 0,
        _ => false,
    };
    let runtime_identity_valid = record.schema
        == crate::sandbox::runtime_record::SANDBOX_RUNTIME_RECORD_SCHEMA
        && record.runtime_socket.as_deref()
            == Some(record.runtime_root.join("runtime.sock").as_path())
        && record.generation.is_some_and(|generation| generation > 0)
        && matches!(record.owner_pid, Some(pid) if pid > 0)
        && matches!(record.owner_pid_start_time, Some(start_time) if start_time > 0)
        && record.agent_path.is_some()
        && record.runtime_sha256.as_deref().is_some_and(valid_sha256)
        && record.agent_sha256.as_deref().is_some_and(valid_sha256);
    if !runtime_identity_valid
        || record.container_id != box_id
        || (record.runtime_root != expected_runtime_root
            && record.runtime_root != legacy_runtime_root)
        || record.bundle_dir != expected_bundle
        || record.init_pid == 0
        || !log_worker_identity_valid
    {
        return Err(a3s_box_core::BoxError::StateError(format!(
            "Sandbox runtime record failed path or identity validation for {box_id}"
        )));
    }

    Ok(Some(RecordedSandboxRuntime {
        runtime_path: record.runtime_path,
        runtime_sha256: record.runtime_sha256,
        agent_path: record.agent_path,
        agent_sha256: record.agent_sha256,
        runtime_root: record.runtime_root,
        runtime_socket: record.runtime_socket,
        bundle_dir: record.bundle_dir,
        init_pid: record.init_pid,
        generation: record.generation,
        owner_pid: record.owner_pid,
        owner_pid_start_time: record.owner_pid_start_time,
        log_worker_pid: record.log_worker_pid,
        log_worker_pid_start_time: record.log_worker_pid_start_time,
    }))
}

#[cfg(target_os = "linux")]
fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(target_os = "linux")]
fn verify_recorded_a3s_oci_owner(
    record: &RecordedSandboxRuntime,
    box_id: &str,
) -> a3s_box_core::Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};

    let (Some(owner_pid), Some(owner_start_time), Some(runtime_socket)) = (
        record.owner_pid,
        record.owner_pid_start_time,
        record.runtime_socket.as_ref(),
    ) else {
        return Err(a3s_box_core::BoxError::StateError(format!(
            "A3S OCI owner identity is incomplete for {box_id}"
        )));
    };
    if !crate::process::is_process_running_with_identity(owner_pid, Some(owner_start_time)) {
        return Err(a3s_box_core::BoxError::StateError(format!(
            "A3S OCI owner process is not running for {box_id}"
        )));
    }

    let runtime_path = record.runtime_path.canonicalize().map_err(|error| {
        a3s_box_core::BoxError::StateError(format!(
            "Cannot resolve recorded A3S OCI runtime for {box_id}: {error}"
        ))
    })?;
    if runtime_path != record.runtime_path
        || std::fs::read_link(format!("/proc/{owner_pid}/exe"))
            .ok()
            .as_deref()
            != Some(runtime_path.as_path())
    {
        return Err(a3s_box_core::BoxError::StateError(format!(
            "A3S OCI owner executable identity is invalid for {box_id}"
        )));
    }
    if sha256_file(&runtime_path).as_deref() != record.runtime_sha256.as_deref() {
        return Err(a3s_box_core::BoxError::StateError(format!(
            "A3S OCI runtime digest changed for {box_id}"
        )));
    }
    let agent_path = record.agent_path.as_ref().ok_or_else(|| {
        a3s_box_core::BoxError::StateError(format!(
            "A3S OCI agent identity is missing for {box_id}"
        ))
    })?;
    if agent_path.canonicalize().ok().as_deref() != Some(agent_path.as_path())
        || sha256_file(agent_path).as_deref() != record.agent_sha256.as_deref()
    {
        return Err(a3s_box_core::BoxError::StateError(format!(
            "A3S OCI agent artifact identity is invalid for {box_id}"
        )));
    }

    let metadata = std::fs::symlink_metadata(runtime_socket).map_err(|error| {
        a3s_box_core::BoxError::StateError(format!(
            "Cannot inspect A3S OCI endpoint for {box_id}: {error}"
        ))
    })?;
    if !metadata.file_type().is_socket()
        || metadata.permissions().mode() & 0o777 != 0o600
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(a3s_box_core::BoxError::StateError(format!(
            "A3S OCI endpoint identity is invalid for {box_id}"
        )));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn sha256_file(path: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Some(hex::encode(hasher.finalize()))
}

#[cfg(target_os = "linux")]
enum SandboxReap {
    NotPresent,
    Cleaned,
    Failed(String),
}

#[cfg(target_os = "linux")]
fn failed_sandbox_reap(box_id: &str, reason: impl Into<String>) -> SandboxReap {
    let reason = reason.into();
    tracing::error!(box_id, %reason, "Failed to reap recorded Sandbox runtime");
    SandboxReap::Failed(reason)
}

/// Reconcile one durable Sandbox record before touching its rootfs.
#[cfg(target_os = "linux")]
fn reap_recorded_sandbox(home_dir: &Path, box_dir: &Path, box_id: &str) -> SandboxReap {
    let record = match load_recorded_sandbox_runtime(home_dir, box_dir, box_id) {
        Ok(Some(record)) => record,
        Ok(None) => return SandboxReap::NotPresent,
        Err(error) => {
            return failed_sandbox_reap(
                box_id,
                format!("invalid runtime record during crash recovery: {error}"),
            );
        }
    };
    reap_orphaned_a3s_oci(record, box_dir, box_id)
}

#[cfg(target_os = "linux")]
fn reap_orphaned_a3s_oci(
    record: RecordedSandboxRuntime,
    box_dir: &Path,
    box_id: &str,
) -> SandboxReap {
    use a3s_oci_sdk::{
        ContainerId, ContainerTarget, DeleteMode, DeleteRequest, Generation, KillRequest,
        OciContainerState, Signal, StateRequest, WaitRequest,
    };

    let (Some(runtime_socket), Some(generation), Some(owner_pid), Some(owner_start_time)) = (
        record.runtime_socket.as_ref(),
        record.generation,
        record.owner_pid,
        record.owner_pid_start_time,
    ) else {
        return failed_sandbox_reap(
            box_id,
            "A3S OCI runtime record lost required recovery identity",
        );
    };
    let client = match crate::sandbox::a3s_oci_client::A3sOciClient::connect_blocking(
        runtime_socket.clone(),
    ) {
        Ok(client) => client,
        Err(error) => {
            return failed_sandbox_reap(
                box_id,
                format!("failed to connect to orphaned A3S OCI owner: {error}"),
            );
        }
    };
    let container_id = match ContainerId::new(box_id) {
        Ok(container_id) => container_id,
        Err(error) => {
            return failed_sandbox_reap(
                box_id,
                format!("invalid A3S OCI container identity during recovery: {error}"),
            );
        }
    };
    let target = ContainerTarget::exact(container_id, Generation(generation));
    let state = match client.state_optional(StateRequest {
        target: target.clone(),
    }) {
        Ok(state) => state,
        Err(error) => {
            return failed_sandbox_reap(
                box_id,
                format!("failed to query orphaned A3S OCI state: {error}"),
            );
        }
    };
    if state.is_some_and(|state| *state.state.status() != OciContainerState::Stopped) {
        let context = match recorded_operation_context(box_id, "reap-kill") {
            Ok(context) => context,
            Err(error) => {
                return failed_sandbox_reap(
                    box_id,
                    format!("failed to construct A3S OCI recovery operation: {error}"),
                );
            }
        };
        let signal = match Signal::new(libc::SIGKILL) {
            Ok(signal) => signal,
            Err(error) => {
                return failed_sandbox_reap(
                    box_id,
                    format!("failed to construct A3S OCI recovery signal: {error}"),
                );
            }
        };
        if let Err(error) = client.kill(KillRequest {
            context,
            target: target.clone(),
            signal,
            all: true,
        }) {
            return failed_sandbox_reap(
                box_id,
                format!("failed to signal orphaned A3S OCI Sandbox: {error}"),
            );
        }
        if let Err(error) = client.wait(WaitRequest {
            target: target.clone(),
            timeout_ms: Some(crate::sandbox::A3S_OCI_LIFECYCLE_TIMEOUT_MS),
        }) {
            return failed_sandbox_reap(
                box_id,
                format!("failed to wait for orphaned A3S OCI Sandbox: {error}"),
            );
        }
    }

    let context = match recorded_operation_context(box_id, "reap-delete") {
        Ok(context) => context,
        Err(error) => {
            return failed_sandbox_reap(
                box_id,
                format!("failed to construct A3S OCI delete operation: {error}"),
            );
        }
    };
    if let Err(error) = client.delete_if_present(DeleteRequest {
        context,
        target,
        mode: DeleteMode::Force,
    }) {
        return failed_sandbox_reap(
            box_id,
            format!("failed to delete orphaned A3S OCI state: {error}"),
        );
    }
    client.close();

    if let Err(error) = crate::sandbox::a3s_oci_owner::stop(owner_pid, owner_start_time) {
        return failed_sandbox_reap(
            box_id,
            format!("failed to stop orphaned A3S OCI owner {owner_pid}: {error}"),
        );
    }

    drain_recorded_log_worker(&record, box_id);
    if let Err(error) = crate::sandbox::cleanup_sandbox_mount_aliases(home_dir, box_id) {
        return failed_sandbox_reap(
            box_id,
            format!("failed to detach Sandbox attachment aliases: {error}"),
        );
    }
    let _ = std::fs::remove_dir_all(&record.bundle_dir);
    let _ = std::fs::remove_dir_all(&record.runtime_root);
    let _ = std::fs::remove_file(box_dir.join("sandbox/runtime.json"));
    tracing::info!(
        box_id,
        "Reaped orphaned A3S OCI Sandbox after runtime restart"
    );
    SandboxReap::Cleaned
}

#[cfg(target_os = "linux")]
fn recorded_operation_context(
    box_id: &str,
    operation: &str,
) -> a3s_box_core::Result<a3s_oci_sdk::OperationContext> {
    a3s_oci_sdk::OperationId::new(format!(
        "{box_id}-{operation}-{}",
        uuid::Uuid::new_v4().simple()
    ))
    .map(a3s_oci_sdk::OperationContext::new)
    .map_err(|error| a3s_box_core::BoxError::StateError(error.to_string()))
}

#[cfg(target_os = "linux")]
fn drain_recorded_log_worker(record: &RecordedSandboxRuntime, box_id: &str) {
    const LOG_WORKER_EXIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

    let (Some(pid), Some(start_time)) = (record.log_worker_pid, record.log_worker_pid_start_time)
    else {
        return;
    };
    if !wait_for_log_worker_identity(record, LOG_WORKER_EXIT_TIMEOUT) {
        tracing::warn!(
            box_id,
            log_worker_pid = pid,
            "Recovered Sandbox log worker did not drain after runtime cleanup; terminating it"
        );
        // Revalidate the stable identity immediately before signalling so a
        // PID reused during cleanup cannot be targeted.
        if crate::process::is_process_running_with_identity(pid, Some(start_time)) {
            if let Ok(pid) = i32::try_from(pid) {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }
    if !crate::process::wait_for_process_exit_with_identity(
        pid,
        start_time,
        LOG_WORKER_EXIT_TIMEOUT,
    ) {
        tracing::warn!(
            box_id,
            log_worker_pid = pid,
            "Recovered Sandbox log worker remained present after cleanup"
        );
    }
}

#[cfg(target_os = "linux")]
fn wait_for_log_worker_identity(
    record: &RecordedSandboxRuntime,
    timeout: std::time::Duration,
) -> bool {
    let (Some(pid), Some(start_time)) = (record.log_worker_pid, record.log_worker_pid_start_time)
    else {
        // Runtime records written before the worker fields have no process to
        // wait for and retain their legacy raw-console behavior.
        return true;
    };
    let deadline = std::time::Instant::now() + timeout;
    while crate::process::is_process_running_with_identity(pid, Some(start_time))
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    !crate::process::is_process_running_with_identity(pid, Some(start_time))
}

/// Poll until every pid in `pids` has exited, or `timeout` elapses.
#[cfg(target_os = "linux")]
fn wait_for_exit(pids: &[i32], timeout: std::time::Duration) {
    if pids.is_empty() {
        return;
    }
    // No `Instant::now` budget here (tests stub the clock); bound by iterations.
    let step = std::time::Duration::from_millis(50);
    let mut remaining = (timeout.as_millis() / step.as_millis().max(1)) as u32;
    while remaining > 0 {
        // `kill(pid, 0)` returns ESRCH once the pid is gone (and reaped).
        let any_alive = pids.iter().any(|&pid| unsafe { libc::kill(pid, 0) } == 0);
        if !any_alive {
            return;
        }
        std::thread::sleep(step);
        remaining -= 1;
    }
}

/// Non-Linux builds are development stubs (no microVMs to reap).
#[cfg(not(target_os = "linux"))]
pub fn reap_orphaned_box(_box_id: &str) {}

#[cfg(not(target_os = "linux"))]
pub fn cleanup_recorded_sandbox_runtime(
    _box_dir: &std::path::Path,
    _box_id: &str,
) -> a3s_box_core::Result<()> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn wait_for_recorded_sandbox_log_drain(
    _box_dir: &std::path::Path,
    _box_id: &str,
    _timeout: std::time::Duration,
) -> a3s_box_core::Result<bool> {
    Ok(true)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn cleanup_recorded_sandbox_runtime_in(
    _home_dir: &std::path::Path,
    _box_dir: &std::path::Path,
    _box_id: &str,
) -> a3s_box_core::Result<()> {
    Ok(())
}

#[cfg(all(test, not(target_os = "linux")))]
mod tests {
    use super::*;

    #[test]
    fn reap_orphaned_box_is_noop_on_non_linux() {
        reap_orphaned_box("non-linux-noop");
    }
}

/// SIGKILL any `a3s-box-shim` process whose command line carries `box_id`.
///
/// The shim is launched as `a3s-box-shim --config '{"box_id":"<id>",...}'`, so
/// matching on both the binary name AND the (UUID) box id scopes the kill to
/// exactly this sandbox's microVM — it can never hit an unrelated process.
#[cfg(target_os = "linux")]
fn kill_orphaned_shim(box_id: &str) -> Vec<i32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut killed = Vec::new();
    for entry in entries.flatten() {
        // Only numeric /proc/<pid> entries are processes.
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(Path::new("/proc").join(name).join("cmdline")) else {
            continue;
        };
        // cmdline is a NUL-separated argv; a plain substring check is enough.
        let cmdline = String::from_utf8_lossy(&cmdline);
        if cmdline.contains("a3s-box-shim") && cmdline.contains(box_id) {
            // SAFETY: kill(2) with a pid we just read from /proc; SIGKILL has no
            // memory effects. The double match (binary + UUID) bounds the target.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
            tracing::info!(box_id = %box_id, pid, "Killed orphaned shim during crash recovery");
            killed.push(pid);
        }
    }
    killed
}

#[cfg(all(test, target_os = "linux"))]
#[path = "reap_tests.rs"]
mod tests;
