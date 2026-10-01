//! Start the warm-pool daemon: reap orphans, bind the socket, pre-warm.
//!
//! Stdout banners stay in the CLI. This function returns once pre-warm has
//! succeeded and the accept loop is running.

#[cfg(not(windows))]
use super::registry::{reap_expired_leases_task, PoolKey, PoolRegistry};
#[cfg(not(windows))]
use super::serve::{serve, serve_pool_metrics};
use super::warm_pool::PoolStats;
#[cfg(not(windows))]
use crate::vm::reap::reap_orphaned_boxes_for_home;
use a3s_box_core::error::BoxError;

/// Parsed `pool start` options. The CLI fills this from clap.
#[derive(Clone, Debug)]
pub struct PoolDaemonConfig {
    pub image: Option<String>,
    pub size: usize,
    pub max: usize,
    pub boot_concurrency: usize,
    pub ttl: u64,
    pub lease_ttl: u64,
    pub socket: String,
    pub warm: Vec<String>,
    pub deferred: bool,
    pub ksm: bool,
    pub snapshot_fork: bool,
    pub metrics_addr: Option<String>,
    pub json: bool,
}

/// What pre-warm produced. The CLI prints it.
#[derive(Debug)]
pub struct PoolDaemonReport {
    pub default_stats: Option<(String, PoolStats)>,
    pub warmed_extra: Vec<(String, usize)>,
}

/// Accept loop left running after a successful pre-warm.
pub struct PoolDaemon {
    pub report: PoolDaemonReport,
    #[cfg(not(windows))]
    serve_task: tokio::task::JoinHandle<Result<(), String>>,
}

impl PoolDaemon {
    #[cfg(not(windows))]
    pub async fn wait(self) -> Result<(), BoxError> {
        self.serve_task
            .await
            .map_err(|error| BoxError::PoolError(format!("pool daemon task failed: {error}")))?
            .map_err(BoxError::PoolError)?;
        Ok(())
    }
}

/// Reject impossible sizes before any socket or VM work.
pub fn validate_pool_daemon_config(config: &PoolDaemonConfig) -> Result<(), String> {
    if config.size == 0 {
        return Err("--size must be greater than 0".to_string());
    }
    if config.size > config.max {
        return Err(format!(
            "--size ({}) cannot exceed --max ({})",
            config.size, config.max
        ));
    }
    if config.boot_concurrency == 0 {
        return Err("--boot-concurrency must be greater than 0".to_string());
    }
    Ok(())
}

/// Parse a `--warm` entry of the form `image[=count]` (count defaults to `default_size`).
pub(crate) fn parse_warm_spec(entry: &str, default_size: usize) -> Result<(String, usize), String> {
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

#[cfg(not(windows))]
pub async fn start_pool_daemon(config: PoolDaemonConfig) -> Result<PoolDaemon, BoxError> {
    // Pool VM ownership is in-memory only. After a previous daemon SIGKILL,
    // orphan shims stay reparented to init with box dirs under A3S_HOME.
    // Reap them before bind/prewarm so pool stop / capacity cannot hide
    // invisible MicroVMs (#373). Does not reattach and does not kill
    // shims still owned by a live parent.
    let home = a3s_box_core::dirs_home();
    let reaped = reap_orphaned_boxes_for_home(&home);
    if reaped > 0 {
        eprintln!(
            "reaped {reaped} orphan pool microVM(s) under {}",
            home.display()
        );
    }

    // Optional Prometheus metrics for the long-lived daemon. One shared registry
    // is handed to every pool (set_metrics) and to the /metrics server; cloning a
    // RuntimeMetrics shares the underlying registry, so the server scrapes what the
    // pools record (warm_pool hit/miss, vm_boot, boot phases, cache).
    let metrics = if config.metrics_addr.is_some() {
        crate::RuntimeMetrics::try_new().ok()
    } else {
        None
    };

    let registry = std::sync::Arc::new(PoolRegistry {
        pools: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        pool_initializers: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        draining: std::sync::atomic::AtomicBool::new(false),
        inflight_requests: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        inflight_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
        #[cfg(not(windows))]
        leases: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        #[cfg(not(windows))]
        default_image: config.image.clone(),
        size: config.size,
        max: config.max,
        boot_concurrency: config.boot_concurrency,
        ttl: config.ttl,
        #[cfg(not(windows))]
        lease_ttl: config.lease_ttl,
        deferred: config.deferred,
        ksm: config.ksm,
        snapshot_fork: config.snapshot_fork,
        metrics: metrics.clone(),
        boot_limiter: std::sync::Arc::new(tokio::sync::Semaphore::new(config.boot_concurrency)),
    });

    // Serve /metrics alongside the pool socket, if requested.
    if let (Some(addr), Some(metrics)) = (config.metrics_addr.clone(), metrics) {
        tokio::spawn(serve_pool_metrics(addr, metrics));
    }

    #[cfg(not(windows))]
    if config.lease_ttl > 0 {
        tokio::spawn(reap_expired_leases_task(registry.clone(), config.lease_ttl));
    }

    // Bind the control socket before pre-warming. Large images can take longer
    // than the autostart client's safety cap to cold boot; keeping the daemon
    // undiscoverable until that work completed made a healthy startup look like
    // a timeout. Requests may connect immediately and naturally wait on the
    // per-pool creation lock until the first VM is truly exec-ready.
    #[cfg(not(windows))]
    let serve_task = {
        let serve_registry = registry.clone();
        let serve_socket = config.socket.clone();
        let serve_json = config.json;
        tokio::spawn(async move {
            serve(serve_registry, &serve_socket, serve_json)
                .await
                .map_err(|error| error.to_string())
        })
    };

    let prewarm_result = async {
        // Validate all explicit warm sizes before booting the default image.
        // This avoids wasting VM boots when a multi-image deployment contains
        // a capacity typo, and keeps --max a hard per-image resource bound.
        let mut warm_specs: Vec<(String, usize)> = Vec::with_capacity(config.warm.len());
        for entry in &config.warm {
            let (image, count) =
                parse_warm_spec(entry, config.size).map_err(BoxError::ConfigError)?;
            if count == 0 {
                return Err(BoxError::ConfigError(format!(
                    "--warm count must be > 0 (in '{entry}')"
                )));
            }
            if count > config.max {
                return Err(BoxError::ConfigError(format!(
                    "--warm count ({count}) cannot exceed --max ({}) (in '{entry}')",
                    config.max
                )));
            }
            warm_specs.push((image, count));
        }

        // Pre-warm the default image, if one was given.
        let default_stats = if let Some(ref image) = config.image {
            let entry = registry
                .get_or_create(PoolKey::default_for_image(image.clone()))
                .await
                .map_err(BoxError::PoolError)?;
            Some((image.clone(), entry.pool.stats().await))
        } else {
            None
        };

        // Pre-warm any extra images requested via --warm.
        for (image, count) in &warm_specs {
            registry
                .get_or_create_with_size(PoolKey::default_for_image(image), *count)
                .await
                .map_err(BoxError::PoolError)?;
        }

        Ok::<_, BoxError>((default_stats, warm_specs))
    }
    .await;

    let (default_stats, warmed_extra) = match prewarm_result {
        Ok(result) => result,
        Err(error) => {
            // The socket is already bound so clients can observe startup
            // progress. If pre-warming fails, tear down every background task
            // and pool created so far before returning the error; otherwise a
            // partial Dify deployment can leave a live socket and orphan VMs.
            #[cfg(not(windows))]
            {
                registry.drain_all().await;
                serve_task.abort();
                let _ = serve_task.await;
                let _ = std::fs::remove_file(&config.socket);
            }
            return Err(error);
        }
    };

    Ok(PoolDaemon {
        report: PoolDaemonReport {
            default_stats,
            warmed_extra,
        },
        serve_task,
    })
}

#[cfg(windows)]
pub async fn start_pool_daemon(_config: PoolDaemonConfig) -> Result<PoolDaemon, BoxError> {
    Err(BoxError::ConfigError(
        "`pool start` is not supported on Windows; the warm pool daemon requires a Unix socket and is unavailable on WHPX"
            .into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
