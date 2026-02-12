//! InstanceSpec - Complete configuration for a VM instance.

use a3s_box_core::config::ResourceLimits;
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;
use std::path::PathBuf;

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

/// Network instance configuration for passt-based networking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkInstanceConfig {
    /// Path to the passt Unix socket.
    pub passt_socket_path: PathBuf,

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

/// Complete configuration for a VM instance.
///
/// This struct is serialized and passed to the shim subprocess,
/// which uses it to configure and start the VM via libkrun.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceSpec {
    /// Unique identifier for this box instance
    pub box_id: String,

    /// Number of vCPUs (default: 2)
    pub vcpus: u8,

    /// Memory in MiB (default: 512)
    pub memory_mib: u32,

    /// Path to the root filesystem
    pub rootfs_path: PathBuf,

    /// Path to the Unix socket for gRPC communication
    /// This socket is bridged to vsock inside the VM
    pub grpc_socket_path: PathBuf,

    /// Path to the Unix socket for exec communication
    /// This socket is bridged to vsock port 4089 inside the VM
    pub exec_socket_path: PathBuf,

    /// Path to the Unix socket for PTY communication
    /// This socket is bridged to vsock port 4090 inside the VM
    #[serde(default)]
    pub pty_socket_path: PathBuf,

    /// Filesystem mounts (virtio-fs shares)
    pub fs_mounts: Vec<FsMount>,

    /// Guest agent entrypoint
    pub entrypoint: Entrypoint,

    /// Optional console output file path
    pub console_output: Option<PathBuf>,

    /// Working directory inside the VM
    pub workdir: String,

    /// TEE configuration (None for standard VM)
    pub tee_config: Option<TeeInstanceConfig>,

    /// TSI port mappings: ["host_port:guest_port", ...]
    /// Maps host ports to guest ports via Transparent Socket Impersonation.
    #[serde(default)]
    pub port_map: Vec<String>,

    /// User to run as inside the VM (from OCI USER directive).
    /// Format: "uid", "uid:gid", "user", or "user:group"
    #[serde(default)]
    pub user: Option<String>,

    /// Network configuration for passt-based networking.
    /// None = TSI mode (default), Some = passt virtio-net mode.
    #[serde(default)]
    pub network: Option<NetworkInstanceConfig>,

    /// Resource limits (PID limits, CPU pinning, ulimits, cgroup controls).
    #[serde(default)]
    pub resource_limits: ResourceLimits,
}

impl Default for InstanceSpec {
    fn default() -> Self {
        Self {
            box_id: String::new(),
            vcpus: 2,
            memory_mib: 512,
            rootfs_path: PathBuf::new(),
            grpc_socket_path: PathBuf::new(),
            exec_socket_path: PathBuf::new(),
            pty_socket_path: PathBuf::new(),
            fs_mounts: Vec::new(),
            entrypoint: Entrypoint {
                executable: String::new(),
                args: Vec::new(),
                env: Vec::new(),
            },
            console_output: None,
            workdir: "/".to_string(),
            tee_config: None,
            port_map: Vec::new(),
            user: None,
            network: None,
            resource_limits: ResourceLimits::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_instance_spec_default_values() {
        let spec = InstanceSpec::default();
        assert_eq!(spec.vcpus, 2);
        assert_eq!(spec.memory_mib, 512);
        assert_eq!(spec.workdir, "/");
        assert!(spec.box_id.is_empty());
        assert!(spec.fs_mounts.is_empty());
        assert!(spec.port_map.is_empty());
        assert!(spec.tee_config.is_none());
        assert!(spec.user.is_none());
        assert!(spec.network.is_none());
        assert!(spec.console_output.is_none());
    }

    #[test]
    fn test_instance_spec_serde_roundtrip() {
        let spec = InstanceSpec {
            box_id: "test-box-123".to_string(),
            vcpus: 4,
            memory_mib: 2048,
            rootfs_path: PathBuf::from("/tmp/rootfs"),
            grpc_socket_path: PathBuf::from("/tmp/grpc.sock"),
            exec_socket_path: PathBuf::from("/tmp/exec.sock"),
            pty_socket_path: PathBuf::from("/tmp/pty.sock"),
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
            resource_limits: ResourceLimits::default(),
        };

        let json = serde_json::to_string(&spec).unwrap();
        let deserialized: InstanceSpec = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.box_id, "test-box-123");
        assert_eq!(deserialized.vcpus, 4);
        assert_eq!(deserialized.memory_mib, 2048);
        assert_eq!(deserialized.workdir, "/app");
        assert_eq!(deserialized.fs_mounts.len(), 1);
        assert_eq!(deserialized.fs_mounts[0].tag, "workspace");
        assert!(!deserialized.fs_mounts[0].read_only);
        assert_eq!(deserialized.entrypoint.executable, "/usr/bin/agent");
        assert_eq!(deserialized.entrypoint.args.len(), 2);
        assert_eq!(deserialized.entrypoint.env.len(), 1);
        assert_eq!(deserialized.port_map, vec!["8080:80"]);
        assert_eq!(deserialized.user, Some("1000:1000".to_string()));
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
                passt_socket_path: PathBuf::from("/tmp/passt.sock"),
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
        // Minimal JSON — optional/default fields omitted
        let json = r#"{
            "box_id": "min",
            "vcpus": 1,
            "memory_mib": 256,
            "rootfs_path": "/rootfs",
            "grpc_socket_path": "/grpc.sock",
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
        assert_eq!(deserialized.resource_limits.cpuset_cpus, Some("0-3".to_string()));
    }
}
