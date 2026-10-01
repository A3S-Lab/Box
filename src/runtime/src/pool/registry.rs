//! Image-keyed warm pools and lease lifecycle for one pool daemon.
//!
//! The CLI parses flags and prints status. This module owns which VM can
//! satisfy a request, how leases are reclaimed, and how idle VMs are destroyed.

use a3s_box_core::config::{BoxConfig, PoolConfig, ResourceConfig};
use a3s_box_core::event::EventEmitter;
#[cfg(not(windows))]
use tokio::task::JoinSet;

#[cfg(any(not(windows), test))]
use super::client::PoolRunRequest;
#[cfg(not(windows))]
use super::client::{
    PoolImageStat, PoolLeaseExecRequest, PoolLeaseReleaseRequest, PoolLeaseRequest, PoolRunResponse,
};
use super::warm_pool::WarmPool;
#[cfg(target_os = "linux")]
use crate::vm::reap::reap_orphaned_box;

pub const DEFAULT_POOL_VCPUS: u32 = 2;
pub const DEFAULT_POOL_MEMORY: &str = "512m";
pub const DEFAULT_POOL_MEMORY_MB: u32 = 512;
pub const DEFAULT_POOL_LEASE_TTL_SECS: u64 = 3600;
pub const DEFAULT_POOL_BOOT_CONCURRENCY: usize = 2;

#[cfg(not(windows))]
const POOL_DRAIN_CONCURRENCY: usize = 4;

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
#[cfg(any(not(windows), test))]
pub(crate) fn deferred_spec_json(req: &PoolRunRequest) -> Vec<u8> {
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

/// One image's warm pool plus a semaphore bounding concurrent in-flight sandboxes.
/// `WarmPool::acquire` boots on a pool miss with no `max_size` cap, so without this
/// a burst of `pool run`s would boot unbounded VMs; the permit makes excess
/// requests queue instead.
#[derive(Clone)]
pub(crate) struct PoolEntry {
    pub(crate) pool: std::sync::Arc<WarmPool>,
    #[cfg(not(windows))]
    pub(crate) sem: std::sync::Arc<tokio::sync::Semaphore>,
    #[cfg(not(windows))]
    pub(crate) max_size: usize,
}

/// Boot-time dimensions that define whether a pre-warmed VM can satisfy a run.
///
/// Image alone is not enough: virtio-fs mounts, vCPUs, and memory are fixed in
/// the VM spec at boot. Keep those in the key so a request with a workspace bind
/// mount does not accidentally acquire a sandbox that lacks it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PoolKey {
    image: String,
    volumes: Vec<String>,
    vcpus: u32,
    memory_mb: u32,
}

impl PoolKey {
    pub(crate) fn default_for_image(image: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            volumes: Vec::new(),
            vcpus: DEFAULT_POOL_VCPUS,
            memory_mb: DEFAULT_POOL_MEMORY_MB,
        }
    }

    #[cfg(any(not(windows), test))]
    pub(crate) fn from_request(image: String, req: &PoolRunRequest) -> Self {
        Self {
            image,
            volumes: req.volumes.clone(),
            vcpus: req.vcpus.unwrap_or(DEFAULT_POOL_VCPUS),
            memory_mb: req.memory_mb.unwrap_or(DEFAULT_POOL_MEMORY_MB),
        }
    }

    #[cfg(not(windows))]
    pub(crate) fn from_lease(image: String, req: &PoolLeaseRequest) -> Self {
        Self {
            image,
            volumes: req.volumes.clone(),
            vcpus: req.vcpus.unwrap_or(DEFAULT_POOL_VCPUS),
            memory_mb: req.memory_mb.unwrap_or(DEFAULT_POOL_MEMORY_MB),
        }
    }

    #[cfg(any(not(windows), test))]
    pub(crate) fn label(&self) -> String {
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
pub(crate) struct LeasedVm {
    key: PoolKey,
    vm: std::sync::Arc<tokio::sync::Mutex<crate::VmManager>>,
    last_used_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    active_execs: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

#[cfg(not(windows))]
struct LeaseExecGuard {
    last_used_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    active_execs: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

pub(crate) struct PoolRequestGuard {
    inflight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    notify: std::sync::Arc<tokio::sync::Notify>,
}

impl Drop for PoolRequestGuard {
    fn drop(&mut self) {
        self.inflight
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        self.notify.notify_waiters();
    }
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
pub(crate) struct PoolRegistry {
    pub(crate) pools: tokio::sync::Mutex<std::collections::HashMap<PoolKey, PoolEntry>>,
    /// Per-key initialization locks. Weak values avoid retaining image keys
    /// after all callers have finished creating or using a pool.
    pub(crate) pool_initializers: tokio::sync::Mutex<
        std::collections::HashMap<PoolKey, std::sync::Weak<tokio::sync::Mutex<()>>>,
    >,
    /// Set before shutdown drains pools so a concurrent lazy initializer cannot
    /// publish a newly booted pool after the daemon has begun teardown.
    pub(crate) draining: std::sync::atomic::AtomicBool,
    /// Number of socket handlers still running. Shutdown waits for these
    /// handlers so their acquired VMs can be destroyed before the process exits.
    pub(crate) inflight_requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) inflight_notify: std::sync::Arc<tokio::sync::Notify>,
    #[cfg(not(windows))]
    pub(crate) leases: tokio::sync::Mutex<std::collections::HashMap<String, LeasedVm>>,
    #[cfg(not(windows))]
    pub(crate) default_image: Option<String>,
    pub(crate) size: usize,
    pub(crate) max: usize,
    pub(crate) ttl: u64,
    #[cfg(not(windows))]
    pub(crate) lease_ttl: u64,
    /// When true, pooled VMs boot IDLE and `pool run` spawns the command as the
    /// box's real MAIN (full box semantics), instead of exec-into-keepalive.
    pub(crate) deferred: bool,
    /// Mark pooled VM memory KSM-mergeable (host page dedup across same-image VMs).
    pub(crate) ksm: bool,
    /// Fill the pool by snapshot-fork (one template, restore the rest).
    pub(crate) snapshot_fork: bool,
    /// Maximum number of VM boots in flight for every lazily-created pool.
    pub(crate) boot_concurrency: usize,
    /// Daemon-wide boot limiter shared by every image pool.
    pub(crate) boot_limiter: std::sync::Arc<tokio::sync::Semaphore>,
    /// Optional Prometheus metrics shared across every pool this registry
    /// creates, so warm_pool hit/miss + vm_boot/cache numbers are scrapeable
    /// from the long-lived daemon (the one process where they matter most).
    pub(crate) metrics: Option<crate::RuntimeMetrics>,
}

impl PoolRegistry {
    pub(crate) fn request_guard(&self) -> PoolRequestGuard {
        self.inflight_requests
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        PoolRequestGuard {
            inflight: self.inflight_requests.clone(),
            notify: self.inflight_notify.clone(),
        }
    }

    pub(crate) async fn wait_for_requests(&self, timeout: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.inflight_notify.notified();
            if self
                .inflight_requests
                .load(std::sync::atomic::Ordering::Acquire)
                == 0
            {
                return true;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() || tokio::time::timeout(remaining, notified).await.is_err() {
                return self
                    .inflight_requests
                    .load(std::sync::atomic::Ordering::Acquire)
                    == 0;
            }
        }
    }

    async fn pool_creation_lock(&self, key: &PoolKey) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        let mut initializers = self.pool_initializers.lock().await;
        initializers.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = initializers.get(key).and_then(std::sync::Weak::upgrade) {
            return lock;
        }

        let lock = std::sync::Arc::new(tokio::sync::Mutex::new(()));
        initializers.insert(key.clone(), std::sync::Arc::downgrade(&lock));
        lock
    }

    /// The pool entry for `image`, lazily started (and pre-warmed in the background)
    /// on first use, with `min_idle = size`. Initialization is serialized only
    /// for the exact pool key; unrelated image/resource shapes can initialize
    /// concurrently and are still bounded by the daemon-wide boot limiter.
    pub(crate) async fn get_or_create_with_size(
        &self,
        key: PoolKey,
        size: usize,
    ) -> Result<PoolEntry, String> {
        if size > self.max {
            return Err(format!(
                "requested pool size ({size}) cannot exceed daemon --max ({})",
                self.max
            ));
        }

        let creation_lock = self.pool_creation_lock(&key).await;
        let _creation_guard = creation_lock.lock().await;

        if self.draining.load(std::sync::atomic::Ordering::Acquire) {
            return Err("warm-pool daemon is shutting down".to_string());
        }
        {
            let pools = self.pools.lock().await;
            if let Some(entry) = pools.get(&key) {
                return Ok(entry.clone());
            }
        }

        let max_size = self.max.max(size);
        let pool_config = PoolConfig {
            enabled: true,
            min_idle: size,
            max_size,
            max_concurrent_boots: self.boot_concurrency,
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
            deferred_main: self.deferred || self.snapshot_fork,
            ksm: self.ksm,
            ..Default::default()
        };
        let pool = WarmPool::start_with_metrics_and_boot_limiter_first_ready(
            pool_config,
            box_config,
            EventEmitter::new(256),
            self.metrics.clone(),
            Some(self.boot_limiter.clone()),
        )
        .await
        .map_err(|e| e.to_string())?;
        let pool = std::sync::Arc::new(pool);
        let entry = PoolEntry {
            pool: pool.clone(),
            #[cfg(not(windows))]
            sem: std::sync::Arc::new(tokio::sync::Semaphore::new(max_size)),
            #[cfg(not(windows))]
            max_size,
        };

        let publish = {
            let mut pools = self.pools.lock().await;
            if self.draining.load(std::sync::atomic::Ordering::Acquire) {
                false
            } else {
                pools.insert(key, entry.clone());
                true
            }
        };
        if !publish {
            pool.signal_shutdown();
            let _ = pool.drain_idle().await;
            return Err("warm-pool daemon is shutting down".to_string());
        }
        Ok(entry)
    }

    /// Lazy pool for `key` at the daemon's default size.
    pub(crate) async fn get_or_create(&self, key: PoolKey) -> Result<PoolEntry, String> {
        self.get_or_create_with_size(key, self.size).await
    }

    /// Lease pools with boot-time volumes are usually build-stage rootfs mounts:
    /// unique, short-lived, and useful only to the single holder. Do not pre-warm
    /// a whole pool for those keys; acquire will cold-fill exactly the VM needed.
    #[cfg(not(windows))]
    fn lease_min_idle(&self, key: &PoolKey) -> usize {
        if key.volumes.is_empty() {
            self.size
        } else {
            0
        }
    }

    /// Resolve the image for a request: the requested one, else the daemon default.
    #[cfg(not(windows))]
    pub(crate) fn resolve_image(&self, requested: Option<String>) -> Option<String> {
        requested.or_else(|| self.default_image.clone())
    }

    /// Stop replenishment and destroy idle VMs across all pools (shutdown).
    #[cfg(not(windows))]
    pub(crate) async fn drain_all(&self) {
        self.draining
            .store(true, std::sync::atomic::Ordering::Release);
        // Remove leases from the registry before awaiting VM destruction so
        // concurrent lease requests cannot be blocked by a slow teardown.
        let leases = {
            let mut leases = self.leases.lock().await;
            leases.drain().collect::<Vec<_>>()
        };

        // Lease teardown is independent per VM. Keep a small bounded fan-out so
        // a large lease set does not serialize shutdown or overwhelm the host.
        let mut pending_leases = leases.into_iter();
        let mut lease_tasks = JoinSet::new();
        for _ in 0..POOL_DRAIN_CONCURRENCY {
            if let Some((lease_id, leased)) = pending_leases.next() {
                lease_tasks.spawn(async move { destroy_leased_vm(lease_id, leased).await });
            }
        }
        while let Some(result) = lease_tasks.join_next().await {
            match result {
                Ok((lease_id, box_id, Ok(()))) => {
                    tracing::debug!(
                        %lease_id,
                        %box_id,
                        "Destroyed warm-pool lease during shutdown"
                    );
                }
                Ok((lease_id, box_id, Err(error))) => {
                    tracing::warn!(
                        %lease_id,
                        %box_id,
                        %error,
                        "Failed to destroy warm-pool lease during shutdown"
                    );
                }
                Err(error) => {
                    tracing::warn!(%error, "Warm-pool lease teardown task failed");
                }
            }
            if let Some((lease_id, leased)) = pending_leases.next() {
                lease_tasks.spawn(async move { destroy_leased_vm(lease_id, leased).await });
            }
        }

        // Snapshot pool handles and release the map lock before draining. Each
        // pool detaches its idle VMs before awaiting their destruction, so other
        // registry operations are not serialized behind the full shutdown.
        let pools = {
            let pools = self.pools.lock().await;
            pools
                .values()
                .map(|entry| entry.pool.clone())
                .collect::<Vec<_>>()
        };
        // Broadcast the stop signal before awaiting any teardown so every
        // maintenance loop exits promptly, even when there are more pools than
        // drain workers.
        for pool in &pools {
            pool.signal_shutdown();
        }

        let mut pending_pools = pools.into_iter();
        let mut pool_tasks = JoinSet::new();
        for _ in 0..POOL_DRAIN_CONCURRENCY {
            if let Some(pool) = pending_pools.next() {
                pool_tasks.spawn(async move { pool.drain_idle().await });
            }
        }
        while let Some(result) = pool_tasks.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(%error, "Failed to drain warm pool during shutdown");
                }
                Err(error) => {
                    tracing::warn!(%error, "Warm-pool drain task failed");
                }
            }
            if let Some(pool) = pending_pools.next() {
                pool_tasks.spawn(async move { pool.drain_idle().await });
            }
        }
    }

    /// Snapshot live per-image stats, sorted by image name.
    #[cfg(not(windows))]
    pub(crate) async fn stats(&self) -> Vec<PoolImageStat> {
        let pools = {
            let pools = self.pools.lock().await;
            pools
                .iter()
                .map(|(key, entry)| (key.clone(), entry.clone()))
                .collect::<Vec<_>>()
        };
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
            let leased = leased_by_key.get(&key).copied().unwrap_or(0);
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
    pub(crate) async fn lease_vm(&self, req: PoolLeaseRequest) -> Result<String, String> {
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
    pub(crate) async fn exec_lease(&self, req: PoolLeaseExecRequest) -> PoolRunResponse {
        let (vm, _guard) = {
            let leases = self.leases.lock().await;
            let Some(leased) = leases.get(&req.lease_id) else {
                return err_resp(format!("unknown pool lease '{}'", req.lease_id));
            };
            (leased.vm.clone(), LeaseExecGuard::new(leased))
        };
        let request_id = match resolve_pool_lease_request_id(req.request_id) {
            Ok(request_id) => request_id,
            Err(error) => return err_resp(error),
        };
        let output = vm
            .lock()
            .await
            .exec_request(&a3s_box_core::exec::ExecRequest {
                request_id: Some(request_id.clone()),
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
            Err(e) => err_resp(annotate_pool_lease_unavailable(e.to_string(), &request_id)),
        }
    }

    #[cfg(not(windows))]
    pub(crate) async fn release_lease(&self, req: PoolLeaseReleaseRequest) -> Option<String> {
        let leased = match self.leases.lock().await.remove(&req.lease_id) {
            Some(leased) => leased,
            None => return Some(format!("unknown pool lease '{}'", req.lease_id)),
        };
        let mut vm = leased.vm.lock().await;
        destroy_vm_or_reap(&mut vm)
            .await
            .err()
            .map(|e| e.to_string())
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
    pub(crate) async fn reap_expired_leases(&self) -> usize {
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
            let mut vm = leased.vm.lock().await;
            match destroy_vm_or_reap(&mut vm).await {
                Ok(()) => count += 1,
                Err(error) => {
                    tracing::warn!(
                        %lease_id,
                        box_id = %vm.box_id(),
                        %error,
                        "Failed to destroy expired warm-pool lease after orphan reap attempt"
                    );
                    count += 1;
                }
            }
        }
        count
    }
}

/// Destroy a VM; on Linux, best-effort reap leftovers if destroy fails.
#[cfg(not(windows))]
pub(crate) async fn destroy_vm_or_reap(
    vm: &mut crate::VmManager,
) -> Result<(), a3s_box_core::error::BoxError> {
    let box_id = vm.box_id().to_string();
    let result = vm.destroy().await;
    if let Err(error) = &result {
        tracing::warn!(
            %box_id,
            %error,
            "VM destroy failed; attempting orphan reap"
        );
        #[cfg(target_os = "linux")]
        reap_orphaned_box(&box_id);
    }
    result
}

/// Destroy a leased VM; on Linux, reap orphans if destroy fails.
#[cfg(not(windows))]
async fn destroy_leased_vm(
    lease_id: String,
    leased: LeasedVm,
) -> (String, String, Result<(), a3s_box_core::error::BoxError>) {
    let mut vm = leased.vm.lock().await;
    let box_id = vm.box_id().to_string();
    let result = destroy_vm_or_reap(&mut vm).await;
    (lease_id, box_id, result)
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
pub(crate) async fn reap_expired_leases_task(
    registry: std::sync::Arc<PoolRegistry>,
    lease_ttl_secs: u64,
) {
    let mut interval = tokio::time::interval(lease_reaper_interval(lease_ttl_secs));
    loop {
        interval.tick().await;
        let reaped = registry.reap_expired_leases().await;
        if reaped > 0 {
            tracing::warn!(reaped, "Reaped expired warm-pool leases");
        }
    }
}

#[cfg(not(windows))]
pub(crate) fn err_resp(msg: impl Into<String>) -> PoolRunResponse {
    PoolRunResponse {
        stdout: vec![],
        stderr: vec![],
        exit_code: -1,
        error: Some(msg.into()),
    }
}

#[cfg(not(windows))]
const MAX_POOL_REQUEST_ID_BYTES: usize = 512;

#[cfg(not(windows))]
fn mint_cli_pool_request_id() -> String {
    format!("cli-pool-{}", uuid::Uuid::new_v4().simple())
}

#[cfg(not(windows))]
fn resolve_pool_lease_request_id(request_id: Option<String>) -> Result<String, String> {
    match request_id {
        Some(request_id) => {
            if request_id.is_empty()
                || request_id.len() > MAX_POOL_REQUEST_ID_BYTES
                || request_id.contains('\0')
            {
                return Err(
                    "pool lease exec request_id must be a non-empty UTF-8 string of at most 512 bytes without NUL"
                        .to_string(),
                );
            }
            Ok(request_id)
        }
        None => Ok(mint_cli_pool_request_id()),
    }
}

#[cfg(not(windows))]
fn annotate_pool_lease_unavailable(message: String, request_id: &str) -> String {
    let lower = message.to_ascii_lowercase();
    let retryable = lower.contains("unavailable")
        || lower.contains("closed without response")
        || lower.contains("response timed out")
        || lower.contains("response read failed")
        || lower.contains("connection failed");
    if retryable {
        format!("{message} (reuse request_id {request_id} on the same lease)")
    } else {
        message
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
