//! Instance spec building — entrypoint resolution, volume mounts, OCI config.

use std::path::{Path, PathBuf};

use crate::oci::OciImageConfig;
use crate::rootfs::GUEST_WORKDIR;
use crate::vmm::{Entrypoint, FsMount, InstanceSpec, RootfsSource};
use a3s_box_core::config::{validate_vcpu_count, TeeConfig};
use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::guest_exec::{
    GuestBootConfig, GuestExecConfig, GuestHostConfig, GUEST_BOOT_CONFIG_ENV,
    GUEST_BOOT_CONFIG_FILE_NAME, GUEST_BOOT_CONFIG_PATH, GUEST_BOOT_CONTROL_TAG,
    GUEST_TERMINAL_CONTROL_TAG, GUEST_TERMINAL_STATUS_FILE_NAME, MAX_GUEST_BOOT_CONFIG_BYTES,
    MAX_RUNTIME_EXEC_CONFIG_BYTES, RUNTIME_EXEC_CONFIG_PATH,
};
use a3s_box_core::rootfs_metadata::RUNTIME_ENV_PATH;

use super::{fnv1a_hash, BoxLayout, VmManager};

const SBIN_INIT: &str = "/sbin/init";
const USR_SBIN_INIT: &str = "/usr/sbin/init";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuestControlTransport {
    RootfsFiles,
    VirtioFsBootBundle,
}

#[derive(Debug)]
pub(crate) struct ParsedVolumeMount {
    pub(crate) host_path: PathBuf,
    pub(crate) guest_path: String,
    pub(crate) read_only: bool,
    pub(crate) copy_up: bool,
}

/// Read an environment variable, returning `None` if unset or empty.
fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

pub(crate) mod guest_control;

use guest_control::{
    secure_guest_control_file, stage_guest_boot_config, stage_guest_terminal_control,
    write_guest_boot_config,
};

impl VmManager {
    /// Build an InstanceSpec using the legacy rootfs-file bootstrap transport.
    ///
    /// Sandbox uses this path because its OCI owner mounts a directory root and
    /// guest-init starts after the runtime has installed all mounts.
    pub(crate) fn build_instance_spec(&mut self, layout: &BoxLayout) -> Result<InstanceSpec> {
        self.build_instance_spec_with_bootstrap(layout, true, GuestControlTransport::RootfsFiles)
    }

    /// Build a MicroVM spec whose launch data is carried by a private read-only
    /// virtio-fs share instead of mutating the guest rootfs.
    pub(crate) fn build_microvm_instance_spec(
        &mut self,
        layout: &BoxLayout,
    ) -> Result<InstanceSpec> {
        self.build_instance_spec_with_bootstrap(
            layout,
            true,
            GuestControlTransport::VirtioFsBootBundle,
        )
    }

    /// Build the image process and filesystem view for an OCI runtime that owns
    /// PID 1 and all process I/O itself. Unlike the VM bootstrap path, this must
    /// not select the packaged guest init or inject its host-control variables.
    pub(crate) fn build_runtime_owned_instance_spec(
        &mut self,
        layout: &BoxLayout,
    ) -> Result<InstanceSpec> {
        self.build_instance_spec_with_bootstrap(layout, false, GuestControlTransport::RootfsFiles)
    }

    fn build_instance_spec_with_bootstrap(
        &mut self,
        layout: &BoxLayout,
        include_guest_controls: bool,
        guest_control_transport: GuestControlTransport,
    ) -> Result<InstanceSpec> {
        // Build filesystem mounts
        let mut fs_mounts = vec![FsMount {
            tag: "workspace".to_string(),
            host_path: layout.workspace_path.clone(),
            read_only: false,
        }];

        // Add user-specified volume mounts (-v host:guest or -v host:guest:ro).
        // Single-file binds are staged under this per-box dir (cleaned with the
        // box) since virtio-fs can only share directories — see prepare_volume_mount.
        let filemounts_dir = self
            .home_dir
            .join("boxes")
            .join(&self.box_id)
            .join(".filemounts");
        let named_volume_paths: std::collections::HashSet<PathBuf> =
            crate::volume::VolumeStore::new(
                self.home_dir.join("volumes.json"),
                self.home_dir.join("volumes"),
            )
            .load()?
            .into_values()
            .map(|volume| PathBuf::from(volume.mount_point))
            .collect();
        let parsed_volumes = self
            .config
            .volumes
            .iter()
            .map(|volume| {
                let mut parsed = Self::parse_volume_spec(volume)?;
                parsed.copy_up = named_volume_paths.contains(&parsed.host_path);
                Ok(parsed)
            })
            .collect::<Result<Vec<_>>>()?;
        for (i, volume) in parsed_volumes.iter().enumerate() {
            let mount = Self::prepare_volume_mount(
                volume,
                i,
                &filemounts_dir,
                self.managed_secret_root.as_deref(),
                &self.box_id,
            )?;
            fs_mounts.push(mount);
        }

        // Auto-create anonymous volumes for OCI VOLUME directives
        let user_guest_paths: std::collections::HashSet<String> = parsed_volumes
            .iter()
            .map(|volume| volume.guest_path.clone())
            .collect();
        let mut anon_vol_offset = self.config.volumes.len();
        let mut seen_anonymous_volumes = std::collections::HashSet::new();
        self.anonymous_volumes
            .retain(|name| seen_anonymous_volumes.insert(name.clone()));

        if let Some(ref oci_config) = layout.oci_config {
            for vol_path in &oci_config.volumes {
                Self::validate_guest_mount_path(vol_path)?;
                // Skip if the user already mounted something at this path
                if user_guest_paths.contains(vol_path) {
                    tracing::debug!(
                        path = vol_path,
                        "Skipping anonymous volume — user volume already covers this path"
                    );
                    continue;
                }

                // Generate a deterministic anonymous volume name
                let path_hash = &format!("{:x}", fnv1a_hash(vol_path))[..8];
                let short_box_id = &self.box_id[..8.min(self.box_id.len())];
                let anon_name = format!("anon_{}_{}", short_box_id, path_hash);

                // Create the volume via VolumeStore (best-effort)
                match self.create_anonymous_volume(&anon_name) {
                    Ok((host_path, created)) => {
                        let tag = format!("vol{}", anon_vol_offset);
                        fs_mounts.push(FsMount {
                            tag: tag.clone(),
                            host_path: PathBuf::from(&host_path),
                            read_only: false,
                        });
                        if seen_anonymous_volumes.insert(anon_name.clone()) {
                            self.anonymous_volumes.push(anon_name.clone());
                        }
                        if created {
                            self.created_anonymous_volumes.push(anon_name);
                        }
                        anon_vol_offset += 1;
                        tracing::info!(
                            volume = %tag,
                            guest_path = vol_path,
                            host_path = %host_path,
                            "Created anonymous volume for OCI VOLUME directive"
                        );
                    }
                    Err(e) => {
                        if self.config.isolation.is_sandbox() {
                            return Err(BoxError::BoxBootError {
                                message: format!(
                                    "Failed to create required Sandbox anonymous volume for {vol_path}: {e}"
                                ),
                                hint: None,
                            });
                        }
                        tracing::warn!(
                            path = vol_path,
                            error = %e,
                            "Failed to create anonymous volume, skipping"
                        );
                    }
                }
            }
        }

        // Determine whether guest init is installed (it becomes PID 1 and
        // launches the container entrypoint from runtime control data).
        let guest_init_exec = if include_guest_controls {
            layout
                .resumed_rootfs
                .as_ref()
                .map(|rootfs| rootfs.guest_init_exec.as_str())
                .or_else(|| Self::guest_init_exec_path(&layout.rootfs_path))
        } else {
            None
        };
        // When guest init is PID 1 it applies the staged container user to the
        // main process itself; the shim must then NOT call libkrun set_uid
        // (which would drop PID 1 and break init). Only the legacy
        // no-guest-init path falls back to the shim's set_uid.
        let has_guest_init = guest_init_exec.is_some();
        let workdir = Self::effective_workdir(&self.config, layout.oci_config.as_ref());
        let user = Self::effective_user(&self.config, layout.oci_config.as_ref());

        // Build entrypoint
        let mut entrypoint = if let Some(guest_init_exec) = guest_init_exec {
            // Guest init is PID 1. Pass fixed control pointers inline and stage
            // user-controlled process and environment data in the rootfs.
            let (exec, args, mut container_env) = match &layout.oci_config {
                Some(oci_config) => {
                    let (exec, args) = Self::resolve_oci_entrypoint(
                        oci_config,
                        &self.config.cmd,
                        self.config.entrypoint_override.as_deref(),
                    );
                    (exec, args, oci_config.env.clone())
                }
                None => {
                    let (exec, args) = Self::resolve_config_entrypoint(
                        &self.config.cmd,
                        self.config.entrypoint_override.as_deref(),
                    );
                    (exec, args, vec![])
                }
            };
            a3s_box_core::env::merge_env_pairs(&mut container_env, &self.config.extra_env);

            // Keep user-controlled exec/argv strings off libkrun's kernel command
            // line. Linux truncates that command line at COMMAND_LINE_SIZE, which
            // made sufficiently long but valid commands silently fail during boot.
            let exec_config = GuestExecConfig::new(
                exec,
                args,
                workdir.clone(),
                user.clone(),
                !self.config.stdin_open,
            );
            exec_config
                .validate()
                .map_err(|message| BoxError::BoxBootError {
                    message,
                    hint: Some("shorten the command or correct its process settings".to_string()),
                })?;
            let exec_config_bytes =
                serde_json::to_vec(&exec_config).map_err(|error| BoxError::BoxBootError {
                    message: format!("failed to serialize guest exec configuration: {error}"),
                    hint: None,
                })?;
            if exec_config_bytes.len() > MAX_RUNTIME_EXEC_CONFIG_BYTES {
                return Err(BoxError::BoxBootError {
                    message: format!(
                        "guest exec configuration is {} bytes; limit is {} bytes",
                        exec_config_bytes.len(),
                        MAX_RUNTIME_EXEC_CONFIG_BYTES
                    ),
                    hint: Some("shorten the command arguments".to_string()),
                });
            }

            let mut env: Vec<(String, String)> = match guest_control_transport {
                GuestControlTransport::RootfsFiles => {
                    if layout.resumed_rootfs.is_some() {
                        return Err(BoxError::BoxBootError {
                            message: "guest-owned rootfs cannot use the legacy rootfs-file control transport"
                                .to_string(),
                            hint: None,
                        });
                    }
                    let exec_config_host_path = crate::oci::rootfs::replace_guest_file_no_follow(
                        &layout.rootfs_path,
                        RUNTIME_EXEC_CONFIG_PATH.trim_start_matches('/'),
                        exec_config_bytes,
                    )?;
                    secure_guest_control_file(&exec_config_host_path)?;

                    // Sandbox retains the legacy split files because the OCI
                    // runtime owns its directory root and mounts before PID 1.
                    use base64::Engine;
                    let b64 = |value: &str| {
                        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.as_bytes())
                    };
                    let env_file_body: String = container_env
                        .iter()
                        .map(|(key, value)| format!("{}={}\n", key, b64(value)))
                        .collect();
                    let mut env = vec![
                        ("BOX_EXEC_B64".to_string(), "1".to_string()),
                        (
                            "BOX_EXEC_CONFIG_FILE".to_string(),
                            RUNTIME_EXEC_CONFIG_PATH.to_string(),
                        ),
                    ];
                    if !env_file_body.is_empty() {
                        let host_path = crate::oci::rootfs::write_guest_file(
                            &layout.rootfs_path,
                            RUNTIME_ENV_PATH.trim_start_matches('/'),
                            env_file_body,
                        )?;
                        secure_guest_control_file(&host_path)?;
                        env.push((
                            "BOX_EXEC_ENV_FILE".to_string(),
                            RUNTIME_ENV_PATH.to_string(),
                        ));
                    }
                    env
                }
                GuestControlTransport::VirtioFsBootBundle => {
                    let boot_config = GuestBootConfig::new(
                        exec_config,
                        container_env,
                        GuestHostConfig::default(),
                    );
                    fs_mounts.push(stage_guest_boot_config(layout, &boot_config)?);
                    let box_dir = self.home_dir.join("boxes").join(&self.box_id);
                    let capture_diff_baseline = self.rootfs_provider.guest_owns_diff_baseline()
                        && crate::rootfs::guest_diff_baseline_required(&box_dir)?;
                    fs_mounts.push(stage_guest_terminal_control(
                        &box_dir,
                        capture_diff_baseline,
                    )?);
                    vec![(
                        GUEST_BOOT_CONFIG_ENV.to_string(),
                        GUEST_BOOT_CONFIG_PATH.to_string(),
                    )]
                }
            };

            // Prototype: deferred-main-spawn. If the host set BOX_DEFERRED_MAIN=1,
            // tell guest init to boot IDLE; the runtime then sends a spawn-main
            // control frame post-readiness to run the command above as the main.
            if self.config.deferred_main
                || std::env::var("BOX_DEFERRED_MAIN")
                    .map(|v| v == "1")
                    .unwrap_or(false)
            {
                env.push(("BOX_DEFERRED_MAIN".to_string(), "1".to_string()));
            }

            if let Some(cache_mode) = self
                .config
                .virtiofs_cache
                .clone()
                .or_else(|| env_nonempty("A3S_VIRTIOFS_CACHE"))
            {
                env.push(("A3S_VIRTIOFS_CACHE".to_string(), cache_mode));
            }

            // Pass user volume mounts to guest init for mounting inside the VM.
            // Format: BOX_VOL_<index>=<tag>:<guest_path>[:ro]
            for (i, volume) in parsed_volumes.iter().enumerate() {
                let mode = if volume.read_only { ":ro" } else { "" };
                // Mark single-file bind mounts so the guest binds the file onto
                // guest_path instead of mounting the virtio-fs share over its
                // parent directory (which would clobber e.g. /etc). The host is
                // authoritative here (it can stat the path); the guest must not
                // re-guess from the guest path's shape.
                let file_flag = if volume.host_path.is_file() {
                    ":file"
                } else {
                    ""
                };
                let copy_flag = if volume.copy_up { ":copy" } else { "" };
                env.push((
                    format!("BOX_VOL_{}", i),
                    format!(
                        "vol{}:{}{}{}{}",
                        i, volume.guest_path, mode, file_flag, copy_flag
                    ),
                ));
            }

            // Pass anonymous volume mounts (from OCI VOLUME directives) to guest init
            if let Some(ref oci_config) = layout.oci_config {
                let mut anon_idx = self.config.volumes.len();
                for vol_path in &oci_config.volumes {
                    if user_guest_paths.contains(vol_path) {
                        continue;
                    }
                    env.push((
                        format!("BOX_VOL_{}", anon_idx),
                        format!("vol{}:{}:copy", anon_idx, vol_path),
                    ));
                    anon_idx += 1;
                }
            }

            // Pass tmpfs mounts to guest init.
            // Format: BOX_TMPFS_<index>=<path>[:<options>]
            for (i, tmpfs_spec) in self.config.tmpfs.iter().enumerate() {
                let guest_path = tmpfs_spec
                    .split_once(':')
                    .map_or(tmpfs_spec.as_str(), |(path, _)| path);
                Self::validate_guest_mount_path(guest_path)?;
                env.push((format!("BOX_TMPFS_{}", i), tmpfs_spec.clone()));
            }

            // Pass pod sysctls to guest init.
            // Format: BOX_SYSCTL_<index>=<name>=<value>
            for (i, (name, value)) in self.config.sysctls.iter().enumerate() {
                env.push((format!("BOX_SYSCTL_{}", i), format!("{}={}", name, value)));
            }

            // Pass security configuration to guest init
            let security_config = a3s_box_core::SecurityConfig::from_options(
                &self.config.security_opt,
                &self.config.cap_add,
                &self.config.cap_drop,
                self.config.privileged,
            );
            env.extend(security_config.to_env_vars());

            // Process-count cap (`--pids-limit`). Unlike `--memory`/`--cpus`
            // (enforced by sizing the microVM itself), a pids cap has no
            // VM-boundary equivalent, so guest-init enforces it via an in-guest
            // cgroup `pids.max`; it reads this env in PID 1 before the container
            // fork.
            if let Some(pids_limit) = self.config.resource_limits.pids_limit {
                env.push(("A3S_SEC_PIDS_LIMIT".to_string(), pids_limit.to_string()));
            }

            // CPU cgroup limits (`--cpu-quota`/`--cpu-period`/`--cpu-shares`).
            // Like the pids cap these have no VM-boundary equivalent, so the
            // guest enforces them with a per-container cgroup v2 cpu.max /
            // cpu.weight. The CRI path already plumbs the identical A3S_SEC_CPU_*
            // vars (runtime_service mod.rs) and the guest consumes them in
            // exec_server; mirror it here so a `run --cpu-quota ...` is actually
            // capped instead of silently dropped. A quota of 0/-1 is unlimited.
            if let Some(cpu_quota) = self.config.resource_limits.cpu_quota {
                if cpu_quota > 0 {
                    env.push(("A3S_SEC_CPU_QUOTA".to_string(), cpu_quota.to_string()));
                    if let Some(cpu_period) = self.config.resource_limits.cpu_period {
                        if cpu_period > 0 {
                            env.push(("A3S_SEC_CPU_PERIOD".to_string(), cpu_period.to_string()));
                        }
                    }
                }
            }
            if let Some(cpu_shares) = self.config.resource_limits.cpu_shares {
                if cpu_shares > 0 {
                    env.push(("A3S_SEC_CPU_SHARES".to_string(), cpu_shares.to_string()));
                }
            }

            // Memory soft-reservation (--memory-reservation → memory.low) and
            // swap cap (--memory-swap → memory.swap.max). Like the CPU caps these
            // are enforced by the in-guest per-container cgroup (the broken
            // host-side path was removed); the hard --memory limit stays
            // VM-sized, so no A3S_SEC_MEM_LIMIT is emitted here.
            if let Some(reservation) = self.config.resource_limits.memory_reservation {
                if reservation > 0 {
                    env.push(("A3S_SEC_MEM_LOW".to_string(), reservation.to_string()));
                }
            }
            if let Some(swap) = self.config.resource_limits.memory_swap {
                env.push(("A3S_SEC_MEM_SWAP".to_string(), swap.to_string()));
            }

            // Signal guest init to remount rootfs read-only after all setup
            if self.config.read_only {
                env.push(("BOX_READONLY".to_string(), "1".to_string()));
            }

            if guest_control_transport == GuestControlTransport::RootfsFiles {
                if let Some(hostname) = self.config.hostname.as_ref() {
                    env.push(("BOX_HOSTNAME".to_string(), hostname.clone()));
                }
            }

            #[cfg(target_os = "windows")]
            env.push(("KRUN_INIT_PID1".to_string(), "1".to_string()));

            // Log only the count, never values. Runtime controls can include
            // user-supplied hostname and sidecar settings, while staged container
            // environment may contain Kubernetes secretKeyRef/envFrom values.
            // The no-guest-init branch logs only a count for the same reason.
            tracing::debug!(env_count = env.len(), "Using guest init as PID 1");

            Entrypoint {
                executable: guest_init_exec.to_string(),
                args: vec![],
                env,
            }
        } else {
            // No guest init — exec the container entrypoint directly as PID 1
            match &layout.oci_config {
                Some(oci_config) => {
                    let (executable, args) = Self::resolve_oci_entrypoint(
                        oci_config,
                        &self.config.cmd,
                        self.config.entrypoint_override.as_deref(),
                    );
                    let mut env = oci_config.env.clone();
                    a3s_box_core::env::merge_env_pairs(&mut env, &self.config.extra_env);

                    tracing::debug!(
                        executable = %executable,
                        args = ?args,
                        env_count = env.len(),
                        workdir = ?oci_config.working_dir,
                        "Using OCI image entrypoint directly"
                    );

                    Entrypoint {
                        executable,
                        args,
                        env,
                    }
                }
                None => {
                    let (executable, args) = Self::resolve_config_entrypoint(
                        &self.config.cmd,
                        self.config.entrypoint_override.as_deref(),
                    );
                    Entrypoint {
                        executable,
                        args,
                        env: self.config.extra_env.clone(),
                    }
                }
            }
        };

        // Inject TEE simulation env var when simulate mode is enabled
        if matches!(self.config.tee, TeeConfig::SevSnp { simulate: true, .. })
            || matches!(self.config.tee, TeeConfig::Tdx { simulate: true, .. })
        {
            entrypoint
                .env
                .push(("A3S_TEE_SIMULATE".to_string(), "1".to_string()));
        }

        if include_guest_controls {
            #[cfg(target_os = "windows")]
            {
                // WHPX named-pipe mappings are guest-initiated. Keep the shared
                // Windows host-control channel connected even without published
                // ports so stop requests can reach guest init.
                entrypoint
                    .env
                    .push(("BOX_WINDOWS_PORT_FWD".to_string(), "1".to_string()));
            }

            #[cfg(not(target_os = "windows"))]
            entrypoint
                .env
                .push(("BOX_CRI_PORT_FWD".to_string(), "1".to_string()));

            if self.config.persistent {
                entrypoint
                    .env
                    .push(("BOX_PERSIST_ROOTFS_METADATA".to_string(), "1".to_string()));
            }

            // Inject sidecar configuration so guest-init can launch the sidecar process.
            if let Some(ref sidecar) = self.config.sidecar {
                entrypoint
                    .env
                    .push(("BOX_SIDECAR_IMAGE".to_string(), sidecar.image.clone()));
                entrypoint.env.push((
                    "BOX_SIDECAR_VSOCK_PORT".to_string(),
                    sidecar.vsock_port.to_string(),
                ));
                for (i, (key, value)) in sidecar.env.iter().enumerate() {
                    entrypoint.env.push((
                        format!("BOX_SIDECAR_ENV_{}", i),
                        format!("{}={}", key, value),
                    ));
                }
                entrypoint.env.push((
                    "BOX_SIDECAR_ENV_COUNT".to_string(),
                    sidecar.env.len().to_string(),
                ));
            }
        }

        // The CLI validates this up front; this also guards compose, CRI, SDK,
        // and direct runtime callers against unsupported platform sizing.
        validate_vcpu_count(self.config.resources.vcpus).map_err(BoxError::ConfigError)?;
        let vcpus = u8::try_from(self.config.resources.vcpus).map_err(|_| {
            BoxError::ConfigError(format!(
                "vcpus {} exceeds the maximum of 255",
                self.config.resources.vcpus
            ))
        })?;
        Ok(InstanceSpec {
            box_id: self.box_id.clone(),
            vcpus,
            memory_mib: self.config.resources.memory_mb,
            // Fresh generations start with the staging directory and are
            // replaced at the final provider boundary. Persistent guest-owned
            // generations arrive already finalized and never regain a host view.
            rootfs: layout
                .resumed_rootfs
                .as_ref()
                .map(|rootfs| rootfs.source.clone())
                .unwrap_or_else(|| RootfsSource::directory(layout.rootfs_path.clone())),
            block_devices: Vec::new(),
            exec_socket_path: layout.exec_socket_path.clone(),
            pty_socket_path: layout.pty_socket_path.clone(),
            attest_socket_path: layout.attest_socket_path.clone(),
            port_forward_socket_path: layout.port_forward_socket_path.clone(),
            fs_mounts,
            entrypoint,
            console_output: layout.console_output.clone(),
            workdir,
            tee_config: layout.tee_instance_config.clone(),
            port_map: self.config.port_map.clone(),
            // Guest init applies the staged user to the main process; only the
            // legacy no-guest-init path uses the shim's set_uid.
            user: if has_guest_init { None } else { user },
            network: None, // Network config is set by CLI when --network is specified
            disable_tsi: matches!(&self.config.network, a3s_box_core::NetworkMode::None),
            resource_limits: self.config.resource_limits.clone(),
            log_config: self.log_config.clone(),
            // KSM page-merging: config field, or the A3S_BOX_KSM env override.
            ksm: self.config.ksm
                || std::env::var("A3S_BOX_KSM")
                    .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
                    .unwrap_or(false),
            // Snapshot-fork (per-VM): config field, or the env override (single-VM
            // `run`). The pool / fork daemon set these per-VM via config so one
            // process can drive a different template/restore per VM.
            snapshot_mem_file: self
                .config
                .snapshot_mem_file
                .clone()
                .or_else(|| env_nonempty("KRUN_SNAPSHOT_MEM_FILE")),
            snapshot_sock: self
                .config
                .snapshot_sock
                .clone()
                .or_else(|| env_nonempty("KRUN_SNAPSHOT_SOCK")),
            restore_from: self
                .config
                .restore_from
                .clone()
                .or_else(|| env_nonempty("KRUN_RESTORE_FROM")),
        })
    }

    /// Fill the host-controlled portion of a staged MicroVM boot bundle.
    ///
    /// Returns `false` for legacy images that have no packaged guest-init and
    /// therefore no boot share. Callers can retain their directory-root fallback
    /// for that compatibility case.
    pub(crate) fn finalize_microvm_guest_boot_config(
        spec: &InstanceSpec,
        host: GuestHostConfig,
    ) -> Result<bool> {
        let mut mounts = spec
            .fs_mounts
            .iter()
            .filter(|mount| mount.tag == GUEST_BOOT_CONTROL_TAG);
        let Some(mount) = mounts.next() else {
            return Ok(false);
        };
        if mounts.next().is_some() || !mount.read_only {
            return Err(BoxError::BoxBootError {
                message: "invalid MicroVM guest boot control mount contract".to_string(),
                hint: None,
            });
        }
        let mut terminal_mounts = spec
            .fs_mounts
            .iter()
            .filter(|mount| mount.tag == GUEST_TERMINAL_CONTROL_TAG);
        let Some(terminal_mount) = terminal_mounts.next() else {
            return Err(BoxError::BoxBootError {
                message: "MicroVM guest terminal control mount is missing".to_string(),
                hint: None,
            });
        };
        if terminal_mounts.next().is_some() || terminal_mount.read_only {
            return Err(BoxError::BoxBootError {
                message: "invalid MicroVM guest terminal control mount contract".to_string(),
                hint: None,
            });
        }
        if !spec
            .entrypoint
            .env
            .iter()
            .any(|(name, value)| name == GUEST_BOOT_CONFIG_ENV && value == GUEST_BOOT_CONFIG_PATH)
        {
            return Err(BoxError::BoxBootError {
                message: "MicroVM guest boot control pointer is missing".to_string(),
                hint: None,
            });
        }

        let directory_metadata = std::fs::symlink_metadata(&mount.host_path).map_err(|error| {
            BoxError::BoxBootError {
                message: format!(
                    "failed to inspect guest boot control directory {}: {error}",
                    mount.host_path.display()
                ),
                hint: None,
            }
        })?;
        if !directory_metadata.is_dir() || directory_metadata.file_type().is_symlink() {
            return Err(BoxError::BoxBootError {
                message: format!(
                    "guest boot control source is not a directory: {}",
                    mount.host_path.display()
                ),
                hint: None,
            });
        }

        let config_path = mount.host_path.join(GUEST_BOOT_CONFIG_FILE_NAME);
        let metadata =
            std::fs::symlink_metadata(&config_path).map_err(|error| BoxError::BoxBootError {
                message: format!(
                    "failed to inspect guest boot configuration {}: {error}",
                    config_path.display()
                ),
                hint: None,
            })?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(BoxError::BoxBootError {
                message: format!(
                    "guest boot configuration is not a regular file: {}",
                    config_path.display()
                ),
                hint: None,
            });
        }
        if metadata.len() > MAX_GUEST_BOOT_CONFIG_BYTES as u64 {
            return Err(BoxError::BoxBootError {
                message: format!(
                    "guest boot configuration is {} bytes; limit is {} bytes",
                    metadata.len(),
                    MAX_GUEST_BOOT_CONFIG_BYTES
                ),
                hint: None,
            });
        }

        let bytes = std::fs::read(&config_path).map_err(BoxError::IoError)?;
        if bytes.len() > MAX_GUEST_BOOT_CONFIG_BYTES {
            return Err(BoxError::BoxBootError {
                message: format!(
                    "guest boot configuration grew to {} bytes; limit is {} bytes",
                    bytes.len(),
                    MAX_GUEST_BOOT_CONFIG_BYTES
                ),
                hint: None,
            });
        }
        let mut config: GuestBootConfig =
            serde_json::from_slice(&bytes).map_err(|error| BoxError::BoxBootError {
                message: format!("failed to parse guest boot configuration: {error}"),
                hint: None,
            })?;
        config
            .validate()
            .map_err(|message| BoxError::BoxBootError {
                message: format!("invalid guest boot configuration: {message}"),
                hint: None,
            })?;
        config.host = host;
        write_guest_boot_config(&mount.host_path, &config)?;
        Ok(true)
    }

    /// Delete the one-shot bundle after guest readiness. The virtio-fs device
    /// remains attached for the VM lifetime, but a privileged workload that
    /// tries to remount its tag can no longer recover argv or environment data.
    pub(crate) fn clear_microvm_guest_boot_config(spec: &InstanceSpec) -> Result<()> {
        let Some(mount) = spec
            .fs_mounts
            .iter()
            .find(|mount| mount.tag == GUEST_BOOT_CONTROL_TAG)
        else {
            return Ok(());
        };
        crate::oci::rootfs::remove_guest_entry_no_follow(
            &mount.host_path,
            GUEST_BOOT_CONFIG_FILE_NAME,
        )?;
        Ok(())
    }

    /// Resolve the executable and args from an OCI image config.
    ///
    /// Follows Docker semantics:
    /// - If `entrypoint_override` is set, it replaces the OCI ENTRYPOINT
    /// - If ENTRYPOINT is set: executable = ENTRYPOINT[0], args = ENTRYPOINT[1:] + CMD
    /// - If only CMD is set: executable = CMD[0], args = CMD[1:]
    /// - If neither: fall back to `/bin/sh` (universal across distros; `/sbin/init`
    ///   does not exist on Alpine, which was the original cause of issue #3)
    /// - If `cmd_override` is non-empty, it replaces the OCI CMD
    ///
    /// Paths are used as-is since the OCI image is always extracted at rootfs root.
    fn resolve_oci_entrypoint(
        oci_config: &OciImageConfig,
        cmd_override: &[String],
        entrypoint_override: Option<&[String]>,
    ) -> (String, Vec<String>) {
        let oci_entrypoint = match entrypoint_override {
            Some(ep) => ep,
            None => oci_config.entrypoint.as_deref().unwrap_or(&[]),
        };
        let oci_cmd = if cmd_override.is_empty() {
            oci_config.cmd.as_deref().unwrap_or(&[])
        } else {
            cmd_override
        };

        if !oci_entrypoint.is_empty() {
            // ENTRYPOINT is set: use it as executable, CMD as additional args
            let exec = oci_entrypoint[0].clone();
            let mut args: Vec<String> = oci_entrypoint.iter().skip(1).cloned().collect();
            args.extend(oci_cmd.iter().cloned());
            (exec, args)
        } else if !oci_cmd.is_empty() {
            // Only CMD is set: use CMD[0] as executable, CMD[1:] as args
            let exec = oci_cmd[0].clone();
            let args: Vec<String> = oci_cmd.iter().skip(1).cloned().collect();
            (exec, args)
        } else {
            // Neither set: fall back to /bin/sh (universal across all Linux distros)
            Self::default_entrypoint()
        }
    }

    /// Resolve an entrypoint from the box config alone.
    ///
    /// Snapshot restores can mount a prepared rootfs without an OCI config file,
    /// but the CLI record still preserves the original ENTRYPOINT/CMD. Keep the
    /// same Docker ordering here: entrypoint args first, then CMD.
    fn resolve_config_entrypoint(
        cmd: &[String],
        entrypoint_override: Option<&[String]>,
    ) -> (String, Vec<String>) {
        if let Some(entrypoint) = entrypoint_override.filter(|entrypoint| !entrypoint.is_empty()) {
            let exec = entrypoint[0].clone();
            let mut args: Vec<String> = entrypoint.iter().skip(1).cloned().collect();
            args.extend(cmd.iter().cloned());
            (exec, args)
        } else if !cmd.is_empty() {
            let exec = cmd[0].clone();
            let args: Vec<String> = cmd.iter().skip(1).cloned().collect();
            (exec, args)
        } else {
            Self::default_entrypoint()
        }
    }

    fn default_entrypoint() -> (String, Vec<String>) {
        (
            "/bin/sh".to_string(),
            vec![
                "-c".to_string(),
                "echo No command specified; exec /bin/sh".to_string(),
            ],
        )
    }

    fn guest_init_exec_path(rootfs_path: &Path) -> Option<&'static str> {
        if crate::oci::rootfs::resolve_guest_file_path(rootfs_path, "sbin/init")
            .is_ok_and(|path| path.is_file())
        {
            return Some(SBIN_INIT);
        }

        if crate::oci::rootfs::resolve_guest_file_path(rootfs_path, "usr/sbin/init")
            .is_ok_and(|path| path.is_file())
        {
            return Some(USR_SBIN_INIT);
        }

        None
    }

    fn effective_workdir(
        config: &a3s_box_core::config::BoxConfig,
        oci_config: Option<&OciImageConfig>,
    ) -> String {
        let image_workdir = oci_config
            .and_then(|oci| oci.working_dir.clone())
            .filter(|workdir| !workdir.is_empty());

        match config
            .workdir
            .as_ref()
            .filter(|workdir| !workdir.is_empty())
        {
            // Absolute override is used as-is.
            Some(workdir) if workdir.starts_with('/') => workdir.clone(),
            // Relative override resolves against the image WORKDIR (Docker's
            // `-w sub` => <image WORKDIR>/sub), falling back to `/` as the base.
            Some(workdir) => {
                let base = image_workdir.unwrap_or_else(|| "/".to_string());
                let base = base.trim_end_matches('/');
                format!("{}/{}", base, workdir.trim_start_matches('/'))
            }
            None => image_workdir.unwrap_or_else(|| GUEST_WORKDIR.to_string()),
        }
    }

    fn effective_user(
        config: &a3s_box_core::config::BoxConfig,
        oci_config: Option<&OciImageConfig>,
    ) -> Option<String> {
        config
            .user
            .as_ref()
            .filter(|user| !user.is_empty())
            .cloned()
            .or_else(|| {
                oci_config
                    .and_then(|oci| oci.user.clone())
                    .filter(|user| !user.is_empty())
            })
    }
}

mod volumes;

#[cfg(test)]
#[path = "spec/tests.rs"]
mod tests;
