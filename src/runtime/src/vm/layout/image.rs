//! Image authorization, persistent-generation detection, and health compatibility.

use super::*;

pub(super) fn registry_auth_for_image(
    home_dir: &Path,
    reference: &str,
    transient: Option<crate::oci::RegistryAuth>,
) -> Result<crate::oci::RegistryAuth> {
    let parsed = crate::oci::ImageReference::parse(reference)?;
    Ok(transient.unwrap_or_else(|| {
        crate::oci::RegistryAuth::from_credential_store_at(home_dir, &parsed.registry)
    }))
}

pub(crate) fn persistent_rootfs_generation_exists(box_dir: &Path) -> Result<bool> {
    for directory in [box_dir.join("rootfs"), box_dir.join("upper")] {
        match std::fs::read_dir(&directory) {
            Ok(mut entries) => {
                if entries.next().is_some() {
                    return Ok(true);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(BoxError::BuildError(format!(
                    "Failed to inspect persistent rootfs state {}: {error}",
                    directory.display()
                )));
            }
        }
    }

    #[cfg(target_os = "macos")]
    if box_dir.join("rootfs-apfs-v2.sparseimage").is_file() {
        return Ok(true);
    }

    #[cfg(target_os = "macos")]
    match std::fs::symlink_metadata(box_dir.join("rootfs-ext4-v1")) {
        Ok(_) => return Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(BoxError::BuildError(format!(
                "Failed to inspect persistent guest-native rootfs state: {error}"
            )));
        }
    }

    Ok(false)
}

pub(super) fn validate_image_health_support(
    health_check: Option<&crate::oci::OciHealthCheck>,
    healthcheck_disabled: bool,
) -> Result<()> {
    #[cfg(windows)]
    if !healthcheck_disabled && health_check.is_some_and(crate::oci::OciHealthCheck::is_enabled) {
        return Err(BoxError::ConfigError(
            "container health checks are not supported on Windows; disable the image health check explicitly to start this box"
                .to_string(),
        ));
    }

    #[cfg(not(windows))]
    let _ = (health_check, healthcheck_disabled);

    Ok(())
}
