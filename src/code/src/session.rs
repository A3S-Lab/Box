//! Session management
//!
//! Provides session-based conversation management:
//! - Multiple independent sessions per agent
//! - Conversation history tracking
//! - Context usage monitoring
//! - Per-session LLM client configuration
//! - Session persistence (TODO)

use crate::agent::{AgentConfig, AgentEvent, AgentLoop, AgentResult};
use crate::llm::{self, LlmClient, LlmConfig, Message, TokenUsage, ToolDefinition};
use crate::tools::ToolExecutor;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

/// Context usage statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextUsage {
    pub used_tokens: usize,
    pub max_tokens: usize,
    pub percent: f32,
    pub turns: usize,
}

impl Default for ContextUsage {
    fn default() -> Self {
        Self {
            used_tokens: 0,
            max_tokens: 200_000,
            percent: 0.0,
            turns: 0,
        }
    }
}

/// Session state
#[allow(dead_code)]
pub struct Session {
    pub id: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub context_usage: ContextUsage,
    pub total_usage: TokenUsage,
    pub tools: Vec<ToolDefinition>,
    pub thinking_enabled: bool,
    pub thinking_budget: Option<usize>,
    /// Per-session LLM client (overrides default if set)
    pub llm_client: Option<Arc<dyn LlmClient>>,
    /// Activated skills and their tool names
    pub active_skills: HashMap<String, Vec<String>>,
}

impl Session {
    pub fn new(id: String, system: Option<String>, tools: Vec<ToolDefinition>) -> Self {
        Self {
            id,
            system,
            messages: Vec::new(),
            context_usage: ContextUsage::default(),
            total_usage: TokenUsage::default(),
            tools,
            thinking_enabled: false,
            thinking_budget: None,
            llm_client: None,
            active_skills: HashMap::new(),
        }
    }

    /// Get conversation history
    #[allow(dead_code)]
    pub fn history(&self) -> &[Message] {
        &self.messages
    }

    /// Add a message to history
    #[allow(dead_code)]
    pub fn add_message(&mut self, message: Message) {
        self.messages.push(message);
        self.context_usage.turns = self.messages.len();
    }

    /// Update context usage after a response
    pub fn update_usage(&mut self, usage: &TokenUsage) {
        self.total_usage.prompt_tokens += usage.prompt_tokens;
        self.total_usage.completion_tokens += usage.completion_tokens;
        self.total_usage.total_tokens += usage.total_tokens;

        // Estimate context usage (rough approximation)
        self.context_usage.used_tokens = usage.prompt_tokens;
        self.context_usage.percent =
            self.context_usage.used_tokens as f32 / self.context_usage.max_tokens as f32;
    }

    /// Clear conversation history
    pub fn clear(&mut self) {
        self.messages.clear();
        self.context_usage = ContextUsage::default();
    }

    /// Compact context by summarizing old messages
    pub async fn compact(&mut self, _llm_client: &Arc<dyn LlmClient>) -> Result<()> {
        // TODO: Implement context compaction using LLM summarization
        // For now, just keep last N messages
        let keep_messages = 20;
        if self.messages.len() > keep_messages {
            self.messages = self.messages.split_off(self.messages.len() - keep_messages);
        }
        Ok(())
    }

    /// Activate a skill and track its tools
    pub fn activate_skill(&mut self, skill_name: String, tool_names: Vec<String>) {
        self.active_skills.insert(skill_name, tool_names);
    }

    /// Deactivate a skill and return its tool names
    pub fn deactivate_skill(&mut self, skill_name: &str) -> Option<Vec<String>> {
        self.active_skills.remove(skill_name)
    }

    /// Check if a skill is active
    pub fn is_skill_active(&self, skill_name: &str) -> bool {
        self.active_skills.contains_key(skill_name)
    }

    /// Get all active skill names
    pub fn active_skill_names(&self) -> Vec<String> {
        self.active_skills.keys().cloned().collect()
    }
}

/// Session manager handles multiple concurrent sessions
pub struct SessionManager {
    sessions: Arc<RwLock<HashMap<String, Arc<RwLock<Session>>>>>,
    llm_client: Option<Arc<dyn LlmClient>>,  // Optional default LLM client
    tool_executor: Arc<ToolExecutor>,
}

impl SessionManager {
    pub fn new(llm_client: Option<Arc<dyn LlmClient>>, tool_executor: Arc<ToolExecutor>) -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            llm_client,
            tool_executor,
        }
    }

    /// Create a new session
    pub async fn create_session(
        &self,
        id: String,
        system: Option<String>,
        context_threshold: Option<f32>,
        _context_strategy: Option<String>,
    ) -> Result<String> {
        // Get tool definitions from the executor
        let tools = self.tool_executor.definitions();
        let mut session = Session::new(id.clone(), system, tools);

        // Set context threshold if provided
        if let Some(threshold) = context_threshold {
            session.context_usage.max_tokens =
                (200_000.0 * threshold) as usize;
        }

        let mut sessions = self.sessions.write().await;
        sessions.insert(id.clone(), Arc::new(RwLock::new(session)));

        tracing::info!("Created session: {}", id);
        Ok(id)
    }

    /// Destroy a session
    pub async fn destroy_session(&self, id: &str) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        sessions.remove(id);
        tracing::info!("Destroyed session: {}", id);
        Ok(())
    }

    /// Get a session by ID
    pub async fn get_session(&self, id: &str) -> Result<Arc<RwLock<Session>>> {
        let sessions = self.sessions.read().await;
        sessions
            .get(id)
            .cloned()
            .context(format!("Session not found: {}", id))
    }

    /// List all session IDs
    #[allow(dead_code)]
    pub async fn list_sessions(&self) -> Vec<String> {
        let sessions = self.sessions.read().await;
        sessions.keys().cloned().collect()
    }

    /// Generate response for a prompt
    pub async fn generate(
        &self,
        session_id: &str,
        prompt: &str,
    ) -> Result<AgentResult> {
        let session_lock = self.get_session(session_id).await?;

        // Get session state and LLM client
        let (history, system, tools, session_llm_client) = {
            let session = session_lock.read().await;
            (
                session.messages.clone(),
                session.system.clone(),
                session.tools.clone(),
                session.llm_client.clone(),
            )
        };

        // Use session's LLM client if configured, otherwise use default
        let llm_client = if let Some(client) = session_llm_client {
            client
        } else if let Some(client) = &self.llm_client {
            client.clone()
        } else {
            anyhow::bail!(
                "LLM client not configured for session {}. Please call Configure RPC with model configuration first.",
                session_id
            );
        };

        // Create agent loop
        let config = AgentConfig {
            system_prompt: system,
            tools,
            max_tool_rounds: 50,
        };

        let agent = AgentLoop::new(
            llm_client,
            self.tool_executor.clone(),
            config,
        );

        // Execute
        let result = agent.execute(&history, prompt, None).await?;

        // Update session
        {
            let mut session = session_lock.write().await;
            session.messages = result.messages.clone();
            session.update_usage(&result.usage);
        }

        Ok(result)
    }

    /// Generate response with streaming events
    pub async fn generate_streaming(
        &self,
        session_id: &str,
        prompt: &str,
    ) -> Result<(mpsc::Receiver<AgentEvent>, tokio::task::JoinHandle<Result<AgentResult>>)> {
        let session_lock = self.get_session(session_id).await?;

        // Get session state and LLM client
        let (history, system, tools, session_llm_client) = {
            let session = session_lock.read().await;
            (
                session.messages.clone(),
                session.system.clone(),
                session.tools.clone(),
                session.llm_client.clone(),
            )
        };

        // Use session's LLM client if configured, otherwise use default
        let llm_client = if let Some(client) = session_llm_client {
            client
        } else if let Some(client) = &self.llm_client {
            client.clone()
        } else {
            anyhow::bail!(
                "LLM client not configured for session {}. Please call Configure RPC with model configuration first.",
                session_id
            );
        };

        // Create agent loop
        let config = AgentConfig {
            system_prompt: system,
            tools,
            max_tool_rounds: 50,
        };

        let agent = AgentLoop::new(
            llm_client,
            self.tool_executor.clone(),
            config,
        );

        // Execute with streaming
        let (rx, handle) = agent.execute_streaming(&history, prompt).await?;

        // Spawn task to update session after completion
        let session_lock_clone = session_lock.clone();
        let original_handle = handle;

        let wrapped_handle = tokio::spawn(async move {
            let result = original_handle.await??;

            // Update session
            {
                let mut session = session_lock_clone.write().await;
                session.messages = result.messages.clone();
                session.update_usage(&result.usage);
            }

            Ok(result)
        });

        Ok((rx, wrapped_handle))
    }

    /// Get context usage for a session
    pub async fn context_usage(&self, session_id: &str) -> Result<ContextUsage> {
        let session_lock = self.get_session(session_id).await?;
        let session = session_lock.read().await;
        Ok(session.context_usage.clone())
    }

    /// Get conversation history for a session
    pub async fn history(&self, session_id: &str) -> Result<Vec<Message>> {
        let session_lock = self.get_session(session_id).await?;
        let session = session_lock.read().await;
        Ok(session.messages.clone())
    }

    /// Clear session history
    pub async fn clear(&self, session_id: &str) -> Result<()> {
        let session_lock = self.get_session(session_id).await?;
        let mut session = session_lock.write().await;
        session.clear();
        Ok(())
    }

    /// Compact session context
    pub async fn compact(&self, session_id: &str) -> Result<()> {
        let session_lock = self.get_session(session_id).await?;
        let mut session = session_lock.write().await;

        // Get LLM client for compaction (if available)
        let llm_client = if let Some(client) = &session.llm_client {
            client.clone()
        } else if let Some(client) = &self.llm_client {
            client.clone()
        } else {
            // If no LLM client available, just do simple truncation
            tracing::warn!("No LLM client configured for compaction, using simple truncation");
            let keep_messages = 20;
            if session.messages.len() > keep_messages {
                let len = session.messages.len();
                session.messages = session.messages.split_off(len - keep_messages);
            }
            return Ok(());
        };

        session.compact(&llm_client).await
    }

    /// Configure session
    pub async fn configure(
        &self,
        session_id: &str,
        thinking: Option<bool>,
        budget: Option<usize>,
        model_config: Option<LlmConfig>,
    ) -> Result<()> {
        let session_lock = self.get_session(session_id).await?;
        let mut session = session_lock.write().await;

        if let Some(t) = thinking {
            session.thinking_enabled = t;
        }
        if let Some(b) = budget {
            session.thinking_budget = Some(b);
        }
        if let Some(config) = model_config {
            tracing::info!(
                "Configuring session {} with LLM: provider={}, model={}",
                session_id,
                config.provider,
                config.model
            );
            session.llm_client = Some(llm::create_client_with_config(config));
        }

        Ok(())
    }

    /// Get session count
    #[allow(dead_code)]
    pub async fn session_count(&self) -> usize {
        let sessions = self.sessions.read().await;
        sessions.len()
    }

    /// Activate a skill for a session
    ///
    /// Registers the skill's tools with the executor and tracks them in the session.
    /// Returns the names of tools that were registered.
    pub async fn use_skill(
        &self,
        session_id: &str,
        skill_name: &str,
        skill_content: &str,
    ) -> Result<Vec<String>> {
        let session_lock = self.get_session(session_id).await?;

        // Check if skill is already active
        {
            let session = session_lock.read().await;
            if session.is_skill_active(skill_name) {
                tracing::info!("Skill {} is already active for session {}", skill_name, session_id);
                return Ok(session.active_skills.get(skill_name).cloned().unwrap_or_default());
            }
        }

        // Register tools with the executor
        let tool_names = self.tool_executor.register_skill_tools(skill_content);

        if tool_names.is_empty() {
            tracing::warn!("No tools found in skill: {}", skill_name);
        }

        // Track in session
        {
            let mut session = session_lock.write().await;
            session.activate_skill(skill_name.to_string(), tool_names.clone());
            // Update session's tool definitions
            session.tools = self.tool_executor.definitions();
        }

        tracing::info!(
            "Activated skill {} for session {} with tools: {:?}",
            skill_name,
            session_id,
            tool_names
        );

        Ok(tool_names)
    }

    /// Deactivate a skill for a session
    ///
    /// Unregisters the skill's tools from the executor.
    /// Returns the names of tools that were unregistered.
    pub async fn remove_skill(
        &self,
        session_id: &str,
        skill_name: &str,
    ) -> Result<Vec<String>> {
        let session_lock = self.get_session(session_id).await?;

        // Get tool names from session
        let tool_names = {
            let mut session = session_lock.write().await;
            session.deactivate_skill(skill_name)
        };

        let Some(tool_names) = tool_names else {
            tracing::info!("Skill {} is not active for session {}", skill_name, session_id);
            return Ok(vec![]);
        };

        // Unregister tools from executor
        let removed = self.tool_executor.unregister_tools(&tool_names);

        // Update session's tool definitions
        {
            let mut session = session_lock.write().await;
            session.tools = self.tool_executor.definitions();
        }

        tracing::info!(
            "Deactivated skill {} for session {} (removed tools: {:?})",
            skill_name,
            session_id,
            removed
        );

        Ok(removed)
    }

    /// List active skills for a session
    pub async fn list_active_skills(&self, session_id: &str) -> Result<Vec<String>> {
        let session_lock = self.get_session(session_id).await?;
        let session = session_lock.read().await;
        Ok(session.active_skill_names())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_creation() {
        let session = Session::new(
            "test-1".to_string(),
            Some("You are helpful.".to_string()),
            vec![],
        );
        assert_eq!(session.id, "test-1");
        assert_eq!(session.system, Some("You are helpful.".to_string()));
        assert!(session.messages.is_empty());
    }

    #[test]
    fn test_context_usage_default() {
        let usage = ContextUsage::default();
        assert_eq!(usage.used_tokens, 0);
        assert_eq!(usage.max_tokens, 200_000);
        assert_eq!(usage.percent, 0.0);
    }
}
