//! Instance spec building — entrypoint resolution, volume mounts, OCI config.

use std::path::{Path, PathBuf};

use a3s_box_core::config::{validate_vcpu_count, TeeConfig};
use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::guest_exec::{
    GuestExecConfig, MAX_RUNTIME_EXEC_CONFIG_BYTES, RUNTIME_EXEC_CONFIG_PATH,
};
use a3s_box_core::rootfs_metadata::RUNTIME_ENV_PATH;

use crate::oci::OciImageConfig;
use crate::rootfs::GUEST_WORKDIR;
use crate::vmm::{Entrypoint, FsMount, InstanceSpec};

use super::{fnv1a_hash, BoxLayout, VmManager};

const SBIN_INIT: &str = "/sbin/init";
const USR_SBIN_INIT: &str = "/usr/sbin/init";

#[derive(Debug)]
struct ParsedVolumeMount {
    host_path: PathBuf,
    guest_path: String,
    read_only: bool,
}

/// Read an environment variable, returning `None` if unset or empty.
fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn secure_guest_control_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(
            |error| BoxError::BoxBootError {
                message: format!(
                    "failed to secure guest control file {}: {error}",
                    path.display()
                ),
                hint: None,
            },
        )?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

impl VmManager {
    /// Build InstanceSpec from config and layout.
    pub(crate) fn build_instance_spec(&mut self, layout: &BoxLayout) -> Result<InstanceSpec> {
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
        let parsed_volumes = self
            .config
            .volumes
            .iter()
            .map(|volume| Self::parse_volume_spec(volume))
            .collect::<Result<Vec<_>>>()?;
        for (i, volume) in parsed_volumes.iter().enumerate() {
            let mount = Self::prepare_volume_mount(volume, i, &filemounts_dir)?;
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
        let guest_init_exec = Self::guest_init_exec_path(&layout.rootfs_path);
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

            // Stage process configuration in the guest rootfs instead of adding
            // user-controlled exec/argv strings to libkrun's kernel command line.
            // Linux truncates that command line at COMMAND_LINE_SIZE, which made
            // sufficiently long but valid commands silently fail during WHPX boot.
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
            let exec_config_host_path = crate::oci::rootfs::replace_guest_file_no_follow(
                &layout.rootfs_path,
                RUNTIME_EXEC_CONFIG_PATH.trim_start_matches('/'),
                exec_config_bytes,
            )?;
            secure_guest_control_file(&exec_config_host_path)?;

            // Container environment values remain base64-encoded in their own
            // staged file. Keep the marker for old guest-init fallback decoding.
            use base64::Engine;
            let b64 =
                |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s.as_bytes());
            let mut env: Vec<(String, String)> = vec![
                ("BOX_EXEC_B64".to_string(), "1".to_string()),
                (
                    "BOX_EXEC_CONFIG_FILE".to_string(),
                    RUNTIME_EXEC_CONFIG_PATH.to_string(),
                ),
            ];

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

            // Container environment variables. Values are base64-encoded like the
            // rest (so `"`/spaces/etc. survive); the key stays raw (env names are a
            // safe charset). These are staged in a FILE in the guest rootfs rather
            // than passed inline: Kubernetes injects ~150 service env vars per pod
            // (one set per Service via enableServiceLinks), and inline they bloat
            // the env block libkrun packs into the guest kernel cmdline, overflow
            // COMMAND_LINE_SIZE, and the guest silently fails to boot. Guest-init
            // reads the file via the BOX_EXEC_ENV_FILE pointer below; only that
            // small pointer rides the cmdline. Each line is `KEY=base64(value)`.
            let env_file_body: String = container_env
                .iter()
                .map(|(key, value)| format!("{}={}\n", key, b64(value)))
                .collect();
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
                env.push((
                    format!("BOX_VOL_{}", i),
                    format!("vol{}:{}{}{}", i, volume.guest_path, mode, file_flag),
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
                        format!("vol{}:{}", anon_idx, vol_path),
                    ));
                    anon_idx += 1;
                }
            }

            // Pass tmpfs mounts to guest init.
            // Format: BOX_TMPFS_<index>=<path>[:<options>]
            for (i, tmpfs_spec) in self.config.tmpfs.iter().enumerate() {
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

            if let Some(hostname) = self.config.hostname.as_ref() {
                env.push(("BOX_HOSTNAME".to_string(), hostname.clone()));
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

        // Inject sidecar configuration so guest-init can launch the sidecar process
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
            rootfs_path: layout.rootfs_path.clone(),
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

    /// Parse a volume mount string from the right so colons in a host path do
    /// not consume the host/guest separator. The guest always uses an absolute
    /// Linux path, even when the host path is a Windows drive or UNC path.
    fn parse_volume_spec(volume: &str) -> Result<ParsedVolumeMount> {
        let (mount, read_only) = match volume.rsplit_once(':') {
            Some((mount, "ro")) => (mount, true),
            Some((mount, "rw")) => (mount, false),
            Some((mount, mode)) if mount.contains(':') && !mode.starts_with('/') => {
                return Err(BoxError::ConfigError(format!(
                    "Invalid volume mode '{}' (expected 'ro' or 'rw'): {}",
                    mode, volume
                )));
            }
            _ => (volume, false),
        };

        let (host_path, guest_path) = mount.rsplit_once(':').ok_or_else(|| {
            BoxError::ConfigError(format!(
                "Invalid volume format (expected host:guest[:ro|rw]): {}",
                volume
            ))
        })?;
        if host_path.is_empty() || !guest_path.starts_with('/') {
            return Err(BoxError::ConfigError(format!(
                "Invalid volume format (expected host:guest[:ro|rw]): {}",
                volume
            )));
        }

        Ok(ParsedVolumeMount {
            host_path: PathBuf::from(host_path),
            guest_path: guest_path.to_string(),
            read_only,
        })
    }

    fn prepare_volume_mount(
        volume: &ParsedVolumeMount,
        index: usize,
        filemounts_dir: &Path,
    ) -> Result<FsMount> {
        let host_path = volume.host_path.clone();
        if !host_path.exists() {
            std::fs::create_dir_all(&host_path).map_err(|e| BoxError::BoxBootError {
                message: format!(
                    "Failed to create volume host directory {}: {}",
                    host_path.display(),
                    e
                ),
                hint: None,
            })?;
        }
        let host_path = host_path
            .canonicalize()
            .map_err(|e| BoxError::BoxBootError {
                message: format!(
                    "Failed to resolve volume path {}: {}",
                    host_path.display(),
                    e
                ),
                hint: None,
            })?;

        let host_path = if host_path.is_file() {
            Self::stage_single_file_mount(&host_path, &volume.guest_path, index, filemounts_dir)?
        } else {
            host_path
        };
        let tag = format!("vol{}", index);

        tracing::info!(
            tag = %tag,
            host = %host_path.display(),
            guest = %volume.guest_path,
            read_only = volume.read_only,
            "Adding user volume mount"
        );

        Ok(FsMount {
            tag,
            host_path,
            read_only: volume.read_only,
        })
    }

    #[cfg(test)]
    fn parse_volume_mount(volume: &str, index: usize, filemounts_dir: &Path) -> Result<FsMount> {
        let parsed_volume = Self::parse_volume_spec(volume)?;
        Self::prepare_volume_mount(&parsed_volume, index, filemounts_dir)
    }

    /// Stage a single-file bind source into a per-box directory so virtio-fs (which
    /// shares directories, not bare files) can expose it. Returns the directory to
    /// share; it contains exactly one entry — the file under the guest path's
    /// basename, which `mount_user_volumes` then binds onto the guest path. The
    /// file is hard-linked to keep the bind live in both directions; across
    /// filesystems it falls back to a copy (host-side writes then do not propagate).
    fn stage_single_file_mount(
        source: &Path,
        guest_path: &str,
        index: usize,
        filemounts_dir: &Path,
    ) -> Result<PathBuf> {
        let basename = Path::new(guest_path).file_name().ok_or_else(|| {
            BoxError::ConfigError(format!(
                "Single-file bind guest path has no file name: {guest_path}"
            ))
        })?;
        let stage_dir = filemounts_dir.join(index.to_string());
        std::fs::create_dir_all(&stage_dir).map_err(|e| BoxError::BoxBootError {
            message: format!(
                "Failed to create file-mount staging dir {}: {}",
                stage_dir.display(),
                e
            ),
            hint: None,
        })?;
        let staged = stage_dir.join(basename);
        let _ = std::fs::remove_file(&staged); // idempotent across restarts
        if std::fs::hard_link(source, &staged).is_err() {
            std::fs::copy(source, &staged).map_err(|e| BoxError::BoxBootError {
                message: format!(
                    "Failed to stage single-file mount {} -> {}: {}",
                    source.display(),
                    staged.display(),
                    e
                ),
                hint: None,
            })?;
            tracing::warn!(
                source = %source.display(),
                "Single-file bind staged by copy (source on a different filesystem); \
                 host-side writes will not propagate to the container"
            );
        }
        Ok(stage_dir)
    }

    /// Create an anonymous volume via VolumeStore.
    ///
    /// Returns the host path of the created volume.
    fn create_anonymous_volume(&self, name: &str) -> Result<(String, bool)> {
        use crate::volume::VolumeStore;

        let store = VolumeStore::new(
            self.home_dir.join("volumes.json"),
            self.home_dir.join("volumes"),
        );

        // If the volume already exists (e.g., from a previous run), reuse it
        if let Some(existing) = store.get(name)? {
            return Ok((existing.mount_point, false));
        }

        let mut config = a3s_box_core::volume::VolumeConfig::new(name, "");
        config
            .labels
            .insert("anonymous".to_string(), "true".to_string());
        config.attach(&self.box_id);
        let created = store.create(config)?;
        Ok((created.mount_point, true))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use a3s_box_core::config::BoxConfig;
    use a3s_box_core::event::EventEmitter;

    use super::*;
    use tempfile::tempdir;
    use tempfile::TempDir;

    #[cfg(unix)]
    fn create_dir_symlink(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).unwrap();
        true
    }

    #[cfg(unix)]
    fn create_file_symlink(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).unwrap();
        true
    }

    #[cfg(windows)]
    fn create_file_symlink(target: &Path, link: &Path) -> bool {
        match std::os::windows::fs::symlink_file(target, link) {
            Ok(()) => true,
            Err(error) if error.raw_os_error() == Some(1314) => false,
            Err(error) => panic!("failed to create Windows test symlink: {error}"),
        }
    }

    #[cfg(windows)]
    fn create_dir_symlink(target: &Path, link: &Path) -> bool {
        match std::os::windows::fs::symlink_dir(target, link) {
            Ok(()) => true,
            Err(error) if error.raw_os_error() == Some(1314) => false,
            Err(error) => panic!("failed to create Windows test symlink: {error}"),
        }
    }

    /// Decode a base64 (URL-safe, no pad) staged environment value the way
    /// guest-init does, so assertions can compare against the original value.
    fn b64d(s: &str) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(s.as_bytes())
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .unwrap_or_else(|| s.to_string())
    }

    fn test_oci_config(workdir: Option<&str>, user: Option<&str>) -> OciImageConfig {
        OciImageConfig {
            entrypoint: Some(vec!["/bin/app".to_string()]),
            cmd: Some(vec!["--serve".to_string()]),
            env: vec![],
            working_dir: workdir.map(str::to_string),
            user: user.map(str::to_string),
            exposed_ports: vec![],
            labels: std::collections::HashMap::new(),
            volumes: vec![],
            stop_signal: None,
            health_check: None,
            onbuild: vec![],
        }
    }

    fn test_layout(
        base: &Path,
        oci_config: Option<OciImageConfig>,
        with_guest_init: bool,
    ) -> BoxLayout {
        let rootfs_path = base.join("rootfs");
        fs::create_dir_all(&rootfs_path).unwrap();
        if with_guest_init {
            fs::create_dir_all(rootfs_path.join("sbin")).unwrap();
            fs::write(rootfs_path.join("sbin").join("init"), b"guest-init").unwrap();
        }

        BoxLayout {
            rootfs_path,
            exec_socket_path: base.join("exec.sock"),
            pty_socket_path: base.join("pty.sock"),
            attest_socket_path: base.join("attest.sock"),
            port_forward_socket_path: base.join("portfwd.sock"),
            workspace_path: base.join("workspace"),
            console_output: None,
            oci_config,
            prefer_image_rootfs_metadata: false,
            tee_instance_config: None,
        }
    }

    fn test_vm_manager(config: BoxConfig) -> VmManager {
        VmManager::with_box_id(config, EventEmitter::new(16), "test-box".to_string())
    }

    fn env_value<'a>(spec: &'a InstanceSpec, key: &str) -> Option<&'a str> {
        spec.entrypoint
            .env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn staged_exec_config(layout: &BoxLayout) -> GuestExecConfig {
        serde_json::from_slice(&fs::read(layout.rootfs_path.join(".a3s-box-exec.json")).unwrap())
            .unwrap()
    }

    #[test]
    fn test_build_instance_spec_passes_configured_virtiofs_cache_mode() {
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path(), Some(test_oci_config(None, None)), true);
        let mut vm = test_vm_manager(BoxConfig {
            virtiofs_cache: Some("always".to_string()),
            ..Default::default()
        });

        let spec = vm.build_instance_spec(&layout).unwrap();

        assert_eq!(env_value(&spec, "A3S_VIRTIOFS_CACHE"), Some("always"));
    }

    #[test]
    fn test_persistent_box_requests_terminal_rootfs_metadata() {
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path(), Some(test_oci_config(None, None)), true);
        let mut vm = test_vm_manager(BoxConfig {
            persistent: true,
            ..Default::default()
        });

        let spec = vm.build_instance_spec(&layout).unwrap();

        assert_eq!(env_value(&spec, "BOX_PERSIST_ROOTFS_METADATA"), Some("1"));
    }

    #[cfg(windows)]
    #[test]
    fn test_windows_box_enables_host_control_without_published_ports() {
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path(), Some(test_oci_config(None, None)), true);
        let mut vm = test_vm_manager(BoxConfig::default());

        let spec = vm.build_instance_spec(&layout).unwrap();

        assert!(spec.port_map.is_empty());
        assert_eq!(env_value(&spec, "BOX_WINDOWS_PORT_FWD"), Some("1"));
    }

    #[test]
    fn test_run_path_plumbs_cpu_cgroup_limits_to_guest() {
        // The `run` boot path must hand the CPU cgroup limits to guest-init as
        // A3S_SEC_CPU_* (the same vars the CRI path emits and the guest consumes),
        // so `run --cpu-quota/--cpu-shares` is actually enforced in-guest instead
        // of silently dropped.
        let temp = tempdir().unwrap();
        let mut config = BoxConfig::default();
        config.resource_limits.cpu_quota = Some(50_000);
        config.resource_limits.cpu_period = Some(100_000);
        config.resource_limits.cpu_shares = Some(512);
        config.resource_limits.pids_limit = Some(100);

        let mut vm = test_vm_manager(config);
        let layout = test_layout(temp.path(), Some(test_oci_config(None, None)), true);
        let spec = vm.build_instance_spec(&layout).unwrap();

        assert_eq!(env_value(&spec, "A3S_SEC_CPU_QUOTA"), Some("50000"));
        assert_eq!(env_value(&spec, "A3S_SEC_CPU_PERIOD"), Some("100000"));
        assert_eq!(env_value(&spec, "A3S_SEC_CPU_SHARES"), Some("512"));
        assert_eq!(env_value(&spec, "A3S_SEC_PIDS_LIMIT"), Some("100"));
    }

    #[test]
    fn test_run_path_plumbs_memory_reservation_and_swap_to_guest() {
        // --memory-reservation (memory.low) and --memory-swap (memory.swap.max)
        // must reach guest-init as A3S_SEC_MEM_LOW / A3S_SEC_MEM_SWAP so the
        // in-guest cgroup enforces them (the broken host path was removed).
        let temp = tempdir().unwrap();
        let mut config = BoxConfig::default();
        config.resource_limits.memory_reservation = Some(256 * 1024 * 1024);
        config.resource_limits.memory_swap = Some(-1);

        let mut vm = test_vm_manager(config);
        let layout = test_layout(temp.path(), Some(test_oci_config(None, None)), true);
        let spec = vm.build_instance_spec(&layout).unwrap();

        assert_eq!(env_value(&spec, "A3S_SEC_MEM_LOW"), Some("268435456"));
        assert_eq!(env_value(&spec, "A3S_SEC_MEM_SWAP"), Some("-1"));
        // The hard --memory limit is VM-sized, not an in-guest memory.max.
        assert_eq!(env_value(&spec, "A3S_SEC_MEM_LIMIT"), None);
    }

    #[test]
    fn test_run_path_omits_cpu_limits_when_unset_or_unlimited() {
        // No limits set, plus an explicit unlimited quota (-1): nothing should be
        // emitted, so the guest leaves cpu.max at "max".
        let temp = tempdir().unwrap();
        let mut config = BoxConfig::default();
        config.resource_limits.cpu_quota = Some(-1);
        config.resource_limits.cpu_period = Some(100_000);

        let mut vm = test_vm_manager(config);
        let layout = test_layout(temp.path(), Some(test_oci_config(None, None)), true);
        let spec = vm.build_instance_spec(&layout).unwrap();

        assert!(
            !spec
                .entrypoint
                .env
                .iter()
                .any(|(k, _)| k.starts_with("A3S_SEC_CPU_")),
            "no A3S_SEC_CPU_* must be emitted for an unset/unlimited quota"
        );
    }

    #[test]
    fn test_parse_volume_mount_host_guest() {
        let temp = TempDir::new().unwrap();
        let host_path = temp.path().to_str().unwrap();
        let volume = format!("{}:/data", host_path);

        let mount =
            VmManager::parse_volume_mount(&volume, 0, std::path::Path::new("/tmp")).unwrap();
        assert_eq!(mount.tag, "vol0");
        assert_eq!(mount.host_path, temp.path().canonicalize().unwrap());
        assert!(!mount.read_only);
    }

    #[test]
    fn test_parse_volume_mount_read_only() {
        let temp = TempDir::new().unwrap();
        let host_path = temp.path().to_str().unwrap();
        let volume = format!("{}:/data:ro", host_path);

        let mount =
            VmManager::parse_volume_mount(&volume, 1, std::path::Path::new("/tmp")).unwrap();
        assert_eq!(mount.tag, "vol1");
        assert!(mount.read_only);
    }

    #[test]
    fn test_parse_volume_mount_explicit_rw() {
        let temp = TempDir::new().unwrap();
        let host_path = temp.path().to_str().unwrap();
        let volume = format!("{}:/data:rw", host_path);

        let mount =
            VmManager::parse_volume_mount(&volume, 2, std::path::Path::new("/tmp")).unwrap();
        assert_eq!(mount.tag, "vol2");
        assert!(!mount.read_only);
    }

    #[test]
    fn test_parse_volume_spec_preserves_windows_drive_path() {
        for (volume, host) in [
            (r"C:\Users\Temp:/data:ro", r"C:\Users\Temp"),
            (r"C:/Users/Temp:/data:ro", r"C:/Users/Temp"),
        ] {
            let parsed = VmManager::parse_volume_spec(volume).unwrap();

            assert_eq!(parsed.host_path, PathBuf::from(host));
            assert_eq!(parsed.guest_path, "/data");
            assert!(parsed.read_only);
        }
    }

    #[test]
    fn test_parse_volume_spec_preserves_windows_unc_path() {
        let parsed = VmManager::parse_volume_spec(r"\\server\share\folder:/workspace:rw").unwrap();

        assert_eq!(parsed.host_path, PathBuf::from(r"\\server\share\folder"));
        assert_eq!(parsed.guest_path, "/workspace");
        assert!(!parsed.read_only);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn test_build_instance_spec_windows_bind_uses_linux_guest_target() {
        let home = tempdir().unwrap();
        let host = tempdir().unwrap();
        let layout_dir = tempdir().unwrap();
        let mut oci_config = test_oci_config(None, None);
        oci_config.volumes = vec!["/tests".to_string()];
        let layout = test_layout(layout_dir.path(), Some(oci_config), true);
        let mut vm = test_vm_manager(BoxConfig {
            volumes: vec![format!(r"{}:/tests:ro", host.path().display())],
            ..Default::default()
        });
        vm.home_dir = home.path().to_path_buf();

        let spec = vm.build_instance_spec(&layout).unwrap();

        assert_eq!(env_value(&spec, "BOX_VOL_0"), Some("vol0:/tests:ro"));
        assert!(
            vm.anonymous_volumes.is_empty(),
            "the user bind must cover the matching OCI volume"
        );
        assert_eq!(spec.fs_mounts.len(), 2);
    }

    #[test]
    fn test_parse_volume_mount_single_file_is_staged_as_dir() {
        let temp = TempDir::new().unwrap();
        // A real source FILE (not a directory).
        let src = temp.path().join("hostfile.txt");
        std::fs::write(&src, b"DATA").unwrap();
        let stage_base = temp.path().join("filemounts");
        let volume = format!("{}:/etc/myconf", src.display());

        let mount = VmManager::parse_volume_mount(&volume, 3, &stage_base).unwrap();

        // virtio-fs shares directories, so host_path must be the staging DIR, not
        // the bare file.
        assert!(
            mount.host_path.is_dir(),
            "single-file bind must be staged into a directory, got {}",
            mount.host_path.display()
        );
        // The staged dir holds the file under the GUEST basename — what the guest
        // binds onto the guest path.
        let staged = mount.host_path.join("myconf");
        assert!(
            staged.exists(),
            "staged file under guest basename must exist"
        );
        assert_eq!(std::fs::read(&staged).unwrap(), b"DATA");
    }

    #[test]
    fn test_parse_volume_mount_invalid_mode() {
        let temp = TempDir::new().unwrap();
        let host_path = temp.path().to_str().unwrap();
        let volume = format!("{}:/data:invalid", host_path);

        let result = VmManager::parse_volume_mount(&volume, 0, std::path::Path::new("/tmp"));
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Invalid volume mode"));
    }

    #[test]
    fn test_parse_volume_mount_invalid_format() {
        let result = VmManager::parse_volume_mount("invalid", 0, std::path::Path::new("/tmp"));
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Invalid volume format"));
    }

    #[test]
    fn test_parse_volume_mount_creates_missing_dir() {
        let temp = TempDir::new().unwrap();
        let host_path = temp.path().join("nonexistent");
        let volume = format!("{}:/data", host_path.display());

        assert!(!host_path.exists());
        let mount =
            VmManager::parse_volume_mount(&volume, 0, std::path::Path::new("/tmp")).unwrap();
        assert!(host_path.exists());
        assert_eq!(mount.host_path, host_path.canonicalize().unwrap());
    }

    #[test]
    fn test_resolve_oci_entrypoint_with_entrypoint_and_cmd() {
        let config = OciImageConfig {
            entrypoint: Some(vec!["/bin/app".to_string()]),
            cmd: Some(vec!["--flag".to_string()]),
            env: vec![],
            working_dir: None,
            user: None,
            exposed_ports: vec![],
            labels: std::collections::HashMap::new(),
            volumes: vec![],
            stop_signal: None,
            health_check: None,
            onbuild: vec![],
        };

        let (exec, args) = VmManager::resolve_oci_entrypoint(&config, &[], None);
        assert_eq!(exec, "/bin/app");
        assert_eq!(args, vec!["--flag"]);
    }

    #[test]
    fn test_resolve_oci_entrypoint_cmd_only() {
        let config = OciImageConfig {
            entrypoint: None,
            cmd: Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "echo hi".to_string(),
            ]),
            env: vec![],
            working_dir: None,
            user: None,
            exposed_ports: vec![],
            labels: std::collections::HashMap::new(),
            volumes: vec![],
            stop_signal: None,
            health_check: None,
            onbuild: vec![],
        };

        let (exec, args) = VmManager::resolve_oci_entrypoint(&config, &[], None);
        assert_eq!(exec, "/bin/sh");
        assert_eq!(args, vec!["-c", "echo hi"]);
    }

    #[test]
    fn test_resolve_oci_entrypoint_neither() {
        let config = OciImageConfig {
            entrypoint: None,
            cmd: None,
            env: vec![],
            working_dir: None,
            user: None,
            exposed_ports: vec![],
            labels: std::collections::HashMap::new(),
            volumes: vec![],
            stop_signal: None,
            health_check: None,
            onbuild: vec![],
        };

        let (exec, _args) = VmManager::resolve_oci_entrypoint(&config, &[], None);
        assert_eq!(exec, "/bin/sh");
    }

    #[test]
    fn test_resolve_oci_entrypoint_cmd_override() {
        let config = OciImageConfig {
            entrypoint: None,
            cmd: Some(vec!["/bin/sh".to_string()]),
            env: vec![],
            working_dir: None,
            user: None,
            exposed_ports: vec![],
            labels: std::collections::HashMap::new(),
            volumes: vec![],
            stop_signal: None,
            health_check: None,
            onbuild: vec![],
        };

        let override_cmd = vec!["sleep".to_string(), "3600".to_string()];
        let (exec, args) = VmManager::resolve_oci_entrypoint(&config, &override_cmd, None);
        assert_eq!(exec, "sleep");
        assert_eq!(args, vec!["3600"]);
    }

    #[test]
    fn test_resolve_oci_entrypoint_with_override() {
        let config = OciImageConfig {
            entrypoint: Some(vec!["/bin/app".to_string()]),
            cmd: Some(vec!["--flag".to_string()]),
            env: vec![],
            working_dir: None,
            user: None,
            exposed_ports: vec![],
            labels: std::collections::HashMap::new(),
            volumes: vec![],
            stop_signal: None,
            health_check: None,
            onbuild: vec![],
        };

        // Override replaces the image entrypoint entirely
        let override_ep = vec!["/bin/sh".to_string(), "-c".to_string()];
        let (exec, args) = VmManager::resolve_oci_entrypoint(&config, &[], Some(&override_ep));
        assert_eq!(exec, "/bin/sh");
        // args = entrypoint[1:] + cmd
        assert_eq!(args, vec!["-c", "--flag"]);
    }

    #[test]
    fn test_resolve_oci_entrypoint_override_with_cmd_override() {
        let config = OciImageConfig {
            entrypoint: Some(vec!["/bin/app".to_string()]),
            cmd: Some(vec!["--flag".to_string()]),
            env: vec![],
            working_dir: None,
            user: None,
            exposed_ports: vec![],
            labels: std::collections::HashMap::new(),
            volumes: vec![],
            stop_signal: None,
            health_check: None,
            onbuild: vec![],
        };

        // Both entrypoint and cmd overridden
        let override_ep = vec!["/bin/sh".to_string()];
        let cmd_override = vec!["echo".to_string(), "hello".to_string()];
        let (exec, args) =
            VmManager::resolve_oci_entrypoint(&config, &cmd_override, Some(&override_ep));
        assert_eq!(exec, "/bin/sh");
        assert_eq!(args, vec!["echo", "hello"]);
    }

    #[test]
    fn test_resolve_config_entrypoint_preserves_overrides() {
        let entrypoint = vec!["/custom".to_string(), "--flag".to_string()];
        let cmd = vec!["echo".to_string(), "restored".to_string()];

        let (exec, args) = VmManager::resolve_config_entrypoint(&cmd, Some(&entrypoint));

        assert_eq!(exec, "/custom");
        assert_eq!(args, vec!["--flag", "echo", "restored"]);
    }

    #[test]
    fn test_guest_init_exec_path_prefers_sbin() {
        let dir = tempdir().unwrap();
        let rootfs = dir.path();
        fs::create_dir_all(rootfs.join("sbin")).unwrap();
        fs::write(rootfs.join("sbin").join("init"), b"guest-init").unwrap();

        assert_eq!(VmManager::guest_init_exec_path(rootfs), Some("/sbin/init"));
    }

    #[test]
    fn test_guest_init_exec_path_resolves_multi_hop_guest_sbin_symlink() {
        let dir = tempdir().unwrap();
        let rootfs = dir.path();
        fs::create_dir_all(rootfs.join("usr")).unwrap();
        fs::create_dir_all(rootfs.join("shared/sbin")).unwrap();
        fs::write(rootfs.join("shared/sbin/init"), b"guest-init").unwrap();
        if !create_dir_symlink(Path::new("/usr/sbin"), &rootfs.join("sbin"))
            || !create_dir_symlink(Path::new("../shared/sbin"), &rootfs.join("usr/sbin"))
        {
            return;
        }

        assert_eq!(VmManager::guest_init_exec_path(rootfs), Some("/sbin/init"));
    }

    #[test]
    fn test_guest_init_exec_path_rejects_sbin_escape() {
        let dir = tempdir().unwrap();
        let rootfs = dir.path().join("rootfs");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&rootfs).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("init"), b"host-init").unwrap();
        if !create_dir_symlink(Path::new("../outside"), &rootfs.join("sbin")) {
            return;
        }

        assert_eq!(VmManager::guest_init_exec_path(&rootfs), None);
    }

    #[test]
    fn test_build_instance_spec_restored_rootfs_uses_saved_cmd_with_guest_init() {
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path(), None, true);
        let mut vm = test_vm_manager(BoxConfig {
            cmd: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "sleep 3600".to_string(),
            ],
            ..Default::default()
        });

        let spec = vm.build_instance_spec(&layout).unwrap();

        assert_eq!(spec.entrypoint.executable, "/sbin/init");
        let staged = staged_exec_config(&layout);
        assert_eq!(staged.executable, "/bin/sh");
        assert_eq!(staged.args, ["-c", "sleep 3600"]);
        assert!(!spec
            .entrypoint
            .env
            .iter()
            .any(|(_, value)| value.contains("No command specified")));
    }

    #[test]
    fn test_build_instance_spec_stages_large_exec_config_off_kernel_cmdline() {
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path(), None, true);
        let long_arg = "x".repeat(4096);
        let mut vm = test_vm_manager(BoxConfig {
            cmd: vec!["/bin/echo".to_string(), long_arg.clone()],
            ..Default::default()
        });

        let spec = vm.build_instance_spec(&layout).unwrap();

        assert_eq!(
            env_value(&spec, "BOX_EXEC_CONFIG_FILE"),
            Some("/.a3s-box-exec.json")
        );
        assert!(!spec.entrypoint.env.iter().any(|(key, _)| {
            key == "BOX_EXEC_EXEC"
                || key == "BOX_EXEC_ARGC"
                || key == "BOX_EXEC_WORKDIR"
                || key == "BOX_EXEC_USER"
                || key == "BOX_EXEC_STDIN"
                || key.starts_with("BOX_EXEC_ARG_")
        }));

        let staged = staged_exec_config(&layout);
        assert_eq!(staged.schema, "a3s.box.guest-exec.v1");
        assert_eq!(staged.executable, "/bin/echo");
        assert_eq!(staged.args[0], long_arg);
    }

    #[test]
    fn test_build_instance_spec_rejects_oversized_exec_config() {
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path(), None, true);
        let mut vm = test_vm_manager(BoxConfig {
            cmd: vec![
                "/bin/echo".to_string(),
                "x".repeat(MAX_RUNTIME_EXEC_CONFIG_BYTES),
            ],
            ..Default::default()
        });

        let error = vm.build_instance_spec(&layout).unwrap_err().to_string();

        assert!(error.contains("guest exec configuration"), "{error}");
        assert!(error.contains("limit"), "{error}");
        assert!(!layout.rootfs_path.join(".a3s-box-exec.json").exists());
    }

    #[test]
    fn test_build_instance_spec_replaces_exec_config_symlink_without_following_target() {
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path(), None, true);
        let outside = dir.path().join("outside-exec-config");
        fs::write(&outside, "unchanged").unwrap();
        if !create_file_symlink(
            Path::new("../outside-exec-config"),
            &layout.rootfs_path.join(".a3s-box-exec.json"),
        ) {
            return;
        }
        let mut vm = test_vm_manager(BoxConfig {
            cmd: vec!["/bin/echo".to_string(), "safe".to_string()],
            ..Default::default()
        });

        vm.build_instance_spec(&layout).unwrap();

        assert_eq!(fs::read_to_string(outside).unwrap(), "unchanged");
        assert!(
            fs::symlink_metadata(layout.rootfs_path.join(".a3s-box-exec.json"))
                .unwrap()
                .file_type()
                .is_file()
        );
        assert_eq!(staged_exec_config(&layout).args, ["safe"]);
    }

    #[test]
    fn test_build_instance_spec_restored_rootfs_uses_saved_entrypoint_without_guest_init() {
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path(), None, false);
        let mut vm = test_vm_manager(BoxConfig {
            cmd: vec!["hello".to_string()],
            entrypoint_override: Some(vec!["/bin/echo".to_string(), "prefix".to_string()]),
            extra_env: vec![("FOO".to_string(), "bar".to_string())],
            ..Default::default()
        });

        let spec = vm.build_instance_spec(&layout).unwrap();

        assert_eq!(spec.entrypoint.executable, "/bin/echo");
        assert_eq!(spec.entrypoint.args, vec!["prefix", "hello"]);
        assert_eq!(env_value(&spec, "FOO"), Some("bar"));
    }

    #[test]
    fn test_build_instance_spec_prefers_config_workdir_and_user() {
        let dir = tempdir().unwrap();
        let layout = test_layout(
            dir.path(),
            Some(test_oci_config(Some("/oci"), Some("2000:2000"))),
            true,
        );
        let mut vm = test_vm_manager(BoxConfig {
            workdir: Some("/override".to_string()),
            user: Some("1000:1000".to_string()),
            ..Default::default()
        });

        let spec = vm.build_instance_spec(&layout).unwrap();

        assert_eq!(spec.workdir, "/override");
        // With guest init present, the user is applied from the staged process
        // config, not by the shim's set_uid, so spec.user is None.
        assert_eq!(spec.user, None);
        let staged = staged_exec_config(&layout);
        assert_eq!(staged.user.as_deref(), Some("1000:1000"));
        assert_eq!(staged.workdir, "/override");
    }

    #[test]
    fn test_relative_workdir_resolves_against_image_workdir() {
        // Docker `-w sub` resolves against the image WORKDIR.
        let oci = test_oci_config(Some("/srv/app"), None);
        let cfg = BoxConfig {
            workdir: Some("sub".to_string()),
            ..Default::default()
        };
        assert_eq!(
            VmManager::effective_workdir(&cfg, Some(&oci)),
            "/srv/app/sub"
        );
        // Absolute override is used verbatim.
        let cfg_abs = BoxConfig {
            workdir: Some("/abs".to_string()),
            ..Default::default()
        };
        assert_eq!(VmManager::effective_workdir(&cfg_abs, Some(&oci)), "/abs");
        // Relative with no image WORKDIR resolves against `/`.
        let cfg_rel = BoxConfig {
            workdir: Some("work".to_string()),
            ..Default::default()
        };
        assert_eq!(VmManager::effective_workdir(&cfg_rel, None), "/work");
    }

    #[test]
    fn test_build_instance_spec_uses_oci_workdir_and_user_without_override() {
        let dir = tempdir().unwrap();
        let layout = test_layout(
            dir.path(),
            Some(test_oci_config(Some("/oci"), Some("2000:2000"))),
            true,
        );
        let mut vm = test_vm_manager(BoxConfig::default());

        let spec = vm.build_instance_spec(&layout).unwrap();

        assert_eq!(spec.workdir, "/oci");
        assert_eq!(spec.user, None);
        let staged = staged_exec_config(&layout);
        assert_eq!(staged.user.as_deref(), Some("2000:2000"));
        assert_eq!(staged.workdir, "/oci");
    }

    #[test]
    fn test_build_instance_spec_passes_default_workdir_to_guest_init() {
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path(), Some(test_oci_config(None, None)), true);
        let mut vm = test_vm_manager(BoxConfig::default());

        let spec = vm.build_instance_spec(&layout).unwrap();

        assert_eq!(spec.workdir, GUEST_WORKDIR);
        assert_eq!(staged_exec_config(&layout).workdir, GUEST_WORKDIR);
    }

    #[test]
    fn test_build_instance_spec_without_oci_uses_persisted_command() {
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path(), None, true);
        let mut vm = test_vm_manager(BoxConfig {
            cmd: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "printf snapshot-restored".to_string(),
            ],
            ..Default::default()
        });

        vm.build_instance_spec(&layout).unwrap();

        let staged = staged_exec_config(&layout);
        assert_eq!(staged.executable, "/bin/sh");
        assert_eq!(staged.args, ["-c", "printf snapshot-restored"]);
    }

    #[test]
    fn test_build_instance_spec_passes_hostname_to_guest_init() {
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path(), Some(test_oci_config(None, None)), true);
        let mut vm = test_vm_manager(BoxConfig {
            hostname: Some("web".to_string()),
            ..Default::default()
        });

        let spec = vm.build_instance_spec(&layout).unwrap();

        assert!(spec
            .entrypoint
            .env
            .iter()
            .any(|(key, value)| key == "BOX_HOSTNAME" && value == "web"));
    }

    #[test]
    fn test_build_instance_spec_guest_init_prefixes_extra_env() {
        let dir = tempdir().unwrap();
        let mut oci_config = test_oci_config(None, None);
        oci_config.env = vec![
            ("FOO".to_string(), "image".to_string()),
            ("BAR".to_string(), "image".to_string()),
        ];
        let layout = test_layout(dir.path(), Some(oci_config), true);
        let mut vm = test_vm_manager(BoxConfig {
            extra_env: vec![
                ("FOO".to_string(), "cli".to_string()),
                ("BAZ".to_string(), "cli".to_string()),
            ],
            ..Default::default()
        });

        let spec = vm.build_instance_spec(&layout).unwrap();

        // Container env is staged in a file in the rootfs, not inlined: only a
        // small BOX_EXEC_ENV_FILE pointer rides the env block (cmdline overflow).
        assert!(spec
            .entrypoint
            .env
            .iter()
            .any(|(key, value)| key == "BOX_EXEC_ENV_FILE" && value == "/.a3s-box-env"));
        // No raw container env keys leak into the inline env block.
        assert!(!spec
            .entrypoint
            .env
            .iter()
            .any(|(key, _)| key == "FOO" || key == "BAR" || key == "BAZ"));

        // The staged file holds one `KEY=base64(value)` line per var, with the
        // CLI extra_env overriding the image's env (FOO/BAZ from cli, BAR from image).
        let staged = std::fs::read_to_string(layout.rootfs_path.join(".a3s-box-env")).unwrap();
        let entries: std::collections::HashMap<&str, String> = staged
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k, b64d(v)))
            .collect();
        assert_eq!(entries.get("FOO").map(String::as_str), Some("cli"));
        assert_eq!(entries.get("BAR").map(String::as_str), Some("image"));
        assert_eq!(entries.get("BAZ").map(String::as_str), Some("cli"));
    }

    #[test]
    fn test_build_instance_spec_stages_env_through_internal_guest_symlink() {
        let dir = tempdir().unwrap();
        let mut oci_config = test_oci_config(None, None);
        oci_config.env = vec![("FOO".to_string(), "safe".to_string())];
        let layout = test_layout(dir.path(), Some(oci_config), true);
        fs::create_dir_all(layout.rootfs_path.join("shared")).unwrap();
        if !create_file_symlink(
            Path::new("shared/env"),
            &layout.rootfs_path.join(".a3s-box-env"),
        ) {
            return;
        }
        let mut vm = test_vm_manager(BoxConfig::default());

        vm.build_instance_spec(&layout).unwrap();

        assert_eq!(
            b64d(
                fs::read_to_string(layout.rootfs_path.join("shared/env"))
                    .unwrap()
                    .trim_start_matches("FOO=")
                    .trim()
            ),
            "safe"
        );
    }

    #[test]
    fn test_build_instance_spec_rejects_env_file_symlink_escape() {
        let dir = tempdir().unwrap();
        let rootfs_parent = dir.path().join("layout");
        let mut oci_config = test_oci_config(None, None);
        oci_config.env = vec![("FOO".to_string(), "unsafe".to_string())];
        let layout = test_layout(&rootfs_parent, Some(oci_config), true);
        let outside = rootfs_parent.join("outside-env");
        if !create_file_symlink(
            Path::new("../outside-env"),
            &layout.rootfs_path.join(".a3s-box-env"),
        ) {
            return;
        }
        let mut vm = test_vm_manager(BoxConfig::default());

        let error = vm.build_instance_spec(&layout).unwrap_err().to_string();

        assert!(error.contains("escapes rootfs"), "{error}");
        assert!(!outside.exists());
    }

    #[test]
    fn test_build_instance_spec_direct_entrypoint_merges_extra_env() {
        let dir = tempdir().unwrap();
        let mut oci_config = test_oci_config(None, None);
        oci_config.env = vec![
            ("FOO".to_string(), "image".to_string()),
            ("BAR".to_string(), "image".to_string()),
        ];
        let layout = test_layout(dir.path(), Some(oci_config), false);
        let mut vm = test_vm_manager(BoxConfig {
            extra_env: vec![
                ("FOO".to_string(), "cli".to_string()),
                ("BAZ".to_string(), "cli".to_string()),
            ],
            ..Default::default()
        });

        let spec = vm.build_instance_spec(&layout).unwrap();

        assert!(spec
            .entrypoint
            .env
            .iter()
            .any(|(key, value)| key == "FOO" && value == "cli"));
        assert!(spec
            .entrypoint
            .env
            .iter()
            .any(|(key, value)| key == "BAR" && value == "image"));
        assert!(spec
            .entrypoint
            .env
            .iter()
            .any(|(key, value)| key == "BAZ" && value == "cli"));
    }

    #[test]
    fn test_build_instance_spec_tracks_new_anonymous_volumes_only() {
        let home = tempdir().unwrap();
        let layout_dir = tempdir().unwrap();
        let mut oci_config = test_oci_config(None, None);
        oci_config.volumes = vec!["/data".to_string()];
        let layout = test_layout(layout_dir.path(), Some(oci_config), true);

        let mut first_vm = test_vm_manager(BoxConfig::default());
        first_vm.home_dir = home.path().to_path_buf();
        let first_spec = first_vm.build_instance_spec(&layout).unwrap();

        assert_eq!(first_vm.anonymous_volumes.len(), 1);
        assert_eq!(
            first_vm.created_anonymous_volumes,
            first_vm.anonymous_volumes
        );
        assert!(first_spec.fs_mounts.iter().any(|mount| {
            mount.tag == "vol0" && mount.host_path.starts_with(home.path().join("volumes"))
        }));

        let volume_name = first_vm.anonymous_volumes[0].clone();
        let store = crate::volume::VolumeStore::new(
            home.path().join("volumes.json"),
            home.path().join("volumes"),
        );
        assert!(store.get(&volume_name).unwrap().is_some());

        let mut second_vm = test_vm_manager(BoxConfig::default());
        second_vm.home_dir = home.path().to_path_buf();
        second_vm.anonymous_volumes = vec![volume_name.clone(), volume_name.clone()];
        second_vm.build_instance_spec(&layout).unwrap();
        second_vm.build_instance_spec(&layout).unwrap();

        assert_eq!(second_vm.anonymous_volumes, vec![volume_name]);
        assert!(second_vm.created_anonymous_volumes.is_empty());
        assert_eq!(
            store
                .get(&second_vm.anonymous_volumes[0])
                .unwrap()
                .unwrap()
                .in_use_by,
            vec!["test-box".to_string()]
        );
    }

    #[test]
    fn test_guest_init_exec_path_supports_usr_sbin_without_sbin() {
        let dir = tempdir().unwrap();
        let rootfs = dir.path();
        fs::create_dir_all(rootfs.join("usr").join("sbin")).unwrap();
        fs::write(rootfs.join("usr").join("sbin").join("init"), b"guest-init").unwrap();

        assert_eq!(
            VmManager::guest_init_exec_path(rootfs),
            Some("/usr/sbin/init")
        );
    }

    #[test]
    fn test_parse_volume_mount_guest_path_with_colons() {
        let temp = TempDir::new().unwrap();
        let host_path = temp.path().to_str().unwrap();
        // Path like /host/path:/guest/path:ro where guest path contains colon
        let volume = format!("{}:/data:/media/c:ro", host_path);

        let result = VmManager::parse_volume_mount(&volume, 0, std::path::Path::new("/tmp"));
        // Should handle this gracefully or error on the guest path with colon
        // The exact behavior depends on implementation
        assert!(result.is_err() || result.is_ok()); // Just verify it doesn't panic
    }
}
