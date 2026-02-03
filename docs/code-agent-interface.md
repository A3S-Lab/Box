# A3S Code Agent 接口规范

## 概述

本文档定义了 A3S Code Agent 的标准接口，任何实现了该接口的编码智能体都可以集成到 A3S Box 中。

## 设计原则

1. **协议无关** - 支持 gRPC、REST、WebSocket 等多种协议
2. **能力声明** - 智能体可以声明自己支持的功能
3. **工具可扩展** - 支持自定义工具和扩展
4. **会话管理** - 支持多会话并发
5. **流式响应** - 支持流式生成和事件推送

## 核心接口

### 1. Agent Service（智能体服务）

所有编码智能体必须实现以下核心接口：

```protobuf
syntax = "proto3";
package a3s.code.agent.v1;

// 编码智能体服务
service CodeAgentService {
  // === 生命周期管理 ===

  // 健康检查
  rpc HealthCheck(HealthCheckRequest) returns (HealthCheckResponse);

  // 获取智能体能力
  rpc GetCapabilities(GetCapabilitiesRequest) returns (GetCapabilitiesResponse);

  // 初始化智能体
  rpc Initialize(InitializeRequest) returns (InitializeResponse);

  // 关闭智能体
  rpc Shutdown(ShutdownRequest) returns (ShutdownResponse);

  // === 会话管理 ===

  // 创建会话
  rpc CreateSession(CreateSessionRequest) returns (CreateSessionResponse);

  // 销毁会话
  rpc DestroySession(DestroySessionRequest) returns (DestroySessionResponse);

  // 列出会话
  rpc ListSessions(ListSessionsRequest) returns (ListSessionsResponse);

  // 获取会话信息
  rpc GetSession(GetSessionRequest) returns (GetSessionResponse);

  // 配置会话
  rpc ConfigureSession(ConfigureSessionRequest) returns (ConfigureSessionResponse);

  // === 代码生成 ===

  // 生成代码（同步）
  rpc Generate(GenerateRequest) returns (GenerateResponse);

  // 生成代码（流式）
  rpc StreamGenerate(GenerateRequest) returns (stream GenerateChunk);

  // 生成结构化输出（同步）
  rpc GenerateStructured(GenerateStructuredRequest) returns (GenerateStructuredResponse);

  // 生成结构化输出（流式）
  rpc StreamGenerateStructured(GenerateStructuredRequest) returns (stream GenerateStructuredChunk);

  // === 工具执行 ===

  // 执行工具
  rpc ExecuteTool(ExecuteToolRequest) returns (ExecuteToolResponse);

  // 批量执行工具
  rpc ExecuteToolBatch(ExecuteToolBatchRequest) returns (ExecuteToolBatchResponse);

  // 列出可用工具
  rpc ListTools(ListToolsRequest) returns (ListToolsResponse);

  // 注册自定义工具
  rpc RegisterTool(RegisterToolRequest) returns (RegisterToolResponse);

  // === 技能管理 ===

  // 加载技能
  rpc LoadSkill(LoadSkillRequest) returns (LoadSkillResponse);

  // 卸载技能
  rpc UnloadSkill(UnloadSkillRequest) returns (UnloadSkillResponse);

  // 列出技能
  rpc ListSkills(ListSkillsRequest) returns (ListSkillsResponse);

  // === 上下文管理 ===

  // 获取上下文使用情况
  rpc GetContextUsage(GetContextUsageRequest) returns (GetContextUsageResponse);

  // 压缩上下文
  rpc CompactContext(CompactContextRequest) returns (CompactContextResponse);

  // 清空上下文
  rpc ClearContext(ClearContextRequest) returns (ClearContextResponse);

  // === 事件流 ===

  // 订阅事件
  rpc SubscribeEvents(SubscribeEventsRequest) returns (stream AgentEvent);

  // === 控制操作 ===

  // 取消操作
  rpc Cancel(CancelRequest) returns (CancelResponse);

  // 暂停操作
  rpc Pause(PauseRequest) returns (PauseResponse);

  // 恢复操作
  rpc Resume(ResumeRequest) returns (ResumeResponse);
}
```

### 2. 消息定义

#### 2.1 健康检查

```protobuf
message HealthCheckRequest {}

message HealthCheckResponse {
  enum Status {
    UNKNOWN = 0;
    HEALTHY = 1;
    DEGRADED = 2;
    UNHEALTHY = 3;
  }

  Status status = 1;
  string message = 2;
  map<string, string> details = 3;
}
```

#### 2.2 能力声明

```protobuf
message GetCapabilitiesRequest {}

message GetCapabilitiesResponse {
  // 智能体基本信息
  AgentInfo info = 1;

  // 支持的功能
  repeated string features = 2;

  // 支持的工具
  repeated ToolCapability tools = 3;

  // 支持的模型
  repeated ModelCapability models = 4;

  // 资源限制
  ResourceLimits limits = 5;

  // 扩展元数据
  map<string, string> metadata = 6;
}

message AgentInfo {
  string name = 1;           // 智能体名称，如 "a3s-code", "opencode"
  string version = 2;        // 版本号，如 "0.1.0"
  string description = 3;    // 描述
  string author = 4;         // 作者
  string license = 5;        // 许可证
  string homepage = 6;       // 主页
}

message ToolCapability {
  string name = 1;           // 工具名称
  string description = 2;    // 工具描述
  repeated string parameters = 3;  // 参数列表
  bool async = 4;            // 是否支持异步执行
}

message ModelCapability {
  string provider = 1;       // 提供商，如 "anthropic", "openai"
  string model = 2;          // 模型名称
  repeated string features = 3;  // 支持的功能
}

message ResourceLimits {
  uint64 max_context_tokens = 1;    // 最大上下文 token 数
  uint32 max_concurrent_sessions = 2;  // 最大并发会话数
  uint32 max_tools_per_request = 3;    // 单次请求最大工具数
}
```

#### 2.3 初始化

```protobuf
message InitializeRequest {
  // 工作目录
  string workspace = 1;

  // 配置
  AgentConfig config = 2;

  // 环境变量
  map<string, string> env = 3;
}

message InitializeResponse {
  bool success = 1;
  string message = 2;
  AgentInfo info = 3;
}

message AgentConfig {
  // LLM 配置
  LLMConfig llm = 1;

  // 工具配置
  ToolsConfig tools = 2;

  // 日志配置
  LogConfig log = 3;

  // 自定义配置
  map<string, string> custom = 4;
}

message LLMConfig {
  string provider = 1;       // 提供商
  string model = 2;          // 模型
  string api_key = 3;        // API 密钥
  string base_url = 4;       // 基础 URL
  float temperature = 5;     // 温度
  uint32 max_tokens = 6;     // 最大 token 数
}

message ToolsConfig {
  repeated string enabled_tools = 1;   // 启用的工具
  repeated string disabled_tools = 2;  // 禁用的工具
  map<string, string> tool_config = 3; // 工具配置
}

message LogConfig {
  enum Level {
    DEBUG = 0;
    INFO = 1;
    WARN = 2;
    ERROR = 3;
  }

  Level level = 1;
  string format = 2;
  string output = 3;
}
```

#### 2.4 会话管理

```protobuf
message CreateSessionRequest {
  // 会话 ID（可选，不提供则自动生成）
  string session_id = 1;

  // 会话配置
  SessionConfig config = 2;

  // 初始上下文
  repeated Message initial_context = 3;
}

message CreateSessionResponse {
  string session_id = 1;
  Session session = 2;
}

message SessionConfig {
  // 会话名称
  string name = 1;

  // 工作目录
  string workspace = 2;

  // LLM 配置（覆盖全局配置）
  LLMConfig llm = 3;

  // 系统提示词
  string system_prompt = 4;

  // 最大上下文长度
  uint32 max_context_length = 5;

  // 自动压缩上下文
  bool auto_compact = 6;
}

message Session {
  string session_id = 1;
  SessionConfig config = 2;
  SessionState state = 3;
  ContextUsage context_usage = 4;
  int64 created_at = 5;
  int64 updated_at = 6;
}

enum SessionState {
  SESSION_STATE_UNKNOWN = 0;
  SESSION_STATE_ACTIVE = 1;
  SESSION_STATE_PAUSED = 2;
  SESSION_STATE_COMPLETED = 3;
  SESSION_STATE_ERROR = 4;
}

message ContextUsage {
  uint32 total_tokens = 1;
  uint32 prompt_tokens = 2;
  uint32 completion_tokens = 3;
  uint32 message_count = 4;
}
```

#### 2.5 代码生成

```protobuf
message GenerateRequest {
  // 会话 ID
  string session_id = 1;

  // 用户消息
  repeated Message messages = 2;

  // 生成选项
  GenerateOptions options = 3;
}

message Message {
  enum Role {
    ROLE_UNKNOWN = 0;
    ROLE_USER = 1;
    ROLE_ASSISTANT = 2;
    ROLE_SYSTEM = 3;
    ROLE_TOOL = 4;
  }

  Role role = 1;
  string content = 2;
  repeated Attachment attachments = 3;
  map<string, string> metadata = 4;
}

message Attachment {
  enum Type {
    TYPE_UNKNOWN = 0;
    TYPE_FILE = 1;
    TYPE_IMAGE = 2;
    TYPE_CODE = 3;
    TYPE_DATA = 4;
  }

  Type type = 1;
  string name = 2;
  string mime_type = 3;
  bytes content = 4;
  string url = 5;
}

message GenerateOptions {
  // 是否启用工具
  bool enable_tools = 1;

  // 允许的工具列表
  repeated string allowed_tools = 2;

  // 最大工具调用次数
  uint32 max_tool_calls = 3;

  // 生成参数
  float temperature = 4;
  uint32 max_tokens = 5;
  repeated string stop_sequences = 6;

  // 是否返回中间步骤
  bool return_intermediate_steps = 7;
}

message GenerateResponse {
  // 会话 ID
  string session_id = 1;

  // 生成的消息
  Message message = 2;

  // 工具调用
  repeated ToolCall tool_calls = 3;

  // 使用情况
  Usage usage = 4;

  // 完成原因
  FinishReason finish_reason = 5;

  // 元数据
  map<string, string> metadata = 6;
}

message ToolCall {
  string id = 1;
  string name = 2;
  string arguments = 3;  // JSON 格式
  ToolResult result = 4;
}

message ToolResult {
  bool success = 1;
  string output = 2;
  string error = 3;
  map<string, string> metadata = 4;
}

message Usage {
  uint32 prompt_tokens = 1;
  uint32 completion_tokens = 2;
  uint32 total_tokens = 3;
}

enum FinishReason {
  FINISH_REASON_UNKNOWN = 0;
  FINISH_REASON_STOP = 1;
  FINISH_REASON_LENGTH = 2;
  FINISH_REASON_TOOL_CALLS = 3;
  FINISH_REASON_CONTENT_FILTER = 4;
  FINISH_REASON_ERROR = 5;
}

// 流式响应
message GenerateChunk {
  enum ChunkType {
    CHUNK_TYPE_UNKNOWN = 0;
    CHUNK_TYPE_CONTENT = 1;
    CHUNK_TYPE_TOOL_CALL = 2;
    CHUNK_TYPE_TOOL_RESULT = 3;
    CHUNK_TYPE_METADATA = 4;
    CHUNK_TYPE_DONE = 5;
  }

  ChunkType type = 1;
  string session_id = 2;
  string content = 3;
  ToolCall tool_call = 4;
  ToolResult tool_result = 5;
  map<string, string> metadata = 6;
}
```

#### 2.6 工具执行

```protobuf
message ExecuteToolRequest {
  string session_id = 1;
  string tool_name = 2;
  string arguments = 3;  // JSON 格式
  map<string, string> options = 4;
}

message ExecuteToolResponse {
  ToolResult result = 1;
}

message ListToolsRequest {
  string session_id = 1;
}

message ListToolsResponse {
  repeated Tool tools = 1;
}

message Tool {
  string name = 1;
  string description = 2;
  string parameters_schema = 3;  // JSON Schema
  repeated string tags = 4;
  bool async = 5;
}
```

#### 2.7 事件流

```protobuf
message SubscribeEventsRequest {
  string session_id = 1;
  repeated string event_types = 2;
}

message AgentEvent {
  enum EventType {
    EVENT_TYPE_UNKNOWN = 0;
    EVENT_TYPE_SESSION_CREATED = 1;
    EVENT_TYPE_SESSION_DESTROYED = 2;
    EVENT_TYPE_GENERATION_STARTED = 3;
    EVENT_TYPE_GENERATION_COMPLETED = 4;
    EVENT_TYPE_TOOL_CALLED = 5;
    EVENT_TYPE_TOOL_COMPLETED = 6;
    EVENT_TYPE_ERROR = 7;
    EVENT_TYPE_WARNING = 8;
    EVENT_TYPE_INFO = 9;
  }

  EventType type = 1;
  string session_id = 2;
  int64 timestamp = 3;
  string message = 4;
  map<string, string> data = 5;
}
```

## 内置工具规范

所有编码智能体应该支持以下内置工具：

### 文件操作工具

1. **read_file** - 读取文件内容
2. **write_file** - 写入文件内容
3. **edit_file** - 编辑文件（精确替换）
4. **delete_file** - 删除文件
5. **list_files** - 列出文件
6. **search_files** - 搜索文件（glob 模式）

### 代码操作工具

7. **grep** - 搜索代码内容
8. **find_definition** - 查找定义
9. **find_references** - 查找引用
10. **format_code** - 格式化代码
11. **lint_code** - 代码检查

### 命令执行工具

12. **bash** - 执行 bash 命令
13. **run_script** - 运行脚本

### Git 工具

14. **git_status** - Git 状态
15. **git_diff** - Git 差异
16. **git_commit** - Git 提交
17. **git_log** - Git 日志

### 其他工具

18. **web_search** - 网络搜索
19. **web_fetch** - 获取网页内容
20. **ask_user** - 询问用户

## 工具参数规范

每个工具必须提供 JSON Schema 定义其参数：

```json
{
  "name": "read_file",
  "description": "Read the contents of a file",
  "parameters": {
    "type": "object",
    "properties": {
      "path": {
        "type": "string",
        "description": "Path to the file to read"
      },
      "encoding": {
        "type": "string",
        "enum": ["utf-8", "ascii", "base64"],
        "default": "utf-8",
        "description": "File encoding"
      }
    },
    "required": ["path"]
  }
}
```

## 错误处理

所有 RPC 方法应该使用标准的 gRPC 状态码：

```protobuf
enum ErrorCode {
  OK = 0;
  CANCELLED = 1;
  UNKNOWN = 2;
  INVALID_ARGUMENT = 3;
  DEADLINE_EXCEEDED = 4;
  NOT_FOUND = 5;
  ALREADY_EXISTS = 6;
  PERMISSION_DENIED = 7;
  RESOURCE_EXHAUSTED = 8;
  FAILED_PRECONDITION = 9;
  ABORTED = 10;
  OUT_OF_RANGE = 11;
  UNIMPLEMENTED = 12;
  INTERNAL = 13;
  UNAVAILABLE = 14;
  DATA_LOSS = 15;
  UNAUTHENTICATED = 16;
}

message Error {
  ErrorCode code = 1;
  string message = 2;
  repeated ErrorDetail details = 3;
}

message ErrorDetail {
  string field = 1;
  string message = 2;
}
```

## 协议适配

### gRPC 实现（推荐）

直接实现上述 protobuf 定义的接口。

### REST API 实现

如果智能体使用 REST API（如 OpenCode），需要提供适配器：

```
POST /sessions                    → CreateSession
DELETE /sessions/{id}             → DestroySession
POST /sessions/{id}/generate      → Generate
GET /sessions/{id}/generate/stream → StreamGenerate
POST /sessions/{id}/tools/{name}  → ExecuteTool
GET /health                       → HealthCheck
GET /capabilities                 → GetCapabilities
```

### WebSocket 实现

通过 WebSocket 传输 JSON 格式的消息：

```json
{
  "method": "generate",
  "params": {
    "session_id": "session-123",
    "messages": [...]
  },
  "id": "request-456"
}
```

## 实现示例

### 最小实现（Rust）

```rust
use tonic::{Request, Response, Status};
use a3s_code_agent::*;

pub struct MyCodeAgent {
    // 智能体状态
}

#[tonic::async_trait]
impl CodeAgentService for MyCodeAgent {
    async fn health_check(
        &self,
        _request: Request<HealthCheckRequest>,
    ) -> Result<Response<HealthCheckResponse>, Status> {
        Ok(Response::new(HealthCheckResponse {
            status: HealthCheckResponse::Status::Healthy as i32,
            message: "OK".to_string(),
            details: HashMap::new(),
        }))
    }

    async fn get_capabilities(
        &self,
        _request: Request<GetCapabilitiesRequest>,
    ) -> Result<Response<GetCapabilitiesResponse>, Status> {
        Ok(Response::new(GetCapabilitiesResponse {
            info: Some(AgentInfo {
                name: "my-code-agent".to_string(),
                version: "1.0.0".to_string(),
                description: "My custom coding agent".to_string(),
                author: "Me".to_string(),
                license: "MIT".to_string(),
                homepage: "https://example.com".to_string(),
            }),
            features: vec![
                "code_generation".to_string(),
                "tool_execution".to_string(),
            ],
            tools: vec![
                ToolCapability {
                    name: "read_file".to_string(),
                    description: "Read file contents".to_string(),
                    parameters: vec!["path".to_string()],
                    async_: false,
                },
            ],
            models: vec![],
            limits: Some(ResourceLimits {
                max_context_tokens: 200000,
                max_concurrent_sessions: 10,
                max_tools_per_request: 20,
            }),
            metadata: HashMap::new(),
        }))
    }

    // 实现其他方法...
}
```

### 最小实现（Python）

```python
import grpc
from concurrent import futures
from a3s_code_agent_pb2_grpc import CodeAgentServiceServicer
from a3s_code_agent_pb2 import *

class MyCodeAgent(CodeAgentServiceServicer):
    def HealthCheck(self, request, context):
        return HealthCheckResponse(
            status=HealthCheckResponse.HEALTHY,
            message="OK",
            details={}
        )

    def GetCapabilities(self, request, context):
        return GetCapabilitiesResponse(
            info=AgentInfo(
                name="my-code-agent",
                version="1.0.0",
                description="My custom coding agent",
                author="Me",
                license="MIT",
                homepage="https://example.com"
            ),
            features=["code_generation", "tool_execution"],
            tools=[
                ToolCapability(
                    name="read_file",
                    description="Read file contents",
                    parameters=["path"],
                    async_=False
                )
            ],
            models=[],
            limits=ResourceLimits(
                max_context_tokens=200000,
                max_concurrent_sessions=10,
                max_tools_per_request=20
            ),
            metadata={}
        )

    # 实现其他方法...

def serve():
    server = grpc.server(futures.ThreadPoolExecutor(max_workers=10))
    add_CodeAgentServiceServicer_to_server(MyCodeAgent(), server)
    server.add_insecure_port('[::]:4088')
    server.start()
    server.wait_for_termination()
```

## 集成到 A3S Box

### 1. 配置编码智能体

```yaml
# box-config.yaml
coding_agent:
  kind: "custom"
  name: "my-code-agent"
  image: "ghcr.io/myorg/my-code-agent:v1"
  protocol: "grpc"  # 或 "rest", "websocket"
  port: 4088
```

### 2. A3S Box 自动发现

A3S Box 会：
1. 启动智能体容器
2. 调用 `HealthCheck` 确认就绪
3. 调用 `GetCapabilities` 获取能力
4. 调用 `Initialize` 初始化智能体
5. 开始使用智能体

### 3. 协议适配

如果智能体使用非 gRPC 协议，A3S Box 会自动加载适配器：

```rust
// A3S Box 内部
let agent_client = match config.protocol {
    Protocol::Grpc => GrpcAgentClient::new(config),
    Protocol::Rest => RestAgentAdapter::new(config),
    Protocol::WebSocket => WebSocketAgentAdapter::new(config),
};
```

## 兼容性矩阵

| 智能体 | 协议 | 适配器 | 状态 |
|--------|------|--------|------|
| A3S Code | gRPC | 原生 | ✅ 完全支持 |
| OpenCode | REST | REST 适配器 | ✅ 完全支持 |
| Claude Code | 专有 | 专有适配器 | 🚧 计划中 |
| 自定义智能体 | gRPC/REST/WS | 自动检测 | ✅ 完全支持 |

## 测试和验证

### 1. 接口测试

```bash
# 健康检查
grpcurl -plaintext localhost:4088 a3s.code.agent.v1.CodeAgentService/HealthCheck

# 获取能力
grpcurl -plaintext localhost:4088 a3s.code.agent.v1.CodeAgentService/GetCapabilities

# 创建会话
grpcurl -plaintext -d '{"config": {"name": "test"}}' \
  localhost:4088 a3s.code.agent.v1.CodeAgentService/CreateSession
```

### 2. 兼容性测试

A3S Box 提供测试套件验证智能体兼容性：

```bash
a3s-box test-agent --image ghcr.io/myorg/my-agent:latest
```

## 未来规划

### 近期计划
- 多模态支持（图片、音频）
- 协作式编辑
- 实时协作

### 远期计划
- 分布式智能体
- 智能体间通信
- 联邦学习

---

**最后更新**: 2026-02-03
