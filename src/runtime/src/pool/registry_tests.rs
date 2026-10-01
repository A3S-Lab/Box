use super::*;

#[test]
fn test_keepalive_cmd_is_a_sleep_loop() {
    let c = keepalive_cmd();
    assert_eq!(c[0], "/bin/sh");
    assert!(c.last().unwrap().contains("sleep"));
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
fn resolve_pool_lease_request_id_mints_cli_prefix_when_omitted() {
    let minted = resolve_pool_lease_request_id(None).expect("mint");
    assert!(
        minted.starts_with("cli-pool-"),
        "unexpected request_id: {minted}"
    );
}

#[cfg(not(windows))]
#[test]
fn resolve_pool_lease_request_id_rejects_invalid_ids() {
    assert!(resolve_pool_lease_request_id(Some(String::new())).is_err());
    assert!(resolve_pool_lease_request_id(Some("bad\0id".to_string())).is_err());
    assert!(resolve_pool_lease_request_id(Some("x".repeat(513))).is_err());
    assert_eq!(
        resolve_pool_lease_request_id(Some("caller-stable-pool-1".to_string())).as_deref(),
        Ok("caller-stable-pool-1")
    );
}

#[cfg(not(windows))]
#[test]
fn annotate_pool_lease_unavailable_surfaces_request_id() {
    let annotated = annotate_pool_lease_unavailable(
        "Exec server closed without response".to_string(),
        "cli-pool-abc",
    );
    assert!(
        annotated.contains("reuse request_id cli-pool-abc on the same lease"),
        "{annotated}"
    );
}

#[cfg(not(windows))]
fn test_registry_with_lease_ttl(lease_ttl: u64) -> std::sync::Arc<PoolRegistry> {
    std::sync::Arc::new(PoolRegistry {
        pools: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        pool_initializers: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        draining: std::sync::atomic::AtomicBool::new(false),
        inflight_requests: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        inflight_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
        leases: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        default_image: Some("alpine:latest".to_string()),
        size: 1,
        max: 4,
        ttl: 0,
        lease_ttl,
        deferred: false,
        ksm: false,
        snapshot_fork: false,
        boot_concurrency: DEFAULT_POOL_BOOT_CONCURRENCY,
        metrics: None,
        boot_limiter: std::sync::Arc::new(tokio::sync::Semaphore::new(
            DEFAULT_POOL_BOOT_CONCURRENCY,
        )),
    })
}

#[cfg(not(windows))]
#[tokio::test]
async fn test_pool_creation_lock_is_key_scoped() {
    let registry = test_registry_with_lease_ttl(0);
    let first_key = PoolKey::default_for_image("alpine:latest");
    let second_key = PoolKey::default_for_image("busybox:latest");
    let first = registry.pool_creation_lock(&first_key).await;
    let first_again = registry.pool_creation_lock(&first_key).await;
    let second = registry.pool_creation_lock(&second_key).await;

    assert!(std::sync::Arc::ptr_eq(&first, &first_again));
    assert!(!std::sync::Arc::ptr_eq(&first, &second));
}

#[cfg(not(windows))]
#[tokio::test]
async fn test_draining_registry_rejects_lazy_pool_creation() {
    let registry = test_registry_with_lease_ttl(0);
    registry
        .draining
        .store(true, std::sync::atomic::Ordering::Release);

    let result = registry
        .get_or_create(PoolKey::default_for_image("alpine:latest"))
        .await;

    assert!(matches!(
        result,
        Err(error) if error == "warm-pool daemon is shutting down"
    ));
    assert!(registry.pools.lock().await.is_empty());
}

#[cfg(not(windows))]
#[tokio::test]
async fn test_registry_rejects_pool_size_above_max_without_booting() {
    let registry = test_registry_with_lease_ttl(0);
    let result = registry
        .get_or_create_with_size(PoolKey::default_for_image("alpine:latest"), 5)
        .await;

    assert!(matches!(
        result,
        Err(error) if error.contains("requested pool size") && error.contains("--max")
    ));
    assert!(registry.pools.lock().await.is_empty());
}

#[cfg(not(windows))]
#[tokio::test]
async fn test_drain_all_marks_registry_draining_and_clears_empty_state() {
    let registry = test_registry_with_lease_ttl(0);

    registry.drain_all().await;

    assert!(registry.draining.load(std::sync::atomic::Ordering::Acquire));
    assert!(registry.leases.lock().await.is_empty());
    assert!(registry.pools.lock().await.is_empty());
}

#[cfg(not(windows))]
#[tokio::test]
async fn test_request_guard_waits_for_inflight_handlers() {
    let registry = test_registry_with_lease_ttl(0);
    let guard = registry.request_guard();

    assert_eq!(
        registry
            .inflight_requests
            .load(std::sync::atomic::Ordering::Acquire),
        1
    );
    assert!(
        !registry
            .wait_for_requests(std::time::Duration::from_millis(10))
            .await
    );

    drop(guard);
    assert!(
        registry
            .wait_for_requests(std::time::Duration::from_secs(1))
            .await
    );
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
    let vm = crate::VmManager::with_box_id(
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
            active_execs: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(active_execs)),
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
