//! Guest execution channel, provider completion, and attach operations.

use super::*;

impl VmManager {
    /// Get the exec client, if connected.
    #[cfg(unix)]
    pub fn exec_client(&self) -> Option<&ExecClient> {
        self.exec_client.as_ref()
    }

    #[cfg(unix)]
    async fn connect_exec_client_for_request(socket_path: &Path) -> Result<ExecClient> {
        const ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

        let client = ExecClient::connect(socket_path).await?;
        match tokio::time::timeout(ATTEMPT_TIMEOUT, client.heartbeat()).await {
            Ok(Ok(true)) => Ok(client),
            Ok(Ok(false)) => Err(BoxError::ExecError(format!(
                "Exec client not connected: heartbeat failed at {}",
                socket_path.display()
            ))),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(BoxError::ExecError(format!(
                "Exec client not connected: heartbeat timed out at {}",
                socket_path.display()
            ))),
        }
    }

    /// Wait until the guest exec server can complete a heartbeat.
    ///
    /// Cold foreground boots may proceed after the short diagnostic readiness
    /// cap so logs remain visible. A warm pool has a stronger contract: an idle
    /// VM must actually be executable before it is published to callers.
    #[cfg(unix)]
    pub async fn wait_for_exec_available(&mut self, timeout: std::time::Duration) -> Result<()> {
        let socket_path = self
            .exec_socket_path
            .clone()
            .ok_or_else(|| BoxError::ExecError("Exec socket path is unavailable".to_string()))?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match Self::connect_exec_client_for_request(&socket_path).await {
                Ok(client) => {
                    self.exec_client = Some(client);
                    return Ok(());
                }
                Err(error) if tokio::time::Instant::now() < deadline => {
                    tracing::debug!(%error, "Waiting for pooled VM exec readiness");
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    #[cfg(not(unix))]
    pub async fn wait_for_exec_available(&mut self, _timeout: std::time::Duration) -> Result<()> {
        Ok(())
    }

    /// Attach this manager to an already-running shim process.
    ///
    /// This is useful for crash recovery or control-plane restart flows where
    /// the workload VM is still alive and only the host-side manager state
    /// needs to be reconstructed.
    #[cfg(unix)]
    pub async fn attach_running_process(
        &mut self,
        pid: u32,
        exec_socket_path: PathBuf,
        pty_socket_path: Option<PathBuf>,
    ) -> Result<()> {
        let port_forward_socket_path = exec_socket_path.with_file_name("portfwd.sock");
        let handler = crate::vmm::ShimHandler::from_pid(pid, self.box_id.clone());
        if !handler.is_running() {
            return Err(BoxError::StateError(format!(
                "Cannot attach to non-running VM process {pid}"
            )));
        }

        self.exec_client = match ExecClient::connect(&exec_socket_path).await {
            Ok(client) => Some(client),
            Err(error) => {
                tracing::debug!(
                    box_id = %self.box_id,
                    socket_path = %exec_socket_path.display(),
                    error = %error,
                    "Failed to reconnect exec client while attaching to running VM"
                );
                None
            }
        };
        self.exec_socket_path = Some(exec_socket_path);
        self.pty_socket_path = pty_socket_path;
        self.port_forward_socket_path = Some(port_forward_socket_path);
        *self.handler.write().await = Some(Box::new(handler));
        *self.state.write().await = BoxState::Ready;
        Ok(())
    }

    /// Attach this manager to an already-running Windows shim process.
    #[cfg(windows)]
    pub async fn attach_running_process(
        &mut self,
        pid: u32,
        exec_socket_path: PathBuf,
        pty_socket_path: Option<PathBuf>,
    ) -> Result<()> {
        let handler = crate::vmm::ShimHandler::from_pid(pid, self.box_id.clone());
        if !handler.is_running() {
            return Err(BoxError::StateError(format!(
                "Cannot attach to non-running VM process {pid}"
            )));
        }

        self.exec_socket_path = Some(exec_socket_path);
        self.pty_socket_path = pty_socket_path;
        self.port_forward_socket_path = None;
        *self.handler.write().await = Some(Box::new(handler));
        *self.state.write().await = BoxState::Ready;
        Ok(())
    }

    /// Get the exec socket path, if the VM has been booted.
    pub fn exec_socket_path(&self) -> Option<&Path> {
        self.exec_socket_path.as_deref()
    }

    /// Get the PTY socket path, if the VM has been booted.
    pub fn pty_socket_path(&self) -> Option<&Path> {
        self.pty_socket_path.as_deref()
    }

    /// Get the CRI port-forward socket path, if the VM has been booted.
    pub fn port_forward_socket_path(&self) -> Option<&Path> {
        self.port_forward_socket_path.as_deref()
    }

    /// Inject a custom VMM provider (e.g., a VmController with a known shim path).
    ///
    /// If set before `boot()`, the injected provider is used instead of the
    /// default `VmController::find_shim()` fallback.
    pub fn set_provider(&mut self, provider: Box<dyn VmmProvider>) {
        self.provider = Some(provider);
    }

    /// Override the rootfs preparation and transport provider.
    ///
    /// By default, `default_provider()` auto-detects the best available provider.
    /// Call this before `boot()` to force a specific provider.
    pub fn set_rootfs_provider(&mut self, provider: Box<dyn crate::rootfs::RootfsProvider>) {
        self.rootfs_provider = provider;
    }

    /// Get the name of the active rootfs provider.
    pub fn rootfs_provider_name(&self) -> &str {
        self.rootfs_provider.name()
    }

    /// Set a progress callback for image pulls: `(current, total, digest, size_bytes)`.
    /// Called once per layer when `run` pulls an image that is not yet cached.
    pub fn set_pull_progress_fn(&mut self, f: PullProgressFn) {
        self.pull_progress_fn = Some(f);
    }

    /// Attach Prometheus metrics to this VM manager.
    pub fn set_metrics(&mut self, metrics: crate::prom::RuntimeMetrics) {
        self.prom = Some(metrics);
    }

    /// Start a drop-based timer for one stable VM boot phase.
    ///
    /// Cloning the optional metrics handle keeps the timer independent from the
    /// manager borrow, so callers can hold it across asynchronous preparation
    /// and launch operations. A missing metrics sink is deliberately cheap and
    /// preserves the runtime's opt-in instrumentation behavior.
    pub(crate) fn boot_phase_timer(&self, phase: &'static str) -> crate::prom::BootPhaseTimer {
        crate::prom::BootPhaseTimer::new(self.prom.clone(), phase)
    }

    /// Set the logging driver config. Threaded into the InstanceSpec so the shim
    /// runs the log processor for the box's lifetime.
    pub fn set_log_config(&mut self, log_config: a3s_box_core::log::LogConfig) {
        self.log_config = log_config;
    }

    /// Set whether an image-defined health check is explicitly disabled.
    pub fn set_healthcheck_disabled(&mut self, disabled: bool) {
        self.healthcheck_disabled = disabled;
    }

    /// Get the attached Prometheus metrics (if any).
    pub fn metrics_prom(&self) -> Option<&crate::prom::RuntimeMetrics> {
        self.prom.as_ref()
    }

    /// Get the names of anonymous volumes created during boot.
    ///
    /// These are auto-created from OCI VOLUME directives and should be tracked
    /// for cleanup when the box is removed.
    pub fn anonymous_volumes(&self) -> &[String] {
        &self.anonymous_volumes
    }

    /// Get the OCI image config resolved during boot.
    pub fn image_config(&self) -> Option<&crate::oci::OciImageConfig> {
        self.image_config.as_ref()
    }

    /// Return the immutable execution resolution captured for this boot.
    pub fn resolved_execution_plan(&self) -> Option<&ResolvedExecutionPlan> {
        self.resolved_execution_plan.as_ref()
    }

    /// Get the exit code of the container, if it has exited.
    ///
    /// Returns `Some(code)` after `destroy()` has been called and the shim
    /// process exited naturally (not killed). Returns `None` if the VM has not
    /// yet stopped or the exit code could not be determined.
    pub fn exit_code(&self) -> Option<i32> {
        self.shim_exit_code
    }

    #[cfg(not(target_os = "windows"))]
    fn persisted_exit_code(&self) -> Option<i32> {
        crate::rootfs::read_persisted_exit_code(&self.home_dir.join("boxes").join(&self.box_id))
    }

    /// Poll the owned VM process for natural exit without sending a signal.
    ///
    /// This is used by foreground CLI flows where the container command may
    /// finish on its own and the CLI should clean up instead of waiting for
    /// a Ctrl-C.
    pub async fn try_wait_exit(&mut self) -> Result<Option<i32>> {
        if let Some(code) = self.shim_exit_code {
            return Ok(Some(code));
        }

        #[cfg(not(target_os = "windows"))]
        let box_dir = self.home_dir.join("boxes").join(&self.box_id);

        let mut handler = self.handler.write().await;
        let Some(handler) = handler.as_mut() else {
            // A recovered terminal manager can have no live provider handle.
            // In that state the durable guest result is the remaining source
            // of truth and no runtime writer can still append console bytes.
            #[cfg(not(target_os = "windows"))]
            if let Some(code) = crate::rootfs::read_persisted_exit_code(&box_dir) {
                self.shim_exit_code = Some(code);
            }
            return Ok(self.shim_exit_code);
        };

        if let Some(code) = handler.try_wait_exit()? {
            #[cfg(target_os = "windows")]
            let code = collect_windows_guest_result(
                &self.home_dir.join("boxes").join(&self.box_id),
                &self.log_config,
                code,
            )?;
            #[cfg(not(target_os = "windows"))]
            let Some(code) = crate::rootfs::resolve_workload_exit_code(&box_dir, Some(code)) else {
                return Ok(None);
            };
            self.shim_exit_code = Some(code);
            return Ok(Some(code));
        }

        #[cfg(not(target_os = "windows"))]
        if handler.has_exited() {
            // Attached handlers cannot reap another process owner's child, but
            // zombie-aware provider completion still proves that the shim has
            // closed the raw streams and joined its log processor. Prefer the
            // durable workload status over a provider-specific status.
            if let Some(code) =
                crate::rootfs::resolve_workload_exit_code(&box_dir, handler.exit_code())
            {
                self.shim_exit_code = Some(code);
                return Ok(Some(code));
            }
        }

        Ok(None)
    }

    /// Return true once the runtime provider has finished its terminal work.
    ///
    /// The guest can persist its workload status before the shim has relayed the
    /// final console bytes. That durable status alone must not publish provider
    /// completion or foreground cleanup can terminate the shim mid-drain.
    pub async fn has_exited(&self) -> bool {
        if self.shim_exit_code.is_some() {
            return true;
        }

        let handler = self.handler.read().await;
        if let Some(handler) = handler.as_ref() {
            return handler.has_exited();
        }
        drop(handler);

        #[cfg(not(target_os = "windows"))]
        {
            self.persisted_exit_code().is_some()
        }

        #[cfg(target_os = "windows")]
        {
            false
        }
    }

    /// Run a command as the container MAIN in an IDLE-booted (deferred-main) VM.
    ///
    /// Sends the `spawn-main` control frame carrying `spec_json` (the command),
    /// waits for the main to exit (which halts the VM), and returns its real exit
    /// code + the box's json-file console logs split by stream. This is the full-
    /// box-semantics counterpart to [`Self::exec_command`] (whose output is piped
    /// over the exec stream, not the json-file logs).
    #[cfg(unix)]
    pub async fn run_deferred_main(
        &mut self,
        spec_json: &[u8],
        timeout: std::time::Duration,
    ) -> Result<a3s_box_core::exec::ExecOutput> {
        let log_dir = self.home_dir.join("boxes").join(&self.box_id).join("logs");
        let console_out_path = log_dir.join("console.log");
        let console_err_path = a3s_box_core::log::stderr_console_path(&console_out_path);
        let console_out_start = std::fs::metadata(&console_out_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let console_err_start = std::fs::metadata(&console_err_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);

        let acked = {
            let owned_client;
            let client = if let Some(client) = self.exec_client.as_ref() {
                client
            } else {
                let socket_path = self
                    .exec_socket_path
                    .as_deref()
                    .ok_or_else(|| BoxError::ExecError("Exec client not connected".to_string()))?;
                owned_client = Self::connect_exec_client_for_request(socket_path).await?;
                &owned_client
            };
            client.spawn_main(Some(spec_json)).await?
        };
        let exit_wait_timeout = if acked {
            timeout
        } else {
            // Very short deferred mains can exit and halt the VM before the
            // guest's ACK frame makes it back to the host. Treat a missing ACK as
            // provisional: if the VM exits promptly, the spawn succeeded and the
            // real exit code/logs are authoritative; otherwise fail quickly
            // instead of waiting the full command timeout for an IDLE VM.
            tracing::debug!(
                box_id = %self.box_id,
                "spawn-main was not acknowledged; waiting briefly for main exit"
            );
            timeout.min(std::time::Duration::from_secs(2))
        };

        // Wait for the main to exit — guest-init persists the code and halts the VM.
        let start = std::time::Instant::now();
        let exit_code = loop {
            if let Some(code) = self.try_wait_exit().await? {
                break code;
            }
            if start.elapsed() >= exit_wait_timeout {
                let message = if acked {
                    "deferred main did not exit within the timeout"
                } else {
                    "spawn-main was not acknowledged by the guest"
                };
                return Err(BoxError::ExecError(message.to_string()));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };

        // Let the shim's log processor finish draining console.log into the json
        // file (it flushes as the VM halts). A single short "stable length"
        // sample is not enough here: deferred-main can persist its exit code
        // before the final stdout/stderr bytes have reached the host tailer,
        // especially with pre-warmed pools. Require a small quiet window before
        // reading logs, bounded so no-output commands still return promptly.
        let json_path = log_dir.join("container.json");
        let drain_start = std::time::Instant::now();
        let max_wait = std::time::Duration::from_secs(2);
        let min_wait = std::time::Duration::from_millis(500);
        let quiet_window = std::time::Duration::from_millis(200);
        let mut last_len: Option<u64> = None;
        let mut last_change = drain_start;
        loop {
            let len = std::fs::metadata(&json_path).map(|m| m.len()).unwrap_or(0);
            if last_len != Some(len) {
                last_len = Some(len);
                last_change = std::time::Instant::now();
            }
            let elapsed = drain_start.elapsed();
            if elapsed >= max_wait || (elapsed >= min_wait && last_change.elapsed() >= quiet_window)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let (mut stdout, mut stderr) = self.read_container_logs();
        if stdout.is_empty() {
            stdout = Self::read_file_from_offset(&console_out_path, console_out_start);
        }
        if stderr.is_empty() {
            stderr = Self::read_file_from_offset(&console_err_path, console_err_start);
        }
        let truncated = stdout.len() > a3s_box_core::exec::MAX_OUTPUT_BYTES
            || stderr.len() > a3s_box_core::exec::MAX_OUTPUT_BYTES;
        stdout.truncate(a3s_box_core::exec::MAX_OUTPUT_BYTES);
        stderr.truncate(a3s_box_core::exec::MAX_OUTPUT_BYTES);
        Ok(a3s_box_core::exec::ExecOutput {
            stdout,
            stderr,
            exit_code,
            truncated,
        })
    }

    #[cfg(unix)]
    fn read_file_from_offset(path: &Path, offset: u64) -> Vec<u8> {
        use std::io::{Read, Seek, SeekFrom};

        let mut file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(_) => return vec![],
        };
        if file.seek(SeekFrom::Start(offset)).is_err() {
            return vec![];
        }

        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_err() {
            return vec![];
        }
        bytes
    }

    /// Read the box's json-file console logs, split into stdout/stderr by stream.
    #[cfg(unix)]
    fn read_container_logs(&self) -> (Vec<u8>, Vec<u8>) {
        let path = self
            .home_dir
            .join("boxes")
            .join(&self.box_id)
            .join("logs")
            .join("container.json");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        if let Ok(content) = std::fs::read_to_string(&path) {
            for line in content.lines() {
                if let Ok(entry) = serde_json::from_str::<a3s_box_core::log::LogEntry>(line) {
                    if entry.stream == "stderr" {
                        err.extend_from_slice(entry.log.as_bytes());
                    } else {
                        out.extend_from_slice(entry.log.as_bytes());
                    }
                }
            }
        }
        (out, err)
    }

    /// Execute a command in the guest VM.
    ///
    /// Requires the VM to be in Ready, Busy, or Compacting state.
    #[cfg(unix)]
    #[tracing::instrument(skip(self, request), fields(box_id = %self.box_id))]
    pub async fn exec_request(
        &self,
        request: &a3s_box_core::exec::ExecRequest,
    ) -> Result<a3s_box_core::exec::ExecOutput> {
        if request.cmd.is_empty() {
            return Err(BoxError::ExecError(
                "Exec request requires a non-empty command".to_string(),
            ));
        }

        let state = self.state.read().await;
        match *state {
            BoxState::Ready | BoxState::Busy | BoxState::Compacting => {}
            BoxState::Created => {
                return Err(BoxError::ExecError("VM not yet booted".to_string()));
            }
            BoxState::Stopped => {
                return Err(BoxError::ExecError("VM is stopped".to_string()));
            }
        }
        drop(state);

        let owned_client;
        let client = if let Some(client) = self.exec_client.as_ref() {
            client
        } else {
            let socket_path = self
                .exec_socket_path
                .as_deref()
                .ok_or_else(|| BoxError::ExecError("Exec client not connected".to_string()))?;
            owned_client = Self::connect_exec_client_for_request(socket_path).await?;
            &owned_client
        };

        let exec_start = std::time::Instant::now();
        let result = client.exec_command(request).await;

        // Record Prometheus metrics
        if let Some(ref prom) = self.prom {
            prom.exec_total.inc();
            prom.exec_duration
                .observe(exec_start.elapsed().as_secs_f64());
            if result.is_err() || result.as_ref().is_ok_and(|o| o.exit_code != 0) {
                prom.exec_errors_total.inc();
            }
        }

        result
    }

    /// Execute a command in the guest VM.
    ///
    /// Requires the VM to be in Ready, Busy, or Compacting state.
    #[cfg(unix)]
    #[tracing::instrument(skip(self, cmd), fields(box_id = %self.box_id))]
    pub async fn exec_command(
        &self,
        cmd: Vec<String>,
        timeout_ns: u64,
    ) -> Result<a3s_box_core::exec::ExecOutput> {
        let request = a3s_box_core::exec::ExecRequest {
            request_id: None,
            cmd,
            timeout_ns,
            env: vec![],
            working_dir: None,
            rootfs: None,
            stdin: None,
            stdin_streaming: false,
            user: None,
            streaming: false,
        };

        self.exec_request(&request).await
    }
}
