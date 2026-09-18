//! Host-global runtime control-socket layout.
//!
//! Unix MicroVM/Sandbox control sockets live under a short `/tmp` path so UDS
//! addresses stay within the kernel limit. The shared parent must remain sticky
//! world-writable (`1777`) so a root-lane create cannot permanently brick later
//! unprivileged boots (#560).

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};

/// Host-global parent of per-box runtime socket directories on Unix.
#[cfg(all(unix, target_os = "macos"))]
pub fn shared_runtime_socket_root() -> PathBuf {
    PathBuf::from("/private/tmp").join("a3s-box-sockets")
}

/// Host-global parent of per-box runtime socket directories on Unix.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn shared_runtime_socket_root() -> PathBuf {
    PathBuf::from("/tmp").join("a3s-box-sockets")
}

/// Per-box runtime socket directory.
pub fn runtime_socket_dir(home_dir: &Path, box_id: &str) -> PathBuf {
    #[cfg(unix)]
    {
        let _ = home_dir;
        shared_runtime_socket_root().join(box_id)
    }

    #[cfg(not(unix))]
    {
        home_dir.join("boxes").join(box_id).join("sockets")
    }
}

/// Create the per-box runtime socket directory, keeping the Unix shared parent
/// multi-user safe (`1777` sticky). Per-box directories stay private (`0700`).
pub fn ensure_runtime_socket_dir(home_dir: &Path, box_id: &str) -> Result<PathBuf> {
    let socket_dir = runtime_socket_dir(home_dir, box_id);

    #[cfg(unix)]
    {
        ensure_shared_runtime_socket_root(&shared_runtime_socket_root())?;
        std::fs::create_dir_all(&socket_dir).map_err(|error| BoxError::BoxBootError {
            message: format!(
                "Failed to create socket directory {}: {error}",
                socket_dir.display()
            ),
            hint: shared_socket_root_hint(),
        })?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o700)).map_err(
            |error| BoxError::BoxBootError {
                message: format!(
                    "Failed to set private mode on socket directory {}: {error}",
                    socket_dir.display()
                ),
                hint: None,
            },
        )?;
    }

    #[cfg(not(unix))]
    {
        let _ = home_dir;
        std::fs::create_dir_all(&socket_dir).map_err(|error| BoxError::BoxBootError {
            message: format!(
                "Failed to create socket directory {}: {error}",
                socket_dir.display()
            ),
            hint: None,
        })?;
    }

    Ok(socket_dir)
}

/// Ensure the host-global Unix socket root exists as sticky world-writable.
#[cfg(unix)]
pub fn ensure_shared_runtime_socket_root(shared_root: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if !shared_root.exists() {
        std::fs::create_dir_all(shared_root).map_err(|error| BoxError::BoxBootError {
            message: format!(
                "Failed to create shared socket root {}: {error}",
                shared_root.display()
            ),
            hint: None,
        })?;
    }

    let metadata = std::fs::metadata(shared_root).map_err(|error| BoxError::BoxBootError {
        message: format!(
            "Failed to inspect shared socket root {}: {error}",
            shared_root.display()
        ),
        hint: None,
    })?;
    if !metadata.is_dir() {
        return Err(BoxError::BoxBootError {
            message: format!(
                "Shared socket root {} exists and is not a directory",
                shared_root.display()
            ),
            hint: shared_socket_root_hint(),
        });
    }

    let mode = metadata.permissions().mode() & 0o7777;
    if mode == 0o1777 {
        return Ok(());
    }

    match std::fs::set_permissions(shared_root, std::fs::Permissions::from_mode(0o1777)) {
        Ok(()) => Ok(()),
        Err(error) => {
            if let Ok(again) = std::fs::metadata(shared_root) {
                if again.permissions().mode() & 0o7777 == 0o1777 {
                    return Ok(());
                }
            }
            Err(BoxError::BoxBootError {
                message: format!(
                    "Shared socket root {} is not sticky world-writable (mode {:04o}) and could not be fixed: {error}",
                    shared_root.display(),
                    mode
                ),
                hint: shared_socket_root_hint(),
            })
        }
    }
}

#[cfg(unix)]
fn shared_socket_root_hint() -> Option<String> {
    Some(format!(
        "A prior root-lane box left {} as a non-shared directory. As root run `chmod 1777 {}` (or remove it) so every user can create per-box socket dirs.",
        shared_runtime_socket_root().display(),
        shared_runtime_socket_root().display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn unique_tmp_subdir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("a3s-box-socket-root-{label}-{nanos}"))
    }

    #[cfg(unix)]
    #[test]
    fn ensure_shared_root_creates_sticky_world_writable() {
        let root = unique_tmp_subdir("create");
        ensure_shared_runtime_socket_root(&root).unwrap();
        let mode = fs::metadata(&root).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o1777, "shared root must be sticky world-writable");
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_shared_root_upgrades_private_0755() {
        let root = unique_tmp_subdir("upgrade");
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_shared_runtime_socket_root(&root).unwrap();
        let mode = fs::metadata(&root).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o1777);
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_runtime_socket_dir_keeps_per_box_private() {
        let home = unique_tmp_subdir("home");
        fs::create_dir_all(&home).unwrap();
        let box_id = format!(
            "test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        );
        let dir = ensure_runtime_socket_dir(&home, &box_id).unwrap();
        assert_eq!(dir, runtime_socket_dir(&home, &box_id));
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o700, "per-box socket dir must stay private");
        let shared = shared_runtime_socket_root();
        let shared_mode = fs::metadata(&shared).unwrap().permissions().mode() & 0o7777;
        assert_eq!(shared_mode, 0o1777);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&home);
    }
}
