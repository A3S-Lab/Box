# OpenCode 适配器设计

## 概述

本文档描述如何将 OpenCode 集成到 A3S Box 中，通过适配器将 OpenCode 的 REST API 转换为 A3S Code Agent 标准接口。

## 架构

```
┌─────────────────────────────────────────────────────────────┐
│ A3S Box Runtime                                             │
│                                                             │
│  ┌──────────────────────────────────────────────────────┐  │
│  │ AgentClient (gRPC)                                   │  │
│  └────────────────────┬─────────────────────────────────┘  │
│                       │                                     │
│  ┌────────────────────▼─────────────────────────────────┐  │
│  │ OpenCodeAdapter                                      │  │
│  │  - gRPC Server (实现 CodeAgentService)               │  │
│  │  - REST Client (调用 OpenCode API)                   │  │
│  │  - 协议转换                                           │  │
│  └────────────────────┬─────────────────────────────────┘  │
└───────────────────────┼─────────────────────────────────────┘
                        │ HTTP REST
                        ▼
┌─────────────────────────────────────────────────────────────┐
│ OpenCode Container                                          │
│                                                             │
│  ┌──────────────────────────────────────────────────────┐  │
│  │ OpenCode Server                                      │  │
│  │  - REST API (OpenAPI 3.1.1)                          │  │
│  │  - Project management                                │  │
│  │  - PTY sessions                                      │  │
│  │  - Event streaming (SSE)                             │  │
│  └──────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

## API 映射

### 1. 生命周期管理

| A3S Code Agent | OpenCode API | 说明 |
|----------------|--------------|------|
| HealthCheck | GET /global/health | 健康检查 |
| GetCapabilities | GET /global/config | 获取配置和能力 |
| Initialize | POST /project + PATCH /global/config | 初始化项目和配置 |
| Shutdown | POST /global/dispose | 清理资源 |

### 2. 会话管理

OpenCode 使用 Project 概念，我们将其映射为 Session：

| A3S Code Agent | OpenCode API | 说明 |
|----------------|--------------|------|
| CreateSession | POST /project | 创建项目 |
| DestroySession | DELETE /project/{id} | 删除项目 |
| ListSessions | GET /project | 列出项目 |
| GetSession | GET /project/{id} | 获取项目信息 |
| ConfigureSession | PATCH /project/{id} | 更新项目配置 |

### 3. 代码生成

OpenCode 没有直接的代码生成 API，需要通过 PTY 会话模拟：

| A3S Code Agent | OpenCode 实现 | 说明 |
|----------------|--------------|------|
| Generate | POST /pty + 发送消息 | 创建 PTY 会话并发送提示词 |
| StreamGenerate | GET /pty/{id}/stream | 流式接收响应 |

### 4. 工具执行

| A3S Code Agent | OpenCode API | 说明 |
|----------------|--------------|------|
| ExecuteTool | POST /pty/{id}/input | 通过 PTY 执行命令 |
| ListTools | 静态列表 | OpenCode 工具是内置的 |

### 5. 事件流

| A3S Code Agent | OpenCode API | 说明 |
|----------------|--------------|------|
| SubscribeEvents | GET /global/event | 订阅全局事件（SSE） |

## 实现

### 1. OpenCodeAdapter 结构

```rust
use tonic::{Request, Response, Status};
use reqwest::Client;
use a3s_code_agent::*;

pub struct OpenCodeAdapter {
    /// OpenCode 服务器地址
    base_url: String,

    /// HTTP 客户端
    client: Client,

    /// 会话映射（Session ID -> Project ID）
    sessions: Arc<RwLock<HashMap<String, String>>>,

    /// PTY 映射（Session ID -> PTY ID）
    ptys: Arc<RwLock<HashMap<String, String>>>,
}

impl OpenCodeAdapter {
    pub fn new(base_url: String) -> Self {
        Self {
            base_url,
            client: Client::new(),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            ptys: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// 调用 OpenCode API
    async fn call_opencode<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<T, Status> {
        let url = format!("{}{}", self.base_url, path);

        let request = match method {
            "GET" => self.client.get(&url),
            "POST" => self.client.post(&url),
            "PATCH" => self.client.patch(&url),
            "DELETE" => self.client.delete(&url),
            _ => return Err(Status::unimplemented("Unsupported HTTP method")),
        };

        let request = if let Some(body) = body {
            request.json(&body)
        } else {
            request
        };

        let response = request
            .send()
            .await
            .map_err(|e| Status::unavailable(format!("OpenCode API error: {}", e)))?;

        if !response.status().is_success() {
            return Err(Status::internal(format!(
                "OpenCode API returned error: {}",
                response.status()
            )));
        }

        response
            .json()
            .await
            .map_err(|e| Status::internal(format!("Failed to parse response: {}", e)))
    }
}

#[tonic::async_trait]
impl CodeAgentService for OpenCodeAdapter {
    async fn health_check(
        &self,
        _request: Request<HealthCheckRequest>,
    ) -> Result<Response<HealthCheckResponse>, Status> {
        #[derive(serde::Deserialize)]
        struct HealthResponse {
            healthy: bool,
            version: String,
        }

        let health: HealthResponse = self.call_opencode("GET", "/global/health", None).await?;

        Ok(Response::new(HealthCheckResponse {
            status: if health.healthy {
                HealthCheckResponse::Status::Healthy as i32
            } else {
                HealthCheckResponse::Status::Unhealthy as i32
            },
            message: format!("OpenCode v{}", health.version),
            details: HashMap::new(),
        }))
    }

    async fn get_capabilities(
        &self,
        _request: Request<GetCapabilitiesRequest>,
    ) -> Result<Response<GetCapabilitiesResponse>, Status> {
        Ok(Response::new(GetCapabilitiesResponse {
            info: Some(AgentInfo {
                name: "opencode".to_string(),
                version: "0.1.0".to_string(),
                description: "The open source AI coding agent".to_string(),
                author: "Anomaly Co".to_string(),
                license: "MIT".to_string(),
                homepage: "https://opencode.ai".to_string(),
            }),
            features: vec![
                "code_generation".to_string(),
                "tool_execution".to_string(),
                "pty_sessions".to_string(),
                "lsp_support".to_string(),
            ],
            tools: vec![
                ToolCapability {
                    name: "read_file".to_string(),
                    description: "Read file contents".to_string(),
                    parameters: vec!["path".to_string()],
                    async_: false,
                },
                ToolCapability {
                    name: "write_file".to_string(),
                    description: "Write file contents".to_string(),
                    parameters: vec!["path".to_string(), "content".to_string()],
                    async_: false,
                },
                ToolCapability {
                    name: "bash".to_string(),
                    description: "Execute bash command".to_string(),
                    parameters: vec!["command".to_string()],
                    async_: true,
                },
                // ... 其他工具
            ],
            models: vec![
                ModelCapability {
                    provider: "anthropic".to_string(),
                    model: "claude-3-5-sonnet".to_string(),
                    features: vec!["code_generation".to_string()],
                },
                ModelCapability {
                    provider: "openai".to_string(),
                    model: "gpt-4".to_string(),
                    features: vec!["code_generation".to_string()],
                },
            ],
            limits: Some(ResourceLimits {
                max_context_tokens: 200000,
                max_concurrent_sessions: 10,
                max_tools_per_request: 20,
            }),
            metadata: HashMap::new(),
        }))
    }

    async fn create_session(
        &self,
        request: Request<CreateSessionRequest>,
    ) -> Result<Response<CreateSessionResponse>, Status> {
        let req = request.into_inner();

        #[derive(serde::Serialize)]
        struct CreateProjectRequest {
            directory: String,
            name: Option<String>,
        }

        #[derive(serde::Deserialize)]
        struct Project {
            id: String,
            directory: String,
            name: String,
        }

        let workspace = req.config.as_ref()
            .and_then(|c| Some(c.workspace.clone()))
            .unwrap_or_else(|| "/workspace".to_string());

        let project: Project = self.call_opencode(
            "POST",
            "/project",
            Some(serde_json::json!({
                "directory": workspace,
                "name": req.config.as_ref().and_then(|c| Some(c.name.clone())),
            })),
        ).await?;

        // 生成会话 ID
        let session_id = req.session_id.clone()
            .unwrap_or_else(|| format!("session-{}", uuid::Uuid::new_v4()));

        // 保存映射
        self.sessions.write().await.insert(session_id.clone(), project.id.clone());

        Ok(Response::new(CreateSessionResponse {
            session_id: session_id.clone(),
            session: Some(Session {
                session_id,
                config: req.config,
                state: SessionState::Active as i32,
                context_usage: Some(ContextUsage::default()),
                created_at: chrono::Utc::now().timestamp(),
                updated_at: chrono::Utc::now().timestamp(),
            }),
        }))
    }

    async fn generate(
        &self,
        request: Request<GenerateRequest>,
    ) -> Result<Response<GenerateResponse>, Status> {
        let req = request.into_inner();

        // 获取项目 ID
        let project_id = self.sessions.read().await
            .get(&req.session_id)
            .cloned()
            .ok_or_else(|| Status::not_found("Session not found"))?;

        // 创建 PTY 会话
        #[derive(serde::Deserialize)]
        struct Pty {
            id: String,
        }

        let pty: Pty = self.call_opencode(
            "POST",
            "/pty",
            Some(serde_json::json!({
                "directory": format!("/project/{}", project_id),
            })),
        ).await?;

        // 保存 PTY 映射
        self.ptys.write().await.insert(req.session_id.clone(), pty.id.clone());

        // 发送消息到 PTY
        let prompt = req.messages.iter()
            .filter(|m| m.role == Message::Role::User as i32)
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n");

        self.call_opencode::<serde_json::Value>(
            "POST",
            &format!("/pty/{}/input", pty.id),
            Some(serde_json::json!({
                "data": prompt,
            })),
        ).await?;

        // 等待响应（简化版，实际应该使用流式）
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        // 读取输出
        #[derive(serde::Deserialize)]
        struct PtyOutput {
            data: String,
        }

        let output: PtyOutput = self.call_opencode(
            "GET",
            &format!("/pty/{}/output", pty.id),
            None,
        ).await?;

        Ok(Response::new(GenerateResponse {
            session_id: req.session_id,
            message: Some(Message {
                role: Message::Role::Assistant as i32,
                content: output.data,
                attachments: vec![],
                metadata: HashMap::new(),
            }),
            tool_calls: vec![],
            usage: Some(Usage::default()),
            finish_reason: FinishReason::Stop as i32,
            metadata: HashMap::new(),
        }))
    }

    // 实现其他方法...
}
```

### 2. 流式生成实现

```rust
impl CodeAgentService for OpenCodeAdapter {
    async fn stream_generate(
        &self,
        request: Request<GenerateRequest>,
    ) -> Result<Response<Self::StreamGenerateStream>, Status> {
        let req = request.into_inner();

        // 获取 PTY ID
        let pty_id = self.ptys.read().await
            .get(&req.session_id)
            .cloned()
            .ok_or_else(|| Status::not_found("PTY not found"))?;

        // 创建 SSE 流
        let url = format!("{}/pty/{}/stream", self.base_url, pty_id);
        let response = self.client.get(&url)
            .send()
            .await
            .map_err(|e| Status::unavailable(format!("Failed to connect to stream: {}", e)))?;

        // 转换为 gRPC 流
        let stream = response.bytes_stream()
            .map(|chunk| {
                let chunk = chunk.map_err(|e| Status::internal(format!("Stream error: {}", e)))?;

                // 解析 SSE 数据
                let data = String::from_utf8_lossy(&chunk);

                Ok(GenerateChunk {
                    type_: GenerateChunk::ChunkType::Content as i32,
                    session_id: req.session_id.clone(),
                    content: data.to_string(),
                    tool_call: None,
                    tool_result: None,
                    metadata: HashMap::new(),
                })
            });

        Ok(Response::new(Box::pin(stream)))
    }
}
```

### 3. 工具执行实现

```rust
impl CodeAgentService for OpenCodeAdapter {
    async fn execute_tool(
        &self,
        request: Request<ExecuteToolRequest>,
    ) -> Result<Response<ExecuteToolResponse>, Status> {
        let req = request.into_inner();

        // 获取 PTY ID
        let pty_id = self.ptys.read().await
            .get(&req.session_id)
            .cloned()
            .ok_or_else(|| Status::not_found("PTY not found"))?;

        // 解析工具参数
        let args: serde_json::Value = serde_json::from_str(&req.arguments)
            .map_err(|e| Status::invalid_argument(format!("Invalid arguments: {}", e)))?;

        // 根据工具名称构建命令
        let command = match req.tool_name.as_str() {
            "read_file" => {
                let path = args["path"].as_str()
                    .ok_or_else(|| Status::invalid_argument("Missing 'path' parameter"))?;
                format!("cat {}", path)
            }
            "write_file" => {
                let path = args["path"].as_str()
                    .ok_or_else(|| Status::invalid_argument("Missing 'path' parameter"))?;
                let content = args["content"].as_str()
                    .ok_or_else(|| Status::invalid_argument("Missing 'content' parameter"))?;
                format!("echo '{}' > {}", content, path)
            }
            "bash" => {
                args["command"].as_str()
                    .ok_or_else(|| Status::invalid_argument("Missing 'command' parameter"))?
                    .to_string()
            }
            _ => return Err(Status::unimplemented(format!("Tool '{}' not supported", req.tool_name))),
        };

        // 执行命令
        self.call_opencode::<serde_json::Value>(
            "POST",
            &format!("/pty/{}/input", pty_id),
            Some(serde_json::json!({
                "data": command,
            })),
        ).await?;

        // 等待输出
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        // 读取输出
        #[derive(serde::Deserialize)]
        struct PtyOutput {
            data: String,
        }

        let output: PtyOutput = self.call_opencode(
            "GET",
            &format!("/pty/{}/output", pty_id),
            None,
        ).await?;

        Ok(Response::new(ExecuteToolResponse {
            result: Some(ToolResult {
                success: true,
                output: output.data,
                error: String::new(),
                metadata: HashMap::new(),
            }),
        }))
    }
}
```

## 配置

### 1. A3S Box 配置

```yaml
# box-config.yaml
coding_agent:
  kind: "opencode"
  version: "latest"
  adapter:
    type: "rest"
    base_url: "http://localhost:3000"
  config:
    provider: "anthropic"
    model: "claude-3-5-sonnet"
    api_key: "${ANTHROPIC_API_KEY}"
```

### 2. OpenCode 配置

```yaml
# opencode-config.yaml
provider: anthropic
model: claude-3-5-sonnet-20241022
apiKey: ${ANTHROPIC_API_KEY}
```

## 限制和注意事项

### 1. 功能差异

| 功能 | A3S Code Agent | OpenCode | 适配器支持 |
|------|----------------|----------|-----------|
| 会话管理 | ✅ | ✅ (Project) | ✅ |
| 代码生成 | ✅ | ✅ (PTY) | ✅ |
| 流式响应 | ✅ | ✅ (SSE) | ✅ |
| 工具执行 | ✅ | ✅ (PTY) | ✅ |
| 结构化输出 | ✅ | ❌ | ⚠️ 部分支持 |
| 技能管理 | ✅ | ❌ | ❌ |
| 上下文压缩 | ✅ | ❌ | ❌ |

### 2. 性能考虑

- **延迟**: REST API 比 gRPC 延迟稍高
- **流式**: SSE 转 gRPC 流有额外开销
- **并发**: OpenCode 的并发限制可能影响性能

### 3. 兼容性

- OpenCode 版本 >= 0.1.0
- 需要 OpenCode 服务器运行在可访问的地址
- 某些高级功能可能不可用

## 测试

### 1. 单元测试

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_health_check() {
        let adapter = OpenCodeAdapter::new("http://localhost:3000".to_string());

        let request = Request::new(HealthCheckRequest {});
        let response = adapter.health_check(request).await.unwrap();

        assert_eq!(response.get_ref().status, HealthCheckResponse::Status::Healthy as i32);
    }

    #[tokio::test]
    async fn test_create_session() {
        let adapter = OpenCodeAdapter::new("http://localhost:3000".to_string());

        let request = Request::new(CreateSessionRequest {
            session_id: None,
            config: Some(SessionConfig {
                name: "test".to_string(),
                workspace: "/workspace".to_string(),
                ..Default::default()
            }),
            initial_context: vec![],
        });

        let response = adapter.create_session(request).await.unwrap();
        assert!(!response.get_ref().session_id.is_empty());
    }
}
```

### 2. 集成测试

```bash
# 启动 OpenCode
docker run -d -p 3000:3000 opencode/opencode:latest

# 启动适配器
cargo run --bin opencode-adapter

# 测试
grpcurl -plaintext localhost:4088 a3s.code.agent.v1.CodeAgentService/HealthCheck
```

## 部署

### 1. Docker Compose

```yaml
version: '3.8'

services:
  opencode:
    image: opencode/opencode:latest
    ports:
      - "3000:3000"
    volumes:
      - ./workspace:/workspace
    environment:
      - ANTHROPIC_API_KEY=${ANTHROPIC_API_KEY}

  opencode-adapter:
    image: a3s/opencode-adapter:latest
    ports:
      - "4088:4088"
    environment:
      - OPENCODE_URL=http://opencode:3000
    depends_on:
      - opencode
```

### 2. Kubernetes

```yaml
apiVersion: v1
kind: Service
metadata:
  name: opencode-adapter
spec:
  selector:
    app: opencode-adapter
  ports:
    - port: 4088
      targetPort: 4088
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: opencode-adapter
spec:
  replicas: 1
  selector:
    matchLabels:
      app: opencode-adapter
  template:
    metadata:
      labels:
        app: opencode-adapter
    spec:
      containers:
      - name: adapter
        image: a3s/opencode-adapter:latest
        ports:
        - containerPort: 4088
        env:
        - name: OPENCODE_URL
          value: "http://opencode:3000"
```

## 未来改进

1. **性能优化**
   - 连接池
   - 请求缓存
   - 批量操作

2. **功能增强**
   - 支持更多 OpenCode 功能
   - 更好的错误处理
   - 重试机制

3. **监控和日志**
   - Prometheus 指标
   - 结构化日志
   - 分布式追踪

---

**版本**: 1.0.0
**最后更新**: 2026-02-03
**状态**: 草案
