//! User-level A3S Box configuration.
//!
//! The CLI reads `~/.a3s/config.json` (or `$A3S_HOME/config.json`) for Docker-
//! compatible defaults that should apply across commands.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{BoxError, Result};

/// Docker Hub registry host used for authentication.
pub const DOCKER_HUB_AUTH_REGISTRY: &str = "index.docker.io";

/// Docker Hub registry host used in image references.
pub const DOCKER_HUB_IMAGE_REGISTRY: &str = "docker.io";

/// Top-level user configuration loaded from `~/.a3s/config.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct A3sConfig {
    /// Registry defaults and compatibility settings.
    pub registry: RegistryConfig,
}

impl A3sConfig {
    /// Load configuration from the default path.
    pub fn load_default() -> Result<Self> {
        Self::load_from_path(crate::dirs_home().join("config.json"))
    }

    /// Load configuration from a JSON file, returning defaults when absent.
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Self::default());
        }

        let data = std::fs::read_to_string(path).map_err(|e| {
            BoxError::ConfigError(format!("Failed to read config {}: {}", path.display(), e))
        })?;
        serde_json::from_str(&data).map_err(|e| {
            BoxError::ConfigError(format!("Failed to parse config {}: {}", path.display(), e))
        })
    }
}

/// Registry-related user configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RegistryConfig {
    /// Default registry for short image references and `login` without a server.
    #[serde(rename = "default", alias = "default_registry")]
    pub default_registry: Option<String>,

    /// Registries that should use insecure registry behavior, similar to Docker.
    pub insecure_registries: Vec<String>,

    /// Whether `a3s-box login` should validate credentials with the registry.
    pub login_verify: bool,
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            default_registry: None,
            insecure_registries: Vec::new(),
            login_verify: true,
        }
    }
}

impl RegistryConfig {
    /// Registry used by `login` when no server argument is provided.
    pub fn default_login_registry(&self) -> String {
        self.default_registry
            .as_deref()
            .map(normalize_registry_server)
            .unwrap_or_else(|| DOCKER_HUB_AUTH_REGISTRY.to_string())
    }

    /// Registry used for short image references such as `alpine:latest`.
    pub fn default_image_registry(&self) -> String {
        let registry = self
            .default_registry
            .as_deref()
            .map(normalize_registry_server)
            .unwrap_or_else(|| DOCKER_HUB_IMAGE_REGISTRY.to_string());

        if is_docker_hub_registry(&registry) {
            DOCKER_HUB_IMAGE_REGISTRY.to_string()
        } else {
            registry
        }
    }

    /// Whether a registry is configured as insecure.
    pub fn is_insecure_registry(&self, registry: &str) -> bool {
        let registry = normalize_registry_server(registry);
        self.insecure_registries
            .iter()
            .any(|configured| normalize_registry_server(configured) == registry)
    }
}

/// Normalize registry host aliases and Docker-style login URLs.
pub fn normalize_registry_server(registry: &str) -> String {
    let mut value = registry.trim().trim_end_matches('/').to_lowercase();

    if let Some((_, rest)) = value.split_once("://") {
        value = rest.to_string();
    }
    if let Some((host, _path)) = value.split_once('/') {
        value = host.to_string();
    }

    if is_docker_hub_registry(&value) {
        DOCKER_HUB_AUTH_REGISTRY.to_string()
    } else {
        value
    }
}

/// Whether a registry string explicitly asks for plain HTTP.
pub fn registry_uses_http(registry: &str) -> bool {
    registry
        .trim()
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
}

/// Whether a registry name is one of Docker Hub's common aliases.
pub fn is_docker_hub_registry(registry: &str) -> bool {
    matches!(
        registry
            .trim()
            .trim_end_matches('/')
            .to_lowercase()
            .as_str(),
        "docker.io" | "index.docker.io" | "registry-1.docker.io" | "index.docker.io/v1"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_missing_config_uses_defaults() {
        let dir = TempDir::new().unwrap();
        let config = A3sConfig::load_from_path(dir.path().join("config.json")).unwrap();

        assert_eq!(
            config.registry.default_login_registry(),
            DOCKER_HUB_AUTH_REGISTRY
        );
        assert_eq!(
            config.registry.default_image_registry(),
            DOCKER_HUB_IMAGE_REGISTRY
        );
        assert!(config.registry.login_verify);
    }

    #[test]
    fn test_parse_registry_config_with_default_alias() {
        let config: A3sConfig = serde_json::from_str(
            r#"{
                "registry": {
                    "default": "registry.example.com:5000",
                    "insecure_registries": ["localhost:5000"]
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            config.registry.default_login_registry(),
            "registry.example.com:5000"
        );
        assert_eq!(
            config.registry.default_image_registry(),
            "registry.example.com:5000"
        );
        assert!(config
            .registry
            .is_insecure_registry("http://localhost:5000"));
    }

    #[test]
    fn test_docker_hub_normalization() {
        assert_eq!(
            normalize_registry_server("docker.io"),
            DOCKER_HUB_AUTH_REGISTRY
        );
        assert_eq!(
            normalize_registry_server("https://index.docker.io/v1/"),
            DOCKER_HUB_AUTH_REGISTRY
        );
        assert_eq!(
            normalize_registry_server("registry-1.docker.io"),
            DOCKER_HUB_AUTH_REGISTRY
        );
    }

    #[test]
    fn test_registry_uses_http() {
        assert!(registry_uses_http("http://localhost:5000"));
        assert!(!registry_uses_http("https://localhost:5000"));
        assert!(!registry_uses_http("localhost:5000"));
    }
}
