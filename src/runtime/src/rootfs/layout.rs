//! Guest filesystem layout constants.
//!
//! Defines the directory structure inside the guest VM.

/// Path to the guest agent binary inside the VM.
pub const GUEST_AGENT_PATH: &str = "/a3s/agent/a3s-box-code";

/// Working directory inside the guest VM.
pub const GUEST_WORKDIR: &str = "/workspace";

/// Skills directory inside the guest VM.
pub const GUEST_SKILLS_DIR: &str = "/skills";

/// Guest filesystem layout.
#[derive(Debug, Clone)]
pub struct GuestLayout {
    /// Base directory for A3S files inside guest.
    pub base_dir: &'static str,

    /// Directory for agent binaries.
    pub agent_dir: &'static str,

    /// Workspace mount point (virtio-fs).
    pub workspace_dir: &'static str,

    /// Skills mount point (virtio-fs).
    pub skills_dir: &'static str,

    /// Temporary directory.
    pub tmp_dir: &'static str,

    /// Run directory for runtime files.
    pub run_dir: &'static str,
}

impl Default for GuestLayout {
    fn default() -> Self {
        Self {
            base_dir: "/a3s",
            agent_dir: "/a3s/agent",
            workspace_dir: "/workspace",
            skills_dir: "/skills",
            tmp_dir: "/tmp",
            run_dir: "/run",
        }
    }
}

impl GuestLayout {
    /// Get the standard guest layout.
    pub fn standard() -> Self {
        Self::default()
    }

    /// Get all directories that need to be created in the rootfs.
    pub fn required_dirs(&self) -> Vec<&str> {
        vec![
            self.base_dir,
            self.agent_dir,
            self.workspace_dir,
            self.skills_dir,
            self.tmp_dir,
            self.run_dir,
            "/dev",
            "/proc",
            "/sys",
            "/etc",
            "/var",
            "/var/tmp",
            "/var/log",
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_guest_layout_defaults() {
        let layout = GuestLayout::default();
        assert_eq!(layout.base_dir, "/a3s");
        assert_eq!(layout.agent_dir, "/a3s/agent");
        assert_eq!(layout.workspace_dir, "/workspace");
        assert_eq!(layout.skills_dir, "/skills");
        assert_eq!(layout.tmp_dir, "/tmp");
        assert_eq!(layout.run_dir, "/run");
    }

    #[test]
    fn test_guest_layout_standard_equals_default() {
        let standard = GuestLayout::standard();
        let default = GuestLayout::default();
        assert_eq!(standard.base_dir, default.base_dir);
        assert_eq!(standard.agent_dir, default.agent_dir);
        assert_eq!(standard.workspace_dir, default.workspace_dir);
    }

    #[test]
    fn test_required_dirs_contains_all_layout_dirs() {
        let layout = GuestLayout::standard();
        let dirs = layout.required_dirs();

        // Verify all layout directories are included
        assert!(dirs.contains(&layout.base_dir));
        assert!(dirs.contains(&layout.agent_dir));
        assert!(dirs.contains(&layout.workspace_dir));
        assert!(dirs.contains(&layout.skills_dir));
        assert!(dirs.contains(&layout.tmp_dir));
        assert!(dirs.contains(&layout.run_dir));
    }

    #[test]
    fn test_required_dirs_contains_system_dirs() {
        let layout = GuestLayout::standard();
        let dirs = layout.required_dirs();

        // Verify system directories are included
        assert!(dirs.contains(&"/dev"));
        assert!(dirs.contains(&"/proc"));
        assert!(dirs.contains(&"/sys"));
        assert!(dirs.contains(&"/etc"));
        assert!(dirs.contains(&"/var"));
        assert!(dirs.contains(&"/var/tmp"));
        assert!(dirs.contains(&"/var/log"));
    }

    #[test]
    fn test_guest_agent_path_constant() {
        assert_eq!(GUEST_AGENT_PATH, "/a3s/agent/a3s-box-code");
        // Agent path should be under agent_dir
        let layout = GuestLayout::default();
        assert!(GUEST_AGENT_PATH.starts_with(layout.agent_dir));
    }

    #[test]
    fn test_guest_workdir_constant() {
        assert_eq!(GUEST_WORKDIR, "/workspace");
        // Should match layout workspace_dir
        let layout = GuestLayout::default();
        assert_eq!(GUEST_WORKDIR, layout.workspace_dir);
    }

    #[test]
    fn test_guest_skills_dir_constant() {
        assert_eq!(GUEST_SKILLS_DIR, "/skills");
        // Should match layout skills_dir
        let layout = GuestLayout::default();
        assert_eq!(GUEST_SKILLS_DIR, layout.skills_dir);
    }

    #[test]
    fn test_guest_layout_clone() {
        let layout = GuestLayout::default();
        let cloned = layout.clone();
        assert_eq!(layout.base_dir, cloned.base_dir);
        assert_eq!(layout.workspace_dir, cloned.workspace_dir);
    }
}
