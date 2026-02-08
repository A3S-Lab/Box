use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// TEE (Trusted Execution Environment) configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TeeConfig {
    /// No TEE (standard VM)
    #[default]
    None,

    /// AMD SEV-SNP (Secure Encrypted Virtualization - Secure Nested Paging)
    SevSnp {
        /// Workload identifier for attestation
        workload_id: String,
        /// CPU generation: "milan" or "genoa"
        #[serde(default)]
        generation: SevSnpGeneration,
    },
}

/// AMD SEV-SNP CPU generation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SevSnpGeneration {
    /// AMD EPYC Milan (3rd gen)
    #[default]
    Milan,
    /// AMD EPYC Genoa (4th gen)
    Genoa,
}

impl SevSnpGeneration {
    /// Get the generation as a string for TEE config.
    pub fn as_str(&self) -> &'static str {
        match self {
            SevSnpGeneration::Milan => "milan",
            SevSnpGeneration::Genoa => "genoa",
        }
    }
}

/// Agent type configuration - specifies how the coding agent is loaded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentType {
    /// Built-in A3S Code agent (default)
    A3sCode,

    /// OCI image containing the agent
    OciImage {
        /// Path to OCI image directory
        path: PathBuf,
    },

    /// Local binary agent
    LocalBinary {
        /// Path to the binary
        path: PathBuf,
        /// Arguments to pass to the binary
        args: Vec<String>,
    },

    /// Remote binary (downloaded on first use)
    RemoteBinary {
        /// URL to download the binary
        url: String,
        /// SHA256 checksum for verification
        checksum: String,
    },

    /// OCI image from a container registry (pulled on first use)
    OciRegistry {
        /// Image reference (e.g., "ghcr.io/a3s-box/code:v0.1.0")
        reference: String,
    },
}

impl Default for AgentType {
    fn default() -> Self {
        Self::A3sCode
    }
}

/// Business code configuration - specifies how business code is loaded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BusinessType {
    /// No business code (default)
    None,

    /// OCI image containing business code
    OciImage {
        /// Path to OCI image directory
        path: PathBuf,
    },

    /// Directory to mount as workspace
    Directory {
        /// Path to the directory
        path: PathBuf,
    },
}

impl Default for BusinessType {
    fn default() -> Self {
        Self::None
    }
}

/// Box configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoxConfig {
    /// Agent type (how the coding agent is loaded)
    #[serde(default)]
    pub agent: AgentType,

    /// Business code type (how business code is loaded)
    #[serde(default)]
    pub business: BusinessType,

    /// Workspace directory (mounted to /a3s/workspace/)
    pub workspace: PathBuf,

    /// Skill directories (mounted to /a3s/skills/)
    pub skills: Vec<PathBuf>,

    /// Resource limits
    pub resources: ResourceConfig,

    /// Log level
    pub log_level: LogLevel,

    /// Enable gRPC debug logging
    pub debug_grpc: bool,

    /// TEE (Trusted Execution Environment) configuration
    #[serde(default)]
    pub tee: TeeConfig,

    /// Command override (replaces OCI CMD when set)
    #[serde(default)]
    pub cmd: Vec<String>,

    /// Extra volume mounts (host_path:guest_path or host_path:guest_path:ro)
    #[serde(default)]
    pub volumes: Vec<String>,

    /// Extra environment variables for the entrypoint
    #[serde(default)]
    pub extra_env: Vec<(String, String)>,
}

impl Default for BoxConfig {
    fn default() -> Self {
        Self {
            agent: AgentType::default(),
            business: BusinessType::default(),
            workspace: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            skills: vec![PathBuf::from("./skills")],
            resources: ResourceConfig::default(),
            log_level: LogLevel::Info,
            debug_grpc: false,
            tee: TeeConfig::default(),
            cmd: vec![],
            volumes: vec![],
            extra_env: vec![],
        }
    }
}

/// Resource configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceConfig {
    /// Number of virtual CPUs
    pub vcpus: u32,

    /// Memory in MB
    pub memory_mb: u32,

    /// Disk space in MB
    pub disk_mb: u32,

    /// Box lifetime timeout in seconds (0 = unlimited)
    pub timeout: u64,
}

impl Default for ResourceConfig {
    fn default() -> Self {
        Self {
            vcpus: 2,
            memory_mb: 1024,
            disk_mb: 4096,
            timeout: 3600, // 1 hour
        }
    }
}

/// Log level
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl From<LogLevel> for tracing::Level {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Debug => tracing::Level::DEBUG,
            LogLevel::Info => tracing::Level::INFO,
            LogLevel::Warn => tracing::Level::WARN,
            LogLevel::Error => tracing::Level::ERROR,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_box_config_default() {
        let config = BoxConfig::default();

        assert_eq!(config.agent, AgentType::A3sCode);
        assert_eq!(config.business, BusinessType::None);
        assert!(!config.workspace.as_os_str().is_empty());
        assert_eq!(config.skills.len(), 1);
        assert_eq!(config.resources.vcpus, 2);
        assert!(!config.debug_grpc);
    }

    #[test]
    fn test_resource_config_default() {
        let config = ResourceConfig::default();

        assert_eq!(config.vcpus, 2);
        assert_eq!(config.memory_mb, 1024);
        assert_eq!(config.disk_mb, 4096);
        assert_eq!(config.timeout, 3600);
    }

    #[test]
    fn test_resource_config_custom() {
        let config = ResourceConfig {
            vcpus: 4,
            memory_mb: 2048,
            disk_mb: 8192,
            timeout: 7200,
        };

        assert_eq!(config.vcpus, 4);
        assert_eq!(config.memory_mb, 2048);
        assert_eq!(config.disk_mb, 8192);
        assert_eq!(config.timeout, 7200);
    }

    #[test]
    fn test_log_level_conversion() {
        assert_eq!(tracing::Level::from(LogLevel::Debug), tracing::Level::DEBUG);
        assert_eq!(tracing::Level::from(LogLevel::Info), tracing::Level::INFO);
        assert_eq!(tracing::Level::from(LogLevel::Warn), tracing::Level::WARN);
        assert_eq!(tracing::Level::from(LogLevel::Error), tracing::Level::ERROR);
    }

    #[test]
    fn test_box_config_serialization() {
        let config = BoxConfig::default();
        let json = serde_json::to_string(&config).unwrap();

        assert!(json.contains("workspace"));
        assert!(json.contains("resources"));
    }

    #[test]
    fn test_box_config_deserialization() {
        let json = r#"{
            "workspace": "/tmp/workspace",
            "skills": ["/tmp/skills"],
            "resources": {
                "vcpus": 4,
                "memory_mb": 2048,
                "disk_mb": 8192,
                "timeout": 1800
            },
            "log_level": "Debug",
            "debug_grpc": true
        }"#;

        let config: BoxConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.workspace.to_str().unwrap(), "/tmp/workspace");
        assert_eq!(config.resources.vcpus, 4);
        assert!(config.debug_grpc);
    }

    #[test]
    fn test_resource_config_serialization() {
        let config = ResourceConfig {
            vcpus: 8,
            memory_mb: 4096,
            disk_mb: 16384,
            timeout: 0,
        };

        let json = serde_json::to_string(&config).unwrap();
        let parsed: ResourceConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.vcpus, 8);
        assert_eq!(parsed.memory_mb, 4096);
        assert_eq!(parsed.timeout, 0); // Unlimited
    }

    #[test]
    fn test_log_level_serialization() {
        let levels = vec![
            LogLevel::Debug,
            LogLevel::Info,
            LogLevel::Warn,
            LogLevel::Error,
        ];

        for level in levels {
            let json = serde_json::to_string(&level).unwrap();
            let parsed: LogLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(tracing::Level::from(parsed), tracing::Level::from(level));
        }
    }

    #[test]
    fn test_config_clone() {
        let config = BoxConfig::default();
        let cloned = config.clone();

        assert_eq!(config.workspace, cloned.workspace);
        assert_eq!(config.resources.vcpus, cloned.resources.vcpus);
    }

    #[test]
    fn test_config_debug() {
        let config = BoxConfig::default();
        let debug_str = format!("{:?}", config);

        assert!(debug_str.contains("BoxConfig"));
        assert!(debug_str.contains("workspace"));
    }

    #[test]
    fn test_agent_type_default() {
        let agent = AgentType::default();
        assert_eq!(agent, AgentType::A3sCode);
    }

    #[test]
    fn test_agent_type_oci_image() {
        let agent = AgentType::OciImage {
            path: PathBuf::from("/path/to/agent-image"),
        };

        match agent {
            AgentType::OciImage { path } => {
                assert_eq!(path, PathBuf::from("/path/to/agent-image"));
            }
            _ => panic!("Expected OciImage variant"),
        }
    }

    #[test]
    fn test_agent_type_local_binary() {
        let agent = AgentType::LocalBinary {
            path: PathBuf::from("/usr/bin/agent"),
            args: vec!["--listen".to_string(), "vsock://4088".to_string()],
        };

        match agent {
            AgentType::LocalBinary { path, args } => {
                assert_eq!(path, PathBuf::from("/usr/bin/agent"));
                assert_eq!(args.len(), 2);
            }
            _ => panic!("Expected LocalBinary variant"),
        }
    }

    #[test]
    fn test_agent_type_remote_binary() {
        let agent = AgentType::RemoteBinary {
            url: "https://example.com/agent".to_string(),
            checksum: "abc123".to_string(),
        };

        match agent {
            AgentType::RemoteBinary { url, checksum } => {
                assert_eq!(url, "https://example.com/agent");
                assert_eq!(checksum, "abc123");
            }
            _ => panic!("Expected RemoteBinary variant"),
        }
    }

    #[test]
    fn test_agent_type_oci_registry() {
        let agent = AgentType::OciRegistry {
            reference: "ghcr.io/a3s-box/code:v0.1.0".to_string(),
        };

        match agent {
            AgentType::OciRegistry { reference } => {
                assert_eq!(reference, "ghcr.io/a3s-box/code:v0.1.0");
            }
            _ => panic!("Expected OciRegistry variant"),
        }
    }

    #[test]
    fn test_agent_type_oci_registry_serialization() {
        let agent = AgentType::OciRegistry {
            reference: "docker.io/library/nginx:latest".to_string(),
        };

        let json = serde_json::to_string(&agent).unwrap();
        let parsed: AgentType = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed, agent);
    }

    #[test]
    fn test_business_type_default() {
        let business = BusinessType::default();
        assert_eq!(business, BusinessType::None);
    }

    #[test]
    fn test_business_type_oci_image() {
        let business = BusinessType::OciImage {
            path: PathBuf::from("/path/to/business-image"),
        };

        match business {
            BusinessType::OciImage { path } => {
                assert_eq!(path, PathBuf::from("/path/to/business-image"));
            }
            _ => panic!("Expected OciImage variant"),
        }
    }

    #[test]
    fn test_business_type_directory() {
        let business = BusinessType::Directory {
            path: PathBuf::from("/path/to/app"),
        };

        match business {
            BusinessType::Directory { path } => {
                assert_eq!(path, PathBuf::from("/path/to/app"));
            }
            _ => panic!("Expected Directory variant"),
        }
    }

    #[test]
    fn test_agent_type_serialization() {
        let agent = AgentType::OciImage {
            path: PathBuf::from("/images/agent"),
        };

        let json = serde_json::to_string(&agent).unwrap();
        let parsed: AgentType = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed, agent);
    }

    #[test]
    fn test_business_type_serialization() {
        let business = BusinessType::OciImage {
            path: PathBuf::from("/images/business"),
        };

        let json = serde_json::to_string(&business).unwrap();
        let parsed: BusinessType = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed, business);
    }

    #[test]
    fn test_box_config_with_oci_images() {
        let config = BoxConfig {
            agent: AgentType::OciImage {
                path: PathBuf::from("/images/agent"),
            },
            business: BusinessType::OciImage {
                path: PathBuf::from("/images/business"),
            },
            ..Default::default()
        };

        let json = serde_json::to_string(&config).unwrap();
        let parsed: BoxConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.agent, config.agent);
        assert_eq!(parsed.business, config.business);
    }

    #[test]
    fn test_tee_config_default() {
        let tee = TeeConfig::default();
        assert_eq!(tee, TeeConfig::None);
    }

    #[test]
    fn test_tee_config_sev_snp() {
        let tee = TeeConfig::SevSnp {
            workload_id: "test-agent".to_string(),
            generation: SevSnpGeneration::Milan,
        };

        match tee {
            TeeConfig::SevSnp {
                workload_id,
                generation,
            } => {
                assert_eq!(workload_id, "test-agent");
                assert_eq!(generation, SevSnpGeneration::Milan);
            }
            _ => panic!("Expected SevSnp variant"),
        }
    }

    #[test]
    fn test_sev_snp_generation_as_str() {
        assert_eq!(SevSnpGeneration::Milan.as_str(), "milan");
        assert_eq!(SevSnpGeneration::Genoa.as_str(), "genoa");
    }

    #[test]
    fn test_sev_snp_generation_default() {
        let gen = SevSnpGeneration::default();
        assert_eq!(gen, SevSnpGeneration::Milan);
    }

    #[test]
    fn test_tee_config_serialization() {
        let tee = TeeConfig::SevSnp {
            workload_id: "my-workload".to_string(),
            generation: SevSnpGeneration::Genoa,
        };

        let json = serde_json::to_string(&tee).unwrap();
        let parsed: TeeConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed, tee);
    }

    #[test]
    fn test_tee_config_none_serialization() {
        let tee = TeeConfig::None;
        let json = serde_json::to_string(&tee).unwrap();
        let parsed: TeeConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed, TeeConfig::None);
    }

    #[test]
    fn test_box_config_with_tee() {
        let config = BoxConfig {
            tee: TeeConfig::SevSnp {
                workload_id: "secure-agent".to_string(),
                generation: SevSnpGeneration::Milan,
            },
            ..Default::default()
        };

        let json = serde_json::to_string(&config).unwrap();
        let parsed: BoxConfig = serde_json::from_str(&json).unwrap();

        match parsed.tee {
            TeeConfig::SevSnp {
                workload_id,
                generation,
            } => {
                assert_eq!(workload_id, "secure-agent");
                assert_eq!(generation, SevSnpGeneration::Milan);
            }
            _ => panic!("Expected SevSnp TEE config"),
        }
    }

    #[test]
    fn test_box_config_default_has_no_tee() {
        let config = BoxConfig::default();
        assert_eq!(config.tee, TeeConfig::None);
    }
}
