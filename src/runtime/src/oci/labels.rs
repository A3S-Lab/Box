//! Agent configuration from OCI image labels.
//!
//! This module provides functionality to parse OCI image labels and convert them
//! into agent configuration. Labels follow the `a3s.box.*` namespace.
//!
//! # Label Schema
//!
//! ## Agent Configuration
//! - `a3s.box.agent.type` - Agent type (e.g., "code")
//! - `a3s.box.agent.version` - Agent version
//! - `a3s.box.agent.binary` - Path to agent binary in the image
//!
//! ## LLM Configuration
//! - `a3s.box.llm.provider` - LLM provider (anthropic, openai, deepseek, etc.)
//! - `a3s.box.llm.model` - Model name
//! - `a3s.box.llm.api_key` - API key (not recommended, use environment variables instead)
//! - `a3s.box.llm.base_url` - Custom API base URL
//!
//! ## Runtime Configuration
//! - `a3s.box.workspace` - Workspace directory path
//! - `a3s.box.env.*` - Environment variables (e.g., `a3s.box.env.RUST_LOG=debug`)
//!
//! # Example
//!
//! ```dockerfile
//! LABEL a3s.box.agent.type="code"
//! LABEL a3s.box.agent.version="0.1.0"
//! LABEL a3s.box.llm.provider="anthropic"
//! LABEL a3s.box.llm.model="claude-sonnet-4-20250514"
//! LABEL a3s.box.workspace="/a3s/workspace"
//! LABEL a3s.box.env.RUST_LOG="info"
//! ```

use std::collections::HashMap;

/// Agent configuration parsed from OCI labels.
#[derive(Debug, Clone, Default)]
pub struct AgentLabels {
    /// Agent type (e.g., "code")
    pub agent_type: Option<String>,

    /// Agent version
    pub agent_version: Option<String>,

    /// Path to agent binary in the image
    pub agent_binary: Option<String>,

    /// LLM provider
    pub llm_provider: Option<String>,

    /// LLM model name
    pub llm_model: Option<String>,

    /// LLM API key (not recommended, use environment variables)
    pub llm_api_key: Option<String>,

    /// Custom LLM API base URL
    pub llm_base_url: Option<String>,

    /// Workspace directory path
    pub workspace: Option<String>,

    /// Additional environment variables from labels
    pub env_vars: HashMap<String, String>,
}

impl AgentLabels {
    /// Parse agent configuration from OCI image labels.
    ///
    /// # Arguments
    ///
    /// * `labels` - HashMap of OCI image labels
    ///
    /// # Returns
    ///
    /// Parsed agent configuration
    pub fn from_labels(labels: &HashMap<String, String>) -> Self {
        let mut config = Self::default();

        for (key, value) in labels {
            match key.as_str() {
                // Agent configuration
                "a3s.box.agent.type" => config.agent_type = Some(value.clone()),
                "a3s.box.agent.version" => config.agent_version = Some(value.clone()),
                "a3s.box.agent.binary" => config.agent_binary = Some(value.clone()),

                // LLM configuration
                "a3s.box.llm.provider" => config.llm_provider = Some(value.clone()),
                "a3s.box.llm.model" => config.llm_model = Some(value.clone()),
                "a3s.box.llm.api_key" => config.llm_api_key = Some(value.clone()),
                "a3s.box.llm.base_url" => config.llm_base_url = Some(value.clone()),

                // Runtime configuration
                "a3s.box.workspace" => config.workspace = Some(value.clone()),

                // Environment variables
                _ if key.starts_with("a3s.box.env.") => {
                    let env_key = key.strip_prefix("a3s.box.env.").unwrap();
                    config.env_vars.insert(env_key.to_string(), value.clone());
                }

                // Ignore other labels
                _ => {}
            }
        }

        config
    }

    /// Check if this is a valid agent image.
    ///
    /// An image is considered a valid agent image if it has the `a3s.box.agent.type` label.
    pub fn is_agent_image(&self) -> bool {
        self.agent_type.is_some()
    }

    /// Get the agent type, or "unknown" if not set.
    pub fn agent_type_or_default(&self) -> &str {
        self.agent_type.as_deref().unwrap_or("unknown")
    }

    /// Get the workspace path, or a default value if not set.
    pub fn workspace_or_default(&self) -> &str {
        self.workspace.as_deref().unwrap_or("/a3s/workspace")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_agent_labels() {
        let mut labels = HashMap::new();
        labels.insert("a3s.box.agent.type".to_string(), "code".to_string());
        labels.insert("a3s.box.agent.version".to_string(), "0.1.0".to_string());
        labels.insert(
            "a3s.box.agent.binary".to_string(),
            "/usr/bin/a3s-code".to_string(),
        );

        let config = AgentLabels::from_labels(&labels);

        assert_eq!(config.agent_type, Some("code".to_string()));
        assert_eq!(config.agent_version, Some("0.1.0".to_string()));
        assert_eq!(config.agent_binary, Some("/usr/bin/a3s-code".to_string()));
        assert!(config.is_agent_image());
    }

    #[test]
    fn test_parse_llm_labels() {
        let mut labels = HashMap::new();
        labels.insert("a3s.box.llm.provider".to_string(), "anthropic".to_string());
        labels.insert(
            "a3s.box.llm.model".to_string(),
            "claude-sonnet-4-20250514".to_string(),
        );
        labels.insert(
            "a3s.box.llm.base_url".to_string(),
            "https://api.anthropic.com".to_string(),
        );

        let config = AgentLabels::from_labels(&labels);

        assert_eq!(config.llm_provider, Some("anthropic".to_string()));
        assert_eq!(
            config.llm_model,
            Some("claude-sonnet-4-20250514".to_string())
        );
        assert_eq!(
            config.llm_base_url,
            Some("https://api.anthropic.com".to_string())
        );
    }

    #[test]
    fn test_parse_env_labels() {
        let mut labels = HashMap::new();
        labels.insert("a3s.box.env.RUST_LOG".to_string(), "debug".to_string());
        labels.insert("a3s.box.env.API_TIMEOUT".to_string(), "30".to_string());

        let config = AgentLabels::from_labels(&labels);

        assert_eq!(config.env_vars.get("RUST_LOG"), Some(&"debug".to_string()));
        assert_eq!(config.env_vars.get("API_TIMEOUT"), Some(&"30".to_string()));
    }

    #[test]
    fn test_parse_workspace_label() {
        let mut labels = HashMap::new();
        labels.insert(
            "a3s.box.workspace".to_string(),
            "/custom/workspace".to_string(),
        );

        let config = AgentLabels::from_labels(&labels);

        assert_eq!(config.workspace, Some("/custom/workspace".to_string()));
        assert_eq!(config.workspace_or_default(), "/custom/workspace");
    }

    #[test]
    fn test_default_workspace() {
        let labels = HashMap::new();
        let config = AgentLabels::from_labels(&labels);

        assert_eq!(config.workspace_or_default(), "/a3s/workspace");
    }

    #[test]
    fn test_is_agent_image() {
        let mut labels = HashMap::new();
        let config = AgentLabels::from_labels(&labels);
        assert!(!config.is_agent_image());

        labels.insert("a3s.box.agent.type".to_string(), "code".to_string());
        let config = AgentLabels::from_labels(&labels);
        assert!(config.is_agent_image());
    }

    #[test]
    fn test_ignore_non_a3s_labels() {
        let mut labels = HashMap::new();
        labels.insert(
            "org.opencontainers.image.title".to_string(),
            "test".to_string(),
        );
        labels.insert("com.example.custom".to_string(), "value".to_string());
        labels.insert("a3s.box.agent.type".to_string(), "code".to_string());

        let config = AgentLabels::from_labels(&labels);

        // Only a3s.box labels should be parsed
        assert_eq!(config.agent_type, Some("code".to_string()));
        assert!(config.llm_provider.is_none());
    }

    #[test]
    fn test_agent_type_or_default() {
        let labels = HashMap::new();
        let config = AgentLabels::from_labels(&labels);
        assert_eq!(config.agent_type_or_default(), "unknown");

        let mut labels = HashMap::new();
        labels.insert("a3s.box.agent.type".to_string(), "code".to_string());
        let config = AgentLabels::from_labels(&labels);
        assert_eq!(config.agent_type_or_default(), "code");
    }
}
