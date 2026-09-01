//! `a3s-box run` command — Pull + Create + Start.

use std::io::IsTerminal;
use std::path::PathBuf;

use super::common::{self, CommonBoxArgs};
use super::pool::{
    PoolAutoStartConfig, DEFAULT_AUTOSTART_POOL_MAX, DEFAULT_AUTOSTART_POOL_SIZE, DEFAULT_SOCKET,
};
use crate::output::parse_memory;
use crate::state::{generate_name, BoxRecord, StateFile};
use a3s_box_core::config::{BoxConfig, ResourceConfig, SidecarConfig, TeeConfig};
use a3s_box_core::{
    CreateExecutionRequest, ExecutionGeneration, ExecutionId, ExecutionManager,
    ExecutionRecordPolicy, ExecutionRestartPolicy, ExecutionState, OperationId,
};
use a3s_box_runtime::pool::PoolClientRun;
use a3s_box_runtime::LocalExecutionManager;
use clap::{Args, ValueEnum};

const PNPM_CACHE_VOLUME_SPEC: &str = "a3s-cache-pnpm:/a3s-cache/pnpm";
const PNPM_CONFIG_STORE_ENV: &str = "PNPM_CONFIG_STORE_DIR";
const PNPM_STORE_ENV: &str = "npm_config_store_dir";
const PNPM_STORE_DIR: &str = "/a3s-cache/pnpm/store";
const PNPM_COREPACK_HOME_ENV: &str = "COREPACK_HOME";
const PNPM_COREPACK_HOME_DIR: &str = "/a3s-cache/pnpm/corepack";
const PNPM_HOME_ENV: &str = "PNPM_HOME";
const PNPM_HOME_DIR: &str = "/a3s-cache/pnpm/home";
const PNPM_NPM_CACHE_ENV: &str = NPM_CACHE_ENV;
const PNPM_NPM_CACHE_DIR: &str = "/a3s-cache/pnpm/npm-cache";
const PNPM_CONFIG_PREFER_OFFLINE_ENV: &str = "PNPM_CONFIG_PREFER_OFFLINE";
const PNPM_PREFER_OFFLINE_ENV: &str = NPM_PREFER_OFFLINE_ENV;
const PNPM_PREFER_OFFLINE_VALUE: &str = NPM_PREFER_OFFLINE_VALUE;
const NPM_CACHE_VOLUME_SPEC: &str = "a3s-cache-npm:/a3s-cache/npm";
const NPM_CACHE_ENV: &str = "npm_config_cache";
const NPM_CACHE_DIR: &str = "/a3s-cache/npm/cache";
const NPM_PREFER_OFFLINE_ENV: &str = "npm_config_prefer_offline";
const NPM_PREFER_OFFLINE_VALUE: &str = "true";
const COREPACK_DOWNLOAD_PROMPT_ENV: &str = "COREPACK_ENABLE_DOWNLOAD_PROMPT";
const COREPACK_DOWNLOAD_PROMPT_VALUE: &str = "0";
const RUN_POOL_SOCKET_ENV: &str = "A3S_BOX_RUN_POOL_SOCKET";

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum PackageCache {
    Pnpm,
    Npm,
}

#[derive(Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub common: CommonBoxArgs,

    /// Run in detached mode (background)
    #[arg(short = 'd', long)]
    pub detach: bool,

    /// Keep STDIN open (interactive mode)
    #[arg(short = 'i', long = "interactive")]
    pub interactive: bool,

    /// Close STDIN for the guest command
    #[arg(long)]
    pub no_stdin: bool,

    /// Allocate a pseudo-TTY
    #[arg(short = 't', long = "tty")]
    pub tty: bool,

    /// Stop the box if the foreground run exceeds this many seconds
    #[arg(long, value_name = "SECONDS")]
    pub timeout: Option<u64>,

    /// Automatically remove the box when it stops
    #[arg(long)]
    pub rm: bool,

    /// Run the command through the warm-pool daemon instead of cold-starting a box.
    ///
    /// Pool mode is currently for foreground one-shot commands (`--rm`) and
    /// supports image/user/workdir/env/volumes/resources/package-cache/timeout.
    #[arg(long)]
    pub pool: bool,

    /// Unix socket of the warm-pool daemon used by `--pool`.
    #[arg(long = "pool-socket", default_value = DEFAULT_SOCKET)]
    pub pool_socket: String,

    /// Start a warm-pool daemon on --pool-socket when one is not already running.
    #[arg(long = "pool-autostart")]
    pub pool_autostart: bool,

    /// Force exec mode against a deferred pool daemon.
    #[arg(long = "pool-exec")]
    pub pool_exec: bool,

    /// Mount a persistent package-manager cache (pnpm or npm)
    #[arg(long = "package-cache", value_enum)]
    pub package_cache: Vec<PackageCache>,

    /// Command to run (override entrypoint)
    #[arg(last = true)]
    pub cmd: Vec<String>,

    /// Logging driver (json-file, none) [default: json-file]
    #[arg(long, default_value = "json-file")]
    pub log_driver: String,

    /// Log driver options (KEY=VALUE), can be repeated
    #[arg(long = "log-opt")]
    pub log_opts: Vec<String>,

    /// Enable TEE (Trusted Execution Environment) with AMD SEV-SNP.
    /// Use --tee-simulate for development without hardware support.
    #[arg(long)]
    pub tee: bool,

    /// TEE workload identifier for attestation (default: image name)
    #[arg(long)]
    pub tee_workload_id: Option<String>,

    /// Enable TEE simulation mode (no AMD SEV-SNP hardware required)
    #[arg(long)]
    pub tee_simulate: bool,

    /// Sidecar OCI image to run alongside the main container inside the VM.
    /// Intended for security proxies such as SafeClaw.
    /// Example: --sidecar ghcr.io/a3s-lab/safeclaw:latest
    #[arg(long)]
    pub sidecar: Option<String>,

    /// Vsock port for the sidecar process (default: 4092)
    #[arg(long, default_value = "4092")]
    pub sidecar_vsock_port: u32,
}

/// Intermediate state produced by the setup phase, consumed by the run phase.
struct RunContext {
    manager: LocalExecutionManager,
    execution_id: ExecutionId,
    generation: ExecutionGeneration,
    box_id: String,
    box_dir: PathBuf,
    name: String,
    record: BoxRecord,
    exec_socket_path: PathBuf,
    #[cfg_attr(windows, allow(dead_code))]
    pty_socket_path: PathBuf,
    anonymous_volumes: Vec<String>,
    health_checker: Option<tokio::task::JoinHandle<()>>,
    completed_during_start: bool,
}

pub(super) fn is_completed_managed_start(record: &BoxRecord) -> bool {
    record.exit_code.is_some()
        && record
            .managed_state()
            .is_ok_and(|state| state == Some(a3s_box_runtime::ManagedExecutionState::Stopped))
}

pub async fn execute(args: RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    validate_run_mode(&args, std::io::stdin().is_terminal())
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let env_pool_socket = std::env::var(RUN_POOL_SOCKET_ENV).ok();
    if let Some(pool_socket) = selected_pool_socket(&args, env_pool_socket.as_deref()) {
        if args.pool_autostart {
            super::pool::ensure_pool_daemon_running(&pool_autostart_config_for_run(
                &args,
                &pool_socket,
            )?)
            .await?;
        }
        return execute_pool_run(&args, &pool_socket).await;
    }

    let mut ctx = setup_and_boot(&args).await?;
    crate::audit::record(
        a3s_box_core::audit::AuditAction::BoxStart,
        a3s_box_core::audit::AuditOutcome::Success,
        &ctx.box_id,
        &format!("started box from image {}", args.common.image),
    );
    if args.detach {
        if !ctx.completed_during_start {
            crate::health::spawn_detached_health_checker(&ctx.record)
                .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
        }
        println!("{}", ctx.box_id);
        return Ok(());
    }

    ctx.health_checker = match (ctx.completed_during_start, ctx.record.health_check.as_ref()) {
        (false, Some(health_check)) => Some(crate::health::spawn_health_checker(
            ctx.box_id.clone(),
            ctx.exec_socket_path.clone(),
            health_check.clone(),
        )?),
        _ => None,
    };

    if args.tty && !ctx.completed_during_start {
        return run_tty(ctx, &args).await;
    }

    run_foreground(ctx, &args).await
}

fn validate_run_mode(args: &RunArgs, stdin_is_terminal: bool) -> Result<(), &'static str> {
    if args.detach && args.tty {
        return Err("Cannot use -t (tty) with -d (detach)");
    }
    if args.interactive && args.no_stdin {
        return Err("Cannot use --interactive with --no-stdin");
    }
    if args.timeout.is_some() && args.detach {
        return Err("Cannot use --timeout with -d (detach)");
    }
    if args.timeout.is_some() && args.tty {
        return Err("Cannot use --timeout with -t (tty)");
    }
    if matches!(args.timeout, Some(0)) {
        return Err("--timeout must be greater than zero seconds");
    }
    if args.tty && !stdin_is_terminal {
        return Err("The -t flag requires a terminal (stdin is not a TTY)");
    }
    if args.pool || args.pool_autostart {
        validate_pool_run_mode(args)?;
    }
    Ok(())
}

fn validate_pool_run_mode(args: &RunArgs) -> Result<(), &'static str> {
    match pool_run_mode_error(args) {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn pool_run_mode_error(args: &RunArgs) -> Option<&'static str> {
    if !args.rm {
        return Some("--pool currently requires --rm");
    }
    if args.detach {
        return Some("Cannot use --pool with -d (detach)");
    }
    if args.tty {
        return Some("Cannot use --pool with -t (tty)");
    }
    if args.interactive {
        return Some("Cannot use --pool with --interactive");
    }
    if args.cmd.is_empty() {
        return Some("--pool currently requires an explicit command");
    }
    if has_unsupported_pool_common_options(&args.common)
        || args.log_driver != "json-file"
        || !args.log_opts.is_empty()
        || args.tee
        || args.tee_simulate
        || args.tee_workload_id.is_some()
        || args.sidecar.is_some()
    {
        return Some("--pool currently supports only image, --rm, command, --user, --workdir, --env, --env-file, --volume, --cpus, --memory, --timeout, and --package-cache");
    }
    None
}

fn selected_pool_socket(args: &RunArgs, env_socket: Option<&str>) -> Option<String> {
    if args.pool || args.pool_autostart {
        return Some(args.pool_socket.clone());
    }
    let socket = env_socket
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    if pool_run_mode_error(args).is_none() {
        Some(socket.to_string())
    } else {
        None
    }
}

fn pool_autostart_config_for_run(
    args: &RunArgs,
    socket: &str,
) -> Result<PoolAutoStartConfig, Box<dyn std::error::Error>> {
    let memory_mb =
        parse_memory(&args.common.memory).map_err(|e| format!("Invalid --memory: {e}"))?;
    let prewarm_image = if args.common.volumes.is_empty()
        && args.package_cache.is_empty()
        && args.common.cpus == 2
        && memory_mb == 512
    {
        Some(args.common.image.clone())
    } else {
        None
    };

    Ok(PoolAutoStartConfig {
        socket: socket.to_string(),
        image: prewarm_image,
        size: DEFAULT_AUTOSTART_POOL_SIZE,
        max: DEFAULT_AUTOSTART_POOL_MAX,
    })
}

fn has_unsupported_pool_common_options(common: &CommonBoxArgs) -> bool {
    common.name.is_some()
        || !common.publish.is_empty()
        || !common.dns.is_empty()
        || common.entrypoint.is_some()
        || common.hostname.is_some()
        || common.restart != "no"
        || !common.labels.is_empty()
        || !common.tmpfs.is_empty()
        || common.virtiofs_cache.is_some()
        || common.network.is_some()
        || common.health_cmd.is_some()
        || common.health_interval != 30
        || common.health_timeout != 5
        || common.health_retries != 3
        || common.health_start_period != 0
        || common.pids_limit.is_some()
        || common.cpuset_cpus.is_some()
        || !common.ulimits.is_empty()
        || common.cpu_shares.is_some()
        || common.cpu_quota.is_some()
        || common.cpu_period.is_some()
        || common.memory_reservation.is_some()
        || common.memory_swap.is_some()
        || !common.add_host.is_empty()
        || common.platform.is_some()
        || common.init
        || common.read_only
        || !common.cap_add.is_empty()
        || !common.cap_drop.is_empty()
        || !common.security_opt.is_empty()
        || common.privileged
        || !common.device.is_empty()
        || common.gpus.is_some()
        || common.shm_size.is_some()
        || common.stop_signal.is_some()
        || common.stop_timeout.is_some()
        || common.no_healthcheck
        || common.oom_kill_disable
        || common.oom_score_adj.is_some()
        || common.persistent
}

async fn execute_pool_run(args: &RunArgs, socket: &str) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;

    let output =
        a3s_box_runtime::pool::client::run_client(build_pool_client_run(args, socket)?).await?;

    std::io::stdout().write_all(&output.stdout)?;
    std::io::stderr().write_all(&output.stderr)?;
    if output.exit_code != 0 {
        std::process::exit(output.exit_code);
    }
    Ok(())
}

fn build_pool_client_run(
    args: &RunArgs,
    socket: &str,
) -> Result<PoolClientRun, Box<dyn std::error::Error>> {
    common::validate_runtime_options(&args.common)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let memory_mb =
        parse_memory(&args.common.memory).map_err(|e| format!("Invalid --memory: {e}"))?;
    let mut env = common::build_env_map(&args.common)?;
    apply_run_env_defaults(args, &mut env);
    let mut volume_specs = args.common.volumes.clone();
    apply_package_caches(&args.package_cache, &mut volume_specs, &mut env);
    let (resolved_volumes, _) = resolve_volumes(&volume_specs)?;
    let mut env_entries: Vec<String> = env
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    env_entries.sort();

    Ok(PoolClientRun {
        socket: socket.to_string(),
        image: Some(args.common.image.clone()),
        user: common::normalize_user_option(args.common.user.as_deref())
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?,
        workdir: args.common.workdir.clone(),
        rootfs: None,
        env: env_entries,
        volumes: resolved_volumes,
        vcpus: args.common.cpus,
        memory_mb,
        exec: args.pool_exec,
        timeout_ns: args.timeout.map(|secs| secs.saturating_mul(1_000_000_000)),
        cmd: args.cmd.clone(),
    })
}

mod setup;

use setup::setup_and_boot;
#[cfg(test)]
use setup::{
    build_box_config, build_execution_request, interactive_keepalive_entrypoint,
    runtime_start_progress_message, should_create_diff_baseline, RunRecordPolicy,
};

// ============================================================================
// Phase 2a: Interactive PTY mode
// ============================================================================

#[cfg(not(windows))]
async fn run_tty(mut ctx: RunContext, args: &RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    use crate::terminal;
    use a3s_box_core::pty::PtyRequest;
    use a3s_box_core::ExecutionSessionManager;

    let entrypoint_override = args
        .common
        .entrypoint
        .as_ref()
        .map(|ep| ep.split_whitespace().map(String::from).collect::<Vec<_>>());

    let pty_cmd = if !args.cmd.is_empty() {
        args.cmd.clone()
    } else if let Some(ref ep) = entrypoint_override {
        ep.clone()
    } else {
        vec!["/bin/sh".to_string()]
    };

    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let user = common::normalize_user_option(args.common.user.as_deref())
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let env = common::build_env_map(&args.common)?
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    let request = PtyRequest {
        cmd: pty_cmd,
        env,
        working_dir: args.common.workdir.clone(),
        rootfs: None,
        user,
        cols,
        rows,
    };
    let exit_code = if run_context_uses_oci(&ctx) {
        let process = ctx
            .manager
            .start_pty(&ctx.execution_id, ctx.generation, request)
            .await?;
        let _raw_mode = terminal::raw_mode()?;
        super::exec::run_managed_pty_session(process).await
    } else {
        let mut client = super::exec::connect_pty_with_retry(
            &ctx.pty_socket_path,
            std::time::Duration::from_secs(10),
        )
        .await?;
        client.send_request(&request).await?;
        let (read_half, write_half) = client.into_split();
        let _raw_mode = terminal::raw_mode()?;
        super::exec::run_pty_session(read_half, write_half).await
    };

    // Cleanup
    cleanup_box(&mut ctx, args.rm, Some(exit_code)).await?;

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

#[cfg(windows)]
async fn run_tty(_ctx: RunContext, _args: &RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    Err(crate::platform::unsupported_command(
        "run -it",
        "interactive PTY support",
    ))
}

mod foreground;

#[cfg(test)]
use foreground::{
    foreground_completion_message, foreground_exit_code, foreground_health_stop_reason,
    foreground_workload_exit_code, wait_for_foreground_log_drain, ForegroundStopReason,
    FOREGROUND_EXIT_POLL, FOREGROUND_HEALTH_POLL, FOREGROUND_LOG_DRAIN_POLL,
    FOREGROUND_LOG_DRAIN_QUIET, FOREGROUND_SIGINT, FOREGROUND_SIGTERM,
};
use foreground::{run_context_uses_oci, run_foreground};

// ============================================================================
// Shared helpers
// ============================================================================

/// Parse health check config from common args.
#[cfg(test)]
fn parse_health_check(common: &common::CommonBoxArgs) -> Option<crate::state::HealthCheck> {
    common::effective_health_check(common, None)
}

/// Resolve named volumes, returning (resolved_specs, volume_names).
fn resolve_volumes(
    volume_specs: &[String],
) -> Result<(Vec<String>, Vec<String>), Box<dyn std::error::Error>> {
    let mut resolved = Vec::new();
    let mut names = Vec::new();
    for spec in volume_specs {
        let (r, vol_name) = super::volume::resolve_named_volume(spec)?;
        if let Some(name) = vol_name {
            names.push(name);
        }
        resolved.push(r);
    }
    Ok((resolved, names))
}

fn apply_package_caches(
    caches: &[PackageCache],
    volume_specs: &mut Vec<String>,
    env: &mut std::collections::HashMap<String, String>,
) {
    for cache in caches {
        match cache {
            PackageCache::Pnpm => {
                ensure_package_cache_volume(volume_specs, PNPM_CACHE_VOLUME_SPEC);
                env.entry(PNPM_CONFIG_STORE_ENV.to_string())
                    .or_insert_with(|| PNPM_STORE_DIR.to_string());
                env.entry(PNPM_STORE_ENV.to_string())
                    .or_insert_with(|| PNPM_STORE_DIR.to_string());
                env.entry(PNPM_COREPACK_HOME_ENV.to_string())
                    .or_insert_with(|| PNPM_COREPACK_HOME_DIR.to_string());
                env.entry(PNPM_HOME_ENV.to_string())
                    .or_insert_with(|| PNPM_HOME_DIR.to_string());
                env.entry(PNPM_NPM_CACHE_ENV.to_string())
                    .or_insert_with(|| PNPM_NPM_CACHE_DIR.to_string());
                env.entry(PNPM_CONFIG_PREFER_OFFLINE_ENV.to_string())
                    .or_insert_with(|| PNPM_PREFER_OFFLINE_VALUE.to_string());
                env.entry(PNPM_PREFER_OFFLINE_ENV.to_string())
                    .or_insert_with(|| PNPM_PREFER_OFFLINE_VALUE.to_string());
                env.entry(COREPACK_DOWNLOAD_PROMPT_ENV.to_string())
                    .or_insert_with(|| COREPACK_DOWNLOAD_PROMPT_VALUE.to_string());
            }
            PackageCache::Npm => {
                ensure_package_cache_volume(volume_specs, NPM_CACHE_VOLUME_SPEC);
                env.entry(NPM_CACHE_ENV.to_string())
                    .or_insert_with(|| NPM_CACHE_DIR.to_string());
                env.entry(NPM_PREFER_OFFLINE_ENV.to_string())
                    .or_insert_with(|| NPM_PREFER_OFFLINE_VALUE.to_string());
            }
        }
    }
}

fn apply_run_env_defaults(args: &RunArgs, env: &mut std::collections::HashMap<String, String>) {
    if !args.interactive {
        env.entry(COREPACK_DOWNLOAD_PROMPT_ENV.to_string())
            .or_insert_with(|| COREPACK_DOWNLOAD_PROMPT_VALUE.to_string());
    }
}

fn ensure_package_cache_volume(volume_specs: &mut Vec<String>, volume_spec: &str) {
    if !volume_specs.iter().any(|spec| spec == volume_spec) {
        volume_specs.push(volume_spec.to_string());
    }
}

/// Shared cleanup: stop the managed execution and update retained state.
#[cfg(not(windows))]
async fn cleanup_box(
    ctx: &mut RunContext,
    auto_remove: bool,
    exit_code: Option<i32>,
) -> Result<(), Box<dyn std::error::Error>> {
    archive_auto_removed_logs(ctx, auto_remove, exit_code, false);
    cleanup_managed_execution(ctx, auto_remove, exit_code, false, false).await
}

async fn cleanup_managed_execution(
    ctx: &mut RunContext,
    auto_remove: bool,
    exit_code: Option<i32>,
    stopped_by_user: bool,
    natural_exit: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(ref handle) = ctx.health_checker {
        handle.abort();
    }

    let manager_reconcile_start = std::time::Instant::now();
    let cleanup_result = if natural_exit {
        match ctx.manager.inspect(&ctx.execution_id).await {
            Ok(status)
                if matches!(
                    status.state,
                    ExecutionState::Stopped | ExecutionState::Failed
                ) =>
            {
                Ok(())
            }
            Ok(_) => ctx
                .manager
                .kill(&ctx.execution_id, ctx.generation)
                .await
                .map(|_| ()),
            Err(_) => ctx
                .manager
                .kill(&ctx.execution_id, ctx.generation)
                .await
                .map(|_| ()),
        }
    } else {
        ctx.manager
            .kill(&ctx.execution_id, ctx.generation)
            .await
            .map(|_| ())
    };

    cleanup_result.map_err(|error| {
        format!(
            "failed to stop managed execution {}; state was preserved for recovery: {error}",
            ctx.box_id
        )
    })?;
    a3s_box_core::lifecycle_profile::record_lifecycle_phase(
        "foreground.manager_reconcile",
        manager_reconcile_start.elapsed(),
    );

    let removal_start = std::time::Instant::now();
    if auto_remove {
        StateFile::remove_record(&ctx.box_id)
            .map_err(|error| format!("failed to remove box {} state: {error}", ctx.box_id))?;
        if natural_exit {
            // Explicit managed kills remove auto-remove anonymous volumes in the
            // backend. Natural exit has no kill path, so the CLI owns cleanup.
            crate::cleanup::cleanup_anonymous_volumes(&ctx.box_id, &ctx.anonymous_volumes);
        }
        if let Err(error) = std::fs::remove_dir_all(&ctx.box_dir) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(format!(
                    "removed box {} state but failed to remove {}: {error}",
                    ctx.box_id,
                    ctx.box_dir.display()
                )
                .into());
            }
        }
    } else {
        StateFile::modify(|s| {
            mark_record_stopped(s, &ctx.box_id, exit_code, stopped_by_user);
            Ok::<(), std::io::Error>(())
        })
        .map_err(|error| format!("failed to mark box {} stopped: {error}", ctx.box_id))?;
    }
    a3s_box_core::lifecycle_profile::record_lifecycle_phase(
        "foreground.removal",
        removal_start.elapsed(),
    );

    Ok(())
}

fn archive_auto_removed_logs(
    ctx: &RunContext,
    auto_remove: bool,
    exit_code: Option<i32>,
    stopped_by_user: bool,
) {
    if !auto_remove {
        return;
    }

    let archive_record = stopped_record_for_archive(&ctx.record, exit_code, stopped_by_user);
    match crate::log_archive::archive_removed_logs(&archive_record) {
        Ok(Some(path)) => {
            if should_print_retained_log_hint(exit_code, stopped_by_user) {
                eprintln!(
                    "Retained logs for removed box {} at {}. View with: a3s-box logs {}",
                    ctx.name,
                    path.display(),
                    ctx.name
                );
            }
        }
        Ok(None) => {}
        Err(error) => {
            tracing::debug!(
                box_id = %ctx.box_id,
                error = %error,
                "Failed to archive auto-removed box logs"
            );
        }
    }
}

fn should_print_retained_log_hint(exit_code: Option<i32>, stopped_by_user: bool) -> bool {
    matches!(exit_code, Some(code) if code != 0) && !stopped_by_user
}

fn stopped_record_for_archive(
    record: &BoxRecord,
    exit_code: Option<i32>,
    stopped_by_user: bool,
) -> BoxRecord {
    let mut record = record.clone();
    record.status = "stopped".to_string();
    record.pid = None;
    record.exit_code = exit_code;
    record.stopped_by_user = stopped_by_user;
    record
}

fn mark_record_stopped(
    state: &mut StateFile,
    box_id: &str,
    exit_code: Option<i32>,
    stopped_by_user: bool,
) {
    if let Some(rec) = state.find_by_id_mut(box_id) {
        rec.status = "stopped".to_string();
        rec.pid = None;
        rec.exit_code = exit_code;
        rec.stopped_by_user = stopped_by_user;
    }
}

#[cfg(test)]
#[path = "run/tests.rs"]
mod tests;
