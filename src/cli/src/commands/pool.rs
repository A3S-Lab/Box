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

use clap::{Parser, Subcommand};

use a3s_box_core::config::{BoxConfig, PoolConfig, ResourceConfig};
use a3s_box_core::event::EventEmitter;
#[cfg(not(windows))]
use a3s_box_runtime::pool::client::{read_frame, run_client, stop_client, write_frame};
use a3s_box_runtime::pool::{
    PoolClientRun, PoolImageStat, PoolLeaseExecRequest, PoolLeaseReleaseRequest,
    PoolLeaseReleaseResponse, PoolLeaseRequest, PoolLeaseResponse, PoolRequest, PoolRunRequest,
    PoolRunResponse, PoolStats, PoolStatusResponse, PoolStopResponse, WarmPool,
};

/// Default Unix socket the `pool` daemon listens on.
pub(crate) const DEFAULT_SOCKET: &str = "/tmp/a3s-box-pool.sock";
const DEFAULT_POOL_VCPUS: u32 = 2;
const DEFAULT_POOL_MEMORY: &str = "512m";
const DEFAULT_POOL_MEMORY_MB: u32 = 512;
const DEFAULT_POOL_LEASE_TTL_SECS: u64 = 3600;
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
) -> Result<(), Box<dyn std::error::Error>> {
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
        .map_err(|e| format!("Failed to auto-start warm-pool daemon: {e}"))?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while std::time::Instant::now() < deadline {
        if a3s_box_runtime::pool::client::status_client(&config.socket)
            .await
            .is_ok()
        {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            return Err(format!("Auto-started warm-pool daemon exited early: {status}").into());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    Err(format!(
        "Timed out waiting for auto-started warm-pool daemon at {}",
        config.socket
    )
    .into())
}

#[cfg(windows)]
pub(crate) async fn ensure_pool_daemon_running(
    _config: &PoolAutoStartConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    Err("warm-pool daemon auto-start is not supported on Windows".into())
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

    /// Serve Prometheus metrics (warm-pool hit/miss, VM boot, cache) on this
    /// address (e.g. `127.0.0.1:9101`). Off when unset. Bind loopback — no auth.
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
pub async fn execute(args: PoolArgs) -> Result<(), Box<dyn std::error::Error>> {
    match args.action {
        PoolAction::Start(a) => execute_start(a).await,
        PoolAction::Run(a) => execute_run(a).await,
        PoolAction::Stop(a) => execute_stop(a).await,
        PoolAction::Status(a) => execute_status(a).await,
    }
}

/// Keepalive main process so a pooled VM stays up with its exec server available;
/// the real `pool run` command runs via exec, not as this main.
fn keepalive_cmd() -> Vec<String> {
    vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "trap 'exit 0' TERM INT; while :; do sleep 3600; done".to_string(),
    ]
}

/// Build the `spawn-main` JSON spec for a deferred-mode pool command (executable +
/// args + a standard PATH so the binary resolves like a normal container main,
/// plus optional user/workdir and extra env from the request).
fn deferred_spec_json(req: &PoolRunRequest) -> Vec<u8> {
    let mut env: Vec<(String, String)> = vec![(
        "PATH".to_string(),
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
    )];
    for entry in &req.env {
        if let Some((k, v)) = entry.split_once('=') {
            env.push((k.to_string(), v.to_string()));
        }
    }
    let spec = serde_json::json!({
        "executable": req.cmd.first().map(String::as_str).unwrap_or("/bin/sh"),
        "args": req.cmd.get(1..).unwrap_or(&[]),
        "env": env,
        "workdir": req.workdir,
        "user": req.user,
    });
    serde_json::to_vec(&spec).unwrap_or_default()
}

/// Parse a `--warm` entry of the form `image[=count]` (count defaults to `default_size`).
fn parse_warm_spec(entry: &str, default_size: usize) -> Result<(String, usize), String> {
    match entry.split_once('=') {
        Some((image, count)) => {
            let image = image.trim();
            if image.is_empty() {
                return Err(format!("missing image in '{entry}'"));
            }
            let count: usize = count
                .trim()
                .parse()
                .map_err(|_| format!("invalid warm count in '{entry}'"))?;
            Ok((image.to_string(), count))
        }
        None => Ok((entry.trim().to_string(), default_size)),
    }
}

/// One image's warm pool plus a semaphore bounding concurrent in-flight sandboxes.
/// `WarmPool::acquire` boots on a pool miss with no `max_size` cap, so without this
/// a burst of `pool run`s would boot unbounded VMs; the permit makes excess
/// requests queue instead.
#[derive(Clone)]
struct PoolEntry {
    pool: std::sync::Arc<WarmPool>,
    sem: std::sync::Arc<tokio::sync::Semaphore>,
    max_size: usize,
}

/// Boot-time dimensions that define whether a pre-warmed VM can satisfy a run.
///
/// Image alone is not enough: virtio-fs mounts, vCPUs, and memory are fixed in
/// the VM spec at boot. Keep those in the key so a request with a workspace bind
/// mount does not accidentally acquire a sandbox that lacks it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PoolKey {
    image: String,
    volumes: Vec<String>,
    vcpus: u32,
    memory_mb: u32,
}

impl PoolKey {
    fn default_for_image(image: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            volumes: Vec::new(),
            vcpus: DEFAULT_POOL_VCPUS,
            memory_mb: DEFAULT_POOL_MEMORY_MB,
        }
    }

    fn from_request(image: String, req: &PoolRunRequest) -> Self {
        Self {
            image,
            volumes: req.volumes.clone(),
            vcpus: req.vcpus.unwrap_or(DEFAULT_POOL_VCPUS),
            memory_mb: req.memory_mb.unwrap_or(DEFAULT_POOL_MEMORY_MB),
        }
    }

    fn from_lease(image: String, req: &PoolLeaseRequest) -> Self {
        Self {
            image,
            volumes: req.volumes.clone(),
            vcpus: req.vcpus.unwrap_or(DEFAULT_POOL_VCPUS),
            memory_mb: req.memory_mb.unwrap_or(DEFAULT_POOL_MEMORY_MB),
        }
    }

    fn label(&self) -> String {
        if self.volumes.is_empty()
            && self.vcpus == DEFAULT_POOL_VCPUS
            && self.memory_mb == DEFAULT_POOL_MEMORY_MB
        {
            return self.image.clone();
        }

        format!(
            "{} [vcpus={}, memory={}m, volumes={}]",
            self.image,
            self.vcpus,
            self.memory_mb,
            self.volumes.len()
        )
    }
}

#[cfg(not(windows))]
struct LeasedVm {
    key: PoolKey,
    vm: std::sync::Arc<tokio::sync::Mutex<a3s_box_runtime::VmManager>>,
    last_used_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    active_execs: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

#[cfg(not(windows))]
struct LeaseExecGuard {
    last_used_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    active_execs: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(not(windows))]
impl LeaseExecGuard {
    fn new(leased: &LeasedVm) -> Self {
        leased
            .active_execs
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self {
            last_used_ms: leased.last_used_ms.clone(),
            active_execs: leased.active_execs.clone(),
        }
    }
}

#[cfg(not(windows))]
impl Drop for LeaseExecGuard {
    fn drop(&mut self) {
        self.last_used_ms
            .store(now_millis(), std::sync::atomic::Ordering::SeqCst);
        self.active_execs
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A registry of warm pools keyed by image, created lazily on first use, so one
/// daemon can serve sandboxes of different images.
struct PoolRegistry {
    pools: tokio::sync::Mutex<std::collections::HashMap<PoolKey, PoolEntry>>,
    #[cfg(not(windows))]
    leases: tokio::sync::Mutex<std::collections::HashMap<String, LeasedVm>>,
    default_image: Option<String>,
    size: usize,
    max: usize,
    ttl: u64,
    #[cfg(not(windows))]
    lease_ttl: u64,
    /// When true, pooled VMs boot IDLE and `pool run` spawns the command as the
    /// box's real MAIN (full box semantics), instead of exec-into-keepalive.
    deferred: bool,
    /// Mark pooled VM memory KSM-mergeable (host page dedup across same-image VMs).
    ksm: bool,
    /// Fill the pool by snapshot-fork (one template, restore the rest).
    snapshot_fork: bool,
    /// Optional Prometheus metrics shared across every pool this registry
    /// creates, so warm_pool hit/miss + vm_boot/cache numbers are scrapeable
    /// from the long-lived daemon (the one process where they matter most).
    metrics: Option<a3s_box_runtime::RuntimeMetrics>,
}

impl PoolRegistry {
    /// The pool entry for `image`, lazily started (and pre-warmed in the background)
    /// on first use, with `min_idle = size`. `WarmPool::start` returns once the
    /// replenisher is spawned, so holding the map lock across it is brief. The
    /// concurrency semaphore is sized to the pool's `max_size`.
    async fn get_or_create_with_size(
        &self,
        key: PoolKey,
        size: usize,
    ) -> Result<PoolEntry, String> {
        let mut pools = self.pools.lock().await;
        if let Some(entry) = pools.get(&key) {
            return Ok(entry.clone());
        }
        let max_size = self.max.max(size);
        let pool_config = PoolConfig {
            enabled: true,
            min_idle: size,
            max_size,
            idle_ttl_secs: self.ttl,
            snapshot_fork: self.snapshot_fork,
            ..Default::default()
        };
        let box_config = BoxConfig {
            image: key.image.clone(),
            resources: ResourceConfig {
                vcpus: key.vcpus,
                memory_mb: key.memory_mb,
                ..Default::default()
            },
            volumes: key.volumes.clone(),
            // In deferred mode the VM boots IDLE (keepalive cmd is stashed but
            // unused — the per-request command arrives via spawn-main).
            cmd: keepalive_cmd(),
            pool: pool_config.clone(),
            deferred_main: self.deferred,
            ksm: self.ksm,
            ..Default::default()
        };
        let mut pool = WarmPool::start(pool_config, box_config, EventEmitter::new(256))
            .await
            .map_err(|e| e.to_string())?;
        // Record this pool's hit/miss/boot/cache metrics into the shared registry
        // (set before Arc-wrapping — set_metrics needs &mut). All pools share one
        // RuntimeMetrics registry, so the daemon's /metrics endpoint aggregates them.
        if let Some(metrics) = &self.metrics {
            pool.set_metrics(metrics.clone());
        }
        let pool = std::sync::Arc::new(pool);
        let entry = PoolEntry {
            pool,
            sem: std::sync::Arc::new(tokio::sync::Semaphore::new(max_size)),
            max_size,
        };
        pools.insert(key, entry.clone());
        Ok(entry)
    }

    /// Lazy pool for `key` at the daemon's default size.
    async fn get_or_create(&self, key: PoolKey) -> Result<PoolEntry, String> {
        self.get_or_create_with_size(key, self.size).await
    }

    /// Lease pools with boot-time volumes are usually build-stage rootfs mounts:
    /// unique, short-lived, and useful only to the single holder. Do not pre-warm
    /// a whole pool for those keys; acquire will cold-fill exactly the VM needed.
    fn lease_min_idle(&self, key: &PoolKey) -> usize {
        if key.volumes.is_empty() {
            self.size
        } else {
            0
        }
    }

    /// Resolve the image for a request: the requested one, else the daemon default.
    fn resolve_image(&self, requested: Option<String>) -> Option<String> {
        requested.or_else(|| self.default_image.clone())
    }

    /// Stop replenishment and destroy idle VMs across all pools (shutdown).
    async fn drain_all(&self) {
        #[cfg(not(windows))]
        {
            let mut leases = self.leases.lock().await;
            for (_, leased) in leases.drain() {
                let _ = leased.vm.lock().await.destroy().await;
            }
        }

        let pools = self.pools.lock().await;
        for entry in pools.values() {
            entry.pool.signal_shutdown();
            let _ = entry.pool.drain_idle().await;
        }
    }

    /// Snapshot live per-image stats, sorted by image name.
    async fn stats(&self) -> Vec<PoolImageStat> {
        let pools = {
            let pools = self.pools.lock().await;
            pools
                .iter()
                .map(|(key, entry)| (key.clone(), entry.clone()))
                .collect::<Vec<_>>()
        };
        #[cfg(not(windows))]
        let leased_by_key = {
            let leases = self.leases.lock().await;
            let mut counts = std::collections::HashMap::<PoolKey, usize>::new();
            for leased in leases.values() {
                *counts.entry(leased.key.clone()).or_default() += 1;
            }
            counts
        };
        let mut out = Vec::with_capacity(pools.len());
        for (key, entry) in pools {
            let s = entry.pool.stats().await;
            let active = entry.max_size.saturating_sub(entry.sem.available_permits());
            #[cfg(not(windows))]
            let leased = leased_by_key.get(&key).copied().unwrap_or(0);
            #[cfg(windows)]
            let leased = 0;
            out.push(PoolImageStat {
                image: key.image.clone(),
                pool: key.label(),
                max: entry.max_size,
                idle: s.idle_count,
                active,
                leased,
                total_created: s.total_created,
                total_acquired: s.total_acquired,
                total_evicted: s.total_evicted,
            });
        }
        out.sort_by(|a, b| a.image.cmp(&b.image).then_with(|| a.pool.cmp(&b.pool)));
        out
    }

    #[cfg(not(windows))]
    async fn lease_vm(&self, req: PoolLeaseRequest) -> Result<String, String> {
        let image = self.resolve_image(req.image.clone()).ok_or_else(|| {
            "no image: pass an image or start the daemon with --image".to_string()
        })?;
        let key = PoolKey::from_lease(image.clone(), &req);
        let entry = self
            .get_or_create_with_size(key.clone(), self.lease_min_idle(&key))
            .await
            .map_err(|e| format!("pool for {image}: {e}"))?;
        let permit = entry
            .sem
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "pool semaphore closed".to_string())?;
        let vm = entry
            .pool
            .acquire()
            .await
            .map_err(|e| format!("acquire failed: {e}"))?;
        let lease_id = uuid::Uuid::new_v4().to_string();
        self.leases.lock().await.insert(
            lease_id.clone(),
            LeasedVm {
                key,
                vm: std::sync::Arc::new(tokio::sync::Mutex::new(vm)),
                last_used_ms: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(now_millis())),
                active_execs: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                _permit: permit,
            },
        );
        Ok(lease_id)
    }

    #[cfg(not(windows))]
    async fn exec_lease(&self, req: PoolLeaseExecRequest) -> PoolRunResponse {
        let (vm, _guard) = {
            let leases = self.leases.lock().await;
            let Some(leased) = leases.get(&req.lease_id) else {
                return err_resp(format!("unknown pool lease '{}'", req.lease_id));
            };
            (leased.vm.clone(), LeaseExecGuard::new(leased))
        };
        let output = vm
            .lock()
            .await
            .exec_request(&a3s_box_core::exec::ExecRequest {
                request_id: None,
                cmd: req.cmd,
                timeout_ns: req.timeout_ns.unwrap_or(60_000_000_000),
                env: req.env,
                working_dir: req.working_dir,
                rootfs: req.rootfs,
                stdin: req.stdin,
                stdin_streaming: false,
                user: req.user,
                streaming: false,
            })
            .await;
        match output {
            Ok(o) => PoolRunResponse {
                stdout: o.stdout,
                stderr: o.stderr,
                exit_code: o.exit_code,
                error: None,
            },
            Err(e) => err_resp(e.to_string()),
        }
    }

    #[cfg(not(windows))]
    async fn release_lease(&self, req: PoolLeaseReleaseRequest) -> Option<String> {
        let leased = match self.leases.lock().await.remove(&req.lease_id) {
            Some(leased) => leased,
            None => return Some(format!("unknown pool lease '{}'", req.lease_id)),
        };
        let result = {
            let mut vm = leased.vm.lock().await;
            vm.destroy().await.err().map(|e| e.to_string())
        };
        result
    }

    #[cfg(not(windows))]
    async fn expired_lease_ids(&self, now_ms: u64) -> Vec<String> {
        if self.lease_ttl == 0 {
            return Vec::new();
        }
        let leases = self.leases.lock().await;
        let mut ids = leases
            .iter()
            .filter(|(_, leased)| lease_is_expired(leased, self.lease_ttl, now_ms))
            .map(|(lease_id, _)| lease_id.clone())
            .collect::<Vec<_>>();
        ids.sort();
        ids
    }

    #[cfg(not(windows))]
    async fn reap_expired_leases(&self) -> usize {
        let expired_ids = self.expired_lease_ids(now_millis()).await;
        if expired_ids.is_empty() {
            return 0;
        }

        let mut expired = Vec::new();
        {
            let mut leases = self.leases.lock().await;
            for lease_id in expired_ids {
                if let Some(leased) = leases.remove(&lease_id) {
                    expired.push((lease_id, leased));
                }
            }
        }

        let mut count = 0;
        for (lease_id, leased) in expired {
            if !lease_is_expired(&leased, self.lease_ttl, now_millis()) {
                self.leases.lock().await.insert(lease_id, leased);
                continue;
            }
            tracing::warn!(lease_id = %lease_id, "Reclaiming expired warm-pool lease");
            let _ = leased.vm.lock().await.destroy().await;
            count += 1;
        }
        count
    }
}

async fn execute_start(args: PoolStartArgs) -> Result<(), Box<dyn std::error::Error>> {
    if args.size == 0 {
        return Err("--size must be greater than 0".into());
    }
    if args.size > args.max {
        return Err(format!("--size ({}) cannot exceed --max ({})", args.size, args.max).into());
    }

    // Optional Prometheus metrics for the long-lived daemon. One shared registry
    // is handed to every pool (set_metrics) and to the /metrics server; cloning a
    // RuntimeMetrics shares the underlying registry, so the server scrapes what the
    // pools record (warm_pool hit/miss, vm_boot, cache).
    let metrics = if args.metrics_addr.is_some() {
        a3s_box_runtime::RuntimeMetrics::try_new().ok()
    } else {
        None
    };

    let registry = std::sync::Arc::new(PoolRegistry {
        pools: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        #[cfg(not(windows))]
        leases: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        default_image: args.image.clone(),
        size: args.size,
        max: args.max,
        ttl: args.ttl,
        #[cfg(not(windows))]
        lease_ttl: args.lease_ttl,
        deferred: args.deferred,
        ksm: args.ksm,
        snapshot_fork: args.snapshot_fork,
        metrics: metrics.clone(),
    });

    // Serve /metrics alongside the pool socket, if requested.
    if let (Some(addr), Some(metrics)) = (args.metrics_addr.clone(), metrics) {
        tokio::spawn(serve_pool_metrics(addr, metrics));
    }

    #[cfg(not(windows))]
    if args.lease_ttl > 0 {
        tokio::spawn(reap_expired_leases_task(registry.clone(), args.lease_ttl));
    }

    // Bind the control socket before pre-warming. Large images can take longer
    // than the autostart client's safety cap to cold boot; keeping the daemon
    // undiscoverable until that work completed made a healthy startup look like
    // a timeout. Requests may connect immediately and naturally wait on the
    // per-pool creation lock until the first VM is truly exec-ready.
    #[cfg(not(windows))]
    let serve_task = {
        let serve_registry = registry.clone();
        let serve_socket = args.socket.clone();
        let serve_json = args.json;
        tokio::spawn(async move {
            serve(serve_registry, &serve_socket, serve_json)
                .await
                .map_err(|error| error.to_string())
        })
    };

    // Pre-warm the default image, if one was given.
    let default_stats = if let Some(ref image) = args.image {
        let entry = registry
            .get_or_create(PoolKey::default_for_image(image.clone()))
            .await?;
        Some((image.clone(), entry.pool.stats().await))
    } else {
        None
    };

    // Pre-warm any extra images requested via --warm.
    let mut warmed_extra: Vec<(String, usize)> = Vec::new();
    for entry in &args.warm {
        let (image, count) = parse_warm_spec(entry, args.size)?;
        if count == 0 {
            return Err(format!("--warm count must be > 0 (in '{entry}')").into());
        }
        registry
            .get_or_create_with_size(PoolKey::default_for_image(&image), count)
            .await?;
        warmed_extra.push((image, count));
    }

    if args.json {
        match &default_stats {
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
        for (image, count) in &warmed_extra {
            println!("  pre-warmed: {image} (size {count})");
        }
        println!("  max:      {}", args.max);
        println!("  ttl:      {}s", args.ttl);
        println!("  lease ttl: {}s", args.lease_ttl);
        println!("  socket:   {}", args.socket);
    }

    #[cfg(not(windows))]
    serve_task.await??;
    #[cfg(windows)]
    serve(registry, &args.socket, args.json).await?;

    if !args.json {
        println!("Done.");
    }
    Ok(())
}

#[cfg(not(windows))]
fn lease_is_expired(leased: &LeasedVm, lease_ttl_secs: u64, now_ms: u64) -> bool {
    if lease_ttl_secs == 0 {
        return false;
    }
    if leased
        .active_execs
        .load(std::sync::atomic::Ordering::SeqCst)
        != 0
    {
        return false;
    }
    let ttl_ms = lease_ttl_secs.saturating_mul(1000);
    let cutoff = now_ms.saturating_sub(ttl_ms);
    leased
        .last_used_ms
        .load(std::sync::atomic::Ordering::SeqCst)
        <= cutoff
}

#[cfg(not(windows))]
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(not(windows))]
fn lease_reaper_interval(lease_ttl_secs: u64) -> std::time::Duration {
    let secs = if lease_ttl_secs <= 4 {
        1
    } else {
        (lease_ttl_secs / 4).clamp(1, 60)
    };
    std::time::Duration::from_secs(secs)
}

#[cfg(not(windows))]
async fn reap_expired_leases_task(registry: std::sync::Arc<PoolRegistry>, lease_ttl_secs: u64) {
    let mut interval = tokio::time::interval(lease_reaper_interval(lease_ttl_secs));
    loop {
        interval.tick().await;
        let reaped = registry.reap_expired_leases().await;
        if reaped > 0 {
            tracing::warn!(reaped, "Reaped expired warm-pool leases");
        }
    }
}

/// Serve a Prometheus `/metrics` endpoint exposing the pool daemon's runtime
/// metrics (warm_pool hit/miss, vm_boot, cache). Minimal raw-HTTP server,
/// mirroring the monitor's metrics endpoint.
async fn serve_pool_metrics(addr: String, metrics: a3s_box_runtime::RuntimeMetrics) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("pool metrics: failed to bind {addr}: {e}");
            return;
        }
    };
    println!("  metrics:  http://{addr}/metrics");

    loop {
        let Ok((mut sock, _)) = listener.accept().await else {
            continue;
        };
        let metrics = metrics.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let n = match sock.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req.split_whitespace().nth(1).unwrap_or("");
            let (status, body) = if path.starts_with("/metrics") {
                ("200 OK", metrics.encode())
            } else {
                ("404 Not Found", String::new())
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(response.as_bytes()).await;
        });
    }
}

/// Accept `pool run` connections until Ctrl-C, serving each request concurrently
/// so independent sandboxes don't queue behind one another. On shutdown, stop the
/// replenisher and destroy idle VMs (in-flight requests keep their own acquired VM).
#[cfg(not(windows))]
async fn serve(
    registry: std::sync::Arc<PoolRegistry>,
    socket: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use tokio::net::UnixListener;

    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    if !json {
        println!("Listening on {} (Ctrl-C to drain and stop)", socket);
    }

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (mut stream, _) = accepted?;
                let registry = registry.clone();
                let shutdown_tx = shutdown_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(&registry, &shutdown_tx, &mut stream).await {
                        tracing::warn!(error = %e, "pool connection failed");
                    }
                });
            }
            _ = shutdown_rx.recv() => {
                let _ = std::fs::remove_file(socket);
                if !json {
                    println!("Draining warm pools...");
                }
                registry.drain_all().await;
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                let _ = std::fs::remove_file(socket);
                if !json {
                    println!("Draining warm pools...");
                }
                registry.drain_all().await;
                break;
            }
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn err_resp(msg: impl Into<String>) -> PoolRunResponse {
    PoolRunResponse {
        stdout: vec![],
        stderr: vec![],
        exit_code: -1,
        error: Some(msg.into()),
    }
}

#[cfg(not(windows))]
fn timeout_duration(timeout_ns: Option<u64>, default_ns: u64) -> std::time::Duration {
    std::time::Duration::from_nanos(timeout_ns.unwrap_or(default_ns))
}

#[cfg(not(windows))]
async fn handle_conn(
    registry: &PoolRegistry,
    shutdown_tx: &tokio::sync::mpsc::UnboundedSender<()>,
    stream: &mut tokio::net::UnixStream,
) -> std::io::Result<()> {
    // 60s exec cap — generous for a sandbox command.
    const EXEC_TIMEOUT_NS: u64 = 60_000_000_000;

    let req: PoolRequest = serde_json::from_slice(&read_frame(stream).await?)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    let run = match req {
        PoolRequest::Status => {
            let resp = PoolStatusResponse {
                images: registry.stats().await,
            };
            let bytes = serde_json::to_vec(&resp)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            return write_frame(stream, &bytes).await;
        }
        PoolRequest::Stop => {
            let bytes = serde_json::to_vec(&PoolStopResponse { error: None })
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            write_frame(stream, &bytes).await?;
            let _ = shutdown_tx.send(());
            return Ok(());
        }
        PoolRequest::Lease(lease) => {
            let resp = match registry.lease_vm(lease).await {
                Ok(lease_id) => PoolLeaseResponse {
                    lease_id: Some(lease_id),
                    error: None,
                },
                Err(error) => PoolLeaseResponse {
                    lease_id: None,
                    error: Some(error),
                },
            };
            let bytes = serde_json::to_vec(&resp)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            return write_frame(stream, &bytes).await;
        }
        PoolRequest::Exec(exec) => {
            let resp = registry.exec_lease(exec).await;
            let bytes = serde_json::to_vec(&resp)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            return write_frame(stream, &bytes).await;
        }
        PoolRequest::Release(release) => {
            let resp = PoolLeaseReleaseResponse {
                error: registry.release_lease(release).await,
            };
            let bytes = serde_json::to_vec(&resp)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            return write_frame(stream, &bytes).await;
        }
        PoolRequest::Run(run) => run,
    };

    // Resolve the image, get-or-create its pool, acquire a warm VM, run the
    // command. Keep the VM so we tear it down AFTER responding (a one-shot sandbox
    // is discarded; the pool replenishes a fresh one) — the client's latency must
    // not include VM teardown.
    // Holds (vm, permit) until after the response: the permit bounds concurrent
    // in-flight sandboxes and is released only once the VM is torn down.
    let mut used = None;
    let resp = match registry.resolve_image(run.image.clone()) {
        None => err_resp("no image: pass --image or start the daemon with --image"),
        Some(image) => match registry
            .get_or_create(PoolKey::from_request(image.clone(), &run))
            .await
        {
            Err(e) => err_resp(format!("pool for {image}: {e}")),
            Ok(entry) => {
                // Backpressure: wait for a slot so a burst doesn't boot unbounded VMs.
                let permit = entry
                    .sem
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("pool semaphore is never closed");
                match entry.pool.acquire().await {
                    Err(e) => err_resp(format!("acquire failed: {e}")),
                    Ok(mut vm) => {
                        // Deferred-main: run the command as the box's real MAIN
                        // (full box semantics — exit code + json-file console logs).
                        // Otherwise exec it (output via the exec stream); `exec:
                        // true` forces exec mode per request on a deferred daemon
                        // (its IDLE VMs serve exec just as well). Both honor
                        // user/workdir/env from the request.
                        let result = if registry.deferred && !run.exec {
                            vm.run_deferred_main(
                                &deferred_spec_json(&run),
                                timeout_duration(run.timeout_ns, EXEC_TIMEOUT_NS),
                            )
                            .await
                        } else {
                            vm.exec_request(&a3s_box_core::exec::ExecRequest {
                                request_id: None,
                                cmd: run.cmd,
                                timeout_ns: run.timeout_ns.unwrap_or(EXEC_TIMEOUT_NS),
                                env: run.env,
                                working_dir: run.workdir,
                                rootfs: run.rootfs,
                                stdin: None,
                                stdin_streaming: false,
                                user: run.user,
                                streaming: false,
                            })
                            .await
                        };
                        let resp = match result {
                            Ok(o) => PoolRunResponse {
                                stdout: o.stdout,
                                stderr: o.stderr,
                                exit_code: o.exit_code,
                                error: None,
                            },
                            Err(e) => err_resp(e.to_string()),
                        };
                        used = Some((vm, permit));
                        resp
                    }
                }
            }
        },
    };

    let bytes = serde_json::to_vec(&resp)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    write_frame(stream, &bytes).await?;

    // Tear down the used sandbox in the background so neither the client nor the
    // daemon's accept loop blocks on it; release the concurrency permit afterwards.
    if let Some((mut vm, permit)) = used {
        tokio::spawn(async move {
            let _ = vm.destroy().await;
            drop(permit);
        });
    }
    Ok(())
}

#[cfg(not(windows))]
async fn execute_run(args: PoolRunArgs) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;

    let memory_mb =
        crate::output::parse_memory(&args.memory).map_err(|e| format!("Invalid --memory: {e}"))?;

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
async fn serve(
    _registry: std::sync::Arc<PoolRegistry>,
    _socket: &str,
    _json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!(
        "pool socket serving is not supported on Windows; pool stays pre-warmed until Ctrl-C."
    );
    tokio::signal::ctrl_c().await?;
    Ok(())
}

#[cfg(windows)]
async fn execute_run(_args: PoolRunArgs) -> Result<(), Box<dyn std::error::Error>> {
    Err("`pool run` is not supported on Windows".into())
}

#[cfg(not(windows))]
async fn execute_stop(args: PoolStopArgs) -> Result<(), Box<dyn std::error::Error>> {
    match stop_client(&args.socket).await {
        Ok(()) => {
            if args.json {
                println!(r#"{{"stopped":true}}"#);
            } else {
                println!("Warm pool daemon stopped.");
            }
        }
        Err(_) => {
            if args.json {
                println!(r#"{{"stopped":false,"reason":"not_running"}}"#);
            } else {
                println!("No pool daemon running.");
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
async fn execute_stop(_args: PoolStopArgs) -> Result<(), Box<dyn std::error::Error>> {
    Err("`pool stop` is not supported on Windows".into())
}

#[cfg(not(windows))]
async fn execute_status(args: PoolStatusArgs) -> Result<(), Box<dyn std::error::Error>> {
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
    let resp: PoolStatusResponse = serde_json::from_slice(&read_frame(&mut stream).await?)?;

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
async fn execute_status(_args: PoolStatusArgs) -> Result<(), Box<dyn std::error::Error>> {
    Err("`pool status` is not supported on Windows".into())
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
mod tests {
    use super::*;
    use a3s_box_runtime::pool::PoolStats;

    fn sample_stats() -> PoolStats {
        PoolStats {
            idle_count: 2,
            total_created: 5,
            total_acquired: 4,
            total_released: 3,
            total_evicted: 1,
        }
    }

    #[test]
    fn test_format_stats_json_fields() {
        let stats = sample_stats();
        let json = format_stats_json("alpine:latest", &stats);
        assert!(json.contains(r#""image":"alpine:latest""#));
        assert!(json.contains(r#""idle":2"#));
        assert!(json.contains(r#""total_created":5"#));
        assert!(json.contains(r#""total_acquired":4"#));
        assert!(json.contains(r#""total_released":3"#));
        assert!(json.contains(r#""total_evicted":1"#));
        assert!(json.contains("hit_rate"));
    }

    #[test]
    fn test_format_stats_json_zero_acquired() {
        let stats = PoolStats {
            idle_count: 0,
            total_created: 0,
            total_acquired: 0,
            total_released: 0,
            total_evicted: 0,
        };
        let json = format_stats_json("nginx:alpine", &stats);
        assert!(json.contains(r#""hit_rate":0.00"#));
    }

    #[test]
    fn test_format_stats_json_is_valid_structure() {
        let stats = sample_stats();
        let json = format_stats_json("alpine:latest", &stats);
        assert!(json.starts_with('{'));
        assert!(json.ends_with('}'));
    }

    #[test]
    fn test_keepalive_cmd_is_a_sleep_loop() {
        let c = keepalive_cmd();
        assert_eq!(c[0], "/bin/sh");
        assert!(c.last().unwrap().contains("sleep"));
    }

    #[test]
    fn test_pool_autostart_start_args() {
        let config = PoolAutoStartConfig {
            socket: "/tmp/a3s-pool.sock".to_string(),
            image: Some("alpine:latest".to_string()),
            size: 1,
            max: 4,
        };

        assert_eq!(
            config.start_args(),
            vec![
                "pool",
                "start",
                "--socket",
                "/tmp/a3s-pool.sock",
                "--size",
                "1",
                "--max",
                "4",
                "--image",
                "alpine:latest"
            ]
        );

        let lazy = PoolAutoStartConfig {
            image: None,
            ..config
        };
        assert!(!lazy.start_args().contains(&"--image".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn test_pool_autostart_lock_serializes_same_socket() {
        use std::sync::mpsc;
        use std::time::Duration;

        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("pool.sock").display().to_string();
        let lock_path = pool_autostart_lock_path(&socket);
        let guard = PoolAutoStartLock::acquire(&socket).unwrap();
        assert!(lock_path.exists());

        let thread_socket = socket.clone();
        let (tx, rx) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let _guard = PoolAutoStartLock::acquire(&thread_socket).unwrap();
            tx.send(()).unwrap();
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "second auto-start lock should block while the first guard is alive"
        );
        drop(guard);
        rx.recv_timeout(Duration::from_secs(2))
            .expect("second auto-start lock should proceed after drop");
        waiter.join().unwrap();
    }

    #[test]
    fn test_parse_warm_spec() {
        // image=count
        assert_eq!(
            parse_warm_spec("python:3=4", 2).unwrap(),
            ("python:3".to_string(), 4)
        );
        // bare image → default size
        assert_eq!(
            parse_warm_spec("node:20", 7).unwrap(),
            ("node:20".to_string(), 7)
        );
        // whitespace tolerated
        assert_eq!(
            parse_warm_spec("  alpine = 3 ", 2).unwrap(),
            ("alpine".to_string(), 3)
        );
        // bad count / empty image error out
        assert!(parse_warm_spec("alpine=notanum", 2).is_err());
        assert!(parse_warm_spec("=4", 2).is_err());
    }

    #[test]
    fn test_pool_key_includes_boot_time_dimensions() {
        let base = PoolKey::default_for_image("node:24-bookworm");
        let mounted = PoolKey::from_request(
            "node:24-bookworm".to_string(),
            &PoolRunRequest {
                image: None,
                user: None,
                workdir: None,
                rootfs: None,
                env: vec![],
                volumes: vec!["/host/work:/workspace:ro".into()],
                vcpus: Some(4),
                memory_mb: Some(8192),
                exec: false,
                timeout_ns: None,
                cmd: vec!["node".into(), "--version".into()],
            },
        );

        assert_ne!(base, mounted);
        assert_eq!(mounted.image, "node:24-bookworm");
        assert_eq!(mounted.volumes, vec!["/host/work:/workspace:ro"]);
        assert_eq!(mounted.vcpus, 4);
        assert_eq!(mounted.memory_mb, 8192);
        assert!(mounted.label().contains("volumes=1"));
    }

    #[test]
    fn test_deferred_spec_json() {
        // The spawn-main spec for a deferred pool run: executable + args + a PATH
        // so the binary resolves like a normal container main, plus per-request
        // user/workdir and extra env.
        let req = PoolRunRequest {
            image: None,
            user: Some("1000".into()),
            workdir: Some("/work".into()),
            rootfs: None,
            env: vec!["FOO=bar".into(), "not-a-pair".into()],
            volumes: vec![],
            vcpus: None,
            memory_mb: None,
            exec: false,
            timeout_ns: None,
            cmd: vec!["sh".into(), "-c".into(), "echo hi".into()],
        };
        let json = deferred_spec_json(&req);
        let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(v["executable"], "sh");
        assert_eq!(v["args"][0], "-c");
        assert_eq!(v["args"][1], "echo hi");
        assert_eq!(v["env"][0][0], "PATH");
        assert!(v["env"][0][1].as_str().unwrap().contains("/bin"));
        assert_eq!(v["env"][1][0], "FOO");
        assert_eq!(v["env"][1][1], "bar");
        assert_eq!(v["env"].as_array().unwrap().len(), 2); // malformed entry dropped
        assert_eq!(v["user"], "1000");
        assert_eq!(v["workdir"], "/work");
        // Empty cmd falls back to a shell rather than panicking.
        let req2 = PoolRunRequest {
            image: None,
            user: None,
            workdir: None,
            rootfs: None,
            env: vec![],
            volumes: vec![],
            vcpus: None,
            memory_mb: None,
            exec: false,
            timeout_ns: None,
            cmd: vec![],
        };
        let v2: serde_json::Value = serde_json::from_slice(&deferred_spec_json(&req2)).unwrap();
        assert_eq!(v2["executable"], "/bin/sh");
        assert!(v2["user"].is_null());
    }

    #[cfg(not(windows))]
    #[test]
    fn test_timeout_duration_uses_request_or_default() {
        assert_eq!(
            timeout_duration(Some(7_000_000_000), 60_000_000_000),
            std::time::Duration::from_secs(7)
        );
        assert_eq!(
            timeout_duration(None, 60_000_000_000),
            std::time::Duration::from_secs(60)
        );
    }

    #[cfg(not(windows))]
    fn test_registry_with_lease_ttl(lease_ttl: u64) -> std::sync::Arc<PoolRegistry> {
        std::sync::Arc::new(PoolRegistry {
            pools: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            leases: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            default_image: Some("alpine:latest".to_string()),
            size: 1,
            max: 4,
            ttl: 0,
            lease_ttl,
            deferred: false,
            ksm: false,
            snapshot_fork: false,
            metrics: None,
        })
    }

    #[cfg(not(windows))]
    async fn insert_test_lease(
        registry: &PoolRegistry,
        lease_id: &str,
        last_used_ms: u64,
        active_execs: usize,
    ) {
        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let permit = sem.acquire_owned().await.unwrap();
        let config = BoxConfig {
            image: "alpine:latest".to_string(),
            ..Default::default()
        };
        let vm = a3s_box_runtime::VmManager::with_box_id(
            config,
            EventEmitter::new(16),
            format!("test-lease-{lease_id}"),
        );
        registry.leases.lock().await.insert(
            lease_id.to_string(),
            LeasedVm {
                key: PoolKey::default_for_image("alpine:latest"),
                vm: std::sync::Arc::new(tokio::sync::Mutex::new(vm)),
                last_used_ms: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(last_used_ms)),
                active_execs: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
                    active_execs,
                )),
                _permit: permit,
            },
        );
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn test_expired_lease_ids_only_reports_idle_stale_leases() {
        let registry = test_registry_with_lease_ttl(60);
        let now = 1_000_000;
        insert_test_lease(&registry, "busy", now - 120_000, 1).await;
        insert_test_lease(&registry, "fresh", now - 10_000, 0).await;
        insert_test_lease(&registry, "stale", now - 120_000, 0).await;

        let expired = registry.expired_lease_ids(now).await;

        assert_eq!(expired, vec!["stale".to_string()]);
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn test_expired_lease_ids_disabled_when_ttl_zero() {
        let registry = test_registry_with_lease_ttl(0);
        let now = 1_000_000;
        insert_test_lease(&registry, "stale", now - 120_000, 0).await;

        assert!(registry.expired_lease_ids(now).await.is_empty());
    }

    #[cfg(not(windows))]
    #[test]
    fn test_lease_min_idle_skips_prewarm_for_volume_bound_leases() {
        let registry = test_registry_with_lease_ttl(60);
        let plain_key = PoolKey::default_for_image("alpine:latest");
        let volume_key = PoolKey {
            image: "alpine:latest".to_string(),
            volumes: vec!["/host/stage:/run/a3s/build-rootfs:rw".to_string()],
            vcpus: DEFAULT_POOL_VCPUS,
            memory_mb: DEFAULT_POOL_MEMORY_MB,
        };

        assert_eq!(registry.lease_min_idle(&plain_key), registry.size);
        assert_eq!(registry.lease_min_idle(&volume_key), 0);
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn test_lease_exec_guard_marks_busy_and_refreshes_activity() {
        let registry = test_registry_with_lease_ttl(60);
        let now = now_millis();
        insert_test_lease(&registry, "lease", now.saturating_sub(120_000), 0).await;

        let (last_used, active_execs) = {
            let leases = registry.leases.lock().await;
            let leased = leases.get("lease").unwrap();
            let guard = LeaseExecGuard::new(leased);
            assert_eq!(
                leased
                    .active_execs
                    .load(std::sync::atomic::Ordering::SeqCst),
                1
            );
            assert!(!lease_is_expired(
                leased,
                registry.lease_ttl,
                now.saturating_add(120_000)
            ));
            let last_used = leased.last_used_ms.clone();
            let active_execs = leased.active_execs.clone();
            drop(guard);
            (last_used, active_execs)
        };

        assert_eq!(active_execs.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(
            last_used.load(std::sync::atomic::Ordering::SeqCst) >= now,
            "dropping the guard should refresh lease activity"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn test_lease_reaper_interval_is_bounded() {
        assert_eq!(lease_reaper_interval(1), std::time::Duration::from_secs(1));
        assert_eq!(
            lease_reaper_interval(60),
            std::time::Duration::from_secs(15)
        );
        assert_eq!(
            lease_reaper_interval(3600),
            std::time::Duration::from_secs(60)
        );
    }

    #[tokio::test]
    async fn test_backpressure_bounds_concurrency() {
        // The contract PoolEntry relies on: a permit (held until teardown) caps
        // concurrent in-flight sandboxes to the semaphore size, so a burst queues
        // instead of all running at once.
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let sem = Arc::new(tokio::sync::Semaphore::new(2));
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..6 {
            let (sem, live, peak) = (sem.clone(), live.clone(), peak.clone());
            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire_owned().await.unwrap();
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                live.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert!(
            peak.load(Ordering::SeqCst) <= 2,
            "concurrency exceeded the permit limit"
        );
    }

    #[test]
    fn test_run_request_response_roundtrip() {
        let req = PoolRunRequest {
            image: Some("alpine:latest".into()),
            user: Some("1000".into()),
            workdir: Some("/tmp".into()),
            rootfs: None,
            env: vec!["FOO=bar".into()],
            volumes: vec!["/host:/work:ro".into()],
            vcpus: Some(4),
            memory_mb: Some(2048),
            exec: false,
            timeout_ns: None,
            cmd: vec!["echo".into(), "hi".into()],
        };
        let bytes = serde_json::to_vec(&req).unwrap();
        let parsed: PoolRunRequest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.cmd, vec!["echo", "hi"]);
        assert_eq!(parsed.image.as_deref(), Some("alpine:latest"));
        assert_eq!(parsed.user.as_deref(), Some("1000"));
        assert_eq!(parsed.workdir.as_deref(), Some("/tmp"));
        assert_eq!(parsed.env, vec!["FOO=bar"]);
        assert_eq!(parsed.volumes, vec!["/host:/work:ro"]);
        assert_eq!(parsed.vcpus, Some(4));
        assert_eq!(parsed.memory_mb, Some(2048));

        // image/user/workdir/env are optional on the wire (older clients).
        let no_img: PoolRunRequest = serde_json::from_slice(br#"{"cmd":["ls"]}"#).unwrap();
        assert!(no_img.image.is_none());
        assert!(no_img.user.is_none() && no_img.workdir.is_none() && no_img.env.is_empty());
        assert!(no_img.volumes.is_empty());
        assert!(no_img.vcpus.is_none());
        assert!(no_img.memory_mb.is_none());

        let resp = PoolRunResponse {
            stdout: b"hi\n".to_vec(),
            stderr: vec![],
            exit_code: 0,
            error: None,
        };
        let rb = serde_json::to_vec(&resp).unwrap();
        let rp: PoolRunResponse = serde_json::from_slice(&rb).unwrap();
        assert_eq!(rp.stdout, b"hi\n");
        assert_eq!(rp.exit_code, 0);
        assert!(rp.error.is_none());
    }

    #[tokio::test]
    async fn test_execute_start_size_zero_fails() {
        let args = PoolStartArgs {
            image: Some("alpine:latest".to_string()),
            size: 0,
            max: 5,
            ttl: 300,
            lease_ttl: DEFAULT_POOL_LEASE_TTL_SECS,
            socket: DEFAULT_SOCKET.to_string(),
            warm: vec![],
            deferred: false,
            ksm: false,
            snapshot_fork: false,
            metrics_addr: None,
            json: false,
        };
        let result = execute_start(args).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("greater than 0"));
    }

    #[tokio::test]
    async fn test_execute_start_size_exceeds_max_fails() {
        let args = PoolStartArgs {
            image: Some("alpine:latest".to_string()),
            size: 10,
            max: 5,
            ttl: 300,
            lease_ttl: DEFAULT_POOL_LEASE_TTL_SECS,
            socket: DEFAULT_SOCKET.to_string(),
            warm: vec![],
            deferred: false,
            ksm: false,
            snapshot_fork: false,
            metrics_addr: None,
            json: false,
        };
        let result = execute_start(args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("cannot exceed --max"));
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn test_execute_stop_is_ok() {
        let result = execute_stop(PoolStopArgs {
            socket: "/tmp/a3s-box-pool-does-not-exist.sock".to_string(),
            json: false,
        })
        .await;
        assert!(result.is_ok());
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn test_stop_request_shuts_down_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("pool.sock");
        let socket_arg = socket.display().to_string();
        let server_socket = socket_arg.clone();
        let registry = std::sync::Arc::new(PoolRegistry {
            pools: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            leases: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            default_image: None,
            size: 1,
            max: 1,
            ttl: 0,
            lease_ttl: DEFAULT_POOL_LEASE_TTL_SECS,
            deferred: false,
            ksm: false,
            snapshot_fork: false,
            metrics: None,
        });

        let server = tokio::spawn(async move {
            serve(registry, &server_socket, true)
                .await
                .expect("pool server should stop cleanly");
        });

        for _ in 0..50 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(socket.exists(), "pool socket should be bound before stop");

        stop_client(&socket_arg).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .expect("pool server should exit after stop")
            .unwrap();
        assert!(!socket.exists(), "pool socket should be removed on stop");
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn test_execute_status_no_daemon_succeeds_empty() {
        // With no daemon listening, status reports "nothing running" and SUCCEEDS —
        // a status query shouldn't fail just because no pool is up (like `ps`).
        let result = execute_status(PoolStatusArgs {
            socket: "/tmp/a3s-box-pool-does-not-exist.sock".to_string(),
            json: false,
        })
        .await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_request_envelope_tagging() {
        // Run carries an op tag + the flattened PoolRunRequest; Status is a bare tag.
        let run = serde_json::to_string(&PoolRequest::Run(PoolRunRequest {
            image: Some("alpine".into()),
            user: None,
            workdir: None,
            rootfs: None,
            env: vec![],
            volumes: vec![],
            vcpus: None,
            memory_mb: None,
            exec: false,
            timeout_ns: None,
            cmd: vec!["echo".into(), "hi".into()],
        }))
        .unwrap();
        assert!(run.contains(r#""op":"run""#));
        assert!(run.contains(r#""cmd":["echo","hi"]"#));

        let status = serde_json::to_string(&PoolRequest::Status).unwrap();
        assert_eq!(status, r#"{"op":"status"}"#);

        let stop = serde_json::to_string(&PoolRequest::Stop).unwrap();
        assert_eq!(stop, r#"{"op":"stop"}"#);

        let lease = serde_json::to_string(&PoolRequest::Lease(PoolLeaseRequest {
            image: Some("alpine".into()),
            volumes: vec!["/host/rootfs:/run/a3s/build-rootfs:rw".into()],
            vcpus: Some(2),
            memory_mb: Some(512),
        }))
        .unwrap();
        assert!(lease.contains(r#""op":"lease""#));
        assert!(lease.contains("/run/a3s/build-rootfs"));

        let exec = serde_json::to_string(&PoolRequest::Exec(PoolLeaseExecRequest {
            lease_id: "lease-1".into(),
            cmd: vec!["/bin/sh".into(), "-c".into(), "echo hi".into()],
            timeout_ns: Some(5_000_000_000),
            env: vec!["FOO=bar".into()],
            working_dir: Some("/".into()),
            rootfs: Some("/run/a3s/build-rootfs".into()),
            stdin: None,
            user: None,
        }))
        .unwrap();
        assert!(exec.contains(r#""op":"exec""#));
        assert!(exec.contains(r#""lease_id":"lease-1""#));
        assert!(exec.contains(r#""rootfs":"/run/a3s/build-rootfs""#));

        // PoolStatusResponse round-trips.
        let sr = PoolStatusResponse {
            images: vec![PoolImageStat {
                image: "alpine".into(),
                pool: "alpine".into(),
                max: 4,
                idle: 2,
                active: 1,
                leased: 1,
                total_created: 5,
                total_acquired: 3,
                total_evicted: 1,
            }],
        };
        let parsed: PoolStatusResponse =
            serde_json::from_slice(&serde_json::to_vec(&sr).unwrap()).unwrap();
        assert_eq!(parsed.images[0].image, "alpine");
        assert_eq!(parsed.images[0].idle, 2);
        assert_eq!(parsed.images[0].max, 4);
        assert_eq!(parsed.images[0].active, 1);
        assert_eq!(parsed.images[0].leased, 1);

        let legacy: PoolStatusResponse = serde_json::from_str(
            r#"{"images":[{"image":"alpine","pool":"alpine","idle":2,"total_created":5,"total_acquired":3,"total_evicted":1}]}"#,
        )
        .unwrap();
        assert_eq!(legacy.images[0].max, 0);
        assert_eq!(legacy.images[0].active, 0);
        assert_eq!(legacy.images[0].leased, 0);
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn test_frame_roundtrip() {
        // write_frame then read_frame must return the exact bytes.
        let (mut a, mut b) = tokio::io::duplex(4096);
        let payload = serde_json::to_vec(&PoolRunRequest {
            image: None,
            user: None,
            workdir: None,
            rootfs: None,
            env: vec![],
            volumes: vec![],
            vcpus: None,
            memory_mb: None,
            exec: false,
            timeout_ns: None,
            cmd: vec!["echo".into(), "hi there".into()],
        })
        .unwrap();
        write_frame(&mut a, &payload).await.unwrap();
        let got = read_frame(&mut b).await.unwrap();
        let parsed: PoolRunRequest = serde_json::from_slice(&got).unwrap();
        assert_eq!(parsed.cmd, vec!["echo", "hi there"]);
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn test_socket_request_response_protocol() {
        // Exercise the full client/server wire protocol over a real Unix socket
        // (the exact framing `serve` and `pool run` use), with a stub server
        // standing in for the VM pool's acquire+exec.
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("pool.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let req: PoolRequest =
                serde_json::from_slice(&read_frame(&mut s).await.unwrap()).unwrap();
            let PoolRequest::Run(req) = req else {
                panic!("expected run request");
            };
            let resp = PoolRunResponse {
                stdout: format!("ran {:?}", req.cmd).into_bytes(),
                stderr: vec![],
                exit_code: 0,
                error: None,
            };
            write_frame(&mut s, &serde_json::to_vec(&resp).unwrap())
                .await
                .unwrap();
        });

        let output = run_client(PoolClientRun {
            socket: sock.display().to_string(),
            image: Some("alpine:latest".into()),
            user: None,
            workdir: None,
            rootfs: None,
            env: vec![],
            volumes: vec![],
            vcpus: 2,
            memory_mb: 512,
            exec: false,
            timeout_ns: None,
            cmd: vec!["ls".into(), "-la".into()],
        })
        .await
        .unwrap();

        assert_eq!(output.exit_code, 0);
        assert!(String::from_utf8_lossy(&output.stdout).contains("ls"));
        server.await.unwrap();
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn test_read_frame_truncated_errors() {
        // A truncated stream must error, not hang or panic.
        use tokio::io::AsyncWriteExt;
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&[1u8, 0]).await.unwrap(); // partial 4-byte length prefix
        drop(a);
        assert!(read_frame(&mut b).await.is_err());
    }
}
