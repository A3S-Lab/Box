//! Filesystem mount management for virtio-fs

use a3s_box_core::error::{BoxError, Result};
use std::path::{Path, PathBuf};

/// Mount point configuration
#[derive(Debug, Clone)]
pub struct MountPoint {
    /// Host path
    pub host_path: PathBuf,

    /// Guest path
    pub guest_path: PathBuf,

    /// Read-only
    pub readonly: bool,
}

/// Filesystem manager
pub struct FsManager {
    /// Mount points
    mounts: Vec<MountPoint>,
}

impl FsManager {
    /// Create a new filesystem manager
    pub fn new() -> Self {
        Self { mounts: Vec::new() }
    }

    /// Add a mount point
    pub fn add_mount(
        &mut self,
        host_path: impl AsRef<Path>,
        guest_path: impl AsRef<Path>,
        readonly: bool,
    ) {
        self.mounts.push(MountPoint {
            host_path: host_path.as_ref().to_path_buf(),
            guest_path: guest_path.as_ref().to_path_buf(),
            readonly,
        });
    }

    /// Setup default mounts for A3S Box
    pub fn setup_default_mounts(
        &mut self,
        workspace: impl AsRef<Path>,
        skills: &[PathBuf],
        cache: impl AsRef<Path>,
    ) -> Result<()> {
        // Workspace mount (read-write)
        self.add_mount(workspace, "/a3s/workspace", false);

        // Skills mounts (read-only)
        for (i, skill_dir) in skills.iter().enumerate() {
            if skill_dir.exists() {
                self.add_mount(skill_dir, format!("/a3s/skills/{}", i), true);
            }
        }

        // Cache mount (read-write, persistent)
        self.add_mount(cache, "/a3s/cache", false);

        Ok(())
    }

    /// Get all mount points
    pub fn mounts(&self) -> &[MountPoint] {
        &self.mounts
    }

    /// Apply mounts to VM (placeholder)
    pub async fn apply_mounts(&self) -> Result<()> {
        // TODO: Implement actual virtio-fs mount setup with libkrun
        // This would involve:
        // 1. Creating virtio-fs devices for each mount
        // 2. Configuring the VM to expose these devices
        // 3. Guest kernel automatically mounts them

        for mount in &self.mounts {
            tracing::debug!(
                "Mount: {} -> {} (readonly: {})",
                mount.host_path.display(),
                mount.guest_path.display(),
                mount.readonly
            );
        }

        Ok(())
    }
}

impl Default for FsManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Ensure cache directory exists
pub async fn ensure_cache_dir() -> Result<PathBuf> {
    let cache_dir = dirs::cache_dir()
        .ok_or_else(|| BoxError::ConfigError("Cannot determine cache directory".to_string()))?
        .join("a3s-box");

    tokio::fs::create_dir_all(&cache_dir).await?;

    Ok(cache_dir)
}

// External dependency for cache directory
mod dirs {
    use std::path::PathBuf;

    pub fn cache_dir() -> Option<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Caches"))
        }

        #[cfg(target_os = "linux")]
        {
            std::env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        }

        #[cfg(target_os = "windows")]
        {
            std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
        }
    }
}
