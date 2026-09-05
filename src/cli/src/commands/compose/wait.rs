//! Compose dependency readiness and completion waits.

use super::*;

/// Bound the delay between a dependency becoming healthy and the next Compose
/// convergence check. The health worker still controls probe cadence; this only
/// avoids adding a multi-second scheduling gap to dependent service startup.
const HEALTH_WAIT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

pub(super) async fn wait_for_healthy(
    project_name: &str,
    service_names: &[String],
    timeout_secs: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

    loop {
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "Timed out waiting for services to become healthy: {}",
                service_names.join(", ")
            )
            .into());
        }

        let state = StateFile::load_default()?;
        let all_healthy = service_names.iter().all(|svc_name| {
            // Find the box for this service by label
            state
                .find_by_label(LABEL_SERVICE, svc_name)
                .iter()
                .any(|r| {
                    r.labels.get(LABEL_PROJECT).map(String::as_str) == Some(project_name)
                        && r.health_status == "healthy"
                })
        });

        if all_healthy {
            return Ok(());
        }

        tokio::time::sleep(HEALTH_WAIT_POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::HEALTH_WAIT_POLL_INTERVAL;

    #[test]
    fn health_wait_poll_interval_is_subsecond() {
        assert_eq!(
            HEALTH_WAIT_POLL_INTERVAL,
            std::time::Duration::from_millis(500)
        );
    }
}

pub(super) fn validate_compose_up_platform_support() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(windows)]
    {
        Err(crate::platform::unsupported_command(
            "compose up",
            "bridge networking support",
        ))
    }
    #[cfg(not(windows))]
    {
        Ok(())
    }
}

/// Wait for dependency services to run to completion (Docker's
/// `service_completed_successfully`).
///
/// A dependency is "completed" once it is no longer active — preferring the
/// record's terminal status (set by the monitor) and falling back to shim-PID
/// liveness for the daemonless case. Completion is never inferred without an
/// authoritative exit code; zero satisfies the dependency and non-zero fails it.
pub(super) async fn wait_for_completed(
    project_name: &str,
    service_names: &[String],
    timeout_secs: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

    loop {
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "Timed out waiting for services to complete: {}",
                service_names.join(", ")
            )
            .into());
        }

        let state = StateFile::load_default()?;
        let mut all_done = true;
        for svc_name in service_names {
            let records = state.find_by_label(LABEL_SERVICE, svc_name);
            let Some(record) = records
                .iter()
                .find(|r| r.labels.get(LABEL_PROJECT).map(String::as_str) == Some(project_name))
            else {
                all_done = false;
                continue;
            };

            // A detached box's shim becomes a zombie under this process when its
            // VM halts; is_process_exited is zombie-aware (is_process_alive /
            // kill(pid,0) is not), so a completed dependency is detected.
            let exited = !status::is_active(record)
                || record
                    .pid
                    .map(crate::process::is_process_exited)
                    .unwrap_or(true);
            if !exited {
                all_done = false;
                continue;
            }

            // A just-reaped shim may not have been reconciled into the state
            // file yet. guest-init persists the authoritative container code
            // before halting the VM, so read it directly instead of treating an
            // unknown code as success.
            let exit_code = a3s_box_runtime::rootfs::resolve_workload_exit_code(
                &record.box_dir,
                record.exit_code,
            );
            let Some(code) = exit_code else {
                all_done = false;
                continue;
            };
            if code != 0 {
                return Err(format!(
                    "dependency service '{}' did not complete successfully (exit code {})",
                    svc_name, code
                )
                .into());
            }
        }

        if all_done {
            return Ok(());
        }

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}
