//! `a3s-box create` command — Create without starting.

use a3s_box_core::{
    BoxConfig, CreateExecutionRequest, ExecutionManager, ExecutionRecordPolicy,
    ExecutionRestartPolicy, OperationId, ResourceConfig,
};
use a3s_box_runtime::LocalExecutionManager;
use clap::Args;

use super::common::{self, CommonBoxArgs};
use crate::output::parse_memory;
use crate::state::{generate_name, StateFile};

#[derive(Args)]
pub struct CreateArgs {
    #[command(flatten)]
    pub common: CommonBoxArgs,

    /// Command to run when the box starts (override image CMD)
    #[arg(last = true)]
    pub cmd: Vec<String>,
}

pub async fn execute(args: CreateArgs) -> Result<(), Box<dyn std::error::Error>> {
    common::validate_runtime_options(&args.common)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    // Validate restart policy
    let (restart_policy, max_restart_count) =
        crate::state::parse_restart_policy(&args.common.restart)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let restart_policy = execution_restart_policy(&restart_policy)?;

    let memory_mb =
        parse_memory(&args.common.memory).map_err(|e| format!("Invalid --memory: {e}"))?;

    // Build resource limits before any partial moves of args
    let resource_limits = common::build_resource_limits(&args.common)?;

    let port_map = common::normalize_port_maps(&args.common.publish)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let env = common::build_env_map(&args.common)?;
    let labels = common::parse_env_vars(&args.common.labels)
        .map_err(|e| e.replace("environment variable", "label"))?
        .into_iter()
        .collect();
    if let Some(network) = args.common.network.as_deref() {
        ensure_network_exists(network)?;
    }

    let image_config = common::cached_image_config(&args.common.image).await?;
    let health_check = common::effective_health_check(
        &args.common,
        image_config
            .as_ref()
            .and_then(|config| config.health_check.as_ref()),
    );
    common::validate_health_check_support(health_check.as_ref())
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let effective_stop_signal = common::effective_stop_signal(
        args.common.stop_signal.as_deref(),
        image_config
            .as_ref()
            .and_then(|config| config.stop_signal.as_deref()),
    );
    let isolation = common::execution_isolation(&args.common);
    let name = args.common.name.unwrap_or_else(generate_name);

    // Parse --shm-size
    let shm_size = match &args.common.shm_size {
        Some(s) => {
            Some(common::parse_memory_bytes(s).map_err(|e| format!("Invalid --shm-size: {e}"))?)
        }
        None => None,
    };

    let home = a3s_box_core::dirs_home();

    // Resolve named volumes
    let mut resolved_volumes = Vec::new();
    let mut volume_names = Vec::new();
    for vol_spec in &args.common.volumes {
        let (resolved, vol_name) = super::volume::resolve_named_volume(vol_spec)?;
        if let Some(name) = vol_name {
            volume_names.push(name);
        }
        resolved_volumes.push(resolved);
    }

    let entrypoint = args
        .common
        .entrypoint
        .as_ref()
        .map(|ep| ep.split_whitespace().map(String::from).collect::<Vec<_>>());

    // Determine network mode
    let network_mode = match &args.common.network {
        Some(name) => a3s_box_core::NetworkMode::Bridge {
            network: name.clone(),
        },
        None => a3s_box_core::NetworkMode::Tsi,
    };
    let mut extra_env = env.into_iter().collect::<Vec<_>>();
    extra_env.sort_by(|left, right| left.0.cmp(&right.0));

    let config = BoxConfig {
        isolation,
        image: args.common.image.clone(),
        resources: ResourceConfig {
            vcpus: args.common.cpus,
            memory_mb,
            ..Default::default()
        },
        cmd: args.cmd.clone(),
        entrypoint_override: entrypoint,
        user: args.common.user.clone(),
        workdir: args.common.workdir.clone(),
        hostname: args.common.hostname.clone(),
        volumes: resolved_volumes,
        virtiofs_cache: args
            .common
            .virtiofs_cache
            .map(|mode| mode.as_guest_value().to_string()),
        extra_env,
        port_map,
        dns: args.common.dns.clone(),
        add_hosts: args.common.add_host.clone(),
        network: network_mode,
        tmpfs: args.common.tmpfs.clone(),
        resource_limits,
        read_only: args.common.read_only,
        cap_add: args.common.cap_add.clone(),
        cap_drop: args.common.cap_drop.clone(),
        security_opt: args.common.security_opt.clone(),
        privileged: args.common.privileged,
        // A created box is restartable and therefore retains its writable
        // filesystem until an explicit remove.
        persistent: true,
        ..Default::default()
    };
    let policy = ExecutionRecordPolicy {
        name: Some(name.clone()),
        auto_remove: false,
        restart_policy,
        max_restart_count,
        health_check,
        healthcheck_disabled: args.common.no_healthcheck,
        log_config: a3s_box_core::log::LogConfig::default(),
        volume_names: volume_names.clone(),
        platform: args.common.platform.clone(),
        init: args.common.init,
        devices: args.common.device.clone(),
        gpus: args.common.gpus.clone(),
        shm_size,
        stop_signal: effective_stop_signal,
        stop_timeout: args.common.stop_timeout,
        oom_kill_disable: args.common.oom_kill_disable,
        oom_score_adj: args.common.oom_score_adj,
    };
    let operation_id = OperationId::new(format!("cli-create-{}", uuid::Uuid::new_v4()))?;
    let request = CreateExecutionRequest {
        external_sandbox_id: operation_id.as_str().to_string(),
        config,
        labels,
        policy,
        rootfs_snapshot_id: None,
    };
    let manager = LocalExecutionManager::with_vm_backend(home.join("boxes.json"), home);
    let reservation = manager.create(request, &operation_id).await?;
    let box_id = reservation.execution_id.to_string();

    // Attach named volumes to this box
    if let Err(error) = super::volume::attach_volumes(&volume_names, &box_id) {
        let mut state = StateFile::load_default()?;
        if let Some(record) = state.find_by_id(&box_id).cloned() {
            crate::cleanup::cleanup_partial_box_record(&record, Some(&mut state));
        }
        return Err(error);
    }

    crate::audit::record(
        a3s_box_core::audit::AuditAction::BoxCreate,
        a3s_box_core::audit::AuditOutcome::Success,
        &box_id,
        &format!("created box {name}"),
    );
    println!("{box_id}");
    Ok(())
}

fn execution_restart_policy(value: &str) -> Result<ExecutionRestartPolicy, String> {
    match value {
        "no" => Ok(ExecutionRestartPolicy::No),
        "always" => Ok(ExecutionRestartPolicy::Always),
        "on-failure" => Ok(ExecutionRestartPolicy::OnFailure),
        "unless-stopped" => Ok(ExecutionRestartPolicy::UnlessStopped),
        other => Err(format!("Invalid normalized restart policy: {other}")),
    }
}

fn ensure_network_exists(network: &str) -> Result<(), Box<dyn std::error::Error>> {
    let store = a3s_box_runtime::NetworkStore::default_path()?;
    let config = store
        .get(network)?
        .ok_or_else(|| format!("network '{}' not found", network))?;
    super::network::validate_attachable_network(&config)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_restart_policies_map_to_typed_creation_policy() {
        assert_eq!(
            execution_restart_policy("no").unwrap(),
            ExecutionRestartPolicy::No
        );
        assert_eq!(
            execution_restart_policy("always").unwrap(),
            ExecutionRestartPolicy::Always
        );
        assert_eq!(
            execution_restart_policy("on-failure").unwrap(),
            ExecutionRestartPolicy::OnFailure
        );
        assert_eq!(
            execution_restart_policy("unless-stopped").unwrap(),
            ExecutionRestartPolicy::UnlessStopped
        );
        assert!(execution_restart_policy("on-failure:3").is_err());
    }
}
