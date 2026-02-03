#![deny(clippy::all)]

use napi::bindgen_prelude::*;
use napi_derive::napi;
use std::collections::HashMap;

/// Model configuration
#[napi(object)]
#[derive(Clone)]
pub struct ModelConfig {
    pub provider: String,
    pub name: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

/// Resource configuration
#[napi(object)]
#[derive(Clone)]
pub struct ResourceConfig {
    pub vcpus: u32,
    pub memory_mb: u32,
    pub disk_mb: u32,
    pub timeout: u32,
}

/// Lane configuration
#[napi(object)]
#[derive(Clone)]
pub struct LaneConfig {
    pub min_concurrency: u32,
    pub max_concurrency: u32,
}

/// Box configuration
#[napi(object)]
pub struct BoxConfig {
    pub workspace: Option<String>,
    pub skills: Option<Vec<String>>,
    pub model: Option<ModelConfig>,
    pub resources: Option<ResourceConfig>,
    pub lanes: Option<HashMap<String, LaneConfig>>,
    pub log_level: Option<String>,
    pub debug_grpc: Option<bool>,
}

/// Token usage
#[napi(object)]
#[derive(Clone)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// Generate result
#[napi(object)]
pub struct GenerateResult {
    pub text: String,
    pub usage: TokenUsage,
}

/// Session configuration
#[napi(object)]
pub struct SessionConfig {
    pub system: Option<String>,
    pub context_threshold: Option<f64>,
    pub context_strategy: Option<String>,
}

/// A3S Box - main entry point
#[napi(js_name = "Box")]
pub struct A3sBox {
    // TODO: Add actual A3S Box runtime handle
    _placeholder: (),
}

#[napi]
impl A3sBox {
    /// Create a new box
    #[napi(factory)]
    pub fn new(_config: Option<BoxConfig>) -> Result<Self> {
        // TODO: Implement box creation
        // 1. Create BoxConfig from parameters
        // 2. Initialize VmManager
        // 3. Boot VM (lazy)

        Ok(Self { _placeholder: () })
    }

    /// Create a session
    #[napi]
    pub fn create_session(&self, _config: Option<SessionConfig>) -> Result<Session> {
        // TODO: Implement session creation
        Ok(Session {
            session_id: "placeholder".to_string(),
        })
    }

    /// List sessions
    #[napi]
    pub fn list_sessions(&self) -> Result<Vec<String>> {
        // TODO: Implement
        Ok(vec![])
    }

    /// Destroy the box
    #[napi]
    pub async fn destroy(&self) -> Result<()> {
        // TODO: Implement VM destruction
        Ok(())
    }

    /// Get queue status
    #[napi]
    pub async fn queue_status(&self) -> Result<String> {
        // TODO: Implement
        Ok("{}".to_string())
    }

    /// Get metrics
    #[napi]
    pub async fn metrics(&self) -> Result<String> {
        // TODO: Implement
        Ok("{}".to_string())
    }
}

/// Session
#[napi]
pub struct Session {
    session_id: String,
}

#[napi]
impl Session {
    /// Get session ID
    #[napi(getter)]
    pub fn session_id(&self) -> String {
        self.session_id.clone()
    }

    /// Generate (non-streaming)
    #[napi]
    pub async fn generate(&self, _prompt: String) -> Result<GenerateResult> {
        // TODO: Implement
        Ok(GenerateResult {
            text: "placeholder".to_string(),
            usage: TokenUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
            },
        })
    }

    /// Stream (streaming)
    #[napi]
    pub async fn stream(&self, _prompt: String) -> Result<()> {
        // TODO: Implement streaming
        Ok(())
    }

    /// Generate object (structured output)
    #[napi]
    pub async fn generate_object(&self, _prompt: String, _schema: String) -> Result<String> {
        // TODO: Implement
        Ok("{}".to_string())
    }

    /// Use skill
    #[napi]
    pub async fn use_skill(&self, _skill_name: String) -> Result<()> {
        // TODO: Implement
        Ok(())
    }

    /// Remove skill
    #[napi]
    pub async fn remove_skill(&self, _skill_name: String) -> Result<()> {
        // TODO: Implement
        Ok(())
    }

    /// List active skills
    #[napi]
    pub async fn list_skills(&self) -> Result<Vec<String>> {
        // TODO: Implement
        Ok(vec![])
    }

    /// Compact context
    #[napi]
    pub async fn compact(&self) -> Result<()> {
        // TODO: Implement
        Ok(())
    }

    /// Clear context
    #[napi]
    pub async fn clear(&self) -> Result<()> {
        // TODO: Implement
        Ok(())
    }

    /// Configure session
    #[napi]
    pub async fn configure(
        &self,
        _thinking: Option<bool>,
        _budget: Option<u32>,
        _model: Option<ModelConfig>,
    ) -> Result<()> {
        // TODO: Implement
        Ok(())
    }

    /// Get context usage
    #[napi]
    pub async fn context_usage(&self) -> Result<String> {
        // TODO: Implement
        Ok("{}".to_string())
    }

    /// Get history
    #[napi]
    pub async fn history(&self) -> Result<String> {
        // TODO: Implement
        Ok("[]".to_string())
    }

    /// Destroy session
    #[napi]
    pub async fn destroy(&self) -> Result<()> {
        // TODO: Implement
        Ok(())
    }
}

/// Create a box (convenience function)
#[napi]
pub async fn create_box(config: Option<BoxConfig>) -> Result<A3sBox> {
    A3sBox::new(config)
}
