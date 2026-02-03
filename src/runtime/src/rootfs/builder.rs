//! Rootfs builder for guest VM.
//!
//! Creates a minimal rootfs containing:
//! - Basic directory structure
//! - Guest agent binary
//! - Essential configuration files

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};

use super::layout::GuestLayout;

/// Builder for creating guest rootfs.
pub struct RootfsBuilder {
    /// Target rootfs directory.
    rootfs_path: PathBuf,

    /// Path to the guest agent binary on the host.
    agent_binary_path: Option<PathBuf>,

    /// Guest layout configuration.
    layout: GuestLayout,
}

impl RootfsBuilder {
    /// Create a new rootfs builder.
    pub fn new(rootfs_path: impl Into<PathBuf>) -> Self {
        Self {
            rootfs_path: rootfs_path.into(),
            agent_binary_path: None,
            layout: GuestLayout::default(),
        }
    }

    /// Set the path to the guest agent binary.
    pub fn with_agent_binary(mut self, path: impl Into<PathBuf>) -> Self {
        self.agent_binary_path = Some(path.into());
        self
    }

    /// Set a custom guest layout.
    pub fn with_layout(mut self, layout: GuestLayout) -> Self {
        self.layout = layout;
        self
    }

    /// Build the rootfs.
    pub fn build(&self) -> Result<()> {
        tracing::info!(
            rootfs = %self.rootfs_path.display(),
            "Building guest rootfs"
        );

        // Create base directory
        fs::create_dir_all(&self.rootfs_path).map_err(|e| {
            BoxError::Other(format!(
                "Failed to create rootfs directory {}: {}",
                self.rootfs_path.display(),
                e
            ))
        })?;

        // Create directory structure
        self.create_directories()?;

        // Create essential files
        self.create_essential_files()?;

        // Copy guest agent binary
        if let Some(agent_path) = &self.agent_binary_path {
            self.copy_agent_binary(agent_path)?;
        }

        tracing::info!("Guest rootfs built successfully");
        Ok(())
    }

    /// Create the directory structure.
    fn create_directories(&self) -> Result<()> {
        for dir in self.layout.required_dirs() {
            let full_path = self.rootfs_path.join(dir.trim_start_matches('/'));
            fs::create_dir_all(&full_path).map_err(|e| {
                BoxError::Other(format!(
                    "Failed to create directory {}: {}",
                    full_path.display(),
                    e
                ))
            })?;
            tracing::debug!(dir = %full_path.display(), "Created directory");
        }
        Ok(())
    }

    /// Create essential configuration files.
    fn create_essential_files(&self) -> Result<()> {
        // /etc/passwd - minimal user database
        let passwd_content =
            "root:x:0:0:root:/root:/bin/sh\nnobody:x:65534:65534:nobody:/:/bin/false\n";
        self.write_file("etc/passwd", passwd_content)?;

        // /etc/group - minimal group database
        let group_content = "root:x:0:\nnogroup:x:65534:\n";
        self.write_file("etc/group", group_content)?;

        // /etc/hosts - basic hosts file
        let hosts_content = "127.0.0.1\tlocalhost\n::1\t\tlocalhost\n";
        self.write_file("etc/hosts", hosts_content)?;

        // /etc/resolv.conf - DNS configuration (can be overridden)
        let resolv_content = "nameserver 8.8.8.8\nnameserver 8.8.4.4\n";
        self.write_file("etc/resolv.conf", resolv_content)?;

        // /etc/nsswitch.conf - name service switch configuration
        let nsswitch_content = "passwd: files\ngroup: files\nhosts: files dns\n";
        self.write_file("etc/nsswitch.conf", nsswitch_content)?;

        Ok(())
    }

    /// Copy the guest agent binary to the rootfs.
    fn copy_agent_binary(&self, source: &Path) -> Result<()> {
        if !source.exists() {
            return Err(BoxError::Other(format!(
                "Guest agent binary not found: {}",
                source.display()
            )));
        }

        let agent_dir = self
            .rootfs_path
            .join(self.layout.agent_dir.trim_start_matches('/'));
        fs::create_dir_all(&agent_dir)
            .map_err(|e| BoxError::Other(format!("Failed to create agent directory: {}", e)))?;

        let dest = agent_dir.join("a3s-box-code");

        // Check if we need to update (compare mtime and size)
        if dest.exists() {
            let src_meta = fs::metadata(source)
                .map_err(|e| BoxError::Other(format!("Failed to read source metadata: {}", e)))?;
            let dst_meta = fs::metadata(&dest)
                .map_err(|e| BoxError::Other(format!("Failed to read dest metadata: {}", e)))?;

            if src_meta.len() == dst_meta.len() {
                if let (Ok(src_mtime), Ok(dst_mtime)) = (src_meta.modified(), dst_meta.modified()) {
                    if src_mtime <= dst_mtime {
                        tracing::debug!("Guest agent binary is up to date");
                        return Ok(());
                    }
                }
            }
        }

        tracing::info!(
            src = %source.display(),
            dest = %dest.display(),
            "Copying guest agent binary"
        );

        fs::copy(source, &dest)
            .map_err(|e| BoxError::Other(format!("Failed to copy agent binary: {}", e)))?;

        // Set executable permissions
        let mut perms = fs::metadata(&dest)
            .map_err(|e| BoxError::Other(format!("Failed to read permissions: {}", e)))?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&dest, perms)
            .map_err(|e| BoxError::Other(format!("Failed to set permissions: {}", e)))?;

        Ok(())
    }

    /// Write a file to the rootfs.
    fn write_file(&self, relative_path: &str, content: &str) -> Result<()> {
        let full_path = self.rootfs_path.join(relative_path);

        // Ensure parent directory exists
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                BoxError::Other(format!("Failed to create parent directory: {}", e))
            })?;
        }

        fs::write(&full_path, content).map_err(|e| {
            BoxError::Other(format!("Failed to write {}: {}", full_path.display(), e))
        })?;

        tracing::debug!(path = %full_path.display(), "Created file");
        Ok(())
    }
}

/// Find the guest agent binary.
///
/// Searches in the following order:
/// 1. A3S_AGENT_PATH environment variable
/// 2. Same directory as the current executable
/// 3. Common installation paths
pub fn find_agent_binary() -> Result<PathBuf> {
    // Check environment variable
    if let Ok(path) = std::env::var("A3S_AGENT_PATH") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Ok(path);
        }
    }

    // Check same directory as current executable
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            let agent_path = exe_dir.join("a3s-box-code");
            if agent_path.exists() {
                return Ok(agent_path);
            }
        }
    }

    // Check common paths
    let common_paths = [
        "/usr/local/bin/a3s-box-code",
        "/usr/bin/a3s-box-code",
        "./target/release/a3s-box-code",
        "./target/debug/a3s-box-code",
    ];

    for path in common_paths {
        let path = PathBuf::from(path);
        if path.exists() {
            return Ok(path);
        }
    }

    Err(BoxError::Other(
        "Guest agent binary (a3s-box-code) not found. \
         Set A3S_AGENT_PATH or ensure it's in the same directory as the runtime."
            .to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_rootfs_builder_creates_directories() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");

        let builder = RootfsBuilder::new(&rootfs_path);
        builder.build().unwrap();

        // Check directories were created
        assert!(rootfs_path.join("dev").exists());
        assert!(rootfs_path.join("proc").exists());
        assert!(rootfs_path.join("sys").exists());
        assert!(rootfs_path.join("tmp").exists());
        assert!(rootfs_path.join("etc").exists());
        assert!(rootfs_path.join("a3s/agent").exists());
        assert!(rootfs_path.join("workspace").exists());
        assert!(rootfs_path.join("skills").exists());
    }

    #[test]
    fn test_rootfs_builder_creates_essential_files() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");

        let builder = RootfsBuilder::new(&rootfs_path);
        builder.build().unwrap();

        // Check files were created
        assert!(rootfs_path.join("etc/passwd").exists());
        assert!(rootfs_path.join("etc/group").exists());
        assert!(rootfs_path.join("etc/hosts").exists());
        assert!(rootfs_path.join("etc/resolv.conf").exists());

        // Check content
        let passwd = fs::read_to_string(rootfs_path.join("etc/passwd")).unwrap();
        assert!(passwd.contains("root:x:0:0"));
    }

    #[test]
    fn test_rootfs_builder_essential_files_content() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");

        RootfsBuilder::new(&rootfs_path).build().unwrap();

        // Verify /etc/passwd content
        let passwd = fs::read_to_string(rootfs_path.join("etc/passwd")).unwrap();
        assert!(passwd.contains("root:x:0:0:root:/root:/bin/sh"));
        assert!(passwd.contains("nobody:x:65534:65534"));

        // Verify /etc/group content
        let group = fs::read_to_string(rootfs_path.join("etc/group")).unwrap();
        assert!(group.contains("root:x:0:"));
        assert!(group.contains("nogroup:x:65534:"));

        // Verify /etc/hosts content
        let hosts = fs::read_to_string(rootfs_path.join("etc/hosts")).unwrap();
        assert!(hosts.contains("127.0.0.1"));
        assert!(hosts.contains("localhost"));

        // Verify /etc/resolv.conf content
        let resolv = fs::read_to_string(rootfs_path.join("etc/resolv.conf")).unwrap();
        assert!(resolv.contains("nameserver"));

        // Verify /etc/nsswitch.conf content
        let nsswitch = fs::read_to_string(rootfs_path.join("etc/nsswitch.conf")).unwrap();
        assert!(nsswitch.contains("passwd: files"));
        assert!(nsswitch.contains("hosts: files dns"));
    }

    #[test]
    fn test_rootfs_builder_with_agent_binary() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");

        // Create a fake agent binary
        let fake_agent = temp_dir.path().join("fake-agent");
        fs::write(&fake_agent, b"#!/bin/sh\necho hello").unwrap();

        let builder = RootfsBuilder::new(&rootfs_path).with_agent_binary(&fake_agent);
        builder.build().unwrap();

        // Verify agent binary was copied
        let dest = rootfs_path.join("a3s/agent/a3s-box-code");
        assert!(dest.exists());

        // Verify content matches
        let content = fs::read(&dest).unwrap();
        assert_eq!(content, b"#!/bin/sh\necho hello");

        // Verify executable permissions
        let perms = fs::metadata(&dest).unwrap().permissions();
        assert_eq!(perms.mode() & 0o755, 0o755);
    }

    #[test]
    fn test_rootfs_builder_agent_binary_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");

        let builder =
            RootfsBuilder::new(&rootfs_path).with_agent_binary("/nonexistent/path/to/agent");

        let result = builder.build();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_rootfs_builder_with_custom_layout() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");

        let custom_layout = GuestLayout {
            base_dir: "/custom",
            agent_dir: "/custom/bin",
            workspace_dir: "/work",
            skills_dir: "/plugins",
            tmp_dir: "/tmp",
            run_dir: "/run",
        };

        let builder = RootfsBuilder::new(&rootfs_path).with_layout(custom_layout);
        builder.build().unwrap();

        // Verify custom directories were created
        assert!(rootfs_path.join("custom/bin").exists());
        assert!(rootfs_path.join("work").exists());
        assert!(rootfs_path.join("plugins").exists());
    }

    #[test]
    fn test_rootfs_builder_idempotent() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");

        let builder = RootfsBuilder::new(&rootfs_path);

        // Build twice should succeed
        builder.build().unwrap();
        builder.build().unwrap();

        // Verify everything still exists
        assert!(rootfs_path.join("etc/passwd").exists());
        assert!(rootfs_path.join("dev").exists());
    }

    #[test]
    fn test_rootfs_builder_agent_binary_skip_if_unchanged() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");

        // Create a fake agent binary
        let fake_agent = temp_dir.path().join("fake-agent");
        fs::write(&fake_agent, b"binary content").unwrap();

        let builder = RootfsBuilder::new(&rootfs_path).with_agent_binary(&fake_agent);

        // First build
        builder.build().unwrap();
        let dest = rootfs_path.join("a3s/agent/a3s-box-code");
        let first_mtime = fs::metadata(&dest).unwrap().modified().unwrap();

        // Small delay to ensure different mtime if file is rewritten
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Second build should skip copy (same size, dest mtime >= src mtime)
        builder.build().unwrap();
        let second_mtime = fs::metadata(&dest).unwrap().modified().unwrap();

        // mtime should be unchanged (file not rewritten)
        assert_eq!(first_mtime, second_mtime);
    }

    #[test]
    fn test_find_agent_binary_from_env() {
        let temp_dir = TempDir::new().unwrap();
        let fake_agent = temp_dir.path().join("a3s-box-code");
        fs::write(&fake_agent, b"fake").unwrap();

        // Set environment variable
        std::env::set_var("A3S_AGENT_PATH", fake_agent.to_str().unwrap());

        let result = find_agent_binary();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), fake_agent);

        // Clean up
        std::env::remove_var("A3S_AGENT_PATH");
    }

    #[test]
    fn test_find_agent_binary_not_found() {
        // Ensure env var is not set
        std::env::remove_var("A3S_AGENT_PATH");

        let result = find_agent_binary();
        // This will fail unless agent is installed in common paths
        // Just verify it returns an error message mentioning A3S_AGENT_PATH
        if let Err(e) = result {
            assert!(e.to_string().contains("A3S_AGENT_PATH"));
        }
    }
}
