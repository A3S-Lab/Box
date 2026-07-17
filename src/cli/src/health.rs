//! Health check executor for running containers.
//!
//! Runs user-defined health checks through the exec socket and updates box
//! state. Foreground commands use a Tokio task; detached boxes use a
//! generation-fenced child process so scheduling survives the creating CLI.
//!
//! Follows Docker health check semantics:
//! - Wait `start_period_secs` before the first check
//! - Run every `interval_secs`; timeout each run at `timeout_secs`
//! - Exit code 0 → healthy; non-zero → failure
//! - After `retries` consecutive failures → status becomes "unhealthy"
//! - Socket disappearing → box has stopped; checker exits

#[cfg(not(windows))]
use std::path::Path;
use std::path::PathBuf;

use crate::state::BoxRecord;
use crate::state::HealthCheck;
#[cfg(not(windows))]
use crate::state::StateFile;

/// Spawn a background health checker task for a running box.
///
/// Returns a `JoinHandle` that the caller can abort when the box stops.
/// Foreground callers abort the handle during cleanup. Detached callers must
/// use [`spawn_detached_health_checker`] instead.
pub fn spawn_health_checker(
    box_id: String,
    exec_socket_path: PathBuf,
    health_check: HealthCheck,
) -> tokio::task::JoinHandle<()> {
    #[cfg(not(windows))]
    {
        tokio::spawn(async move {
            run_health_loop(box_id, exec_socket_path, health_check, None).await;
        })
    }
    #[cfg(windows)]
    {
        // Health checks require exec socket (Unix domain sockets); no-op on Windows.
        let _ = (box_id, exec_socket_path, health_check);
        tokio::spawn(async {})
    }
}

/// Start a process-owned health checker for a detached box.
///
/// A Tokio task owned by `run -d`, `compose up`, or `start` disappears when that
/// short-lived CLI exits. The child process uses a generation-specific lock so
/// duplicate launch attempts collapse to one worker, while a restarted box can
/// immediately acquire a new generation lock.
#[cfg(not(windows))]
pub(crate) fn spawn_detached_health_checker(record: &BoxRecord) -> Result<(), String> {
    if record.health_check.is_none() {
        return Ok(());
    }
    let generation = health_generation(record)
        .ok_or_else(|| format!("box '{}' has no health-check generation", record.name))?;
    let executable = std::env::current_exe()
        .map_err(|error| format!("failed to locate a3s-box for health checker: {error}"))?;
    let arguments = detached_health_worker_args(&record.id, generation);

    std::process::Command::new(executable)
        .args(arguments)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| {
            format!(
                "failed to start detached health checker for '{}': {error}",
                record.name
            )
        })
}

#[cfg(any(not(windows), test))]
fn detached_health_worker_args(box_id: &str, generation: i64) -> Vec<String> {
    vec![
        "monitor".to_string(),
        "--health-worker".to_string(),
        box_id.to_string(),
        "--health-generation".to_string(),
        generation.to_string(),
    ]
}

#[cfg(windows)]
pub(crate) fn spawn_detached_health_checker(_record: &BoxRecord) -> Result<(), String> {
    Ok(())
}

/// Run the hidden process-owned health worker for one box generation.
#[cfg(not(windows))]
pub(crate) async fn run_detached_health_worker(
    box_id: String,
    generation: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(_lock) = HealthWorkerLock::try_acquire(&box_id, generation)? else {
        return Ok(());
    };

    let state = StateFile::load_default()?;
    let Some(record) = state.find_by_id(&box_id) else {
        return Ok(());
    };
    if health_generation(record) != Some(generation) || record.status != "running" {
        return Ok(());
    }
    let Some(health_check) = record.health_check.clone() else {
        return Ok(());
    };

    run_health_loop(
        box_id,
        record.exec_socket_path.clone(),
        health_check,
        Some(generation),
    )
    .await;
    Ok(())
}

#[cfg(windows)]
pub(crate) async fn run_detached_health_worker(
    _box_id: String,
    _generation: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    Ok(())
}

#[cfg(not(windows))]
struct HealthWorkerLock {
    _file: std::fs::File,
}

#[cfg(not(windows))]
impl HealthWorkerLock {
    fn try_acquire(box_id: &str, generation: i64) -> std::io::Result<Option<Self>> {
        Self::try_acquire_path(&health_worker_lock_path(box_id, generation))
    }

    fn try_acquire_path(path: &Path) -> std::io::Result<Option<Self>> {
        use std::os::fd::AsRawFd;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(Some(Self { _file: file }));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            Ok(None)
        } else {
            Err(error)
        }
    }
}

#[cfg(not(windows))]
fn health_worker_lock_path(box_id: &str, generation: i64) -> PathBuf {
    a3s_box_core::dirs_home()
        .join("locks")
        .join(format!("{box_id}.{generation}.health.lock"))
}

#[cfg(any(not(windows), test))]
fn health_generation(record: &BoxRecord) -> Option<i64> {
    record
        .started_at
        .and_then(|started_at| started_at.timestamp_nanos_opt())
}

#[cfg(not(windows))]
pub(crate) fn detached_health_worker_active(record: &BoxRecord) -> bool {
    let Some(generation) = health_generation(record) else {
        return false;
    };
    match HealthWorkerLock::try_acquire(&record.id, generation) {
        Ok(Some(lock)) => {
            drop(lock);
            false
        }
        Ok(None) => true,
        Err(_) => false,
    }
}

#[cfg(not(windows))]
async fn run_health_loop(
    box_id: String,
    exec_socket_path: PathBuf,
    hc: HealthCheck,
    expected_generation: Option<i64>,
) {
    use std::time::Duration;

    // Honour start_period before the first probe
    if hc.start_period_secs > 0 {
        tokio::time::sleep(Duration::from_secs(hc.start_period_secs)).await;
    }

    let interval = Duration::from_secs(hc.interval_secs.max(1));
    let timeout_ns = probe_timeout_ns(&hc);

    loop {
        tokio::time::sleep(interval).await;

        if !health_worker_is_current(&box_id, expected_generation) {
            break;
        }

        let healthy = run_probe(&exec_socket_path, &hc.cmd, timeout_ns).await;

        // Reload fresh under the state lock and apply ONLY this box's health
        // fields, so concurrent monitor/CLI writers are not clobbered.
        let keep_going = StateFile::modify(|state| {
            let Some(record) = state.find_by_id_mut(&box_id) else {
                return Ok::<bool, std::io::Error>(false); // box removed
            };
            if record.status != "running" {
                return Ok(false); // box stopped
            }
            if expected_generation.is_some() && health_generation(record) != expected_generation {
                return Ok(false); // box restarted; a new generation owns probes
            }
            apply_probe_result(record, healthy, chrono::Utc::now());
            Ok(true)
        });
        match keep_going {
            Ok(true) => {}
            Ok(false) => break,
            Err(_) => continue,
        }
    }
}

#[cfg(not(windows))]
fn health_worker_is_current(box_id: &str, expected_generation: Option<i64>) -> bool {
    let Ok(state) = StateFile::load_default() else {
        return true;
    };
    state.find_by_id(box_id).is_some_and(|record| {
        record.status == "running"
            && expected_generation
                .map(|generation| health_generation(record) == Some(generation))
                .unwrap_or(true)
    })
}

#[cfg(not(windows))]
pub(crate) async fn run_probe(
    exec_socket_path: &std::path::Path,
    cmd: &[String],
    timeout_ns: u64,
) -> bool {
    use a3s_box_core::exec::ExecRequest;
    use a3s_box_runtime::ExecClient;

    let client = match ExecClient::connect(exec_socket_path).await {
        Ok(c) => c,
        Err(_) => return false,
    };

    let request = ExecRequest {
        request_id: None,
        cmd: cmd.to_vec(),
        timeout_ns,
        env: vec![],
        working_dir: None,
        rootfs: None,
        stdin: None,
        stdin_streaming: false,
        user: None,
        streaming: false,
    };

    match client.exec_command(&request).await {
        Ok(output) => output.exit_code == 0,
        Err(_) => false,
    }
}

#[cfg(any(not(windows), test))]
pub(crate) fn probe_timeout_ns(hc: &HealthCheck) -> u64 {
    hc.timeout_secs.saturating_mul(1_000_000_000)
}

#[cfg(any(not(windows), test))]
pub(crate) fn should_probe(record: &BoxRecord, now: chrono::DateTime<chrono::Utc>) -> bool {
    let Some(hc) = record.health_check.as_ref() else {
        return false;
    };
    if record.status != "running" {
        return false;
    }

    if let Some(started_at) = record.started_at {
        let start_period = bounded_chrono_seconds(hc.start_period_secs);
        if now < started_at + start_period {
            return false;
        }
    }

    let Some(last_check) = record.health_last_check else {
        return true;
    };

    now >= last_check + bounded_chrono_seconds(hc.interval_secs.max(1))
}

#[cfg(any(not(windows), test))]
pub(crate) fn apply_probe_result(
    record: &mut BoxRecord,
    healthy: bool,
    checked_at: chrono::DateTime<chrono::Utc>,
) {
    if record.status != "running" {
        return;
    }

    if healthy {
        record.health_status = "healthy".to_string();
        record.health_retries = 0;
    } else {
        record.health_retries = record.health_retries.saturating_add(1);
        if let Some(hc) = record.health_check.as_ref() {
            if record.health_retries >= hc.retries {
                record.health_status = "unhealthy".to_string();
            }
        }
    }
    record.health_last_check = Some(checked_at);
}

#[cfg(any(not(windows), test))]
fn bounded_chrono_seconds(seconds: u64) -> chrono::Duration {
    chrono::Duration::seconds(seconds.min(i64::MAX as u64) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health_check_interval_floor() {
        // Ensure interval_secs of 0 doesn't cause busy-loop (max(1) guard)
        let hc = HealthCheck {
            cmd: vec!["true".to_string()],
            interval_secs: 0,
            timeout_secs: 5,
            retries: 3,
            start_period_secs: 0,
        };
        let interval = std::time::Duration::from_secs(hc.interval_secs.max(1));
        assert_eq!(interval, std::time::Duration::from_secs(1));
    }

    #[test]
    fn test_timeout_ns_overflow_safe() {
        // Large timeout_secs must not overflow u64
        let hc = HealthCheck {
            cmd: vec!["true".to_string()],
            interval_secs: 30,
            timeout_secs: 5,
            retries: 3,
            start_period_secs: 0,
        };
        assert_eq!(probe_timeout_ns(&hc), 5_000_000_000);

        let big_hc = HealthCheck {
            timeout_secs: u64::MAX,
            ..hc
        };
        assert_eq!(probe_timeout_ns(&big_hc), u64::MAX); // saturates instead of overflowing
    }

    #[test]
    fn test_should_probe_respects_start_period() {
        let now = chrono::Utc::now();
        let mut record =
            crate::test_helpers::fixtures::make_record("health-id", "health", "running", Some(1));
        record.started_at = Some(now);
        record.health_check = Some(HealthCheck {
            cmd: vec!["true".to_string()],
            interval_secs: 30,
            timeout_secs: 5,
            retries: 3,
            start_period_secs: 10,
        });

        assert!(!should_probe(&record, now + chrono::Duration::seconds(9)));
        assert!(should_probe(&record, now + chrono::Duration::seconds(10)));
    }

    #[test]
    fn test_should_probe_respects_interval() {
        let now = chrono::Utc::now();
        let mut record =
            crate::test_helpers::fixtures::make_record("health-id", "health", "running", Some(1));
        record.started_at = Some(now - chrono::Duration::seconds(60));
        record.health_last_check = Some(now);
        record.health_check = Some(HealthCheck {
            cmd: vec!["true".to_string()],
            interval_secs: 30,
            timeout_secs: 5,
            retries: 3,
            start_period_secs: 0,
        });

        assert!(!should_probe(&record, now + chrono::Duration::seconds(29)));
        assert!(should_probe(&record, now + chrono::Duration::seconds(30)));
    }

    #[test]
    fn test_apply_probe_result_tracks_retries_and_recovery() {
        let now = chrono::Utc::now();
        let mut record =
            crate::test_helpers::fixtures::make_record("health-id", "health", "running", Some(1));
        record.health_status = "starting".to_string();
        record.health_check = Some(HealthCheck {
            cmd: vec!["false".to_string()],
            interval_secs: 30,
            timeout_secs: 5,
            retries: 2,
            start_period_secs: 0,
        });

        apply_probe_result(&mut record, false, now);
        assert_eq!(record.health_status, "starting");
        assert_eq!(record.health_retries, 1);

        apply_probe_result(&mut record, false, now);
        assert_eq!(record.health_status, "unhealthy");
        assert_eq!(record.health_retries, 2);

        apply_probe_result(&mut record, true, now);
        assert_eq!(record.health_status, "healthy");
        assert_eq!(record.health_retries, 0);
    }

    #[test]
    fn test_apply_probe_result_ignores_stopped_records() {
        let now = chrono::Utc::now();
        let mut record =
            crate::test_helpers::fixtures::make_record("health-id", "health", "stopped", None);
        record.health_check = Some(HealthCheck {
            cmd: vec!["true".to_string()],
            interval_secs: 30,
            timeout_secs: 5,
            retries: 1,
            start_period_secs: 0,
        });

        apply_probe_result(&mut record, true, now);
        assert_eq!(record.health_status, "none");
        assert!(record.health_last_check.is_none());
    }

    #[test]
    fn test_detached_worker_args_bind_box_generation() {
        assert_eq!(
            detached_health_worker_args("box-id", 1234),
            vec![
                "monitor",
                "--health-worker",
                "box-id",
                "--health-generation",
                "1234",
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_health_worker_lock_allows_one_owner_per_generation() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("worker.lock");

        let first = HealthWorkerLock::try_acquire_path(&path)
            .unwrap()
            .expect("first worker should own the generation");
        assert!(HealthWorkerLock::try_acquire_path(&path).unwrap().is_none());
        drop(first);
        assert!(HealthWorkerLock::try_acquire_path(&path).unwrap().is_some());
    }
}
