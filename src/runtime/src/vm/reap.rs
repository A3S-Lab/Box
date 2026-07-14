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

#[cfg(target_os = "linux")]
fn cleanup_recorded_sandbox_runtime_in(
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
#[derive(Debug, serde::Deserialize)]
struct SandboxRuntimeRecord {
    schema: String,
    container_id: String,
    runtime_path: std::path::PathBuf,
    runtime_root: std::path::PathBuf,
    bundle_dir: std::path::PathBuf,
    init_pid: u32,
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
    let bytes = match std::fs::read(&record_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return SandboxReap::NotPresent;
        }
        Err(error) => {
            tracing::error!(
                box_id,
                path = %record_path.display(),
                %error,
                "Failed to read Sandbox runtime record during crash recovery"
            );
            return SandboxReap::Failed;
        }
    };
    let record: SandboxRuntimeRecord = match serde_json::from_slice(&bytes) {
        Ok(record) => record,
        Err(error) => {
            tracing::error!(
                box_id,
                path = %record_path.display(),
                %error,
                "Invalid Sandbox runtime record during crash recovery"
            );
            return SandboxReap::Failed;
        }
    };
    let expected_runtime_root = home_dir.join("run/crun").join(box_id);
    let expected_bundle = box_dir.join("sandbox/bundle");
    if record.schema != "a3s.box.sandbox-runtime.v1"
        || record.container_id != box_id
        || record.runtime_root != expected_runtime_root
        || record.bundle_dir != expected_bundle
        || record.init_pid == 0
    {
        tracing::error!(
            box_id,
            "Sandbox runtime record failed path or identity validation"
        );
        return SandboxReap::Failed;
    }

    let capabilities = crate::sandbox::probe_sandbox_capabilities(Some(&record.runtime_path));
    let Some(runtime) = capabilities.runtime else {
        tracing::error!(
            box_id,
            failures = ?capabilities.failures,
            "Cannot verify the recorded Sandbox runtime during crash recovery"
        );
        return SandboxReap::Failed;
    };
    let state = match crate::sandbox::handler::CrunHandler::query_state_at(
        &runtime.path,
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
        let output = Command::new(&runtime.path)
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

    let output = Command::new(&runtime.path)
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
                &runtime.path,
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

    let _ = std::fs::remove_dir_all(&record.bundle_dir);
    let _ = std::fs::remove_dir_all(&record.runtime_root);
    let _ = std::fs::remove_file(&record_path);
    tracing::info!(box_id, "Reaped orphaned crun Sandbox after runtime restart");
    SandboxReap::Cleaned
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
}
