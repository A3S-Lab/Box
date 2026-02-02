//! Agent Loop Implementation
//!
//! The agent loop handles the core conversation cycle:
//! 1. User sends a prompt
//! 2. LLM generates a response (possibly with tool calls)
//! 3. If tool calls present, execute them and send results back
//! 4. Repeat until LLM returns without tool calls
//!
//! This implements agentic behavior where the LLM can use tools
//! to accomplish tasks autonomously.

use crate::llm::{LlmClient, LlmResponse, Message, TokenUsage, ToolDefinition, default_tools};
use crate::tools::ToolExecutor;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Maximum number of tool execution rounds before stopping
const MAX_TOOL_ROUNDS: usize = 50;

/// Agent configuration
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub system_prompt: Option<String>,
    pub tools: Vec<ToolDefinition>,
    pub max_tool_rounds: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            system_prompt: None,
            tools: default_tools(),
            max_tool_rounds: MAX_TOOL_ROUNDS,
        }
    }
}

/// Events emitted during agent execution
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AgentEvent {
    /// Agent started processing
    #[serde(rename = "agent_start")]
    Start { prompt: String },

    /// LLM turn started
    #[serde(rename = "turn_start")]
    TurnStart { turn: usize },

    /// Text delta from streaming
    #[serde(rename = "text_delta")]
    TextDelta { text: String },

    /// Tool execution started
    #[serde(rename = "tool_start")]
    ToolStart { id: String, name: String },

    /// Tool execution completed
    #[serde(rename = "tool_end")]
    ToolEnd {
        id: String,
        name: String,
        output: String,
        exit_code: i32,
    },

    /// LLM turn completed
    #[serde(rename = "turn_end")]
    TurnEnd { turn: usize, usage: TokenUsage },

    /// Agent completed
    #[serde(rename = "agent_end")]
    End { text: String, usage: TokenUsage },

    /// Error occurred
    #[serde(rename = "error")]
    Error { message: String },
}

/// Result of agent execution
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AgentResult {
    pub text: String,
    pub messages: Vec<Message>,
    pub usage: TokenUsage,
    pub tool_calls_count: usize,
}

/// Agent loop executor
pub struct AgentLoop {
    llm_client: Arc<dyn LlmClient>,
    tool_executor: Arc<ToolExecutor>,
    config: AgentConfig,
}

impl AgentLoop {
    pub fn new(
        llm_client: Arc<dyn LlmClient>,
        tool_executor: Arc<ToolExecutor>,
        config: AgentConfig,
    ) -> Self {
        Self {
            llm_client,
            tool_executor,
            config,
        }
    }

    /// Execute the agent loop for a prompt
    ///
    /// Takes the conversation history and a new user prompt.
    /// Returns the agent result and updated message history.
    /// When event_tx is provided, uses streaming LLM API for real-time text output.
    pub async fn execute(
        &self,
        history: &[Message],
        prompt: &str,
        event_tx: Option<mpsc::Sender<AgentEvent>>,
    ) -> Result<AgentResult> {
        let mut messages = history.to_vec();
        let mut total_usage = TokenUsage::default();
        let mut tool_calls_count = 0;
        let mut turn = 0;

        // Send start event
        if let Some(tx) = &event_tx {
            tx.send(AgentEvent::Start {
                prompt: prompt.to_string(),
            })
            .await
            .ok();
        }

        // Add user message
        messages.push(Message::user(prompt));

        loop {
            turn += 1;

            if turn > self.config.max_tool_rounds {
                let error = format!("Max tool rounds ({}) exceeded", self.config.max_tool_rounds);
                if let Some(tx) = &event_tx {
                    tx.send(AgentEvent::Error {
                        message: error.clone(),
                    })
                    .await
                    .ok();
                }
                anyhow::bail!(error);
            }

            // Send turn start event
            if let Some(tx) = &event_tx {
                tx.send(AgentEvent::TurnStart { turn }).await.ok();
            }

            // Call LLM - use streaming if we have an event channel
            let response = if event_tx.is_some() {
                // Streaming mode
                let mut stream_rx = self
                    .llm_client
                    .complete_streaming(
                        &messages,
                        self.config.system_prompt.as_deref(),
                        &self.config.tools,
                    )
                    .await
                    .context("LLM streaming call failed")?;

                let mut final_response: Option<LlmResponse> = None;

                while let Some(event) = stream_rx.recv().await {
                    match event {
                        crate::llm::StreamEvent::TextDelta(text) => {
                            if let Some(tx) = &event_tx {
                                tx.send(AgentEvent::TextDelta { text }).await.ok();
                            }
                        }
                        crate::llm::StreamEvent::ToolUseStart { id, name } => {
                            if let Some(tx) = &event_tx {
                                tx.send(AgentEvent::ToolStart {
                                    id,
                                    name,
                                })
                                .await
                                .ok();
                            }
                        }
                        crate::llm::StreamEvent::ToolUseInputDelta(_) => {
                            // We could forward this if needed
                        }
                        crate::llm::StreamEvent::Done(resp) => {
                            final_response = Some(resp);
                        }
                    }
                }

                final_response.context("Stream ended without final response")?
            } else {
                // Non-streaming mode
                self.llm_client
                    .complete(
                        &messages,
                        self.config.system_prompt.as_deref(),
                        &self.config.tools,
                    )
                    .await
                    .context("LLM call failed")?
            };

            // Update usage
            total_usage.prompt_tokens += response.usage.prompt_tokens;
            total_usage.completion_tokens += response.usage.completion_tokens;
            total_usage.total_tokens += response.usage.total_tokens;

            // Add assistant message to history
            messages.push(response.message.clone());

            // Check for tool calls
            let tool_calls = response.tool_calls();

            // Send turn end event
            if let Some(tx) = &event_tx {
                tx.send(AgentEvent::TurnEnd {
                    turn,
                    usage: response.usage.clone(),
                })
                .await
                .ok();
            }

            if tool_calls.is_empty() {
                // No tool calls, we're done
                let final_text = response.text();

                if let Some(tx) = &event_tx {
                    tx.send(AgentEvent::End {
                        text: final_text.clone(),
                        usage: total_usage.clone(),
                    })
                    .await
                    .ok();
                }

                return Ok(AgentResult {
                    text: final_text,
                    messages,
                    usage: total_usage,
                    tool_calls_count,
                });
            }

            // Execute tools
            for tool_call in tool_calls {
                tool_calls_count += 1;

                // Send tool start event (only if not already sent during streaming)
                // In streaming mode, ToolStart is sent when we receive ToolUseStart from LLM
                // But we still need to send ToolEnd after execution

                // Execute the tool
                let result = self
                    .tool_executor
                    .execute(&tool_call.name, &tool_call.args)
                    .await;

                let (output, exit_code, is_error) = match result {
                    Ok(r) => (r.output, r.exit_code, r.exit_code != 0),
                    Err(e) => (format!("Tool execution error: {}", e), 1, true),
                };

                // Send tool end event
                if let Some(tx) = &event_tx {
                    tx.send(AgentEvent::ToolEnd {
                        id: tool_call.id.clone(),
                        name: tool_call.name.clone(),
                        output: output.clone(),
                        exit_code,
                    })
                    .await
                    .ok();
                }

                // Add tool result to messages
                messages.push(Message::tool_result(&tool_call.id, &output, is_error));
            }
        }
    }

    /// Execute with streaming events
    pub async fn execute_streaming(
        &self,
        history: &[Message],
        prompt: &str,
    ) -> Result<(mpsc::Receiver<AgentEvent>, tokio::task::JoinHandle<Result<AgentResult>>)> {
        let (tx, rx) = mpsc::channel(100);

        let llm_client = self.llm_client.clone();
        let tool_executor = self.tool_executor.clone();
        let config = self.config.clone();
        let history = history.to_vec();
        let prompt = prompt.to_string();

        let handle = tokio::spawn(async move {
            let agent = AgentLoop::new(llm_client, tool_executor, config);
            agent.execute(&history, &prompt, Some(tx)).await
        });

        Ok((rx, handle))
    }
}

/// Builder for creating an agent
#[allow(dead_code)]
pub struct AgentBuilder {
    llm_client: Option<Arc<dyn LlmClient>>,
    tool_executor: Option<Arc<ToolExecutor>>,
    config: AgentConfig,
}

#[allow(dead_code)]
impl AgentBuilder {
    pub fn new() -> Self {
        Self {
            llm_client: None,
            tool_executor: None,
            config: AgentConfig::default(),
        }
    }

    pub fn llm_client(mut self, client: Arc<dyn LlmClient>) -> Self {
        self.llm_client = Some(client);
        self
    }

    pub fn tool_executor(mut self, executor: Arc<ToolExecutor>) -> Self {
        self.tool_executor = Some(executor);
        self
    }

    pub fn system_prompt(mut self, prompt: &str) -> Self {
        self.config.system_prompt = Some(prompt.to_string());
        self
    }

    pub fn tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.config.tools = tools;
        self
    }

    pub fn max_tool_rounds(mut self, max: usize) -> Self {
        self.config.max_tool_rounds = max;
        self
    }

    pub fn build(self) -> Result<AgentLoop> {
        let llm_client = self.llm_client.context("LLM client is required")?;
        let tool_executor = self.tool_executor.context("Tool executor is required")?;

        Ok(AgentLoop::new(llm_client, tool_executor, self.config))
    }
}

impl Default for AgentBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_config_default() {
        let config = AgentConfig::default();
        assert!(config.system_prompt.is_none());
        assert!(!config.tools.is_empty());
        assert_eq!(config.max_tool_rounds, MAX_TOOL_ROUNDS);
    }
}
