//! VM teardown, state transitions, pause/resume, health, and resizing.

use super::*;

#[cfg(unix)]
const GUEST_STOP_DELIVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
#[cfg(unix)]
const GUEST_STOP_FINALIZATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(unix)]
const GUEST_STOP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);

#[cfg(unix)]
async fn wait_for_provider_exit(
    handler: &mut dyn VmHandler,
    timeout: std::time::Duration,
) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if handler.try_wait_exit()?.is_some() || handler.has_exited() || !handler.is_running() {
            return Ok(true);
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(GUEST_STOP_POLL_INTERVAL.min(deadline - now)).await;
    }
}

impl VmManager {
    /// Destroy the VM with the default shutdown timeout and SIGTERM.
    pub async fn destroy(&mut self) -> Result<()> {
        self.destroy_with_options(default_stop_signal(), DEFAULT_SHUTDOWN_TIMEOUT_MS)
            .await
    }

    /// Destroy the VM with a custom shutdown timeout and SIGTERM.
    pub async fn destroy_with_timeout(&mut self, timeout_ms: u64) -> Result<()> {
        self.destroy_with_options(default_stop_signal(), timeout_ms)
            .await
    }

    /// Destroy the VM with a specific stop signal and timeout.
    ///
    /// Delivers `signal` to the workload through the private guest control
    /// channel and waits up to `timeout_ms` for it to exit. If it does not,
    /// asks the guest to SIGKILL the workload and gives PID 1 a bounded window
    /// to flush and quiesce the root disk. Signalling the shim is the final
    /// fallback only when guest-owned shutdown cannot complete.
    #[tracing::instrument(skip(self), fields(box_id = %self.box_id))]
    pub async fn destroy_with_options(&mut self, signal: i32, timeout_ms: u64) -> Result<()> {
        let preserve_rootfs = self.config.persistent;
        self.destroy_with_rootfs_policy(signal, timeout_ms, preserve_rootfs)
            .await
    }

    /// Stop the runtime while retaining its writable rootfs for a managed
    /// restart or filesystem-only pause.
    pub(crate) async fn destroy_preserving_rootfs_with_options(
        &mut self,
        signal: i32,
        timeout_ms: u64,
    ) -> Result<()> {
        self.destroy_with_rootfs_policy(signal, timeout_ms, true)
            .await
    }

    pub(crate) async fn destroy_preserving_rootfs(&mut self) -> Result<()> {
        self.destroy_with_rootfs_policy(default_stop_signal(), DEFAULT_SHUTDOWN_TIMEOUT_MS, true)
            .await
    }

    async fn destroy_with_rootfs_policy(
        &mut self,
        signal: i32,
        timeout_ms: u64,
        preserve_rootfs: bool,
    ) -> Result<()> {
        let mut state = self.state.write().await;

        if *state == BoxState::Stopped {
            return Ok(());
        }

        tracing::info!(box_id = %self.box_id, signal, timeout_ms, "Destroying VM");

        // Mark as stopped first — ensures state is correct even if handler.stop() fails.
        *state = BoxState::Stopped;

        let box_dir = self.home_dir.join("boxes").join(&self.box_id);

        // Stop the VM handler and capture its exit code before it's dropped.
        // A stop failure must NOT skip the host-resource teardown below (network
        // backend, overlay unmount, socket + box dirs) — those are already
        // best-effort and would otherwise leak on every wedged stop. Capture the
        // error and surface it after teardown instead of returning early.
        let mut stop_error = None;
        #[cfg(unix)]
        let requires_guest_rootfs_handoff =
            if preserve_rootfs && self.boot_mode != VmBootMode::RootfsMaintenance {
                match crate::rootfs::guest_native_ext4_generation_exists(&box_dir) {
                    Ok(exists) => exists,
                    Err(error) => {
                        stop_error = Some(error);
                        false
                    }
                }
            } else {
                false
            };
        if let Some(mut handler) = self.handler.write().await.take() {
            #[cfg(windows)]
            let stop_request = match windows_stop::stage(&self.socket_dir(), signal) {
                Ok(path) => {
                    tracing::debug!(
                        box_id = %self.box_id,
                        signal,
                        path = %path.display(),
                        "Staged Windows guest stop request"
                    );
                    Some(path)
                }
                Err(error) => {
                    tracing::warn!(
                        box_id = %self.box_id,
                        signal,
                        error = %error,
                        "Failed to stage Windows guest stop request; force-stop fallback remains active"
                    );
                    None
                }
            };

            #[cfg(windows)]
            let handler_timeout_ms = if timeout_ms == 0 {
                0
            } else if let Some(request) = stop_request.as_deref() {
                let delivery_started = std::time::Instant::now();
                let delivery_timeout = std::time::Duration::from_millis(
                    timeout_ms.min(WINDOWS_STOP_DELIVERY_TIMEOUT_MS),
                );
                let delivered =
                    match windows_stop::wait_until_delivered(request, delivery_timeout).await {
                        Ok(delivered) => delivered,
                        Err(error) => {
                            tracing::warn!(
                                box_id = %self.box_id,
                                error = %error,
                                "Failed while waiting for Windows guest stop request delivery"
                            );
                            false
                        }
                    };
                let delivery_elapsed_ms =
                    u64::try_from(delivery_started.elapsed().as_millis()).unwrap_or(u64::MAX);
                let remaining_timeout_ms = timeout_ms.saturating_sub(delivery_elapsed_ms);
                if delivered {
                    let finalization_timeout_ms = if self.config.persistent {
                        WINDOWS_GUEST_FINALIZATION_TIMEOUT_MS
                    } else {
                        0
                    };
                    let handler_timeout_ms =
                        remaining_timeout_ms.saturating_add(finalization_timeout_ms);
                    tracing::debug!(
                        box_id = %self.box_id,
                        delivery_elapsed_ms,
                        handler_timeout_ms,
                        "Delivered Windows stop request to the guest"
                    );
                    handler_timeout_ms
                } else {
                    tracing::warn!(
                        box_id = %self.box_id,
                        delivery_elapsed_ms,
                        "Windows guest stop request was not delivered before the forwarding deadline"
                    );
                    remaining_timeout_ms
                }
            } else {
                timeout_ms
            };
            #[cfg(unix)]
            let guest_stop_delivered = if self.boot_mode == VmBootMode::RootfsMaintenance {
                self.deliver_rootfs_maintenance_shutdown().await
            } else {
                self.deliver_guest_stop_signal(signal).await
            };
            #[cfg(unix)]
            let _provider_exited = if guest_stop_delivered {
                let graceful_wait = if signal == libc::SIGKILL {
                    std::time::Duration::ZERO
                } else {
                    std::time::Duration::from_millis(timeout_ms)
                };
                let exited = match wait_for_provider_exit(handler.as_mut(), graceful_wait).await {
                    Ok(exited) => exited,
                    Err(error) => {
                        tracing::warn!(
                            box_id = %self.box_id,
                            %error,
                            "Failed while waiting for guest-owned shutdown"
                        );
                        if stop_error.is_none() {
                            stop_error = Some(error);
                        }
                        false
                    }
                };
                if exited {
                    true
                } else {
                    let force_delivered = signal == libc::SIGKILL
                        || self.deliver_guest_stop_signal(libc::SIGKILL).await;
                    if force_delivered {
                        match wait_for_provider_exit(
                            handler.as_mut(),
                            GUEST_STOP_FINALIZATION_TIMEOUT,
                        )
                        .await
                        {
                            Ok(exited) => exited,
                            Err(error) => {
                                tracing::warn!(
                                    box_id = %self.box_id,
                                    %error,
                                    "Failed while waiting for forced guest finalization"
                                );
                                if stop_error.is_none() {
                                    stop_error = Some(error);
                                }
                                false
                            }
                        }
                    } else {
                        false
                    }
                }
            } else {
                false
            };

            #[cfg(unix)]
            let handler_signal = if guest_stop_delivered {
                libc::SIGKILL
            } else {
                signal
            };
            #[cfg(windows)]
            let handler_signal = signal;
            #[cfg(unix)]
            let handler_timeout_ms = if guest_stop_delivered { 0 } else { timeout_ms };

            // Observing provider exit proves that the workload stopped, but it
            // does not finalize the backend. In particular, A3S OCI owns its
            // terminal generation and private endpoint until `stop` performs
            // the authoritative delete. Every handler implementation treats an
            // already-exited process idempotently, so always run this finalizer;
            // the zero timeout prevents a second graceful-wait interval.
            let _handler_stopped = match handler.stop(handler_signal, handler_timeout_ms) {
                Ok(()) => true,
                Err(e) => {
                    tracing::error!(box_id = %self.box_id, error = %e, "Failed to stop VM handler; continuing teardown");
                    if stop_error.is_none() {
                        stop_error = Some(e);
                    }
                    false
                }
            };
            #[cfg(not(windows))]
            {
                self.shim_exit_code =
                    crate::rootfs::resolve_workload_exit_code(&box_dir, handler.exit_code());
            }
            #[cfg(windows)]
            {
                self.shim_exit_code = handler.exit_code();
            }

            #[cfg(unix)]
            let clean_guest_rootfs_handoff = _handler_stopped
                && requires_guest_rootfs_handoff
                && crate::rootfs::guest_rootfs_handoff_complete(&box_dir);
            #[cfg(unix)]
            if _handler_stopped && requires_guest_rootfs_handoff && !clean_guest_rootfs_handoff {
                let error = BoxError::StateError(format!(
                    "Guest-owned persistent rootfs for {} stopped without a verified read-only handoff; the raw disk was retained but must not be treated as clean",
                    self.box_id
                ));
                tracing::error!(box_id = %self.box_id, %error);
                if stop_error.is_none() {
                    stop_error = Some(error);
                }
            }

            #[cfg(unix)]
            if clean_guest_rootfs_handoff && stop_error.is_none() {
                if let Err(error) = self.rootfs_provider.record_clean_stop(&box_dir) {
                    tracing::error!(
                        box_id = %self.box_id,
                        %error,
                        "Failed to publish the verified rootfs clean-stop transition"
                    );
                    stop_error = Some(error);
                }
            }

            #[cfg(windows)]
            if stop_request.is_some() {
                if let Err(error) = windows_stop::clear(&self.socket_dir()) {
                    tracing::warn!(
                        box_id = %self.box_id,
                        error = %error,
                        "Failed to clear Windows guest stop request"
                    );
                }
            }

            #[cfg(windows)]
            if _handler_stopped && self.config.persistent {
                let rootfs = self
                    .home_dir
                    .join("boxes")
                    .join(&self.box_id)
                    .join("rootfs");
                match a3s_box_core::rootfs_metadata::finalize_terminal_rootfs_metadata(&rootfs) {
                    Ok(true) => tracing::info!(
                        box_id = %self.box_id,
                        path = %rootfs.display(),
                        "Published terminal rootfs metadata after Windows guest exit"
                    ),
                    Ok(false) => tracing::debug!(
                        box_id = %self.box_id,
                        path = %rootfs.display(),
                        "No Windows terminal rootfs metadata required host finalization"
                    ),
                    Err(error) => tracing::warn!(
                        box_id = %self.box_id,
                        path = %rootfs.display(),
                        error = %error,
                        "Refused to publish invalid Windows terminal rootfs metadata"
                    ),
                }
            }
        }

        // Stop network backend if running
        if let Some(ref mut net) = self.net_manager {
            net.stop();
        }
        self.net_manager = None;

        let mount_aliases_clean = match self.cleanup_sandbox_mount_aliases() {
            Ok(()) => true,
            Err(error) => {
                tracing::error!(
                    box_id = %self.box_id,
                    %error,
                    "Failed to cleanup Sandbox attachment aliases"
                );
                if stop_error.is_none() {
                    stop_error = Some(error);
                }
                false
            }
        };

        let socket_dir = self.socket_dir();
        // A detached CLI invocation recovers the shim but has no in-memory
        // PasstManager child handle. Reap passt from its durable PID file before
        // removing the socket directory that contains that identity; otherwise
        // a later managed remove cannot find the daemon and it keeps published
        // ports bound indefinitely.
        #[cfg(target_os = "linux")]
        crate::network::terminate_passt(&socket_dir);

        // Cleanup rootfs provider (unmount overlay if applicable)
        if let Err(e) = self.rootfs_provider.cleanup(&box_dir, preserve_rootfs) {
            tracing::warn!(
                box_id = %self.box_id,
                error = %e,
                "Failed to cleanup rootfs provider"
            );
        }

        if let Err(e) = std::fs::remove_dir_all(&socket_dir) {
            tracing::debug!(
                box_id = %self.box_id,
                path = %socket_dir.display(),
                error = %e,
                "Failed to cleanup VM socket directory"
            );
        }

        // Remove the box working directory itself (overlay upper/work, logs,
        // leftover metadata) for non-persistent boxes. Without this, ephemeral
        // CRI pods leak their `boxes/<id>` directory on every destroy; the
        // accumulation slows later RunPodSandbox calls until they time out
        // (observed: pod #21 after churning 20). Persistent boxes keep their
        // dir intentionally.
        if !preserve_rootfs && mount_aliases_clean {
            match std::fs::remove_dir_all(&box_dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(
                        box_id = %self.box_id,
                        path = %box_dir.display(),
                        error = %e,
                        "Failed to remove box directory on destroy"
                    );
                }
            }
        }

        // Record Prometheus metrics
        if let Some(ref prom) = self.prom {
            prom.vm_destroyed_total.inc();
            prom.vm_count.with_label_values(&["ready"]).dec();
        }

        // Emit stopped event
        self.event_emitter.emit(BoxEvent::empty("box.stopped"));

        // Host teardown above is complete; surface a handler-stop failure now so
        // the caller still learns the stop was imperfect.
        match stop_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    #[cfg(unix)]
    async fn deliver_guest_stop_signal(&self, signal: i32) -> bool {
        let socket_path = self
            .exec_socket_path
            .as_deref()
            .or_else(|| self.exec_client.as_ref().map(ExecClient::socket_path));
        let Some(socket_path) = socket_path else {
            return false;
        };
        let client = ExecClient::for_socket(socket_path);
        match tokio::time::timeout(GUEST_STOP_DELIVERY_TIMEOUT, client.signal_main(signal)).await {
            Ok(Ok(true)) => {
                tracing::debug!(
                    box_id = %self.box_id,
                    signal,
                    socket_path = %socket_path.display(),
                    "Delivered stop signal to the workload through guest control"
                );
                true
            }
            Ok(Ok(false)) => {
                tracing::warn!(
                    box_id = %self.box_id,
                    signal,
                    socket_path = %socket_path.display(),
                    "Guest did not acknowledge the workload stop signal"
                );
                false
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    box_id = %self.box_id,
                    signal,
                    socket_path = %socket_path.display(),
                    %error,
                    "Failed to deliver the workload stop signal through guest control"
                );
                false
            }
            Err(_) => {
                tracing::warn!(
                    box_id = %self.box_id,
                    signal,
                    socket_path = %socket_path.display(),
                    "Timed out delivering the workload stop signal through guest control"
                );
                false
            }
        }
    }

    #[cfg(unix)]
    async fn deliver_rootfs_maintenance_shutdown(&self) -> bool {
        let socket_path = self
            .exec_socket_path
            .as_deref()
            .or_else(|| self.exec_client.as_ref().map(ExecClient::socket_path));
        let Some(socket_path) = socket_path else {
            return false;
        };
        let client = ExecClient::for_socket(socket_path);
        match tokio::time::timeout(
            GUEST_STOP_DELIVERY_TIMEOUT,
            client.shutdown_rootfs_maintenance(),
        )
        .await
        {
            Ok(Ok(true)) => {
                tracing::debug!(
                    box_id = %self.box_id,
                    socket_path = %socket_path.display(),
                    "Requested clean rootfs maintenance guest shutdown"
                );
                true
            }
            Ok(Ok(false)) | Ok(Err(_)) | Err(_) => {
                tracing::warn!(
                    box_id = %self.box_id,
                    socket_path = %socket_path.display(),
                    "Rootfs maintenance guest did not acknowledge shutdown"
                );
                false
            }
        }
    }

    /// Transition to busy state.
    pub async fn set_busy(&self) -> Result<()> {
        let mut state = self.state.write().await;

        if *state != BoxState::Ready {
            return Err(BoxError::StateError("VM not ready".to_string()));
        }

        *state = BoxState::Busy;
        Ok(())
    }

    /// Transition back to ready state.
    pub async fn set_ready(&self) -> Result<()> {
        let mut state = self.state.write().await;

        if *state != BoxState::Busy && *state != BoxState::Compacting {
            return Err(BoxError::StateError("Invalid state transition".to_string()));
        }

        *state = BoxState::Ready;
        Ok(())
    }

    /// Transition to compacting state.
    pub async fn set_compacting(&self) -> Result<()> {
        let mut state = self.state.write().await;

        if *state != BoxState::Busy {
            return Err(BoxError::StateError("VM not busy".to_string()));
        }

        *state = BoxState::Compacting;
        Ok(())
    }

    /// Pause the VM by sending SIGSTOP to the shim process.
    ///
    /// The VM must be in Ready, Busy, or Compacting state.
    #[cfg(unix)]
    pub async fn pause(&self) -> Result<()> {
        let state = self.state.read().await;
        match *state {
            BoxState::Ready | BoxState::Busy | BoxState::Compacting => {}
            BoxState::Created => {
                return Err(BoxError::StateError("VM not yet booted".to_string()));
            }
            BoxState::Stopped => {
                return Err(BoxError::StateError("VM is stopped".to_string()));
            }
        }
        drop(state);

        if self
            .resolved_execution_plan
            .as_ref()
            .is_some_and(|plan| plan.backend.is_sandbox())
            || self.config.isolation.is_sandbox()
        {
            return Err(BoxError::StateError(
                "Pause is not supported by the Sandbox backend yet".to_string(),
            ));
        }

        if let Some(pid) = self.pid().await {
            // Safety: sending SIGSTOP to pause the process
            let ret = unsafe { libc::kill(pid as i32, libc::SIGSTOP) };
            if ret != 0 {
                let err = std::io::Error::last_os_error();
                return Err(BoxError::ExecError(format!(
                    "Failed to send SIGSTOP to pid {}: {}",
                    pid, err
                )));
            }
            tracing::info!(box_id = %self.box_id, pid, "VM paused");
            Ok(())
        } else {
            Err(BoxError::StateError(
                "VM has no running process".to_string(),
            ))
        }
    }

    /// Resume the VM by sending SIGCONT to the shim process.
    ///
    /// Can be called on a paused VM to resume execution.
    #[cfg(unix)]
    pub async fn resume(&self) -> Result<()> {
        if self
            .resolved_execution_plan
            .as_ref()
            .is_some_and(|plan| plan.backend.is_sandbox())
            || self.config.isolation.is_sandbox()
        {
            return Err(BoxError::StateError(
                "Resume is not supported by the Sandbox backend yet".to_string(),
            ));
        }
        if let Some(pid) = self.pid().await {
            // Safety: sending SIGCONT to resume the process
            let ret = unsafe { libc::kill(pid as i32, libc::SIGCONT) };
            if ret != 0 {
                let err = std::io::Error::last_os_error();
                return Err(BoxError::ExecError(format!(
                    "Failed to send SIGCONT to pid {}: {}",
                    pid, err
                )));
            }
            tracing::info!(box_id = %self.box_id, pid, "VM resumed");
            Ok(())
        } else {
            Err(BoxError::StateError(
                "VM has no running process".to_string(),
            ))
        }
    }

    /// Pause the VM (Windows stub - not yet implemented).
    #[cfg(windows)]
    pub async fn pause(&self) -> Result<()> {
        Err(BoxError::StateError(
            "VM pause is not yet supported on Windows".to_string(),
        ))
    }

    /// Resume the VM (Windows stub - not yet implemented).
    #[cfg(windows)]
    pub async fn resume(&self) -> Result<()> {
        Err(BoxError::StateError(
            "VM resume is not yet supported on Windows".to_string(),
        ))
    }

    /// Check if VM is healthy.
    pub async fn health_check(&self) -> Result<bool> {
        let state = self.state.read().await;

        match *state {
            BoxState::Ready | BoxState::Busy | BoxState::Compacting => {
                // Check if handler reports VM is running
                if let Some(ref handler) = *self.handler.read().await {
                    Ok(handler.is_running())
                } else {
                    Ok(false)
                }
            }
            _ => Ok(false),
        }
    }

    /// Get VM metrics.
    pub async fn metrics(&self) -> Option<crate::vmm::VmMetrics> {
        let vm_metrics = self
            .handler
            .read()
            .await
            .as_ref()
            .map(|handler| handler.metrics())?;

        // Update per-VM Prometheus gauges if metrics are attached
        if let Some(ref prom) = self.prom {
            prom.vm_cpu_percent
                .with_label_values(&[&self.box_id])
                .set(vm_metrics.cpu_percent.unwrap_or(0.0) as f64);
            prom.vm_memory_bytes
                .with_label_values(&[&self.box_id])
                .set(vm_metrics.memory_bytes.unwrap_or(0) as f64);
        }

        Some(vm_metrics)
    }

    /// Get the PID of the VM shim process.
    pub async fn pid(&self) -> Option<u32> {
        self.handler
            .read()
            .await
            .as_ref()
            .map(|handler| handler.pid())
    }

    /// Get the TEE extension, if TEE is configured and VM is booted.
    #[cfg(unix)]
    pub fn tee(&self) -> Option<&dyn TeeExtension> {
        self.tee.as_deref()
    }

    /// Get the TEE extension or return an error.
    #[cfg(unix)]
    pub fn require_tee(&self) -> Result<&dyn TeeExtension> {
        self.tee.as_deref().ok_or_else(|| {
            BoxError::AttestationError("TEE is not configured for this box".to_string())
        })
    }

    /// Apply a live resource update to the running backend.
    ///
    /// Tier 1 changes (provisioned vCPU count and memory size) retain the public
    /// stop/recreate contract across backends.
    ///
    /// Tier 2 changes use one backend-owned path: the exact-generation A3S OCI
    /// update for a host Sandbox, or guest cgroup writes for a MicroVM.
    #[cfg(unix)]
    pub async fn update_resources(
        &mut self,
        update: &crate::resize::ResourceUpdate,
    ) -> Result<crate::resize::ResizeResult> {
        crate::resize::validate_update(update)?;

        let mut result = crate::resize::ResizeResult {
            applied: Vec::new(),
            rejected: Vec::new(),
        };

        if !update.has_tier2_changes() {
            return Ok(result);
        }

        let sandbox = self
            .resolved_execution_plan
            .as_ref()
            .is_some_and(|plan| plan.backend.is_sandbox())
            || self.config.isolation.is_sandbox();
        if sandbox {
            let mut next_config = self.config.clone();
            update.apply_to_config(&mut next_config);
            let runtime_config = next_config.clone();
            let box_dir = self.home_dir.join("boxes").join(&self.box_id);
            let box_id = self.box_id.clone();
            tokio::task::spawn_blocking(move || {
                crate::sandbox::update_recorded_resources(&box_dir, &box_id, &runtime_config)
            })
            .await
            .map_err(|error| {
                BoxError::StateError(format!(
                    "A3S OCI resource update worker failed for {}: {error}",
                    self.box_id
                ))
            })??;
            self.config = next_config;
            result.applied = update
                .tier2_change_names()
                .into_iter()
                .map(str::to_string)
                .collect();
            return Ok(result);
        }

        // Build cgroup commands and execute them inside the guest
        let commands = update.build_microvm_cgroup_commands();
        for cmd_str in &commands {
            let shell_cmd = vec!["sh".to_string(), "-c".to_string(), cmd_str.clone()];

            match self.exec_command(shell_cmd, 5_000_000_000).await {
                Ok(output) if output.exit_code == 0 => {
                    result.applied.push(cmd_str.clone());
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let reason = if stderr.trim().is_empty() {
                        format!("exit code {}", output.exit_code)
                    } else {
                        stderr.trim().to_string()
                    };
                    tracing::warn!(
                        box_id = %self.box_id,
                        cmd = %cmd_str,
                        exit_code = output.exit_code,
                        stderr = %stderr,
                        "Cgroup update failed inside guest"
                    );
                    result.rejected.push((cmd_str.clone(), reason));
                }
                Err(e) => {
                    tracing::warn!(
                        box_id = %self.box_id,
                        cmd = %cmd_str,
                        error = %e,
                        "Failed to exec cgroup update in guest"
                    );
                    result.rejected.push((cmd_str.clone(), e.to_string()));
                }
            }
        }

        Ok(result)
    }
}
