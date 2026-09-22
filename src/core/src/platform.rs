//! Platform types for multi-architecture image builds.
//!
//! Represents target OS/architecture pairs used by Buildx-style
//! multi-platform builds and OCI Image Index manifests.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A target platform (OS + architecture).
///
/// Used for multi-platform builds and OCI Image Index entries.
/// Compatible with Docker/OCI platform specification.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Platform {
    /// Operating system (e.g., "linux").
    pub os: String,
    /// CPU architecture (e.g., "amd64", "arm64").
    pub architecture: String,
    /// Optional variant (e.g., "v7" for armv7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

impl Platform {
    /// Create a new platform.
    pub fn new(os: impl Into<String>, architecture: impl Into<String>) -> Self {
        Self {
            os: os.into(),
            architecture: architecture.into(),
            variant: None,
        }
    }

    /// Create a platform with a variant.
    pub fn with_variant(
        os: impl Into<String>,
        architecture: impl Into<String>,
        variant: impl Into<String>,
    ) -> Self {
        Self {
            os: os.into(),
            architecture: architecture.into(),
            variant: Some(variant.into()),
        }
    }

    /// linux/amd64
    pub fn linux_amd64() -> Self {
        Self::new("linux", "amd64")
    }

    /// linux/arm64
    pub fn linux_arm64() -> Self {
        Self::new("linux", "arm64")
    }

    /// Detect the current host platform.
    pub fn host() -> Self {
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        };
        let os = match std::env::consts::OS {
            "macos" => "darwin",
            other => other,
        };
        Self::new(os, arch)
    }

    /// Parse a platform string like "linux/amd64" or "linux/arm/v7".
    pub fn parse(s: &str) -> Result<Self, String> {
        let parts: Vec<&str> = s.split('/').collect();
        match parts.len() {
            2 => {
                let arch = normalize_arch(parts[1]);
                Ok(Self::new(parts[0], arch))
            }
            3 => {
                let arch = normalize_arch(parts[1]);
                Ok(Self::with_variant(parts[0], arch, parts[2]))
            }
            _ => Err(format!(
                "Invalid platform '{}': expected 'os/arch' or 'os/arch/variant'",
                s
            )),
        }
    }

    /// Parse a comma-separated list of platforms.
    ///
    /// Example: "linux/amd64,linux/arm64"
    pub fn parse_list(s: &str) -> Result<Vec<Self>, String> {
        s.split(',').map(|p| Self::parse(p.trim())).collect()
    }

    /// Check if this platform matches the host architecture.
    pub fn is_native(&self) -> bool {
        *self == Self::host()
    }

    /// Get the OCI architecture string.
    pub fn oci_arch(&self) -> &str {
        &self.architecture
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.os, self.architecture)?;
        if let Some(ref v) = self.variant {
            write!(f, "/{}", v)?;
        }
        Ok(())
    }
}

/// Runtime capabilities exposed by the current host OS.
///
/// This is intentionally separate from [`Platform`], which represents an OCI
/// image platform. Capabilities describe what the host can execute directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformCapabilities {
    /// Host OS as reported by Rust.
    pub os: String,
    /// Host architecture in OCI naming.
    pub architecture: String,
    /// VM backend used on this host.
    pub vm_backend: VmBackend,
    /// Host/guest control channel available on this host.
    pub host_guest_channel: HostGuestChannel,
    /// Whether Unix domain sockets are available.
    pub unix_sockets: bool,
    /// Whether Windows named pipes are available.
    pub named_pipes: bool,
    /// Whether the native network proxy is available.
    pub netproxy: bool,
    /// Bridge network backend available on this host.
    pub bridge_network_backend: BridgeNetworkBackend,
    /// Whether user-defined bridge networks can provide guest outbound NAT.
    pub bridge_outbound_nat: bool,
    /// Whether published ports can be bridged on this host.
    pub published_ports: bool,
    /// Whether TEE attestation is available in the native runtime.
    pub tee_attestation: bool,
    /// Whether sealed storage is available in the native runtime.
    pub sealed_storage: bool,
    /// Whether this host can run interactive PTY sessions through the native control channel.
    pub interactive_pty: bool,
}

/// VM backend selected for the current host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VmBackend {
    /// Native libkrun-backed VM execution.
    Krun,
    /// Native Windows Hypervisor Platform backend through libkrun.
    Whpx,
    /// No supported VM backend for this host.
    Unsupported,
}

impl fmt::Display for VmBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Krun => write!(f, "krun"),
            Self::Whpx => write!(f, "whpx"),
            Self::Unsupported => write!(f, "unsupported"),
        }
    }
}

/// Host/guest control transport available on the current host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostGuestChannel {
    /// Unix domain sockets.
    UnixSocket,
    /// Windows named pipes.
    NamedPipe,
    /// No supported channel.
    Unsupported,
}

impl fmt::Display for HostGuestChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnixSocket => write!(f, "unix-socket"),
            Self::NamedPipe => write!(f, "named-pipe"),
            Self::Unsupported => write!(f, "unsupported"),
        }
    }
}

/// User-defined bridge network backend selected for the current host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BridgeNetworkBackend {
    /// Linux `passt` backend.
    Passt,
    /// macOS pure-Rust vfkit netproxy backend.
    Netproxy,
    /// No bridge backend is available.
    Unsupported,
}

impl fmt::Display for BridgeNetworkBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Passt => write!(f, "passt"),
            Self::Netproxy => write!(f, "netproxy"),
            Self::Unsupported => write!(f, "unsupported"),
        }
    }
}

/// Host family that owns one execution contract.
///
/// The contract is pure data. Windows and Linux (including a Linux guest
/// such as WSL) can be checked without a hypervisor, and neither family
/// selects the other's runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostFamily {
    /// Linux host, including WSL. MicroVM uses KVM; control uses a Unix socket.
    Linux,
    /// macOS host. MicroVM uses HVF through libkrun.
    Macos,
    /// Native Windows host. MicroVM uses WHPX; control uses a named pipe.
    Windows,
    /// Any other operating system. No VM backend is advertised.
    Other,
}

impl HostFamily {
    /// Family of the process that is actually running.
    pub fn current() -> Self {
        Self::from_os(std::env::consts::OS)
    }

    /// Map a Rust `std::env::consts::OS` name to a host family.
    pub fn from_os(os: &str) -> Self {
        match os {
            "linux" => Self::Linux,
            "macos" => Self::Macos,
            "windows" => Self::Windows,
            _ => Self::Other,
        }
    }
}

impl PlatformCapabilities {
    /// Capabilities for one host family.
    ///
    /// `architecture` uses OCI names (`amd64`, `arm64`). The result does not
    /// read the hypervisor, a WSL distro name, or a kernel package path.
    pub fn for_host(family: HostFamily, architecture: impl Into<String>) -> Self {
        let architecture = architecture.into();
        match family {
            HostFamily::Linux => Self {
                os: "linux".to_string(),
                architecture,
                vm_backend: VmBackend::Krun,
                host_guest_channel: HostGuestChannel::UnixSocket,
                unix_sockets: true,
                named_pipes: false,
                netproxy: false,
                bridge_network_backend: BridgeNetworkBackend::Passt,
                bridge_outbound_nat: true,
                published_ports: true,
                tee_attestation: true,
                sealed_storage: true,
                interactive_pty: true,
            },
            HostFamily::Macos => Self {
                os: "macos".to_string(),
                architecture,
                vm_backend: VmBackend::Krun,
                host_guest_channel: HostGuestChannel::UnixSocket,
                unix_sockets: true,
                named_pipes: false,
                netproxy: true,
                bridge_network_backend: BridgeNetworkBackend::Netproxy,
                bridge_outbound_nat: false,
                published_ports: true,
                tee_attestation: true,
                sealed_storage: true,
                interactive_pty: true,
            },
            HostFamily::Windows => Self {
                os: "windows".to_string(),
                architecture,
                vm_backend: VmBackend::Whpx,
                host_guest_channel: HostGuestChannel::NamedPipe,
                unix_sockets: false,
                named_pipes: true,
                netproxy: false,
                bridge_network_backend: BridgeNetworkBackend::Unsupported,
                bridge_outbound_nat: false,
                published_ports: true,
                tee_attestation: false,
                sealed_storage: false,
                interactive_pty: false,
            },
            HostFamily::Other => Self {
                os: "unknown".to_string(),
                architecture,
                vm_backend: VmBackend::Unsupported,
                host_guest_channel: HostGuestChannel::Unsupported,
                unix_sockets: false,
                named_pipes: false,
                netproxy: false,
                bridge_network_backend: BridgeNetworkBackend::Unsupported,
                bridge_outbound_nat: false,
                published_ports: false,
                tee_attestation: false,
                sealed_storage: false,
                interactive_pty: false,
            },
        }
    }

    /// Detect capabilities for the current host.
    pub fn current() -> Self {
        let host = Platform::host();
        Self::for_host(HostFamily::current(), host.architecture)
    }

    /// Whether the host can run the VM runtime directly.
    pub fn supports_native_vm(&self) -> bool {
        matches!(self.vm_backend, VmBackend::Krun | VmBackend::Whpx)
    }

    /// Whether the host has a supported control channel.
    pub fn supports_host_guest_channel(&self) -> bool {
        self.host_guest_channel != HostGuestChannel::Unsupported
    }

    /// Whether user-defined bridge networking is available.
    pub fn supports_bridge_networking(&self) -> bool {
        self.bridge_network_backend != BridgeNetworkBackend::Unsupported
    }

    /// Human-readable bridge networking mode summary for diagnostics.
    pub fn bridge_networking_summary(&self) -> String {
        match self.bridge_network_backend {
            BridgeNetworkBackend::Passt => {
                "passt (peer networking and outbound NAT supported)".to_string()
            }
            BridgeNetworkBackend::Netproxy => {
                "netproxy (peer networking and outbound TCP proxying supported)".to_string()
            }
            BridgeNetworkBackend::Unsupported => "unsupported".to_string(),
        }
    }
}

/// Normalize architecture names to OCI conventions.
fn normalize_arch(arch: &str) -> String {
    match arch {
        "x86_64" | "x86-64" => "amd64".to_string(),
        "aarch64" | "arm64v8" => "arm64".to_string(),
        "armhf" | "armv7l" => "arm".to_string(),
        "i386" | "i686" | "x86" => "386".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_new() {
        let p = Platform::new("linux", "amd64");
        assert_eq!(p.os, "linux");
        assert_eq!(p.architecture, "amd64");
        assert!(p.variant.is_none());
    }

    #[test]
    fn test_platform_with_variant() {
        let p = Platform::with_variant("linux", "arm", "v7");
        assert_eq!(p.variant, Some("v7".to_string()));
    }

    #[test]
    fn test_platform_display() {
        assert_eq!(Platform::linux_amd64().to_string(), "linux/amd64");
        assert_eq!(Platform::linux_arm64().to_string(), "linux/arm64");
        assert_eq!(
            Platform::with_variant("linux", "arm", "v7").to_string(),
            "linux/arm/v7"
        );
    }

    #[test]
    fn test_platform_parse() {
        let p = Platform::parse("linux/amd64").unwrap();
        assert_eq!(p, Platform::linux_amd64());

        let p = Platform::parse("linux/arm64").unwrap();
        assert_eq!(p, Platform::linux_arm64());

        let p = Platform::parse("linux/arm/v7").unwrap();
        assert_eq!(p.architecture, "arm");
        assert_eq!(p.variant, Some("v7".to_string()));
    }

    #[test]
    fn test_platform_parse_normalizes() {
        let p = Platform::parse("linux/x86_64").unwrap();
        assert_eq!(p.architecture, "amd64");

        let p = Platform::parse("linux/aarch64").unwrap();
        assert_eq!(p.architecture, "arm64");
    }

    #[test]
    fn test_platform_parse_invalid() {
        assert!(Platform::parse("linux").is_err());
        assert!(Platform::parse("a/b/c/d").is_err());
    }

    #[test]
    fn test_platform_parse_list() {
        let platforms = Platform::parse_list("linux/amd64,linux/arm64").unwrap();
        assert_eq!(platforms.len(), 2);
        assert_eq!(platforms[0], Platform::linux_amd64());
        assert_eq!(platforms[1], Platform::linux_arm64());
    }

    #[test]
    fn test_platform_parse_list_with_spaces() {
        let platforms = Platform::parse_list("linux/amd64, linux/arm64").unwrap();
        assert_eq!(platforms.len(), 2);
    }

    #[test]
    fn test_platform_host() {
        let host = Platform::host();
        let expected_os = match std::env::consts::OS {
            "macos" => "darwin",
            other => other,
        };
        assert_eq!(host.os, expected_os);
        assert!(host.architecture == "amd64" || host.architecture == "arm64");
    }

    #[test]
    fn test_platform_is_native() {
        let host = Platform::host();
        assert!(host.is_native());
        // The opposite arch should not be native
        let other = if host.architecture == "amd64" {
            Platform::linux_arm64()
        } else {
            Platform::linux_amd64()
        };
        assert!(!other.is_native());
    }

    #[test]
    fn test_platform_serde_roundtrip() {
        let p = Platform::linux_amd64();
        let json = serde_json::to_string(&p).unwrap();
        let parsed: Platform = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, p);
    }

    #[test]
    fn test_platform_serde_with_variant() {
        let p = Platform::with_variant("linux", "arm", "v7");
        let json = serde_json::to_string(&p).unwrap();
        let parsed: Platform = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, p);
        assert_eq!(parsed.variant, Some("v7".to_string()));
    }

    #[test]
    fn test_normalize_arch() {
        assert_eq!(normalize_arch("x86_64"), "amd64");
        assert_eq!(normalize_arch("aarch64"), "arm64");
        assert_eq!(normalize_arch("armhf"), "arm");
        assert_eq!(normalize_arch("i386"), "386");
        assert_eq!(normalize_arch("riscv64"), "riscv64");
    }

    #[test]
    fn test_platform_equality() {
        assert_eq!(Platform::linux_amd64(), Platform::new("linux", "amd64"));
        assert_ne!(Platform::linux_amd64(), Platform::linux_arm64());
    }

    #[test]
    fn test_platform_capabilities_match_host() {
        let capabilities = PlatformCapabilities::current();
        assert_eq!(capabilities.os, std::env::consts::OS);
        assert!(capabilities.supports_host_guest_channel());

        #[cfg(unix)]
        {
            assert_eq!(capabilities.vm_backend, VmBackend::Krun);
            assert_eq!(
                capabilities.host_guest_channel,
                HostGuestChannel::UnixSocket
            );
            assert!(capabilities.unix_sockets);
            assert!(!capabilities.named_pipes);
            assert!(capabilities.supports_native_vm());
            assert!(capabilities.supports_bridge_networking());
        }

        #[cfg(windows)]
        {
            assert_eq!(capabilities.vm_backend, VmBackend::Whpx);
            assert_eq!(capabilities.host_guest_channel, HostGuestChannel::NamedPipe);
            assert!(!capabilities.unix_sockets);
            assert!(capabilities.named_pipes);
            assert!(capabilities.supports_native_vm());
            assert!(!capabilities.interactive_pty);
        }
    }

    #[test]
    fn test_platform_capability_display_values() {
        assert_eq!(VmBackend::Krun.to_string(), "krun");
        assert_eq!(VmBackend::Whpx.to_string(), "whpx");
        assert_eq!(HostGuestChannel::UnixSocket.to_string(), "unix-socket");
        assert_eq!(HostGuestChannel::NamedPipe.to_string(), "named-pipe");
        assert_eq!(BridgeNetworkBackend::Netproxy.to_string(), "netproxy");
    }

    #[test]
    fn test_bridge_networking_summary_documents_outbound_support() {
        let mut capabilities = PlatformCapabilities::current();
        capabilities.bridge_network_backend = BridgeNetworkBackend::Netproxy;
        capabilities.bridge_outbound_nat = false;

        let summary = capabilities.bridge_networking_summary();

        assert!(summary.contains("netproxy"));
        assert!(summary.contains("outbound TCP proxying supported"));
        assert!(!capabilities.bridge_outbound_nat);
    }

    #[test]
    fn windows_and_linux_contracts_stay_on_their_own_runtimes() {
        let windows = PlatformCapabilities::for_host(HostFamily::Windows, "amd64");
        assert_eq!(windows.os, "windows");
        assert_eq!(windows.vm_backend, VmBackend::Whpx);
        assert_eq!(windows.host_guest_channel, HostGuestChannel::NamedPipe);
        assert!(windows.named_pipes);
        assert!(!windows.unix_sockets);
        assert!(!windows.interactive_pty);
        assert!(!windows.tee_attestation);
        assert!(!windows.supports_bridge_networking());
        assert!(windows.supports_native_vm());

        let linux = PlatformCapabilities::for_host(HostFamily::Linux, "amd64");
        assert_eq!(linux.os, "linux");
        assert_eq!(linux.vm_backend, VmBackend::Krun);
        assert_eq!(linux.host_guest_channel, HostGuestChannel::UnixSocket);
        assert!(linux.unix_sockets);
        assert!(!linux.named_pipes);
        assert!(linux.interactive_pty);
        assert_ne!(linux.vm_backend, VmBackend::Whpx);
        assert_ne!(linux.host_guest_channel, HostGuestChannel::NamedPipe);

        let macos = PlatformCapabilities::for_host(HostFamily::Macos, "arm64");
        assert_eq!(macos.vm_backend, VmBackend::Krun);
        assert_eq!(macos.host_guest_channel, HostGuestChannel::UnixSocket);
        assert_ne!(macos.vm_backend, VmBackend::Whpx);

        let current = PlatformCapabilities::current();
        let expected =
            PlatformCapabilities::for_host(HostFamily::current(), current.architecture.clone());
        assert_eq!(current, expected);
        assert_eq!(HostFamily::from_os("linux"), HostFamily::Linux);
        assert_eq!(HostFamily::from_os("windows"), HostFamily::Windows);
        assert_eq!(HostFamily::from_os("Ubuntu"), HostFamily::Other);
    }
}
