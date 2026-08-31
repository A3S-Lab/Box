//! VMM contract — types and traits for pluggable VM backends.
//!
//! All types here are pure data (no runtime dependencies). This lets
//! third-party VMM implementors depend only on `a3s-box-core` rather
//! than pulling in the full `a3s-box-runtime`.
//!
//! # Extension points
//!
//! - [`VmmProvider`] — start VMs from an [`InstanceSpec`]
//! - [`VmHandler`] — lifecycle operations on a running VM

use std::fmt;
use std::net::Ipv4Addr;
#[cfg(unix)]
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::{ResourceLimits, DEFAULT_VCPUS};
use crate::error::Result;

// ── VM instance spec ──────────────────────────────────────────────────────────

/// A filesystem mount from host to guest via virtio-fs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsMount {
    /// Virtiofs tag (guest uses this to identify the share)
    pub tag: String,
    /// Host directory to share
    pub host_path: PathBuf,
    /// Whether the share is read-only
    pub read_only: bool,
}

/// Entrypoint configuration for the guest agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entrypoint {
    /// Path to the executable inside the VM
    pub executable: String,
    /// Command-line arguments
    pub args: Vec<String>,
    /// Environment variables
    pub env: Vec<(String, String)>,
}

/// TEE instance configuration for the shim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeeInstanceConfig {
    /// Path to TEE configuration JSON file
    pub config_path: PathBuf,
    /// TEE type identifier (e.g., "snp")
    pub tee_type: String,
}

/// Network instance configuration for the network backend (passt on Linux, gvproxy on macOS).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkInstanceConfig {
    /// Path to the network backend Unix socket (passt on Linux, gvproxy on macOS).
    pub net_socket_path: PathBuf,

    /// Optional JSON stats file written by the userspace network backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_stats_path: Option<PathBuf>,

    /// Pre-opened network socket fd inherited by the shim on Unix.
    #[cfg(unix)]
    #[serde(default)]
    pub net_socket_fd: Option<RawFd>,

    /// Proxy-side network socket fd inherited by the shim on Unix.
    #[cfg(unix)]
    #[serde(default)]
    pub net_proxy_fd: Option<RawFd>,

    /// Shared Unix-datagram Ethernet switch directory for this bridge network.
    #[cfg(unix)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge_socket_dir: Option<PathBuf>,

    /// Assigned IPv4 address for this VM.
    pub ip_address: Ipv4Addr,

    /// Gateway IPv4 address.
    pub gateway: Ipv4Addr,

    /// Subnet prefix length (e.g., 24).
    pub prefix_len: u8,

    /// MAC address as 6 bytes.
    pub mac_address: [u8; 6],

    /// DNS servers to configure inside the guest.
    #[serde(default)]
    pub dns_servers: Vec<Ipv4Addr>,
}

/// Stable guest device path used for an A3S-managed ext4 root disk.
pub const GUEST_EXT4_ROOT_DEVICE: &str = "/dev/vda";

/// An explicitly raw auxiliary block device attached to a MicroVM.
///
/// The format is part of the type contract, so the shim never probes a path
/// after guest access. Device order is stable: root block disks are attached
/// first, followed by this list in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawBlockDevice {
    pub id: String,
    pub path: PathBuf,
    #[serde(default)]
    pub read_only: bool,
}

impl RawBlockDevice {
    pub fn new(id: impl Into<String>, path: impl Into<PathBuf>, read_only: bool) -> Self {
        Self {
            id: id.into(),
            path: path.into(),
            read_only,
        }
    }
}

/// Root filesystem presented to a MicroVM.
///
/// Directory roots use libkrun's virtio-fs root transport. `Ext4Disk` is the
/// guest-native path: an explicitly raw ext4 image is attached as the first
/// virtio-blk device and becomes `/dev/vda` inside the guest. Keeping the disk
/// format fixed in this typed variant prevents unsafe image-format probing
/// after an untrusted guest has written to the image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RootfsSource {
    /// Host directory exported to the guest through virtio-fs.
    Directory { path: PathBuf },
    /// Raw ext4 filesystem image opened directly by libkrun as virtio-blk.
    Ext4Disk {
        path: PathBuf,
        #[serde(default)]
        read_only: bool,
    },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TaggedRootfsSource {
    Directory {
        path: PathBuf,
    },
    Ext4Disk {
        path: PathBuf,
        #[serde(default)]
        read_only: bool,
    },
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RootfsSourceRepresentation {
    LegacyDirectory(PathBuf),
    Tagged(TaggedRootfsSource),
}

impl<'de> Deserialize<'de> for RootfsSource {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(
            match RootfsSourceRepresentation::deserialize(deserializer)? {
                RootfsSourceRepresentation::LegacyDirectory(path) => Self::Directory { path },
                RootfsSourceRepresentation::Tagged(TaggedRootfsSource::Directory { path }) => {
                    Self::Directory { path }
                }
                RootfsSourceRepresentation::Tagged(TaggedRootfsSource::Ext4Disk {
                    path,
                    read_only,
                }) => Self::Ext4Disk { path, read_only },
            },
        )
    }
}

impl RootfsSource {
    /// Construct a host-directory root exported through virtio-fs.
    pub fn directory(path: impl Into<PathBuf>) -> Self {
        Self::Directory { path: path.into() }
    }

    /// Construct an explicitly raw ext4 root disk.
    pub fn ext4_disk(path: impl Into<PathBuf>, read_only: bool) -> Self {
        Self::Ext4Disk {
            path: path.into(),
            read_only,
        }
    }

    /// Host path backing this root filesystem.
    pub fn path(&self) -> &Path {
        match self {
            Self::Directory { path } | Self::Ext4Disk { path, .. } => path,
        }
    }

    /// Return the host directory for operations that require direct file access.
    pub fn directory_path(&self) -> Option<&Path> {
        match self {
            Self::Directory { path } => Some(path),
            Self::Ext4Disk { .. } => None,
        }
    }
}

impl Default for RootfsSource {
    fn default() -> Self {
        Self::directory(PathBuf::new())
    }
}

impl fmt::Display for RootfsSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Directory { path } => write!(formatter, "directory:{}", path.display()),
            Self::Ext4Disk { path, .. } => write!(formatter, "ext4-disk:{}", path.display()),
        }
    }
}

/// Complete configuration for a VM instance.
///
/// Serialized and passed to the shim subprocess, which uses it to configure
/// and start the VM via the underlying hypervisor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceSpec {
    /// Unique identifier for this box instance
    pub box_id: String,

    /// Number of vCPUs (platform default: 1 on Windows, 2 elsewhere)
    pub vcpus: u8,

    /// Memory in MiB (default: 512)
    pub memory_mib: u32,

    /// Root filesystem transport and backing path.
    #[serde(alias = "rootfs_path")]
    pub rootfs: RootfsSource,

    /// Additional raw block devices, attached after any block-backed root.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub block_devices: Vec<RawBlockDevice>,

    /// Path to the Unix socket for exec communication
    pub exec_socket_path: PathBuf,

    /// Path to the Unix socket for PTY communication
    #[serde(default)]
    pub pty_socket_path: PathBuf,

    /// Path to the Unix socket for TEE attestation communication
    #[serde(default)]
    pub attest_socket_path: PathBuf,

    /// Path to the Unix socket for CRI port-forward control
    #[serde(default)]
    pub port_forward_socket_path: PathBuf,

    /// Filesystem mounts (virtio-fs shares)
    pub fs_mounts: Vec<FsMount>,

    /// Guest agent entrypoint
    pub entrypoint: Entrypoint,

    /// Mark guest memory KSM-mergeable (host page dedup across same-image VMs;
    /// Linux 6.4+, requires /sys/kernel/mm/ksm/run=1 on the host).
    #[serde(default)]
    pub ksm: bool,

    /// Snapshot-fork (per-VM): file-backed guest RAM path. When set (with
    /// `snapshot_sock`), this VM boots as a snapshot TEMPLATE — guest RAM is
    /// file-backed so it can be snapshotted on demand.
    #[serde(default)]
    pub snapshot_mem_file: Option<String>,

    /// Snapshot-fork (per-VM): unix socket on which libkrun serves snapshot
    /// requests for this template VM.
    #[serde(default)]
    pub snapshot_sock: Option<String>,

    /// Snapshot-fork (per-VM): when set (with `snapshot_mem_file`), this VM is a
    /// RESTORE — it resumes the snapshotted template from this state file with
    /// MAP_PRIVATE CoW of the RAM file, instead of cold-booting. This is the
    /// per-VM seam that lets one process fork many VMs (the pool / fork daemon),
    /// which a process-global `KRUN_RESTORE_FROM` env cannot express.
    #[serde(default)]
    pub restore_from: Option<String>,

    /// Optional console output file path
    pub console_output: Option<PathBuf>,

    /// Working directory inside the VM
    pub workdir: String,

    /// TEE configuration (None for standard VM)
    pub tee_config: Option<TeeInstanceConfig>,

    /// TSI port mappings: ["host_port:guest_port", ...]
    #[serde(default)]
    pub port_map: Vec<String>,

    /// User to run as inside the VM (from OCI USER directive).
    /// Format: "uid", "uid:gid", "user", or "user:group"
    #[serde(default)]
    pub user: Option<String>,

    /// Network configuration for virtio-net networking.
    /// None = TSI mode (default), Some = virtio-net mode (passt on Linux, gvproxy on macOS).
    #[serde(default)]
    pub network: Option<NetworkInstanceConfig>,

    /// Disable TSI socket interception while retaining explicit vsock IPC.
    #[serde(default)]
    pub disable_tsi: bool,

    /// Resource limits (PID limits, CPU pinning, ulimits, cgroup controls).
    #[serde(default)]
    pub resource_limits: ResourceLimits,

    /// Logging driver config. The shim runs the log processor for the box's
    /// lifetime (so detached `run -d` logs aren't truncated when the CLI exits).
    #[serde(default)]
    pub log_config: crate::log::LogConfig,
}

impl Default for InstanceSpec {
    fn default() -> Self {
        Self {
            box_id: String::new(),
            vcpus: DEFAULT_VCPUS as u8,
            memory_mib: 512,
            rootfs: RootfsSource::default(),
            block_devices: Vec::new(),
            exec_socket_path: PathBuf::new(),
            pty_socket_path: PathBuf::new(),
            attest_socket_path: PathBuf::new(),
            port_forward_socket_path: PathBuf::new(),
            fs_mounts: Vec::new(),
            entrypoint: Entrypoint {
                executable: String::new(),
                args: Vec::new(),
                env: Vec::new(),
            },
            ksm: false,
            snapshot_mem_file: None,
            snapshot_sock: None,
            restore_from: None,
            console_output: None,
            workdir: "/".to_string(),
            tee_config: None,
            port_map: Vec::new(),
            user: None,
            network: None,
            disable_tsi: false,
            resource_limits: ResourceLimits::default(),
            log_config: crate::log::LogConfig::default(),
        }
    }
}

// ── VM handler and metrics ────────────────────────────────────────────────────

/// VM resource metrics.
#[derive(Debug, Clone, Default)]
pub struct VmMetrics {
    /// CPU usage percentage (0-100 per core)
    pub cpu_percent: Option<f32>,
    /// Memory usage in bytes
    pub memory_bytes: Option<u64>,
}

/// Default shutdown timeout in milliseconds (10 seconds).
pub const DEFAULT_SHUTDOWN_TIMEOUT_MS: u64 = 10_000;

/// Parse a POSIX signal name or number string to a signal number.
///
/// Accepts "SIGTERM", "TERM", "15", "SIGQUIT", etc.
/// Returns `SIGTERM` (15) for unrecognized names.
pub fn parse_signal_name(name: &str) -> i32 {
    let upper = name.trim().to_uppercase();
    let short = upper.strip_prefix("SIG").unwrap_or(&upper);
    match short {
        "HUP" => 1,
        "INT" => 2,
        "QUIT" => 3,
        "ILL" => 4,
        "ABRT" => 6,
        "FPE" => 8,
        "KILL" => 9,
        "USR1" => 10,
        "SEGV" => 11,
        "USR2" => 12,
        "PIPE" => 13,
        "ALRM" | "ALARM" => 14,
        "TERM" => 15,
        "CHLD" | "CLD" => 17,
        "CONT" => 18,
        "STOP" => 19,
        "TSTP" => 20,
        "WINCH" => 28,
        _ => name.trim().parse::<i32>().unwrap_or(15),
    }
}

/// Lifecycle operations on a running VM.
///
/// Separates runtime operations (stop, metrics) from spawning (VmmProvider).
/// Allows reconnecting to existing VMs by constructing a handler from a PID.
pub trait VmHandler: Send + Sync {
    /// Stop the VM. Sends `signal` first, then SIGKILL after `timeout_ms`.
    fn stop(&mut self, signal: i32, timeout_ms: u64) -> Result<()>;

    /// Get current CPU and memory metrics.
    fn metrics(&self) -> VmMetrics;

    /// Check if the VM process is still alive.
    fn is_running(&self) -> bool;

    /// Whether the VM process has exited, treating a zombie (an exited child not
    /// yet reaped by its parent) as exited.
    ///
    /// Distinct from `!is_running()`: shim handlers implement `is_running` with
    /// `kill(pid, 0)`, which still succeeds for a zombie, so a freshly-exited
    /// shim looks alive until its parent reaps it. Boot-readiness waits use this
    /// so a short-lived container's exit does not stall the wait for the full
    /// timeout. On Linux it inspects `/proc/<pid>` process state; elsewhere it
    /// falls back to `!is_running()`.
    fn has_exited(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            linux_process_exited(self.pid())
        }
        #[cfg(not(target_os = "linux"))]
        {
            !self.is_running()
        }
    }

    /// Return the OS process ID of the VM.
    fn pid(&self) -> u32;

    /// Return the exit code of the VM process, if it has exited.
    ///
    /// Returns `None` until `stop()` has been called and the process has exited.
    /// Backends that do not track exit codes may leave this as the default `None`.
    fn exit_code(&self) -> Option<i32> {
        None
    }

    /// Poll the VM process for natural exit without sending any signal.
    ///
    /// Implementations that own a child process handle can use this to reap
    /// short-lived foreground workloads. Backends that cannot poll should
    /// return `Ok(None)`.
    fn try_wait_exit(&mut self) -> Result<Option<i32>> {
        Ok(None)
    }
}

/// Whether `pid` has exited, treating a zombie/dead process as exited.
///
/// Reads `/proc/<pid>/stat` and inspects the process state field. The `comm`
/// field can contain spaces and parentheses (e.g. libkrun renames the shim to
/// `(libkrun VM)`), so the state is located after the final `)`. A `Z` (zombie)
/// or `X` (dead) state, or a missing `/proc` entry, means the process exited.
#[cfg(target_os = "linux")]
pub(crate) fn linux_process_exited(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => match stat.rfind(')') {
            Some(idx) => {
                let state = stat[idx + 1..].trim_start().chars().next();
                matches!(state, Some('Z') | Some('X'))
            }
            // Malformed stat — be conservative and treat as still running.
            None => false,
        },
        // No /proc entry → the process is gone.
        Err(_) => true,
    }
}

// ── VMM provider ─────────────────────────────────────────────────────────────

/// Trait for VMM backend implementations.
///
/// Implement this to plug in an alternative hypervisor (e.g., QEMU, Cloud
/// Hypervisor) without changing any runtime code.
#[async_trait]
pub trait VmmProvider: Send + Sync {
    /// Start a VM from the given spec. Returns a handler for its lifetime.
    async fn start(&self, spec: &InstanceSpec) -> Result<Box<dyn VmHandler>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ResourceLimits;

    #[cfg(target_os = "linux")]
    #[test]
    fn test_linux_process_exited_current_process_is_alive() {
        // The test process itself is running (state R/S), not exited.
        assert!(!linux_process_exited(std::process::id()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_linux_process_exited_missing_pid_is_exited() {
        // A PID with no /proc entry is treated as exited.
        assert!(linux_process_exited(0x7fff_fffe));
    }

    #[test]
    fn test_parse_signal_name_term() {
        assert_eq!(parse_signal_name("SIGTERM"), 15);
        assert_eq!(parse_signal_name("TERM"), 15);
        assert_eq!(parse_signal_name("15"), 15);
    }

    #[test]
    fn test_parse_signal_name_variants() {
        assert_eq!(parse_signal_name("SIGKILL"), 9);
        assert_eq!(parse_signal_name("KILL"), 9);
        assert_eq!(parse_signal_name("SIGHUP"), 1);
        assert_eq!(parse_signal_name("SIGQUIT"), 3);
        assert_eq!(parse_signal_name("SIGINT"), 2);
        assert_eq!(parse_signal_name("SIGUSR1"), 10);
        assert_eq!(parse_signal_name("SIGUSR2"), 12);
    }

    #[test]
    fn test_parse_signal_name_numeric() {
        assert_eq!(parse_signal_name("9"), 9);
        assert_eq!(parse_signal_name("1"), 1);
    }

    #[test]
    fn test_parse_signal_name_unknown_defaults_to_sigterm() {
        assert_eq!(parse_signal_name("SIGFOO"), 15);
        assert_eq!(parse_signal_name(""), 15);
        assert_eq!(parse_signal_name("notasignal"), 15);
    }

    #[test]
    fn test_parse_signal_name_case_insensitive() {
        assert_eq!(parse_signal_name("sigterm"), 15);
        assert_eq!(parse_signal_name("Sigterm"), 15);
    }

    #[test]
    fn test_instance_spec_default_values() {
        let spec = InstanceSpec::default();
        assert_eq!(spec.vcpus, DEFAULT_VCPUS as u8);
        assert_eq!(spec.memory_mib, 512);
        assert_eq!(spec.workdir, "/");
        assert!(spec.box_id.is_empty());
        assert!(spec.fs_mounts.is_empty());
        assert!(spec.port_map.is_empty());
        assert!(spec.tee_config.is_none());
        assert!(spec.user.is_none());
        assert!(spec.network.is_none());
        assert!(!spec.disable_tsi);
        assert!(spec.console_output.is_none());
    }

    #[test]
    fn test_instance_spec_missing_disable_tsi_keeps_legacy_default() {
        let mut value = serde_json::to_value(InstanceSpec::default()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("disable_tsi")
            .unwrap();

        let spec: InstanceSpec = serde_json::from_value(value).unwrap();

        assert!(!spec.disable_tsi);
    }

    #[test]
    fn test_ext4_disk_rootfs_serde_roundtrip() {
        let rootfs = RootfsSource::Ext4Disk {
            path: PathBuf::from("/tmp/rootfs.ext4"),
            read_only: true,
        };

        let json = serde_json::to_value(&rootfs).unwrap();
        assert_eq!(json["kind"], "ext4_disk");
        assert_eq!(json["path"], "/tmp/rootfs.ext4");
        assert_eq!(json["read_only"], true);

        let decoded: RootfsSource = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, rootfs);
    }

    #[test]
    fn test_instance_spec_accepts_legacy_rootfs_path() {
        let json = r#"{
            "box_id": "legacy",
            "vcpus": 1,
            "memory_mib": 256,
            "rootfs_path": "/legacy/rootfs",
            "exec_socket_path": "/exec.sock",
            "fs_mounts": [],
            "entrypoint": {"executable": "/bin/sh", "args": [], "env": []},
            "console_output": null,
            "workdir": "/"
        }"#;

        let spec: InstanceSpec = serde_json::from_str(json).unwrap();
        assert_eq!(
            spec.rootfs,
            RootfsSource::Directory {
                path: PathBuf::from("/legacy/rootfs")
            }
        );
    }

    #[test]
    fn test_instance_spec_serde_roundtrip() {
        let spec = InstanceSpec {
            box_id: "test-box-123".to_string(),
            ksm: false,
            snapshot_mem_file: None,
            snapshot_sock: None,
            restore_from: None,
            vcpus: 4,
            memory_mib: 2048,
            rootfs: RootfsSource::directory("/tmp/rootfs"),
            block_devices: vec![RawBlockDevice::new("data", "/tmp/data.ext4", true)],
            exec_socket_path: PathBuf::from("/tmp/exec.sock"),
            pty_socket_path: PathBuf::from("/tmp/pty.sock"),
            attest_socket_path: PathBuf::from("/tmp/attest.sock"),
            port_forward_socket_path: PathBuf::from("/tmp/portfwd.sock"),
            fs_mounts: vec![FsMount {
                tag: "workspace".to_string(),
                host_path: PathBuf::from("/home/user/project"),
                read_only: false,
            }],
            entrypoint: Entrypoint {
                executable: "/usr/bin/agent".to_string(),
                args: vec!["--port".to_string(), "8080".to_string()],
                env: vec![("HOME".to_string(), "/root".to_string())],
            },
            console_output: Some(PathBuf::from("/tmp/console.log")),
            workdir: "/app".to_string(),
            tee_config: None,
            port_map: vec!["8080:80".to_string()],
            user: Some("1000:1000".to_string()),
            network: None,
            disable_tsi: true,
            resource_limits: ResourceLimits::default(),
            log_config: crate::log::LogConfig::default(),
        };

        let json = serde_json::to_string(&spec).unwrap();
        let deserialized: InstanceSpec = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.box_id, "test-box-123");
        assert_eq!(deserialized.vcpus, 4);
        assert_eq!(deserialized.memory_mib, 2048);
        assert_eq!(deserialized.rootfs, RootfsSource::directory("/tmp/rootfs"));
        assert_eq!(deserialized.block_devices, spec.block_devices);
        assert_eq!(deserialized.workdir, "/app");
        assert_eq!(deserialized.fs_mounts.len(), 1);
        assert_eq!(deserialized.fs_mounts[0].tag, "workspace");
        assert!(!deserialized.fs_mounts[0].read_only);
        assert_eq!(deserialized.entrypoint.executable, "/usr/bin/agent");
        assert_eq!(deserialized.entrypoint.args.len(), 2);
        assert_eq!(deserialized.entrypoint.env.len(), 1);
        assert_eq!(
            deserialized.port_forward_socket_path,
            PathBuf::from("/tmp/portfwd.sock")
        );
        assert_eq!(deserialized.port_map, vec!["8080:80"]);
        assert_eq!(deserialized.user, Some("1000:1000".to_string()));
        assert!(deserialized.disable_tsi);
    }

    #[test]
    fn instance_spec_omits_empty_auxiliary_block_device_list() {
        let value = serde_json::to_value(InstanceSpec::default()).unwrap();
        assert!(value.get("block_devices").is_none());

        let decoded: InstanceSpec = serde_json::from_value(value).unwrap();
        assert!(decoded.block_devices.is_empty());
    }

    #[test]
    fn test_instance_spec_with_tee_config() {
        let spec = InstanceSpec {
            tee_config: Some(TeeInstanceConfig {
                config_path: PathBuf::from("/etc/tee.json"),
                tee_type: "snp".to_string(),
            }),
            ..Default::default()
        };

        let json = serde_json::to_string(&spec).unwrap();
        let deserialized: InstanceSpec = serde_json::from_str(&json).unwrap();

        let tee = deserialized.tee_config.unwrap();
        assert_eq!(tee.tee_type, "snp");
        assert_eq!(tee.config_path, PathBuf::from("/etc/tee.json"));
    }

    #[test]
    fn test_instance_spec_with_network() {
        let spec = InstanceSpec {
            network: Some(NetworkInstanceConfig {
                net_socket_path: PathBuf::from("/tmp/net.sock"),
                net_stats_path: Some(PathBuf::from("/tmp/net.stats.json")),
                #[cfg(unix)]
                net_socket_fd: Some(42),
                #[cfg(unix)]
                net_proxy_fd: Some(43),
                #[cfg(unix)]
                bridge_socket_dir: Some(PathBuf::from("/tmp/a3s-switch")),
                ip_address: "10.0.0.2".parse().unwrap(),
                gateway: "10.0.0.1".parse().unwrap(),
                prefix_len: 24,
                mac_address: [0x02, 0x42, 0xac, 0x11, 0x00, 0x02],
                dns_servers: vec!["8.8.8.8".parse().unwrap()],
            }),
            ..Default::default()
        };

        let json = serde_json::to_string(&spec).unwrap();
        let deserialized: InstanceSpec = serde_json::from_str(&json).unwrap();

        let net = deserialized.network.unwrap();
        assert_eq!(
            net.net_stats_path,
            Some(PathBuf::from("/tmp/net.stats.json"))
        );
        #[cfg(unix)]
        assert_eq!(net.net_socket_fd, Some(42));
        #[cfg(unix)]
        assert_eq!(net.net_proxy_fd, Some(43));
        assert_eq!(net.ip_address, "10.0.0.2".parse::<Ipv4Addr>().unwrap());
        assert_eq!(net.gateway, "10.0.0.1".parse::<Ipv4Addr>().unwrap());
        assert_eq!(net.prefix_len, 24);
        assert_eq!(net.dns_servers.len(), 1);
    }

    #[test]
    fn test_fs_mount_serde() {
        let mount = FsMount {
            tag: "data".to_string(),
            host_path: PathBuf::from("/mnt/data"),
            read_only: true,
        };

        let json = serde_json::to_string(&mount).unwrap();
        let deserialized: FsMount = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.tag, "data");
        assert_eq!(deserialized.host_path, PathBuf::from("/mnt/data"));
        assert!(deserialized.read_only);
    }

    #[test]
    fn test_entrypoint_serde() {
        let ep = Entrypoint {
            executable: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), "echo hello".to_string()],
            env: vec![
                ("PATH".to_string(), "/usr/bin".to_string()),
                ("HOME".to_string(), "/root".to_string()),
            ],
        };

        let json = serde_json::to_string(&ep).unwrap();
        let deserialized: Entrypoint = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.executable, "/bin/sh");
        assert_eq!(deserialized.args, vec!["-c", "echo hello"]);
        assert_eq!(deserialized.env.len(), 2);
    }

    #[test]
    fn test_instance_spec_deserialize_missing_optional_fields() {
        let json = r#"{
            "box_id": "min",
            "vcpus": 1,
            "memory_mib": 256,
            "rootfs_path": "/rootfs",
            "exec_socket_path": "/exec.sock",
            "fs_mounts": [],
            "entrypoint": {"executable": "/bin/sh", "args": [], "env": []},
            "console_output": null,
            "workdir": "/"
        }"#;

        let spec: InstanceSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.box_id, "min");
        assert!(spec.port_map.is_empty());
        assert!(spec.user.is_none());
        assert!(spec.network.is_none());
        assert!(spec.tee_config.is_none());
    }

    #[test]
    fn test_resource_limits_in_spec() {
        let spec = InstanceSpec {
            resource_limits: ResourceLimits {
                pids_limit: Some(100),
                cpuset_cpus: Some("0-3".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };

        let json = serde_json::to_string(&spec).unwrap();
        let deserialized: InstanceSpec = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.resource_limits.pids_limit, Some(100));
        assert_eq!(
            deserialized.resource_limits.cpuset_cpus,
            Some("0-3".to_string())
        );
    }

    #[test]
    fn test_vm_metrics_default() {
        let m = VmMetrics::default();
        assert!(m.cpu_percent.is_none());
        assert!(m.memory_bytes.is_none());
    }

    #[test]
    fn test_vm_metrics_clone() {
        let m = VmMetrics {
            cpu_percent: Some(50.0),
            memory_bytes: Some(1024 * 1024),
        };
        let cloned = m.clone();
        assert_eq!(cloned.cpu_percent, Some(50.0));
        assert_eq!(cloned.memory_bytes, Some(1024 * 1024));
    }
}
