//! Bounded guest entrypoint configuration staged outside the kernel command line.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Fixed in-guest location of the runtime-owned entrypoint configuration.
pub const RUNTIME_EXEC_CONFIG_PATH: &str = "/.a3s-box-exec.json";
/// Versioned schema written by the runtime and consumed by guest-init.
pub const RUNTIME_EXEC_CONFIG_SCHEMA: &str = "a3s.box.guest-exec.v1";
/// Maximum serialized entrypoint configuration accepted by either side.
pub const MAX_RUNTIME_EXEC_CONFIG_BYTES: usize = 1024 * 1024;
/// Match libkrun's maximum argument pointer count.
pub const MAX_RUNTIME_EXEC_ARGS: usize = 4096;

/// Virtio-fs tag used for the private, runtime-owned MicroVM boot channel.
pub const GUEST_BOOT_CONTROL_TAG: &str = "a3s-boot";
/// Fixed mount point used only while guest-init consumes the boot bundle.
pub const GUEST_BOOT_CONTROL_MOUNT_PATH: &str = "/run/a3s-box/boot";
/// Fixed in-guest location of the versioned MicroVM boot bundle.
pub const GUEST_BOOT_CONFIG_PATH: &str = "/run/a3s-box/boot/config.json";
/// File name stored in the host side of the private boot share.
pub const GUEST_BOOT_CONFIG_FILE_NAME: &str = "config.json";
/// Environment variable selecting the boot-bundle transport.
pub const GUEST_BOOT_CONFIG_ENV: &str = "BOX_BOOT_CONFIG_FILE";
/// Versioned schema written by the runtime and consumed by guest-init.
pub const GUEST_BOOT_CONFIG_SCHEMA: &str = "a3s.box.guest-boot.v1";
/// Maximum serialized boot bundle accepted by either side.
pub const MAX_GUEST_BOOT_CONFIG_BYTES: usize = 4 * 1024 * 1024;
/// Virtio-fs tag for the private terminal-status handoff.
pub const GUEST_TERMINAL_CONTROL_TAG: &str = "a3s-terminal";
/// Temporary guest mount point used only while guest-init opens the status file.
pub const GUEST_TERMINAL_CONTROL_MOUNT_PATH: &str = "/run/a3s-box/terminal";
/// Fixed status file opened before the terminal-control share is unmounted.
pub const GUEST_TERMINAL_STATUS_PATH: &str = "/run/a3s-box/terminal/status.json";
/// Host-side file name within the private terminal-control directory.
pub const GUEST_TERMINAL_STATUS_FILE_NAME: &str = "status.json";
/// Versioned terminal-status contract shared by guest-init and the host runtime.
pub const GUEST_TERMINAL_STATUS_SCHEMA: &str = "a3s.box.guest-terminal.v1";
/// Defensive serialized terminal-status bound.
pub const MAX_GUEST_TERMINAL_STATUS_BYTES: usize = 256;
/// Defensive bound for the number of workload environment entries.
pub const MAX_GUEST_BOOT_ENV_VARS: usize = 16_384;
/// Defensive bound for each generated guest host file.
pub const MAX_GUEST_HOST_FILE_BYTES: usize = 1024 * 1024;
/// Early host-controlled selector for the trusted rootfs maintenance bootstrap.
///
/// The typed boot bundle confirms this value after guest-init mounts and reads
/// the private control share. The early selector exists only so PID 1 can avoid
/// replaying metadata from its temporary maintenance root before that point.
pub const GUEST_ROOTFS_MAINTENANCE_ENV: &str = "A3S_ROOTFS_MAINTENANCE";
/// Exact read-only block device exposed to the maintenance guest.
pub const GUEST_ROOTFS_MAINTENANCE_DEVICE_ENV: &str = "A3S_ROOTFS_MAINTENANCE_DEVICE";
/// Mount point used by the restricted maintenance archive server.
pub const GUEST_ROOTFS_MAINTENANCE_MOUNT_PATH: &str = "/mnt/a3s-rootfs";

/// Host-selected PID 1 behavior carried by the versioned boot bundle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GuestBootMode {
    /// Normal box boot: apply host configuration and launch the workload.
    #[default]
    Workload,
    /// Trusted, workload-free reader for an attached rootfs block device.
    RootfsMaintenance,
}

impl GuestBootMode {
    pub const fn is_workload(&self) -> bool {
        matches!(self, Self::Workload)
    }

    pub const fn is_rootfs_maintenance(self) -> bool {
        matches!(self, Self::RootfsMaintenance)
    }
}

/// Durable workload result written through a private, pre-opened control file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestTerminalStatus {
    pub schema: String,
    pub exit_code: i32,
    /// The guest has flushed its block root and successfully remounted it
    /// read-only. Directory-root transports leave this false because their
    /// lifecycle is owned by the host filesystem provider.
    #[serde(default)]
    pub rootfs_quiesced: bool,
}

impl GuestTerminalStatus {
    pub fn new(exit_code: i32) -> Self {
        Self {
            schema: GUEST_TERMINAL_STATUS_SCHEMA.to_string(),
            exit_code,
            rootfs_quiesced: false,
        }
    }

    /// Mark a terminal result as safe for guest-owned block-disk handoff.
    pub fn with_rootfs_quiesced(mut self) -> Self {
        self.rootfs_quiesced = true;
        self
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != GUEST_TERMINAL_STATUS_SCHEMA {
            return Err(format!(
                "unsupported guest terminal status schema: {}",
                self.schema
            ));
        }
        Ok(())
    }
}

/// Runtime-owned process configuration consumed by guest-init before launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestExecConfig {
    pub schema: String,
    pub executable: String,
    pub args: Vec<String>,
    pub workdir: String,
    pub user: Option<String>,
    pub stdin_null: bool,
}

impl GuestExecConfig {
    pub fn new(
        executable: String,
        args: Vec<String>,
        workdir: String,
        user: Option<String>,
        stdin_null: bool,
    ) -> Self {
        Self {
            schema: RUNTIME_EXEC_CONFIG_SCHEMA.to_string(),
            executable,
            args,
            workdir,
            user,
            stdin_null,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != RUNTIME_EXEC_CONFIG_SCHEMA {
            return Err(format!("unsupported guest exec schema: {}", self.schema));
        }
        if self.executable.is_empty() {
            return Err("guest executable must not be empty".to_string());
        }
        if self.args.len() > MAX_RUNTIME_EXEC_ARGS {
            return Err(format!(
                "guest argument count {} exceeds limit {}",
                self.args.len(),
                MAX_RUNTIME_EXEC_ARGS
            ));
        }
        if self.workdir.is_empty() {
            return Err("guest working directory must not be empty".to_string());
        }
        if self.executable.contains('\0')
            || self.workdir.contains('\0')
            || self.args.iter().any(|argument| argument.contains('\0'))
            || self.user.as_ref().is_some_and(|user| user.contains('\0'))
        {
            return Err("guest exec configuration contains NUL".to_string());
        }
        Ok(())
    }
}

/// Guest-owned files derived from host launch decisions.
///
/// `None` means preserve the image's file. This matters for `/etc/hosts`:
/// images may intentionally provide custom entries when the user did not ask
/// Box to manage that file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GuestHostConfig {
    pub hostname: Option<String>,
    pub resolv_conf: Option<String>,
    pub hosts: Option<String>,
}

impl GuestHostConfig {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(hostname) = self.hostname.as_deref() {
            crate::dns::validate_hostname(hostname)
                .map_err(|error| format!("invalid guest hostname: {error}"))?;
        }
        validate_host_file("resolv.conf", self.resolv_conf.as_deref())?;
        validate_host_file("hosts", self.hosts.as_deref())?;
        Ok(())
    }
}

fn validate_host_file(name: &str, content: Option<&str>) -> Result<(), String> {
    let Some(content) = content else {
        return Ok(());
    };
    if content.len() > MAX_GUEST_HOST_FILE_BYTES {
        return Err(format!(
            "guest {name} is {} bytes; limit is {} bytes",
            content.len(),
            MAX_GUEST_HOST_FILE_BYTES
        ));
    }
    if content.contains('\0') {
        return Err(format!("guest {name} contains NUL"));
    }
    Ok(())
}

/// Complete, versioned control payload consumed once during MicroVM boot.
///
/// Runtime-only controls that must remain on the kernel command line are kept
/// outside this structure. Workload-controlled argv and environment values are
/// carried here so their size cannot overflow the kernel command line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestBootConfig {
    pub schema: String,
    /// Omitted for normal boots so older guest-init binaries keep accepting the
    /// otherwise unchanged v1 payload.
    #[serde(default, skip_serializing_if = "GuestBootMode::is_workload")]
    pub mode: GuestBootMode,
    pub exec: GuestExecConfig,
    pub environment: Vec<(String, String)>,
    #[serde(default)]
    pub host: GuestHostConfig,
}

impl GuestBootConfig {
    pub fn new(
        exec: GuestExecConfig,
        environment: Vec<(String, String)>,
        host: GuestHostConfig,
    ) -> Self {
        Self {
            schema: GUEST_BOOT_CONFIG_SCHEMA.to_string(),
            mode: GuestBootMode::Workload,
            exec,
            environment,
            host,
        }
    }

    /// Build the control payload for the restricted rootfs maintenance guest.
    pub fn rootfs_maintenance() -> Self {
        let mut config = Self::new(
            GuestExecConfig::new(
                "/bin/false".to_string(),
                Vec::new(),
                "/".to_string(),
                None,
                true,
            ),
            Vec::new(),
            GuestHostConfig::default(),
        );
        config.mode = GuestBootMode::RootfsMaintenance;
        config
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != GUEST_BOOT_CONFIG_SCHEMA {
            return Err(format!("unsupported guest boot schema: {}", self.schema));
        }
        self.exec.validate()?;
        if self.environment.len() > MAX_GUEST_BOOT_ENV_VARS {
            return Err(format!(
                "guest environment count {} exceeds limit {}",
                self.environment.len(),
                MAX_GUEST_BOOT_ENV_VARS
            ));
        }

        let mut names = BTreeSet::new();
        for (name, value) in &self.environment {
            if name.is_empty() || name.contains('=') || name.contains('\0') {
                return Err(format!("invalid guest environment name {name:?}"));
            }
            if value.contains('\0') {
                return Err(format!("guest environment value for {name:?} contains NUL"));
            }
            if !names.insert(name.as_str()) {
                return Err(format!("duplicate guest environment name {name:?}"));
            }
        }
        self.host.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_exec_config_validates_schema_bounds_and_nul() {
        let valid = GuestExecConfig::new(
            "/bin/sh".to_string(),
            vec!["-c".to_string(), "printf ok".to_string()],
            "/".to_string(),
            Some("123:456".to_string()),
            true,
        );
        valid.validate().unwrap();

        let mut wrong_schema = valid.clone();
        wrong_schema.schema = "a3s.box.guest-exec.v2".to_string();
        assert!(wrong_schema.validate().is_err());

        let mut too_many = valid.clone();
        too_many.args = vec![String::new(); MAX_RUNTIME_EXEC_ARGS + 1];
        assert!(too_many.validate().is_err());

        let mut nul = valid;
        nul.args.push("bad\0arg".to_string());
        assert!(nul.validate().is_err());
    }

    #[test]
    fn guest_terminal_status_round_trips_and_rejects_unknown_schema() {
        let status = GuestTerminalStatus::new(137);
        status.validate().unwrap();
        assert!(!status.rootfs_quiesced);
        let encoded = serde_json::to_vec(&status).unwrap();
        assert!(encoded.len() <= MAX_GUEST_TERMINAL_STATUS_BYTES);
        assert_eq!(
            serde_json::from_slice::<GuestTerminalStatus>(&encoded).unwrap(),
            status
        );
        assert!(
            GuestTerminalStatus::new(137)
                .with_rootfs_quiesced()
                .rootfs_quiesced
        );

        let legacy = serde_json::from_slice::<GuestTerminalStatus>(
            br#"{"schema":"a3s.box.guest-terminal.v1","exit_code":0}"#,
        )
        .unwrap();
        assert!(!legacy.rootfs_quiesced);

        let mut unsupported = status;
        unsupported.schema = "a3s.box.guest-terminal.v2".to_string();
        assert!(unsupported.validate().is_err());
    }

    fn valid_boot_config() -> GuestBootConfig {
        GuestBootConfig::new(
            GuestExecConfig::new(
                "/bin/sh".to_string(),
                vec!["-c".to_string(), "printf ok".to_string()],
                "/".to_string(),
                None,
                false,
            ),
            vec![("PATH".to_string(), "/bin".to_string())],
            GuestHostConfig {
                hostname: Some("web".to_string()),
                resolv_conf: Some("nameserver 1.1.1.1\n".to_string()),
                hosts: Some("127.0.0.1 localhost\n".to_string()),
            },
        )
    }

    #[test]
    fn guest_boot_config_round_trips_and_validates() {
        let config = valid_boot_config();
        config.validate().unwrap();

        let bytes = serde_json::to_vec(&config).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("\"mode\""));
        let decoded: GuestBootConfig = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(decoded, config);
    }

    #[test]
    fn guest_boot_config_defaults_legacy_payloads_to_workload_mode() {
        let mut value = serde_json::to_value(valid_boot_config()).unwrap();
        value.as_object_mut().unwrap().remove("mode");

        let decoded: GuestBootConfig = serde_json::from_value(value).unwrap();

        assert_eq!(decoded.mode, GuestBootMode::Workload);
    }

    #[test]
    fn rootfs_maintenance_mode_is_explicit_in_the_boot_bundle() {
        let config = GuestBootConfig::rootfs_maintenance();
        config.validate().unwrap();

        let value = serde_json::to_value(&config).unwrap();

        assert_eq!(value["mode"], "rootfs-maintenance");
        assert_eq!(config.exec.executable, "/bin/false");
        assert!(config.environment.is_empty());
    }

    #[test]
    fn guest_boot_config_rejects_duplicate_environment_and_invalid_host_data() {
        let mut duplicate = valid_boot_config();
        duplicate
            .environment
            .push(("PATH".to_string(), "/usr/bin".to_string()));
        assert!(duplicate.validate().unwrap_err().contains("duplicate"));

        let mut invalid_name = valid_boot_config();
        invalid_name.environment[0].0 = "BAD=NAME".to_string();
        assert!(invalid_name.validate().unwrap_err().contains("invalid"));

        let mut invalid_hostname = valid_boot_config();
        invalid_hostname.host.hostname = Some("bad_host".to_string());
        assert!(invalid_hostname
            .validate()
            .unwrap_err()
            .contains("hostname"));

        let mut nul = valid_boot_config();
        nul.host.hosts = Some("bad\0hosts".to_string());
        assert!(nul.validate().unwrap_err().contains("NUL"));
    }
}
