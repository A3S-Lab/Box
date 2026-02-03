# A3S Code Agent 接口设计总结

## 📋 概述

我们设计了一套标准化的编码智能体接口，使得任何实现了该接口的编码智能体（如 A3S Code、OpenCode、Claude Code 等）都可以无缝集成到 A3S Box 中。

## 📚 相关文档

| 文档 | 说明 |
|------|------|
| [code-agent-interface.md](./code-agent-interface.md) | 完整的接口规范文档 |
| [opencode-adapter.md](./opencode-adapter.md) | OpenCode 适配器实现 |
| [../proto/code_agent.proto](../proto/code_agent.proto) | Protobuf 定义文件 |

## 🎯 设计目标

1. **协议无关** - 支持 gRPC、REST、WebSocket 等多种协议
2. **能力声明** - 智能体可以声明自己支持的功能
3. **工具可扩展** - 支持自定义工具和扩展
4. **会话管理** - 支持多会话并发
5. **流式响应** - 支持流式生成和事件推送

## 🔧 核心接口

### 1. 生命周期管理
- `HealthCheck` - 健康检查
- `GetCapabilities` - 获取智能体能力
- `Initialize` - 初始化智能体
- `Shutdown` - 关闭智能体

### 2. 会话管理
- `CreateSession` - 创建会话
- `DestroySession` - 销毁会话
- `ListSessions` - 列出会话
- `GetSession` - 获取会话信息
- `ConfigureSession` - 配置会话

### 3. 代码生成
- `Generate` - 生成代码（同步）
- `StreamGenerate` - 生成代码（流式）
- `GenerateStructured` - 生成结构化输出（同步）
- `StreamGenerateStructured` - 生成结构化输出（流式）

### 4. 工具执行
- `ExecuteTool` - 执行工具
- `ExecuteToolBatch` - 批量执行工具
- `ListTools` - 列出可用工具
- `RegisterTool` - 注册自定义工具

### 5. 技能管理
- `LoadSkill` - 加载技能
- `UnloadSkill` - 卸载技能
- `ListSkills` - 列出技能

### 6. 上下文管理
- `GetContextUsage` - 获取上下文使用情况
- `CompactContext` - 压缩上下文
- `ClearContext` - 清空上下文

### 7. 事件流
- `SubscribeEvents` - 订阅事件

### 8. 控制操作
- `Cancel` - 取消操作
- `Pause` - 暂停操作
- `Resume` - 恢复操作

## 🛠️ 内置工具规范

所有编码智能体应该支持以下 20 个内置工具：

### 文件操作 (6)
1. read_file
2. write_file
3. edit_file
4. delete_file
5. list_files
6. search_files

### 代码操作 (5)
7. grep
8. find_definition
9. find_references
10. format_code
11. lint_code

### 命令执行 (2)
12. bash
13. run_script

### Git 工具 (4)
14. git_status
15. git_diff
16. git_commit
17. git_log

### 其他 (3)
18. web_search
19. web_fetch
20. ask_user

## 💡 实现示例

### 最小实现（Rust）

```rust
use tonic::{Request, Response, Status};
use a3s_code_agent::*;

pub struct MyCodeAgent {}

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
                // ...
            }),
            features: vec!["code_generation".to_string()],
            tools: vec![/* ... */],
            // ...
        }))
    }

    // 实现其他方法...
}
```

### 最小实现（Python）

```python
from a3s_code_agent_pb2_grpc import CodeAgentServiceServicer
from a3s_code_agent_pb2 import *

class MyCodeAgent(CodeAgentServiceServicer):
    def HealthCheck(self, request, context):
        return HealthCheckResponse(
            status=HealthCheckResponse.HEALTHY,
            message="OK"
        )

    def GetCapabilities(self, request, context):
        return GetCapabilitiesResponse(
            info=AgentInfo(
                name="my-code-agent",
                version="1.0.0"
            ),
            features=["code_generation"]
        )

    # 实现其他方法...
```

## 🔌 协议适配

### gRPC（推荐）
直接实现 protobuf 定义的接口。

### REST API
通过适配器转换：
```
POST /sessions → CreateSession
POST /sessions/{id}/generate → Generate
GET /sessions/{id}/generate/stream → StreamGenerate
```

### WebSocket
通过 JSON 消息传输：
```json
{
  "method": "generate",
  "params": {...},
  "id": "request-123"
}
```

## 📦 集成到 A3S Box

### 1. 配置

```yaml
# box-config.yaml
coding_agent:
  kind: "custom"
  name: "my-code-agent"
  image: "ghcr.io/myorg/my-agent:v1"
  protocol: "grpc"
  port: 4088
```

### 2. 自动发现

A3S Box 会自动：
1. 启动智能体容器
2. 调用 `HealthCheck` 确认就绪
3. 调用 `GetCapabilities` 获取能力
4. 调用 `Initialize` 初始化
5. 开始使用智能体

### 3. 协议适配

如果智能体使用非 gRPC 协议，A3S Box 会自动加载适配器。

## 🎨 OpenCode 集成示例

OpenCode 使用 REST API，我们提供了完整的适配器实现：

```rust
pub struct OpenCodeAdapter {
    base_url: String,
    client: Client,
    sessions: Arc<RwLock<HashMap<String, String>>>,
}

#[tonic::async_trait]
impl CodeAgentService for OpenCodeAdapter {
    // 将 OpenCode 的 REST API 转换为 gRPC 接口
    async fn health_check(...) -> Result<...> {
        let health: HealthResponse = 
            self.call_opencode("GET", "/global/health", None).await?;
        // ...
    }

    async fn create_session(...) -> Result<...> {
        let project: Project = 
            self.call_opencode("POST", "/project", Some(body)).await?;
        // ...
    }

    // 其他方法...
}
```

详见 [opencode-adapter.md](./opencode-adapter.md)。

## ✅ 兼容性矩阵

| 智能体 | 协议 | 适配器 | 状态 |
|--------|------|--------|------|
| A3S Code | gRPC | 原生 | ✅ 完全支持 |
| OpenCode | REST | REST 适配器 | ✅ 完全支持 |
| Claude Code | 专有 | 专有适配器 | 🚧 计划中 |
| 自定义智能体 | gRPC/REST/WS | 自动检测 | ✅ 完全支持 |

## 🧪 测试和验证

### 接口测试

```bash
# 健康检查
grpcurl -plaintext localhost:4088 \
  a3s.code.agent.v1.CodeAgentService/HealthCheck

# 获取能力
grpcurl -plaintext localhost:4088 \
  a3s.code.agent.v1.CodeAgentService/GetCapabilities

# 创建会话
grpcurl -plaintext -d '{"config": {"name": "test"}}' \
  localhost:4088 \
  a3s.code.agent.v1.CodeAgentService/CreateSession
```

### 兼容性测试

```bash
# A3S Box 提供测试套件
a3s-box test-agent --image ghcr.io/myorg/my-agent:latest
```

## 📈 未来规划

### 近期计划
- 🚧 多模态支持（图片、音频）
- 🚧 协作式编辑
- 🚧 实时协作
- 🚧 WebSocket 适配器

### 远期计划
- 📋 分布式智能体
- 📋 智能体间通信
- 📋 联邦学习

## 🚀 快速开始

### 1. 实现智能体

选择你喜欢的语言实现 `CodeAgentService` 接口。

### 2. 测试智能体

```bash
# 启动智能体
./my-agent --port 4088

# 测试接口
grpcurl -plaintext localhost:4088 list
```

### 3. 集成到 A3S Box

```yaml
coding_agent:
  kind: "custom"
  image: "my-agent:latest"
```

### 4. 运行

```python
from a3s_box import create_box

box = await create_box()
await box.coding.generate("Write a function...")
```

## 📞 获取帮助

- 📖 完整文档: [code-agent-interface.md](./code-agent-interface.md)
- 🔧 适配器示例: [opencode-adapter.md](./opencode-adapter.md)
- 📝 Proto 定义: [../proto/code_agent.proto](../proto/code_agent.proto)
- 💬 讨论: GitHub Issues

---

**版本**: 1.0.0
**最后更新**: 2026-02-03
**状态**: 已发布
