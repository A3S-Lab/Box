# A3S Code Agent

Rust implementation of the coding agent that runs inside the guest VM. This is a full-featured agentic coding assistant, with tool calling, streaming, and session management.

## Architecture

```
┌─────────────────────────────────────────┐
│ Host (SDK / runtime)                    │
│ - gRPC Client                           │
│ - VM Management                         │
├─────────────────────────────────────────┤
│ Guest (code) ← This Package             │
│ - gRPC Server (:4088)                   │
│ - Agent Loop (LLM + Tools cycle)        │
│ - LLM Clients (Anthropic, OpenAI)       │
│ - Tool Executor (sandboxed)             │
│ - Session Manager (multi-session)       │
└─────────────────────────────────────────┘
```

## Components

### 1. Agent Loop (`agent.rs`)
Core agentic behavior implementation:
- Handles the prompt → LLM → tool execution → LLM cycle
- Supports up to 50 tool rounds per request
- Emits streaming events for real-time UI updates
- Tracks token usage and execution steps

### 2. LLM Clients (`llm.rs`)
Unified LLM API with tool calling support:
- **Anthropic Claude**: Full Messages API with tool use
- **OpenAI GPT**: Chat Completions API with function calling
- Streaming support with event types
- Token usage tracking (including cache hits)

### 3. Tool Executor (`tools.rs`)
Sandboxed tool implementations:
- `bash`: Execute shell commands with timeout, output truncation
- `read`: Read files with line numbers, pagination, image support
- `write`: Write files with automatic directory creation
- `edit`: String replacement with uniqueness validation, diff output
- `grep`: Ripgrep integration with glob filters and context
- `glob`: File pattern matching
- `ls`: Directory listing

### 4. Session Manager (`session.rs`)
Multi-session conversation management:
- Create/destroy independent sessions
- Conversation history tracking
- Context usage monitoring
- Thinking mode configuration
- Context compaction (basic)

### 5. gRPC Service (`service.rs`)
Full AgentService implementation:
- Session management (create, destroy)
- Generation (sync and streaming)
- Object generation with JSON schema
- Session commands (compact, clear, configure)
- Introspection (context usage, history)
- Control (cancel, health check)

## Building

```bash
# Build the agent
cargo build -p a3s-code

# Build release
cargo build -p a3s-code --release

# Run tests
cargo test -p a3s-code --lib
```

## Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `LLM_PROVIDER` | LLM provider (anthropic, openai) | anthropic |
| `ANTHROPIC_API_KEY` | Anthropic API key | - |
| `OPENAI_API_KEY` | OpenAI API key | - |
| `LLM_MODEL` | Model name | claude-sonnet-4-20250514 |
| `WORKSPACE` | Workspace directory path | /a3s/workspace |
| `LISTEN_ADDR` | gRPC listen address | 0.0.0.0:4088 |
| `RUST_LOG` | Log level | info |

## Usage

### Inside VM

```bash
# Set environment
export ANTHROPIC_API_KEY="sk-ant-..."
export WORKSPACE="/a3s/workspace"
export RUST_LOG=info

# Run agent
/a3s/agent/a3s-code
```

### Local Development

```bash
# Run with environment
ANTHROPIC_API_KEY=sk-ant-... \
WORKSPACE=/tmp/workspace \
LISTEN_ADDR=127.0.0.1:4088 \
cargo run -p a3s-code
```

## gRPC API

The agent exposes the `AgentService` gRPC service:

```protobuf
service AgentService {
  // Session management
  rpc CreateSession(CreateSessionRequest) returns (CreateSessionResponse);
  rpc DestroySession(DestroySessionRequest) returns (DestroySessionResponse);

  // Generation
  rpc Generate(GenerateRequest) returns (GenerateResponse);
  rpc Stream(GenerateRequest) returns (stream StreamChunk);
  rpc GenerateObject(GenerateObjectRequest) returns (GenerateObjectResponse);
  rpc StreamObject(GenerateObjectRequest) returns (stream ObjectStreamChunk);

  // Session commands
  rpc Compact(SessionCommandRequest) returns (SessionCommandResponse);
  rpc Clear(SessionCommandRequest) returns (SessionCommandResponse);
  rpc Configure(ConfigureRequest) returns (ConfigureResponse);

  // Introspection
  rpc GetContextUsage(ContextUsageRequest) returns (ContextUsageResponse);
  rpc GetHistory(HistoryRequest) returns (HistoryResponse);

  // Control
  rpc Cancel(CancelRequest) returns (CancelResponse);
  rpc HealthCheck(HealthCheckRequest) returns (HealthCheckResponse);
}
```

## Tool Definitions

All tools are available to the LLM with JSON Schema parameter definitions:

| Tool | Description |
|------|-------------|
| `bash` | Execute shell commands with timeout |
| `read` | Read files with line numbers and pagination |
| `write` | Write content to files |
| `edit` | Edit files with string replacement |
| `grep` | Search files with ripgrep |
| `glob` | Find files by pattern |
| `ls` | List directory contents |

## TODO

- [ ] Skill loading system
- [ ] Advanced context compaction with LLM summarization
- [ ] Cancel in-flight requests
- [ ] Proper vsock transport (currently TCP)
- [ ] Image tool (for vision models)
- [ ] Web fetch tool
- [ ] Comprehensive test suite
