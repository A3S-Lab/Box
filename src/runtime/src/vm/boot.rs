//! VM boot transaction and guest-native rootfs handoff.

use super::*;

impl VmManager {
    /// Boot the VM.
    pub async fn boot(&mut self) -> Result<()> {
        let boot_span = tracing::info_span!("vm_boot", box_id = %self.box_id);
        // Check and transition state: Created → booting
        {
            let state = self.state.read().await;
            if *state != BoxState::Created {
                return Err(BoxError::StateError("VM already booted".to_string()));
            }
        }

        let box_dir = self.home_dir.join("boxes").join(&self.box_id);
        self.preserve_rootfs_on_boot_failure =
            self.config.persistent && layout::persistent_rootfs_generation_exists(&box_dir)?;

        let execution_plan = a3s_box_core::resolve_execution(&self.config)?;
        self.resolved_execution_plan = Some(execution_plan.clone());
        if execution_plan.backend.is_sandbox() {
            let boot_start = std::time::Instant::now();
            return self
                .boot_sandbox(execution_plan, &boot_span, boot_start)
                .await;
        }

        let boot_start = std::time::Instant::now();

        tracing::info!(parent: &boot_span, box_id = %self.box_id, "Booting VM");

        // 1. Prepare filesystem layout
        let layout = match self
            .prepare_layout()
            .instrument(tracing::info_span!(parent: &boot_span, "prepare_layout"))
            .await
        {
            Ok(layout) => layout,
            Err(error) => {
                self.cleanup_boot_failure().await;
                return Err(error);
            }
        };
        self.image_config = layout.oci_config.clone();

        // `prepare_layout` may only now have mounted a Snapshot lower through
        // this box's overlay. Stage via the exact guest-visible root so rename
        // copy-ups into the per-box upper before any guest process can launch.
        if !self.rootfs_provider.guest_owns_terminal_fencing() {
            if let Err(error) =
                a3s_box_core::rootfs_metadata::stage_terminal_rootfs_metadata_for_boot(
                    &layout.rootfs_path,
                )
            {
                self.cleanup_boot_failure().await;
                return Err(BoxError::IoError(error));
            }
        }

        // 2. Build InstanceSpec
        let mut spec = match self.build_microvm_instance_spec(&layout) {
            Ok(s) => s,
            Err(e) => {
                self.cleanup_boot_failure().await;
                return Err(e);
            }
        };

        // 2.5. Configure bridge networking if requested
        let bridge_network = match &self.config.network {
            a3s_box_core::NetworkMode::Bridge { network } => Some(network.clone()),
            _ => None,
        };
        if let Some(network_name) = bridge_network.as_deref() {
            let net_config = match self.setup_bridge_network(network_name) {
                Ok(n) => n,
                Err(e) => {
                    self.cleanup_boot_failure().await;
                    return Err(e);
                }
            };

            // Inject network env vars into entrypoint so they are passed via
            // krun_set_exec's envp (not krun_set_env which overwrites all vars).
            let ip_cidr = format!("{}/{}", net_config.ip_address, net_config.prefix_len);
            spec.entrypoint
                .env
                .push(("A3S_NET_IP".to_string(), ip_cidr));
            spec.entrypoint.env.push((
                "A3S_NET_GATEWAY".to_string(),
                net_config.gateway.to_string(),
            ));
            spec.entrypoint.env.push((
                "A3S_NET_DNS".to_string(),
                net_config
                    .dns_servers
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            ));

            spec.network = Some(net_config);
        }

        #[cfg(target_os = "macos")]
        if spec.network.is_none()
            && matches!(self.config.network, a3s_box_core::NetworkMode::Tsi)
            && !self.config.port_map.is_empty()
        {
            let net_config = match self.setup_published_default_network() {
                Ok(network) => network,
                Err(error) => {
                    self.cleanup_boot_failure().await;
                    return Err(error);
                }
            };
            let ip_cidr = format!("{}/{}", net_config.ip_address, net_config.prefix_len);
            spec.entrypoint
                .env
                .push(("A3S_NET_IP".to_string(), ip_cidr));
            spec.entrypoint.env.push((
                "A3S_NET_GATEWAY".to_string(),
                net_config.gateway.to_string(),
            ));
            spec.entrypoint.env.push((
                "A3S_NET_DNS".to_string(),
                net_config
                    .dns_servers
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            ));
            spec.network = Some(net_config);
        }

        // Resolve all dynamic launch-time files only after network allocation.
        // New guest-init images receive them through the private boot share;
        // legacy images without guest-init retain the directory-root fallback.
        let host_config = match self.guest_host_config(
            bridge_network.as_deref(),
            spec.network
                .as_ref()
                .map(|network| network.dns_servers.as_slice()),
        ) {
            Ok(config) => config,
            Err(error) => {
                self.cleanup_boot_failure().await;
                return Err(error);
            }
        };
        let uses_guest_boot_config =
            match Self::finalize_microvm_guest_boot_config(&spec, host_config) {
                Ok(value) => value,
                Err(error) => {
                    self.cleanup_boot_failure().await;
                    return Err(error);
                }
            };
        if layout.resumed_rootfs.is_some() && !uses_guest_boot_config {
            self.cleanup_boot_failure().await;
            return Err(BoxError::BoxBootError {
                message: "guest-owned rootfs did not select the private guest boot transport"
                    .to_string(),
                hint: None,
            });
        }
        if !uses_guest_boot_config {
            let resolv_content = a3s_box_core::dns::generate_resolv_conf(&self.config.dns);
            if let Err(error) = crate::oci::rootfs::write_guest_file(
                &layout.rootfs_path,
                "etc/resolv.conf",
                &resolv_content,
            ) {
                self.cleanup_boot_failure().await;
                return Err(error);
            }
            if let Err(error) = self.write_hostname_file(&layout) {
                self.cleanup_boot_failure().await;
                return Err(error);
            }
            let hosts_result = match bridge_network.as_deref() {
                Some(network_name) => self.write_hosts_file(&layout, network_name),
                None => self.write_standalone_hosts_file(&layout),
            };
            if let Err(error) = hosts_result {
                self.cleanup_boot_failure().await;
                return Err(error);
            }
        }

        // Directory providers retain the compatibility host-side baseline.
        // Guest-native providers capture it inside guest-init after ownership
        // handoff, before any workload or sidecar process can mutate the disk.
        if !self.rootfs_provider.guest_owns_diff_baseline() {
            self.create_diff_baseline(&layout);
        }

        #[cfg(target_os = "macos")]
        let artifact_cache = match self.rootfs_artifact_cache_options(
            &layout,
            &spec.entrypoint.executable,
            uses_guest_boot_config,
        ) {
            Ok(cache) => cache,
            Err(error) => {
                self.cleanup_boot_failure().await;
                return Err(error);
            }
        };
        #[cfg(not(target_os = "macos"))]
        let artifact_cache = None;

        // This is the ownership boundary between a host-visible staging tree
        // and the root filesystem presented to the guest. Providers may keep
        // the directory transport, or atomically publish a guest-native block
        // artifact after every host-side mutation is complete.
        if let Some(resumed) = layout.resumed_rootfs.as_ref() {
            spec.rootfs = resumed.source.clone();
        } else {
            spec.rootfs = match self.rootfs_provider.finalize_for_boot(
                &box_dir,
                &layout.rootfs_path,
                crate::rootfs::RootfsFinalizeOptions {
                    disk_mib: self.config.resources.disk_mb,
                    persistent: self.config.persistent,
                    snapshot: super::rootfs_snapshot_requested(&self.config),
                    artifact_cache,
                },
            ) {
                Ok(rootfs) => rootfs,
                Err(error) => {
                    self.cleanup_boot_failure().await;
                    return Err(error);
                }
            };
        }

        // 3. Initialize VMM provider (use injected provider or default to VmController)
        if self.provider.is_none() {
            let shim_path = match VmController::find_shim() {
                Ok(p) => p,
                Err(e) => {
                    self.cleanup_boot_failure().await;
                    return Err(e);
                }
            };
            let controller = match VmController::new(shim_path) {
                Ok(c) => c,
                Err(e) => {
                    self.cleanup_boot_failure().await;
                    return Err(e);
                }
            };
            self.provider = Some(Box::new(controller));
        }

        // 4. Start VM via provider
        let handler = {
            let provider = self
                .provider
                .as_ref()
                .ok_or_else(|| BoxError::BoxBootError {
                    message: "VMM provider not initialized".to_string(),
                    hint: Some("Ensure VmManager has a provider set before boot".to_string()),
                })?;
            let vm_start_span = tracing::info_span!(parent: &boot_span, "vm_start");
            match async { provider.start(&spec).await }
                .instrument(vm_start_span)
                .await
            {
                Ok(h) => h,
                Err(e) => {
                    self.cleanup_boot_failure().await;
                    return Err(e);
                }
            }
        };

        // Store handler
        *self.handler.write().await = Some(handler);

        // 5. Wait for guest ready
        {
            let wait_span = tracing::info_span!(parent: &boot_span, "wait_for_ready");
            if let Err(e) = async {
                self.wait_for_vm_running().await?;

                // 5b. Become ready. A snapshot-restore boot resumes an already-booted
                // guest whose exec server won't re-signal readiness, so the cold-boot
                // wait would stall registration on its safety cap — do one best-effort
                // probe instead. A normal boot waits for the Heartbeat health check.
                #[cfg(unix)]
                if is_restore_mode(&self.config) {
                    self.probe_exec_ready_once(&layout.exec_socket_path).await;
                } else {
                    self.wait_for_exec_ready(&layout.exec_socket_path).await?;
                }
                #[cfg(windows)]
                self.wait_for_exec_ready(&layout.exec_socket_path).await?;
                Ok::<(), BoxError>(())
            }
            .instrument(wait_span)
            .await
            {
                self.cleanup_boot_failure().await;
                return Err(e);
            }
        }

        if self.rootfs_provider.guest_owns_diff_baseline() {
            if let Err(error) = crate::rootfs::publish_guest_diff_baseline(&box_dir) {
                self.cleanup_boot_failure().await;
                return Err(error);
            }
        }

        // guest-init has consumed and unmounted the one-shot boot share before
        // signalling readiness. Remove its host payload now so even a later
        // privileged remount cannot recover workload environment data.
        if let Err(error) = Self::clear_microvm_guest_boot_config(&spec) {
            self.cleanup_boot_failure().await;
            return Err(error);
        }

        // Prototype: deferred-main-spawn. The guest booted IDLE (BOX_DEFERRED_MAIN);
        // now that the exec server is ready, tell it to spawn the container command
        // (already passed via BOX_EXEC_*) as the MAIN process — full box semantics
        // (exit code + json-file console logs) without a cold boot.
        // Auto-trigger spawn-main only for the env-driven `run` path, where the
        // command is known at boot. The pool sets config.deferred_main to boot the
        // VM IDLE but drives spawn-main EXPLICITLY per request (the per-request
        // command isn't known at pre-warm), so a pool VM must NOT auto-trigger here.
        // A restored guest's main is ALREADY running (captured in the snapshot), so
        // it must never re-spawn — doing so would start a duplicate main.
        #[cfg(unix)]
        if !is_restore_mode(&self.config)
            && std::env::var("BOX_DEFERRED_MAIN")
                .map(|v| v == "1")
                .unwrap_or(false)
        {
            if let Some(client) = self.exec_client.as_ref() {
                match client.spawn_main(None).await {
                    Ok(true) => tracing::info!("deferred container main spawned"),
                    Ok(false) => tracing::warn!("deferred spawn-main not acknowledged"),
                    Err(e) => tracing::warn!(error = %e, "deferred spawn-main failed"),
                }
            }
        }

        // 5b2. Store socket paths for CRI streaming access
        self.exec_socket_path = Some(layout.exec_socket_path.clone());
        self.pty_socket_path = Some(layout.pty_socket_path.clone());
        self.port_forward_socket_path = Some(layout.port_forward_socket_path.clone());

        // 5c. Initialize TEE extension for TEE environments
        #[cfg(unix)]
        if !matches!(self.config.tee, TeeConfig::None) {
            self.tee = Some(Box::new(crate::tee::SnpTeeExtension::new(
                self.box_id.clone(),
                layout.attest_socket_path.clone(),
            )));
        }

        // 6. Update state to Ready
        *self.state.write().await = BoxState::Ready;

        // Record Prometheus metrics
        if let Some(ref prom) = self.prom {
            let boot_duration = boot_start.elapsed().as_secs_f64();
            prom.vm_boot_duration.observe(boot_duration);
            prom.vm_created_total.inc();
            prom.vm_count.with_label_values(&["ready"]).inc();
        }

        // Emit ready event
        self.event_emitter.emit(BoxEvent::empty("box.ready"));

        tracing::info!(parent: &boot_span, box_id = %self.box_id, "VM ready");

        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn rootfs_artifact_cache_options(
        &self,
        layout: &BoxLayout,
        guest_executable: &str,
        uses_guest_boot_config: bool,
    ) -> Result<Option<crate::rootfs::RootfsArtifactCacheOptions>> {
        if !self.rootfs_provider.supports_artifact_cache()
            || !self.config.cache.enabled
            || !uses_guest_boot_config
            || layout.resumed_rootfs.is_some()
        {
            return Ok(None);
        }
        let Some(oci_manifest_digest) = layout.oci_manifest_digest.as_ref() else {
            return Ok(None);
        };
        let relative = guest_executable.strip_prefix('/').ok_or_else(|| {
            BoxError::BuildError(format!(
                "guest-init executable is not absolute: {guest_executable}"
            ))
        })?;
        let guest_init =
            crate::oci::rootfs::resolve_guest_file_path(&layout.rootfs_path, relative)?;
        let guest_init_sha256 = Self::guest_init_sha256(&guest_init)?;

        let architecture = a3s_box_core::platform::Platform::host().architecture;
        Ok(Some(crate::rootfs::RootfsArtifactCacheOptions {
            directory: self.resolve_cache_dir().join("rootfs-ext4-v1"),
            oci_manifest_digest: oci_manifest_digest.clone(),
            platform: format!("linux/{architecture}"),
            guest_init_sha256,
            max_entries: self.config.cache.max_rootfs_entries,
            max_allocated_bytes: self.config.cache.max_cache_bytes,
        }))
    }

    pub(crate) fn guest_init_sha256(path: &std::path::Path) -> Result<String> {
        use sha2::{Digest, Sha256};
        use std::io::Read;

        let path_metadata = std::fs::symlink_metadata(path).map_err(BoxError::IoError)?;
        let mut open_options = std::fs::OpenOptions::new();
        open_options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open_options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        }
        let mut file = open_options.open(path).map_err(BoxError::IoError)?;
        let metadata = file.metadata().map_err(BoxError::IoError)?;
        const MAX_GUEST_INIT_BYTES: u64 = 256 * 1024 * 1024;
        if !path_metadata.is_file()
            || path_metadata.file_type().is_symlink()
            || !metadata.is_file()
            || path_metadata.len() != metadata.len()
            || metadata.len() > MAX_GUEST_INIT_BYTES
        {
            return Err(BoxError::BuildError(format!(
                "guest-init cache identity source is not a bounded plain file: {}",
                path.display()
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if path_metadata.dev() != metadata.dev() || path_metadata.ino() != metadata.ino() {
                return Err(BoxError::BuildError(format!(
                    "guest-init changed while opening cache identity source: {}",
                    path.display()
                )));
            }
        }
        let expected_length = metadata.len();
        let mut read_length = 0u64;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 1024 * 1024];
        {
            let mut bounded = file.by_ref().take(MAX_GUEST_INIT_BYTES + 1);
            loop {
                let read = bounded.read(&mut buffer).map_err(BoxError::IoError)?;
                if read == 0 {
                    break;
                }
                read_length = read_length.checked_add(read as u64).ok_or_else(|| {
                    BoxError::BuildError("guest-init length overflow".to_string())
                })?;
                if read_length > expected_length {
                    return Err(BoxError::BuildError(format!(
                        "guest-init changed while computing cache identity: {}",
                        path.display()
                    )));
                }
                hasher.update(&buffer[..read]);
            }
        }
        if read_length != expected_length
            || file.metadata().map_err(BoxError::IoError)?.len() != expected_length
        {
            return Err(BoxError::BuildError(format!(
                "guest-init changed while computing cache identity: {}",
                path.display()
            )));
        }
        Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
    }

    pub(super) fn create_diff_baseline(&self, layout: &BoxLayout) {
        let box_dir = self.home_dir.join("boxes").join(&self.box_id);
        if let Err(error) =
            crate::rootfs::create_diff_baseline_if_absent(&box_dir, &layout.rootfs_path)
        {
            tracing::warn!(
                box_id = %self.box_id,
                %error,
                "Failed to create rootfs diff baseline before workload launch"
            );
        }
    }
}
