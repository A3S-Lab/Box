//! Sandbox configuration options.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Options for creating a new sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxOptions {
    /// OCI image reference (e.g., "alpine:latest", "python:3.12-slim").
    pub image: String,

    /// Number of vCPUs (default: 1).
    #[serde(default = "default_cpus")]
    pub cpus: u32,

    /// Memory in megabytes (default: 256).
    #[serde(default = "default_memory_mb")]
    pub memory_mb: u32,

    /// Environment variables to set in the guest.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Working directory inside the guest.
    #[serde(default)]
    pub workdir: Option<String>,

    /// Host directories to mount into the guest as `host_path:guest_path`.
    #[serde(default)]
    pub mounts: Vec<MountSpec>,

    /// Enable outbound networking (default: true).
    #[serde(default = "default_true")]
    pub network: bool,

    /// Enable TEE (AMD SEV-SNP) if hardware supports it.
    #[serde(default)]
    pub tee: bool,

    /// Custom sandbox name (auto-generated if not set).
    #[serde(default)]
    pub name: Option<String>,
}

/// A host-to-guest mount specification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountSpec {
    /// Path on the host.
    pub host_path: String,
    /// Path inside the guest.
    pub guest_path: String,
    /// Read-only mount (default: false).
    #[serde(default)]
    pub readonly: bool,
}

impl Default for SandboxOptions {
    fn default() -> Self {
        Self {
            image: "alpine:latest".into(),
            cpus: default_cpus(),
            memory_mb: default_memory_mb(),
            env: HashMap::new(),
            workdir: None,
            mounts: Vec::new(),
            network: true,
            tee: false,
            name: None,
        }
    }
}

fn default_cpus() -> u32 {
    1
}

fn default_memory_mb() -> u32 {
    256
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_options() {
        let opts = SandboxOptions::default();
        assert_eq!(opts.image, "alpine:latest");
        assert_eq!(opts.cpus, 1);
        assert_eq!(opts.memory_mb, 256);
        assert!(opts.network);
        assert!(!opts.tee);
        assert!(opts.env.is_empty());
        assert!(opts.mounts.is_empty());
        assert!(opts.workdir.is_none());
        assert!(opts.name.is_none());
    }

    #[test]
    fn test_options_serde_roundtrip() {
        let opts = SandboxOptions {
            image: "python:3.12-slim".into(),
            cpus: 4,
            memory_mb: 1024,
            env: [("KEY".into(), "val".into())].into(),
            workdir: Some("/app".into()),
            mounts: vec![MountSpec {
                host_path: "/tmp/data".into(),
                guest_path: "/data".into(),
                readonly: true,
            }],
            network: false,
            tee: true,
            name: Some("my-sandbox".into()),
        };
        let json = serde_json::to_string(&opts).unwrap();
        let parsed: SandboxOptions = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.image, "python:3.12-slim");
        assert_eq!(parsed.cpus, 4);
        assert_eq!(parsed.memory_mb, 1024);
        assert_eq!(parsed.env["KEY"], "val");
        assert_eq!(parsed.workdir.as_deref(), Some("/app"));
        assert_eq!(parsed.mounts.len(), 1);
        assert!(parsed.mounts[0].readonly);
        assert!(!parsed.network);
        assert!(parsed.tee);
        assert_eq!(parsed.name.as_deref(), Some("my-sandbox"));
    }

    #[test]
    fn test_options_from_minimal_json() {
        let json = r#"{"image":"ubuntu:22.04"}"#;
        let opts: SandboxOptions = serde_json::from_str(json).unwrap();
        assert_eq!(opts.image, "ubuntu:22.04");
        assert_eq!(opts.cpus, 1);
        assert_eq!(opts.memory_mb, 256);
        assert!(opts.network);
    }

    #[test]
    fn test_mount_spec() {
        let mount = MountSpec {
            host_path: "/home/user/code".into(),
            guest_path: "/workspace".into(),
            readonly: false,
        };
        let json = serde_json::to_string(&mount).unwrap();
        assert!(json.contains("/workspace"));
        let parsed: MountSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.guest_path, "/workspace");
        assert!(!parsed.readonly);
    }
}
