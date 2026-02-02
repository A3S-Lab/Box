# A3S Box

<p align="center">
  <strong>Meta-Agent Sandbox Runtime based on MicroVMs</strong>
</p>

<p align="center">
  <em>"SQLite for sandboxing" — a lightweight library embedded directly in applications</em>
</p>

<p align="center">
  <a href="#features">Features</a> •
  <a href="#quick-start">Quick Start</a> •
  <a href="#architecture">Architecture</a> •
  <a href="#sdks">SDKs</a> •
  <a href="#roadmap">Roadmap</a>
</p>

---

## Overview

**A3S Box** embeds a full-featured coding agent inside hardware-isolated virtual machines, exposing Python and TypeScript SDKs. No daemon, no root privileges — just import the library and run sandboxed AI agents.

```typescript
import { A3sClient } from "@a3s-lab/box";

const client = new A3sClient();
const sessionId = await client.createSession({
  system: "You are a helpful coding assistant.",
});

// Generate text
const response = await client.generate(sessionId, "Write a hello world in Python");
console.log(response.text);

// Generate structured data
const result = await client.generateObject(sessionId, "Create a user profile", {
  type: "object",
  properties: {
    name: { type: "string" },
    age: { type: "number" },
  },
});
console.log(result.object);

await client.destroySession(sessionId);
```

## Features

- 🔒 **Hardware Isolation**: Each agent runs in its own microVM with dedicated Linux kernel
- 🚀 **Instant Boot**: Sub-second VM startup with libkrun
- 🛠️ **Full Tool Suite**: bash, read, write, edit, grep, glob — all sandboxed
- 🔄 **Streaming**: Real-time streaming responses with tool call visibility
- 📦 **Structured Output**: Generate JSON objects with schema validation
- 🎯 **Multi-Session**: Run multiple independent conversations
- 🔌 **Multi-Provider**: Anthropic Claude, OpenAI GPT, and more
- 📊 **Lane-Based Queue**: Priority scheduling for concurrent operations

## Quick Start

### Prerequisites

- **macOS ARM64** (Apple Silicon) or **Linux x86_64/ARM64**
- Rust 1.93+ (for building)
- Node.js 25.4.0+ (for TypeScript SDK)

> ⚠️ **Note**: macOS Intel is NOT supported

### Installation

```bash
# Clone the repository
git clone https://github.com/a3s-lab/box.git
cd box

# Build the project
cd src && cargo build --release

# Install TypeScript SDK dependencies
cd ../examples/typescript && npm install
```

### Running the Agent

```bash
# Set your API key
export ANTHROPIC_API_KEY="sk-ant-..."

# Start the code agent
cd src && cargo run -p a3s-box-code
```

### Running Examples

```bash
cd examples/typescript

# Basic usage
npx tsx src/index.ts

# Streaming responses
npx tsx src/stream-example.ts

# Structured object generation
npx tsx src/object-example.ts

# Tool calling
npx tsx src/tool-example.ts
```

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    Host Process                              │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐  │
│  │ Python SDK  │  │   TS SDK    │  │    Your App         │  │
│  └──────┬──────┘  └──────┬──────┘  └────��─────┬──────────┘  │
│         │                │                     │             │
│         └────────────────┼─────────────────────┘             │
│                          │                                   │
│                          ▼                                   │
│  ┌───────────────────────────────────────────────────────┐  │
│  │                  a3s-box-runtime                       │  │
│  │  ┌─────────────┐ ┌─────────────┐ ┌─────────────────┐  │  │
│  │  │ VmManager   │ │SessionMgr   │ │  CommandQueue   │  │  │
│  │  │ (lifecycle) │ │(multi-sess) │ │ (lane-based)    │  │  │
│  │  └─────────────┘ └─────────────┘ └─────────────────┘  │  │
│  └───────────────────────────┬───────────────────────────┘  │
│                              │ gRPC over vsock:4088         │
└──────────────────────────────┼──────────────────────────────┘
                               │
┌──────────────────────────────┼──────────────────────────────┐
│                              ▼                               │
│  ┌───────────────────────────────────────────────────────┐  │
│  │                   a3s-box-code                         │  │
│  │  ┌─────────────┐ ┌─────────────┐ ┌─────────────────┐  │  │
│  │  │ Agent Loop  │ │ LLM Client  │ │ Tool Executor   │  │  │
│  │  │ (agentic)   │ │ (providers) │ │ (sandboxed)     │  │  │
│  │  └─────────────┘ └─────────────┘ └─────────────────┘  │  │
│  └───────────────────────────────────────────────────────┘  │
│                        Guest VM (microVM)                    │
└──────────────────────────────────────────────────────────────┘
```

### Workspace Crates

| Crate | Type | Purpose |
|-------|------|---------|
| `core` | lib | Foundational types: `BoxConfig`, `BoxError`, `BoxEvent`, `CommandQueue` |
| `runtime` | lib | VM lifecycle, session management, gRPC client, virtio-fs mounts |
| `code` | bin | Guest agent: LLM providers, tool execution, session management |
| `queue` | lib | `QueueManager` (builder pattern) and `QueueMonitor` (health checking) |
| `cli` | bin | CLI commands: `create`, `build`, `cache-warmup` |
| `sdk/python` | cdylib | Python bindings via PyO3 |
| `sdk/typescript` | cdylib | TypeScript bindings via NAPI-RS |

## SDKs

### TypeScript SDK

```typescript
import { A3sClient, getConfig } from "@a3s-lab/box";

const config = getConfig();
const client = new A3sClient({ address: config.serverAddress });

// Create session
const sessionId = await client.createSession({
  system: "You are a helpful assistant.",
  model: config.model,
});

// Generate response
const response = await client.generate(sessionId, "Hello!");

// Stream response
for await (const chunk of client.stream(sessionId, "Explain closures")) {
  if (chunk.textDelta) process.stdout.write(chunk.textDelta);
}

// Generate structured object
const { object } = await client.generateObject(sessionId, "Create a task", {
  type: "object",
  properties: {
    title: { type: "string" },
    priority: { type: "string", enum: ["low", "medium", "high"] },
  },
});

// Cleanup
await client.destroySession(sessionId);
client.close();
```

### Python SDK (Coming Soon)

```python
from a3s_box import Box

async with Box() as box:
    session = await box.create_session(system="You are helpful.")

    # Generate response
    result = await session.generate("Hello!")
    print(result.text)

    # Stream response
    async for chunk in session.stream("Explain closures"):
        print(chunk.text, end="")

    # Generate structured object
    obj = await session.generate_object(
        "Create a task",
        schema={"type": "object", "properties": {"title": {"type": "string"}}}
    )
    print(obj)
```

## API Reference

### Session Management

| Method | Description |
|--------|-------------|
| `createSession(options?)` | Create a new conversation session |
| `destroySession(sessionId)` | Destroy a session and free resources |
| `configure(sessionId, options)` | Configure session settings (model, thinking mode) |

### Generation

| Method | Description |
|--------|-------------|
| `generate(sessionId, prompt)` | Generate a response (non-streaming) |
| `stream(sessionId, prompt)` | Stream a response with real-time chunks |
| `generateObject(sessionId, prompt, schema)` | Generate a structured JSON object |
| `streamObject(sessionId, prompt, schema)` | Stream a structured JSON object |

### Introspection

| Method | Description |
|--------|-------------|
| `getContextUsage(sessionId)` | Get token usage and context stats |
| `getHistory(sessionId)` | Get conversation history |
| `healthCheck()` | Check agent health |

### Session Commands

| Method | Description |
|--------|-------------|
| `clear(sessionId)` | Clear conversation history |
| `compact(sessionId)` | Compact context to reduce token usage |
| `cancel(sessionId)` | Cancel ongoing request |

## Configuration

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `ANTHROPIC_API_KEY` | Anthropic API key | - |
| `OPENAI_API_KEY` | OpenAI API key | - |
| `LLM_PROVIDER` | LLM provider (anthropic, openai) | anthropic |
| `LLM_MODEL` | Model name | claude-sonnet-4-20250514 |
| `A3S_SERVER_ADDRESS` | Agent server address | localhost:4088 |
| `WORKSPACE` | Workspace directory | /a3s/workspace |
| `RUST_LOG` | Log level | info |

### Supported Providers

| Provider | Models |
|----------|--------|
| Anthropic | claude-sonnet-4-20250514, claude-3-5-sonnet, claude-3-opus |
| OpenAI | gpt-4o, gpt-4-turbo, gpt-3.5-turbo |
| DeepSeek | deepseek-chat, deepseek-coder |
| Groq | llama-3.1-70b, mixtral-8x7b |
| Together | Various open models |
| Ollama | Local models |

---

## Roadmap

### Phase 1: Foundation ✅

**Core Infrastructure**
- [x] MicroVM runtime with libkrun
- [x] gRPC communication over vsock
- [x] Multi-session support
- [x] Lane-based command queue
- [x] Basic tool suite (bash, read, write, edit, grep, glob)

**LLM Integration**
- [x] Anthropic Claude support
- [x] OpenAI GPT support
- [x] Streaming responses
- [x] Tool calling

**SDK**
- [x] TypeScript SDK (gRPC client)
- [x] Basic examples

### Phase 2: Structured Output & Stability 🚧

**Structured Generation**
- [x] `generateObject` - JSON schema-based generation
- [x] `streamObject` - Streaming structured output
- [x] Response transformers (think tag removal, JSON extraction)
- [ ] Schema validation with detailed errors
- [ ] Partial object streaming with incremental parsing

**Reliability**
- [ ] Request cancellation
- [ ] Timeout handling
- [ ] Retry with exponential backoff
- [ ] Connection pooling

**Developer Experience**
- [x] TypeScript design guidelines (CLAUDE.md)
- [ ] Comprehensive test suite
- [ ] API documentation
- [ ] Error message improvements

### Phase 3: Skill System 📋

**Skill Infrastructure**
- [ ] SKILL.md parser (YAML frontmatter)
- [ ] Remote tool download and caching
- [ ] Skill activation/deactivation
- [ ] Custom lane provisioning per skill

**Built-in Skills**
- [ ] Web fetch skill
- [ ] Image generation skill
- [ ] Code execution skill (multi-language)
- [ ] Database query skill

**Skill Marketplace**
- [ ] Skill registry
- [ ] Version management
- [ ] Dependency resolution

### Phase 4: Advanced Features 📋

**Context Management**
- [ ] LLM-based context compaction
- [ ] Conversation summarization
- [ ] Long-term memory
- [ ] Context window optimization

**Multi-Agent**
- [ ] Agent-to-agent communication
- [ ] Hierarchical agent orchestration
- [ ] Shared context between agents

**Observability**
- [ ] OpenTelemetry integration
- [ ] Token usage analytics
- [ ] Performance metrics dashboard
- [ ] Cost tracking

### Phase 5: Production Ready 📋

**Security**
- [ ] Resource limits (CPU, memory, disk)
- [ ] Network isolation
- [ ] Audit logging
- [ ] Secret management

**Scalability**
- [ ] Horizontal scaling
- [ ] Load balancing
- [ ] Session persistence
- [ ] Distributed queue

**Platform Support**
- [ ] Docker container support
- [ ] Kubernetes operator
- [ ] Cloud provider integrations (AWS, GCP, Azure)

### Phase 6: Ecosystem 📋

**SDKs**
- [ ] Python SDK (full implementation)
- [ ] Go SDK
- [ ] Rust SDK (native)

**Integrations**
- [ ] VS Code extension
- [ ] JetBrains plugin
- [ ] GitHub Actions
- [ ] Slack/Discord bots

**Documentation**
- [ ] Interactive tutorials
- [ ] Video guides
- [ ] Best practices guide
- [ ] Migration guides

---

## Development

### Build Commands

```bash
cd src

# Build
cargo build                           # Build entire workspace
cargo build -p a3s-box-code           # Build specific crate
cargo build --release                 # Release build

# Test
cargo test --all                      # All tests
cargo test -p a3s-box-code --lib      # Unit tests for specific crate

# Format & Lint
cargo fmt --all                       # Format code
cargo clippy                          # Lint (enforced in CI)
```

### Project Structure

```
box/
├── src/
│   ├── core/           # Foundational types and error handling
│   ├── runtime/        # VM lifecycle and gRPC client
│   ├── code/           # Guest agent binary
│   ├── queue/          # Command queue utilities
│   ├── cli/            # CLI commands
│   └── sdk/
│       ├── python/     # Python bindings (PyO3)
│       └── typescript/ # TypeScript bindings (NAPI-RS)
├── examples/
│   └── typescript/     # TypeScript examples
├── CLAUDE.md           # AI coding guidelines
└── README.md           # This file
```

### Contributing

1. Read [CLAUDE.md](./CLAUDE.md) for code design rules
2. Search before implementing (`grep -r "pattern" src/`)
3. Follow the pre-submission checklist
4. Run `cargo fmt` and `cargo clippy` before committing

## License

MIT

---

<p align="center">
  Built with ❤️ by <a href="https://github.com/a3s-lab">A3S Lab</a>
</p>
