//! `a3s-box pool` — Warm VM pool daemon + client.
//!
//! Pre-boots keepalive MicroVMs of one image so a command can run in an
//! already-ready sandbox instead of paying a full cold boot. `pool start` is the
//! daemon (pre-warms a pool and serves requests over a Unix socket); `pool run`
//! is the client (runs a command in a fresh warm sandbox via the guest exec
//! server, no cold boot). This is the low-risk keepalive+exec MVP from
//! docs/cow-snapshot-fork-design.md — it removes cold boot from the hot path
//! without touching guest-init's lifecycle.
//!
//! Subcommands:
//!   pool start --image IMAGE --size N [--socket P]   Daemon: pre-warm + serve
//!   pool run [--socket P] -- CMD...                  Client: run CMD in a sandbox
//!   pool stop / pool status                          Discoverability helpers

use a3s_box_core::error::BoxError;
use clap::{Parser, Subcommand};

#[cfg(not(windows))]
use a3s_box_runtime::pool::client::{
    read_frame_with_timeout, run_client, stop_client, write_frame, FRAME_READ_TIMEOUT,
};
#[cfg(not(windows))]
use a3s_box_runtime::pool::{
    start_pool_daemon, PoolClientRun, PoolDaemonReport, PoolRequest, PoolStatusResponse,
};
use a3s_box_runtime::pool::{
    validate_pool_daemon_config, PoolDaemonConfig, PoolStats, DEFAULT_POOL_BOOT_CONCURRENCY,
    DEFAULT_POOL_LEASE_TTL_SECS, DEFAULT_POOL_MEMORY, DEFAULT_POOL_VCPUS,
};
#[cfg(not(windows))]
use a3s_box_runtime::vm::reap::reap_orphaned_boxes_for_home;

/// Default Unix socket the `pool` daemon listens on.
pub(crate) const DEFAULT_SOCKET: &str = "/tmp/a3s-box-pool.sock";

pub(crate) const DEFAULT_AUTOSTART_POOL_SIZE: usize = 1;
pub(crate) const DEFAULT_AUTOSTART_POOL_MAX: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PoolAutoStartConfig {
    pub socket: String,
    pub image: Option<String>,
    pub size: usize,
    pub max: usize,
}

impl PoolAutoStartConfig {
    #[cfg(any(not(windows), test))]
    fn start_args(&self) -> Vec<String> {
        let mut args = vec![
            "pool".to_string(),
            "start".to_string(),
            "--socket".to_string(),
            self.socket.clone(),
            "--size".to_string(),
            self.size.to_string(),
            "--max".to_string(),
            self.max.to_string(),
        ];
        if let Some(image) = &self.image {
            args.push("--image".to_string());
            args.push(image.clone());
        }
        args
    }
}

#[cfg(unix)]
struct PoolAutoStartLock {
    _file: std::fs::File,
}

#[cfg(unix)]
impl PoolAutoStartLock {
    fn acquire(socket: &str) -> std::io::Result<Self> {
        use std::os::unix::io::AsRawFd;

        let lock_path = pool_autostart_lock_path(socket);
        if let Some(parent) = lock_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { _file: file })
    }
}

#[cfg(unix)]
fn pool_autostart_lock_path(socket: &str) -> std::path::PathBuf {
    let mut path = std::ffi::OsString::from(socket);
    path.push(".autostart.lock");
    std::path::PathBuf::from(path)
}

#[cfg(not(windows))]
pub(crate) async fn ensure_pool_daemon_running(
    config: &PoolAutoStartConfig,
) -> Result<(), BoxError> {
    if a3s_box_runtime::pool::client::status_client(&config.socket)
        .await
        .is_ok()
    {
        return Ok(());
    }

    #[cfg(unix)]
    let _autostart_lock = PoolAutoStartLock::acquire(&config.socket)?;

    if a3s_box_runtime::pool::client::status_client(&config.socket)
        .await
        .is_ok()
    {
        return Ok(());
    }

    if let Some(parent) = std::path::Path::new(&config.socket)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }

    let exe = std::env::current_exe()?;
    let mut child = std::process::Command::new(exe)
        .args(config.start_args())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| {
            BoxError::PoolError(format!("Failed to auto-start warm-pool daemon: {error}"))
        })?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while std::time::Instant::now() < deadline {
        if a3s_box_runtime::pool::client::status_client(&config.socket)
            .await
            .is_ok()
        {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            return Err(BoxError::PoolError(format!(
                "Auto-started warm-pool daemon exited early: {status}"
            )));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    Err(BoxError::TimeoutError(format!(
        "Timed out waiting for auto-started warm-pool daemon at {}",
        config.socket
    )))
}

#[cfg(windows)]
pub(crate) async fn ensure_pool_daemon_running(
    _config: &PoolAutoStartConfig,
) -> Result<(), BoxError> {
    Err(BoxError::ConfigError(
        "warm-pool daemon auto-start is not supported on Windows".to_string(),
    ))
}

/// Manage the warm VM pool.
#[derive(Parser)]
pub struct PoolArgs {
    #[command(subcommand)]
    pub action: PoolAction,
}

/// Pool subcommands.
#[derive(Subcommand)]
pub enum PoolAction {
    /// Start the warm pool daemon (pre-boot VMs + serve `pool run` over a socket)
    Start(PoolStartArgs),
    /// Run a command in a fresh warm sandbox (client of `pool start`)
    Run(PoolRunArgs),
    /// Drain and stop the warm pool
    Stop(PoolStopArgs),
    /// Show warm pool statistics
    Status(PoolStatusArgs),
}

/// Arguments for `pool start`.
#[derive(Parser)]
pub struct PoolStartArgs {
    /// Image to pre-warm (optional). Sandboxes default to this image; `pool run`
    /// may request any other image, which the daemon warms on first use.
    #[arg(long)]
    pub image: Option<String>,

    /// Number of VMs to keep pre-booted (min_idle)
    #[arg(long, default_value = "2")]
    pub size: usize,

    /// Maximum pool capacity
    #[arg(long, default_value = "8")]
    pub max: usize,

    /// Maximum number of VM boots that may run concurrently while filling pools
    #[arg(long, default_value_t = DEFAULT_POOL_BOOT_CONCURRENCY)]
    pub boot_concurrency: usize,

    /// Idle TTL in seconds before evicting a pre-booted VM (0 = unlimited)
    #[arg(long, default_value = "300")]
    pub ttl: u64,

    /// Idle TTL before reclaiming an unreleased lease (0 = unlimited).
    ///
    /// This protects the daemon when an internal lease client exits before it can
    /// send release. Running lease exec requests are never reclaimed mid-command.
    #[arg(long = "lease-ttl", default_value_t = DEFAULT_POOL_LEASE_TTL_SECS, value_parser = crate::output::parse_duration_secs)]
    pub lease_ttl: u64,

    /// Unix socket to serve `pool run` requests on
    #[arg(long, default_value = DEFAULT_SOCKET)]
    pub socket: String,

    /// Extra images to pre-warm at startup, `image[=count]` (count defaults to
    /// --size). Repeat or comma-separate: `--warm python:3=4,node:20`.
    #[arg(long, value_delimiter = ',')]
    pub warm: Vec<String>,

    /// Boot pooled VMs IDLE and run each `pool run` command as the box's real MAIN
    /// (full box semantics: exit code + json-file console logs), instead of
    /// exec-into-keepalive.
    #[arg(long)]
    pub deferred: bool,

    /// Mark pooled VM memory KSM-mergeable so the host dedups identical pages
    /// across same-image VMs (Linux 6.4+; needs /sys/kernel/mm/ksm/run=1).
    #[arg(long)]
    pub ksm: bool,

    /// Fill the pool by snapshot-fork: boot one template VM, snapshot it, then
    /// restore every other slot (MAP_PRIVATE CoW) instead of cold-booting each.
    #[arg(long)]
    pub snapshot_fork: bool,

    /// Serve Prometheus metrics (warm-pool hit/miss, VM boot, boot phases, and
    /// cache) on this address (e.g. `127.0.0.1:9101`). Off when unset. Bind
    /// loopback — no auth.
    #[arg(long)]
    pub metrics_addr: Option<String>,

    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `pool run`.
#[derive(Parser)]
pub struct PoolRunArgs {
    /// Unix socket of the `pool start` daemon
    #[arg(long, default_value = DEFAULT_SOCKET)]
    pub socket: String,

    /// Image to run in (defaults to the daemon's --image). The daemon warms a
    /// pool for this image on first use.
    #[arg(long)]
    pub image: Option<String>,

    /// User to run as (uid[:gid] or a name resolved in the container).
    #[arg(long, short = 'u')]
    pub user: Option<String>,

    /// Working directory inside the sandbox.
    #[arg(long, short = 'w')]
    pub workdir: Option<String>,

    /// Extra environment variables, KEY=VALUE (repeatable).
    #[arg(long, short = 'e')]
    pub env: Vec<String>,

    /// Bind mount a host path into the pre-warmed sandbox, HOST:CONTAINER[:ro|rw].
    ///
    /// Volumes are part of the warm-pool key because virtio-fs mounts must exist
    /// before the VM boots; requests with different mounts use different pools.
    #[arg(long = "volume", short = 'v')]
    pub volumes: Vec<String>,

    /// Number of vCPUs for lazily-created pools.
    #[arg(long, default_value_t = DEFAULT_POOL_VCPUS)]
    pub cpus: u32,

    /// Memory for lazily-created pools.
    #[arg(long, default_value = DEFAULT_POOL_MEMORY)]
    pub memory: String,

    /// On a --deferred daemon: run via exec instead of as the box's main —
    /// faster (the VM survives and is returned to use), output via the exec
    /// stream rather than the json-file logs.
    #[arg(long)]
    pub exec: bool,

    /// Command and arguments to run in a fresh warm sandbox
    #[arg(last = true, required = true)]
    pub cmd: Vec<String>,
}

/// Arguments for `pool stop`.
#[derive(Parser)]
pub struct PoolStopArgs {
    /// Unix socket of the `pool start` daemon
    #[arg(long, default_value = DEFAULT_SOCKET)]
    pub socket: String,

    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `pool status`.
#[derive(Parser)]
pub struct PoolStatusArgs {
    /// Unix socket of the `pool start` daemon
    #[arg(long, default_value = DEFAULT_SOCKET)]
    pub socket: String,

    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

/// Execute a pool command.
pub async fn execute(args: PoolArgs) -> Result<(), BoxError> {
    match args.action {
        PoolAction::Start(a) => execute_start(a).await,
        PoolAction::Run(a) => execute_run(a).await,
        PoolAction::Stop(a) => execute_stop(a).await,
        PoolAction::Status(a) => execute_status(a).await,
    }
}

async fn execute_start(args: PoolStartArgs) -> Result<(), BoxError> {
    let config = PoolDaemonConfig {
        image: args.image.clone(),
        size: args.size,
        max: args.max,
        boot_concurrency: args.boot_concurrency,
        ttl: args.ttl,
        lease_ttl: args.lease_ttl,
        socket: args.socket.clone(),
        warm: args.warm.clone(),
        deferred: args.deferred,
        ksm: args.ksm,
        snapshot_fork: args.snapshot_fork,
        metrics_addr: args.metrics_addr.clone(),
        json: args.json,
    };
    validate_pool_daemon_config(&config).map_err(BoxError::ConfigError)?;

    #[cfg(windows)]
    {
        let _ = config;
        return Err(BoxError::ConfigError(
            "`pool start` is not supported on Windows; the warm pool daemon requires a Unix socket and is unavailable on WHPX"
                .to_string(),
        ));
    }

    #[cfg(not(windows))]
    {
        let daemon = start_pool_daemon(config).await?;
        let PoolDaemonReport {
            default_stats,
            warmed_extra,
        } = &daemon.report;
        if args.json {
            match default_stats {
                Some((image, stats)) => println!("{}", format_stats_json(image, stats)),
                None => println!(
                    r#"{{"default_image":null,"max":{},"socket":"{}"}}"#,
                    args.max, args.socket
                ),
            }
        } else {
            println!("Warm pool started");
            match &args.image {
                Some(i) => println!("  default image: {i} (pre-warming {})", args.size),
                None => println!("  default image: (none — `pool run` must pass --image)"),
            }
            for (image, count) in warmed_extra {
                println!("  pre-warmed: {image} (size {count})");
            }
            println!("  max:      {}", args.max);
            println!(
                "  boot concurrency: {} (daemon-wide)",
                args.boot_concurrency
            );
            println!("  ttl:      {}s", args.ttl);
            println!("  lease ttl: {}s", args.lease_ttl);
            println!("  socket:   {}", args.socket);
        }
        daemon.wait().await?;
        if !args.json {
            println!("Done.");
        }
        Ok(())
    }
}

#[cfg(not(windows))]
async fn execute_run(args: PoolRunArgs) -> Result<(), BoxError> {
    use std::io::Write;

    let memory_mb = crate::output::parse_memory(&args.memory)
        .map_err(|error| BoxError::ConfigError(format!("Invalid --memory: {error}")))?;

    let output = run_client(PoolClientRun {
        socket: args.socket,
        image: args.image,
        user: args.user,
        workdir: args.workdir,
        rootfs: None,
        env: args.env,
        volumes: args.volumes,
        vcpus: args.cpus,
        memory_mb,
        exec: args.exec,
        timeout_ns: None,
        cmd: args.cmd,
    })
    .await?;

    std::io::stdout().write_all(&output.stdout)?;
    std::io::stderr().write_all(&output.stderr)?;
    std::process::exit(output.exit_code);
}
#[cfg(windows)]
async fn execute_run(_args: PoolRunArgs) -> Result<(), BoxError> {
    Err(BoxError::ConfigError(
        "`pool run` is not supported on Windows".to_string(),
    ))
}

#[cfg(not(windows))]
async fn execute_stop(args: PoolStopArgs) -> Result<(), BoxError> {
    let stopped = match stop_client(&args.socket).await {
        Ok(()) => true,
        Err(_) => false,
    };

    // Pool ownership is in-memory. After a SIGKILL'd daemon, orphan shims stay
    // reparented to init under A3S_HOME and remain invisible to a socket-only
    // stop. Reap the same home-fenced PPID-1 set as `pool start` (#373) so
    // "pool compute is gone" is honest when the daemon is already dead.
    let home = a3s_box_core::dirs_home();
    let reaped = reap_orphaned_boxes_for_home(&home);

    if args.json {
        if stopped {
            println!(r#"{{"stopped":true,"reaped":{reaped}}}"#);
        } else {
            println!(r#"{{"stopped":false,"reason":"not_running","reaped":{reaped}}}"#);
        }
    } else if stopped {
        println!("Warm pool daemon stopped.");
    } else {
        println!("No pool daemon running.");
    }
    if reaped > 0 {
        eprintln!(
            "reaped {reaped} orphan pool microVM(s) under {}",
            home.display()
        );
    }
    Ok(())
}

#[cfg(windows)]
async fn execute_stop(_args: PoolStopArgs) -> Result<(), BoxError> {
    Err(BoxError::ConfigError(
        "`pool stop` is not supported on Windows".to_string(),
    ))
}

#[cfg(not(windows))]
async fn execute_status(args: PoolStatusArgs) -> Result<(), BoxError> {
    use tokio::net::UnixStream;

    // No daemon running is not an error for a status query — report "nothing" and
    // succeed, like `ps` with no boxes. (Only a connected daemon that misbehaves is.)
    let mut stream = match UnixStream::connect(&args.socket).await {
        Ok(stream) => stream,
        Err(_) => {
            if args.json {
                println!("[]");
            } else {
                println!("No pool daemon running (start one with `a3s-box pool start`).");
            }
            return Ok(());
        }
    };

    write_frame(&mut stream, &serde_json::to_vec(&PoolRequest::Status)?).await?;
    let resp: PoolStatusResponse =
        serde_json::from_slice(&read_frame_with_timeout(&mut stream, FRAME_READ_TIMEOUT).await?)?;

    if args.json {
        println!("{}", serde_json::to_string(&resp.images)?);
    } else if resp.images.is_empty() {
        println!("No warm pools yet (no images warmed).");
    } else {
        println!(
            "{:<60} {:>5} {:>5} {:>5} {:>6} {:>8} {:>9} {:>8}",
            "POOL", "MAX", "IDLE", "ACT", "LEASED", "CREATED", "ACQUIRED", "EVICTED"
        );
        for s in &resp.images {
            println!(
                "{:<60} {:>5} {:>5} {:>5} {:>6} {:>8} {:>9} {:>8}",
                s.pool,
                s.max,
                s.idle,
                s.active,
                s.leased,
                s.total_created,
                s.total_acquired,
                s.total_evicted
            );
        }
    }
    Ok(())
}

#[cfg(windows)]
async fn execute_status(_args: PoolStatusArgs) -> Result<(), BoxError> {
    Err(BoxError::ConfigError(
        "`pool status` is not supported on Windows".to_string(),
    ))
}

/// Format pool stats as a JSON string.
fn format_stats_json(image: &str, stats: &PoolStats) -> String {
    let hit_rate = if stats.total_acquired > 0 {
        stats.total_acquired.saturating_sub(stats.total_evicted) as f64
            / stats.total_acquired as f64
    } else {
        0.0
    };
    format!(
        r#"{{"image":"{image}","idle":{idle},"total_created":{created},"total_acquired":{acquired},"total_released":{released},"total_evicted":{evicted},"hit_rate":{hit_rate:.2}}}"#,
        image = image,
        idle = stats.idle_count,
        created = stats.total_created,
        acquired = stats.total_acquired,
        released = stats.total_released,
        evicted = stats.total_evicted,
        hit_rate = hit_rate,
    )
}

#[cfg(test)]
#[path = "pool_tests.rs"]
mod tests;
