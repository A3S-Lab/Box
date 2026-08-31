//! Private host-to-guest boot and terminal control shares.

use super::*;
use a3s_box_core::rootfs_baseline::GUEST_DIFF_BASELINE_FILE_NAME;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

pub(super) fn secure_guest_control_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(
            |error| BoxError::BoxBootError {
                message: format!(
                    "failed to secure guest control file {}: {error}",
                    path.display()
                ),
                hint: None,
            },
        )?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn secure_guest_control_directory(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(BoxError::BoxBootError {
                message: format!(
                    "guest boot control path is not a private directory: {}",
                    path.display()
                ),
                hint: None,
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path).map_err(|error| BoxError::BoxBootError {
                message: format!(
                    "failed to create guest boot control directory {}: {error}",
                    path.display()
                ),
                hint: None,
            })?;
        }
        Err(error) => {
            return Err(BoxError::BoxBootError {
                message: format!(
                    "failed to inspect guest boot control directory {}: {error}",
                    path.display()
                ),
                hint: None,
            });
        }
    }

    #[cfg(unix)]
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|error| {
        BoxError::BoxBootError {
            message: format!(
                "failed to secure guest boot control directory {}: {error}",
                path.display()
            ),
            hint: None,
        }
    })?;
    Ok(())
}

pub(super) fn write_guest_boot_config(
    control_dir: &Path,
    config: &GuestBootConfig,
) -> Result<PathBuf> {
    config
        .validate()
        .map_err(|message| BoxError::BoxBootError {
            message,
            hint: Some("correct the workload launch or host configuration".to_string()),
        })?;
    let bytes = serde_json::to_vec(config).map_err(|error| BoxError::BoxBootError {
        message: format!("failed to serialize guest boot configuration: {error}"),
        hint: None,
    })?;
    if bytes.len() > MAX_GUEST_BOOT_CONFIG_BYTES {
        return Err(BoxError::BoxBootError {
            message: format!(
                "guest boot configuration is {} bytes; limit is {} bytes",
                bytes.len(),
                MAX_GUEST_BOOT_CONFIG_BYTES
            ),
            hint: Some("reduce command arguments or environment data".to_string()),
        });
    }

    let path = crate::oci::rootfs::replace_guest_file_no_follow(
        control_dir,
        GUEST_BOOT_CONFIG_FILE_NAME,
        bytes,
    )?;
    secure_guest_control_file(&path)?;
    Ok(path)
}

pub(super) fn stage_guest_boot_config(
    layout: &BoxLayout,
    config: &GuestBootConfig,
) -> Result<FsMount> {
    let runtime_dir = layout
        .exec_socket_path
        .parent()
        .ok_or_else(|| BoxError::BoxBootError {
            message: format!(
                "guest exec socket has no runtime directory: {}",
                layout.exec_socket_path.display()
            ),
            hint: None,
        })?;
    stage_guest_boot_config_in_runtime_dir(runtime_dir, config)
}

pub(crate) fn stage_guest_boot_config_in_runtime_dir(
    runtime_dir: &Path,
    config: &GuestBootConfig,
) -> Result<FsMount> {
    let control_dir = runtime_dir.join("boot-control");
    secure_guest_control_directory(&control_dir)?;
    write_guest_boot_config(&control_dir, config)?;
    let control_dir = control_dir
        .canonicalize()
        .map_err(|error| BoxError::BoxBootError {
            message: format!(
                "failed to resolve guest boot control directory {}: {error}",
                control_dir.display()
            ),
            hint: None,
        })?;

    Ok(FsMount {
        tag: GUEST_BOOT_CONTROL_TAG.to_string(),
        host_path: control_dir,
        read_only: true,
    })
}

pub(super) fn stage_guest_terminal_control(
    box_dir: &Path,
    capture_diff_baseline: bool,
) -> Result<FsMount> {
    let control_dir = box_dir.join("runtime-control");
    secure_guest_control_directory(&control_dir)?;
    let status_path = crate::oci::rootfs::replace_guest_file_no_follow(
        &control_dir,
        GUEST_TERMINAL_STATUS_FILE_NAME,
        [],
    )?;
    secure_guest_control_file(&status_path)?;
    if capture_diff_baseline {
        let baseline_path = crate::oci::rootfs::replace_guest_file_no_follow(
            &control_dir,
            GUEST_DIFF_BASELINE_FILE_NAME,
            [],
        )?;
        secure_guest_control_file(&baseline_path)?;
    } else {
        crate::oci::rootfs::remove_guest_entry_no_follow(
            &control_dir,
            GUEST_DIFF_BASELINE_FILE_NAME,
        )?;
    }
    let control_dir = control_dir
        .canonicalize()
        .map_err(|error| BoxError::BoxBootError {
            message: format!(
                "failed to resolve guest terminal control directory {}: {error}",
                control_dir.display()
            ),
            hint: None,
        })?;

    Ok(FsMount {
        tag: GUEST_TERMINAL_CONTROL_TAG.to_string(),
        host_path: control_dir,
        read_only: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_control_stages_baseline_only_when_guest_owns_it() {
        let directory = tempfile::tempdir().unwrap();
        let without_baseline = directory.path().join("host-owned");
        let with_baseline = directory.path().join("guest-owned");

        let host_mount = stage_guest_terminal_control(&without_baseline, false).unwrap();
        assert!(host_mount
            .host_path
            .join(GUEST_TERMINAL_STATUS_FILE_NAME)
            .is_file());
        assert!(!host_mount
            .host_path
            .join(GUEST_DIFF_BASELINE_FILE_NAME)
            .exists());

        let guest_mount = stage_guest_terminal_control(&with_baseline, true).unwrap();
        assert!(guest_mount
            .host_path
            .join(GUEST_TERMINAL_STATUS_FILE_NAME)
            .is_file());
        assert!(guest_mount
            .host_path
            .join(GUEST_DIFF_BASELINE_FILE_NAME)
            .is_file());

        let reused_mount = stage_guest_terminal_control(&with_baseline, false).unwrap();
        assert!(!reused_mount
            .host_path
            .join(GUEST_DIFF_BASELINE_FILE_NAME)
            .exists());
    }

    #[cfg(unix)]
    #[test]
    fn terminal_control_files_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let mount = stage_guest_terminal_control(directory.path(), true).unwrap();

        assert_eq!(
            std::fs::metadata(&mount.host_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );
        for file_name in [
            GUEST_TERMINAL_STATUS_FILE_NAME,
            GUEST_DIFF_BASELINE_FILE_NAME,
        ] {
            assert_eq!(
                std::fs::metadata(mount.host_path.join(file_name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o7777,
                0o600
            );
        }
    }
}
