//! Guest hostname and sysctl configuration.

use a3s_box_core::guest_exec::GuestHostConfig;
use std::path::Path;

/// Apply host configuration from the boot environment: pod sysctls and, if
/// present, the hostname.
pub fn apply_from_env() -> Result<(), Box<dyn std::error::Error>> {
    apply_sysctls_from_env();

    let Ok(hostname) = std::env::var("BOX_HOSTNAME") else {
        return Ok(());
    };
    apply_hostname(&hostname, Path::new("/etc/hostname"))
}

/// Apply a validated, runtime-owned host configuration from the MicroVM boot
/// bundle. These writes happen inside the guest and never require a host mount
/// of the root filesystem.
pub fn apply_from_boot_config(config: &GuestHostConfig) -> Result<(), Box<dyn std::error::Error>> {
    apply_from_boot_config_at(
        config,
        Path::new("/etc/hostname"),
        Path::new("/etc/resolv.conf"),
        Path::new("/etc/hosts"),
    )
}

fn apply_from_boot_config_at(
    config: &GuestHostConfig,
    hostname_path: &Path,
    resolv_conf_path: &Path,
    hosts_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    config
        .validate()
        .map_err(|error| format!("invalid guest host configuration: {error}"))?;
    apply_sysctls_from_env();

    if let Some(hostname) = config.hostname.as_deref() {
        apply_hostname(hostname, hostname_path)?;
    }
    if let Some(content) = config.resolv_conf.as_deref() {
        write_host_file(resolv_conf_path, content)?;
    }
    if let Some(content) = config.hosts.as_deref() {
        write_host_file(hosts_path, content)?;
    }
    Ok(())
}

/// Apply pod sysctls passed as `BOX_SYSCTL_<index>=<name>=<value>`.
///
/// Each is written to `/proc/sys/<name with '.' as '/'>`. Best-effort: a sysctl
/// the guest kernel does not expose is logged and skipped rather than aborting
/// VM startup.
fn apply_sysctls_from_env() {
    let mut index = 0;
    while let Ok(spec) = std::env::var(format!("BOX_SYSCTL_{index}")) {
        index += 1;
        let Some((name, value)) = spec.split_once('=') else {
            continue;
        };
        let path = format!("/proc/sys/{}", name.trim().replace('.', "/"));
        match std::fs::write(&path, value) {
            Ok(()) => tracing::info!("Applied sysctl {name}={value}"),
            Err(e) => tracing::warn!("Failed to apply sysctl {name}={value} ({path}): {e}"),
        }
    }
}

fn apply_hostname(hostname: &str, hostname_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    a3s_box_core::dns::validate_hostname(hostname)
        .map_err(|e| format!("invalid BOX_HOSTNAME: {e}"))?;

    set_kernel_hostname(hostname)?;
    write_hostname_file(hostname_path, hostname)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_kernel_hostname(hostname: &str) -> Result<(), Box<dyn std::error::Error>> {
    use std::ffi::CString;

    let hostname = CString::new(hostname.as_bytes())?;
    let ret = unsafe { libc::sethostname(hostname.as_ptr(), hostname.as_bytes().len()) };
    if ret != 0 {
        return Err(Box::new(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn set_kernel_hostname(hostname: &str) -> Result<(), Box<dyn std::error::Error>> {
    let _ = hostname;
    Ok(())
}

fn write_hostname_file(
    hostname_path: &Path,
    hostname: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = hostname_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(hostname_path, format!("{hostname}\n"))?;
    Ok(())
}

fn write_host_file(path: &Path, content: &str) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_write_hostname_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("etc/hostname");

        write_hostname_file(&path, "web").unwrap();

        assert_eq!(std::fs::read_to_string(path).unwrap(), "web\n");
    }

    #[test]
    fn test_apply_hostname_rejects_invalid_hostname_before_write() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("etc/hostname");

        let err = apply_hostname("bad_host", &path).unwrap_err();

        assert!(err.to_string().contains("invalid BOX_HOSTNAME"));
        assert!(!path.exists());
    }

    #[test]
    fn test_apply_boot_config_materializes_guest_owned_files() {
        let dir = TempDir::new().unwrap();
        let hostname = dir.path().join("etc/hostname");
        let resolv = dir.path().join("etc/resolv.conf");
        let hosts = dir.path().join("etc/hosts");
        let config = GuestHostConfig {
            hostname: None,
            resolv_conf: Some("nameserver 1.1.1.1\n".to_string()),
            hosts: Some("127.0.0.1 localhost\n".to_string()),
        };

        apply_from_boot_config_at(&config, &hostname, &resolv, &hosts).unwrap();

        assert!(!hostname.exists());
        assert_eq!(
            std::fs::read_to_string(resolv).unwrap(),
            "nameserver 1.1.1.1\n"
        );
        assert_eq!(
            std::fs::read_to_string(hosts).unwrap(),
            "127.0.0.1 localhost\n"
        );
    }

    #[test]
    fn test_apply_boot_config_rejects_invalid_data_before_writes() {
        let dir = TempDir::new().unwrap();
        let config = GuestHostConfig {
            hostname: Some("bad_host".to_string()),
            resolv_conf: Some("nameserver 1.1.1.1\n".to_string()),
            hosts: None,
        };

        let error = apply_from_boot_config_at(
            &config,
            &dir.path().join("etc/hostname"),
            &dir.path().join("etc/resolv.conf"),
            &dir.path().join("etc/hosts"),
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("invalid guest host configuration"));
        assert!(!dir.path().join("etc").exists());
    }
}
