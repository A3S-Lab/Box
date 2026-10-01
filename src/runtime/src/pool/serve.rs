//! Pool daemon socket: accept run and lease frames until shutdown.
//!
//! Request handling calls the registry. Framing lives in `client`.

#[cfg(not(windows))]
use super::client::{
    read_frame_with_timeout, write_frame, PoolLeaseReleaseResponse, PoolLeaseResponse, PoolRequest,
    PoolRunResponse, PoolStatusResponse, PoolStopResponse, FRAME_READ_TIMEOUT,
};
#[cfg(not(windows))]
use super::registry::{deferred_spec_json, destroy_vm_or_reap, err_resp, PoolKey, PoolRegistry};

#[cfg(not(windows))]
const POOL_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Serve a Prometheus `/metrics` endpoint exposing the pool daemon's runtime
/// metrics (warm_pool hit/miss, vm_boot, boot phases, cache). Minimal raw-HTTP server,
/// mirroring the monitor's metrics endpoint.
pub(crate) async fn serve_pool_metrics(addr: String, metrics: crate::RuntimeMetrics) {
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

/// Resolve when the pool daemon should drain: SIGTERM/SIGINT on Unix, Ctrl-C
/// elsewhere. Mirrors `monitor_shutdown_signal` so `kill` (SIGTERM) from benches
/// and supervisors runs the same drain path as Ctrl-C / `pool stop`.
#[cfg(unix)]
async fn pool_shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    match (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) {
        (Ok(mut sigterm), Ok(mut sigint)) => {
            tokio::select! {
                _ = sigterm.recv() => {}
                _ = sigint.recv() => {}
            }
        }
        _ => std::future::pending::<()>().await,
    }
}

#[cfg(all(not(unix), not(windows)))]
async fn pool_shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(not(windows))]
async fn drain_pool_registry_on_shutdown(registry: &PoolRegistry, socket: &str, json: bool) {
    let _ = std::fs::remove_file(socket);
    if !json {
        println!("Draining warm pools...");
    }
    registry.drain_all().await;
    if !registry.wait_for_requests(POOL_DRAIN_TIMEOUT).await && !json {
        eprintln!("Timed out waiting for in-flight pool requests to finish");
    }
}

/// Accept `pool run` connections until SIGTERM/SIGINT, serving each request
/// concurrently so independent sandboxes don't queue behind one another. On
/// shutdown, stop the replenisher and destroy idle VMs (in-flight requests keep
/// their own acquired VM).
#[cfg(not(windows))]
pub(crate) async fn serve(
    registry: std::sync::Arc<PoolRegistry>,
    socket: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use tokio::net::UnixListener;

    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    if !json {
        println!(
            "Listening on {} (Ctrl-C or SIGTERM to drain and stop)",
            socket
        );
    }

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (mut stream, _) = accepted?;
                let registry = registry.clone();
                let shutdown_tx = shutdown_tx.clone();
                tokio::spawn(async move {
                    let _request_guard = registry.request_guard();
                    if let Err(e) = handle_conn(&registry, &shutdown_tx, &mut stream).await {
                        tracing::warn!(error = %e, "pool connection failed");
                    }
                });
            }
            _ = shutdown_rx.recv() => {
                drain_pool_registry_on_shutdown(&registry, socket, json).await;
                break;
            }
            _ = pool_shutdown_signal() => {
                drain_pool_registry_on_shutdown(&registry, socket, json).await;
                break;
            }
        }
    }
    Ok(())
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

    let req: PoolRequest =
        serde_json::from_slice(&read_frame_with_timeout(stream, FRAME_READ_TIMEOUT).await?)
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
                            // One-shot pool run destroys the VM after the
                            // response, so a keyed request_id cannot span
                            // client retries (new VM, empty replay cache).
                            // Lease exec mints/reuses `cli-pool-*` instead.
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

    // The response is sent before teardown, so the client does not pay for VM
    // destruction. Keep the teardown in this request task instead of detaching
    // it: the request guard remains live until the VM and its host resources are
    // actually gone, allowing daemon shutdown to wait for a complete cleanup.
    // This task does not block the accept loop because every connection already
    // runs in its own Tokio task. Perform cleanup even when the client disconnects
    // while the response is being written.
    let write_result = write_frame(stream, &bytes).await;
    if let Some((mut vm, permit)) = used {
        if let Err(error) = destroy_vm_or_reap(&mut vm).await {
            tracing::warn!(%error, "failed to destroy pooled VM after request");
        }
        drop(permit);
    }
    write_result
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::super::client::stop_client;
    use super::super::registry::{DEFAULT_POOL_BOOT_CONCURRENCY, DEFAULT_POOL_LEASE_TTL_SECS};
    use super::*;

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

    #[tokio::test]
    async fn test_stop_request_shuts_down_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("pool.sock");
        let socket_arg = socket.display().to_string();
        let server_socket = socket_arg.clone();
        let registry = std::sync::Arc::new(PoolRegistry {
            pools: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            pool_initializers: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            draining: std::sync::atomic::AtomicBool::new(false),
            inflight_requests: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            inflight_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
            leases: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            default_image: None,
            size: 1,
            max: 1,
            ttl: 0,
            lease_ttl: DEFAULT_POOL_LEASE_TTL_SECS,
            deferred: false,
            ksm: false,
            snapshot_fork: false,
            boot_concurrency: DEFAULT_POOL_BOOT_CONCURRENCY,
            metrics: None,
            boot_limiter: std::sync::Arc::new(tokio::sync::Semaphore::new(
                DEFAULT_POOL_BOOT_CONCURRENCY,
            )),
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
}
