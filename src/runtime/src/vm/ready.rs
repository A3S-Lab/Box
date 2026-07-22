//! VM readiness checks — waiting for exec socket.

use a3s_box_core::error::{BoxError, Result};

#[cfg(unix)]
use crate::grpc::ExecClient;

use super::VmManager;

#[cfg(any(unix, test))]
const DEFAULT_EXEC_READY_TIMEOUT_MS: u64 = 15_000;
#[cfg(target_os = "windows")]
const DEFAULT_WINDOWS_GUEST_READY_TIMEOUT_MS: u64 = 30_000;
const EXEC_READY_PROGRESS_LOG_MS: u64 = 5_000;

fn parse_ready_timeout_ms(value: Option<&str>, default: u64) -> u64 {
    value
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|timeout| *timeout > 0)
        .unwrap_or(default)
}

#[cfg(any(unix, test))]
fn parse_exec_ready_timeout_ms(value: Option<&str>) -> u64 {
    parse_ready_timeout_ms(value, DEFAULT_EXEC_READY_TIMEOUT_MS)
}

#[cfg(unix)]
fn exec_ready_timeout_ms() -> u64 {
    parse_exec_ready_timeout_ms(std::env::var("A3S_EXEC_READY_TIMEOUT_MS").ok().as_deref())
}

#[cfg(target_os = "windows")]
fn windows_guest_ready_timeout_ms() -> u64 {
    parse_ready_timeout_ms(
        std::env::var("A3S_EXEC_READY_TIMEOUT_MS").ok().as_deref(),
        DEFAULT_WINDOWS_GUEST_READY_TIMEOUT_MS,
    )
}

impl VmManager {
    /// Confirm the VM didn't fail on launch (for generic OCI images without an agent).
    ///
    /// A bad config makes libkrun exit within milliseconds, so we only need a short
    /// window to catch an *immediate* crash and fail loudly. Poll for that instead
    /// of a fixed 1 s sleep — it shaved ~750 ms off every boot. Crashes that happen
    /// later are caught by `wait_for_exec_ready`'s `has_exited` checks, which gate
    /// the rest of boot anyway.
    pub(crate) async fn wait_for_vm_running(&self) -> Result<()> {
        // This is a crash-detection grace period, not a readiness wait: the VM
        // process is alive the instant the shim is spawned, and we just watch for it
        // exiting immediately. A snapshot-restored VM reaches its run loop in ~20ms
        // (no cold boot), so a short grace catches an immediate restore failure while
        // saving ~200ms on the fork fast-path; a cold boot keeps the longer grace.
        #[cfg(unix)]
        let max_wait_ms: u64 = if super::is_restore_mode(&self.config) {
            40
        } else {
            250
        };
        #[cfg(not(unix))]
        let max_wait_ms: u64 = 250;
        const POLL_MS: u64 = 10;

        tracing::debug!("Confirming VM process started");
        let start = std::time::Instant::now();
        loop {
            if let Some(ref handler) = *self.handler.read().await {
                // has_exited is zombie-aware (a halted VM's shim becomes a zombie);
                // is_running's kill(pid,0) would still report it alive.
                if handler.has_exited() {
                    return Err(BoxError::BoxBootError {
                        message: "VM process exited immediately after start".to_string(),
                        hint: Some("Check console output for errors".to_string()),
                    });
                }
            }
            if start.elapsed().as_millis() >= max_wait_ms as u128 {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(POLL_MS)).await;
        }

        tracing::debug!("VM process is running");
        Ok(())
    }

    /// Wait until Windows guest-init publishes this boot's unique readiness token.
    ///
    /// WHPX has no host-side exec transport, so process liveness alone cannot
    /// distinguish a running guest from a VMM whose vCPU is wedged before PID 1.
    /// The token is written through the shared rootfs after guest filesystem and
    /// network setup. A per-boot value prevents a stale persistent-rootfs marker
    /// from satisfying a new launch while the shim is still initializing.
    #[cfg(target_os = "windows")]
    pub(crate) async fn wait_for_windows_guest_ready(
        &self,
        rootfs_path: &std::path::Path,
        expected_token: &str,
    ) -> Result<()> {
        self.wait_for_windows_guest_ready_with_timeout(
            rootfs_path,
            expected_token,
            windows_guest_ready_timeout_ms(),
        )
        .await
    }

    #[cfg(target_os = "windows")]
    async fn wait_for_windows_guest_ready_with_timeout(
        &self,
        rootfs_path: &std::path::Path,
        expected_token: &str,
        max_wait_ms: u64,
    ) -> Result<()> {
        use a3s_box_core::guest_boot::{validate_guest_ready_token, GUEST_READY_PATH};
        use tokio::time::Duration;

        const POLL_INTERVAL: Duration = Duration::from_millis(20);

        validate_guest_ready_token(expected_token).map_err(|message| BoxError::BoxBootError {
            message: format!("Invalid Windows guest readiness token: {message}"),
            hint: None,
        })?;

        let marker = rootfs_path.join(GUEST_READY_PATH.trim_start_matches('/'));
        let start = std::time::Instant::now();
        let mut next_progress_log_ms = EXEC_READY_PROGRESS_LOG_MS;

        loop {
            if super::windows_marker_matches(&marker, expected_token.as_bytes()) {
                tracing::debug!(path = %marker.display(), "Windows guest bootstrap token matched");
                return Ok(());
            }

            let handler_exited = self
                .handler
                .read()
                .await
                .as_ref()
                .map(|handler| handler.has_exited())
                .unwrap_or(true);
            if handler_exited {
                return Err(BoxError::BoxBootError {
                    message: "WHPX stopped before guest-init published bootstrap readiness"
                        .to_string(),
                    hint: Some(
                        "The guest failed before completing filesystem and network setup"
                            .to_string(),
                    ),
                });
            }

            let elapsed_ms = start.elapsed().as_millis() as u64;
            if elapsed_ms >= max_wait_ms {
                return Err(BoxError::BoxBootError {
                    message: format!(
                        "WHPX guest did not publish bootstrap readiness within {max_wait_ms} ms"
                    ),
                    hint: Some(
                        "The VMM remained alive but the guest was wedged before completing guest-init bootstrap"
                            .to_string(),
                    ),
                });
            }
            if elapsed_ms >= next_progress_log_ms {
                tracing::warn!(
                    elapsed_ms,
                    timeout_ms = max_wait_ms,
                    marker = %marker.display(),
                    "Still waiting for Windows guest bootstrap readiness"
                );
                next_progress_log_ms =
                    next_progress_log_ms.saturating_add(EXEC_READY_PROGRESS_LOG_MS);
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Wait for the exec server to become ready (a Frame Heartbeat round-trip).
    ///
    /// Waits for the readiness EVENT — a successful heartbeat — bounded by VM
    /// liveness, instead of guessing a fixed timeout. guest-init binds the exec
    /// socket early (before the slow network bring-up and container spawn), so the
    /// host connect succeeds immediately and the heartbeat passes the moment the
    /// guest's accept loop runs — however late in a slow cold boot. Each attempt
    /// is individually time-bounded (the early-bound socket makes a host `connect`
    /// succeed and then block on read until the guest accepts), the loop returns
    /// at once if the VM has exited (a fast-exiting container never stalls), and a
    /// large absolute cap is only a last-resort backstop against a wedged-but-alive
    /// guest — not the expected wait. Best-effort: exec/attach also connect on
    /// demand, so even a timed-out probe does not mean exec is unavailable.
    #[cfg(unix)]
    pub(crate) async fn wait_for_exec_ready(
        &mut self,
        exec_socket_path: &std::path::Path,
    ) -> Result<()> {
        use tokio::time::Duration;

        // Per-attempt cap on one connect + heartbeat round-trip. guest-init binds
        // the exec socket early, so the host `connect` succeeds as soon as the VM
        // boots and `heartbeat()`'s read then blocks until the guest's accept loop
        // runs; bounding each attempt keeps the loop checking VM liveness instead
        // of hanging in that read.
        const ATTEMPT_TIMEOUT: Duration = Duration::from_millis(500);
        const POLL_INTERVAL: Duration = Duration::from_millis(200);
        // Last-resort backstop against a wedged-but-alive guest that binds but
        // never accepts. A healthy guest passes the heartbeat the instant its
        // accept loop runs, and an exited VM returns immediately below. Keep the
        // default short enough that foreground `run` starts streaming the guest's
        // logs promptly; callers that truly need a longer cold-boot grace can set
        // A3S_EXEC_READY_TIMEOUT_MS.
        let max_wait_ms = exec_ready_timeout_ms();

        tracing::debug!(
            socket_path = %exec_socket_path.display(),
            timeout_ms = max_wait_ms,
            "Waiting for exec server readiness"
        );

        let start = std::time::Instant::now();
        let mut next_progress_log_ms = EXEC_READY_PROGRESS_LOG_MS;

        loop {
            // Return at once if the VM has already exited (zombie-aware: has_exited
            // treats a zombie shim as exited, unlike is_running's kill(pid,0)). A
            // fast-exiting container never stalls here.
            if self.try_wait_exit().await?.is_some() {
                tracing::debug!("VM exited before exec server became ready");
                return Ok(());
            }
            if let Some(ref handler) = *self.handler.read().await {
                if handler.has_exited() {
                    tracing::debug!("VM exited before exec server became ready");
                    return Ok(());
                }
            }

            // One bounded connect + heartbeat attempt. A timeout (early-bound
            // socket, guest not yet accepting) or any error just means "retry".
            if let Ok(Ok(client)) =
                tokio::time::timeout(ATTEMPT_TIMEOUT, ExecClient::connect(exec_socket_path)).await
            {
                if let Ok(Ok(true)) =
                    tokio::time::timeout(ATTEMPT_TIMEOUT, client.heartbeat()).await
                {
                    tracing::debug!("Exec server heartbeat passed");
                    self.exec_client = Some(client);
                    return Ok(());
                }
            }

            let elapsed_ms = start.elapsed().as_millis() as u64;
            if elapsed_ms >= max_wait_ms {
                tracing::warn!(
                    timeout_ms = max_wait_ms,
                    elapsed_ms,
                    socket_path = %exec_socket_path.display(),
                    "Exec server did not become ready within the safety cap; proceeding so foreground logs and process exit are visible. Exec/attach will connect on demand once the guest finishes starting."
                );
                return Ok(());
            }
            if elapsed_ms >= next_progress_log_ms {
                tracing::warn!(
                    elapsed_ms,
                    timeout_ms = max_wait_ms,
                    socket_path = %exec_socket_path.display(),
                    "Still waiting for exec server readiness; guest init may be mounting volumes, starting the container, or blocked before its accept loop"
                );
                next_progress_log_ms =
                    next_progress_log_ms.saturating_add(EXEC_READY_PROGRESS_LOG_MS);
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Single best-effort exec-server probe for snapshot-restore boots.
    ///
    /// A restored guest is already past boot, so its exec server never re-signals
    /// readiness the way a cold boot does — blocking on [`wait_for_exec_ready`]'s
    /// cold-boot loop would stall registration for up to its safety cap. Instead try
    /// exactly one connect + heartbeat to populate `exec_client` if the guest answers
    /// promptly, and otherwise proceed immediately: exec/attach connect on demand.
    #[cfg(unix)]
    pub(crate) async fn probe_exec_ready_once(&mut self, exec_socket_path: &std::path::Path) {
        use tokio::time::Duration;
        const ATTEMPT_TIMEOUT: Duration = Duration::from_millis(500);

        if let Ok(Ok(client)) =
            tokio::time::timeout(ATTEMPT_TIMEOUT, ExecClient::connect(exec_socket_path)).await
        {
            if let Ok(Ok(true)) = tokio::time::timeout(ATTEMPT_TIMEOUT, client.heartbeat()).await {
                tracing::debug!("restore: exec server heartbeat passed");
                self.exec_client = Some(client);
                return;
            }
        }
        tracing::debug!(
            "restore: exec server did not answer an immediate heartbeat; exec/attach will connect on demand"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "windows")]
    struct RunningHandler;

    #[cfg(target_os = "windows")]
    impl a3s_box_core::VmHandler for RunningHandler {
        fn stop(&mut self, _signal: i32, _timeout_ms: u64) -> a3s_box_core::Result<()> {
            Ok(())
        }

        fn metrics(&self) -> a3s_box_core::VmMetrics {
            a3s_box_core::VmMetrics::default()
        }

        fn is_running(&self) -> bool {
            true
        }

        fn has_exited(&self) -> bool {
            false
        }

        fn pid(&self) -> u32 {
            42
        }
    }

    #[test]
    fn test_parse_exec_ready_timeout_ms() {
        assert_eq!(
            parse_exec_ready_timeout_ms(None),
            DEFAULT_EXEC_READY_TIMEOUT_MS
        );
        assert_eq!(
            parse_exec_ready_timeout_ms(Some("0")),
            DEFAULT_EXEC_READY_TIMEOUT_MS
        );
        assert_eq!(
            parse_exec_ready_timeout_ms(Some("not-a-number")),
            DEFAULT_EXEC_READY_TIMEOUT_MS
        );
        assert_eq!(parse_exec_ready_timeout_ms(Some("2500")), 2500);
        #[cfg(target_os = "windows")]
        assert_eq!(
            parse_ready_timeout_ms(None, DEFAULT_WINDOWS_GUEST_READY_TIMEOUT_MS),
            DEFAULT_WINDOWS_GUEST_READY_TIMEOUT_MS
        );
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn windows_guest_readiness_requires_the_current_boot_token() {
        let rootfs = tempfile::tempdir().unwrap();
        let vm = VmManager::with_box_id(
            a3s_box_core::BoxConfig::default(),
            a3s_box_core::EventEmitter::new(4),
            "windows-ready-test".to_string(),
        );
        let marker = rootfs
            .path()
            .join(a3s_box_core::guest_boot::GUEST_READY_PATH.trim_start_matches('/'));
        std::fs::write(&marker, b"stale-token").unwrap();

        let error = vm
            .wait_for_windows_guest_ready_with_timeout(rootfs.path(), "current-token", 25)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("stopped before guest-init"), "{error}");

        *vm.handler.write().await = Some(Box::new(RunningHandler));
        let error = vm
            .wait_for_windows_guest_ready_with_timeout(rootfs.path(), "current-token", 25)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("did not publish bootstrap readiness"),
            "{error}"
        );

        std::fs::write(&marker, b"current-token").unwrap();
        vm.wait_for_windows_guest_ready_with_timeout(rootfs.path(), "current-token", 25)
            .await
            .unwrap();
    }
}
