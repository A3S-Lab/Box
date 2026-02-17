//! VM readiness checks — waiting for agent and exec sockets.

use a3s_box_core::error::{BoxError, Result};

use crate::grpc::{AgentClient, ExecClient};

use super::VmManager;

impl VmManager {
    /// Wait for the VM process to be running (for generic OCI images without an agent).
    ///
    /// Gives the VM a brief moment to start, then verifies the process hasn't exited.
    pub(crate) async fn wait_for_vm_running(&self) -> Result<()> {
        const STABILIZE_MS: u64 = 1000;

        tracing::debug!("Waiting for VM process to stabilize");
        tokio::time::sleep(tokio::time::Duration::from_millis(STABILIZE_MS)).await;

        if let Some(ref handler) = *self.handler.read().await {
            if !handler.is_running() {
                return Err(BoxError::BoxBootError {
                    message: "VM process exited immediately after start".to_string(),
                    hint: Some("Check console output for errors".to_string()),
                });
            }
        }

        tracing::debug!("VM process is running");
        Ok(())
    }

    /// Wait for the guest agent to become ready.
    ///
    /// Phase 1: Wait for the Unix socket file to appear on disk.
    /// Phase 2: Connect via gRPC and perform a health check with retries.
    /// Wait for the agent socket to appear and be connectable.
    ///
    /// This only verifies the agent process has started and is listening.
    /// The actual health check is done via Heartbeat on the exec server.
    pub(crate) async fn wait_for_agent_socket(&mut self, socket_path: &std::path::Path) -> Result<()> {
        const MAX_WAIT_MS: u64 = 30000;
        const POLL_INTERVAL_MS: u64 = 100;

        tracing::debug!(
            socket_path = %socket_path.display(),
            "Waiting for agent socket to appear"
        );

        let start = std::time::Instant::now();

        // Wait for socket file to appear
        loop {
            if start.elapsed().as_millis() >= MAX_WAIT_MS as u128 {
                return Err(BoxError::TimeoutError(
                    "Timed out waiting for agent socket to appear".to_string(),
                ));
            }

            if socket_path.exists() {
                tracing::debug!("Agent socket file detected");
                break;
            }

            // Check if VM is still running
            if let Some(ref handler) = *self.handler.read().await {
                if !handler.is_running() {
                    return Err(BoxError::BoxBootError {
                        message: "VM process exited unexpectedly".to_string(),
                        hint: Some("Check console output for errors".to_string()),
                    });
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
        }

        // Try to connect (stores client for later use)
        let mut last_err = None;
        while start.elapsed().as_millis() < MAX_WAIT_MS as u128 {
            match AgentClient::connect(socket_path).await {
                Ok(client) => {
                    tracing::debug!("Agent socket connectable");
                    self.agent_client = Some(client);
                    return Ok(());
                }
                Err(e) => {
                    tracing::debug!(error = %e, "Agent connect failed, retrying");
                    last_err = Some(e);
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
        }

        Err(BoxError::TimeoutError(format!(
            "Timed out connecting to agent socket (last error: {})",
            last_err
                .map(|e| e.to_string())
                .unwrap_or_else(|| "none".to_string()),
        )))
    }

    /// Wait for the exec server socket to become ready.
    ///
    /// Polls for the socket file to appear, then verifies the exec server
    /// is healthy via a Frame Heartbeat round-trip. This is best-effort:
    /// if the exec socket never appears (e.g., older guest init without
    /// exec server), the VM still boots successfully.
    pub(crate) async fn wait_for_exec_ready(&mut self, exec_socket_path: &std::path::Path) -> Result<()> {
        const MAX_WAIT_MS: u64 = 10000;
        const POLL_INTERVAL_MS: u64 = 200;

        tracing::debug!(
            socket_path = %exec_socket_path.display(),
            "Waiting for exec server socket"
        );

        let start = std::time::Instant::now();

        // Phase 1: Wait for socket file to appear
        loop {
            if start.elapsed().as_millis() >= MAX_WAIT_MS as u128 {
                tracing::warn!("Exec socket did not appear, exec will not be available");
                return Ok(());
            }

            if exec_socket_path.exists() {
                tracing::debug!("Exec socket file detected");
                break;
            }

            // Check if VM is still running
            if let Some(ref handler) = *self.handler.read().await {
                if !handler.is_running() {
                    tracing::warn!("VM exited before exec socket appeared");
                    return Ok(());
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
        }

        // Phase 2: Connect and verify with Heartbeat health check
        while start.elapsed().as_millis() < MAX_WAIT_MS as u128 {
            match ExecClient::connect(exec_socket_path).await {
                Ok(client) => match client.heartbeat().await {
                    Ok(true) => {
                        tracing::debug!("Exec server heartbeat passed");
                        self.exec_client = Some(client);
                        return Ok(());
                    }
                    Ok(false) => {
                        tracing::debug!("Exec server heartbeat failed, retrying");
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "Exec heartbeat error, retrying");
                    }
                },
                Err(e) => {
                    tracing::debug!(error = %e, "Exec connect failed, retrying");
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
        }

        tracing::warn!("Exec socket appeared but heartbeat failed, exec will not be available");
        Ok(())
    }
}
