//! Health check executor for running containers.
//!
//! Spawns a background task that periodically runs the user-defined health
//! check command via the exec socket and updates the box state accordingly.
//!
//! Follows Docker health check semantics:
//! - Wait `start_period_secs` before the first check
//! - Run every `interval_secs`; timeout each run at `timeout_secs`
//! - Exit code 0 → healthy; non-zero → failure
//! - After `retries` consecutive failures → status becomes "unhealthy"
//! - Socket disappearing → box has stopped; checker exits

use std::path::PathBuf;

use crate::state::HealthCheck;
#[cfg(not(windows))]
use crate::state::StateFile;

/// Spawn a background health checker task for a running box.
///
/// Returns a `JoinHandle` that the caller can abort when the box stops.
/// In detached/daemon scenarios the handle may be dropped; the task will
/// self-terminate once the exec socket disappears.
pub fn spawn_health_checker(
    box_id: String,
    exec_socket_path: PathBuf,
    health_check: HealthCheck,
) -> tokio::task::JoinHandle<()> {
    #[cfg(not(windows))]
    {
        tokio::spawn(async move {
            run_health_loop(box_id, exec_socket_path, health_check).await;
        })
    }
    #[cfg(windows)]
    {
        // Health checks require exec socket (Unix domain sockets); no-op on Windows.
        let _ = (box_id, exec_socket_path, health_check);
        tokio::spawn(async {})
    }
}

#[cfg(not(windows))]
async fn run_health_loop(box_id: String, exec_socket_path: PathBuf, hc: HealthCheck) {
    use std::time::Duration;

    // Set initial status to "starting" during start_period
    if hc.start_period_secs > 0 {
        if let Ok(mut state) = StateFile::load_default() {
            if let Some(record) = state.find_by_id_mut(&box_id) {
                record.health_status = "starting".to_string();
                let _ = state.save();
            }
        }
        tokio::time::sleep(Duration::from_secs(hc.start_period_secs)).await;
    }

    let interval = Duration::from_secs(hc.interval_secs.max(1));
    let timeout_ns = hc.timeout_secs.saturating_mul(1_000_000_000);
    let mut consecutive_failures = 0u32;
    let mut was_unhealthy = false;

    loop {
        tokio::time::sleep(interval).await;

        // Box stopped — exec socket is gone
        if !exec_socket_path.exists() {
            break;
        }

        let healthy = run_probe(&exec_socket_path, &hc.cmd, timeout_ns).await;

        let Ok(mut state) = StateFile::load_default() else {
            continue;
        };
        let Some(record) = state.find_by_id_mut(&box_id) else {
            break; // Box removed from state
        };

        let previous_status = record.health_status.clone();

        if healthy {
            record.health_status = "healthy".to_string();
            consecutive_failures = 0;
            record.health_retries = 0;

            // Log recovery from unhealthy state
            if was_unhealthy {
                tracing::info!(
                    box_id = %box_id,
                    box_name = %record.name,
                    "Container recovered from unhealthy state"
                );
                was_unhealthy = false;
            }
        } else {
            consecutive_failures += 1;
            record.health_retries = consecutive_failures;

            if consecutive_failures >= hc.retries {
                let newly_unhealthy = record.health_status != "unhealthy";
                record.health_status = "unhealthy".to_string();
                was_unhealthy = true;

                if newly_unhealthy {
                    tracing::warn!(
                        box_id = %box_id,
                        box_name = %record.name,
                        consecutive_failures = consecutive_failures,
                        "Container marked as unhealthy after {} consecutive failures",
                        consecutive_failures
                    );

                    // Check if we should restart the container based on restart policy
                    if should_restart_on_unhealthy(&record.restart_policy) {
                        tracing::info!(
                            box_id = %box_id,
                            box_name = %record.name,
                            restart_policy = %record.restart_policy,
                            "Triggering container restart due to unhealthy status"
                        );

                        // Notify the monitor to restart the container
                        // The monitor will handle the actual restart logic
                        crate::monitor_global::notify_container_stopped(&box_id).await;
                    }
                }
            } else {
                tracing::debug!(
                    box_id = %box_id,
                    box_name = %record.name,
                    consecutive_failures = consecutive_failures,
                    retries_threshold = hc.retries,
                    "Health check failed ({}/{})",
                    consecutive_failures,
                    hc.retries
                );
            }
        }

        record.health_last_check = Some(chrono::Utc::now());

        // Log status transitions
        if previous_status != record.health_status {
            tracing::info!(
                box_id = %box_id,
                box_name = %record.name,
                previous_status = %previous_status,
                new_status = %record.health_status,
                "Health status changed: {} → {}",
                previous_status,
                record.health_status
            );
        }

        let _ = state.save();
    }
}

/// Check if the container should be restarted when it becomes unhealthy.
///
/// Containers with "always" or "unless-stopped" restart policies should be
/// restarted when they become unhealthy.
fn should_restart_on_unhealthy(restart_policy: &str) -> bool {
    matches!(restart_policy, "always" | "unless-stopped")
}

#[cfg(not(windows))]
async fn run_probe(exec_socket_path: &std::path::Path, cmd: &[String], timeout_ns: u64) -> bool {
    use a3s_box_core::exec::ExecRequest;
    use a3s_box_runtime::ExecClient;

    let client = match ExecClient::connect(exec_socket_path).await {
        Ok(c) => c,
        Err(_) => return false,
    };

    let request = ExecRequest {
        cmd: cmd.to_vec(),
        timeout_ns,
        env: vec![],
        working_dir: None,
        stdin: None,
        user: None,
        streaming: false,
    };

    match client.exec_command(&request).await {
        Ok(output) => output.exit_code == 0,
        Err(_) => false,
    }
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
        let timeout_ns = 5u64.saturating_mul(1_000_000_000);
        assert_eq!(timeout_ns, 5_000_000_000);

        let big_timeout_ns = u64::MAX.saturating_mul(1_000_000_000);
        assert_eq!(big_timeout_ns, u64::MAX); // saturates instead of overflowing
    }
}
