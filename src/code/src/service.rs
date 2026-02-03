//! gRPC service implementation
//!
//! Implements the AgentService gRPC API for host-guest communication.
//! The service runs on port 4088 and handles:
//! - Session management (create, destroy)
//! - Generation (sync and streaming)
//! - Session commands (compact, clear, configure)
//! - Introspection (context usage, history)
//! - Control (cancel, health check)

use crate::agent::AgentEvent;
use crate::llm::{self, ContentBlock};
use crate::session::SessionManager;
use crate::tools::ToolExecutor;
use anyhow::Result;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

// Include generated proto code
pub mod proto {
    tonic::include_proto!("a3s.sandbox.agent");
}

use proto::agent_service_server::{AgentService, AgentServiceServer};
use proto::*;

/// Agent service implementation
pub struct AgentServiceImpl {
    session_manager: Arc<SessionManager>,
}

impl AgentServiceImpl {
    pub fn new(session_manager: Arc<SessionManager>) -> Self {
        Self { session_manager }
    }
}

#[tonic::async_trait]
impl AgentService for AgentServiceImpl {
    // ========================================================================
    // Session Management
    // ========================================================================

    async fn create_session(
        &self,
        request: Request<CreateSessionRequest>,
    ) -> Result<Response<CreateSessionResponse>, Status> {
        let req = request.into_inner();
        let session_id = uuid::Uuid::new_v4().to_string();

        self.session_manager
            .create_session(
                session_id.clone(),
                req.system,
                if req.context_threshold > 0.0 {
                    Some(req.context_threshold)
                } else {
                    None
                },
                if !req.context_strategy.is_empty() {
                    Some(req.context_strategy)
                } else {
                    None
                },
            )
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(CreateSessionResponse { session_id }))
    }

    async fn destroy_session(
        &self,
        request: Request<DestroySessionRequest>,
    ) -> Result<Response<DestroySessionResponse>, Status> {
        let req = request.into_inner();

        self.session_manager
            .destroy_session(&req.session_id)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(DestroySessionResponse {}))
    }

    // ========================================================================
    // Generation
    // ========================================================================

    async fn generate(
        &self,
        request: Request<GenerateRequest>,
    ) -> Result<Response<GenerateResponse>, Status> {
        let req = request.into_inner();

        let result = self
            .session_manager
            .generate(&req.session_id, &req.prompt)
            .await
            .map_err(|e| {
                // Log full error chain for debugging
                tracing::error!("Generate failed: {:?}", e);
                // Include full error chain in gRPC status
                Status::internal(format!("{:#}", e))
            })?;

        // Convert to proto format
        let usage = Some(TokenUsage {
            prompt_tokens: result.usage.prompt_tokens as i32,
            completion_tokens: result.usage.completion_tokens as i32,
            total_tokens: result.usage.total_tokens as i32,
        });

        // Extract tool calls and results from messages
        let mut tool_calls = Vec::new();
        let mut tool_results = Vec::new();
        let mut steps = Vec::new();
        let mut step_index = 0;

        for message in &result.messages {
            for block in &message.content {
                match block {
                    ContentBlock::Text { text } => {
                        steps.push(Step {
                            index: step_index,
                            step_type: "text".to_string(),
                            content: text.clone(),
                        });
                        step_index += 1;
                    }
                    ContentBlock::ToolUse { id: _, name, input } => {
                        tool_calls.push(ToolCall {
                            name: name.clone(),
                            args: input.to_string(),
                        });
                        steps.push(Step {
                            index: step_index,
                            step_type: "tool_call".to_string(),
                            content: format!("{}({})", name, input),
                        });
                        step_index += 1;
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        tool_results.push(proto::ToolResult {
                            name: tool_use_id.clone(),
                            output: content.clone(),
                            exit_code: if is_error.unwrap_or(false) { 1 } else { 0 },
                        });
                    }
                }
            }
        }

        Ok(Response::new(GenerateResponse {
            text: result.text,
            usage,
            tool_calls,
            tool_results,
            steps,
        }))
    }

    type StreamStream =
        Pin<Box<dyn Stream<Item = Result<StreamChunk, Status>> + Send + 'static>>;

    async fn stream(
        &self,
        request: Request<GenerateRequest>,
    ) -> Result<Response<Self::StreamStream>, Status> {
        let req = request.into_inner();

        let (rx, _handle) = self
            .session_manager
            .generate_streaming(&req.session_id, &req.prompt)
            .await
            .map_err(|e| {
                tracing::error!("Stream failed: {:?}", e);
                Status::internal(format!("{:#}", e))
            })?;

        // Convert agent events to stream chunks
        let stream = convert_events_to_stream(rx);

        Ok(Response::new(Box::pin(stream)))
    }

    async fn generate_object(
        &self,
        request: Request<GenerateObjectRequest>,
    ) -> Result<Response<GenerateObjectResponse>, Status> {
        let req = request.into_inner();

        // For now, use regular generation with schema in prompt
        let prompt_with_schema = format!(
            "{}\n\nRespond with ONLY a valid JSON object matching this schema (no markdown, no explanation, no code blocks):\n{}",
            req.prompt, req.schema
        );

        let result = self
            .session_manager
            .generate(&req.session_id, &prompt_with_schema)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        // Extract JSON from the response (handle <think> tags and markdown code blocks)
        let json_str = transform_for_structured_output(&result.text);

        Ok(Response::new(GenerateObjectResponse {
            object: json_str,
            usage: Some(TokenUsage {
                prompt_tokens: result.usage.prompt_tokens as i32,
                completion_tokens: result.usage.completion_tokens as i32,
                total_tokens: result.usage.total_tokens as i32,
            }),
        }))
    }

    type StreamObjectStream =
        Pin<Box<dyn Stream<Item = Result<ObjectStreamChunk, Status>> + Send + 'static>>;

    async fn stream_object(
        &self,
        request: Request<GenerateObjectRequest>,
    ) -> Result<Response<Self::StreamObjectStream>, Status> {
        // For simplicity, use non-streaming for now
        let response = self.generate_object(request).await?;
        let inner = response.into_inner();

        let (tx, rx) = mpsc::channel(1);
        tokio::spawn(async move {
            tx.send(Ok(ObjectStreamChunk {
                chunk: Some(object_stream_chunk::Chunk::Done(GenerateObjectResponse {
                    object: inner.object,
                    usage: inner.usage,
                })),
            }))
            .await
            .ok();
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    // ========================================================================
    // Skills
    // ========================================================================

    async fn use_skill(
        &self,
        request: Request<UseSkillRequest>,
    ) -> Result<Response<UseSkillResponse>, Status> {
        let req = request.into_inner();

        let tool_names = self
            .session_manager
            .use_skill(&req.session_id, &req.skill_name, &req.skill_content)
            .await
            .map_err(|e| {
                tracing::error!("UseSkill failed: {:?}", e);
                Status::internal(format!("{:#}", e))
            })?;

        Ok(Response::new(UseSkillResponse {
            tool_names,
        }))
    }

    async fn remove_skill(
        &self,
        request: Request<RemoveSkillRequest>,
    ) -> Result<Response<RemoveSkillResponse>, Status> {
        let req = request.into_inner();

        let removed_tools = self
            .session_manager
            .remove_skill(&req.session_id, &req.skill_name)
            .await
            .map_err(|e| {
                tracing::error!("RemoveSkill failed: {:?}", e);
                Status::internal(format!("{:#}", e))
            })?;

        Ok(Response::new(RemoveSkillResponse {
            removed_tools,
        }))
    }

    // ========================================================================
    // Session Commands
    // ========================================================================

    async fn compact(
        &self,
        request: Request<SessionCommandRequest>,
    ) -> Result<Response<SessionCommandResponse>, Status> {
        let req = request.into_inner();

        self.session_manager
            .compact(&req.session_id)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(SessionCommandResponse {}))
    }

    async fn clear(
        &self,
        request: Request<SessionCommandRequest>,
    ) -> Result<Response<SessionCommandResponse>, Status> {
        let req = request.into_inner();

        self.session_manager
            .clear(&req.session_id)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(SessionCommandResponse {}))
    }

    async fn configure(
        &self,
        request: Request<ConfigureRequest>,
    ) -> Result<Response<ConfigureResponse>, Status> {
        let req = request.into_inner();

        // Convert proto ModelConfig to LlmConfig if provided
        let model_config = req.model.map(|m| {
            let mut config = llm::LlmConfig::new(&m.provider, &m.name, m.api_key.unwrap_or_default());
            if let Some(base_url) = m.base_url {
                config = config.with_base_url(base_url);
            }
            config
        });

        self.session_manager
            .configure(
                &req.session_id,
                req.thinking,
                req.budget.map(|b| b as usize),
                model_config,
            )
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(ConfigureResponse {}))
    }

    // ========================================================================
    // Introspection
    // ========================================================================

    async fn get_context_usage(
        &self,
        request: Request<ContextUsageRequest>,
    ) -> Result<Response<ContextUsageResponse>, Status> {
        let req = request.into_inner();

        let usage = self
            .session_manager
            .context_usage(&req.session_id)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(ContextUsageResponse {
            used_tokens: usage.used_tokens as i32,
            max_tokens: usage.max_tokens as i32,
            percent: usage.percent,
            turns: usage.turns as i32,
        }))
    }

    async fn get_history(
        &self,
        request: Request<HistoryRequest>,
    ) -> Result<Response<HistoryResponse>, Status> {
        let req = request.into_inner();

        let messages = self
            .session_manager
            .history(&req.session_id)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let turns: Vec<Turn> = messages
            .iter()
            .map(|m| Turn {
                role: m.role.clone(),
                content: m.text(),
                timestamp: 0, // TODO: Track timestamps
            })
            .collect();

        Ok(Response::new(HistoryResponse { turns }))
    }

    // ========================================================================
    // Control
    // ========================================================================

    async fn cancel(
        &self,
        _request: Request<CancelRequest>,
    ) -> Result<Response<CancelResponse>, Status> {
        // TODO: Implement cancellation
        Ok(Response::new(CancelResponse {}))
    }

    async fn health_check(
        &self,
        _request: Request<HealthCheckRequest>,
    ) -> Result<Response<HealthCheckResponse>, Status> {
        Ok(Response::new(HealthCheckResponse { healthy: true }))
    }
}

// ============================================================================
// Response Transformers
// ============================================================================

/// Remove <think>...</think> blocks from text
fn remove_think_tags(text: &str) -> String {
    let mut content = text.to_string();
    while let Some(start) = content.find("<think>") {
        if let Some(end) = content.find("</think>") {
            content = format!("{}{}", &content[..start], &content[end + 8..]);
        } else {
            break;
        }
    }
    content
}

/// Extract JSON from markdown code blocks or raw text
fn extract_json(text: &str) -> String {
    let content = text.trim();

    // Try to extract JSON from markdown code blocks
    if let Some(start) = content.find("```json") {
        if let Some(end) = content[start + 7..].find("```") {
            return content[start + 7..start + 7 + end].trim().to_string();
        }
    }

    // Try generic code block
    if let Some(start) = content.find("```") {
        let after_start = &content[start + 3..];
        // Skip language identifier if present
        let json_start = after_start.find('\n').map(|i| i + 1).unwrap_or(0);
        if let Some(end) = after_start[json_start..].find("```") {
            return after_start[json_start..json_start + end].trim().to_string();
        }
    }

    // Try to find raw JSON object
    if let Some(start) = content.find('{') {
        if let Some(end) = content.rfind('}') {
            if end > start {
                return content[start..=end].to_string();
            }
        }
    }

    // Try to find raw JSON array
    if let Some(start) = content.find('[') {
        if let Some(end) = content.rfind(']') {
            if end > start {
                return content[start..=end].to_string();
            }
        }
    }

    // Return as-is if no JSON found
    content.to_string()
}

/// Transform LLM response for structured output
/// Applies: remove think tags -> extract JSON
fn transform_for_structured_output(text: &str) -> String {
    let without_think = remove_think_tags(text);
    extract_json(&without_think)
}

/// Convert agent events to gRPC stream chunks
fn convert_events_to_stream(
    mut rx: mpsc::Receiver<AgentEvent>,
) -> impl Stream<Item = Result<StreamChunk, Status>> {
    async_stream::stream! {
        while let Some(event) = rx.recv().await {
            let chunk = match event {
                AgentEvent::TextDelta { text } => StreamChunk {
                    chunk: Some(stream_chunk::Chunk::TextDelta(text)),
                },
                AgentEvent::ToolStart { id: _, name } => StreamChunk {
                    chunk: Some(stream_chunk::Chunk::ToolCall(ToolCall {
                        name,
                        args: "{}".to_string(),
                    })),
                },
                AgentEvent::ToolEnd { name, output, exit_code, .. } => StreamChunk {
                    chunk: Some(stream_chunk::Chunk::ToolResult(proto::ToolResult {
                        name,
                        output,
                        exit_code,
                    })),
                },
                AgentEvent::End { text, usage } => StreamChunk {
                    chunk: Some(stream_chunk::Chunk::Done(GenerateResponse {
                        text,
                        usage: Some(TokenUsage {
                            prompt_tokens: usage.prompt_tokens as i32,
                            completion_tokens: usage.completion_tokens as i32,
                            total_tokens: usage.total_tokens as i32,
                        }),
                        tool_calls: vec![],
                        tool_results: vec![],
                        steps: vec![],
                    })),
                },
                _ => continue,
            };
            yield Ok(chunk);
        }
    }
}

/// Start gRPC server
/// If the specified port is in use, automatically tries the next available port
pub async fn start_server() -> Result<()> {
    // Get configuration from environment (only workspace and listen address)
    // LLM configuration should be provided by clients via Configure RPC

    // Support both WORKSPACE and A3S_WORKSPACE environment variables
    let workspace = std::env::var("WORKSPACE")
        .or_else(|_| std::env::var("A3S_WORKSPACE"))
        .unwrap_or_else(|_| "/a3s/workspace".to_string());

    let listen_addr = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:4088".to_string());

    tracing::info!("Workspace: {}", workspace);
    tracing::info!("LLM configuration: Clients must provide via Configure RPC");

    // Create session manager without default LLM client
    // Sessions must configure their LLM via Configure RPC before use
    let tool_executor = Arc::new(ToolExecutor::new(workspace));
    let session_manager = Arc::new(SessionManager::new(None, tool_executor));

    let service = AgentServiceImpl::new(session_manager);

    // Parse the base address to extract host and port
    let (host, base_port) = parse_listen_addr(&listen_addr)?;

    // Try default port first, fallback to OS-assigned port if busy
    let (listener, actual_port) = {
        let addr = format!("{}:{}", host, base_port);
        match tokio::net::TcpListener::bind(&addr).await {
            Ok(listener) => {
                tracing::info!("Starting gRPC server on {}:{}", host, base_port);
                (listener, base_port)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                // Port busy, let OS assign an available port
                let fallback_addr = format!("{}:0", host);
                let listener = tokio::net::TcpListener::bind(&fallback_addr)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to bind to {}: {}", fallback_addr, e))?;
                let actual_port = listener.local_addr()?.port();
                tracing::warn!(
                    "Port {} was in use, using port {} instead",
                    base_port,
                    actual_port
                );
                tracing::info!("Starting gRPC server on {}:{}", host, actual_port);
                (listener, actual_port)
            }
            Err(e) => {
                return Err(anyhow::anyhow!("Failed to bind to {}: {}", addr, e));
            }
        }
    };
    let _ = actual_port; // Available for future use (e.g., write to file)

    // Convert to tonic incoming stream
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    // Start server with the bound listener
    tonic::transport::Server::builder()
        .add_service(AgentServiceServer::new(service))
        .serve_with_incoming(incoming)
        .await?;

    Ok(())
}

/// Parse listen address into host and port
fn parse_listen_addr(addr: &str) -> Result<(String, u16)> {
    let parts: Vec<&str> = addr.rsplitn(2, ':').collect();
    if parts.len() != 2 {
        return Err(anyhow::anyhow!(
            "Invalid listen address '{}': expected format 'host:port'",
            addr
        ));
    }
    let port: u16 = parts[0].parse().map_err(|e| {
        anyhow::anyhow!("Invalid port '{}': {}", parts[0], e)
    })?;
    let host = parts[1].to_string();
    Ok((host, port))
}
