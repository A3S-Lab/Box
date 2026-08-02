//! Shared-kernel Sandbox boot path.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::event::BoxEvent;
use a3s_box_core::execution::ResolvedExecutionPlan;
use a3s_box_core::guest_exec::{
    GuestExecConfig, MAX_RUNTIME_EXEC_CONFIG_BYTES, RUNTIME_EXEC_CONFIG_PATH,
};
use base64::Engine;
use sha2::{Digest, Sha256};

use crate::sandbox::rootfs::{
    inspect_rootfs_identity_requirements_with_preference, prepare_rootfs_ownership_with_preference,
};
use crate::sandbox::A3sOciController;
use crate::sandbox::{
    compile_oci_spec, compile_runtime_owned_oci_spec, plan_id_mappings,
    prepare_managed_mount_source, prepare_managed_secret_mount_source, prepare_sandbox_path_access,
    probe_sandbox_capabilities_for, stage_read_only_mount_aliases, validate_external_mount_access,
    write_bundle, SandboxBundleSpec, SandboxCapabilitySnapshot, SandboxLaunchSpec, SandboxMount,
    SandboxResources, SandboxRuntimeProcess, SandboxTmpfs,
};

use super::{BoxState, VmManager};

/// Product-owned bundle ready to be loaded through the public A3S OCI SDK.
#[derive(Debug, Clone)]
pub(crate) struct RuntimeOwnedSandboxBundle {
    pub bundle_dir: PathBuf,
    pub console_output: PathBuf,
    pub anonymous_volumes: Vec<String>,
}

impl VmManager {
    pub(super) async fn boot_sandbox(
        &mut self,
        execution_plan: ResolvedExecutionPlan,
        boot_span: &tracing::Span,
        boot_start: std::time::Instant,
    ) -> Result<()> {
        if execution_plan.backend != a3s_box_core::ExecutionBackend::A3sOci {
            return Err(BoxError::BoxBootError {
                message: "A3S OCI Runtime is the only supported Sandbox backend".to_string(),
                hint: None,
            });
        }
        // This probe is deliberately before image pulls, rootfs mounts, volume
        // creation, or bundle writes. Every mandatory control is fail-closed.
        let capability_start = std::time::Instant::now();
        let capabilities = probe_sandbox_capabilities_for(execution_plan.backend, None, None);
        capabilities.require_ready()?;
        // Sandbox logging is hosted by the packaged shim in a dedicated worker
        // mode so it survives detached CLI clients. Resolve it before image or
        // rootfs preparation to keep a missing artifact side-effect free.
        let log_worker_path = crate::vmm::VmController::find_shim()?;
        let user_namespace =
            capabilities
                .user_namespace
                .as_ref()
                .ok_or_else(|| BoxError::BoxBootError {
                    message: "Sandbox capability probe did not return user-namespace evidence"
                        .to_string(),
                    hint: None,
                })?;
        a3s_box_core::lifecycle_profile::record_lifecycle_phase(
            "sandbox.capability",
            capability_start.elapsed(),
        );

        let box_dir = self.home_dir.join("boxes").join(&self.box_id);
        let sandbox_dir = box_dir.join("sandbox");
        let bundle_dir = sandbox_dir.join("bundle");
        let runtime_root = super::sandbox_runtime_root(&self.home_dir, &self.box_id);
        let runtime_record = sandbox_dir.join("runtime.json");
        let runtime = capabilities
            .a3s_oci
            .clone()
            .ok_or_else(|| BoxError::BoxBootError {
                message: "Sandbox capability probe returned no A3S OCI artifacts".to_string(),
                hint: None,
            })?;
        let runtime_digest =
            combined_runtime_digest(&runtime.runtime_sha256, &runtime.agent_sha256);
        A3sOciController::new(runtime.clone()).require_absent(&runtime_root, &self.box_id)?;

        tracing::info!(
            parent: boot_span,
            box_id = %self.box_id,
            isolation_class = "shared-kernel",
            runtime = ?execution_plan.backend,
            "Booting Sandbox"
        );

        let layout_start = std::time::Instant::now();
        let layout = match self.prepare_layout().await {
            Ok(layout) => layout,
            Err(error) => {
                self.cleanup_boot_failure().await;
                return Err(error);
            }
        };
        a3s_box_core::lifecycle_profile::record_lifecycle_phase(
            "sandbox.layout",
            layout_start.elapsed(),
        );
        self.image_config = layout.oci_config.clone();

        let prepare = (|| -> Result<_> {
            // Snapshot-backed rootfs overlays are mounted by `prepare_layout`.
            // Stage through the exact merged root before ownership planning or
            // runtime launch so the terminal completion marker is invalidated in
            // this box's writable upper, never in the shared Snapshot lower.
            a3s_box_core::rootfs_metadata::stage_terminal_rootfs_metadata_for_boot(
                &layout.rootfs_path,
            )?;
            let instance_prepare_start = std::time::Instant::now();
            let resolv_content = a3s_box_core::dns::generate_resolv_conf(&self.config.dns);
            crate::oci::rootfs::write_guest_file(
                &layout.rootfs_path,
                "etc/resolv.conf",
                resolv_content,
            )?;
            self.write_hostname_file(&layout)?;
            self.write_standalone_hosts_file(&layout)?;

            let resources = SandboxResources::from_box_config(&self.config)?;
            let mut instance_spec = self.build_instance_spec(&layout)?;
            apply_sandbox_workload_resources(&mut instance_spec.entrypoint.env, &resources)?;
            if !matches!(
                instance_spec.entrypoint.executable.as_str(),
                "/sbin/init" | "/usr/sbin/init"
            ) || !instance_spec.entrypoint.env.iter().any(|(key, value)| {
                key == "BOX_EXEC_CONFIG_FILE" && value == RUNTIME_EXEC_CONFIG_PATH
            }) {
                return Err(BoxError::BoxBootError {
                    message: "Sandbox requires the packaged a3s-box guest init as OCI PID 1"
                        .to_string(),
                    hint: Some("Install the matching a3s-box-guest-init artifact".to_string()),
                });
            }

            let (mut mounts, tmpfs) = self.compile_sandbox_mounts(&layout, &instance_spec)?;
            ensure_mount_destinations(&layout.rootfs_path, &mounts, &tmpfs)?;

            let rootfs_ids = inspect_rootfs_identity_requirements_with_preference(
                &layout.rootfs_path,
                layout.prefer_image_rootfs_metadata,
            )?;
            let (account_uid, account_gid) = maximum_account_ids(&layout.rootfs_path)?;
            let (process_uid, process_gid) =
                maximum_process_ids(&layout.rootfs_path, &instance_spec.entrypoint.env)?;
            let maximum_uid = rootfs_ids.maximum_uid.max(account_uid).max(process_uid);
            let maximum_gid = rootfs_ids.maximum_gid.max(account_gid).max(process_gid);
            let id_mappings = plan_id_mappings(user_namespace, maximum_uid, maximum_gid)?;
            a3s_box_core::lifecycle_profile::record_lifecycle_phase(
                "sandbox.instance_prepare",
                instance_prepare_start.elapsed(),
            );

            let mount_sources_start = std::time::Instant::now();
            self.prepare_sandbox_mount_sources(&layout, &mut mounts, &id_mappings)?;
            a3s_box_core::lifecycle_profile::record_lifecycle_phase(
                "sandbox.mount_sources",
                mount_sources_start.elapsed(),
            );
            let rootfs_ownership_start = std::time::Instant::now();
            prepare_rootfs_ownership_with_preference(
                &layout.rootfs_path,
                &id_mappings,
                user_namespace.effective_uid,
                self.config.read_only,
                layout.prefer_image_rootfs_metadata,
            )?;
            a3s_box_core::lifecycle_profile::record_lifecycle_phase(
                "sandbox.rootfs_ownership",
                rootfs_ownership_start.elapsed(),
            );

            let bundle_start = std::time::Instant::now();
            let execution_plan_digest = digest_json(&execution_plan)?;
            let bundle_spec = SandboxBundleSpec {
                box_id: self.box_id.clone(),
                rootfs_path: layout.rootfs_path.clone(),
                rootfs_read_only: self.config.read_only,
                hostname: self
                    .config
                    .hostname
                    .clone()
                    .unwrap_or_else(|| self.box_id.clone()),
                init_path: instance_spec.entrypoint.executable.clone(),
                init_environment: instance_spec.entrypoint.env.clone(),
                mounts,
                tmpfs,
                id_mappings,
                resources,
                requested_capabilities: self.config.cap_add.clone(),
                execution_plan_digest,
                runtime_digest,
            };
            let oci_spec = compile_oci_spec(&bundle_spec)?;
            write_bundle(&bundle_dir, &oci_spec, &execution_plan, &capabilities)?;
            prepare_sandbox_path_access(
                &self.home_dir,
                &self.box_id,
                &bundle_dir,
                &layout.rootfs_path,
                &bundle_spec.mounts,
                &bundle_spec.id_mappings,
            )?;
            a3s_box_core::lifecycle_profile::record_lifecycle_phase(
                "sandbox.bundle",
                bundle_start.elapsed(),
            );

            Ok((instance_spec, bundle_spec))
        })();

        let (instance_spec, _bundle_spec) = match prepare {
            Ok(value) => value,
            Err(error) => {
                self.cleanup_boot_failure().await;
                return Err(error);
            }
        };

        // `controller.start` launches PID 1 and the user command. Preserve the
        // pristine baseline before that boundary so short-lived filesystem
        // mutations cannot win a race against the CLI's post-start bookkeeping.
        self.create_diff_baseline(&layout);

        let console_output = instance_spec
            .console_output
            .clone()
            .unwrap_or_else(|| box_dir.join("logs").join("console.log"));
        let launch = SandboxLaunchSpec {
            container_id: self.box_id.clone(),
            bundle_dir,
            runtime_root,
            runtime_record,
            exec_socket_path: layout.exec_socket_path.clone(),
            pty_socket_path: layout.pty_socket_path.clone(),
            stdout_path: console_output.clone(),
            stderr_path: a3s_box_core::log::stderr_console_path(&console_output),
            init_log_path: box_dir.join("logs").join("sandbox-init.log"),
            log_config: self.log_config.clone(),
            log_worker_path,
            log_worker_log_path: box_dir.join("logs").join("sandbox-log-worker.log"),
            log_worker_ready_path: sandbox_dir.join("bundle").join("log-worker.ready"),
        };
        let launch_start = std::time::Instant::now();
        let controller = A3sOciController::new(runtime);
        let handler: Box<dyn a3s_box_core::vmm::VmHandler> = match controller.start(launch).await {
            Ok(handler) => Box::new(handler),
            Err(error) => {
                self.cleanup_boot_failure().await;
                return Err(error);
            }
        };
        a3s_box_core::lifecycle_profile::record_lifecycle_phase(
            "sandbox.launch",
            launch_start.elapsed(),
        );
        *self.handler.write().await = Some(handler);

        let readiness_start = std::time::Instant::now();
        if let Err(error) = async {
            // The selected controller returns only after the exact runtime
            // reports this exact generation as running. The generic VM grace
            // period would merely recheck process liveness for a fixed 250 ms;
            // the heartbeat path below already checks liveness on every
            // attempt and returns immediately for a naturally exited one-shot.
            #[cfg(unix)]
            self.wait_for_exec_ready(&layout.exec_socket_path).await?;
            Ok(())
        }
        .await
        {
            self.cleanup_boot_failure().await;
            return Err(error);
        }
        a3s_box_core::lifecycle_profile::record_lifecycle_phase(
            "sandbox.readiness",
            readiness_start.elapsed(),
        );

        self.exec_socket_path = Some(layout.exec_socket_path);
        self.pty_socket_path = Some(layout.pty_socket_path);
        // Port publishing is intentionally rejected for Sandbox. Keep no stale
        // VM port-forward path in the public manager state.
        self.port_forward_socket_path = None;
        *self.state.write().await = BoxState::Ready;

        if let Some(ref prom) = self.prom {
            prom.vm_boot_duration
                .observe(boot_start.elapsed().as_secs_f64());
            prom.vm_created_total.inc();
            prom.vm_count.with_label_values(&["ready"]).inc();
        }
        self.event_emitter.emit(BoxEvent::empty("box.ready"));
        tracing::info!(
            parent: boot_span,
            box_id = %self.box_id,
            "Sandbox ready"
        );
        a3s_box_core::lifecycle_profile::record_lifecycle_phase(
            "sandbox.start_total",
            boot_start.elapsed(),
        );
        Ok(())
    }

    /// Prepare a Sandbox bundle for a long-lived OCI Runtime service. The
    /// runtime owns PID 1 and process I/O, so this path compiles the image
    /// process directly instead of launching Box guest-init with inherited FDs.
    pub(crate) async fn prepare_runtime_owned_sandbox_bundle(
        &mut self,
        execution_plan: &ResolvedExecutionPlan,
        capabilities: &SandboxCapabilitySnapshot,
    ) -> Result<RuntimeOwnedSandboxBundle> {
        if execution_plan.backend != a3s_box_core::ExecutionBackend::A3sOci {
            return Err(BoxError::BoxBootError {
                message: "A3S OCI Runtime is the only supported Sandbox backend".to_string(),
                hint: None,
            });
        }
        capabilities.require_ready()?;
        let runtime = capabilities
            .a3s_oci
            .as_ref()
            .ok_or_else(|| BoxError::BoxBootError {
                message: "Sandbox capability probe returned no A3S OCI artifacts".to_string(),
                hint: None,
            })?;
        let user_namespace =
            capabilities
                .user_namespace
                .as_ref()
                .ok_or_else(|| BoxError::BoxBootError {
                    message: "Sandbox capability probe did not return user-namespace evidence"
                        .to_string(),
                    hint: None,
                })?;
        let runtime_digest =
            combined_runtime_digest(&runtime.runtime_sha256, &runtime.agent_sha256);
        let original_anonymous_volumes = self.anonymous_volumes.clone();
        let box_dir = self.home_dir.join("boxes").join(&self.box_id);
        let bundle_dir = box_dir.join("sandbox").join("bundle");

        let layout = match self.prepare_layout().await {
            Ok(layout) => layout,
            Err(error) => {
                self.cleanup_boot_failure().await;
                return Err(error);
            }
        };
        self.image_config = layout.oci_config.clone();

        let prepare = (|| -> Result<RuntimeOwnedSandboxBundle> {
            a3s_box_core::rootfs_metadata::stage_terminal_rootfs_metadata_for_boot(
                &layout.rootfs_path,
            )?;
            let resolv_content = a3s_box_core::dns::generate_resolv_conf(&self.config.dns);
            crate::oci::rootfs::write_guest_file(
                &layout.rootfs_path,
                "etc/resolv.conf",
                resolv_content,
            )?;
            self.write_hostname_file(&layout)?;
            self.write_standalone_hosts_file(&layout)?;

            let resources = SandboxResources::from_box_config(&self.config)?;
            let instance_spec = self.build_runtime_owned_instance_spec(&layout)?;
            if self.anonymous_volumes != original_anonymous_volumes {
                return Err(BoxError::ConfigError(
                    "OCI migration does not yet introduce image-declared anonymous volumes; create an explicit named or bind mount before enabling migration"
                        .to_string(),
                ));
            }
            let runtime_process = resolve_runtime_owned_process(
                &layout.rootfs_path,
                &instance_spec,
                &self.config.cap_drop,
            )?;
            let (mut mounts, tmpfs) = self.compile_sandbox_mounts(&layout, &instance_spec)?;
            ensure_mount_destinations(&layout.rootfs_path, &mounts, &tmpfs)?;

            let rootfs_ids = inspect_rootfs_identity_requirements_with_preference(
                &layout.rootfs_path,
                layout.prefer_image_rootfs_metadata,
            )?;
            let (account_uid, account_gid) = maximum_account_ids(&layout.rootfs_path)?;
            let process_uid = runtime_process.uid;
            let process_gid = std::iter::once(runtime_process.gid)
                .chain(runtime_process.additional_gids.iter().copied())
                .max()
                .unwrap_or(0);
            let maximum_uid = rootfs_ids.maximum_uid.max(account_uid).max(process_uid);
            let maximum_gid = rootfs_ids.maximum_gid.max(account_gid).max(process_gid);
            let id_mappings = plan_id_mappings(user_namespace, maximum_uid, maximum_gid)?;

            self.prepare_sandbox_mount_sources(&layout, &mut mounts, &id_mappings)?;
            prepare_rootfs_ownership_with_preference(
                &layout.rootfs_path,
                &id_mappings,
                user_namespace.effective_uid,
                self.config.read_only,
                layout.prefer_image_rootfs_metadata,
            )?;

            let bundle_spec = SandboxBundleSpec {
                box_id: self.box_id.clone(),
                rootfs_path: layout.rootfs_path.clone(),
                rootfs_read_only: self.config.read_only,
                hostname: self
                    .config
                    .hostname
                    .clone()
                    .unwrap_or_else(|| self.box_id.clone()),
                // Unused by the runtime-owned compiler, but retained in the
                // shared bundle input so mount/resource policy has one source.
                init_path: "/sbin/init".to_string(),
                init_environment: Vec::new(),
                mounts,
                tmpfs,
                id_mappings,
                resources,
                requested_capabilities: self.config.cap_add.clone(),
                execution_plan_digest: digest_json(execution_plan)?,
                runtime_digest,
            };
            let oci_spec = compile_runtime_owned_oci_spec(&bundle_spec, &runtime_process)?;
            write_bundle(&bundle_dir, &oci_spec, execution_plan, capabilities)?;
            prepare_sandbox_path_access(
                &self.home_dir,
                &self.box_id,
                &bundle_dir,
                &layout.rootfs_path,
                &bundle_spec.mounts,
                &bundle_spec.id_mappings,
            )?;
            self.create_diff_baseline(&layout);

            Ok(RuntimeOwnedSandboxBundle {
                bundle_dir,
                console_output: instance_spec
                    .console_output
                    .unwrap_or_else(|| box_dir.join("logs").join("console.log")),
                anonymous_volumes: self.anonymous_volumes.clone(),
            })
        })();

        match prepare {
            Ok(prepared) => Ok(prepared),
            Err(error) => {
                self.cleanup_boot_failure().await;
                Err(error)
            }
        }
    }

    /// Detach preparation-owned mounts after the OCI Runtime has deleted the
    /// exact generation. Persistent writable data follows normal Box policy.
    pub(crate) fn cleanup_runtime_owned_sandbox_bundle(&self) -> Result<()> {
        let box_dir = self.home_dir.join("boxes").join(&self.box_id);
        crate::sandbox::cleanup_sandbox_mount_aliases(&self.home_dir, &self.box_id)?;
        self.rootfs_provider
            .cleanup(&box_dir, self.config.persistent)?;
        for path in [box_dir.join("sandbox").join("bundle"), self.socket_dir()] {
            match std::fs::remove_dir_all(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(BoxError::IoError(error)),
            }
        }
        Ok(())
    }

    fn compile_sandbox_mounts(
        &self,
        layout: &super::BoxLayout,
        instance_spec: &crate::vmm::InstanceSpec,
    ) -> Result<(Vec<SandboxMount>, Vec<SandboxTmpfs>)> {
        let mut mounts = Vec::new();
        let mut user_destinations = HashSet::new();
        for volume in &self.config.volumes {
            let mount = parse_sandbox_volume(volume)?;
            user_destinations.insert(mount.destination.clone());
            mounts.push(mount);
        }
        if !user_destinations.contains(Path::new("/workspace")) {
            mounts.insert(
                0,
                SandboxMount {
                    source: layout.workspace_path.clone(),
                    destination: PathBuf::from("/workspace"),
                    read_only: false,
                },
            );
        }

        if let Some(image) = layout.oci_config.as_ref() {
            let mut anonymous_index = self.config.volumes.len();
            for destination in &image.volumes {
                let destination = normalized_container_path(destination, "volume destination")?;
                if user_destinations.contains(&destination) {
                    continue;
                }
                let tag = format!("vol{anonymous_index}");
                let source = instance_spec
                    .fs_mounts
                    .iter()
                    .find(|mount| mount.tag == tag)
                    .ok_or_else(|| BoxError::BoxBootError {
                        message: format!(
                            "Required Sandbox anonymous volume {tag} was not materialized"
                        ),
                        hint: None,
                    })?
                    .host_path
                    .canonicalize()
                    .map_err(BoxError::IoError)?;
                mounts.push(SandboxMount {
                    source,
                    destination,
                    read_only: false,
                });
                anonymous_index += 1;
            }
        }

        let mut tmpfs = Vec::with_capacity(self.config.tmpfs.len());
        for value in &self.config.tmpfs {
            tmpfs.push(parse_sandbox_tmpfs(value)?);
        }
        Ok((mounts, tmpfs))
    }

    fn prepare_sandbox_mount_sources(
        &self,
        layout: &super::BoxLayout,
        mounts: &mut [SandboxMount],
        id_mappings: &crate::sandbox::SandboxIdMappingPlan,
    ) -> Result<()> {
        let managed = self.managed_sandbox_mount_sources(&layout.workspace_path, mounts)?;
        let mut read_only_external = Vec::new();

        for mount in mounts.iter() {
            if self
                .managed_secret_root
                .as_ref()
                .is_some_and(|root| mount.source.starts_with(root))
            {
                prepare_managed_secret_mount_source(
                    self.managed_secret_root.as_deref().ok_or_else(|| {
                        BoxError::ConfigError("Sandbox Secret root disappeared".into())
                    })?,
                    &mount.source,
                    id_mappings,
                    mount.read_only,
                )
                .map_err(|error| mount_source_error("prepare managed Secret", mount, error))?;
            } else if managed.contains(&mount.source) {
                prepare_managed_mount_source(&mount.source, id_mappings)
                    .map_err(|error| mount_source_error("prepare managed source", mount, error))?;
            } else if mount.read_only {
                read_only_external.push(mount.source.clone());
            } else {
                validate_external_mount_access(&mount.source, id_mappings, mount.read_only)
                    .map_err(|error| {
                        mount_source_error("validate external source", mount, error)
                    })?;
            }
        }

        let aliases = stage_read_only_mount_aliases(
            &self.home_dir,
            &self.box_id,
            &read_only_external,
            id_mappings,
        )
        .map_err(|error| BoxError::BoxBootError {
            message: format!(
                "Failed to stage read-only Sandbox attachment aliases for [{}]: {error}",
                read_only_external
                    .iter()
                    .map(|source| source.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            hint: None,
        })?;
        for mount in mounts {
            if let Some(alias) = aliases.get(&mount.source) {
                mount.source = alias.clone();
            }
        }
        Ok(())
    }

    fn managed_sandbox_mount_sources(
        &self,
        workspace_path: &Path,
        mounts: &[SandboxMount],
    ) -> Result<HashSet<PathBuf>> {
        let mut managed = HashSet::new();
        if self.config.workspace.as_os_str().is_empty() {
            managed.insert(workspace_path.to_path_buf());
        }
        let volume_store = crate::volume::VolumeStore::new(
            self.home_dir.join("volumes.json"),
            self.home_dir.join("volumes"),
        );
        let volumes = volume_store.load()?;
        for name in &self.anonymous_volumes {
            let volume = volumes.get(name).ok_or_else(|| BoxError::BoxBootError {
                message: format!("Sandbox anonymous volume {name} disappeared during boot"),
                hint: None,
            })?;
            managed.insert(
                PathBuf::from(&volume.mount_point)
                    .canonicalize()
                    .map_err(BoxError::IoError)?,
            );
        }

        // Named volumes are resolved to host paths before VmManager boots, so
        // their names are not present in BoxConfig. Match only mount roots that
        // are registered in A3S's volume store; arbitrary bind mounts remain
        // external and are never chowned implicitly.
        for volume in volumes.values() {
            let Ok(source) = PathBuf::from(&volume.mount_point).canonicalize() else {
                // A stale, unused volume entry must not prevent unrelated boxes
                // from starting. A mounted missing path already fails while the
                // Sandbox volume specification is canonicalized.
                continue;
            };
            if mounts.iter().any(|mount| mount.source == source) {
                managed.insert(source);
            }
        }

        Ok(managed)
    }
}

fn mount_source_error(action: &str, mount: &SandboxMount, error: BoxError) -> BoxError {
    BoxError::BoxBootError {
        message: format!(
            "Failed to {action} {} for {}: {error}",
            mount.source.display(),
            mount.destination.display()
        ),
        hint: None,
    }
}

fn apply_sandbox_workload_resources(
    environment: &mut Vec<(String, String)>,
    resources: &SandboxResources,
) -> Result<()> {
    set_environment_value(
        environment,
        "A3S_SEC_MEM_LIMIT",
        resources.memory_limit.to_string(),
    );
    set_environment_value(
        environment,
        "A3S_SEC_CPU_QUOTA",
        resources.cpu_quota.to_string(),
    );
    set_environment_value(
        environment,
        "A3S_SEC_CPU_PERIOD",
        resources.cpu_period.to_string(),
    );
    set_environment_value(
        environment,
        "A3S_SEC_PIDS_LIMIT",
        resources.pids_limit.to_string(),
    );
    if let Some(reservation) = resources.memory_reservation {
        set_environment_value(environment, "A3S_SEC_MEM_LOW", reservation.to_string());
    }
    if let Some(swap) = resources.memory_swap {
        let swap_only = if swap == -1 {
            -1
        } else {
            swap.checked_sub(resources.memory_limit).ok_or_else(|| {
                BoxError::ConfigError("Sandbox workload swap limit underflows".to_string())
            })?
        };
        set_environment_value(environment, "A3S_SEC_MEM_SWAP", swap_only.to_string());
    }
    if let Some(shares) = resources.cpu_shares {
        set_environment_value(environment, "A3S_SEC_CPU_SHARES", shares.to_string());
    }
    Ok(())
}

fn set_environment_value(environment: &mut Vec<(String, String)>, key: &str, value: String) {
    if let Some((_, current)) = environment.iter_mut().find(|(name, _)| name == key) {
        *current = value;
    } else {
        environment.push((key.to_string(), value));
    }
}

fn parse_sandbox_volume(value: &str) -> Result<SandboxMount> {
    let (without_mode, read_only) = match value.rsplit_once(':') {
        Some((prefix, "ro")) => (prefix, true),
        Some((prefix, "rw")) => (prefix, false),
        _ => (value, false),
    };
    let (source, destination) = without_mode.rsplit_once(':').ok_or_else(|| {
        BoxError::ConfigError(format!(
            "Invalid Sandbox volume {value:?}; expected host:guest[:ro|rw]"
        ))
    })?;
    if source.is_empty() {
        return Err(BoxError::ConfigError(format!(
            "Sandbox volume source is empty: {value:?}"
        )));
    }
    let source = PathBuf::from(source);
    if !source.exists() {
        std::fs::create_dir_all(&source).map_err(BoxError::IoError)?;
    }
    let source = source.canonicalize().map_err(BoxError::IoError)?;
    let destination = normalized_container_path(destination, "volume destination")?;
    Ok(SandboxMount {
        source,
        destination,
        read_only,
    })
}

fn parse_sandbox_tmpfs(value: &str) -> Result<SandboxTmpfs> {
    const DEFAULT_SIZE: u64 = 64 * 1024 * 1024;
    let (destination, options) = value
        .split_once(':')
        .map_or((value, None), |(path, options)| (path, Some(options)));
    let mut size_bytes = DEFAULT_SIZE;
    let mut size_seen = false;
    let mut read_only = None;
    for option in options
        .into_iter()
        .flat_map(|options| options.split(','))
        .filter(|option| !option.is_empty())
    {
        match option {
            "ro" | "rw" => {
                let requested = option == "ro";
                if read_only.replace(requested).is_some() {
                    return Err(BoxError::ConfigError(format!(
                        "Sandbox tmpfs has duplicate or conflicting access modes: {value:?}"
                    )));
                }
            }
            _ if option.starts_with("size=") => {
                if size_seen {
                    return Err(BoxError::ConfigError(format!(
                        "Sandbox tmpfs has duplicate size options: {value:?}"
                    )));
                }
                size_seen = true;
                size_bytes = parse_byte_size(&option["size=".len()..])?;
            }
            _ => {
                return Err(BoxError::ConfigError(format!(
                    "Invalid Sandbox tmpfs option {option:?}; only size=<bytes>, ro, and rw are supported"
                )));
            }
        }
    }
    Ok(SandboxTmpfs {
        destination: normalized_container_path(destination, "tmpfs destination")?,
        size_bytes,
        read_only: read_only.unwrap_or(false),
    })
}

fn parse_byte_size(value: &str) -> Result<u64> {
    let value = value.trim();
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let number = value[..split]
        .parse::<u64>()
        .map_err(|_| BoxError::ConfigError(format!("Invalid Sandbox tmpfs size {value:?}")))?;
    let multiplier = match value[split..].to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        _ => {
            return Err(BoxError::ConfigError(format!(
                "Invalid Sandbox tmpfs size suffix in {value:?}"
            )))
        }
    };
    number
        .checked_mul(multiplier)
        .filter(|size| *size > 0)
        .ok_or_else(|| {
            BoxError::ConfigError(format!(
                "Sandbox tmpfs size overflows or is zero: {value:?}"
            ))
        })
}

fn normalized_container_path(value: &str, label: &str) -> Result<PathBuf> {
    // Container paths always use Linux semantics. Host `Path::is_absolute`
    // rejects `/work` on Windows because it has no drive prefix, even though it
    // is the correct absolute path inside the guest/container.
    if !value.starts_with('/')
        || value
            .split('/')
            .any(|component| matches!(component, "." | ".."))
    {
        return Err(BoxError::ConfigError(format!(
            "Sandbox {label} must be an absolute normalized path: {value:?}"
        )));
    }
    Ok(PathBuf::from(value))
}

fn ensure_mount_destinations(
    rootfs: &Path,
    mounts: &[SandboxMount],
    tmpfs: &[SandboxTmpfs],
) -> Result<()> {
    for mount in mounts {
        ensure_mount_destination(rootfs, &mount.destination, mount.source.is_file())?;
    }
    for mount in tmpfs {
        ensure_mount_destination(rootfs, &mount.destination, false)?;
    }
    Ok(())
}

fn ensure_mount_destination(rootfs: &Path, destination: &Path, file: bool) -> Result<()> {
    let relative = destination.strip_prefix("/").map_err(|_| {
        BoxError::ConfigError(format!(
            "Sandbox mount destination is not absolute: {}",
            destination.display()
        ))
    })?;
    let mut current = rootfs.to_path_buf();
    let components: Vec<_> = relative.components().collect();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(BoxError::ConfigError(format!(
                "Invalid Sandbox mount destination {}",
                destination.display()
            )));
        };
        current.push(name);
        let final_component = index + 1 == components.len();
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(BoxError::ConfigError(format!(
                    "Sandbox mount destination traverses a symlink at {}",
                    current.display()
                )))
            }
            Ok(metadata) if final_component && file && !metadata.is_file() => {
                return Err(BoxError::ConfigError(format!(
                    "Sandbox file mount destination is not a file: {}",
                    current.display()
                )))
            }
            Ok(metadata) if (!final_component || !file) && !metadata.is_dir() => {
                return Err(BoxError::ConfigError(format!(
                    "Sandbox directory mount destination is not a directory: {}",
                    current.display()
                )))
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if final_component && file {
                    std::fs::File::create(&current).map_err(BoxError::IoError)?;
                } else {
                    std::fs::create_dir(&current).map_err(BoxError::IoError)?;
                }
            }
            Err(error) => return Err(BoxError::IoError(error)),
        }
    }
    Ok(())
}

const DEFAULT_CONTAINER_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

#[derive(Debug, Clone)]
struct RuntimePasswdEntry {
    name: String,
    uid: u32,
    gid: u32,
    home: Option<String>,
}

#[derive(Debug, Clone)]
struct RuntimeGroupEntry {
    name: String,
    gid: u32,
    members: Vec<String>,
}

fn resolve_runtime_owned_process(
    rootfs: &Path,
    instance_spec: &crate::vmm::InstanceSpec,
    dropped_capabilities: &[String],
) -> Result<SandboxRuntimeProcess> {
    let cwd = normalize_linux_guest_path("/", &instance_spec.workdir, "working directory")?;
    crate::oci::rootfs::ensure_guest_directory(rootfs, cwd.trim_start_matches('/'))?;

    let mut environment = instance_spec.entrypoint.env.clone();
    let path = environment
        .iter()
        .rev()
        .find_map(|(key, value)| (key == "PATH").then_some(value.as_str()))
        .unwrap_or(DEFAULT_CONTAINER_PATH);
    let executable =
        resolve_runtime_executable(rootfs, &cwd, path, &instance_spec.entrypoint.executable)?;
    let identity = resolve_runtime_identity(rootfs, instance_spec.user.as_deref())?;
    if !environment.iter().any(|(key, _)| key == "HOME") {
        if let Some(home) = identity.home.as_ref() {
            environment.push(("HOME".to_string(), home.clone()));
        }
    }
    let mut args = Vec::with_capacity(instance_spec.entrypoint.args.len() + 1);
    args.push(executable);
    args.extend(instance_spec.entrypoint.args.iter().cloned());

    Ok(SandboxRuntimeProcess {
        args,
        environment,
        cwd: PathBuf::from(cwd),
        uid: identity.uid,
        gid: identity.gid,
        additional_gids: identity.additional_gids,
        dropped_capabilities: dropped_capabilities.to_vec(),
    })
}

fn resolve_runtime_executable(
    rootfs: &Path,
    cwd: &str,
    path: &str,
    command: &str,
) -> Result<String> {
    if command.is_empty() || command.contains('\0') {
        return Err(BoxError::ConfigError(
            "Sandbox runtime-owned executable must be non-empty and contain no NUL".to_string(),
        ));
    }

    if command.contains('/') {
        let candidate = normalize_linux_guest_path(cwd, command, "process executable")?;
        require_runtime_executable(rootfs, &candidate)?;
        return Ok(candidate);
    }

    for directory in path.split(':') {
        let directory = if directory.is_empty() { cwd } else { directory };
        let directory = normalize_linux_guest_path(cwd, directory, "PATH entry")?;
        let candidate =
            normalize_linux_guest_path(&directory, command, "PATH-resolved process executable")?;
        if runtime_executable_exists(rootfs, &candidate)? {
            return Ok(candidate);
        }
    }
    Err(BoxError::BoxBootError {
        message: format!(
            "Sandbox executable {command:?} was not found as an executable file in the prepared rootfs PATH"
        ),
        hint: Some("Use an absolute image ENTRYPOINT or include it in PATH".to_string()),
    })
}

fn require_runtime_executable(rootfs: &Path, guest_path: &str) -> Result<()> {
    if runtime_executable_exists(rootfs, guest_path)? {
        Ok(())
    } else {
        Err(BoxError::BoxBootError {
            message: format!(
                "Sandbox executable {guest_path:?} is missing, not regular, or not executable in the prepared rootfs"
            ),
            hint: None,
        })
    }
}

fn runtime_executable_exists(rootfs: &Path, guest_path: &str) -> Result<bool> {
    let host_path =
        crate::oci::rootfs::resolve_guest_file_path(rootfs, guest_path.trim_start_matches('/'))?;
    let metadata = match std::fs::metadata(&host_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(BoxError::IoError(error)),
    };
    if !metadata.is_file() {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

fn normalize_linux_guest_path(base: &str, value: &str, label: &str) -> Result<String> {
    if value.is_empty() || value.contains('\0') {
        return Err(BoxError::ConfigError(format!(
            "Sandbox {label} must be non-empty and contain no NUL"
        )));
    }
    if !base.starts_with('/') {
        return Err(BoxError::ConfigError(format!(
            "Sandbox {label} base is not absolute: {base:?}"
        )));
    }

    let mut components = if value.starts_with('/') {
        Vec::new()
    } else {
        base.split('/')
            .filter(|component| !component.is_empty())
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    };
    for component in value.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if components.pop().is_none() {
                    return Err(BoxError::ConfigError(format!(
                        "Sandbox {label} escapes the container root: {value:?}"
                    )));
                }
            }
            component => components.push(component.to_string()),
        }
    }
    Ok(format!("/{}", components.join("/")))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeIdentity {
    uid: u32,
    gid: u32,
    additional_gids: Vec<u32>,
    home: Option<String>,
}

fn resolve_runtime_identity(rootfs: &Path, requested: Option<&str>) -> Result<RuntimeIdentity> {
    let passwd = runtime_passwd_entries(rootfs)?;
    let groups = runtime_group_entries(rootfs)?;
    let Some(requested) = requested.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(RuntimeIdentity {
            uid: 0,
            gid: 0,
            additional_gids: Vec::new(),
            home: None,
        });
    };
    if requested.matches(':').count() > 1 {
        return Err(BoxError::ConfigError(format!(
            "Invalid Sandbox user {requested:?}; expected user[:group]"
        )));
    }
    let (user_part, group_part) = requested
        .split_once(':')
        .map_or((requested, None), |(user, group)| (user, Some(group)));
    if user_part.is_empty() || group_part.is_some_and(str::is_empty) {
        return Err(BoxError::ConfigError(format!(
            "Invalid Sandbox user {requested:?}; user and group must be non-empty"
        )));
    }

    let passwd_entry = if user_part == "root" {
        passwd.iter().find(|entry| entry.uid == 0)
    } else if let Ok(uid) = user_part.parse::<u32>() {
        passwd.iter().find(|entry| entry.uid == uid)
    } else {
        Some(
            passwd
                .iter()
                .find(|entry| entry.name == user_part)
                .ok_or_else(|| {
                    BoxError::ConfigError(format!(
                "Sandbox user {user_part:?} is not present in the prepared rootfs /etc/passwd"
            ))
                })?,
        )
    };
    let uid = if user_part == "root" {
        0
    } else {
        user_part
            .parse::<u32>()
            .ok()
            .or_else(|| passwd_entry.map(|entry| entry.uid))
            .ok_or_else(|| {
                BoxError::ConfigError(format!("Sandbox user {user_part:?} cannot be resolved"))
            })?
    };
    let gid = match group_part {
        Some("root") => 0,
        Some(group) => group
            .parse::<u32>()
            .ok()
            .or_else(|| {
                groups
                    .iter()
                    .find(|entry| entry.name == group)
                    .map(|entry| entry.gid)
            })
            .ok_or_else(|| {
                BoxError::ConfigError(format!(
                    "Sandbox group {group:?} is not present in the prepared rootfs /etc/group"
                ))
            })?,
        None => passwd_entry.map(|entry| entry.gid).unwrap_or(0),
    };
    let username = if user_part.parse::<u32>().is_ok() || user_part == "root" {
        passwd_entry.map(|entry| entry.name.as_str())
    } else {
        Some(user_part)
    };
    let mut additional_gids = groups
        .iter()
        .filter(|entry| {
            entry.gid != gid
                && username
                    .is_some_and(|username| entry.members.iter().any(|member| member == username))
        })
        .map(|entry| entry.gid)
        .collect::<Vec<_>>();
    additional_gids.sort_unstable();
    additional_gids.dedup();

    let home = passwd_entry
        .and_then(|entry| entry.home.as_deref())
        .map(|home| normalize_linux_guest_path("/", home, "user home"))
        .transpose()?;
    Ok(RuntimeIdentity {
        uid,
        gid,
        additional_gids,
        home,
    })
}

fn runtime_passwd_entries(rootfs: &Path) -> Result<Vec<RuntimePasswdEntry>> {
    let Some(contents) = crate::oci::rootfs::read_guest_file_to_string(rootfs, "etc/passwd")?
    else {
        return Ok(Vec::new());
    };
    Ok(contents
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let fields = line.split(':').collect::<Vec<_>>();
            if fields.len() < 7 || fields[0].is_empty() {
                return None;
            }
            Some(RuntimePasswdEntry {
                name: fields[0].to_string(),
                uid: fields[2].parse().ok()?,
                gid: fields[3].parse().ok()?,
                home: (!fields[5].is_empty()).then(|| fields[5].to_string()),
            })
        })
        .collect())
}

fn runtime_group_entries(rootfs: &Path) -> Result<Vec<RuntimeGroupEntry>> {
    let Some(contents) = crate::oci::rootfs::read_guest_file_to_string(rootfs, "etc/group")? else {
        return Ok(Vec::new());
    };
    Ok(contents
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let fields = line.split(':').collect::<Vec<_>>();
            if fields.len() < 4 || fields[0].is_empty() {
                return None;
            }
            Some(RuntimeGroupEntry {
                name: fields[0].to_string(),
                gid: fields[2].parse().ok()?,
                members: fields[3]
                    .split(',')
                    .map(str::trim)
                    .filter(|member| !member.is_empty())
                    .map(ToString::to_string)
                    .collect(),
            })
        })
        .collect())
}

fn maximum_account_ids(rootfs: &Path) -> Result<(u32, u32)> {
    let mut maximum_uid = 0u32;
    let mut maximum_gid = 0u32;
    if let Some(passwd) = crate::oci::rootfs::read_guest_file_to_string(rootfs, "etc/passwd")? {
        for line in passwd.lines().filter(|line| !line.starts_with('#')) {
            let fields: Vec<_> = line.split(':').collect();
            if fields.len() >= 4 {
                if let Ok(uid) = fields[2].parse::<u32>() {
                    maximum_uid = maximum_uid.max(uid);
                }
                if let Ok(gid) = fields[3].parse::<u32>() {
                    maximum_gid = maximum_gid.max(gid);
                }
            }
        }
    }
    if let Some(group) = crate::oci::rootfs::read_guest_file_to_string(rootfs, "etc/group")? {
        for line in group.lines().filter(|line| !line.starts_with('#')) {
            if let Some(Ok(gid)) = line.split(':').nth(2).map(str::parse::<u32>) {
                maximum_gid = maximum_gid.max(gid);
            }
        }
    }
    Ok((maximum_uid, maximum_gid))
}

fn maximum_process_ids(rootfs: &Path, environment: &[(String, String)]) -> Result<(u32, u32)> {
    let user = if let Some(path) = environment
        .iter()
        .find_map(|(key, value)| (key == "BOX_EXEC_CONFIG_FILE").then_some(value.as_str()))
    {
        if path != RUNTIME_EXEC_CONFIG_PATH {
            return Err(BoxError::ConfigError(format!(
                "Unsupported Sandbox guest exec config path {path:?}"
            )));
        }
        let host_path = rootfs.join(RUNTIME_EXEC_CONFIG_PATH.trim_start_matches('/'));
        let metadata = std::fs::symlink_metadata(&host_path).map_err(BoxError::IoError)?;
        if !metadata.file_type().is_file() {
            return Err(BoxError::ConfigError(format!(
                "Sandbox guest exec config is not a regular file: {}",
                host_path.display()
            )));
        }
        if metadata.len() > MAX_RUNTIME_EXEC_CONFIG_BYTES as u64 {
            return Err(BoxError::ConfigError(format!(
                "Sandbox guest exec config is {} bytes; limit is {} bytes",
                metadata.len(),
                MAX_RUNTIME_EXEC_CONFIG_BYTES
            )));
        }
        let bytes = std::fs::read(&host_path).map_err(BoxError::IoError)?;
        if bytes.len() > MAX_RUNTIME_EXEC_CONFIG_BYTES {
            return Err(BoxError::ConfigError(format!(
                "Sandbox guest exec config grew to {} bytes; limit is {} bytes",
                bytes.len(),
                MAX_RUNTIME_EXEC_CONFIG_BYTES
            )));
        }
        let config: GuestExecConfig = serde_json::from_slice(&bytes).map_err(|error| {
            BoxError::ConfigError(format!("Invalid Sandbox guest exec config: {error}"))
        })?;
        config.validate().map_err(|error| {
            BoxError::ConfigError(format!("Invalid Sandbox guest exec config: {error}"))
        })?;
        config.user
    } else if let Some(encoded) = environment
        .iter()
        .find_map(|(key, value)| (key == "BOX_EXEC_USER").then_some(value))
    {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|error| {
                BoxError::ConfigError(format!("Invalid encoded Sandbox user: {error}"))
            })?;
        Some(String::from_utf8(bytes).map_err(|error| {
            BoxError::ConfigError(format!("Sandbox user is not UTF-8: {error}"))
        })?)
    } else {
        None
    };
    let Some(user) = user.as_deref() else {
        return Ok((0, 0));
    };
    let mut parts = user.split(':');
    let parse_numeric = |value: &str| -> Result<u32> {
        if value == "root" {
            Ok(0)
        } else {
            value.parse::<u32>().map_err(|_| {
                BoxError::ConfigError(format!(
                    "Sandbox group in {user:?} must be numeric before OCI launch"
                ))
            })
        }
    };
    let user_part = parts.next().unwrap_or_default();
    // Named users are resolved by guest-init from /etc/passwd. All passwd and
    // group IDs were already included by maximum_account_ids above.
    let uid = if user_part == "root" {
        0
    } else {
        user_part.parse::<u32>().unwrap_or(0)
    };
    let gid = parts.next().map(parse_numeric).transpose()?.unwrap_or(0);
    if parts.next().is_some() {
        return Err(BoxError::ConfigError(format!(
            "Invalid Sandbox user {user:?}"
        )));
    }
    Ok((uid, gid))
}

fn digest_json(value: &impl serde::Serialize) -> Result<String> {
    let bytes = serde_json::to_vec(value).map_err(|error| {
        BoxError::SerializationError(format!("Failed to encode execution plan: {error}"))
    })?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
}

fn combined_runtime_digest(runtime_sha256: &str, agent_sha256: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(runtime_sha256.as_bytes());
    digest.update([0]);
    digest.update(agent_sha256.as_bytes());
    format!("sha256:{}", hex::encode(digest.finalize()))
}

#[cfg(test)]
mod tests {
    use a3s_box_core::{volume::VolumeConfig, BoxConfig, EventEmitter};

    use super::*;

    #[test]
    fn runtime_owned_process_resolves_path_and_named_identity_from_rootfs() {
        let rootfs = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(rootfs.path().join("usr/local/bin")).unwrap();
        std::fs::create_dir_all(rootfs.path().join("etc")).unwrap();
        let executable = rootfs.path().join("usr/local/bin/example");
        std::fs::write(&executable, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(
            rootfs.path().join("etc/passwd"),
            "root:x:0:0:root:/root:/bin/sh\napp:x:1001:1002:App:/home/app:/bin/sh\n",
        )
        .unwrap();
        std::fs::write(
            rootfs.path().join("etc/group"),
            "app:x:1002:\nmetrics:x:1003:app\n",
        )
        .unwrap();

        assert_eq!(
            resolve_runtime_executable(
                rootfs.path(),
                "/workspace",
                "/usr/local/bin:/bin",
                "example"
            )
            .unwrap(),
            "/usr/local/bin/example"
        );
        assert_eq!(
            resolve_runtime_identity(rootfs.path(), Some("app")).unwrap(),
            RuntimeIdentity {
                uid: 1001,
                gid: 1002,
                additional_gids: vec![1003],
                home: Some("/home/app".to_string()),
            }
        );
    }

    #[test]
    fn runtime_owned_process_rejects_root_escape_and_unknown_named_user() {
        let rootfs = tempfile::tempdir().unwrap();
        assert!(normalize_linux_guest_path("/", "../../host", "test").is_err());
        assert!(resolve_runtime_identity(rootfs.path(), Some("missing")).is_err());
    }

    #[test]
    fn parses_volume_and_tmpfs_without_shell_interpretation() {
        let directory = tempfile::tempdir().unwrap();
        let value = format!("{}:/work:ro", directory.path().display());
        let mount = parse_sandbox_volume(&value).unwrap();
        assert_eq!(mount.destination, Path::new("/work"));
        assert!(mount.read_only);

        let tmpfs = parse_sandbox_tmpfs("/scratch:size=128m").unwrap();
        assert_eq!(tmpfs.size_bytes, 128 * 1024 * 1024);
        assert!(!tmpfs.read_only);

        let read_only = parse_sandbox_tmpfs("/sealed:size=4m,ro").unwrap();
        assert_eq!(read_only.size_bytes, 4 * 1024 * 1024);
        assert!(read_only.read_only);

        assert!(parse_sandbox_tmpfs("/scratch:size=1m,ro,rw").is_err());
        assert!(parse_sandbox_tmpfs("/scratch:exec").is_err());
        assert!(normalized_container_path("relative", "test path").is_err());
        assert!(normalized_container_path("/work/../escape", "test path").is_err());
    }

    #[test]
    fn staged_exec_config_contributes_sandbox_process_ids() {
        let rootfs = tempfile::tempdir().unwrap();
        let config = GuestExecConfig::new(
            "/bin/true".to_string(),
            vec![],
            "/".to_string(),
            Some("1234:5678".to_string()),
            false,
        );
        std::fs::write(
            rootfs.path().join(".a3s-box-exec.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let environment = vec![(
            "BOX_EXEC_CONFIG_FILE".to_string(),
            RUNTIME_EXEC_CONFIG_PATH.to_string(),
        )];

        assert_eq!(
            maximum_process_ids(rootfs.path(), &environment).unwrap(),
            (1234, 5678)
        );
    }

    #[test]
    fn sandbox_workload_environment_keeps_exact_limits_in_one_cgroup() {
        let mut environment = vec![("A3S_SEC_CPU_QUOTA".to_string(), "stale".to_string())];
        let resources = SandboxResources {
            memory_limit: 128 * 1024 * 1024,
            memory_reservation: Some(64 * 1024 * 1024),
            memory_swap: Some(256 * 1024 * 1024),
            cpu_shares: Some(1024),
            cpu_quota: 10_000,
            cpu_period: 100_000,
            cpuset_cpus: None,
            pids_limit: 32,
        };

        apply_sandbox_workload_resources(&mut environment, &resources).unwrap();

        let value = |key: &str| {
            environment
                .iter()
                .find_map(|(name, value)| (name == key).then_some(value.as_str()))
        };
        assert_eq!(value("A3S_SEC_MEM_LIMIT"), Some("134217728"));
        assert_eq!(value("A3S_SEC_MEM_LOW"), Some("67108864"));
        assert_eq!(value("A3S_SEC_MEM_SWAP"), Some("134217728"));
        assert_eq!(value("A3S_SEC_CPU_QUOTA"), Some("10000"));
        assert_eq!(value("A3S_SEC_CPU_PERIOD"), Some("100000"));
        assert_eq!(value("A3S_SEC_CPU_SHARES"), Some("1024"));
        assert_eq!(value("A3S_SEC_PIDS_LIMIT"), Some("32"));
        assert_eq!(
            environment
                .iter()
                .filter(|(name, _)| name == "A3S_SEC_CPU_QUOTA")
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn mount_destination_rejects_symlink_parent() {
        let rootfs = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/", rootfs.path().join("escape")).unwrap();
        let error =
            ensure_mount_destination(rootfs.path(), Path::new("/escape/host"), false).unwrap_err();
        assert!(error.to_string().contains("symlink"));
    }

    #[test]
    fn named_volume_mounts_are_classified_as_a3s_managed() {
        let home = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let store = crate::volume::VolumeStore::new(
            home.path().join("volumes.json"),
            home.path().join("volumes"),
        );
        let volume = store.create(VolumeConfig::new("sandbox-data", "")).unwrap();
        let named_source = PathBuf::from(volume.mount_point).canonicalize().unwrap();
        let external_source = external.path().canonicalize().unwrap();
        let mounts = vec![
            SandboxMount {
                source: named_source.clone(),
                destination: PathBuf::from("/data"),
                read_only: false,
            },
            SandboxMount {
                source: external_source.clone(),
                destination: PathBuf::from("/external"),
                read_only: false,
            },
        ];
        let mut manager = VmManager::with_box_id(
            BoxConfig::default(),
            EventEmitter::new(16),
            "sandbox-managed-volume-test".to_string(),
        );
        manager.home_dir = home.path().to_path_buf();
        let workspace = home.path().join("boxes/test/workspace");

        let managed = manager
            .managed_sandbox_mount_sources(&workspace, &mounts)
            .unwrap();

        assert!(managed.contains(&workspace));
        assert!(managed.contains(&named_source));
        assert!(!managed.contains(&external_source));
    }
}
