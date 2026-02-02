use a3s_box_core::config::SessionConfig;
use a3s_box_core::error::{BoxError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// Session identifier
pub type SessionId = String;

/// Session state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Session ID
    pub id: SessionId,

    /// Session configuration
    pub config: SessionConfig,

    /// Active skills
    pub active_skills: Vec<String>,

    /// Context usage
    pub context_usage: ContextUsage,

    /// Creation timestamp
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl Session {
    /// Create a new session
    pub fn new(config: SessionConfig) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            config,
            active_skills: Vec::new(),
            context_usage: ContextUsage::default(),
            created_at: chrono::Utc::now(),
        }
    }

    /// Check if context threshold is exceeded
    pub fn is_context_threshold_exceeded(&self) -> bool {
        self.context_usage.percent >= self.config.context_threshold
    }
}

/// Context usage information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextUsage {
    /// Tokens currently in context
    pub used_tokens: usize,

    /// Model's context window size
    pub max_tokens: usize,

    /// Usage percentage (0.0 to 1.0)
    pub percent: f32,

    /// Number of conversation turns
    pub turns: usize,
}

impl Default for ContextUsage {
    fn default() -> Self {
        Self {
            used_tokens: 0,
            max_tokens: 200_000, // Default context window
            percent: 0.0,
            turns: 0,
        }
    }
}

/// Token usage statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Prompt tokens
    pub prompt_tokens: usize,

    /// Completion tokens
    pub completion_tokens: usize,

    /// Total tokens
    pub total_tokens: usize,
}

/// Generate result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerateResult {
    /// Full response text
    pub text: String,

    /// Token usage
    pub usage: TokenUsage,

    /// Tool calls made by the agent
    pub tool_calls: Vec<ToolCall>,

    /// Tool execution results
    pub tool_results: Vec<ToolResult>,

    /// All intermediate steps
    pub steps: Vec<Step>,
}

/// Tool call
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Tool name
    pub name: String,

    /// Tool arguments
    pub args: serde_json::Value,
}

/// Tool result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// Tool name
    pub name: String,

    /// Tool output
    pub output: String,

    /// Exit code
    pub exit_code: i32,
}

/// Agent reasoning step
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    /// Step index
    pub index: usize,

    /// Step type
    pub step_type: StepType,

    /// Step content
    pub content: String,
}

/// Step type
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StepType {
    Thinking,
    ToolCall,
    Text,
}

/// Session manager
pub struct SessionManager {
    sessions: Arc<RwLock<HashMap<SessionId, Arc<RwLock<Session>>>>>,
}

impl SessionManager {
    /// Create a new session manager
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create a new session
    pub async fn create_session(&self, config: SessionConfig) -> Result<SessionId> {
        let session = Session::new(config);
        let session_id = session.id.clone();

        let mut sessions = self.sessions.write().await;
        sessions.insert(session_id.clone(), Arc::new(RwLock::new(session)));

        Ok(session_id)
    }

    /// Get a session
    pub async fn get_session(&self, session_id: &str) -> Result<Arc<RwLock<Session>>> {
        let sessions = self.sessions.read().await;
        sessions
            .get(session_id)
            .cloned()
            .ok_or_else(|| BoxError::SessionError(format!("Session not found: {}", session_id)))
    }

    /// Destroy a session
    pub async fn destroy_session(&self, session_id: &str) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        sessions
            .remove(session_id)
            .ok_or_else(|| BoxError::SessionError(format!("Session not found: {}", session_id)))?;

        Ok(())
    }

    /// List all sessions
    pub async fn list_sessions(&self) -> Vec<SessionId> {
        let sessions = self.sessions.read().await;
        sessions.keys().cloned().collect()
    }

    /// Get session count
    pub async fn session_count(&self) -> usize {
        let sessions = self.sessions.read().await;
        sessions.len()
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}
