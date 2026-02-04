# A3S Box

<p align="center">
  <strong>Meta-Agent Sandbox Runtime based on MicroVMs</strong>
</p>

<p align="center">
  <em>Hardware-isolated AI agent execution — run untrusted code safely with microVM sandboxing</em>
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

> **Current Focus**: We are implementing CRI (Container Runtime Interface) support to enable A3S Box to run as a Kubernetes container runtime. See the [CRI Implementation Plan](./docs/cri-implementation-plan.md) for details.

### Basic Usage

```typescript
import { A3sClient } from "@a3s-lab/box";

const client = new A3sClient();
const sessionId = await client.createSession({
  system: "You are a helpful coding assistant.",
});

// Generate text
const response = await client.generate(sessionId, "Write a hello world in Python");
console.log(response.text);

// View tool calls made by the agent
response.toolCalls.forEach(call => {
  console.log(`Tool: ${call.name}, Args: ${call.args}`);
});

await client.destroySession(sessionId);
```

### Streaming Responses

```typescript
// Stream responses with real-time tool visibility
for await (const chunk of client.stream(sessionId, "Create a REST API server")) {
  if (chunk.textDelta) {
    process.stdout.write(chunk.textDelta);
  }
  if (chunk.toolCall) {
    console.log(`\n[Tool] ${chunk.toolCall.name}: ${chunk.toolCall.args}`);
  }
  if (chunk.toolResult) {
    console.log(`[Result] ${chunk.toolResult.output.slice(0, 100)}...`);
  }
}
```

### Structured Output

```typescript
// Generate typed JSON objects with schema validation
const result = await client.generateObject(sessionId, "Create a user profile", {
  type: "object",
  properties: {
    name: { type: "string" },
    age: { type: "number" },
    skills: { type: "array", items: { type: "string" } },
  },
  required: ["name", "age"],
});
console.log(result.object); // { name: "Alice", age: 28, skills: ["TypeScript", "Rust"] }
```

### Multi-Provider Support

```typescript
// Use different LLM providers per session
const claudeSession = await client.createSession({
  system: "You are Claude.",
  model: { provider: "anthropic", name: "claude-sonnet-4-20250514" },
});

const gptSession = await client.createSession({
  system: "You are GPT.",
  model: { provider: "openai", name: "gpt-4o" },
});

// Also supports: deepseek, groq, together, ollama (OpenAI-compatible)
```

### Skills (Extensible Tools)

```typescript
// Load skills globally (available to all sessions)
await client.loadSkill("web-search", skillContent);
await client.loadSkill("image-gen", skillContent);

// Skills automatically available to all sessions
const response = await client.generate(sessionId, "Search for Rust async patterns");

// Use PermissionPolicy to control per-session tool access
await client.setPermissionPolicy(sessionId, {
  allow: ["bash", "read", "web-search"],
  deny: ["image-gen"],  // Disable for this session
});
```

## Features

- 🔒 **Hardware Isolation**: Each agent runs in its own microVM with dedicated Linux kernel
- 🚀 **Instant Boot**: Sub-second VM startup with libkrun (~200ms cold start)
- 🐳 **OCI Image Support**: Load agents and business code from standard OCI container images
- 🔐 **Namespace Isolation**: Agent and business code run in separate Linux namespaces
- 🛠️ **7 Built-in Tools**: bash, read, write, edit, grep, glob, ls — all sandboxed
- 🔄 **Streaming**: Real-time streaming responses with tool call visibility
- 📦 **Structured Output**: Generate JSON objects with schema validation
- 🎯 **Multi-Session**: Run multiple independent conversations in parallel
- 🔌 **Multi-Provider**: Anthropic Claude, OpenAI GPT, DeepSeek, Groq, Ollama
- 📊 **Lane-Based Queue**: 4 priority lanes (Control, Query, Execute, Generate)
- 🧩 **Skill System**: Global extensible tools via SKILL.md (Binary, HTTP, Script backends)
- 🪝 **Hooks System**: Extensible hooks for validating, transforming, or blocking operations
- 👤 **Human-in-the-Loop**: Confirmation system for sensitive operations
- 💾 **Session Persistence**: JSON file storage (default) with pluggable backends
- 📈 **574 Tests**: Comprehensive test coverage across 5 crates

## Quick Start

### Prerequisites

- **macOS ARM64** (Apple Silicon) or **Linux x86_64/ARM64**
- Rust 1.75+ (for building)
- Node.js 18+ (for TypeScript SDK)

> ⚠️ **Note**: macOS Intel is NOT supported

### Installation

#### macOS (Apple Silicon)

```bash
# Install Homebrew dependencies
brew install lld        # LLVM linker (required for cross-compiling init binary)
brew install llvm       # LLVM/Clang (required for bindgen)

# Clone the repository
git clone https://github.com/a3s-lab/box.git
cd box

# Initialize git submodules (libkrun source)
git submodule update --init --recursive

# Build the project
cd src && cargo build --release
```

> **Note**: The first build will automatically download prebuilt `libkrunfw` (~10 seconds) and build `libkrun` from source (~1-2 minutes).

#### Linux (Ubuntu/Debian)

```bash
# Install system dependencies
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libssl-dev

# Clone the repository
git clone https://github.com/a3s-lab/box.git
cd box

# Initialize git submodules
git submodule update --init --recursive

# Build the project
cd src && cargo build --release
```

#### TypeScript SDK

```bash
cd examples/typescript && npm install
```

### Build Modes

| Mode | Command | Use Case |
|------|---------|----------|
| **Full Build** | `cargo build` | Development with VM support |
| **Stub Mode** | `A3S_DEPS_STUB=1 cargo build` | CI linting, tests without VM |

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
│  └──────┬──────┘  └──────┬──────┘  └──────────┬──────────┘  │
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
│  │  ┌─────────────────────────────────────────────────┐  │  │
│  │  │ OCI Support: Image parsing, layer extraction,   │  │  │
│  │  │ rootfs composition from multiple OCI images     │  │  │
│  │  └─────────────────────────────────────────────────┘  │  │
│  └───────────────────────────┬───────────────────────────┘  │
│                              │ gRPC over vsock:4088         │
└──────────────────────────────┼──────────────────────────────┘
                               │
┌──────────────────────────────┼──────────────────────────────┐
│                              ▼                               │
│  ┌───────────────────────────────────────────────────────┐  │
│  │              /sbin/init (guest-init, PID 1)           │  │
│  │  - Mount filesystems (/proc, /sys, /dev, virtio-fs)   │  │
│  │  - Create isolated namespaces (mount, PID, IPC, UTS)  │  │
│  │  - Spawn agent in namespace                           │  │
│  └───────────────────────────┬───────────────────────────┘  │
│                              │                               │
│  ┌───────────────────────────▼───────────────────────────┐  │
│  │                   a3s-box-code (Namespace 1)          │  │
│  │  ┌─────────────┐ ┌─────────────┐ ┌─────────────────┐  │  │
│  │  │ Agent Loop  │ │ LLM Client  │ │ Tool Executor   │  │  │
│  │  │ (agentic)   │ │ (providers) │ │ (sandboxed)     │  │  │
│  │  └─────────────┘ └─────────────┘ └─────────────────┘  │  │
│  └───────────────────────────┬───────────────────────────┘  │
│                              │ /usr/bin/nsexec              │
│  ┌───────────────────────────▼───────────────────────────┐  │
│  │              Business Code (Namespace 2)              │  │
│  │  - Isolated execution environment                     │  │
│  │  - Separate mount, PID, IPC, UTS namespaces           │  │
│  │  - User application code runs here                    │  │
│  └───────────────────────────────────────────────────────┘  │
│                        Guest VM (microVM)                    │
└──────────────────────────────────────────────────────────────┘
```

### Workspace Crates

| Crate | Type | Purpose |
|-------|------|---------|
| `core` | lib | Foundational types: `BoxConfig`, `BoxError`, `BoxEvent`, `CommandQueue` |
| `runtime` | lib | VM lifecycle, session management, gRPC client, OCI image support |
| `code` | bin | Guest agent: LLM providers, tool execution, session management |
| `queue` | lib | `QueueManager` (builder pattern) and `QueueMonitor` (health checking) |
| `guest/init` | bin | Guest init (PID 1) and nsexec for namespace isolation |
| `shim` | bin | CRI shim for Kubernetes integration |
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

### Skills (Global)

| Method | Description |
|--------|-------------|
| `loadSkill(name, content)` | Load a skill globally (available to all sessions) |
| `unloadSkill(name)` | Unload a skill |
| `listSkills()` | List all loaded skills and tools |
| `setPermissionPolicy(sessionId, policy)` | Control per-session tool access |

### Session Persistence

Sessions are automatically persisted and restored on restart. The default storage uses JSON files, but you can implement custom backends via the `SessionStore` trait.

```rust
// Default: JSON file storage
let manager = SessionManager::with_persistence(
    llm_client,
    tool_executor,
    "/path/to/sessions",  // Directory for session files
).await?;

// Custom storage backend (e.g., Redis, PostgreSQL)
let custom_store = MyCustomStore::new();
let manager = SessionManager::with_store(
    llm_client,
    tool_executor,
    Arc::new(custom_store),
);
```

The `SessionStore` trait requires implementing:
- `save(session)` - Persist session data
- `load(id)` - Load session by ID
- `delete(id)` - Remove session
- `list()` - List all session IDs
- `exists(id)` - Check if session exists

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
- [x] MicroVM runtime with libkrun (shim binary complete)
- [x] gRPC service over vsock (guest agent service complete)
- [x] Multi-session support (state machine)
- [x] Lane-based command queue (4 priority lanes: Control, Query, Execute, Generate)
- [x] Basic tool suite (bash, read, write, edit, grep, glob, ls)
- [x] Dynamic tool loaders (binary, HTTP, script)
- [x] Human-in-the-loop confirmation system

**LLM Integration**
- [x] Anthropic Claude support
- [x] OpenAI GPT support
- [x] Streaming responses
- [x] Tool calling
- [x] Token usage tracking

**Rootfs & VM**
- [x] RootfsBuilder for minimal guest filesystem
- [x] GuestLayout configuration
- [x] Virtualization support detection (KVM, Apple HVF)
- [x] VmController with subprocess isolation

### Phase 2: Structured Output & Stability 🚧

**Structured Generation**
- [x] `generateObject` - JSON schema-based generation
- [x] `streamObject` - Streaming structured output
- [x] Response transformers (think tag removal, JSON extraction)
- [ ] Schema validation with detailed errors
- [ ] Partial object streaming with incremental parsing

**Reliability**
- [x] Session persistence (JSON file default, pluggable `SessionStore` trait)
- [ ] Request cancellation (stub exists)
- [x] Timeout handling (bash tool)
- [ ] Retry with exponential backoff
- [ ] Connection pooling
- [ ] Host-to-guest gRPC client (runtime/grpc.rs)

**Developer Experience**
- [x] Rust design guidelines (CLAUDE.md)
- [x] Language policy (English for code/docs)
- [x] Comprehensive test suite (574 tests across 4 crates)
- [ ] API documentation
- [ ] Error message improvements

### Phase 3: CRI Runtime Integration 🚧

**OCI Image Support** ✅
- [x] OCI image parser (manifest, config, layers) - `runtime/src/oci/`
- [x] Rootfs extraction from OCI images - `OciRootfsBuilder` with layer composition
- [x] Integration with Box runtime - `VmManager` OCI support
- [x] Guest init (PID 1) - `/sbin/init` for VM initialization
- [x] Namespace isolation - Mount, PID, IPC, UTS namespaces for agent and business code
- [x] Nsexec tool - Command-line tool for executing code in isolated namespaces
- [ ] OCI image format definition and Dockerfile
- [ ] Agent configuration from OCI labels

**CRI RuntimeService** (3-4 weeks)
- [ ] CRI service structure and gRPC server
- [ ] Pod Sandbox lifecycle (create, start, stop, remove)
- [ ] Container lifecycle (create, start, stop, remove)
- [ ] Pod/Container status and listing
- [ ] Configuration mapping (K8s → Box)
- [ ] Exec and attach support

**CRI ImageService** (2-3 weeks)
- [ ] Image management (list, pull, remove)
- [ ] Image cache with LRU eviction
- [ ] Image status and filesystem usage

**Deployment & Testing** (2-3 weeks)
- [ ] RuntimeClass configuration
- [ ] DaemonSet deployment manifests
- [ ] kubelet integration
- [ ] Integration tests with crictl
- [ ] End-to-end K8s testing

### Phase 4: Production Optimization 📋

**Performance**
- [ ] Image caching and preloading
- [ ] Box instance pooling
- [ ] Fast VM boot optimization
- [ ] Resource usage optimization

**Observability**
- [ ] Prometheus metrics export
- [ ] OpenTelemetry integration
- [ ] Performance metrics dashboard
- [ ] Cost tracking

**Security**
- [ ] Resource limits enforcement
- [ ] Network isolation policies
- [ ] Audit logging
- [ ] Secret management

### Phase 5: Elastic Scaling 📋

**Core Autoscaler**
- [ ] Metrics collector (queue depth, concurrency, latency)
- [ ] Scaler controller with reactive scaling
- [ ] Scale up/down with cooldown periods
- [ ] Integration with CRI runtime

**Warm Pool**
- [ ] Box pool manager
- [ ] Warm instance lifecycle (Cold → Warm → Hot → Drain)
- [ ] Pool size management
- [ ] Cold start optimization (< 500ms target)

**Session Management**
- [ ] Session router with affinity
- [ ] Session state checkpointing
- [ ] Session migration during drain
- [ ] Queue-aware load balancing

**Scale to Zero**
- [ ] Grace period handling
- [ ] State preservation before termination
- [ ] Fast resume from zero
- [ ] Activation queue for pending requests

**Kubernetes Operator**
- [ ] BoxAutoscaler CRD
- [ ] BoxDeployment CRD with revisions
- [ ] Traffic splitting (A/B testing)
- [ ] Predictive scaling (optional)

### Phase 6: Skill System ✅

**Skill Infrastructure**
- [x] SKILL.md parser (YAML frontmatter) - runtime & code agent
- [x] Global skill loading (available to all sessions)
- [x] Per-session tool access via PermissionPolicy
- [x] Skill filtering interface (`SkillFilter` trait)
- [x] Dynamic tool backends (Binary, HTTP, Script)
- [x] Lazy tool download (on first use)

**Built-in Skills**
- [ ] Web fetch skill
- [ ] Image generation skill
- [ ] Code execution skill (multi-language)
- [ ] Database query skill

**Skill Marketplace**
- [ ] Skill registry
- [ ] Version management
- [ ] Dependency resolution

### Phase 7: Ecosystem 📋

**SDKs**
- [ ] TypeScript SDK (bindings exist, needs full implementation)
- [ ] Python SDK (bindings exist, needs full implementation)
- [ ] Go SDK
- [ ] Rust SDK (native)

**Platform Support**
- [ ] Helm charts for Box Operator
- [ ] Cloud provider integrations (AWS, GCP, Azure)
- [ ] Managed service templates

**Integrations**
- [ ] VS Code extension
- [ ] JetBrains plugin
- [ ] GitHub Actions
- [ ] CI/CD pipelines

**Documentation**
- [ ] Interactive tutorials
- [ ] Video guides
- [ ] Best practices guide
- [ ] Migration guides

---

## Development

### Dependencies

#### macOS

| Dependency | Install | Purpose |
|------------|---------|---------|
| `lld` | `brew install lld` | LLVM linker for cross-compiling guest init binary |
| `llvm` | `brew install llvm` | libclang for Rust bindgen (FFI generation) |
| `libkrun` | git submodule | MicroVM hypervisor (built from source) |
| `libkrunfw` | auto-download | Prebuilt Linux kernel for guest VM |
| `cargo-llvm-cov` | `cargo install cargo-llvm-cov` | Code coverage (optional) |
| `lcov` | `brew install lcov` | Coverage report formatting (optional) |

#### Linux

| Dependency | Install | Purpose |
|------------|---------|---------|
| `build-essential` | `apt install build-essential` | GCC, make, etc. |
| `pkg-config` | `apt install pkg-config` | Library discovery |
| `libssl-dev` | `apt install libssl-dev` | TLS support |
| `libkrun` | git submodule | MicroVM hypervisor (built from source) |
| `libkrunfw` | auto-download | Prebuilt .so for guest VM |
| `cargo-llvm-cov` | `cargo install cargo-llvm-cov` | Code coverage (optional) |
| `lcov` | `apt install lcov` | Coverage report formatting (optional) |

### Build Commands

```bash
# Build
just build                            # Build all (Rust + SDKs)
just release                          # Release build
cd src && cargo build -p a3s-box-code # Build specific crate

# Test (with colored progress display)
just test                             # All tests with pretty output
just test-raw                         # Raw cargo output
just test-v                           # Verbose output (--nocapture)

# Test subsets
just test-code                        # Code agent tests
just test-core                        # Core crate tests
just test-skills                      # Skill loader tests
just test-tools                       # All tools tests
just test-queue                       # Queue and HITL tests
just test-runtime                     # Runtime (check only, requires libkrun for full tests)

# Coverage (requires cargo-llvm-cov + lcov)
just cov                              # Pretty terminal coverage report
just cov-html                         # HTML report (opens in browser)
just cov-table                        # File-by-file table
just cov-ci                           # Generate lcov.info for CI
just cov-module queue                 # Coverage for specific module

# Format & Lint
just fmt                              # Format code
just lint                             # Clippy lint
just ci                               # Full CI checks (fmt + lint + test)
```

### Project Structure

```
box/
├── src/
│   ├── core/           # Foundational types and error handling
│   ├── runtime/        # VM lifecycle, OCI support, and gRPC client
│   ├── code/           # Guest agent binary
│   ├── queue/          # Command queue utilities
│   ├── shim/           # CRI shim for Kubernetes
│   ├── guest/
│   │   └── init/       # Guest init (PID 1) and nsexec for namespace isolation
│   └── sdk/
│       ├── python/     # Python bindings (PyO3)
│       └── typescript/ # TypeScript bindings (NAPI-RS)
├── docs/               # Documentation
│   ├── architecture.md           # Architecture design
│   ├── code-agent-interface.md   # Coding agent interface spec
│   ├── configuration-guide.md    # Configuration guide
│   ├── cri-implementation-plan.md # CRI implementation plan
│   ├── elastic-scaling.md        # Autoscaling design
│   ├── extensible-tools.md       # Extensible tool system
│   ├── lane-based-queue.md       # Lane-based command queue
│   ├── llm-config-design.md      # LLM configuration design
│   ├── rootfs-explained.md       # Root filesystem explained
│   ├── middleware-design.md      # Response transformation middleware
│   ├── opencode-adapter.md       # OpenCode REST adapter
│   └── examples/                 # Configuration examples
├── examples/
│   └── typescript/     # TypeScript examples
├── CLAUDE.md           # AI coding guidelines
└── README.md           # This file
```

## Documentation

| Document | Description |
|----------|-------------|
| [Architecture](./docs/architecture.md) | Single-container + file mount architecture design |
| [libkrun Dependencies](./docs/libkrun-dependencies.md) | Why libkrun, libkrunfw, lld, and llvm are needed |
| [Git Submodules](./docs/git-submodules.md) | Understanding Git submodules and why we use them |
| [Configuration Guide](./docs/configuration-guide.md) | Coding agent, LLM, and skill configuration |
| [Lane-Based Queue](./docs/lane-based-queue.md) | Priority-aware command scheduling for coding agents |
| [Elastic Scaling](./docs/elastic-scaling.md) | Knative-inspired autoscaling for AI workloads |
| [Rootfs Explained](./docs/rootfs-explained.md) | Understanding root filesystem in MicroVMs |
| [CRI Implementation Plan](./docs/cri-implementation-plan.md) | Kubernetes CRI runtime integration plan |
| [Code Agent Interface](./docs/code-agent-interface.md) | Standard interface for coding agents |
| [OpenCode Adapter](./docs/opencode-adapter.md) | REST to gRPC adapter for OpenCode compatibility |
| [LLM Config Design](./docs/llm-config-design.md) | Multi-provider, per-model API key configuration |
| [Extensible Tools](./docs/extensible-tools.md) | Extensible tool system design |
| [Hooks System](./docs/hooks.md) | SDK-based hooks for customizing agent behavior |
| [Middleware Design](./docs/middleware-design.md) | Response transformation middleware design |
| [Examples](./docs/examples/) | LLM and configuration examples |

### Troubleshooting

#### macOS: `invalid linker name in argument '-fuse-ld=lld'`

Install lld separately (not included in homebrew llvm):
```bash
brew install lld
```

#### macOS: `Vendored sources not found`

Initialize git submodules:
```bash
git submodule update --init --recursive
```

#### Linux: `libkrunfw not found`

The prebuilt libkrunfw will be downloaded automatically. If download fails, check network connectivity or set `A3S_DEPS_STUB=1` for testing without VM support.

#### CI/Testing without VM

Use stub mode to skip libkrun build (tests that don't require VM will work):
```bash
A3S_DEPS_STUB=1 cargo check -p a3s-box-runtime
# Or use the justfile command which handles this automatically:
just test-runtime
```

### Contributing

1. Read [CLAUDE.md](./CLAUDE.md) for code design rules
2. Search before implementing (`grep -r "pattern" src/`)
3. Follow the pre-submission checklist
4. Run `just fmt` and `just lint` before committing
5. Run `just test` to verify all tests pass

## License

MIT

---

<p align="center">
  Built with ❤️ by <a href="https://github.com/a3s-lab">A3S Lab</a>
</p>
