use super::*;

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
        boot_concurrency: DEFAULT_POOL_BOOT_CONCURRENCY,
        metrics_addr: None,
        json: false,
    };
    let result = execute_start(args).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("greater than 0"));
}

#[tokio::test]
async fn zero_pool_size_is_a_configuration_error() {
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
        boot_concurrency: DEFAULT_POOL_BOOT_CONCURRENCY,
        metrics_addr: None,
        json: false,
    };
    let err = execute_start(args).await.unwrap_err();
    match err {
        a3s_box_core::error::BoxError::ConfigError(message) => {
            assert!(message.contains("greater than 0"), "{message}");
        }
        other => panic!("expected ConfigError, got {other:?}"),
    }
}

#[cfg(windows)]
#[tokio::test]
async fn windows_pool_autostart_is_a_configuration_error() {
    let err = ensure_pool_daemon_running(&PoolAutoStartConfig {
        socket: "/tmp/a3s-box-pool-unused.sock".to_string(),
        image: None,
        size: 1,
        max: 1,
    })
    .await
    .unwrap_err();
    match err {
        a3s_box_core::error::BoxError::ConfigError(message) => {
            assert!(message.contains("not supported"), "{message}");
        }
        other => panic!("expected ConfigError, got {other:?}"),
    }
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
        boot_concurrency: DEFAULT_POOL_BOOT_CONCURRENCY,
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

#[tokio::test]
async fn test_execute_start_zero_boot_concurrency_fails_before_startup() {
    let args = PoolStartArgs {
        image: None,
        size: 1,
        max: 1,
        boot_concurrency: 0,
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
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("--boot-concurrency must be greater than 0"));
}

#[cfg(not(windows))]
#[tokio::test]
async fn test_execute_start_prewarm_failure_cleans_up_socket() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("failed-pool.sock");
    let args = PoolStartArgs {
        image: None,
        size: 1,
        max: 1,
        ttl: 300,
        lease_ttl: DEFAULT_POOL_LEASE_TTL_SECS,
        socket: socket.display().to_string(),
        warm: vec!["alpine=not-a-count".to_string()],
        deferred: false,
        ksm: false,
        snapshot_fork: false,
        boot_concurrency: DEFAULT_POOL_BOOT_CONCURRENCY,
        metrics_addr: None,
        json: true,
    };

    let result = execute_start(args).await;

    assert!(result
        .unwrap_err()
        .to_string()
        .contains("invalid warm count"));
    assert!(!socket.exists(), "failed startup must remove its socket");
}

#[cfg(not(windows))]
#[tokio::test]
async fn test_execute_start_warm_count_exceeding_max_fails_before_boot() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("oversized-pool.sock");
    let args = PoolStartArgs {
        image: Some("alpine:latest".to_string()),
        size: 1,
        max: 1,
        ttl: 300,
        lease_ttl: DEFAULT_POOL_LEASE_TTL_SECS,
        socket: socket.display().to_string(),
        warm: vec!["busybox:latest=2".to_string()],
        deferred: false,
        ksm: false,
        snapshot_fork: false,
        boot_concurrency: DEFAULT_POOL_BOOT_CONCURRENCY,
        metrics_addr: None,
        json: true,
    };

    let result = execute_start(args).await;

    let error = result.unwrap_err().to_string();
    assert!(error.contains("cannot exceed --max"));
    assert!(!socket.exists(), "failed startup must remove its socket");
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
async fn test_execute_stop_json_reports_reaped_when_daemon_absent() {
    // Socket-absent stop must still succeed and advertise the home-fenced
    // orphan reap count (0 when this process tree has no matching PPID-1
    // shims). Proves stop no longer short-circuits before crash recovery.
    let result = execute_stop(PoolStopArgs {
        socket: "/tmp/a3s-box-pool-does-not-exist-json.sock".to_string(),
        json: true,
    })
    .await;
    assert!(result.is_ok());
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
