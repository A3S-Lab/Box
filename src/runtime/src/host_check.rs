//! Host virtualization support detection.
//!
//! Checks if the current host supports hardware virtualization:
//! - macOS: Hypervisor.framework (Apple Silicon only)
//! - Linux: KVM (/dev/kvm)
//! - Windows: WHPX / Windows Hypervisor Platform

use a3s_box_core::error::{BoxError, Result};

/// Information about virtualization support.
#[derive(Debug, Clone)]
pub struct VirtualizationSupport {
    /// Human-readable description of the virtualization backend.
    pub backend: String,
    /// Additional details about the support.
    pub details: String,
}

/// Reject an isolation class this host cannot launch.
///
/// The requested class is unchanged. A missing MicroVM hypervisor or a missing
/// Sandbox driver fails here, before a box directory or VM exists. Windows
/// never substitutes KVM or a WSL distro, and Linux never substitutes WHPX.
///
/// This is the launch-ready probe: Linux opens `/dev/kvm`, Windows queries WHPX.
/// Call it from CLI `preflight_isolation` and from `start`, not from durable
/// create reservation (unit tests and stub CI often lack kvm-group access).
pub fn admit_requested_isolation(isolation: a3s_box_core::ExecutionIsolation) -> Result<()> {
    match isolation {
        a3s_box_core::ExecutionIsolation::Microvm => check_virtualization_support().map(|_| ()),
        a3s_box_core::ExecutionIsolation::Sandbox => {
            crate::sandbox::probe_sandbox_capabilities(None).require_ready()?;
            Ok(())
        }
    }
}

/// Reject an isolation class this OS cannot host, without opening the device.
///
/// Used by durable create reservation so metadata/unit tests can stamp a
/// MicroVM route when `/dev/kvm` exists but is not group-accessible. Launch
/// still requires [`admit_requested_isolation`]. Sandbox stays Linux-only.
pub fn admit_isolation_class(isolation: a3s_box_core::ExecutionIsolation) -> Result<()> {
    match isolation {
        a3s_box_core::ExecutionIsolation::Microvm => admit_microvm_host_class(),
        a3s_box_core::ExecutionIsolation::Sandbox => {
            #[cfg(target_os = "linux")]
            {
                Ok(())
            }
            #[cfg(not(target_os = "linux"))]
            {
                Err(BoxError::ConfigError(
                    "Sandbox isolation is supported only on Linux; use MicroVM isolation on this host"
                        .to_string(),
                ))
            }
        }
    }
}

fn admit_microvm_host_class() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        if !std::path::Path::new("/dev/kvm").exists() {
            return Err(BoxError::ConfigError(
                "KVM is not available: /dev/kvm not found. \
                 Ensure KVM kernel modules are loaded (modprobe kvm kvm_intel or kvm_amd)."
                    .to_string(),
            ));
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        #[cfg(target_arch = "aarch64")]
        {
            Ok(())
        }
        #[cfg(not(target_arch = "aarch64"))]
        {
            Err(BoxError::ConfigError(
                "A3S Box on macOS requires Apple Silicon (ARM64). Intel Macs are not supported."
                    .to_string(),
            ))
        }
    }

    #[cfg(windows)]
    {
        #[cfg(target_arch = "x86_64")]
        {
            Ok(())
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            Err(BoxError::ConfigError(
                "A3S Box on Windows currently requires x86_64 for the WHPX backend.".to_string(),
            ))
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        Err(BoxError::ConfigError(
            "Unsupported platform: A3S Box requires macOS (Apple Silicon), Linux with KVM, or Windows with WHPX"
                .to_string(),
        ))
    }
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

    #[cfg(target_os = "windows")]
    {
        check_windows_whpx()
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Err(BoxError::ConfigError(
            "Unsupported platform: A3S Box requires macOS (Apple Silicon), Linux with KVM, or Windows with WHPX"
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
            .map_err(|e| BoxError::ExecError(format!("Failed to run sysctl: {}", e)))?;

        if !output.status.success() {
            return Err(BoxError::ConfigError(
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
            Err(BoxError::ConfigError(
                "Hypervisor.framework is not available on this system. \
                 Ensure you are running on Apple Silicon and have the necessary entitlements."
                    .to_string(),
            ))
        }
    }

    #[cfg(not(target_arch = "aarch64"))]
    {
        Err(BoxError::ConfigError(
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
        return Err(BoxError::ConfigError(
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
                Err(BoxError::ConfigError(format!(
                    "KVM access denied: {}. Add your user to the 'kvm' group: \
                     sudo usermod -aG kvm $USER",
                    e
                )))
            } else {
                Err(BoxError::ConfigError(format!(
                    "Failed to access /dev/kvm: {}",
                    e
                )))
            }
        }
    }
}

/// Check for Windows Hypervisor Platform (WHPX) support on Windows.
#[cfg(target_os = "windows")]
fn check_windows_whpx() -> Result<VirtualizationSupport> {
    #[cfg(not(target_arch = "x86_64"))]
    {
        return Err(BoxError::ConfigError(
            "A3S Box on Windows currently requires x86_64 for the WHPX backend.".to_string(),
        ));
    }

    use windows_sys::Win32::Foundation::BOOL;
    use windows_sys::Win32::System::Hypervisor::{
        WHvCapabilityCodeHypervisorPresent, WHvGetCapability,
    };

    let mut present: BOOL = 0;
    let mut written = 0_u32;
    // SAFETY: both output pointers refer to initialized, writable values for
    // the exact buffer sizes passed to the Windows Hypervisor Platform API.
    let status = unsafe {
        WHvGetCapability(
            WHvCapabilityCodeHypervisorPresent,
            (&mut present as *mut BOOL).cast(),
            std::mem::size_of::<BOOL>() as u32,
            &mut written,
        )
    };
    if status < 0 {
        return Err(BoxError::ConfigError(format!(
            "Failed to query Windows Hypervisor Platform capability (HRESULT 0x{:08X})",
            status as u32
        )));
    }
    if written == std::mem::size_of::<BOOL>() as u32 && present != 0 {
        Ok(VirtualizationSupport {
            backend: "WHPX".to_string(),
            details: "Windows Hypervisor Platform is available".to_string(),
        })
    } else {
        Err(BoxError::ConfigError(
            "Windows Hypervisor Platform is not available. Enable the 'Windows Hypervisor Platform' optional feature, verify firmware virtualization, and reboot."
                .to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejects_wsl_wording(message: &str) {
        let lower = message.to_lowercase();
        assert!(
            !lower.contains("wsl"),
            "virtualization diagnostic must not send the operator to WSL: {message}"
        );
    }

    #[test]
    fn virtualization_probe_names_this_host_and_does_not_require_wsl() {
        match check_virtualization_support() {
            Ok(support) => {
                #[cfg(windows)]
                assert_eq!(support.backend, "WHPX");
                #[cfg(target_os = "linux")]
                assert_eq!(support.backend, "KVM");
                #[cfg(target_os = "macos")]
                assert!(support.backend.contains("Hypervisor"));
                rejects_wsl_wording(&support.backend);
                rejects_wsl_wording(&support.details);
            }
            Err(error) => {
                let message = error.to_string();
                rejects_wsl_wording(&message);
                #[cfg(windows)]
                assert!(
                    message.contains("WHPX")
                        || message.to_lowercase().contains("hypervisor")
                        || message.contains("x86_64"),
                    "{message}"
                );
                #[cfg(target_os = "linux")]
                {
                    assert!(
                        message.contains("KVM") || message.contains("/dev/kvm"),
                        "{message}"
                    );
                    assert!(!message.contains("WHPX"), "{message}");
                }
                #[cfg(windows)]
                assert!(!message.contains("KVM"), "{message}");
            }
        }
    }

    #[test]
    fn sandbox_admission_keeps_the_requested_class() {
        let isolation = a3s_box_core::ExecutionIsolation::Sandbox;
        let result = admit_requested_isolation(isolation);
        assert_eq!(isolation, a3s_box_core::ExecutionIsolation::Sandbox);
        match result {
            Ok(()) => {
                #[cfg(not(target_os = "linux"))]
                panic!("shared-kernel Sandbox must fail closed off Linux");
            }
            Err(error) => {
                let message = error.to_string();
                rejects_wsl_wording(&message);
                assert!(
                    !message.contains("WHPX"),
                    "Sandbox admission must not select WHPX: {message}"
                );
                #[cfg(not(target_os = "linux"))]
                assert!(
                    message.contains("only on Linux"),
                    "missing Sandbox support must name Linux: {message}"
                );
                #[cfg(target_os = "linux")]
                assert!(
                    message.contains("Sandbox"),
                    "missing Sandbox driver must name the Sandbox probe: {message}"
                );
            }
        }
    }

    #[test]
    fn create_class_admission_does_not_require_opening_the_hypervisor() {
        let isolation = a3s_box_core::ExecutionIsolation::Microvm;
        match admit_isolation_class(isolation) {
            Ok(()) => {}
            Err(error) => {
                let message = error.to_string();
                rejects_wsl_wording(&message);
                assert!(
                    !message.to_lowercase().contains("access denied"),
                    "class admit must not open /dev/kvm: {message}"
                );
                assert!(
                    !message.to_lowercase().contains("kvm group"),
                    "class admit must not require kvm group: {message}"
                );
            }
        }
        assert_eq!(isolation, a3s_box_core::ExecutionIsolation::Microvm);
        #[cfg(not(target_os = "linux"))]
        {
            let sandbox = a3s_box_core::ExecutionIsolation::Sandbox;
            let error = admit_isolation_class(sandbox).expect_err("Sandbox off Linux");
            assert!(error.to_string().contains("only on Linux"));
        }
    }

    #[test]
    fn microvm_admission_does_not_rewrite_to_sandbox() {
        let isolation = a3s_box_core::ExecutionIsolation::Microvm;
        if let Err(error) = admit_requested_isolation(isolation) {
            let message = error.to_string().to_lowercase();
            assert!(
                !message.contains("sandbox"),
                "MicroVM preflight must not offer Sandbox as a replacement: {message}"
            );
            rejects_wsl_wording(&message);
        }
        assert_eq!(isolation, a3s_box_core::ExecutionIsolation::Microvm);
    }
}
