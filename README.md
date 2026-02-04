# A3S Box

<p align="center">
  <strong>MicroVM Sandbox Runtime for AI Agents</strong>
</p>

<p align="center">
  <em>Infrastructure layer — hardware-isolated execution environment with Python and TypeScript SDKs</em>
</p>

<p align="center">
  <a href="#features">Features</a> •
  <a href="#quick-start">Quick Start</a> •
  <a href="#architecture">Architecture</a> •
  <a href="#roadmap">Roadmap</a>
</p>

---

## Overview

**A3S Box** is a MicroVM-based sandbox runtime that provides hardware-isolated execution environments for AI agents. It handles VM lifecycle, OCI image management, and namespace isolation — allowing any AI agent to run securely inside a dedicated virtual machine.

Box is **not** an AI agent itself. It provides the secure sandbox infrastructure that agents run inside.

### What Box Does

- **VM Isolation**: Each sandbox runs in its own MicroVM with a dedicated Linux kernel
- **OCI Images**: Load agent code and dependencies from standard container images
- **Namespace Isolation**: Further isolate agent code from business code within the VM
- **CRI Integration**: Run as a Kubernetes container runtime (planned)

### What Box Does NOT Do

- LLM integration (handled by the agent running inside Box)
- Tool execution (handled by the agent)
- Session/conversation management (handled by the agent)
- Streaming responses (handled by the agent)

## Features

- **Hardware Isolation**: Each sandbox runs in its own MicroVM via libkrun
- **Instant Boot**: Sub-second VM startup (~200ms cold start)
- **OCI Image Support**: Load sandboxes from standard OCI container images
- **Namespace Isolation**: Agent and business code run in separate Linux namespaces
- **Guest Init**: Custom PID 1 process for VM initialization and process management
- **Cross-Platform**: macOS (Apple Silicon) and Linux (x86_64/ARM64)
- **No Root Required**: Runs without elevated privileges using Apple HVF or KVM

## Quick Start

### Prerequisites

- **macOS ARM64** (Apple Silicon) or **Linux x86_64/ARM64**
- Rust 1.75+

> **Note**: macOS Intel is NOT supported

### Installation

#### macOS (Apple Silicon)

```bash
# Install dependencies
brew install lld llvm

# Clone and build
git clone https://github.com/a3s-lab/box.git
cd box
git submodule update --init --recursive
cd src && cargo build --release
```

#### Linux (Ubuntu/Debian)

```bash
# Install dependencies
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libssl-dev

# Clone and build
git clone https://github.com/a3s-lab/box.git
cd box
git submodule update --init --recursive
cd src && cargo build --release
```

### Build Modes

| Mode | Command | Use Case |
|------|---------|----------|
| **Full Build** | `cargo build` | Development with VM support |
| **Stub Mode** | `A3S_DEPS_STUB=1 cargo build` | CI/testing without VM |

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         Host Process                             │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │                    a3s-box-runtime                         │  │
│  │  ┌─────────────┐ ┌─────────────┐ ┌─────────────────────┐  │  │
│  │  │ VmManager   │ │ OciImage    │ │  RootfsBuilder      │  │  │
│  │  │ (lifecycle) │ │ (parsing)   │ │  (composition)      │  │  │
│  │  └─────────────┘ └─────────────┘ └─────────────────────┘  │  │
│  └───────────────────────────┬───────────────────────────────┘  │
│                              │ vsock                             │
└──────────────────────────────┼──────────────────────────────────┘
                               │
┌──────────────────────────────┼──────────────────────────────────┐
│                              ▼                                   │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │              /sbin/init (guest-init, PID 1)               │  │
│  │  - Mount filesystems (/proc, /sys, /dev, virtio-fs)       │  │
│  │  - Create isolated namespaces                              │  │
│  │  - Spawn processes in isolated environments                │  │
│  └───────────────────────────┬───────────────────────────────┘  │
│                              │                                   │
│  ┌───────────────────────────▼───────────────────────────────┐  │
│  │                 Agent Process (Namespace 1)                │  │
│  │  - Your AI agent runs here                                 │  │
│  │  - Isolated mount, PID, IPC, UTS namespaces                │  │
│  └───────────────────────────┬───────────────────────────────┘  │
│                              │ /usr/bin/nsexec                   │
│  ┌───────────────────────────▼───────────────────────────────┐  │
│  │               Business Code (Namespace 2)                  │  │
│  │  - User application code executed by agent                 │  │
│  │  - Further isolated from agent process                     │  │
│  └───────────────────────────────────────────────────────────┘  │
│                        Guest VM (MicroVM)                        │
└──────────────────────────────────────────────────────────────────┘
```

### Crates

| Crate | Purpose |
|-------|---------|
| `core` | Foundational types: `BoxConfig`, `BoxError`, `BoxEvent`, `ContextProvider` |
| `runtime` | VM lifecycle, OCI image parsing, rootfs composition |
| `guest/init` | Guest init (PID 1) and `nsexec` for namespace isolation |
| `shim` | CRI shim for Kubernetes integration |

### A3S Ecosystem

A3S is a modular ecosystem for building and running secure AI agents. Each component can be used independently or together:

```
┌─────────────────────────────────────────────────────────────┐
│                    A3S Ecosystem                            │
│                                                             │
│  ┌─────────────────────────────────────────────────────┐   │
│  │                   a3s-box                            │   │
│  │            MicroVM Sandbox Runtime                   │   │
│  │         (Hardware Isolation Layer)                   │   │
│  │                                                      │   │
│  │  ┌────────────────────────────────────────────────┐ │   │
│  │  │                a3s-code                         │ │   │
│  │  │            AI Coding Agent                      │ │   │
│  │  │          (Runs inside Box)                      │ │   │
│  │  │                                                 │ │   │
│  │  │  ┌─────────────┐      ┌─────────────────────┐  │ │   │
│  │  │  │  a3s-lane   │      │    a3s-context      │  │ │   │
│  │  │  │  Command    │      │    Hierarchical     │  │ │   │
│  │  │  │  Queue      │      │    Memory/Knowledge │  │ │   │
│  │  │  └─────────────┘      └─────────────────────┘  │ │   │
│  │  └────────────────────────────────────────────────┘ │   │
│  └─────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────┘
```

| Project | Package | Layer | Purpose |
|---------|---------|-------|---------|
| **box** | `a3s-box-*` | Infrastructure | MicroVM sandbox runtime with hardware isolation |
| [code](https://github.com/a3s-lab/code) | `a3s-code` | Application | AI coding agent with tool execution |
| [lane](https://github.com/a3s-lab/lane) | `a3s-lane` | Utility | Priority-based command queue for async task scheduling |
| [context](https://github.com/a3s-lab/context) | `a3s-context` | Utility | Hierarchical context management for AI memory/knowledge |

**Standalone Usage**: Each component works independently:
- Use [lane](https://github.com/a3s-lab/lane) for any priority-based async task scheduling
- Use [context](https://github.com/a3s-lab/context) for any hierarchical data organization with semantic search
- Use [code](https://github.com/a3s-lab/code) as a standalone AI agent (without box isolation)
- Use `box` to sandbox any process (not just AI agents)

## Configuration

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `A3S_DEPS_STUB` | Enable stub mode (skip libkrun) | - |
| `RUST_LOG` | Log level | info |

## Roadmap

### Phase 1: Foundation ✅

- [x] MicroVM runtime with libkrun
- [x] Virtualization support detection (KVM, Apple HVF)
- [x] VmController with subprocess isolation
- [x] RootfsBuilder for minimal guest filesystem
- [x] GuestLayout configuration
- [x] Host-guest communication channel (vsock)

### Phase 2: OCI & Isolation ✅

- [x] OCI image parser (manifest, config, layers)
- [x] Rootfs extraction from OCI images with layer composition
- [x] Guest init (PID 1) for VM initialization
- [x] Namespace isolation (Mount, PID, IPC, UTS)
- [x] Nsexec tool for executing code in isolated namespaces

### Phase 3: Ecosystem Integration 🚧

- [ ] OCI image format definition (Dockerfile for Box images)
- [ ] Agent configuration from OCI labels
- [ ] Pre-built `a3s-code` guest image for AI coding agent
- [ ] Host SDK for spawning and communicating with guest agents
- [ ] Python SDK (`a3s-box-python`) for easy integration

### Phase 4: CRI Runtime Integration 📋

**CRI RuntimeService**
- [ ] CRI gRPC server structure
- [ ] Pod Sandbox lifecycle (create, start, stop, remove)
- [ ] Container lifecycle (create, start, stop, remove)
- [ ] Pod/Container status and listing
- [ ] Exec and attach support

**CRI ImageService**
- [ ] Image management (list, pull, remove)
- [ ] Image cache with LRU eviction
- [ ] Image status and filesystem usage

**Deployment**
- [ ] RuntimeClass configuration
- [ ] DaemonSet deployment manifests
- [ ] kubelet integration
- [ ] Integration tests with crictl

### Phase 5: Production 📋

**Performance**
- [ ] Image caching and preloading
- [ ] VM instance pooling
- [ ] Fast boot optimization
- [ ] Resource usage optimization

**Observability**
- [ ] Prometheus metrics export
- [ ] OpenTelemetry integration

**Security**
- [ ] Resource limits enforcement
- [ ] Network isolation policies
- [ ] Audit logging

### Phase 6: Elastic Scaling 📋

- [ ] Metrics collector (queue depth, latency)
- [ ] Autoscaler with reactive scaling
- [ ] Warm pool management
- [ ] Scale to zero support
- [ ] Kubernetes Operator (BoxAutoscaler CRD)

---

## Development

### Dependencies

#### macOS

| Dependency | Install | Purpose |
|------------|---------|---------|
| `lld` | `brew install lld` | LLVM linker for cross-compiling guest init |
| `llvm` | `brew install llvm` | libclang for bindgen |
| `libkrun` | git submodule | MicroVM hypervisor |
| `libkrunfw` | auto-download | Prebuilt Linux kernel |

#### Linux

| Dependency | Install | Purpose |
|------------|---------|---------|
| `build-essential` | `apt install build-essential` | GCC, make |
| `pkg-config` | `apt install pkg-config` | Library discovery |
| `libssl-dev` | `apt install libssl-dev` | TLS support |

### Commands

```bash
# Build
just build              # Build all
just release            # Release build

# Test
just test               # All tests
just test-core          # Core crate
just test-runtime       # Runtime crate

# Lint
just fmt                # Format code
just lint               # Clippy
just ci                 # Full CI checks
```

### Project Structure

```
box/
├── src/
│   ├── core/           # Config, error types, events, context provider trait
│   ├── runtime/        # VM lifecycle, OCI support
│   ├── shim/           # CRI shim
│   └── guest/
│       └── init/       # Guest init (PID 1) and nsexec
├── docs/               # Documentation
└── CLAUDE.md           # Development guidelines
```

## Documentation

| Document | Description |
|----------|-------------|
| [CRI Implementation Plan](./docs/cri-implementation-plan.md) | Kubernetes CRI integration |
| [Rootfs Explained](./docs/rootfs-explained.md) | Root filesystem in MicroVMs |
| [Hooks Design](./docs/hooks-design.md) | Extensibility hooks |

### Troubleshooting

#### `invalid linker name in argument '-fuse-ld=lld'`

```bash
brew install lld
```

#### `Vendored sources not found`

```bash
git submodule update --init --recursive
```

#### Testing without VM

```bash
A3S_DEPS_STUB=1 cargo check -p a3s-box-runtime
```

## License

MIT

---

<p align="center">
  Built by <a href="https://github.com/a3s-lab">A3S Lab</a>
</p>
