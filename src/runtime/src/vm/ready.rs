//! VM readiness checks — waiting for exec socket.

use a3s_box_core::error::{BoxError, Result};

use crate::grpc::ExecClient;

use super::VmManager;

const DEFAULT_EXEC_READY_TIMEOUT_MS: u64 = 15_000;
const EXEC_READY_PROGRESS_LOG_MS: u64 = 5_000;

fn parse_exec_ready_timeout_ms(value: Option<&str>) -> u64 {
    value
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|timeout| *timeout > 0)
        .unwrap_or(DEFAULT_EXEC_READY_TIMEOUT_MS)
}

fn exec_ready_timeout_ms() -> u64 {
    parse_exec_ready_timeout_ms(std::env::var("A3S_EXEC_READY_TIMEOUT_MS").ok().as_deref())
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
    /// guest — not the expected wait. Unix keeps its historical best-effort
    /// behavior so foreground logs and process exit remain visible. Windows fails
    /// startup at the cap because a live WHPX shim without a responsive guest was
    /// previously exposed as a false `running` state.
    pub(crate) async fn wait_for_exec_ready(
        &mut self,
        exec_socket_path: &std::path::Path,
    ) -> Result<()> {
        use tokio::time::Duration;

        // Per-attempt cap on one heartbeat round-trip. Do not call
        // `ExecClient::connect` first: it opens and immediately drops a separate
        // stream, which is harmless for a Unix socket but creates an abandoned
        // WHPX tunnel just as the guest control channel comes online. Windows gets
        // a wider cap for the named-pipe -> control-channel -> guest-socket OPEN
        // handshake; Unix keeps the historical quick liveness polling cadence.
        #[cfg(unix)]
        const ATTEMPT_TIMEOUT: Duration = Duration::from_millis(500);
        #[cfg(windows)]
        const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(3);
        const POLL_INTERVAL: Duration = Duration::from_millis(200);
        // Last-resort backstop against a wedged-but-alive guest that binds but
        // never accepts. A healthy guest passes the heartbeat the instant its
        // accept loop runs, and an exited VM returns immediately below. Keep the
        // default short enough that foreground `run` starts streaming the guest's
        // logs promptly; callers that truly need a longer cold-boot grace can set
        // A3S_EXEC_READY_TIMEOUT_MS.
        let max_wait_ms = exec_ready_timeout_ms();

        // `exec_socket_path` is the layout/record path shared with Unix. WHPX
        // publishes the host side as a named pipe instead, using the box ID.
        // CLI commands perform this mapping when they load a record; readiness
        // runs before that record becomes `running`, so it must map explicitly.
        #[cfg(windows)]
        let exec_endpoint =
            std::path::PathBuf::from(a3s_box_core::exec::windows_exec_pipe_path(&self.box_id));
        #[cfg(not(windows))]
        let exec_endpoint = exec_socket_path.to_path_buf();
        #[cfg(windows)]
        let guest_control_ready_path =
            exec_socket_path.with_file_name(a3s_box_core::exec::WINDOWS_GUEST_CONTROL_READY_FILE);

        tracing::debug!(
            socket_path = %exec_endpoint.display(),
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

            // One bounded connect + heartbeat attempt. On Windows, first wait for
            // the worker's guest-control marker. Opening the host exec pipe before
            // that connection exists would enqueue abandoned sessions in the
            // synchronous pipe worker and can delay the very guest boot being
            // measured. A timeout or any protocol error just means "retry".
            #[cfg(windows)]
            let guest_control_ready = guest_control_ready_path.is_file();
            #[cfg(not(windows))]
            let guest_control_ready = true;
            if guest_control_ready {
                let client = ExecClient::for_socket(&exec_endpoint);
                if let Ok(Ok(true)) =
                    tokio::time::timeout(ATTEMPT_TIMEOUT, client.heartbeat()).await
                {
                    tracing::debug!("Exec server heartbeat passed");
                    #[cfg(unix)]
                    {
                        self.exec_client = Some(client);
                    }
                    return Ok(());
                }
            }

            let elapsed_ms = start.elapsed().as_millis() as u64;
            if elapsed_ms >= max_wait_ms {
                #[cfg(windows)]
                return Err(BoxError::BoxBootError {
                    message: format!(
                        "WHPX guest exec server did not become ready within {max_wait_ms} ms"
                    ),
                    hint: Some(
                        "The VM shim remained alive but the Windows guest-control/exec channel did not answer; inspect the per-box console and shim logs"
                            .to_string(),
                    ),
                });
                #[cfg(not(windows))]
                {
                    tracing::warn!(
                        timeout_ms = max_wait_ms,
                        elapsed_ms,
                        socket_path = %exec_endpoint.display(),
                        "Exec server did not become ready within the safety cap; proceeding so foreground logs and process exit are visible. Exec/attach will connect on demand once the guest finishes starting."
                    );
                    return Ok(());
                }
            }
            if elapsed_ms >= next_progress_log_ms {
                tracing::warn!(
                    elapsed_ms,
                    timeout_ms = max_wait_ms,
                    socket_path = %exec_endpoint.display(),
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

        let client = ExecClient::for_socket(exec_socket_path);
        if let Ok(Ok(true)) = tokio::time::timeout(ATTEMPT_TIMEOUT, client.heartbeat()).await {
            tracing::debug!("restore: exec server heartbeat passed");
            self.exec_client = Some(client);
            return;
        }
        tracing::debug!(
            "restore: exec server did not answer an immediate heartbeat; exec/attach will connect on demand"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
