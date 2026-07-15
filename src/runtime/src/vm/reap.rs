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
    // signals a process. Validate fixed paths and the PID/start-time pair, but
    // leave runtime artifact certification to paths that query or execute crun.
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
    match reap_orphaned_crun(home_dir, box_dir, box_id) {
        SandboxReap::NotPresent | SandboxReap::Cleaned => Ok(()),
        SandboxReap::Failed => Err(a3s_box_core::BoxError::StateError(format!(
            "Failed to clean recorded Sandbox runtime for {box_id}; refusing to touch its rootfs"
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

    match reap_orphaned_crun(home_dir, &box_dir, box_id) {
        SandboxReap::NotPresent | SandboxReap::Cleaned => {}
        SandboxReap::Failed => {
            // A live shared-kernel process may still be using the rootfs. Never
            // unmount or delete it after an unverified/failed runtime cleanup.
            return;
        }
    }

    let killed = kill_orphaned_shim(box_id);
    // Wait for the killed shim(s) to actually exit before touching the overlay:
    // they hold the merged rootfs, so unmounting/removing it while they are
    // still alive would race the VM's own files.
    wait_for_exit(&killed, std::time::Duration::from_secs(5));

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

    // Remove the box's host cgroup (the shim creates `/sys/fs/cgroup/a3s-box/<id>`
    // for host-side cgroup limits and can never remove it; an empty-dir rmdir
    // clears it now that the shim is killed).
    let _ = std::fs::remove_dir(format!("/sys/fs/cgroup/a3s-box/{box_id}"));

    if !killed.is_empty() {
        tracing::info!(box_id = %box_id, "Reaped orphaned sandbox microVM after CRI restart");
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct SandboxRuntimeRecord {
    schema: String,
    container_id: String,
    runtime_path: std::path::PathBuf,
    runtime_root: std::path::PathBuf,
    bundle_dir: std::path::PathBuf,
    init_pid: u32,
    #[serde(default)]
    log_worker_pid: Option<u32>,
    #[serde(default)]
    log_worker_pid_start_time: Option<u64>,
}

/// Validated durable evidence for one live or stopped Sandbox generation.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub(crate) struct RecordedSandboxRuntime {
    pub(crate) runtime_path: std::path::PathBuf,
    pub(crate) runtime_root: std::path::PathBuf,
    pub(crate) bundle_dir: std::path::PathBuf,
    pub(crate) init_pid: u32,
    pub(crate) log_worker_pid: Option<u32>,
    pub(crate) log_worker_pid_start_time: Option<u64>,
}

/// Load and validate the runtime-owned Sandbox record for one internal box ID.
///
/// Every persisted path is checked against the expected internal layout and
/// the recorded runtime binary is re-certified before callers may execute it.
#[cfg(target_os = "linux")]
pub(crate) fn load_recorded_sandbox_runtime(
    home_dir: &Path,
    box_dir: &Path,
    box_id: &str,
) -> a3s_box_core::Result<Option<RecordedSandboxRuntime>> {
    let Some(mut record) = load_recorded_sandbox_runtime_identity(home_dir, box_dir, box_id)?
    else {
        return Ok(None);
    };
    let capabilities = crate::sandbox::probe_sandbox_capabilities(Some(&record.runtime_path));
    let runtime = capabilities.runtime.ok_or_else(|| {
        a3s_box_core::BoxError::StateError(format!(
            "Cannot verify the recorded Sandbox runtime for {box_id}: {:?}",
            capabilities.failures
        ))
    })?;
    record.runtime_path = runtime.path;
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
    let record: SandboxRuntimeRecord = serde_json::from_slice(&bytes).map_err(|error| {
        a3s_box_core::BoxError::StateError(format!(
            "Invalid Sandbox runtime record at {}: {error}",
            record_path.display()
        ))
    })?;
    let expected_runtime_root = home_dir.join("run/crun").join(box_id);
    let expected_bundle = box_dir.join("sandbox/bundle");
    let log_worker_identity_valid = match (record.log_worker_pid, record.log_worker_pid_start_time)
    {
        (None, None) => true,
        (Some(pid), Some(start_time)) => pid > 0 && start_time > 0,
        _ => false,
    };
    if record.schema != "a3s.box.sandbox-runtime.v1"
        || record.container_id != box_id
        || record.runtime_root != expected_runtime_root
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
        runtime_root: record.runtime_root,
        bundle_dir: record.bundle_dir,
        init_pid: record.init_pid,
        log_worker_pid: record.log_worker_pid,
        log_worker_pid_start_time: record.log_worker_pid_start_time,
    }))
}

#[cfg(target_os = "linux")]
enum SandboxReap {
    NotPresent,
    Cleaned,
    Failed,
}

/// Reconcile a durable `crun` record before touching its rootfs. All paths and
/// the runtime artifact are revalidated; persisted PIDs are diagnostic only
/// and are never signalled directly because PID reuse would make that unsafe.
#[cfg(target_os = "linux")]
fn reap_orphaned_crun(home_dir: &Path, box_dir: &Path, box_id: &str) -> SandboxReap {
    use std::process::Command;

    let record_path = box_dir.join("sandbox/runtime.json");
    let record = match load_recorded_sandbox_runtime(home_dir, box_dir, box_id) {
        Ok(Some(record)) => record,
        Ok(None) => return SandboxReap::NotPresent,
        Err(error) => {
            tracing::error!(box_id, %error, "Invalid Sandbox runtime record during crash recovery");
            return SandboxReap::Failed;
        }
    };
    let state = match crate::sandbox::handler::CrunHandler::query_state_at(
        &record.runtime_path,
        &record.runtime_root,
        box_id,
    ) {
        Ok(state) => state,
        Err(error) => {
            tracing::error!(box_id, %error, "Failed to query orphaned Sandbox state");
            return SandboxReap::Failed;
        }
    };
    if state.is_some_and(|state| state.status != "stopped") {
        let output = Command::new(&record.runtime_path)
            .arg("--root")
            .arg(&record.runtime_root)
            .arg("kill")
            .arg(box_id)
            .arg(libc::SIGKILL.to_string())
            .env("LC_ALL", "C")
            .output();
        if let Err(error) = output {
            tracing::error!(box_id, %error, "Failed to signal orphaned Sandbox");
            return SandboxReap::Failed;
        }
    }

    let output = Command::new(&record.runtime_path)
        .arg("--root")
        .arg(&record.runtime_root)
        .arg("delete")
        .arg("--force")
        .arg(box_id)
        .env("LC_ALL", "C")
        .output();
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            match crate::sandbox::handler::CrunHandler::query_state_at(
                &record.runtime_path,
                &record.runtime_root,
                box_id,
            ) {
                Ok(None) => {}
                _ => {
                    tracing::error!(
                        box_id,
                        stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                        "Failed to delete orphaned Sandbox runtime state"
                    );
                    return SandboxReap::Failed;
                }
            }
        }
        Err(error) => {
            tracing::error!(box_id, %error, "Failed to start Sandbox cleanup command");
            return SandboxReap::Failed;
        }
    }

    drain_recorded_log_worker(&record, box_id);
    let _ = std::fs::remove_dir_all(&record.bundle_dir);
    let _ = std::fs::remove_dir_all(&record.runtime_root);
    let _ = std::fs::remove_file(&record_path);
    tracing::info!(box_id, "Reaped orphaned crun Sandbox after runtime restart");
    SandboxReap::Cleaned
}

#[cfg(target_os = "linux")]
fn drain_recorded_log_worker(record: &RecordedSandboxRuntime, box_id: &str) {
    let (Some(pid), Some(_start_time)) = (record.log_worker_pid, record.log_worker_pid_start_time)
    else {
        return;
    };
    if wait_for_log_worker_identity(record, std::time::Duration::from_secs(2)) {
        return;
    }

    tracing::warn!(
        box_id,
        log_worker_pid = pid,
        "Recovered Sandbox log worker did not drain after crun cleanup; terminating it"
    );
    if let Ok(pid) = i32::try_from(pid) {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
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
mod tests {
    use super::*;

    fn write_runtime_record(
        home_dir: &Path,
        box_dir: &Path,
        box_id: &str,
        mutate: impl FnOnce(&mut SandboxRuntimeRecord),
    ) {
        let mut record = SandboxRuntimeRecord {
            schema: "a3s.box.sandbox-runtime.v1".to_string(),
            container_id: box_id.to_string(),
            runtime_path: Path::new("/definitely/missing/certified-crun").to_path_buf(),
            runtime_root: home_dir.join("run/crun").join(box_id),
            bundle_dir: box_dir.join("sandbox/bundle"),
            init_pid: 42,
            log_worker_pid: None,
            log_worker_pid_start_time: None,
        };
        mutate(&mut record);
        std::fs::create_dir_all(box_dir.join("sandbox")).unwrap();
        std::fs::write(
            box_dir.join("sandbox/runtime.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn test_reap_removes_box_dir() {
        // A box dir with no live shim / mount (e.g. left by a crash) is removed.
        let home = tempfile::tempdir().unwrap();
        let box_id = "reap-test-no-such-shim-uuid";
        let box_dir = home.path().join("boxes").join(box_id);
        std::fs::create_dir_all(box_dir.join("logs")).unwrap();
        std::fs::write(box_dir.join("logs/shim.stdout.log"), b"x").unwrap();
        assert!(box_dir.exists());

        reap_orphaned_box_in(home.path(), box_id);
        assert!(!box_dir.exists(), "orphaned box dir should be removed");
    }

    #[test]
    fn test_reap_absent_box_is_noop() {
        let home = tempfile::tempdir().unwrap();
        // No boxes/<id> dir at all — must not panic or error.
        reap_orphaned_box_in(home.path(), "absent-box-uuid");
    }

    #[test]
    fn cleanup_absent_sandbox_runtime_preserves_box_directory() {
        let home = tempfile::tempdir().unwrap();
        let box_id = "cleanup-test-no-runtime-record";
        let box_dir = home.path().join("boxes").join(box_id);
        std::fs::create_dir_all(&box_dir).unwrap();

        cleanup_recorded_sandbox_runtime_in(home.path(), &box_dir, box_id).unwrap();

        assert!(box_dir.exists());
    }

    #[test]
    fn recorded_sandbox_runtime_rejects_an_unexpected_box_directory() {
        let home = tempfile::tempdir().unwrap();
        let box_id = "recorded-sandbox-unexpected-directory";
        let box_dir = home.path().join("external").join(box_id);
        write_runtime_record(home.path(), &box_dir, box_id, |_| {});

        let error = load_recorded_sandbox_runtime(home.path(), &box_dir, box_id).unwrap_err();

        assert!(error.to_string().contains("unexpected host directory"));
    }

    #[test]
    fn recorded_sandbox_runtime_rejects_invalid_paths_before_certification() {
        let home = tempfile::tempdir().unwrap();
        let box_id = "recorded-sandbox-invalid-paths";
        let box_dir = home.path().join("boxes").join(box_id);
        write_runtime_record(home.path(), &box_dir, box_id, |record| {
            record.runtime_root = home.path().join("run/crun/another-box");
        });

        let error = load_recorded_sandbox_runtime(home.path(), &box_dir, box_id).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("path or identity validation"));
        assert!(!message.contains("Cannot verify the recorded Sandbox runtime"));
    }

    #[test]
    fn log_drain_wait_validates_identity_without_recertifying_crun() {
        let home = tempfile::tempdir().unwrap();
        let box_id = "recorded-sandbox-log-drain";
        let box_dir = home.path().join("boxes").join(box_id);
        write_runtime_record(home.path(), &box_dir, box_id, |_| {});

        assert!(wait_for_recorded_sandbox_log_drain_in(
            home.path(),
            &box_dir,
            box_id,
            std::time::Duration::ZERO,
        )
        .unwrap());

        let error = load_recorded_sandbox_runtime(home.path(), &box_dir, box_id).unwrap_err();
        assert!(error
            .to_string()
            .contains("Cannot verify the recorded Sandbox runtime"));
    }
}
