//! `a3s-box container-update` command — Update resource limits on a running box.
//!
//! Similar to `docker update`, allows changing cgroup-based limits on a running
//! box without restarting it. Changes are applied live via the exec channel and
//! persisted to the state file.
//!
//! Tier 1 limits (--cpus, --memory) cannot be changed on a running microVM
//! because libkrun does not expose a hot-resize API. These are rejected with
//! a clear error message.

use clap::Args;

#[cfg(not(windows))]
use a3s_box_core::exec::ExecRequest;
use a3s_box_core::ExecutionRestartPolicy;
use a3s_box_runtime::resize::{validate_update, validate_update_values, ResourceUpdate};
#[cfg(not(windows))]
use a3s_box_runtime::ExecClient;

use super::common;
use crate::output::parse_memory;
use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct ContainerUpdateArgs {
    /// Box name or ID
    pub name: String,

    /// Number of CPUs (requires restart — cannot hot-resize)
    #[arg(long)]
    pub cpus: Option<u32>,

    /// Memory limit (requires restart — cannot hot-resize)
    #[arg(long)]
    pub memory: Option<String>,

    /// Memory reservation/soft limit (e.g., "256m", "1g")
    #[arg(long)]
    pub memory_reservation: Option<String>,

    /// Memory+swap limit (e.g., "1g", "-1" for unlimited)
    #[arg(long)]
    pub memory_swap: Option<String>,

    /// Limit PIDs inside the box
    #[arg(long)]
    pub pids_limit: Option<u64>,

    /// CPU shares (relative weight, 2-262144)
    #[arg(long)]
    pub cpu_shares: Option<u64>,

    /// CPU quota in microseconds per cpu-period
    #[arg(long)]
    pub cpu_quota: Option<i64>,

    /// CPU period in microseconds
    #[arg(long)]
    pub cpu_period: Option<u64>,

    /// Pin to specific CPUs (e.g., "0,1,3" or "0-3")
    #[arg(long)]
    pub cpuset_cpus: Option<String>,

    /// Restart policy: no, always, on-failure, unless-stopped
    #[arg(long)]
    pub restart: Option<String>,
}

pub async fn execute(args: ContainerUpdateArgs) -> Result<(), Box<dyn std::error::Error>> {
    let initial_state = StateFile::load_default()?;
    let box_id = resolve::resolve(&initial_state, &args.name)?.id.clone();
    drop(initial_state);
    let _lifecycle_lock = crate::lifecycle::acquire_box_lifecycle_lock(&box_id).await?;
    // Build the update from a fresh record only after waiting for any
    // start/restart/commit operation. Keep the lock across validation, live
    // guest application, and persistence so a boot cannot consume old limits
    // while the durable record is switched to new limits.
    let mut state = StateFile::load_default()?;
    let record = state
        .find_by_id_mut(&box_id)
        .ok_or_else(|| format!("Box {} was removed while waiting to update", args.name))?;

    let name = record.name.clone();
    let requires_live_apply = record.is_active();
    let live_apply_baseline = record.clone();
    let mut updated = Vec::new();

    // Build ResourceUpdate for live application
    let mut update = ResourceUpdate::default();

    // Tier 1: vCPU and memory — reject if box is running
    if let Some(cpus) = args.cpus {
        update.vcpus = Some(cpus);
        record.cpus = cpus;
        updated.push(format!("cpus={cpus}"));
    }

    if let Some(ref mem_str) = args.memory {
        let mb = parse_memory(mem_str).map_err(|e| format!("Invalid --memory: {e}"))?;
        update.memory_mb = Some(mb);
        record.memory_mb = mb;
        updated.push(format!("memory={mem_str}"));
    }

    // Tier 2: cgroup-based limits — can be applied live
    if let Some(ref reservation) = args.memory_reservation {
        let bytes = common::parse_memory_bytes(reservation)
            .map_err(|e| format!("Invalid --memory-reservation: {e}"))?;
        update.limits.memory_reservation = Some(bytes);
        record.resource_limits.memory_reservation = Some(bytes);
        updated.push(format!("memory-reservation={reservation}"));
    }

    if let Some(ref swap) = args.memory_swap {
        // Same fail-closed parse as the run/create path so `update --memory-swap`
        // can't silently grant unlimited swap on an overflowing value.
        let val = common::parse_memory_swap(swap)?;
        update.limits.memory_swap = Some(val);
        record.resource_limits.memory_swap = Some(val);
        updated.push(format!("memory-swap={swap}"));
    }

    if let Some(pids) = args.pids_limit {
        update.limits.pids_limit = Some(pids);
        record.resource_limits.pids_limit = Some(pids);
        updated.push(format!("pids-limit={pids}"));
    }

    if let Some(shares) = args.cpu_shares {
        update.limits.cpu_shares = Some(shares);
        record.resource_limits.cpu_shares = Some(shares);
        updated.push(format!("cpu-shares={shares}"));
    }

    if let Some(quota) = args.cpu_quota {
        update.limits.cpu_quota = Some(quota);
        record.resource_limits.cpu_quota = Some(quota);
        updated.push(format!("cpu-quota={quota}"));
    }

    if let Some(period) = args.cpu_period {
        update.limits.cpu_period = Some(period);
        record.resource_limits.cpu_period = Some(period);
        updated.push(format!("cpu-period={period}"));
    }

    if let Some(ref cpuset) = args.cpuset_cpus {
        update.limits.cpuset_cpus = Some(cpuset.clone());
        record.resource_limits.cpuset_cpus = Some(cpuset.clone());
        updated.push(format!("cpuset-cpus={cpuset}"));
    }

    if let Some(ref restart) = args.restart {
        let (policy, max_count) = crate::state::parse_restart_policy(restart)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        record.restart_policy = policy;
        record.max_restart_count = max_count;
        updated.push(format!("restart={restart}"));
    }

    if updated.is_empty() {
        println!("No updates specified.");
        return Ok(());
    }

    validate_running_update(requires_live_apply, &update)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    // Snapshot this box's owned fields BEFORE the (awaiting) live-apply below.
    // The final persist re-applies them via StateFile::modify (load-fresh under
    // the lock) instead of saving the full records vector loaded above — which,
    // across the exec awaits, would clobber a concurrent writer (e.g. the
    // monitor updating another box, or this box's status/pid/restart_count).
    let new_restart_policy = record.restart_policy.clone();
    let new_max_restart = record.max_restart_count;
    let restart_policy_updated = args.restart.is_some();
    #[cfg(not(windows))]
    let mut live_tier2_applied = false;
    #[cfg(windows)]
    let live_tier2_applied = false;

    // If the box is running, apply the already-validated live changes.
    if requires_live_apply {
        // Tier 2 changes use the guest exec channel on supported hosts.
        if update.has_tier2_changes() {
            #[cfg(not(windows))]
            {
                let exec_socket_path = crate::socket_paths::runtime_socket(
                    record,
                    crate::socket_paths::RuntimeSocket::Exec,
                );

                if !exec_socket_path.exists() {
                    return Err(format!(
                        "cannot apply live update to running box {}: exec socket is missing at {}; no state changes were persisted. Run `a3s-box ps` to reconcile state, then retry or restart {}",
                        record.name,
                        exec_socket_path.display(),
                        record.name
                    )
                    .into());
                } else {
                    let client = ExecClient::connect(&exec_socket_path).await?;
                    let commands = update.build_cgroup_commands();
                    if commands.is_empty() {
                        return Err(
                            "live resource update produced no enforceable guest commands; no state changes were persisted"
                                .into(),
                        );
                    }
                    let request = ExecRequest {
                        request_id: None,
                        cmd: vec![
                            "sh".to_string(),
                            "-c".to_string(),
                            format!("set -e; {}", commands.join(" && ")),
                        ],
                        timeout_ns: 5_000_000_000,
                        env: vec![],
                        working_dir: None,
                        rootfs: None,
                        stdin: None,
                        stdin_streaming: false,
                        user: None,
                        streaming: false,
                    };

                    match client.exec_command(&request).await {
                        Ok(output) if output.exit_code == 0 => {
                            live_tier2_applied = true;
                        }
                        Ok(output) => {
                            let stderr = String::from_utf8_lossy(&output.stderr);
                            return Err(format!(
                                "live cgroup update failed for {} (exit {}): {}; no state changes were persisted, but the guest may have applied commands before the failure, so retry or restart the box",
                                record.name,
                                output.exit_code,
                                stderr.trim()
                            )
                            .into());
                        }
                        Err(error) => {
                            return Err(format!(
                                "failed to apply live update to {}: {error}; no state changes were persisted",
                                record.name
                            )
                            .into());
                        }
                    }
                }
            } // #[cfg(not(windows))]
        }
    }

    // Persist this box's updated fields atomically (load-fresh under the lock),
    // touching only the fields `update` owns so a concurrent writer is not lost.
    let persist_result: Result<(), std::io::Error> = StateFile::modify(|s| {
        let rec = s.find_by_id_mut(&box_id).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("box {name} was removed while applying its update"),
            )
        })?;
        // Re-check under the state lock. An inactive box may have started, or
        // an active box may have restarted onto a different execution, while
        // the guest call was in flight. Never persist a limit that missed the
        // execution which is now active.
        validate_running_update(rec.is_active(), &update).map_err(std::io::Error::other)?;
        validate_live_apply_target(&live_apply_baseline, rec, &update)
            .map_err(std::io::Error::other)?;
        apply_persisted_resource_update(rec, &update);
        if restart_policy_updated {
            rec.restart_policy = new_restart_policy.clone();
            rec.max_restart_count = new_max_restart;
        }
        sync_managed_creation_intent(rec, restart_policy_updated).map_err(std::io::Error::other)?;
        Ok::<(), std::io::Error>(())
    });
    if let Err(error) = persist_result {
        return Err(persist_update_error(&error, live_tier2_applied).into());
    }
    println!("{name}");

    Ok(())
}

fn persist_update_error(error: &std::io::Error, live_tier2_applied: bool) -> String {
    if live_tier2_applied {
        format!(
            "{error}; the guest accepted the live resource update before persistence failed, so running limits may have changed without a matching durable record; retry the update or restart the box"
        )
    } else {
        error.to_string()
    }
}

fn apply_persisted_resource_update(record: &mut crate::state::BoxRecord, update: &ResourceUpdate) {
    if let Some(vcpus) = update.vcpus {
        record.cpus = vcpus;
    }
    if let Some(memory_mb) = update.memory_mb {
        record.memory_mb = memory_mb;
    }
    if let Some(value) = update.limits.memory_reservation {
        record.resource_limits.memory_reservation = Some(value);
    }
    if let Some(value) = update.limits.memory_swap {
        record.resource_limits.memory_swap = Some(value);
    }
    if let Some(value) = update.limits.pids_limit {
        record.resource_limits.pids_limit = Some(value);
    }
    if let Some(value) = update.limits.cpu_shares {
        record.resource_limits.cpu_shares = Some(value);
    }
    if let Some(value) = update.limits.cpu_quota {
        record.resource_limits.cpu_quota = Some(value);
    }
    if let Some(value) = update.limits.cpu_period {
        record.resource_limits.cpu_period = Some(value);
    }
    if let Some(value) = update.limits.cpuset_cpus.as_ref() {
        record.resource_limits.cpuset_cpus = Some(value.clone());
    }
}

fn validate_live_apply_target(
    baseline: &crate::state::BoxRecord,
    current: &crate::state::BoxRecord,
    update: &ResourceUpdate,
) -> Result<(), String> {
    if !update.has_tier2_changes() || !current.is_active() {
        return Ok(());
    }

    if !baseline.is_active() {
        return Err(format!(
            "box {} started while its resource update was being prepared; no state changes were persisted, retry the update against the running box",
            current.name
        ));
    }

    let baseline_generation = baseline
        .managed_execution
        .as_ref()
        .map(|metadata| metadata.generation);
    let current_generation = current
        .managed_execution
        .as_ref()
        .map(|metadata| metadata.generation);
    if baseline_generation != current_generation
        || baseline.pid != current.pid
        || baseline.pid_start_time != current.pid_start_time
    {
        return Err(format!(
            "box {} changed execution while its live resource update was in flight; no state changes were persisted, retry the update",
            current.name
        ));
    }

    Ok(())
}

fn validate_running_update(is_running: bool, update: &ResourceUpdate) -> Result<(), String> {
    // A stopped box may persist these values for its next start, but malformed
    // values are never valid durable intent. Keep value validation independent
    // from the live-resize capability checks below.
    validate_update_values(update).map_err(|error| error.to_string())?;

    if !is_running {
        return Ok(());
    }

    if update.has_tier1_changes() {
        validate_update(update).map_err(|error| error.to_string())?;
    }

    #[cfg(windows)]
    if update.has_tier2_changes() {
        return Err(
            "live Tier 2 resource updates are not supported on Windows; stop the box before applying cgroup limits so they can be persisted for the next start"
                .to_string(),
        );
    }

    Ok(())
}

/// Keep the durable managed creation request in sync with the compatibility
/// fields on `BoxRecord`. Managed `start` reconstructs the VM from this request,
/// so updating only the record would look successful but boot with stale limits.
fn sync_managed_creation_intent(
    record: &mut crate::state::BoxRecord,
    restart_policy_updated: bool,
) -> Result<(), String> {
    let health_check = (!record.healthcheck_disabled)
        .then_some(record.health_check.as_ref())
        .flatten();
    common::validate_health_check_support(health_check)?;

    let Some(metadata) = record.managed_execution.as_mut() else {
        return Ok(());
    };
    let managed_health_check = (!metadata.request.policy.healthcheck_disabled)
        .then_some(metadata.request.policy.health_check.as_ref())
        .flatten();
    common::validate_health_check_support(managed_health_check)?;

    metadata.request.config.resources.vcpus = record.cpus;
    metadata.request.config.resources.memory_mb = record.memory_mb;
    metadata.request.config.resource_limits = record.resource_limits.clone();
    if restart_policy_updated {
        metadata.request.policy.restart_policy = match record.restart_policy.as_str() {
            "no" => ExecutionRestartPolicy::No,
            "always" => ExecutionRestartPolicy::Always,
            "on-failure" => ExecutionRestartPolicy::OnFailure,
            "unless-stopped" => ExecutionRestartPolicy::UnlessStopped,
            other => return Err(format!("invalid persisted restart policy: {other}")),
        };
        metadata.request.policy.max_restart_count = record.max_restart_count;
    }
    metadata.plan = a3s_box_core::resolve_execution(&metadata.request.config)
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::config::{BoxConfig, ResourceLimits};
    use a3s_box_core::{CreateExecutionRequest, ExecutionGeneration, OperationId};
    use a3s_box_runtime::ManagedExecutionMetadata;

    #[test]
    fn test_tier1_rejected_on_running() {
        let update = ResourceUpdate {
            vcpus: Some(4),
            ..Default::default()
        };
        let err = validate_update(&update);
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("vCPU"));
    }

    #[test]
    fn test_tier2_builds_commands() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpu_shares: Some(512),
                pids_limit: Some(100),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(validate_update(&update).is_ok());
        let cmds = update.build_cgroup_commands();
        assert_eq!(cmds.len(), 2);
    }

    #[test]
    fn test_memory_change_rejected() {
        let update = ResourceUpdate {
            memory_mb: Some(2048),
            ..Default::default()
        };
        let err = validate_update(&update);
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("memory"));
    }

    #[test]
    fn stopped_tier2_update_is_accepted_for_the_next_start() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                memory_reservation: Some(128 * 1024 * 1024),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(validate_running_update(false, &update).is_ok());
    }

    #[test]
    fn stopped_malformed_cpuset_is_rejected_before_persist() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpuset_cpus: Some("4-1".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };

        let error = validate_running_update(false, &update).unwrap_err();
        assert!(error.contains("cpuset.cpus"));
        assert!(error.contains("ascending ranges"));
    }

    #[test]
    fn managed_creation_intent_receives_persisted_resource_updates() {
        let mut record =
            crate::test_helpers::fixtures::make_record("update-id", "update", "created", None);
        record.managed_execution = Some(
            ManagedExecutionMetadata::new(
                OperationId::new("update-operation").unwrap(),
                ExecutionGeneration::INITIAL,
                CreateExecutionRequest {
                    external_sandbox_id: "update-external".to_string(),
                    config: BoxConfig {
                        image: record.image.clone(),
                        ..Default::default()
                    },
                    labels: Default::default(),
                    policy: Default::default(),
                    rootfs_snapshot_id: None,
                },
            )
            .unwrap(),
        );
        let update = ResourceUpdate {
            vcpus: Some(3),
            memory_mb: Some(768),
            limits: ResourceLimits {
                cpu_shares: Some(1024),
                ..Default::default()
            },
        };
        apply_persisted_resource_update(&mut record, &update);
        record.restart_policy = "on-failure".to_string();
        record.max_restart_count = 4;

        sync_managed_creation_intent(&mut record, true).unwrap();

        let metadata = record.managed_execution.as_ref().unwrap();
        assert_eq!(metadata.request.config.resources.vcpus, 3);
        assert_eq!(metadata.request.config.resources.memory_mb, 768);
        assert_eq!(
            metadata.request.config.resource_limits.cpu_shares,
            Some(1024)
        );
        assert_eq!(
            metadata.request.policy.restart_policy,
            ExecutionRestartPolicy::OnFailure
        );
        assert_eq!(metadata.request.policy.max_restart_count, 4);
        metadata.validate().unwrap();
    }

    #[test]
    fn stopped_to_running_race_rejects_tier2_persistence() {
        let baseline =
            crate::test_helpers::fixtures::make_record("race-id", "race", "stopped", None);
        let mut current = baseline.clone();
        current.status = "running".to_string();
        current.pid = Some(1234);
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpu_shares: Some(512),
                ..Default::default()
            },
            ..Default::default()
        };

        let error = validate_live_apply_target(&baseline, &current, &update).unwrap_err();
        assert!(error.contains("started while"));
        assert!(error.contains("no state changes were persisted"));
    }

    #[test]
    fn changed_running_execution_rejects_tier2_persistence() {
        let mut baseline =
            crate::test_helpers::fixtures::make_record("race-id", "race", "running", None);
        baseline.pid = Some(1234);
        baseline.pid_start_time = Some(10);
        let mut current = baseline.clone();
        current.pid = Some(5678);
        current.pid_start_time = Some(20);
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpu_shares: Some(512),
                ..Default::default()
            },
            ..Default::default()
        };

        let error = validate_live_apply_target(&baseline, &current, &update).unwrap_err();
        assert!(error.contains("changed execution"));
        assert!(error.contains("retry"));
    }

    #[test]
    fn stopped_update_only_mutates_fields_owned_by_the_request() {
        let mut record =
            crate::test_helpers::fixtures::make_record("update-id", "update", "stopped", None);
        record.resource_limits.memory_reservation = Some(64);
        record.resource_limits.cpu_quota = Some(20_000);
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpu_shares: Some(1024),
                ..Default::default()
            },
            ..Default::default()
        };

        apply_persisted_resource_update(&mut record, &update);

        assert_eq!(record.resource_limits.cpu_shares, Some(1024));
        assert_eq!(record.resource_limits.memory_reservation, Some(64));
        assert_eq!(record.resource_limits.cpu_quota, Some(20_000));
    }

    #[test]
    fn post_apply_persistence_error_explains_possible_guest_drift() {
        let error = std::io::Error::new(std::io::ErrorKind::NotFound, "box was removed");

        let message = persist_update_error(&error, true);

        assert!(message.contains("guest accepted the live resource update"));
        assert!(message.contains("without a matching durable record"));
        assert!(message.contains("retry the update or restart"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_running_tier2_update_fails_closed() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpu_shares: Some(512),
                ..Default::default()
            },
            ..Default::default()
        };
        let error = validate_running_update(true, &update).unwrap_err();
        assert!(error.contains("not supported on Windows"));
        assert!(error.contains("stop the box"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_update_rejects_persisted_effective_health_check() {
        let mut record = crate::test_helpers::fixtures::make_record(
            "health-update-id",
            "health-update",
            "stopped",
            None,
        );
        record.health_check = Some(crate::state::HealthCheck {
            cmd: vec!["true".to_string()],
            interval_secs: 30,
            timeout_secs: 5,
            retries: 3,
            start_period_secs: 0,
        });

        let error = sync_managed_creation_intent(&mut record, false).unwrap_err();
        assert!(error.contains("health checks are not supported on Windows"));
    }
}
