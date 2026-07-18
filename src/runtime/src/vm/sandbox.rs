//! `crun` Sandbox boot path.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::event::BoxEvent;
use a3s_box_core::execution::ResolvedExecutionPlan;
use base64::Engine;
use sha2::{Digest, Sha256};

use crate::sandbox::{
    compile_oci_spec, inspect_rootfs_identity_requirements, plan_id_mappings,
    prepare_crun_path_access, prepare_managed_mount_source, prepare_rootfs_ownership,
    probe_sandbox_capabilities, validate_external_mount_access, write_bundle, CrunController,
    SandboxBundleSpec, SandboxLaunchSpec, SandboxMount, SandboxResources, SandboxTmpfs,
};

use super::{BoxState, VmManager};

impl VmManager {
    pub(super) async fn boot_sandbox(
        &mut self,
        execution_plan: ResolvedExecutionPlan,
        boot_span: &tracing::Span,
        boot_start: std::time::Instant,
    ) -> Result<()> {
        // This probe is deliberately before image pulls, rootfs mounts, volume
        // creation, or bundle writes. Every mandatory control is fail-closed.
        let capability_start = std::time::Instant::now();
        let capabilities = probe_sandbox_capabilities(None);
        capabilities.require_ready()?;
        let runtime = capabilities
            .runtime
            .clone()
            .ok_or_else(|| BoxError::BoxBootError {
                message: "Sandbox capability probe did not return a certified crun artifact"
                    .to_string(),
                hint: None,
            })?;
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
        let runtime_root = self.home_dir.join("run").join("crun").join(&self.box_id);
        let runtime_record = sandbox_dir.join("runtime.json");
        let controller = CrunController::new(runtime.clone());
        controller.require_absent(&runtime_root, &self.box_id)?;

        tracing::info!(
            parent: boot_span,
            box_id = %self.box_id,
            isolation_class = "shared-kernel",
            runtime = %runtime.path.display(),
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
            let instance_prepare_start = std::time::Instant::now();
            let resolv_content = a3s_box_core::dns::generate_resolv_conf(&self.config.dns);
            std::fs::write(layout.rootfs_path.join("etc/resolv.conf"), resolv_content)
                .map_err(BoxError::IoError)?;
            self.write_hostname_file(&layout)?;
            self.write_standalone_hosts_file(&layout)?;

            let instance_spec = self.build_instance_spec(&layout)?;
            if !matches!(
                instance_spec.entrypoint.executable.as_str(),
                "/sbin/init" | "/usr/sbin/init"
            ) || !instance_spec
                .entrypoint
                .env
                .iter()
                .any(|(key, _)| key == "BOX_EXEC_EXEC")
            {
                return Err(BoxError::BoxBootError {
                    message: "Sandbox requires the packaged a3s-box guest init as OCI PID 1"
                        .to_string(),
                    hint: Some("Install the matching a3s-box-guest-init artifact".to_string()),
                });
            }

            let (mounts, tmpfs) = self.compile_sandbox_mounts(&layout, &instance_spec)?;
            ensure_mount_destinations(&layout.rootfs_path, &mounts, &tmpfs)?;

            let rootfs_ids = inspect_rootfs_identity_requirements(&layout.rootfs_path)?;
            let (account_uid, account_gid) = maximum_account_ids(&layout.rootfs_path)?;
            let (process_uid, process_gid) = maximum_process_ids(&instance_spec.entrypoint.env)?;
            let maximum_uid = rootfs_ids.maximum_uid.max(account_uid).max(process_uid);
            let maximum_gid = rootfs_ids.maximum_gid.max(account_gid).max(process_gid);
            let id_mappings = plan_id_mappings(user_namespace, maximum_uid, maximum_gid)?;
            a3s_box_core::lifecycle_profile::record_lifecycle_phase(
                "sandbox.instance_prepare",
                instance_prepare_start.elapsed(),
            );

            let mount_sources_start = std::time::Instant::now();
            self.prepare_sandbox_mount_sources(&layout, &mounts, &id_mappings)?;
            a3s_box_core::lifecycle_profile::record_lifecycle_phase(
                "sandbox.mount_sources",
                mount_sources_start.elapsed(),
            );
            let rootfs_ownership_start = std::time::Instant::now();
            prepare_rootfs_ownership(
                &layout.rootfs_path,
                &id_mappings,
                user_namespace.effective_uid,
                self.config.read_only,
            )?;
            a3s_box_core::lifecycle_profile::record_lifecycle_phase(
                "sandbox.rootfs_ownership",
                rootfs_ownership_start.elapsed(),
            );

            let bundle_start = std::time::Instant::now();
            let resources = SandboxResources::from_box_config(&self.config)?;
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
                init_environment: instance_spec.entrypoint.env.clone(),
                mounts,
                tmpfs,
                id_mappings,
                resources,
                requested_capabilities: self.config.cap_add.clone(),
                execution_plan_digest,
                runtime_digest: format!("sha256:{}", runtime.sha256),
            };
            let oci_spec = compile_oci_spec(&bundle_spec)?;
            write_bundle(&bundle_dir, &oci_spec, &execution_plan, &capabilities)?;
            prepare_crun_path_access(
                &self.home_dir,
                &self.box_id,
                &bundle_dir,
                &layout.rootfs_path,
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
        let handler = match controller.start(launch).await {
            Ok(handler) => handler,
            Err(error) => {
                self.cleanup_boot_failure().await;
                return Err(error);
            }
        };
        a3s_box_core::lifecycle_profile::record_lifecycle_phase(
            "sandbox.launch",
            launch_start.elapsed(),
        );
        *self.handler.write().await = Some(Box::new(handler));

        let readiness_start = std::time::Instant::now();
        if let Err(error) = async {
            // CrunController::start only returns after the certified runtime
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
        mounts: &[SandboxMount],
        id_mappings: &crate::sandbox::SandboxIdMappingPlan,
    ) -> Result<()> {
        let managed = self.managed_sandbox_mount_sources(&layout.workspace_path, mounts)?;

        for mount in mounts {
            if managed.contains(&mount.source) {
                prepare_managed_mount_source(&mount.source, id_mappings)?;
            } else {
                validate_external_mount_access(&mount.source, id_mappings, mount.read_only)?;
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
    let path = PathBuf::from(value);
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::CurDir | Component::ParentDir | Component::Prefix(_)
            )
        })
    {
        return Err(BoxError::ConfigError(format!(
            "Sandbox {label} must be an absolute normalized path: {value:?}"
        )));
    }
    Ok(path)
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

fn maximum_account_ids(rootfs: &Path) -> Result<(u32, u32)> {
    let mut maximum_uid = 0u32;
    let mut maximum_gid = 0u32;
    if let Ok(passwd) = std::fs::read_to_string(rootfs.join("etc/passwd")) {
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
    if let Ok(group) = std::fs::read_to_string(rootfs.join("etc/group")) {
        for line in group.lines().filter(|line| !line.starts_with('#')) {
            if let Some(Ok(gid)) = line.split(':').nth(2).map(str::parse::<u32>) {
                maximum_gid = maximum_gid.max(gid);
            }
        }
    }
    Ok((maximum_uid, maximum_gid))
}

fn maximum_process_ids(environment: &[(String, String)]) -> Result<(u32, u32)> {
    let Some(encoded) = environment
        .iter()
        .find_map(|(key, value)| (key == "BOX_EXEC_USER").then_some(value))
    else {
        return Ok((0, 0));
    };
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|error| BoxError::ConfigError(format!("Invalid encoded Sandbox user: {error}")))?;
    let user = String::from_utf8(bytes)
        .map_err(|error| BoxError::ConfigError(format!("Sandbox user is not UTF-8: {error}")))?;
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

#[cfg(test)]
mod tests {
    use a3s_box_core::{volume::VolumeConfig, BoxConfig, EventEmitter};

    use super::*;

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
    }

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
