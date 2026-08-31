//! Trusted, read-only maintenance MicroVM for stopped guest-native rootfs data.

use super::*;

#[cfg(target_os = "macos")]
use a3s_box_core::guest_exec::{
    GuestBootConfig, GUEST_BOOT_CONFIG_ENV, GUEST_BOOT_CONFIG_PATH,
    GUEST_ROOTFS_MAINTENANCE_DEVICE_ENV, GUEST_ROOTFS_MAINTENANCE_ENV,
};
#[cfg(target_os = "macos")]
use a3s_box_core::vmm::{
    Entrypoint, InstanceSpec, RawBlockDevice, RootfsSource, GUEST_EXT4_ROOT_DEVICE,
};

/// Stream a stopped guest-native rootfs through a one-shot trusted maintenance
/// VM and always tear that VM down before returning.
///
/// The user disk is an auxiliary raw block device opened read-only. PID 1 and
/// the archive implementation come from the current A3S installation, not from
/// the mutable filesystem being inspected.
pub async fn archive_stopped_guest_native_rootfs<W>(
    config: BoxConfig,
    box_id: String,
    output: &mut W,
) -> Result<u64>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (config, box_id, output);
        Err(BoxError::StateError(
            "Guest-native rootfs maintenance is currently available only on macOS".to_string(),
        ))
    }

    #[cfg(target_os = "macos")]
    {
        let mut config = config;
        sanitize_maintenance_config(&mut config);
        let mut vm = VmManager::with_box_id(config, EventEmitter::new(16), box_id);
        let maintenance_dir = vm.socket_dir().join("rootfs-maintenance");
        vm.boot_rootfs_maintenance().await?;
        let socket = maintenance_dir.join("exec.sock");
        let client = ExecClient::for_socket(&socket);
        let archive_result = client
            .archive_rootfs(output, false)
            .await
            .map_err(|error| maintenance_error(error, &maintenance_dir));
        let shutdown_result = vm.destroy_with_timeout(5_000).await;

        match (archive_result, shutdown_result) {
            (Ok(written), Ok(())) => Ok(written),
            (Err(archive_error), Ok(())) => Err(archive_error),
            (Ok(_), Err(shutdown_error)) => Err(shutdown_error),
            (Err(archive_error), Err(shutdown_error)) => Err(BoxError::StateError(format!(
                "Rootfs maintenance archive failed ({archive_error}); teardown also failed ({shutdown_error})"
            ))),
        }
    }
}

#[cfg(target_os = "macos")]
fn sanitize_maintenance_config(config: &mut BoxConfig) {
    config.isolation = a3s_box_core::ExecutionIsolation::Microvm;
    config.workspace = PathBuf::new();
    config.volumes.clear();
    config.extra_env.clear();
    config.port_map.clear();
    config.dns.clear();
    config.add_hosts.clear();
    config.network = a3s_box_core::NetworkMode::None;
    config.tmpfs.clear();
    config.resource_limits = a3s_box_core::ResourceLimits::default();
    config.cap_add.clear();
    config.cap_drop.clear();
    config.security_opt.clear();
    config.sysctls.clear();
    config.privileged = false;
    config.sidecar = None;
    config.tee = a3s_box_core::config::TeeConfig::None;
    config.deferred_main = false;
    config.snapshot_mem_file = None;
    config.snapshot_sock = None;
    config.restore_from = None;
    config.persistent = true;
}

#[cfg(target_os = "macos")]
impl VmManager {
    async fn boot_rootfs_maintenance(&mut self) -> Result<()> {
        if *self.state.read().await != BoxState::Created {
            return Err(BoxError::StateError(
                "Rootfs maintenance VM was already booted".to_string(),
            ));
        }
        self.boot_mode = VmBootMode::RootfsMaintenance;
        self.preserve_rootfs_on_boot_failure = true;

        let box_dir = self.home_dir.join("boxes").join(&self.box_id);
        let disk = crate::rootfs::guest_native_ext4_maintenance_disk(&box_dir)?;
        let socket_dir = self.socket_dir();
        let maintenance_dir = socket_dir.join("rootfs-maintenance");
        let root = prepare_trusted_maintenance_root(&maintenance_dir)?;
        let exec_socket = maintenance_dir.join("exec.sock");
        let boot = GuestBootConfig::rootfs_maintenance();
        let boot_mount = super::spec::guest_control::stage_guest_boot_config_in_runtime_dir(
            &maintenance_dir,
            &boot,
        )?;
        let spec = InstanceSpec {
            box_id: self.box_id.clone(),
            vcpus: 1,
            memory_mib: self.config.resources.memory_mb.clamp(256, 512),
            rootfs: RootfsSource::directory(root),
            block_devices: vec![RawBlockDevice::new("a3s-rootfs", disk, true)],
            exec_socket_path: exec_socket.clone(),
            pty_socket_path: PathBuf::new(),
            attest_socket_path: PathBuf::new(),
            port_forward_socket_path: PathBuf::new(),
            fs_mounts: vec![boot_mount],
            entrypoint: Entrypoint {
                executable: "/sbin/init".to_string(),
                args: Vec::new(),
                env: vec![
                    (
                        GUEST_BOOT_CONFIG_ENV.to_string(),
                        GUEST_BOOT_CONFIG_PATH.to_string(),
                    ),
                    (GUEST_ROOTFS_MAINTENANCE_ENV.to_string(), "1".to_string()),
                    (
                        GUEST_ROOTFS_MAINTENANCE_DEVICE_ENV.to_string(),
                        GUEST_EXT4_ROOT_DEVICE.to_string(),
                    ),
                ],
            },
            console_output: Some(maintenance_dir.join("logs/console.log")),
            workdir: "/".to_string(),
            tee_config: None,
            port_map: Vec::new(),
            user: None,
            network: None,
            disable_tsi: true,
            resource_limits: a3s_box_core::ResourceLimits::default(),
            log_config: a3s_box_core::log::LogConfig::default(),
            ksm: false,
            snapshot_mem_file: None,
            snapshot_sock: None,
            restore_from: None,
        };

        if self.provider.is_none() {
            let shim = VmController::find_shim()?;
            self.provider = Some(Box::new(VmController::new(shim)?));
        }
        let start_result = self
            .provider
            .as_ref()
            .ok_or_else(|| BoxError::StateError("VMM provider is missing".to_string()))?
            .start(&spec)
            .await;
        let handler = match start_result {
            Ok(handler) => handler,
            Err(error) => {
                self.cleanup_boot_failure().await;
                return Err(error);
            }
        };
        *self.handler.write().await = Some(handler);

        if let Err(error) = async {
            self.wait_for_vm_running().await?;
            self.wait_for_rootfs_maintenance_ready(&exec_socket, &maintenance_dir)
                .await?;
            Ok::<(), BoxError>(())
        }
        .await
        {
            self.cleanup_boot_failure().await;
            return Err(error);
        }
        if let Err(error) = Self::clear_microvm_guest_boot_config(&spec) {
            self.cleanup_boot_failure().await;
            return Err(error);
        }

        self.exec_socket_path = Some(exec_socket);
        self.pty_socket_path = None;
        self.port_forward_socket_path = None;
        *self.state.write().await = BoxState::Ready;
        tracing::info!(box_id = %self.box_id, "Rootfs maintenance VM ready");
        Ok(())
    }

    async fn wait_for_rootfs_maintenance_ready(
        &mut self,
        exec_socket: &Path,
        maintenance_dir: &Path,
    ) -> Result<()> {
        const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
        const HEARTBEAT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);
        const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        loop {
            let provider_exit = {
                let mut handler = self.handler.write().await;
                let handler = handler.as_mut().ok_or_else(|| {
                    BoxError::StateError(
                        "Rootfs maintenance VM lost its provider handle during boot".to_string(),
                    )
                })?;
                if let Some(code) = handler.try_wait_exit()? {
                    Some(format!("shim exited with status {code}"))
                } else if handler.has_exited() || !handler.is_running() {
                    Some("shim exited without a collectable status".to_string())
                } else {
                    None
                }
            };
            if let Some(provider_exit) = provider_exit {
                return Err(maintenance_error(
                    format!("Rootfs maintenance guest exited before readiness: {provider_exit}"),
                    maintenance_dir,
                ));
            }

            let client = ExecClient::for_socket(exec_socket);
            if matches!(
                tokio::time::timeout(HEARTBEAT_TIMEOUT, client.heartbeat()).await,
                Ok(Ok(true))
            ) {
                self.exec_client = Some(client);
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(maintenance_error(
                    format!(
                        "Rootfs maintenance guest did not become ready within {} seconds",
                        READY_TIMEOUT.as_secs()
                    ),
                    maintenance_dir,
                ));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

#[cfg(target_os = "macos")]
fn prepare_trusted_maintenance_root(maintenance_dir: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    remove_path_no_follow(maintenance_dir)?;
    let root = maintenance_dir.join("root");
    for directory in ["dev", "proc", "sys", "run", "tmp", "sbin", "mnt/a3s-rootfs"] {
        std::fs::create_dir_all(root.join(directory)).map_err(BoxError::IoError)?;
    }
    std::fs::set_permissions(maintenance_dir, std::fs::Permissions::from_mode(0o700))
        .map_err(BoxError::IoError)?;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755))
        .map_err(BoxError::IoError)?;
    std::fs::set_permissions(root.join("tmp"), std::fs::Permissions::from_mode(0o1777))
        .map_err(BoxError::IoError)?;

    let source = VmManager::find_guest_init()?;
    let destination = root.join("sbin/init");
    std::fs::copy(&source, &destination).map_err(|error| BoxError::BoxBootError {
        message: format!(
            "Failed to stage trusted guest-init {} at {}: {error}",
            source.display(),
            destination.display()
        ),
        hint: None,
    })?;
    std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o555))
        .map_err(BoxError::IoError)?;
    root.canonicalize().map_err(BoxError::IoError)
}

#[cfg(target_os = "macos")]
fn remove_path_no_follow(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            std::fs::remove_dir_all(path).map_err(BoxError::IoError)
        }
        Ok(_) => std::fs::remove_file(path).map_err(BoxError::IoError),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(BoxError::IoError(error)),
    }
}

#[cfg(target_os = "macos")]
fn maintenance_error(error: impl std::fmt::Display, maintenance_dir: &Path) -> BoxError {
    let diagnostics = maintenance_diagnostics(maintenance_dir);
    let message = if diagnostics.is_empty() {
        error.to_string()
    } else {
        format!("{error}\nRootfs maintenance diagnostics:\n{diagnostics}")
    };
    BoxError::StateError(message)
}

#[cfg(target_os = "macos")]
fn maintenance_diagnostics(maintenance_dir: &Path) -> String {
    const MAX_LOG_TAIL_BYTES: u64 = 16 * 1024;
    let mut sections = Vec::new();
    for relative in [
        "logs/shim.stderr.log",
        "logs/console.err.log",
        "logs/console.log",
    ] {
        let path = maintenance_dir.join(relative);
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            continue;
        }
        let Ok(mut file) = std::fs::File::open(&path) else {
            continue;
        };
        use std::io::{Read, Seek, SeekFrom};
        let start = metadata.len().saturating_sub(MAX_LOG_TAIL_BYTES);
        if file.seek(SeekFrom::Start(start)).is_err() {
            continue;
        }
        let mut bytes = Vec::with_capacity((metadata.len() - start) as usize);
        if file.read_to_end(&mut bytes).is_err() || bytes.is_empty() {
            continue;
        }
        sections.push(format!(
            "[{relative}]\n{}",
            String::from_utf8_lossy(&bytes).trim()
        ));
    }
    sections.join("\n")
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn maintenance_config_removes_every_workload_attachment() {
        let mut config = BoxConfig {
            volumes: vec!["/host:/guest".to_string()],
            extra_env: vec![("SECRET".to_string(), "value".to_string())],
            port_map: vec!["8080:80".to_string()],
            network: a3s_box_core::NetworkMode::Bridge {
                network: "default".to_string(),
            },
            privileged: true,
            persistent: false,
            ..BoxConfig::default()
        };

        sanitize_maintenance_config(&mut config);

        assert!(config.volumes.is_empty());
        assert!(config.extra_env.is_empty());
        assert!(config.port_map.is_empty());
        assert_eq!(config.network, a3s_box_core::NetworkMode::None);
        assert!(!config.privileged);
        assert!(config.persistent);
    }
}
