//! Host virtualization support detection.
//!
//! Checks if the current host supports hardware virtualization:
//! - macOS: Hypervisor.framework (Apple Silicon only)
//! - Linux: KVM (/dev/kvm)

use a3s_box_core::error::{BoxError, Result};

/// Information about virtualization support.
#[derive(Debug, Clone)]
pub struct VirtualizationSupport {
    /// Human-readable description of the virtualization backend.
    pub backend: String,
    /// Additional details about the support.
    pub details: String,
}

/// Check if the current host supports hardware virtualization.
///
/// Returns `Ok(VirtualizationSupport)` if supported, or an error explaining why not.
pub fn check_virtualization_support() -> Result<VirtualizationSupport> {
    #[cfg(target_os = "macos")]
    {
        check_macos_hypervisor()
    }

    #[cfg(target_os = "linux")]
    {
        check_linux_kvm()
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err(BoxError::Other(
            "Unsupported platform: A3S Box requires macOS (Apple Silicon) or Linux with KVM"
                .to_string(),
        ))
    }
}

/// Check for Hypervisor.framework support on macOS.
#[cfg(target_os = "macos")]
fn check_macos_hypervisor() -> Result<VirtualizationSupport> {
    #[cfg(target_arch = "aarch64")]
    {
        // Query via sysctl kern.hv_support
        let output = std::process::Command::new("sysctl")
            .arg("kern.hv_support")
            .output()
            .map_err(|e| BoxError::Other(format!("Failed to run sysctl: {}", e)))?;

        if !output.status.success() {
            return Err(BoxError::Other(
                "Failed to query Hypervisor.framework support via sysctl".to_string(),
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        // Parse: "kern.hv_support: 1" (supported) or "0" (not supported)
        let value = stdout.split(':').nth(1).map(|s| s.trim()).unwrap_or("0");

        if value == "1" {
            Ok(VirtualizationSupport {
                backend: "Hypervisor.framework".to_string(),
                details: "Apple Silicon hardware virtualization is available".to_string(),
            })
        } else {
            Err(BoxError::Other(
                "Hypervisor.framework is not available on this system. \
                 Ensure you are running on Apple Silicon and have the necessary entitlements."
                    .to_string(),
            ))
        }
    }

    #[cfg(not(target_arch = "aarch64"))]
    {
        Err(BoxError::Other(
            "A3S Box on macOS requires Apple Silicon (ARM64). Intel Macs are not supported."
                .to_string(),
        ))
    }
}

/// Check for KVM support on Linux.
#[cfg(target_os = "linux")]
fn check_linux_kvm() -> Result<VirtualizationSupport> {
    use std::path::Path;

    let kvm_path = Path::new("/dev/kvm");

    if !kvm_path.exists() {
        return Err(BoxError::Other(
            "KVM is not available: /dev/kvm not found. \
             Ensure KVM kernel modules are loaded (modprobe kvm kvm_intel or kvm_amd)."
                .to_string(),
        ));
    }

    // Check if we have read/write access
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(kvm_path)
    {
        Ok(_) => Ok(VirtualizationSupport {
            backend: "KVM".to_string(),
            details: "Linux KVM hardware virtualization is available".to_string(),
        }),
        Err(e) => {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                Err(BoxError::Other(format!(
                    "KVM access denied: {}. Add your user to the 'kvm' group: \
                     sudo usermod -aG kvm $USER",
                    e
                )))
            } else {
                Err(BoxError::Other(format!("Failed to access /dev/kvm: {}", e)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_virtualization_support() {
        // This test will pass or fail depending on the host system
        // It's mainly useful for manual testing
        match check_virtualization_support() {
            Ok(support) => {
                println!("Virtualization supported:");
                println!("  Backend: {}", support.backend);
                println!("  Details: {}", support.details);
            }
            Err(e) => {
                println!("Virtualization not supported: {}", e);
            }
        }
    }
}
