//! `a3s-box compose` command — Multi-container orchestration.
//!
//! Project discovery, service selection, and lifecycle operations are kept in
//! this command while individual box behavior is delegated to the existing
//! single-box commands.

mod args;
mod lifecycle;
mod operations;
mod read;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::PathBuf;

use a3s_box_core::compose::{normalize_compose, ComposeConfig, ComposeSourceFormat, ServiceConfig};
use a3s_box_core::config::DEFAULT_VCPUS;
use a3s_box_core::event::EventEmitter;
use a3s_box_runtime::{ComposeRuntimePlan, NetworkStore, VmManager};
use sha2::{Digest, Sha256};

use super::common;
use crate::state::{BoxRecord, HealthCheck, StateFile};
use crate::status;

pub use args::{ComposeArgs, ComposeCommand, ComposeDownArgs, ComposeLogsArgs, ComposeUpArgs};
use lifecycle::{
    cleanup_partial_service_box, execute_down, rollback_compose_up, rollback_with_current,
    teardown_service_box, ServiceBox,
};
use operations::{ComposeStopArgs, ProjectServicesArgs};
use read::{execute_config, execute_logs, execute_ps};

/// Label key for compose project name.
const LABEL_PROJECT: &str = "com.a3s.compose.project";
/// Label key for compose service name.
const LABEL_SERVICE: &str = "com.a3s.compose.service";
/// Label key for the normalized service configuration digest.
const LABEL_CONFIG_HASH: &str = "com.a3s.compose.config-hash";
type ExistingService = (ServiceBox, Option<String>);

/// Default compose file names to search for.
const COMPOSE_FILES: &[&str] = &[
    "compose.acl",
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];

pub async fn execute(args: ComposeArgs) -> Result<(), Box<dyn std::error::Error>> {
    let ComposeArgs {
        file,
        project_name,
        command,
    } = args;

    if let ComposeCommand::Ls(ls_args) = command {
        return operations::execute_ls(ls_args).await;
    }

    let (compose_path, config) = load_compose_file(file.as_deref())?;

    // Derive project name from flag or directory name
    let project_name = project_name.unwrap_or_else(|| {
        compose_path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("default")
            .to_string()
    });

    match command {
        ComposeCommand::Up(up_args) => {
            execute_up(&project_name, config, compose_path, up_args).await
        }
        ComposeCommand::Down(down_args) => execute_down(&project_name, &config, down_args).await,
        ComposeCommand::Ps(command_args) => execute_ps(&project_name, &config, command_args).await,
        ComposeCommand::Config => execute_config(&project_name, config),
        ComposeCommand::Logs(logs_args) => execute_logs(&project_name, &config, logs_args).await,
        ComposeCommand::Start(command_args) => {
            operations::execute_start(&project_name, &config, command_args).await
        }
        ComposeCommand::Stop(command_args) => {
            operations::execute_stop(&project_name, &config, command_args).await
        }
        ComposeCommand::Restart(command_args) => {
            operations::execute_restart(&project_name, &config, command_args).await
        }
        ComposeCommand::Rm(command_args) => {
            operations::execute_rm(&project_name, &config, command_args).await
        }
        ComposeCommand::Kill(command_args) => {
            operations::execute_kill(&project_name, &config, command_args).await
        }
        ComposeCommand::Pause(command_args) => {
            operations::execute_pause(&project_name, &config, command_args).await
        }
        ComposeCommand::Unpause(command_args) => {
            operations::execute_unpause(&project_name, &config, command_args).await
        }
        ComposeCommand::Wait(command_args) => {
            operations::execute_wait(&project_name, &config, command_args).await
        }
        ComposeCommand::Exec(command_args) => {
            operations::execute_exec(&project_name, &config, command_args).await
        }
        ComposeCommand::Top(command_args) => {
            operations::execute_top(&project_name, &config, command_args).await
        }
        ComposeCommand::Port(command_args) => {
            operations::execute_port(&project_name, &config, command_args).await
        }
        ComposeCommand::Cp(command_args) => {
            operations::execute_cp(&project_name, &config, command_args).await
        }
        ComposeCommand::Images(command_args) => {
            operations::execute_images(&project_name, &config, command_args)
        }
        ComposeCommand::Pull(command_args) => {
            operations::execute_pull(&project_name, &config, command_args).await
        }
        ComposeCommand::Volumes => operations::execute_volumes(&project_name, &config),
        ComposeCommand::Ls(_) => {
            Err("Compose ls was not dispatched before project file loading".into())
        }
    }
}

/// Find and load the compose file.
fn load_compose_file(
    explicit_path: Option<&std::path::Path>,
) -> Result<(PathBuf, ComposeConfig), Box<dyn std::error::Error>> {
    load_compose_file_with_environment(explicit_path, std::env::vars())
}

fn load_compose_file_with_environment(
    explicit_path: Option<&std::path::Path>,
    shell_environment: impl IntoIterator<Item = (String, String)>,
) -> Result<(PathBuf, ComposeConfig), Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    let path = resolve_compose_path(explicit_path, &cwd)?;

    let source = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

    let mut environment = HashMap::new();
    let environment_path = path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(".env");
    match std::fs::read_to_string(&environment_path) {
        Ok(contents) => {
            for (key, value) in a3s_box_core::env::parse_env_file_content(&contents) {
                environment.insert(key, value);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "Failed to read Compose environment file {}: {}",
                environment_path.display(),
                error
            )
            .into());
        }
    }
    environment.extend(shell_environment);

    let format = if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("acl"))
    {
        ComposeSourceFormat::Acl
    } else {
        ComposeSourceFormat::Yaml
    };
    let config = normalize_compose(&source, format, &environment)
        .map_err(|error| format!("Failed to normalize {}: {error}", path.display()))?
        .into_config();

    Ok((path, config))
}

fn resolve_compose_path(
    explicit_path: Option<&std::path::Path>,
    search_directory: &std::path::Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(p) = explicit_path {
        if !p.exists() {
            return Err(format!("Compose file not found: {}", p.display()).into());
        }
        Ok(p.to_path_buf())
    } else {
        match COMPOSE_FILES
            .iter()
            .map(|name| search_directory.join(name))
            .find(|p| p.exists())
        {
            Some(path) => Ok(path),
            None => Err(format!(
                "No compose file found. Looked for: {}",
                COMPOSE_FILES.join(", ")
            )
            .into()),
        }
    }
}

fn validate_compose_restart_policies(config: &ComposeConfig) -> Result<(), String> {
    for (service_name, service) in &config.services {
        service_restart_policy(service_name, Some(service))?;
    }
    Ok(())
}

async fn validate_compose_health_support(
    project: &ComposeRuntimePlan,
) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(windows)]
    {
        let default_network = project.default_network_name();
        // Reject every explicit service health check before resolving (and
        // potentially pulling) any image in the project.
        for service_name in &project.service_order {
            let disabled = project.healthcheck_disabled(service_name);
            let service_health_check =
                project
                    .healthcheck(service_name)
                    .map(|health_check| HealthCheck {
                        cmd: health_check.cmd,
                        interval_secs: health_check.interval_secs,
                        timeout_secs: health_check.timeout_secs,
                        retries: health_check.retries,
                        start_period_secs: health_check.start_period_secs,
                    });
            validate_known_compose_health(service_name, disabled, service_health_check, None)
                .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
        }

        for service_name in &project.service_order {
            if project.healthcheck_disabled(service_name) {
                continue;
            }

            let image = project
                .build_box_config(service_name, Some(&default_network))?
                .image;
            let mut image_config = common::cached_image_config(&image).await?;
            if image_config.is_none() {
                // Resolving image metadata may populate the image cache, but it
                // happens before network/box creation and VM startup. This is
                // necessary to distinguish a normal fresh image from one whose
                // OCI config defines an unsupported Windows HEALTHCHECK.
                super::pull::execute(super::pull::PullArgs {
                    image: image.clone(),
                    quiet: false,
                    platform: None,
                    verify_key: None,
                    verify_issuer: None,
                    verify_identity: None,
                })
                .await?;
                image_config = common::cached_image_config(&image).await?;
            }
            let image_config = image_config.ok_or_else(|| {
                format!(
                    "Compose service '{service_name}' image metadata was unavailable after pulling {image}"
                )
            })?;
            validate_known_compose_health(service_name, false, None, Some(&image_config))
                .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
        }
    }

    #[cfg(not(windows))]
    let _ = project;

    Ok(())
}

#[cfg(windows)]
fn validate_known_compose_health(
    service_name: &str,
    healthcheck_disabled: bool,
    service_health_check: Option<HealthCheck>,
    cached_image_config: Option<&a3s_box_runtime::oci::OciImageConfig>,
) -> Result<(), String> {
    if healthcheck_disabled {
        return Ok(());
    }

    let effective_health_check = service_health_check.or_else(|| {
        cached_image_config
            .and_then(|config| config.health_check.as_ref())
            .and_then(common::health_check_from_oci)
    });
    if let Some(health_check) = effective_health_check.as_ref() {
        return common::validate_health_check_support(Some(health_check))
            .map_err(|error| format!("Compose service '{service_name}': {error}"));
    }

    Ok(())
}

fn service_restart_policy(
    service_name: &str,
    service: Option<&ServiceConfig>,
) -> Result<(String, u32), String> {
    let Some(restart) = service.and_then(|service| service.restart.as_deref()) else {
        return Ok(("no".to_string(), 0));
    };

    crate::state::parse_restart_policy(restart)
        .map_err(|error| format!("Service '{service_name}' has invalid restart policy: {error}"))
}

// ============================================================================
// compose up
// ============================================================================

/// `compose up` — Create networks and start services in dependency order.
///
/// When a service declares `depends_on: { svc: { condition: service_healthy } }`,
/// we wait for the dependency to reach "healthy" status before booting the dependent.
async fn execute_up(
    project_name: &str,
    config: ComposeConfig,
    compose_path: PathBuf,
    up_args: ComposeUpArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let isolation = common::resolve_isolation(up_args.isolation);
    let config = operations::select_up_config(config, &up_args.services)?;
    let base_dir = compose_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    validate_compose_restart_policies(&config)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let project = ComposeRuntimePlan::with_base_dir(project_name, config, base_dir)?;
    // Windows has no guest health-probe transport. Resolve declared health
    // checks and image metadata before creating networks, box directories, or
    // starting a VM. Metadata resolution may populate the image cache.
    validate_compose_health_support(&project).await?;
    if isolation.is_sandbox() {
        let default_network = project.default_network_name();
        for service_name in &project.service_order {
            let mut config = project.build_box_config(service_name, Some(&default_network))?;
            config.isolation = isolation;
            a3s_box_core::resolve_execution(&config)?;
        }
    }
    let mut state = StateFile::load_default()?;

    // Step 1: Create networks
    let networks = project.required_networks();
    let net_store = NetworkStore::default_path()?;
    let mut created_networks = Vec::new();
    let mut started_services = Vec::new();
    for (i, net_name) in networks.iter().enumerate() {
        let existing_network = match net_store.get(net_name) {
            Ok(network) => network,
            Err(error) => {
                return rollback_compose_up(
                    &mut state,
                    &started_services,
                    &created_networks,
                    error,
                )
                .await;
            }
        };
        if let Some(config) = existing_network.as_ref() {
            if let Err(error) = super::network::validate_attachable_network(config) {
                return rollback_compose_up(
                    &mut state,
                    &started_services,
                    &created_networks,
                    error,
                )
                .await;
            }
        } else {
            let subnet = format!("10.89.{}.0/24", 100 + i);
            let config = match a3s_box_core::network::NetworkConfig::new(net_name, &subnet) {
                Ok(config) => config,
                Err(error) => {
                    return rollback_compose_up(
                        &mut state,
                        &started_services,
                        &created_networks,
                        format!("Failed to create network '{}': {}", net_name, error),
                    )
                    .await;
                }
            };
            if let Err(error) = super::network::validate_attachable_network(&config) {
                return rollback_compose_up(
                    &mut state,
                    &started_services,
                    &created_networks,
                    error,
                )
                .await;
            }
            if let Err(error) = net_store.create(config) {
                return rollback_compose_up(
                    &mut state,
                    &started_services,
                    &created_networks,
                    error,
                )
                .await;
            }
            created_networks.push(net_name.clone());
            println!("  [+] Network {} ({})", net_name, subnet);
        }
    }

    // Step 2: Boot services in dependency order
    let default_net = project.default_network_name();
    let home = a3s_box_core::dirs_home();

    println!(
        "Starting project '{}' ({} services)...",
        project_name,
        project.service_order.len()
    );

    for svc_name in &project.service_order {
        let service = project.config.services.get(svc_name).ok_or_else(|| {
            format!("Service '{svc_name}' disappeared from the resolved Compose project")
        })?;
        let mut desired_box_config = project.build_box_config(svc_name, Some(&default_net))?;
        desired_box_config.isolation = isolation;
        let config_hash = service_config_hash(service, &desired_box_config)?;
        if let Some((existing, existing_hash)) = find_existing_service(project_name, svc_name)? {
            if existing.is_active() && existing_hash.as_deref() == Some(config_hash.as_str()) {
                println!("  [=] {} is unchanged and already running", svc_name);
                continue;
            }

            if existing.is_active() {
                println!("  [~] Recreating changed service {}...", svc_name);
            } else {
                println!("  [~] Recreating existing service {}...", svc_name);
            }
            teardown_service_box(&mut state, &existing).await?;
        }

        // Wait for healthy dependencies before booting this service
        let health_deps = project.health_wait_deps(svc_name);
        if !health_deps.is_empty() {
            print!(
                "  [~] Waiting for {} to be healthy...",
                health_deps.join(", ")
            );
            if let Err(error) = wait_for_healthy(project_name, &health_deps, up_args.timeout).await
            {
                return rollback_compose_up(
                    &mut state,
                    &started_services,
                    &created_networks,
                    error,
                )
                .await;
            }
            println!(" ✓");
        }

        // Wait for dependencies that must run to completion (exit 0) first.
        let completed_deps = project.completed_wait_deps(svc_name);
        if !completed_deps.is_empty() {
            print!(
                "  [~] Waiting for {} to complete...",
                completed_deps.join(", ")
            );
            if let Err(error) =
                wait_for_completed(project_name, &completed_deps, up_args.timeout).await
            {
                return rollback_compose_up(
                    &mut state,
                    &started_services,
                    &created_networks,
                    error,
                )
                .await;
            }
            println!(" ✓");
        }

        let mut box_config = desired_box_config;
        let (resolved_volumes, volume_names) = match resolve_service_volumes(&box_config.volumes) {
            Ok(volumes) => volumes,
            Err(error) => {
                return rollback_compose_up(
                    &mut state,
                    &started_services,
                    &created_networks,
                    error,
                )
                .await;
            }
        };
        box_config.volumes = resolved_volumes.clone();
        let image = box_config.image.clone();
        let record_env: HashMap<String, String> = box_config.extra_env.iter().cloned().collect();
        let record_hostname = box_config.hostname.clone();
        let record_add_hosts = box_config.add_hosts.clone();
        let network_mode = box_config.network.clone();
        let record_isolation = box_config.isolation;
        let network_name = match &network_mode {
            a3s_box_core::NetworkMode::Bridge { network } => Some(network.clone()),
            _ => None,
        };

        // Create VmManager and boot
        let emitter = EventEmitter::new(256);
        let box_name = format!("{}-{}", project_name, svc_name);
        let mut vm = VmManager::new(box_config, emitter);
        vm.set_healthcheck_disabled(project.healthcheck_disabled(svc_name));
        let box_id = vm.box_id().to_string();
        let box_dir = home.join("boxes").join(&box_id);
        let initial_exec_socket_path = box_dir.join("sockets").join("exec.sock");

        // Create box directory structure
        if let Err(error) = std::fs::create_dir_all(box_dir.join("sockets")) {
            cleanup_partial_service_box(
                &box_id,
                &box_dir,
                &initial_exec_socket_path,
                network_name.as_deref(),
                &volume_names,
                &[],
            );
            return rollback_compose_up(&mut state, &started_services, &created_networks, error)
                .await;
        }
        if let Err(error) = std::fs::create_dir_all(box_dir.join("logs")) {
            cleanup_partial_service_box(
                &box_id,
                &box_dir,
                &initial_exec_socket_path,
                network_name.as_deref(),
                &volume_names,
                &[],
            );
            return rollback_compose_up(&mut state, &started_services, &created_networks, error)
                .await;
        }

        // Connect to network before boot
        if let Some(net_name) = network_name.as_deref() {
            let network_aliases = project.service_network_aliases(svc_name);
            // Atomic load → validate → allocate-IP → save under the store's
            // cross-process lock. A get → connect → update reads the network
            // outside the lock, so a concurrent connect (another compose up, or
            // a `run --network` to the same net) could dup the IP or drop this
            // endpoint. Register the bare service name plus declared network
            // aliases so peers can use Compose DNS names, not only the
            // `{project}-{svc}` box name.
            let endpoint =
                match net_store.with_write_lock(
                    |networks| -> Result<
                        a3s_box_core::network::NetworkEndpoint,
                        Box<dyn std::error::Error>,
                    > {
                        let net_config = networks.get_mut(net_name).ok_or_else(
                            || -> Box<dyn std::error::Error> {
                                format!("Compose network '{}' was not created", net_name).into()
                            },
                        )?;
                        super::network::validate_attachable_network(net_config)
                            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                        net_config
                            .connect_with_aliases(&box_id, &box_name, &network_aliases)
                            .map_err(|e| -> Box<dyn std::error::Error> {
                                format!("Failed to connect service '{}' to network: {e}", svc_name)
                                    .into()
                            })
                    },
                ) {
                    Ok(endpoint) => endpoint,
                    Err(error) => {
                        cleanup_partial_service_box(
                            &box_id,
                            &box_dir,
                            &initial_exec_socket_path,
                            network_name.as_deref(),
                            &volume_names,
                            &[],
                        );
                        return rollback_compose_up(
                            &mut state,
                            &started_services,
                            &created_networks,
                            error,
                        )
                        .await;
                    }
                };
            print!(
                "  [+] {} (image={}, ip={})",
                svc_name, image, endpoint.ip_address
            );
        }

        if let Err(e) = vm.boot().await {
            cleanup_partial_service_box(
                &box_id,
                &box_dir,
                &initial_exec_socket_path,
                network_name.as_deref(),
                &volume_names,
                vm.anonymous_volumes(),
            );
            return rollback_compose_up(
                &mut state,
                &started_services,
                &created_networks,
                format!("Failed to start service '{}': {}", svc_name, e),
            )
            .await;
        }

        let pid = vm.pid().await;
        let exec_socket_path = vm
            .exec_socket_path()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| box_dir.join("sockets").join("exec.sock"));
        let anonymous_volumes = vm.anonymous_volumes().to_vec();
        let image_health_check = vm
            .image_config()
            .and_then(|config| config.health_check.clone());
        let image_stop_signal = vm
            .image_config()
            .and_then(|config| config.stop_signal.clone());

        // Build labels with compose metadata
        let svc = project.config.services.get(svc_name);
        let mut labels = svc.map(|s| s.labels.to_map()).unwrap_or_default();
        labels.insert(LABEL_PROJECT.to_string(), project_name.to_string());
        labels.insert(LABEL_SERVICE.to_string(), svc_name.to_string());
        labels.insert(LABEL_CONFIG_HASH.to_string(), config_hash);

        // Get service config for extra fields
        let port_map: Vec<String> = svc.map(|s| s.ports.clone()).unwrap_or_default();

        // Compose healthcheck overrides image HEALTHCHECK; disable blocks fallback.
        let service_health_check = project.healthcheck(svc_name).map(|hc| HealthCheck {
            cmd: hc.cmd,
            interval_secs: hc.interval_secs,
            timeout_secs: hc.timeout_secs,
            retries: hc.retries,
            start_period_secs: hc.start_period_secs,
        });
        let healthcheck_disabled = project.healthcheck_disabled(svc_name);
        let health_check = if healthcheck_disabled {
            None
        } else {
            service_health_check.or_else(|| {
                image_health_check
                    .as_ref()
                    .and_then(common::health_check_from_oci)
            })
        };

        let health_status = if health_check.is_some() {
            "starting".to_string()
        } else {
            "none".to_string()
        };
        let (restart_policy, max_restart_count) = service_restart_policy(svc_name, svc)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

        let record = BoxRecord {
            id: box_id.clone(),
            short_id: BoxRecord::make_short_id(&box_id),
            name: box_name,
            image,
            isolation: record_isolation,
            managed_execution: None,
            status: "running".to_string(),
            pid,
            pid_start_time: pid.and_then(crate::process::pid_start_time),
            cpus: svc.and_then(|s| s.cpus).unwrap_or(DEFAULT_VCPUS),
            memory_mb: svc
                .and_then(|s| s.mem_limit.as_ref())
                .and_then(|m| crate::output::parse_memory(m).ok())
                .unwrap_or(512),
            volumes: resolved_volumes,
            virtiofs_cache: None,
            env: record_env,
            cmd: svc
                .and_then(|s| s.command.as_ref())
                .map(|c| c.to_vec())
                .unwrap_or_default(),
            entrypoint: svc.and_then(|s| s.entrypoint.as_ref()).map(|e| e.to_vec()),
            box_dir: box_dir.clone(),
            exec_socket_path: exec_socket_path.clone(),
            console_log: box_dir.join("logs").join("console.log"),
            created_at: chrono::Utc::now(),
            started_at: Some(chrono::Utc::now()),
            auto_remove: false,
            hostname: record_hostname,
            user: None,
            workdir: svc.and_then(|s| s.working_dir.clone()),
            restart_policy,
            port_map,
            labels,
            stopped_by_user: false,
            restart_count: 0,
            max_restart_count,
            exit_code: None,
            health_check: health_check.clone(),
            healthcheck_disabled,
            health_status,
            health_retries: 0,
            health_last_check: None,
            network_mode,
            network_name: network_name.clone(),
            volume_names: volume_names.clone(),
            tmpfs: svc.map(|s| s.tmpfs.to_vec()).unwrap_or_default(),
            anonymous_volumes,
            resource_limits: Default::default(),
            log_config: Default::default(),
            add_host: record_add_hosts,
            platform: None,
            init: false,
            read_only: false,
            cap_add: svc.map(|s| s.cap_add.clone()).unwrap_or_default(),
            cap_drop: svc.map(|s| s.cap_drop.clone()).unwrap_or_default(),
            security_opt: vec![],
            privileged: svc.map(|s| s.privileged).unwrap_or(false),
            devices: vec![],
            gpus: None,
            shm_size: None,
            stop_signal: image_stop_signal,
            stop_timeout: None,
            oom_kill_disable: false,
            oom_score_adj: None,
        };

        let service_box = ServiceBox::from_record(&record);
        // Health checks run concurrently while `compose up` waits for later
        // services. Reload before appending so we do not overwrite dependency
        // health transitions captured by the checker.
        state = match StateFile::load_default() {
            Ok(state) => state,
            Err(error) => {
                let rollback_services = rollback_with_current(&started_services, service_box);
                return rollback_compose_up(
                    &mut state,
                    &rollback_services,
                    &created_networks,
                    error,
                )
                .await;
            }
        };
        // Atomic append under the state lock (load-fresh + push + save): a plain
        // state.add() saved a snapshot loaded before concurrent health/sibling
        // writes, clobbering them (the lost-registration → orphan-VM race).
        if let Err(error) = StateFile::add_record(record.clone()) {
            let rollback_services = rollback_with_current(&started_services, service_box);
            return rollback_compose_up(&mut state, &rollback_services, &created_networks, error)
                .await;
        }
        if let Err(error) = super::volume::attach_volumes(&volume_names, &box_id) {
            let rollback_services = rollback_with_current(&started_services, service_box);
            return rollback_compose_up(&mut state, &rollback_services, &created_networks, error)
                .await;
        }
        started_services.push(service_box);

        // Compose returns after startup, so health ownership must outlive this
        // CLI process. A generation-fenced worker updates the same state record.
        if health_check.is_some() {
            if let Err(error) = crate::health::spawn_detached_health_checker(&record) {
                let rollback_services = started_services.clone();
                return rollback_compose_up(
                    &mut state,
                    &rollback_services,
                    &created_networks,
                    error,
                )
                .await;
            }
        }

        // Ensure the log dir exists; the shim runs the log processor (default
        // json-file driver) for each service box's lifetime.
        let _ = std::fs::create_dir_all(box_dir.join("logs"));

        println!(" ✓");
    }

    println!("All {} services converged.", project.service_order.len());

    if !up_args.detach {
        println!("Attaching to project logs. Press Ctrl-C to stop services.");
        let logs = execute_logs(
            project_name,
            &project.config,
            ComposeLogsArgs {
                follow: true,
                tail: 100,
                services: project.service_order.clone(),
            },
        );
        tokio::select! {
            result = logs => result?,
            signal = tokio::signal::ctrl_c() => {
                signal.map_err(|error| format!("Failed to listen for Ctrl-C: {error}"))?;
                operations::execute_stop(
                    project_name,
                    &project.config,
                    ComposeStopArgs {
                        timeout: None,
                        services: project.service_order.clone(),
                    },
                )
                .await?;
            }
        }
    }

    Ok(())
}

fn service_config_hash(
    service: &ServiceConfig,
    config: &a3s_box_core::BoxConfig,
) -> Result<String, serde_json::Error> {
    let mut normalized = config.clone();
    normalized.extra_env.sort();
    let value = serde_json::json!({
        "service": service,
        "runtime": normalized,
    });
    let encoded = serde_json::to_vec(&value)?;
    Ok(hex::encode(Sha256::digest(encoded)))
}

fn find_existing_service(
    project_name: &str,
    service_name: &str,
) -> Result<Option<ExistingService>, Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let matching = state
        .find_by_label(LABEL_PROJECT, project_name)
        .into_iter()
        .filter(|record| record.labels.get(LABEL_SERVICE).map(String::as_str) == Some(service_name))
        .collect::<Vec<_>>();
    if matching.len() > 1 {
        return Err(format!(
            "service '{service_name}' has {} existing boxes; scaling is not yet enabled for this project",
            matching.len()
        )
        .into());
    }
    Ok(matching.first().map(|record| {
        (
            ServiceBox::from_record(record),
            record.labels.get(LABEL_CONFIG_HASH).cloned(),
        )
    }))
}

fn resolve_service_volumes(
    volume_specs: &[String],
) -> Result<(Vec<String>, Vec<String>), Box<dyn std::error::Error>> {
    let mut resolved = Vec::new();
    let mut names = Vec::new();

    for spec in volume_specs {
        let (resolved_spec, volume_name) = super::volume::resolve_named_volume(spec)?;
        if let Some(name) = volume_name {
            names.push(name);
        }
        resolved.push(resolved_spec);
    }

    Ok((resolved, names))
}

/// Wait for all named services to reach "healthy" status in the state file.
///
/// Polls the state file every 2 seconds until all services are healthy or timeout.
async fn wait_for_healthy(
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

        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// Wait for dependency services to run to completion (Docker's
/// `service_completed_successfully`).
///
/// A dependency is "completed" once it is no longer active — preferring the
/// record's terminal status (set by the monitor) and falling back to shim-PID
/// liveness for the daemonless case. If an exit code was recorded and is
/// non-zero, the dependency failed and the wait errors.
async fn wait_for_completed(
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

            if let Some(code) = record.exit_code {
                if code != 0 {
                    return Err(format!(
                        "dependency service '{}' did not complete successfully (exit code {})",
                        svc_name, code
                    )
                    .into());
                }
            }
        }

        if all_done {
            return Ok(());
        }

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}
