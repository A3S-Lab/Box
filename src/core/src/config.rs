use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Box configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoxConfig {
    /// Workspace directory (mounted to /a3s/workspace/)
    pub workspace: PathBuf,

    /// Skill directories (mounted to /a3s/skills/)
    pub skills: Vec<PathBuf>,

    /// Model configuration
    pub model: ModelConfig,

    /// Resource limits
    pub resources: ResourceConfig,

    /// Lane configurations
    pub lanes: HashMap<String, LaneConfig>,

    /// Log level
    pub log_level: LogLevel,

    /// Enable gRPC debug logging
    pub debug_grpc: bool,
}

impl Default for BoxConfig {
    fn default() -> Self {
        Self {
            workspace: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            skills: vec![PathBuf::from("./skills")],
            model: ModelConfig::default(),
            resources: ResourceConfig::default(),
            lanes: Self::default_lanes(),
            log_level: LogLevel::Info,
            debug_grpc: false,
        }
    }
}

impl BoxConfig {
    /// Default lane configurations
    fn default_lanes() -> HashMap<String, LaneConfig> {
        let mut lanes = HashMap::new();

        // System lane (fixed concurrency)
        lanes.insert("system".to_string(), LaneConfig {
            min_concurrency: 1,
            max_concurrency: 1,
        });

        // Control lane
        lanes.insert("control".to_string(), LaneConfig {
            min_concurrency: 1,
            max_concurrency: 8,
        });

        // Query lane
        lanes.insert("query".to_string(), LaneConfig {
            min_concurrency: 1,
            max_concurrency: 8,
        });

        // Session lane
        lanes.insert("session".to_string(), LaneConfig {
            min_concurrency: 1,
            max_concurrency: 4,
        });

        // Skill lane
        lanes.insert("skill".to_string(), LaneConfig {
            min_concurrency: 1,
            max_concurrency: 4,
        });

        // Prompt lane
        lanes.insert("prompt".to_string(), LaneConfig {
            min_concurrency: 1,
            max_concurrency: 1,
        });

        lanes
    }
}

/// Model configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Provider (anthropic, openai, google, bedrock, mistral, etc.)
    pub provider: String,

    /// Model name
    pub name: String,

    /// Base URL (optional)
    pub base_url: Option<String>,

    /// API key (optional, falls back to environment variable)
    pub api_key: Option<String>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: "anthropic".to_string(),
            name: "claude-sonnet-4-20250514".to_string(),
            base_url: None,
            api_key: None,
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

/// Lane configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaneConfig {
    /// Minimum concurrency (reserved slots)
    pub min_concurrency: usize,

    /// Maximum concurrency (ceiling)
    pub max_concurrency: usize,
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

/// Session configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    /// System prompt
    pub system: Option<String>,

    /// Context threshold for auto-compaction (0.0 to 1.0)
    pub context_threshold: f32,

    /// Context compaction strategy
    pub context_strategy: ContextStrategy,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            system: None,
            context_threshold: 0.75,
            context_strategy: ContextStrategy::Summarize,
        }
    }
}

/// Context compaction strategy
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ContextStrategy {
    /// LLM generates a summary of conversation history
    Summarize,

    /// Drop oldest turns
    Truncate,

    /// Keep system prompt + LLM summary + recent turns
    SlidingWindow,
}
