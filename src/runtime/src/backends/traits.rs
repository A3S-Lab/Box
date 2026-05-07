//! Core traits for platform abstraction.
//!
//! These traits define the interface that platform-specific backends must implement
//! to provide cross-platform support for a3s-box.

use async_trait::async_trait;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use a3s_box_core::error::Result;
use a3s_box_core::vmm::InstanceSpec;

// ── VMM Backend ───────────────────────────────────────────────────────────────

/// VMM backend for hypervisor operations.
///
/// Encapsulates platform-specific hypervisor operations (HVF, KVM, WHPX).
#[async_trait]
pub trait VmmBackend: Send + Sync {
    /// Boot a VM with the given specification.
    ///
    /// Returns the process ID of the VM subprocess.
    async fn boot(&self, spec: &InstanceSpec) -> Result<u32>;

    /// Shutdown a running VM.
    ///
    /// Sends the specified signal, then SIGKILL after timeout.
    async fn shutdown(&self, pid: u32, signal: i32, timeout_ms: u64) -> Result<()>;

    /// Pause a running VM (SIGSTOP or equivalent).
    async fn pause(&self, pid: u32) -> Result<()>;

    /// Resume a paused VM (SIGCONT or equivalent).
    async fn resume(&self, pid: u32) -> Result<()>;

    /// Configure CPU resources for a VM.
    ///
    /// Returns an error if the platform doesn't support dynamic CPU configuration.
    async fn configure_cpu(&self, pid: u32, vcpus: u8) -> Result<()>;

    /// Configure memory resources for a VM.
    ///
    /// Returns an error if the platform doesn't support dynamic memory configuration.
    async fn configure_memory(&self, pid: u32, memory_mib: u32) -> Result<()>;

    /// Get CPU and memory metrics for a running VM.
    async fn get_metrics(&self, pid: u32) -> Result<VmMetrics>;

    /// Check if a VM process is still running.
    fn is_running(&self, pid: u32) -> bool;

    /// Get the exit code of a VM process, if it has exited.
    fn exit_code(&self, pid: u32) -> Option<i32>;

    /// Get the name of this backend (e.g., "hvf", "kvm", "whpx").
    fn name(&self) -> &'static str;

    /// Get platform-specific capabilities.
    fn capabilities(&self) -> BackendCapabilities;
}

/// VM resource metrics.
#[derive(Debug, Clone, Default)]
pub struct VmMetrics {
    /// CPU usage percentage (0-100 per core)
    pub cpu_percent: Option<f32>,
    /// Memory usage in bytes
    pub memory_bytes: Option<u64>,
    /// Network bytes received
    pub network_rx_bytes: Option<u64>,
    /// Network bytes transmitted
    pub network_tx_bytes: Option<u64>,
    /// Disk bytes read
    pub disk_read_bytes: Option<u64>,
    /// Disk bytes written
    pub disk_write_bytes: Option<u64>,
}

/// Backend capabilities.
#[derive(Debug, Clone)]
pub struct BackendCapabilities {
    /// Supports dynamic CPU configuration
    pub dynamic_cpu: bool,
    /// Supports dynamic memory configuration
    pub dynamic_memory: bool,
    /// Supports pause/resume
    pub pause_resume: bool,
    /// Supports vsock communication
    pub vsock: bool,
    /// Supports virtiofs
    pub virtiofs: bool,
    /// Supports port forwarding
    pub port_forwarding: bool,
    /// Supports TEE (Trusted Execution Environment)
    pub tee: bool,
}

// ── Network Backend ───────────────────────────────────────────────────────────

/// Network backend for networking operations.
///
/// Encapsulates platform-specific networking (vmnet, bridge/tap, HNS).
#[async_trait]
pub trait NetworkBackend: Send + Sync {
    /// Create a bridge network.
    ///
    /// Returns the bridge name or identifier.
    async fn create_bridge(&self, name: &str, subnet: &str) -> Result<String>;

    /// Delete a bridge network.
    async fn delete_bridge(&self, bridge_id: &str) -> Result<()>;

    /// Attach a VM interface to a bridge.
    ///
    /// Returns the assigned IP address.
    async fn attach_interface(
        &self,
        bridge_id: &str,
        vm_id: &str,
        mac_address: [u8; 6],
    ) -> Result<Ipv4Addr>;

    /// Detach a VM interface from a bridge.
    async fn detach_interface(&self, bridge_id: &str, vm_id: &str) -> Result<()>;

    /// Configure NAT for a bridge network.
    async fn configure_nat(&self, bridge_id: &str, enable: bool) -> Result<()>;

    /// Configure port forwarding.
    ///
    /// Maps host_port to guest_port for the specified VM.
    async fn configure_port_forward(
        &self,
        vm_id: &str,
        host_port: u16,
        guest_port: u16,
        protocol: Protocol,
    ) -> Result<()>;

    /// Remove port forwarding.
    async fn remove_port_forward(&self, vm_id: &str, host_port: u16) -> Result<()>;

    /// Start an embedded DNS server for container name resolution.
    ///
    /// Returns the DNS server address.
    async fn start_dns_server(&self, bridge_id: &str) -> Result<Ipv4Addr>;

    /// Stop the embedded DNS server.
    async fn stop_dns_server(&self, bridge_id: &str) -> Result<()>;

    /// Register a container name for DNS resolution.
    async fn register_dns_name(
        &self,
        bridge_id: &str,
        name: &str,
        ip: Ipv4Addr,
    ) -> Result<()>;

    /// Unregister a container name from DNS.
    async fn unregister_dns_name(&self, bridge_id: &str, name: &str) -> Result<()>;

    /// Get the name of this backend (e.g., "vmnet", "bridge", "hns").
    fn name(&self) -> &'static str;
}

/// Network protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Tcp,
    Udp,
}

// ── Filesystem Backend ────────────────────────────────────────────────────────

/// Filesystem backend for mount operations.
///
/// Encapsulates platform-specific filesystem operations (virtiofs, 9p, SMB).
#[async_trait]
pub trait FsBackend: Send + Sync {
    /// Mount a host directory into a guest VM.
    ///
    /// Returns the mount tag that the guest can use to identify the share.
    async fn mount_host_path(
        &self,
        vm_id: &str,
        host_path: &Path,
        guest_path: &Path,
        read_only: bool,
    ) -> Result<String>;

    /// Unmount a previously mounted directory.
    async fn unmount(&self, vm_id: &str, mount_tag: &str) -> Result<()>;

    /// Create a named volume.
    ///
    /// Returns the path to the volume on the host.
    async fn create_volume(&self, name: &str, size_bytes: Option<u64>) -> Result<PathBuf>;

    /// Delete a named volume.
    async fn delete_volume(&self, name: &str) -> Result<()>;

    /// Get the path to a named volume on the host.
    async fn get_volume_path(&self, name: &str) -> Result<PathBuf>;

    /// Snapshot a volume.
    ///
    /// Returns the snapshot identifier.
    async fn snapshot_volume(&self, name: &str, snapshot_name: &str) -> Result<String>;

    /// Restore a volume from a snapshot.
    async fn restore_volume(&self, name: &str, snapshot_id: &str) -> Result<()>;

    /// Get the name of this backend (e.g., "virtiofs", "9p", "smb").
    fn name(&self) -> &'static str;

    /// Get filesystem capabilities.
    fn capabilities(&self) -> FsCapabilities;
}

/// Filesystem capabilities.
#[derive(Debug, Clone)]
pub struct FsCapabilities {
    /// Supports virtiofs
    pub virtiofs: bool,
    /// Supports 9p protocol
    pub ninep: bool,
    /// Supports SMB/CIFS
    pub smb: bool,
    /// Supports volume snapshots
    pub snapshots: bool,
}

// ── Exec Backend ──────────────────────────────────────────────────────────────

/// Exec backend for command execution in VMs.
///
/// Encapsulates platform-specific exec operations (vsock, TCP proxy).
#[async_trait]
pub trait ExecBackend: Send + Sync {
    /// Execute a command in a running VM.
    ///
    /// Returns the exit code of the command.
    async fn exec(
        &self,
        vm_id: &str,
        command: &[String],
        env: &[(String, String)],
        workdir: &str,
        user: Option<&str>,
    ) -> Result<ExecResult>;

    /// Execute a command with an interactive PTY.
    ///
    /// Returns a handle to the PTY session.
    async fn exec_pty(
        &self,
        vm_id: &str,
        command: &[String],
        env: &[(String, String)],
        workdir: &str,
        user: Option<&str>,
        term_size: TerminalSize,
    ) -> Result<Box<dyn PtySession>>;

    /// Attach to a container's main process PTY.
    async fn attach(&self, vm_id: &str) -> Result<Box<dyn PtySession>>;

    /// Send a signal to a process in the VM.
    async fn send_signal(&self, vm_id: &str, pid: u32, signal: i32) -> Result<()>;

    /// Get the name of this backend (e.g., "vsock", "tcp-proxy").
    fn name(&self) -> &'static str;
}

/// Result of an exec operation.
#[derive(Debug, Clone)]
pub struct ExecResult {
    /// Exit code of the command
    pub exit_code: i32,
    /// Standard output
    pub stdout: Vec<u8>,
    /// Standard error
    pub stderr: Vec<u8>,
}

/// Terminal size for PTY sessions.
#[derive(Debug, Clone, Copy)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

/// PTY session handle.
#[async_trait]
pub trait PtySession: Send + Sync {
    /// Read data from the PTY.
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize>;

    /// Write data to the PTY.
    async fn write(&mut self, buf: &[u8]) -> Result<usize>;

    /// Resize the PTY.
    async fn resize(&mut self, size: TerminalSize) -> Result<()>;

    /// Close the PTY session.
    async fn close(&mut self) -> Result<()>;
}
