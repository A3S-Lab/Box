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

- **Docker-like CLI**: Familiar `run`, `stop`, `pause`, `unpause`, `ps`, `logs`, `exec`, `top`, `rename`, `images`, `tag`, `cp`, `attest` commands with label support
- **Hardware Isolation**: Each sandbox runs in its own MicroVM via libkrun
- **Instant Boot**: Sub-second VM startup (~200ms cold start)
- **OCI Image Support**: Load sandboxes from standard OCI container images
- **Image Registry**: Pull images from any OCI registry with local LRU cache
- **Image Management**: Inspect metadata, prune unused images, tag aliases, configurable cache size
- **Exec in Running VMs**: Execute commands with env vars, working directory, and user specification support
- **File Copy**: Transfer files and directories between host and running boxes via `cp`
- **Restart Policies**: Automatic restart enforcement (`always`, `on-failure`, `unless-stopped`)
- **Health Checks**: Configurable health check commands with interval, timeout, retries, and start period
- **System Cleanup**: One-command prune of stopped boxes and unused images
- **CRI Runtime**: Kubernetes-compatible CRI RuntimeService and ImageService
- **Warm Pool**: Pre-booted idle MicroVMs for instant allocation
- **Rootfs Caching**: Content-addressable rootfs cache with TTL/size pruning
- **Namespace Isolation**: Agent and business code run in separate Linux namespaces
- **Guest Init**: Custom PID 1 process for VM initialization and process management
- **Cross-Platform**: macOS (Apple Silicon) and Linux (x86_64/ARM64)
- **No Root Required**: Runs without elevated privileges using Apple HVF or KVM
- **TEE Support**: AMD SEV-SNP for hardware-enforced memory encryption
- **Remote Attestation**: SNP attestation report generation, ECDSA-P384 signature verification, certificate chain validation, and configurable policy checks via `a3s-box attest`

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

## CLI Usage

The `a3s-box` CLI provides a Docker-like interface for managing MicroVM sandboxes:

```bash
# Image management
a3s-box pull alpine:latest       # Pull an image from a registry
a3s-box pull -q alpine:latest    # Pull quietly (path only)
a3s-box images                   # List cached images
a3s-box images -q                # List image references only
a3s-box images --format '{{.Repository}}:{{.Tag}}'  # Custom format
a3s-box rmi alpine:latest        # Remove a cached image
a3s-box rmi -f img1 img2 img3   # Force-remove multiple images
a3s-box image-inspect alpine:latest  # Show detailed image metadata as JSON
a3s-box tag alpine:latest myalpine:v1  # Create an image alias
a3s-box image-prune -f           # Remove unused images

# Box lifecycle
a3s-box run -d --name dev --cpus 2 --memory 1g --label env=dev alpine:latest
a3s-box run -d --name web --restart always --health-cmd "curl -f http://localhost/" nginx:latest
a3s-box create --name staging --label env=staging alpine:latest
a3s-box start staging
a3s-box pause dev                # Pause a running box (SIGSTOP)
a3s-box unpause dev              # Resume a paused box (SIGCONT)
a3s-box stop dev staging         # Stop multiple boxes
a3s-box rename dev development   # Rename a box
a3s-box rm -f $(a3s-box ps -aq) # Remove all boxes

# Execute commands
a3s-box exec dev -- ls -la       # Run a command in a box
a3s-box exec -u root -e FOO=bar -w /app dev -- python main.py  # With user, env, workdir
a3s-box top dev                  # Display running processes in a box

# File copy
a3s-box cp dev:/var/log/app.log ./app.log   # Box → host (file)
a3s-box cp ./config.yaml dev:/etc/app/      # Host → box (file)
a3s-box cp dev:/var/log/ ./logs/            # Box → host (directory)
a3s-box cp ./src/ dev:/app/src/             # Host → box (directory)

# Observability
a3s-box ps                       # List running boxes
a3s-box ps -a                    # List all boxes (including stopped)
a3s-box ps -q --filter status=running  # IDs of running boxes
a3s-box ps --filter label=env=dev      # Filter by label
a3s-box logs dev -f              # Follow box console output
a3s-box inspect dev              # Show detailed box info as JSON
a3s-box stats                    # Live resource usage

# Cleanup
a3s-box system-prune -f          # Remove stopped boxes + unused images

# System info
a3s-box version
a3s-box info                     # Virtualization support, cache stats

# TEE attestation
a3s-box attest dev               # Request and verify attestation report
a3s-box attest dev --policy policy.json  # Verify against custom policy
a3s-box attest dev --quiet       # Output true/false only (for scripts)
a3s-box attest dev --raw         # Output raw report without verification
```

### Command Reference

| Command | Description |
|---------|-------------|
| `run <image>` | Pull + create + start a box (`-d` detached, `--rm` auto-remove, `-l` labels, `--restart`, `--health-cmd`) |
| `create <image>` | Create a box without starting (`-l` for labels, `--health-cmd` for health check) |
| `start <box>...` | Start one or more created or stopped boxes |
| `stop <box>...` | Graceful stop one or more boxes (SIGTERM then SIGKILL after `-t` timeout) |
| `pause <box>...` | Pause one or more running boxes (SIGSTOP) |
| `unpause <box>...` | Resume one or more paused boxes (SIGCONT) |
| `restart <box>...` | Restart one or more boxes |
| `kill <box>...` | Force-kill one or more running boxes |
| `rm <box>...` | Remove one or more boxes (`-f` to force-remove running boxes) |
| `rename <box> <name>` | Rename a box |
| `ps` | List boxes (`-a` all, `-q` quiet, `--filter status/label`, `--format`) |
| `logs <box>` | View console logs (`-f` to follow, `--tail N` for last N lines) |
| `exec <box> -- <cmd>` | Execute a command in a running box (`-u` user, `-e` env, `-w` workdir) |
| `top <box>` | Display running processes in a box |
| `inspect <box>` | Show detailed box information as JSON |
| `stats` | Display live resource usage statistics |
| `cp <src> <dst>` | Copy files or directories between host and a running box |
| `images` | List cached OCI images (`-q` for quiet, `--format` for custom output) |
| `pull <image>` | Pull an image from a container registry (`-q` for quiet mode) |
| `rmi <image>...` | Remove one or more cached images (`-f` to ignore not-found errors) |
| `image-inspect <image>` | Show detailed image metadata as JSON (config, layers, labels) |
| `image-prune` | Remove unused images (`-a` for all, `-f` to skip confirmation) |
| `tag <source> <target>` | Create a tag that refers to an existing image |
| `system-prune` | Remove all stopped boxes and unused images (`-a`, `-f`) |
| `version` | Show version |
| `info` | Show system information |
| `attest <box>` | Request and verify a TEE attestation report (`--policy`, `--nonce`, `--raw`, `--quiet`) |
| `update` | Update a3s-box to the latest version |

Boxes can be referenced by name, full ID, or unique ID prefix (Docker-compatible resolution).

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

| Crate | Binary | Purpose |
|-------|--------|---------|
| `cli` | `a3s-box` | Docker-like CLI for managing MicroVM sandboxes (206 tests) |
| `core` | — | Foundational types: `BoxConfig`, `BoxError`, `BoxEvent`, `ExecRequest`, `TeeConfig` (122 tests) |
| `runtime` | — | VM lifecycle, OCI image parsing, rootfs composition, health checking, attestation verification (320 tests) |
| `guest/init` | `a3s-box-guest-init` | Guest init (PID 1), `nsexec` for namespace isolation, exec server (20 tests) |
| `shim` | `a3s-box-shim` | VM subprocess shim (libkrun bridge) |
| `cri` | `a3s-box-cri` | CRI runtime for Kubernetes integration (28 tests) |

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
| `A3S_IMAGE_CACHE_SIZE` | Maximum image cache size (e.g., `500m`, `20g`, `1t`) | `10g` |
| `RUST_LOG` | Log level | info |

### TEE Configuration (AMD SEV-SNP)

Enable hardware-enforced memory encryption for confidential computing:

```rust
use a3s_box_core::config::{BoxConfig, TeeConfig, SevSnpGeneration};

let config = BoxConfig {
    tee: TeeConfig::SevSnp {
        workload_id: "my-secure-agent".to_string(),
        generation: SevSnpGeneration::Milan,  // or Genoa
    },
    ..Default::default()
};
```

**Hardware Requirements for TEE:**
- AMD EPYC CPU (Milan 3rd gen or Genoa 4th gen) with SEV-SNP support
- Linux kernel 5.19+ with SEV-SNP patches
- `/dev/sev` device accessible
- libkrun built with `SEV=1` flag

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

### Phase 3: CLI & Ecosystem Integration ✅

- [x] Docker-like CLI (`a3s-box`) with 29 commands: run, create, start, stop, pause, unpause, restart, rm, kill, rename, ps, stats, logs, exec, top, inspect, cp, images, pull, rmi, image-inspect, image-prune, tag, system-prune, version, info, update, attest
- [x] Box state management with atomic persistence (`~/.a3s/boxes.json`)
- [x] Docker-compatible name/ID/prefix resolution
- [x] PID-based liveness reconciliation for dead box detection
- [x] Auto-generated Docker-style names (adjective_noun)
- [x] OCI image pulling from registries with local LRU cache
- [x] Agent-level code cleanup (removed session/skill/context/proto — Box is VM runtime only)
- [x] Exec command execution in running boxes via dedicated exec server (vsock port 4089) with env vars, working directory, and user specification support
- [x] File and directory copy between host and running boxes via exec channel (recursive tar-based transfer)
- [x] System prune for bulk cleanup of stopped boxes and unused images
- [x] Multi-target support for start, stop, restart, rm, kill commands
- [x] Filtering and formatting for ps and images commands
- [x] Configurable image cache size via `A3S_IMAGE_CACHE_SIZE` environment variable
- [x] Docker CLI alignment Phase 1: pause/unpause, top, rename, label support, exec -u/--user, pull -q/--quiet, cp directories
- [x] Docker CLI alignment Phase 2: restart policy enforcement, health check support (--health-cmd, status tracking)
- [x] `a3s-box build` — Dockerfile-based image building (FROM, RUN, COPY, WORKDIR, ENV, ENTRYPOINT, CMD, EXPOSE, LABEL, USER, ARG)
- [ ] Agent configuration from OCI labels
- [ ] Pre-built `a3s-code` guest image for AI coding agent
- [ ] Host SDK for spawning and communicating with guest agents
- [ ] Python SDK (`a3s-box-python`) for easy integration

### Phase 4: CRI Runtime Integration ✅

**CRI RuntimeService**
- [x] CRI gRPC server on Unix domain socket
- [x] Pod Sandbox lifecycle (create, start, stop, remove)
- [x] Container lifecycle (create, start, stop, remove)
- [x] Pod/Container status and listing with label filtering
- [x] ExecSync via guest exec server (vsock port 4089)
- [x] Config mapper (PodSandboxConfig → BoxConfig)
- [x] Sandbox and container state stores

**CRI ImageService**
- [x] Image pull from OCI registries
- [x] Image list, status, and remove
- [x] Image store with LRU eviction and size limits

**Deployment**
- [x] RuntimeClass configuration (`deploy/kubernetes/runtime-class.yaml`)
- [x] DaemonSet deployment manifests (`deploy/kubernetes/daemonset.yaml`)
- [x] Kustomize base with RBAC, ConfigMap, namespace
- [x] kubelet integration guide (`deploy/kubernetes/README.md`)
- [x] crictl smoke test script (`deploy/scripts/crictl-smoke-test.sh`)
- [x] Example pod specs (alpine, AI agent)

### Phase 5: Production 🚧

**Cold Start Optimization**
- [x] Rootfs caching with SHA256 content-addressable keys and TTL/size pruning
- [x] Layer cache for OCI image layers (deduplication across images)
- [x] VM warm pool (pre-booted idle MicroVMs for instant allocation)
- [x] Pool maintenance with configurable TTL and auto-replenish
- [ ] VM snapshot/restore (save "model loaded" state to SSD, restore < 500ms)
- [ ] Layered model cache (L1: VM memory, L2: host SSD mmap, L3: MinIO object storage)
- [ ] Snapshot TTL management and automatic refresh

**Observability**
- [ ] Prometheus metrics export (VM boot time, memory usage, CPU utilization)
- [ ] OpenTelemetry integration (VM lifecycle spans: create → boot → ready)
- [ ] Cold start latency histograms (p50/p90/p95/p99)
- [ ] Warm pool utilization metrics
- [ ] Structured log aggregation

**Security**
- [ ] Resource limits enforcement (CPU, memory, disk)
- [ ] Network isolation policies
- [ ] Audit logging

### Phase 6: TEE (Trusted Execution Environment) 🚧

**Phase 6.1: Basic TEE Support ✅**
- [x] AMD SEV-SNP hardware detection
- [x] TEE configuration types (`TeeConfig`, `SevSnpGeneration`)
- [x] TEE error types (`TeeConfig`, `TeeNotSupported`, `AttestationError`)
- [x] KrunContext TEE methods (`enable_split_irqchip`, `set_tee_config`)
- [x] TEE config file generation for libkrun
- [x] Shim TEE configuration before VM start

**Phase 6.2: Remote Attestation 🚧**
- [x] Attestation report types and SNP report parsing (`AttestationRequest`, `AttestationReport`, `PlatformInfo`, `TcbVersion`)
- [x] Host-guest attestation client via Unix socket (`AttestationClient`)
- [x] VmManager attestation integration (`request_attestation()`)
- [x] ECDSA-P384 signature verification using VCEK public key
- [x] Certificate chain validation (VCEK → ASK → ARK)
- [x] AMD KDS client for fetching/caching certificates from `kds.amd.com`
- [x] Configurable attestation policy (measurement, TCB version, debug mode, SMT, policy mask)
- [x] `a3s-box attest` CLI command with `--policy`, `--nonce`, `--raw`, `--quiet` options
- [ ] Guest Agent `/attest` endpoint (SNP_GET_REPORT ioctl on `/dev/sev-guest`)
- [ ] KBS (Key Broker Service) integration for secret provisioning
- [ ] Periodic re-attestation with configurable interval

**Phase 6.3: Sealed Storage 📋**
- [ ] MRENCLAVE/MRSIGNER key derivation
- [ ] Version-based rollback protection
- [ ] Secure credential storage API
- [ ] Encrypted persistent storage

### Phase 7: SafeClaw Security Integration 📋

A3S Box provides the secure infrastructure layer for [SafeClaw](../safeclaw/README.md)'s privacy-focused AI assistant.

#### SafeClaw + A3S Box Security Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                    SafeClaw Security Architecture                        │
│                                                                          │
│  User Request (contains sensitive data)                                  │
│      │                                                                   │
│      ▼                                                                   │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │                    SafeClaw Gateway                              │    │
│  │  - Privacy classification                                        │    │
│  │  - Sensitivity routing                                           │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│      │                                                                   │
│      │ vsock (encrypted)                                                │
│      ▼                                                                   │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │              A3S Box - Coordinator TEE                           │    │
│  │  ┌───────────────────────────────────────────────────────────┐  │    │
│  │  │  Local LLM (Qwen3/DeepSeek)                                │  │    │
│  │  │  - Full access to sensitive data                          │  │    │
│  │  │  - Task decomposition & sanitization                      │  │    │
│  │  │  - Data NEVER leaves this TEE                             │  │    │
│  │  └───────────────────────────────────────────────────────────┘  │    │
│  │  ┌───────────────────────────────────────────────────────────┐  │    │
│  │  │  Network Firewall                                          │  │    │
│  │  │  - Whitelist: vsock only (no external network)            │  │    │
│  │  └───────────────────────────────────────────────────────────┘  │    │
│  │                    Hardware Isolated (SEV-SNP/SGX)               │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│      │                         │                         │              │
│      │ sanitized               │ partial                 │ sanitized   │
│      ▼                         ▼                         ▼              │
│  ┌──────────────┐      ┌──────────────┐      ┌──────────────┐          │
│  │ A3S Box      │      │ A3S Box      │      │ A3S Box      │          │
│  │ Worker TEE   │      │ Worker TEE   │      │ Worker REE   │          │
│  │              │      │              │      │              │          │
│  │ Secure tasks │      │ Secure tasks │      │ General tasks│          │
│  │ (partial     │      │ (partial     │      │ (no sensitive│          │
│  │  sensitive)  │      │  sensitive)  │      │  data)       │          │
│  └──────┬───────┘      └──────┬───────┘      └──────┬───────┘          │
│         │                     │                     │                   │
│         └─────────────────────┴─────────────────────┘                   │
│                               │                                         │
│                               ▼                                         │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │              A3S Box - Validator TEE                             │    │
│  │  ┌───────────────────────────────────────────────────────────┐  │    │
│  │  │  Local LLM (Independent verification)                      │  │    │
│  │  │  - Check output for data leakage                          │  │    │
│  │  │  - Can BLOCK suspicious responses                         │  │    │
│  │  └───────────────────────────────────────────────────────────┘  │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│      │                                                                   │
│      ▼                                                                   │
│  Safe Response (sensitive data redacted)                                │
└─────────────────────────────────────────────────────────────────────────┘
```

#### Data Security Model

```
┌─────────────────────────────────────────────────────────────────────────┐
│                    Data Security Boundaries                              │
│                                                                          │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │  TRUST ZONE 1: Coordinator TEE (Highest Security)                │    │
│  │  ┌───────────────────────────────────────────────────────────┐  │    │
│  │  │  ✓ Full sensitive data access                              │  │    │
│  │  │  ✓ Local LLM only (no cloud API)                          │  │    │
│  │  │  ✓ Sealed storage for credentials                         │  │    │
│  │  │  ✓ No outbound network                                    │  │    │
│  │  │                                                            │  │    │
│  │  │  Data: passwords, API keys, SSN, credit cards, medical    │  │    │
│  │  └───────────────────────────────────────────────────────────┘  │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│                                                                          │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │  TRUST ZONE 2: Worker TEE (Medium Security)                      │    │
│  │  ┌───────────────────────────────────────────────────────────┐  │    │
│  │  │  ✓ Partial sensitive data (need-to-know)                   │  │    │
│  │  │  ✓ Cloud LLM API allowed (whitelisted)                    │  │    │
│  │  │  ✓ Output sanitization enforced                           │  │    │
│  │  │  ✓ Tool call interception                                 │  │    │
│  │  │                                                            │  │    │
│  │  │  Data: anonymized records, partial identifiers            │  │    │
│  │  └───────────────────────────────────────────────────────────┘  │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│                                                                          │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │  TRUST ZONE 3: Worker REE (Standard Security)                    │    │
│  │  ┌───────────────────────────────────────────────────────────┐  │    │
│  │  │  ✗ No sensitive data access                                │  │    │
│  │  │  ✓ Cloud LLM API allowed                                  │  │    │
│  │  │  ✓ General purpose tasks only                             │  │    │
│  │  │                                                            │  │    │
│  │  │  Data: public info, formatting, translation               │  │    │
│  │  └───────────────────────────────────────────────────────────┘  │    │
│  └─────────────────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────────────────┘
```

#### TEE Security Properties

| Property | Implementation | Threat Mitigated |
|----------|----------------|------------------|
| **Memory Encryption** | AMD SEV-SNP / Intel SGX | Memory scraping, cold boot attacks |
| **Remote Attestation** | Quote verification | Fake TEE, tampered code |
| **Sealed Storage** | MRENCLAVE binding | Data extraction, rollback |
| **Network Isolation** | Whitelist firewall | Data exfiltration |
| **Process Isolation** | Namespace + MicroVM | Container escape |

**Local LLM Support (for SafeClaw Coordinator)**
- [ ] TEE-optimized LLM inference runtime
- [ ] Support for Qwen3, DeepSeek-R1, ChatGLM models
- [ ] Quantization support (Q4, Q8) for memory efficiency
- [ ] Model integrity verification (hash check before loading)
- [ ] GPU passthrough for TEE (where supported)

**Distributed TEE Architecture**
- [ ] Multi-TEE instance orchestration
- [ ] Inter-TEE secure communication channels
- [ ] Cross-TEE attestation verification
- [ ] Worker pool management (TEE/REE environments)
- [ ] Task routing based on sensitivity level

**Network Security**
- [ ] Whitelist-only outbound firewall
- [ ] DNS query restrictions
- [ ] Traffic audit logging
- [ ] Rate limiting per destination

**Secure Channel Enhancement**
- [ ] HKDF key derivation (replace SHA256)
- [ ] Message sequence numbers (replay protection)
- [ ] Automatic key rotation
- [ ] Forward secrecy verification

### Phase 8: Elastic Scaling 📋

- [ ] Metrics collector (queue depth, latency, cold start frequency)
- [ ] Autoscaler with reactive scaling
- [ ] Warm pool management (auto-replenish on allocation)
- [ ] Scale to zero support (with snapshot persistence)
- [ ] Kubernetes Operator (BoxAutoscaler CRD)
- [ ] Integration with Knative cold_start_strategy config

### Phase 9: Docker Feature Parity 📋

Remaining gaps between A3S Box and Docker, prioritized by impact.

**9.1 Networking (P0)**
- [ ] Bridge network driver (container-to-container communication)
- [ ] Custom networks (`a3s-box network create/ls/rm/inspect/connect/disconnect`)
- [ ] DNS service discovery (resolve containers by name within a network)
- [ ] Network isolation policies (inter-network firewall rules)
- [ ] IPv6 support

**9.2 Volume Management (P0)**
- [ ] Named volumes (`a3s-box volume create/ls/rm/inspect/prune`)
- [ ] Anonymous volumes (`VOLUME` Dockerfile instruction)
- [ ] tmpfs mounts (`--tmpfs`)
- [ ] Bind mount propagation modes (shared, slave, private)
- [ ] Volume labels and filtering

**9.3 Registry Push (P1)**
- [ ] `a3s-box push` — push images to OCI registries
- [ ] Registry login/logout (`a3s-box login/logout`)
- [ ] Image signing and verification (cosign/notation)

**9.4 Resource Limits (P1)**
- [ ] CPU shares (`--cpu-shares`) and quota (`--cpu-quota`/`--cpu-period`)
- [ ] CPU pinning (`--cpuset-cpus`)
- [ ] Memory reservation (`--memory-reservation`)
- [ ] Memory swap limit (`--memory-swap`)
- [ ] PID limits (`--pids-limit`)
- [ ] Block I/O limits (`--blkio-weight`, `--device-read-bps`)
- [ ] Ulimits (`--ulimit`)

**9.5 Dockerfile Completion (P2)**
- [ ] `ADD` instruction (URL download, auto-extract tar)
- [ ] `HEALTHCHECK` instruction
- [ ] `SHELL` instruction
- [ ] `STOPSIGNAL` instruction
- [ ] `VOLUME` instruction (anonymous volumes)
- [ ] `ONBUILD` instruction (triggers)
- [ ] Multi-stage builds (`FROM ... AS ...`)

**9.6 Logging (P2)**
- [ ] Logging drivers (json-file, syslog, journald)
- [ ] Log rotation (`--log-opt max-size`, `--log-opt max-file`)
- [ ] Structured JSON log output

**9.7 Security Hardening (P2)**
- [ ] Seccomp profiles (`--security-opt seccomp=...`)
- [ ] Linux capabilities management (`--cap-add`, `--cap-drop`)
- [ ] Read-only rootfs (`--read-only`)
- [ ] No-new-privileges (`--security-opt no-new-privileges`)

**9.8 Advanced Features (P3)**
- [ ] Multi-container orchestration (compose-like YAML)
- [ ] Image export/import (`save`/`load` — partially done)
- [ ] Buildx multi-platform builds
- [ ] Secrets management (`--secret`)
- [ ] CRI streaming API (Exec, Attach, PortForward)
- [ ] Container events API

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
cargo build -p a3s-box-cli  # Build CLI only

# Test
just test               # All tests
just test-core          # Core crate
just test-runtime       # Runtime crate
cargo test -p a3s-box-cli   # CLI tests (183 tests)
cargo test -p a3s-box-core  # Core tests (95 tests)

# Lint
just fmt                # Format code
just lint               # Clippy
just ci                 # Full CI checks
```

### Project Structure

```
box/
├── src/
│   ├── cli/            # Docker-like CLI (a3s-box binary)
│   │   └── src/
│   │       ├── commands/   # 29 subcommands (run, create, start, stop, pause, unpause, restart, rm, kill, rename, ps, stats, logs, exec, top, inspect, cp, images, pull, rmi, image-inspect, image-prune, tag, system-prune, version, info, update, attest)
│   │       ├── state.rs    # Box state persistence (~/.a3s/boxes.json)
│   │       ├── resolve.rs  # Docker-style name/ID resolution
│   │       └── output.rs   # Table formatting, size parsing, memory parsing
│   ├── core/           # Config, error types, events
│   ├── runtime/        # VM lifecycle, OCI support, health checking
│   ├── shim/           # VM subprocess shim (libkrun bridge)
│   ├── cri/            # CRI runtime for Kubernetes
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
