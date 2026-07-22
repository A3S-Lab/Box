//! `a3s-box run` command — Pull + Create + Start.

use std::io::IsTerminal;
use std::path::PathBuf;

use a3s_box_core::config::{BoxConfig, ResourceConfig, SidecarConfig, TeeConfig};
use a3s_box_core::{
    CreateExecutionRequest, ExecutionGeneration, ExecutionId, ExecutionManager,
    ExecutionRecordPolicy, ExecutionRestartPolicy, ExecutionState, OperationId,
};
use a3s_box_runtime::{LocalExecutionManager, VmLocalExecutionBackend};
use clap::{Args, ValueEnum};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use super::common::{self, CommonBoxArgs};
use super::pool::{
    PoolAutoStartConfig, DEFAULT_AUTOSTART_POOL_MAX, DEFAULT_AUTOSTART_POOL_SIZE, DEFAULT_SOCKET,
};
use crate::output::parse_memory;
use crate::state::{generate_name, BoxRecord, StateFile};
use a3s_box_runtime::pool::PoolClientRun;

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
        crate::health::spawn_detached_health_checker(&ctx.record)
            .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
        println!("{}", ctx.box_id);
        return Ok(());
    }

    ctx.health_checker = match ctx.record.health_check.as_ref() {
        Some(health_check) => Some(crate::health::spawn_health_checker(
            ctx.box_id.clone(),
            ctx.exec_socket_path.clone(),
            health_check.clone(),
        )?),
        None => None,
    };

    if args.tty {
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
    should_create_diff_baseline, RunRecordPolicy,
};

// ============================================================================
// Phase 2a: Interactive PTY mode
// ============================================================================

#[cfg(not(windows))]
async fn run_tty(mut ctx: RunContext, args: &RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    use crate::terminal;
    use a3s_box_core::pty::PtyRequest;

    let pty_socket_path = ctx.pty_socket_path.clone();

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
    let mut client =
        super::exec::connect_pty_with_retry(&pty_socket_path, std::time::Duration::from_secs(10))
            .await?;
    client
        .send_request(&PtyRequest {
            cmd: pty_cmd,
            env,
            working_dir: args.common.workdir.clone(),
            rootfs: None,
            user,
            cols,
            rows,
        })
        .await?;

    let (read_half, write_half) = client.into_split();
    let exit_code = {
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

// ============================================================================
// Phase 2b: Foreground mode (tail logs, wait for exit or Ctrl-C)
// ============================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ForegroundStopReason {
    ProcessExited,
    UserInterrupted(i32),
    VmUnhealthy,
    TimedOut,
}

#[cfg(unix)]
type ForegroundTerminateSignal = Option<tokio::signal::unix::Signal>;
#[cfg(not(unix))]
type ForegroundTerminateSignal = ();

#[cfg(unix)]
const FOREGROUND_SIGINT: i32 = libc::SIGINT;
#[cfg(not(unix))]
const FOREGROUND_SIGINT: i32 = 2;
#[cfg(unix)]
const FOREGROUND_SIGTERM: i32 = libc::SIGTERM;
#[cfg(not(unix))]
const FOREGROUND_SIGTERM: i32 = 15;

const FOREGROUND_LOG_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const FOREGROUND_EXIT_POLL: std::time::Duration = std::time::Duration::from_millis(20);
const FOREGROUND_HEALTH_POLL: std::time::Duration = std::time::Duration::from_millis(500);
const FOREGROUND_LOG_DRAIN_QUIET: std::time::Duration = std::time::Duration::from_millis(50);
const FOREGROUND_LOG_DRAIN_POLL: std::time::Duration = std::time::Duration::from_millis(10);

impl ForegroundStopReason {
    fn stopped_by_user(self) -> bool {
        matches!(self, Self::UserInterrupted(_))
    }
}

#[cfg(unix)]
fn foreground_terminate_signal() -> ForegroundTerminateSignal {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok()
}

#[cfg(not(unix))]
fn foreground_terminate_signal() -> ForegroundTerminateSignal {}

#[cfg(unix)]
async fn recv_foreground_terminate(signal: &mut ForegroundTerminateSignal) {
    if let Some(signal) = signal {
        let _ = signal.recv().await;
    } else {
        std::future::pending::<()>().await;
    }
}

#[cfg(not(unix))]
async fn recv_foreground_terminate(_signal: &mut ForegroundTerminateSignal) {
    std::future::pending::<()>().await;
}

async fn run_foreground(
    mut ctx: RunContext,
    args: &RunArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let foreground_start = std::time::Instant::now();
    println!(
        "Box {} ({}) started. Press Ctrl-C to stop.",
        ctx.name,
        BoxRecord::make_short_id(&ctx.box_id)
    );

    #[cfg(target_os = "windows")]
    let (console_log, console_err) = {
        // WHPX persists workload output in the shared rootfs. The shim tails
        // these files into container.json, while the conventional raw console
        // files only receive a completed-stream fallback after exit.
        let rootfs = ctx.box_dir.join("rootfs");
        (
            rootfs.join("guest-init.stdout.log"),
            rootfs.join("guest-init.stderr.log"),
        )
    };
    #[cfg(not(target_os = "windows"))]
    let (console_log, console_err) = (
        ctx.box_dir.join("logs").join("console.log"),
        ctx.box_dir.join("logs").join("console.err.log"),
    );
    let stdout_pos = Arc::new(AtomicU64::new(0));
    let stderr_pos = Arc::new(AtomicU64::new(0));
    let tail_stdout_pos = Arc::clone(&stdout_pos);
    let tail_stderr_pos = Arc::clone(&stderr_pos);
    let tail_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stdout_tail_stop = Arc::clone(&tail_stop);
    let stderr_tail_stop = Arc::clone(&tail_stop);
    let tail_console_log = console_log.clone();
    let tail_console_err = console_err.clone();
    let runtime_filter = cfg!(target_os = "windows")
        .then(|| Arc::new(a3s_box_core::log::RuntimeConsoleFilter::new()));
    let stdout_runtime_filter = runtime_filter.clone();
    let stderr_runtime_filter = runtime_filter;
    let mut log_handle = tokio::spawn(async move {
        // Stream the selected raw stdout/stderr sources to the terminal.
        tokio::join!(
            super::tail_file_stream_positioned(
                &tail_console_log,
                false,
                Some(tail_stdout_pos),
                Some(stdout_tail_stop),
                stdout_runtime_filter,
            ),
            super::tail_file_stream_positioned(
                &tail_console_err,
                true,
                Some(tail_stderr_pos),
                Some(stderr_tail_stop),
                stderr_runtime_filter,
            ),
        );
    });

    let name = ctx.name.clone();
    let mut terminate_signal = foreground_terminate_signal();
    let timeout_at = args
        .timeout
        .map(|secs| tokio::time::Instant::now() + std::time::Duration::from_secs(secs));
    // Process exit is latency-sensitive for short foreground commands, while a
    // VM health check is comparatively expensive and only needs the existing
    // 500 ms cadence. Keeping independent timers avoids adding a fixed half
    // second to every no-op without polling health more aggressively.
    let mut exit_poll = tokio::time::interval(FOREGROUND_EXIT_POLL);
    exit_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut health_poll = tokio::time::interval_at(
        tokio::time::Instant::now() + FOREGROUND_HEALTH_POLL,
        FOREGROUND_HEALTH_POLL,
    );
    health_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let stop_reason = loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("\nStopping box {}...", name);
                break ForegroundStopReason::UserInterrupted(FOREGROUND_SIGINT);
            }
            _ = recv_foreground_terminate(&mut terminate_signal) => {
                println!("\nStopping box {} after SIGTERM...", name);
                break ForegroundStopReason::UserInterrupted(FOREGROUND_SIGTERM);
            }
            _ = recv_foreground_timeout(timeout_at) => {
                println!("\nStopping box {} after --timeout expired...", name);
                break ForegroundStopReason::TimedOut;
            }
            _ = exit_poll.tick() => {
                if !managed_process_alive(&ctx) {
                    break ForegroundStopReason::ProcessExited;
                }
            }
            _ = health_poll.tick() => {
                if !managed_runtime_healthy(&ctx).await {
                    break ForegroundStopReason::VmUnhealthy;
                }
            }
        }
    };
    a3s_box_core::lifecycle_profile::record_lifecycle_phase(
        "foreground.command_execution",
        foreground_start.elapsed(),
    );

    let sandbox_natural_exit =
        stop_reason == ForegroundStopReason::ProcessExited && ctx.record.isolation.is_sandbox();
    if sandbox_natural_exit {
        // The generation-owned worker exits only after crun has closed both
        // raw console streams and projected their final records. Once it is
        // gone, the terminal tailers can catch up to immutable file lengths
        // without an additional writer-quiet grace period.
        let structured_log_drain_start = std::time::Instant::now();
        wait_for_sandbox_structured_log_drain(&ctx).await?;
        a3s_box_core::lifecycle_profile::record_lifecycle_phase(
            "foreground.structured_log_drain",
            structured_log_drain_start.elapsed(),
        );
    }

    let raw_log_drain_start = std::time::Instant::now();
    wait_for_foreground_log_drain(
        &[(&console_log, &stdout_pos), (&console_err, &stderr_pos)],
        sandbox_natural_exit,
    )
    .await;
    a3s_box_core::lifecycle_profile::record_lifecycle_phase(
        "foreground.raw_log_drain",
        raw_log_drain_start.elapsed(),
    );
    tail_stop.store(true, Ordering::Release);
    if tokio::time::timeout(std::time::Duration::from_secs(1), &mut log_handle)
        .await
        .is_err()
    {
        log_handle.abort();
    }

    if stop_reason == ForegroundStopReason::ProcessExited && !sandbox_natural_exit {
        let structured_log_drain_start = std::time::Instant::now();
        wait_for_sandbox_structured_log_drain(&ctx).await?;
        a3s_box_core::lifecycle_profile::record_lifecycle_phase(
            "foreground.structured_log_drain",
            structured_log_drain_start.elapsed(),
        );
    }

    let persisted_exit_code = a3s_box_runtime::rootfs::read_persisted_exit_code(&ctx.box_dir);
    let exit_code = foreground_exit_code(stop_reason, persisted_exit_code);
    let archive_start = std::time::Instant::now();
    archive_auto_removed_logs(&ctx, args.rm, exit_code, stop_reason.stopped_by_user());
    a3s_box_core::lifecycle_profile::record_lifecycle_phase(
        "foreground.archive",
        archive_start.elapsed(),
    );
    cleanup_managed_execution(
        &mut ctx,
        args.rm,
        exit_code,
        stop_reason.stopped_by_user(),
        stop_reason == ForegroundStopReason::ProcessExited,
    )
    .await?;
    println!(
        "{}",
        foreground_completion_message(stop_reason, args.rm, &ctx.name)
    );

    if let Some(code) = exit_code {
        if code != 0 {
            std::process::exit(code);
        }
    }

    Ok(())
}

async fn wait_for_sandbox_structured_log_drain(
    ctx: &RunContext,
) -> Result<(), Box<dyn std::error::Error>> {
    if !ctx.record.isolation.is_sandbox() {
        return Ok(());
    }
    let box_dir = ctx.box_dir.clone();
    let box_id = ctx.box_id.clone();
    let drained = tokio::task::spawn_blocking(move || {
        a3s_box_runtime::vm::reap::wait_for_recorded_sandbox_log_drain(
            &box_dir,
            &box_id,
            std::time::Duration::from_secs(3),
        )
    })
    .await
    .map_err(|error| format!("Sandbox log drain task failed for {}: {error}", ctx.box_id))??;
    if !drained {
        return Err(format!(
            "Sandbox logs did not finish draining for {}; state was preserved for recovery",
            ctx.box_id
        )
        .into());
    }
    Ok(())
}

async fn recv_foreground_timeout(deadline: Option<tokio::time::Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline).await;
    } else {
        std::future::pending::<()>().await;
    }
}

async fn wait_for_foreground_log_drain(
    paths: &[(&std::path::Path, &AtomicU64)],
    writers_finished: bool,
) {
    let start = std::time::Instant::now();
    let mut last_lens = foreground_log_lengths(paths);
    let mut quiet_since = None;

    loop {
        let lens = foreground_log_lengths(paths);
        let lengths_stable = lens == last_lens;
        let tails_caught_up = paths
            .iter()
            .zip(lens.iter())
            .all(|((_, pos), len)| pos.load(Ordering::Relaxed) >= *len);

        if writers_finished && tails_caught_up {
            break;
        }

        if lengths_stable && tails_caught_up {
            let now = std::time::Instant::now();
            match quiet_since {
                Some(since) if now.duration_since(since) >= FOREGROUND_LOG_DRAIN_QUIET => break,
                Some(_) => {}
                None => quiet_since = Some(now),
            }
        } else {
            last_lens = lens;
            quiet_since = None;
        }

        if start.elapsed() >= FOREGROUND_LOG_DRAIN_TIMEOUT {
            break;
        }

        tokio::time::sleep(FOREGROUND_LOG_DRAIN_POLL).await;
    }
}

fn foreground_log_lengths(paths: &[(&std::path::Path, &AtomicU64)]) -> Vec<u64> {
    paths
        .iter()
        .map(|(path, _)| std::fs::metadata(path).map(|m| m.len()).unwrap_or(0))
        .collect()
}

fn foreground_exit_code(reason: ForegroundStopReason, vm_exit_code: Option<i32>) -> Option<i32> {
    match reason {
        // A dead runtime without a persisted guest result is not evidence of a
        // successful command. This happens when the VM/shim fails before
        // guest-init can write `.a3s_exit_code`; returning `None` here used to
        // make foreground `run --rm` fall through to CLI exit status 0.
        ForegroundStopReason::ProcessExited => vm_exit_code.or(Some(1)),
        ForegroundStopReason::UserInterrupted(signal) => vm_exit_code.or(Some(128 + signal)),
        ForegroundStopReason::VmUnhealthy => vm_exit_code.or(Some(1)),
        ForegroundStopReason::TimedOut => Some(124),
    }
}

fn managed_process_alive(ctx: &RunContext) -> bool {
    ctx.record.pid.is_some_and(|pid| {
        a3s_box_runtime::is_process_alive_with_identity(pid, ctx.record.pid_start_time)
    })
}

#[cfg(unix)]
async fn managed_runtime_healthy(ctx: &RunContext) -> bool {
    if !managed_process_alive(ctx) {
        return false;
    }
    let probe = async {
        let client = a3s_box_runtime::ExecClient::connect(&ctx.exec_socket_path)
            .await
            .ok()?;
        client.heartbeat().await.ok().filter(|ready| *ready)
    };
    tokio::time::timeout(std::time::Duration::from_millis(500), probe)
        .await
        .ok()
        .flatten()
        .is_some()
}

#[cfg(not(unix))]
async fn managed_runtime_healthy(ctx: &RunContext) -> bool {
    managed_process_alive(ctx)
}

fn foreground_completion_message(
    reason: ForegroundStopReason,
    auto_remove: bool,
    name: &str,
) -> String {
    match (reason, auto_remove) {
        (ForegroundStopReason::ProcessExited, true) => {
            format!("Box {name} exited and was removed.")
        }
        (ForegroundStopReason::ProcessExited, false) => format!("Box {name} exited."),
        (ForegroundStopReason::UserInterrupted(_), true) => format!("Box {name} removed."),
        (ForegroundStopReason::UserInterrupted(_), false) => format!("Box {name} stopped."),
        (ForegroundStopReason::VmUnhealthy, true) => {
            format!("Box {name} stopped after VM health check failed and was removed.")
        }
        (ForegroundStopReason::VmUnhealthy, false) => {
            format!("Box {name} stopped after VM health check failed.")
        }
        (ForegroundStopReason::TimedOut, true) => {
            format!("Box {name} stopped after --timeout expired and was removed.")
        }
        (ForegroundStopReason::TimedOut, false) => {
            format!("Box {name} stopped after --timeout expired.")
        }
    }
}

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
            crate::cleanup::cleanup_anonymous_volumes(&ctx.anonymous_volumes);
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
