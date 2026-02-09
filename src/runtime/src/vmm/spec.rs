//! InstanceSpec - Complete configuration for a VM instance.

use serde::{Deserialize, Serialize};
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
        }
    }
}
