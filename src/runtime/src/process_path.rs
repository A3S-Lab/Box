//! Linux guest process path validation shared by initial launch and exec.

use std::path::Path;

use a3s_box_core::error::{BoxError, Result};

pub(crate) const DEFAULT_CONTAINER_PATH: &str =
    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

pub(crate) fn resolve_runtime_executable(
    rootfs: &Path,
    cwd: &str,
    path: &str,
    command: &str,
) -> Result<String> {
    if command.is_empty() || command.contains('\0') {
        return Err(BoxError::ConfigError(
            "Sandbox runtime-owned executable must be non-empty and contain no NUL".to_string(),
        ));
    }

    if command.contains('/') {
        let candidate = normalize_linux_guest_path(cwd, command, "process executable")?;
        require_runtime_executable(rootfs, &candidate)?;
        return Ok(candidate);
    }

    for directory in path.split(':') {
        let directory = if directory.is_empty() { cwd } else { directory };
        let directory = normalize_linux_guest_path(cwd, directory, "PATH entry")?;
        let candidate =
            normalize_linux_guest_path(&directory, command, "PATH-resolved process executable")?;
        if runtime_executable_exists(rootfs, &candidate)? {
            return Ok(candidate);
        }
    }
    Err(BoxError::BoxBootError {
        message: format!(
            "Sandbox executable {command:?} was not found as an executable file in the prepared rootfs PATH"
        ),
        hint: Some("Use an absolute command or include it in the container PATH".to_string()),
    })
}

fn require_runtime_executable(rootfs: &Path, guest_path: &str) -> Result<()> {
    if runtime_executable_exists(rootfs, guest_path)? {
        Ok(())
    } else {
        Err(BoxError::BoxBootError {
            message: format!(
                "Sandbox executable {guest_path:?} is missing, not regular, or not executable in the prepared rootfs"
            ),
            hint: None,
        })
    }
}

fn runtime_executable_exists(rootfs: &Path, guest_path: &str) -> Result<bool> {
    let host_path =
        crate::oci::rootfs::resolve_guest_file_path(rootfs, guest_path.trim_start_matches('/'))?;
    let metadata = match std::fs::metadata(&host_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(BoxError::IoError(error)),
    };
    if !metadata.is_file() {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn normalize_linux_guest_path(base: &str, value: &str, label: &str) -> Result<String> {
    if value.is_empty() || value.contains('\0') {
        return Err(BoxError::ConfigError(format!(
            "Sandbox {label} must be non-empty and contain no NUL"
        )));
    }
    if !base.starts_with('/') {
        return Err(BoxError::ConfigError(format!(
            "Sandbox {label} base is not absolute: {base:?}"
        )));
    }

    let mut components = if value.starts_with('/') {
        Vec::new()
    } else {
        base.split('/')
            .filter(|component| !component.is_empty())
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    };
    for component in value.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if components.pop().is_none() {
                    return Err(BoxError::ConfigError(format!(
                        "Sandbox {label} escapes the container root: {value:?}"
                    )));
                }
            }
            component => components.push(component.to_string()),
        }
    }
    Ok(format!("/{}", components.join("/")))
}
