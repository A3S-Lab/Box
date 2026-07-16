use super::*;

pub(super) struct RunRecordPolicy {
    pub(super) name: String,
    pub(super) restart_policy: ExecutionRestartPolicy,
    pub(super) max_restart_count: u32,
    pub(super) health_check: Option<crate::state::HealthCheck>,
    pub(super) log_config: a3s_box_core::log::LogConfig,
    pub(super) volume_names: Vec<String>,
    pub(super) shm_size: Option<u64>,
    pub(super) stop_signal: Option<String>,
}

// ============================================================================
// Phase 1: Parse args, build config, boot VM, save state
// ============================================================================

pub(super) async fn setup_and_boot(
    args: &RunArgs,
) -> Result<RunContext, Box<dyn std::error::Error>> {
    let create_start = std::time::Instant::now();
    common::validate_runtime_options(&args.common)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let (restart_policy, max_restart_count) =
        crate::state::parse_restart_policy(&args.common.restart)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let restart_policy = execution_restart_policy(&restart_policy)?;

    let memory_mb =
        parse_memory(&args.common.memory).map_err(|e| format!("Invalid --memory: {e}"))?;
    let resource_limits = common::build_resource_limits(&args.common)?;

    let log_driver: a3s_box_core::log::LogDriver = args
        .log_driver
        .parse()
        .map_err(|e: String| format!("Invalid --log-driver: {e}"))?;
    let log_opts = common::parse_env_vars(&args.log_opts)
        .map_err(|e| e.replace("environment variable", "log option"))?;
    let log_config = a3s_box_core::log::LogConfig {
        driver: log_driver,
        options: log_opts,
    };

    let name = args.common.name.clone().unwrap_or_else(generate_name);
    let mut env = common::build_env_map(&args.common)?;
    let port_map = common::normalize_port_maps(&args.common.publish)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let labels = common::parse_env_vars(&args.common.labels)
        .map_err(|e| e.replace("environment variable", "label"))?
        .into_iter()
        .collect();
    let entrypoint_override = args
        .common
        .entrypoint
        .as_ref()
        .map(|ep| ep.split_whitespace().map(String::from).collect::<Vec<_>>());
    let mut volume_specs = args.common.volumes.clone();
    apply_package_caches(&args.package_cache, &mut volume_specs, &mut env);
    let (resolved_volumes, volume_names) = resolve_volumes(&volume_specs)?;

    // Parse --shm-size once; reuse for both tmpfs entry and the box record.
    let shm_size = match &args.common.shm_size {
        Some(s) => {
            Some(common::parse_memory_bytes(s).map_err(|e| format!("Invalid --shm-size: {e}"))?)
        }
        None => None,
    };
    let network_mode = match &args.common.network {
        Some(name) => a3s_box_core::NetworkMode::Bridge {
            network: name.clone(),
        },
        None => a3s_box_core::NetworkMode::Tsi,
    };

    // Default (TSI) networking proxies guest sockets to the host, so a container
    // cannot reach its own services over the guest loopback. A health check that
    // probes localhost would always fail — point the user at bridge networking.
    if matches!(network_mode, a3s_box_core::NetworkMode::Tsi) {
        if let Some(cmd) = &args.common.health_cmd {
            let lc = cmd.to_lowercase();
            if lc.contains("localhost") || lc.contains("127.0.0.1") {
                eprintln!(
                    "warning: the health check probes localhost, but default (TSI) networking \
                     cannot reach a container's own services over loopback, so the check will fail. \
                     For a working localhost, create and attach a bridge network: \
                     `a3s-box network create mynet` then run with `--network mynet`."
                );
            }
        }
    }

    let tee = build_tee_config(args);

    let config = build_box_config(
        args,
        memory_mb,
        resource_limits.clone(),
        entrypoint_override.clone(),
        resolved_volumes.clone(),
        env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        port_map.clone(),
        network_mode.clone(),
        args.common.tmpfs.clone(),
        tee,
    )
    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    a3s_box_core::resolve_execution(&config)?;

    // Freeze image-defined lifecycle defaults into the managed creation
    // request. Pulling is cache-first, and happens only after the pure backend
    // compatibility check above, so an invalid Sandbox request has no registry
    // or runtime side effects.
    let pull_progress_fn = pull_progress_callback(args.common.image.clone());
    let image_config_start = std::time::Instant::now();
    let image_config = pull_image_config(args, std::sync::Arc::clone(&pull_progress_fn)).await?;
    a3s_box_core::lifecycle_profile::record_lifecycle_phase(
        "cli.image_config",
        image_config_start.elapsed(),
    );
    let health_check =
        common::effective_health_check(&args.common, image_config.health_check.as_ref());
    let effective_stop_signal = common::effective_stop_signal(
        args.common.stop_signal.as_deref(),
        image_config.stop_signal.as_deref(),
    );

    let operation_id = OperationId::new(format!("cli-run-{}", uuid::Uuid::new_v4()))?;
    let request = build_execution_request(
        args,
        &operation_id,
        config,
        labels,
        RunRecordPolicy {
            name: name.clone(),
            restart_policy,
            max_restart_count,
            health_check,
            log_config,
            volume_names,
            shm_size,
            stop_signal: effective_stop_signal,
        },
    );
    let home = a3s_box_core::dirs_home();
    let backend = VmLocalExecutionBackend::new(&home).with_pull_progress_fn(pull_progress_fn);
    let manager =
        LocalExecutionManager::new(home.join("boxes.json"), &home, std::sync::Arc::new(backend));
    let reserve_start = std::time::Instant::now();
    let reservation = manager.create(request, &operation_id).await?;
    a3s_box_core::lifecycle_profile::record_lifecycle_phase("cli.reserve", reserve_start.elapsed());
    let execution_id = reservation.execution_id.clone();
    let box_id = execution_id.to_string();
    println!(
        "Creating box {} ({})...",
        name,
        BoxRecord::make_short_id(&box_id)
    );
    let runtime_start = std::time::Instant::now();
    let lease = match manager.start(&execution_id, reservation.generation).await {
        Ok(lease) => lease,
        Err(error) => {
            cleanup_failed_managed_run(&box_id);
            return Err(error.into());
        }
    };
    a3s_box_core::lifecycle_profile::record_lifecycle_phase(
        "cli.runtime_start",
        runtime_start.elapsed(),
    );
    // A short-lived command can exit between `start` and this reload. Use the
    // side-effect-free snapshot so legacy PID reconciliation cannot auto-remove
    // the just-created managed record before foreground cleanup observes it.
    let record = StateFile::load_readonly()?
        .find_by_id(&box_id)
        .cloned()
        .ok_or_else(|| format!("managed run {box_id} disappeared after startup"))?;
    let box_dir = record.box_dir.clone();
    let exec_socket_path = record.exec_socket_path.clone();
    let pty_socket_path = exec_socket_path
        .parent()
        .map(|parent| parent.join("pty.sock"))
        .unwrap_or_else(|| box_dir.join("sockets/pty.sock"));
    let anonymous_volumes = record.anonymous_volumes.clone();

    if should_create_diff_baseline(args) {
        if let Err(error) = crate::commands::diff::create_box_baseline_snapshot(&box_dir) {
            tracing::warn!(
                box_id = %box_id,
                error = %error,
                "Failed to create rootfs diff baseline snapshot"
            );
        }
    } else {
        tracing::debug!(
            box_id = %box_id,
            "Skipping rootfs diff baseline snapshot for foreground --rm box"
        );
    }

    let context = RunContext {
        manager,
        execution_id,
        generation: lease.generation,
        box_id,
        box_dir,
        name,
        record,
        exec_socket_path,
        pty_socket_path,
        anonymous_volumes,
        health_checker: None,
    };
    a3s_box_core::lifecycle_profile::record_lifecycle_phase(
        "cli.create_start",
        create_start.elapsed(),
    );
    Ok(context)
}

fn pull_progress_callback(image_name: String) -> a3s_box_runtime::PullProgressFn {
    std::sync::Arc::new(move |current, total, digest, size| {
        if current == 1 && size > 0 {
            println!("Pulling {}...", image_name);
        }
        let short = &digest[digest.len().saturating_sub(12)..];
        if size < 0 {
            // Negative size signals completion
            let actual_size = -size;
            let size_str = if actual_size >= 1_048_576 {
                format!("{:.1} MB", actual_size as f64 / 1_048_576.0)
            } else if actual_size >= 1024 {
                format!("{:.1} KB", actual_size as f64 / 1024.0)
            } else {
                format!("{} B", actual_size)
            };
            println!("  [{current}/{total}] {short}: {size_str} ✓");
        } else {
            // Positive size means downloading - just show once
            let size_str = if size >= 1_048_576 {
                format!("{:.1} MB", size as f64 / 1_048_576.0)
            } else if size >= 1024 {
                format!("{:.1} KB", size as f64 / 1024.0)
            } else {
                format!("{} B", size)
            };
            println!("  [{current}/{total}] {short}: Pulling {size_str}...");
        }
    })
}

async fn pull_image_config(
    args: &RunArgs,
    progress: a3s_box_runtime::PullProgressFn,
) -> Result<a3s_box_runtime::oci::OciImageConfig, Box<dyn std::error::Error>> {
    let store = std::sync::Arc::new(crate::commands::open_image_store()?);
    let reference = a3s_box_runtime::ImageReference::parse(&args.common.image)?;
    let auth = a3s_box_runtime::RegistryAuth::from_credential_store(&reference.registry);
    let puller =
        a3s_box_runtime::ImagePuller::with_platform(store, auth, args.common.platform.clone())
            .with_progress_fn(progress);
    Ok(puller.pull(&args.common.image).await?.config().clone())
}

fn cleanup_failed_managed_run(box_id: &str) {
    let Ok(state) = StateFile::load_default() else {
        return;
    };
    let Some(record) = state.find_by_id(box_id).cloned() else {
        return;
    };
    if let Err(error) = crate::cleanup::cleanup_removed_box(&record) {
        tracing::warn!(box_id, %error, "Failed to roll back managed run startup");
        return;
    }
    if let Err(error) = StateFile::remove_record(box_id) {
        tracing::warn!(box_id, %error, "Failed to remove rolled-back managed run record");
    }
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

pub(super) fn build_execution_request(
    args: &RunArgs,
    operation_id: &OperationId,
    config: BoxConfig,
    labels: std::collections::BTreeMap<String, String>,
    record: RunRecordPolicy,
) -> CreateExecutionRequest {
    CreateExecutionRequest {
        external_sandbox_id: operation_id.as_str().to_string(),
        config,
        labels,
        policy: ExecutionRecordPolicy {
            name: Some(record.name),
            auto_remove: args.rm,
            restart_policy: record.restart_policy,
            max_restart_count: record.max_restart_count,
            health_check: record.health_check,
            healthcheck_disabled: args.common.no_healthcheck,
            log_config: record.log_config,
            volume_names: record.volume_names,
            platform: args.common.platform.clone(),
            init: args.common.init,
            devices: args.common.device.clone(),
            gpus: args.common.gpus.clone(),
            shm_size: record.shm_size,
            stop_signal: record.stop_signal,
            stop_timeout: args.common.stop_timeout,
            oom_kill_disable: args.common.oom_kill_disable,
            oom_score_adj: args.common.oom_score_adj,
        },
        rootfs_snapshot_id: None,
    }
}

/// Build TeeConfig from run args.
fn build_tee_config(args: &RunArgs) -> TeeConfig {
    if args.tee || args.tee_simulate {
        TeeConfig::SevSnp {
            workload_id: args
                .tee_workload_id
                .clone()
                .unwrap_or_else(|| args.common.image.clone()),
            generation: Default::default(),
            simulate: args.tee_simulate,
        }
    } else {
        TeeConfig::None
    }
}

/// Build BoxConfig from parsed run arguments.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_box_config(
    args: &RunArgs,
    memory_mb: u32,
    resource_limits: a3s_box_core::config::ResourceLimits,
    entrypoint_override: Option<Vec<String>>,
    resolved_volumes: Vec<String>,
    extra_env: Vec<(String, String)>,
    port_map: Vec<String>,
    network: a3s_box_core::NetworkMode,
    tmpfs: Vec<String>,
    tee: TeeConfig,
) -> Result<BoxConfig, String> {
    let (cmd, entrypoint_override) = if args.tty {
        (
            vec!["a3s-box-pty-keepalive".to_string()],
            Some(interactive_keepalive_entrypoint()),
        )
    } else {
        (args.cmd.clone(), entrypoint_override)
    };

    Ok(BoxConfig {
        isolation: common::execution_isolation(&args.common),
        image: args.common.image.clone(),
        resources: ResourceConfig {
            vcpus: args.common.cpus,
            memory_mb,
            ..Default::default()
        },
        cmd,
        stdin_open: args.interactive && !args.no_stdin,
        entrypoint_override,
        user: common::normalize_user_option(args.common.user.as_deref())?,
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
        network,
        tmpfs,
        resource_limits,
        tee,
        read_only: args.common.read_only,
        cap_add: args.common.cap_add.clone(),
        cap_drop: args.common.cap_drop.clone(),
        security_opt: args.common.security_opt.clone(),
        privileged: args.common.privileged,
        sidecar: args.sidecar.as_ref().map(|image| SidecarConfig {
            image: image.clone(),
            vsock_port: args.sidecar_vsock_port,
            env: vec![],
        }),
        // A box without `--rm` survives its stop like a Docker stopped
        // container: keep its dir (logs + overlay upper) so `logs`/`start` work
        // afterwards. `--rm` boxes and CRI pods stay non-persistent (removed on
        // teardown). `rm` force-removes either way (cleanup_removed_box).
        persistent: args.common.persistent || !args.rm,
        ..Default::default()
    })
}

pub(super) fn should_create_diff_baseline(args: &RunArgs) -> bool {
    !args.rm || args.detach
}

/// Initial process used only to keep the guest init alive for `run -it`.
///
/// The actual user command is executed over the PTY after guest control sockets
/// are ready, so short-lived interactive commands do not race the VM shutdown.
pub(super) fn interactive_keepalive_entrypoint() -> Vec<String> {
    vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "trap 'exit 0' TERM INT; while :; do sleep 3600; done".to_string(),
    ]
}
