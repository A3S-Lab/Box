# A3S Box

<p align="center">
  <strong>A3S Operating System — MicroVM Sandbox Runtime</strong>
</p>

<p align="center">
  <em>Hardware-isolated execution environment — SafeClaw (the OS main application) runs inside a3s-box MicroVMs</em>
</p>

<p align="center">
  <a href="#features">Features</a> •
  <a href="#quick-start">Quick Start</a> •
  <a href="#architecture">Architecture</a> •
  <a href="#roadmap">Roadmap</a>
</p>

---

## Overview

**A3S Box** is a MicroVM-based sandbox runtime that provides hardware-isolated execution environments for the A3S Agent OS. In the A3S architecture, SafeClaw (the OS main application) runs inside a3s-box MicroVMs, providing hardware-level security boundaries for all agent operations. Box handles VM lifecycle, OCI image management, WarmPool pre-warming, and namespace isolation.

Box is **not** an AI agent itself. It provides the secure sandbox infrastructure that SafeClaw and agents run inside.

### What Box Does

- **VM Isolation**: Each sandbox runs in its own MicroVM with a dedicated Linux kernel (~200ms cold start)
- **OCI Images**: Load agent code and dependencies from standard container images
- **WarmPool**: Pre-warmed VM pool for instant deployment (`min_idle` / `max_size` / `idle_ttl`)
- **Namespace Isolation**: Further isolate agent code from business code within the VM
- **CRI Integration**: Run as a Kubernetes container runtime
- **TEE Support**: AMD SEV-SNP hardware memory encryption + remote attestation

### What Box Does NOT Do

- LLM integration (handled by the agent running inside Box)
- Tool execution (handled by the agent)
- Session/conversation management (handled by the agent)
- Streaming responses (handled by the agent)

## Features

- **Docker-like CLI**: Familiar `run`, `stop`, `pause`, `unpause`, `ps`, `logs`, `exec`, `top`, `rename`, `images`, `tag`, `cp`, `attest`, `network`, `volume`, `push`, `login`, `logout`, `commit`, `diff`, `events`, `container-update` commands with label support
- **Hardware Isolation**: Each sandbox runs in its own MicroVM via libkrun
- **Instant Boot**: Sub-second VM startup (~200ms cold start)
- **OCI Image Support**: Load sandboxes from standard OCI container images
- **Image Registry**: Pull and push images from/to any OCI registry with local LRU cache
- **Registry Auth**: `login`/`logout` with persistent credential store, env var fallback
- **Image Management**: Inspect metadata, prune unused images, tag aliases, configurable cache size
- **Exec in Running VMs**: Execute commands with env vars, working directory, and user specification support
- **Interactive PTY**: Full interactive terminal support (`-it` flags) for `exec`, `attach`, and `run` via dedicated PTY server (vsock port 4090)
- **File Copy**: Transfer files and directories between host and running boxes via `cp`
- **Bridge Networking**: Container-to-container communication via passt virtio-net with IPAM, custom networks, DNS service discovery, and `--network` flag
- **Named Volumes**: Persistent named volumes with `volume create/ls/rm/inspect/prune`, auto-creation on first use, attach/detach tracking, and volume labels
- **tmpfs Mounts**: Ephemeral in-guest memory-backed filesystems via `--tmpfs`
- **Anonymous Volumes**: Automatic volume creation for OCI `VOLUME` directives with cleanup on `rm`
- **Restart Policies**: Automatic restart enforcement (`always`, `on-failure`, `on-failure:N`, `unless-stopped`) with background monitor daemon, exponential backoff, and crash loop prevention
- **Health Checks**: Configurable health check commands with interval, timeout, retries, and start period
- **Resource Limits**: PID limits (`--pids-limit`), CPU pinning (`--cpuset-cpus`), custom ulimits (`--ulimit`), CPU shares/quota, memory reservation/swap via cgroup v2 (Linux)
- **Security Options**: Capabilities management (`--cap-add`, `--cap-drop`), read-only rootfs (`--read-only`), privileged mode (`--privileged`), security options (`--security-opt`), device mapping (`--device`), GPU access (`--gpus`)
- **Logging Drivers**: Structured JSON logging (`--log-driver json-file`) with rotation (`--log-opt max-size`, `--log-opt max-file`), or disable logging (`--log-driver none`)
- **Container Commit**: Create new OCI images from box filesystem changes (`commit` with `--change`, `--message`, `--author`)
- **Filesystem Diff**: Show added/changed/deleted files vs baseline (`diff`)
- **System Events**: Real-time event streaming with filtering (`events` with `--filter`, `--json`)
- **Live Resource Update**: Hot-update CPU, memory, restart policy without restart (`container-update`)
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
- **RA-TLS**: Remote attestation over TLS — SNP report embedded in X.509 certificate extensions, verified during TLS handshake
- **Secret Injection**: Inject secrets into TEE via RA-TLS channel, stored in `/run/secrets/` (tmpfs, mode 0400)
- **Sealed Storage**: AES-256-GCM encryption with HKDF-SHA256 keys derived from TEE identity, three sealing policies (MeasurementAndChip, MeasurementOnly, ChipOnly)
- **Dockerfile Build**: Full `a3s-box build` with FROM, RUN, COPY, ADD, WORKDIR, ENV, ENTRYPOINT, CMD, EXPOSE, LABEL, USER, ARG, SHELL, STOPSIGNAL, HEALTHCHECK, ONBUILD, VOLUME, and multi-stage builds
- **Image Export/Import**: `save`/`load` for offline image transfer

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
a3s-box run -d --name worker --restart on-failure:5 myapp:latest  # Restart up to 5 times on failure
a3s-box monitor &                # Start restart monitor daemon (required for auto-restart)
a3s-box create --name staging --label env=staging alpine:latest
a3s-box start staging
a3s-box pause dev                # Pause a running box (SIGSTOP)
a3s-box unpause dev              # Resume a paused box (SIGCONT)
a3s-box stop dev staging         # Stop multiple boxes
a3s-box rename dev development   # Rename a box
a3s-box rm -f $(a3s-box ps -aq) # Remove all boxes

# Execute commands
a3s-box exec dev -- ls -la       # Run a command in a box
a3s-box exec -it dev -- /bin/sh  # Interactive shell session
a3s-box exec -u root -e FOO=bar -w /app dev -- python main.py  # With user, env, workdir
a3s-box attach -it dev           # Attach interactive PTY to a running box
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
a3s-box diff dev                 # Show filesystem changes (A/C/D)
a3s-box events                   # Stream real-time system events
a3s-box events --filter event=start --json  # Filtered JSON events

# Image creation
a3s-box commit dev myimage:v1    # Create image from box changes
a3s-box commit -m "added config" -c "CMD /start" dev myimage:v2

# Live updates
a3s-box container-update dev --cpus 4 --memory 2g  # Update resource limits
a3s-box container-update dev --restart always       # Change restart policy

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
| `run <image>` | Pull + create + start a box (`-d` detached, `--rm` auto-remove, `-l` labels, `--restart`, `--health-cmd`, `--cap-add/drop`, `--privileged`, `--read-only`, `--device`, `--gpus`, `--init`, `--env-file`, `--add-host`, `--platform`) |
| `create <image>` | Create a box without starting (same flags as `run`) |
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
| `exec <box> -- <cmd>` | Execute a command in a running box (`-u` user, `-e` env, `-w` workdir, `-it` interactive PTY) |
| `top <box>` | Display running processes in a box |
| `inspect <box>` | Show detailed box information as JSON |
| `stats` | Display live resource usage statistics |
| `cp <src> <dst>` | Copy files or directories between host and a running box |
| `commit <box> [repo:tag]` | Create an image from a box's changes (`-m` message, `-a` author, `-c` change) |
| `diff <box>` | Show filesystem changes in a box (Added/Changed/Deleted) |
| `events` | Stream real-time system events (`--filter`, `--json`, `--since`, `--until`) |
| `container-update <box>` | Update resource limits of a running box (`--cpus`, `--memory`, `--restart`, `--pids-limit`) |
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
| `monitor` | Background daemon that monitors and restarts dead boxes according to their restart policy |
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
| `cli` | `a3s-box` | Docker-like CLI for managing MicroVM sandboxes (367 unit tests, 7 integration tests) |
| `core` | — | Foundational types: `BoxConfig`, `BoxError`, `BoxEvent`, `ExecRequest`, `PtyRequest`, `TeeConfig` (160 tests) |
| `runtime` | — | VM lifecycle, OCI image parsing, rootfs composition, health checking, attestation verification (486 tests) |
| `guest/init` | `a3s-box-guest-init` | Guest init (PID 1), `nsexec` for namespace isolation, exec server, PTY server, attestation server (Linux-only tests) |
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
- AMD EPYC CPU with SEV-SNP support:
  - EPYC 7003 series (Milan, 3rd gen) — e.g. EPYC 7763, 7713, 7543, 7443, 7313
  - EPYC 9004 series (Genoa, 4th gen) — e.g. EPYC 9654, 9554, 9454, 9354, 9124
- Linux kernel 5.19+ with SEV-SNP patches
- `/dev/sev` and `/dev/sev-guest` devices accessible
- libkrun built with `SEV=1` flag
- Cloud options: Azure DCasv5/ECasv5 VMs, AWS M6a (SEV only, no SNP), Google Cloud N2D (SEV only)

> **Note:** AMD Ryzen (including Ryzen 9 HX series), Intel CPUs, and Apple Silicon do NOT support SEV-SNP. VMware/VirtualBox nested virtualization also cannot expose SEV-SNP to guests.

**TEE Simulation Mode:**

For local development without SEV-SNP hardware, enable simulation mode:

```bash
export A3S_TEE_SIMULATE=1
a3s-box run --tee sev-snp my-image
```

Simulation mode generates fake attestation reports with deterministic keys, allowing the full TEE workflow (attestation, RA-TLS, secret injection, seal/unseal) to run on any machine.

**Simulation Limitations (not suitable for production):**
- Attestation report signatures are not cryptographically valid (ECDSA verification is bypassed)
- RA-TLS TLS public key is not bound to `report_data` — a real attacker could substitute certificates
- Certificate chain validation checks issuer/subject strings only, not ECDSA signatures (VCEK→ASK→ARK)
- Sealed data is NOT portable to real hardware — key derivation uses different inputs (simulated vs real measurement/chip_id), so data sealed in simulation cannot be unsealed on real SEV-SNP and vice versa
- No replay attack protection (report age is not checked)
- No hardware memory encryption — guest memory is readable by the host

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

- [x] Docker-like CLI (`a3s-box`) with 47 commands: run, create, start, stop, pause, unpause, restart, rm, kill, rename, ps, stats, logs, exec, top, inspect, cp, images, pull, push, rmi, image-inspect, image-prune, tag, save, load, export, build, commit, diff, events, container-update, network, volume, df, system-prune, login, logout, history, port, wait, attach, version, info, update, attest, monitor
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
- [x] Docker CLI alignment Phase 2: restart policy enforcement with monitor daemon (`always`, `on-failure`, `on-failure:N`, `unless-stopped`, exponential backoff), health check support (--health-cmd, status tracking)
- [x] Docker CLI alignment Phase 3: interactive PTY support (`-it` flags) for exec, attach, and run via dedicated PTY server (vsock port 4090)
- [x] `a3s-box build` — Dockerfile-based image building (FROM, RUN, COPY, WORKDIR, ENV, ENTRYPOINT, CMD, EXPOSE, LABEL, USER, ARG, ADD, SHELL, STOPSIGNAL, HEALTHCHECK, ONBUILD, VOLUME, multi-stage builds)

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
- [ ] VM snapshot/restore (save running state to SSD, restore < 500ms)

**Logging & Security**
- [x] Structured log aggregation (json-file logging driver with rotation)
- [x] Resource limits enforcement (CPU, memory, disk via cgroup v2)
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

**Phase 6.2: Remote Attestation ✅**
- [x] Attestation report types and SNP report parsing (`AttestationRequest`, `AttestationReport`, `PlatformInfo`, `TcbVersion`)
- [x] Host-guest attestation client via Unix socket (`AttestationClient`)
- [x] VmManager attestation integration (`request_attestation()` via `TeeExtension` trait)
- [x] ECDSA-P384 signature verification using VCEK public key
- [x] Certificate chain validation (VCEK → ASK → ARK)
- [x] AMD KDS client for fetching/caching certificates from `kds.amd.com`
- [x] Configurable attestation policy (measurement, TCB version, debug mode, SMT, policy mask)
- [x] `a3s-box attest` CLI command with `--policy`, `--nonce`, `--raw`, `--quiet` options
- [x] Guest attestation server with SNP_GET_REPORT ioctl on `/dev/sev-guest` (vsock port 4091)
- [x] TEE simulation mode for local development (`A3S_TEE_SIMULATE=1`)
- [x] RA-TLS module: SNP report embedded in X.509 certificate extensions (P-384 ECDSA)
- [x] RA-TLS end-to-end: guest TLS server + host `RaTlsAttestationClient` with `--ratls` CLI flag
- [x] Secret injection via RA-TLS channel (`SecretInjector`, `/run/secrets/`, env var support)
- [x] `VmmProvider` trait for pluggable VMM backends (libkrun shim default, extensible to QEMU/Firecracker)
- [x] `TeeExtension` trait extracting TEE operations from VmManager into pluggable `SnpTeeExtension`
- [ ] KBS (Key Broker Service) integration for secret provisioning
- [ ] Periodic re-attestation with configurable interval

**Phase 6.3: Sealed Storage ✅**
- [x] HKDF-SHA256 key derivation from TEE identity (measurement + chip_id)
- [x] AES-256-GCM sealed storage with three policies (MeasurementAndChip, MeasurementOnly, ChipOnly)
- [x] Guest-side seal/unseal service via RA-TLS (`POST /seal`, `POST /unseal`)
- [x] Host-side `SealClient` + `VmManager.seal_data()`/`unseal_data()` methods
- [ ] Version-based rollback protection
- [ ] Encrypted persistent storage

**Phase 6.4: TEE Hardening 📋**
- [ ] RA-TLS: bind TLS public key hash to `report_data` field (currently report_data is nonce-only, TLS key not cryptographically bound to attestation)
- [ ] Certificate chain cryptographic signature verification (currently validates issuer/subject strings but does not verify ECDSA signatures: VCEK→ASK→ARK)
- [ ] Attestation report age checking for replay attack prevention (reject stale reports beyond configurable TTL)
- [ ] Hardware detection improvements: CPUID leaf 0x8000001F bit checks, firmware version validation, Genoa product name detection
- [ ] Document sealed data non-portability between simulation and real hardware (different key derivation inputs produce different keys)
- [ ] Real hardware integration testing on AMD SEV-SNP platform (Azure DCasv5 or bare-metal EPYC Milan/Genoa)
- [ ] Simulation-to-production migration guide with sealed data re-encryption tooling

### Phase 7: Host SDK & Transport 📋

Box exposes a host-side SDK and unified transport layer for upstream consumers (SafeClaw, a3s-code, etc.).

**Shared Transport Layer (`a3s-transport`)**
- [ ] `Transport` trait with `connect`/`send`/`recv`/`close`
- [ ] Unified frame protocol: `[type:u8][length:u32][payload]` — shared across exec, PTY, and TEE channels
- [ ] `VsockTransport`: Real vsock implementation with framing, backpressure, reconnection
- [ ] `MockTransport`: For testing without VM
- [ ] Migrate exec server to shared framing protocol
- [ ] Migrate PTY server to shared framing protocol

**`TeeRuntime` High-Level API**
- [ ] `TeeRuntime` struct: orchestrates VM boot + attestation + secure channel
- [ ] `spawn_verified()`: single call to get a verified TEE channel
- [ ] `VerifiedTeeChannel`: wraps `Transport` + attestation proof
- [ ] Lifecycle management: shutdown, re-attestation, channel recovery

> Items moved to other projects:
> - SafeClaw: distributed TEE orchestration, trust zones, LLM inference runtime, task routing, network firewall policies, data security model → see [SafeClaw README](../safeclaw/README.md)
> - a3s-code: agent configuration from OCI labels, pre-built guest image, host SDK for agent communication, Python SDK → see [a3s-code README](https://github.com/a3s-lab/code)

### Phase 8: Elastic Scaling 📋

- [ ] Prometheus metrics export (VM boot time, memory usage, CPU utilization, warm pool stats)
- [ ] OpenTelemetry integration (VM lifecycle spans: create → boot → ready)
- [ ] Autoscaler with reactive scaling based on warm pool pressure
- [ ] Scale to zero support (with snapshot persistence)
- [ ] Kubernetes Operator (BoxAutoscaler CRD)

> Items moved to other projects:
> - SafeClaw: queue depth metrics, task-level latency tracking, Knative cold_start_strategy → see [SafeClaw README](../safeclaw/README.md)

### Phase 9: Docker Feature Parity ✅

Near-complete Docker CLI compatibility. Remaining items are low-priority edge cases.

**9.1 Networking (P0) ✅**
- [x] Bridge network driver (container-to-container communication via passt virtio-net)
- [x] Custom networks (`a3s-box network create/ls/rm/inspect/connect/disconnect`)
- [x] `--network` flag on `run`/`create` commands
- [x] Passt process lifecycle management (spawn/stop/cleanup)
- [x] Network endpoint tracking with IPAM (IP allocation, MAC generation)
- [x] Automatic network cleanup on stop/rm/kill
- [x] DNS service discovery (resolve boxes by name via `/etc/hosts` generation)
- [ ] Network isolation policies (inter-network firewall rules) — deferred to P1
- [ ] IPv6 support — deferred to P1

**9.2 Volume Management (P0) ✅**
- [x] Named volumes (`a3s-box volume create/ls/rm/inspect/prune`) with auto-creation on first use
- [x] Named volume resolution in `-v` flag (e.g., `mydata:/app/data`)
- [x] Volume attach/detach tracking across box lifecycle (run/stop/rm/kill)
- [x] Volume labels (`--label key=value`)
- [x] Anonymous volumes (OCI `VOLUME` directive auto-creates volumes, cleaned up on `rm`)
- [x] tmpfs mounts (`--tmpfs /path` or `--tmpfs /path:size=100m`)
- ~~Bind mount propagation modes~~ — not meaningful for VMs with virtiofs

**9.3 Registry Push (P1) ✅**
- [x] `a3s-box push` — push images to OCI registries
- [x] Registry login/logout (`a3s-box login/logout`) with persistent credential store
- [ ] Image signing and verification (cosign/notation) — deferred

**9.4 Resource Limits (P1) ✅**
- [x] CPU shares (`--cpu-shares`) and quota (`--cpu-quota`/`--cpu-period`) — cgroup v2 `cpu.weight`/`cpu.max` (Linux)
- [x] CPU pinning (`--cpuset-cpus`) — `sched_setaffinity()` on shim process (Linux)
- [x] Memory reservation (`--memory-reservation`) — cgroup v2 `memory.low` (Linux)
- [x] Memory swap limit (`--memory-swap`) — cgroup v2 `memory.swap.max` (Linux)
- [x] PID limits (`--pids-limit`) — guest RLIMIT_NPROC via `krun_set_rlimits`
- [x] Ulimits (`--ulimit`) — custom guest rlimits via `krun_set_rlimits`
- ~~Block I/O limits (`--blkio-weight`, `--device-read-bps`)~~ — not meaningful for VMs with virtio

**9.5 Dockerfile Completion (P2)** ✅
- [x] `ADD` instruction (URL download, auto-extract tar)
- [x] `HEALTHCHECK` instruction (parse options, emit in OCI config)
- [x] `SHELL` instruction (override default shell for RUN)
- [x] `STOPSIGNAL` instruction (set stop signal in OCI config)
- [x] `VOLUME` instruction (anonymous volumes — implemented in Phase 9.2)
- [x] `ONBUILD` instruction (triggers stored in image, executed on FROM)
- [x] Multi-stage builds (`FROM ... AS ...`, `COPY --from=stage`)

**9.5.1 Interactive PTY (P1) ✅**
- [x] PTY protocol over vsock port 4090 (binary framing: request, data, resize, exit, error)
- [x] Guest PTY server with `openpty()` + fork/exec, terminal resize, signal handling
- [x] `exec -it` — interactive shell sessions in running boxes
- [x] `attach -it` — attach interactive PTY to running boxes
- [x] `run -it` — start box with interactive terminal session
- [x] Host terminal raw mode via crossterm (cross-platform)

**9.6 Logging (P2) ✅**
- [x] Logging drivers (`--log-driver json-file` with structured JSON output, `--log-driver none` to disable)
- [x] Log rotation (`--log-opt max-size`, `--log-opt max-file`)
- [x] Structured JSON log output (`container.json` with timestamps, stream tags)

**9.7 Security Hardening (P2) ✅**
- [x] Linux capabilities management (`--cap-add`, `--cap-drop`)
- [x] Read-only rootfs (`--read-only`)
- [x] Privileged mode (`--privileged`)
- [x] Security options (`--security-opt`)
- [x] Device mapping (`--device`)
- [x] GPU access (`--gpus`)
- [x] Init process (`--init`)
- [x] OOM controls (`--oom-kill-disable`, `--oom-score-adj`)
- [x] Stop signal/timeout (`--stop-signal`, `--stop-timeout`)
- [ ] Seccomp profiles (`--security-opt seccomp=...`) — runtime enforcement deferred
- [ ] No-new-privileges (`--security-opt no-new-privileges`) — runtime enforcement deferred

**9.8 Missing Commands (P2) ✅**
- [x] `commit` — Create OCI image from box filesystem changes (`--change`, `--message`, `--author`)
- [x] `diff` — Show filesystem changes (Added/Changed/Deleted) vs baseline snapshot
- [x] `events` — Real-time system event streaming (`--filter`, `--json`, `--since`, `--until`)
- [x] `container-update` — Hot-update resource limits (CPU, memory, restart policy, pids-limit)

**9.9 Additional Flags (P2) ✅**
- [x] `--env-file` — Load environment variables from file
- [x] `--add-host` — Add custom host-to-IP mappings
- [x] `--platform` — Set target platform for pull/run/create
- [x] `--shm-size` — Configure shared memory size
- [x] `--no-healthcheck` — Disable image healthcheck

**9.10 Advanced Features (P3)**
- [ ] Multi-container orchestration (compose-like YAML)
- [x] Image export/import (`save`/`load`)
- [ ] Buildx multi-platform builds
- [ ] Secrets management (`--secret`)
- [ ] CRI streaming API (Exec, Attach, PortForward)
- [x] Container events API (`a3s-box events`)

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
just test               # All unit tests
just test-core          # Core crate
just test-runtime       # Runtime crate
cargo test -p a3s-box-cli --lib   # CLI unit tests (367 tests)
cargo test -p a3s-box-core        # Core tests (160 tests)
cargo test -p a3s-box-runtime     # Runtime tests (486 tests)

# Lint
just fmt                # Format code
just lint               # Clippy
just ci                 # Full CI checks
```

### Test Results

**Unit Tests: 1,013 passed** (as of 2025-02-14)

| Crate | Tests | Notes |
|-------|------:|-------|
| `a3s-box-cli` | 367 | State management, name resolution, output formatting, restart policies |
| `a3s-box-core` | 160 | Config validation, error types, event serialization |
| `a3s-box-runtime` | 486 | OCI parsing, rootfs composition, health checking, attestation, RA-TLS, sealed storage |
| `a3s-box-cri` | 28 | CRI sandbox/container lifecycle, config mapping |
| `a3s-box-guest-init` | — | Tests compile only on `aarch64-unknown-linux-musl` target |
| **Total** | **1,041** | 2 doc-tests ignored (require Linux runtime) |

All unit tests run without VM, network, or hardware dependencies (`A3S_DEPS_STUB=1` for CI).

**Integration Tests: 7 tests** (all `#[ignore]`, require real VM)

| Test Suite | Tests | What It Covers |
|------------|------:|----------------|
| `nginx_integration` | 4 | VM lifecycle, image pull, exec, env vars, labels, nginx |
| `tee_integration` | 3 | TEE attestation, seal/unseal, secret injection, sealing policies |

### Integration Test Details

#### nginx_integration (VM Lifecycle)

Tests the full MicroVM lifecycle using Alpine Linux and nginx images:

| Test | Flow |
|------|------|
| `test_alpine_full_lifecycle` | pull → run → ps → inspect → exec (uname, cat, ls) → logs → stop → rm |
| `test_exec_commands` | run → exec (cat, ls, env, write+read file) → cleanup |
| `test_env_and_labels` | run with `-e`/`-l` → verify env vars inside guest → cleanup |
| `test_nginx_image_pull_and_run` | pull nginx:alpine → run with port mapping → check HTTP → cleanup |

#### tee_integration (TEE Workflow)

Tests the full Trusted Execution Environment workflow using simulated AMD SEV-SNP:

| Test | Flow |
|------|------|
| `test_tee_seal_unseal_lifecycle` | run `--tee-simulate` → attest (RA-TLS) → seal data → unseal data → verify wrong context fails → cleanup |
| `test_tee_secret_injection` | run `--tee-simulate` → inject-secret (2 secrets via RA-TLS) → exec cat `/run/secrets/*` → verify values → cleanup |
| `test_tee_seal_policies` | run `--tee-simulate` → seal/unseal roundtrip for each policy (measurement-and-chip, measurement-only, chip-only) → cleanup |

**TEE integration test architecture:**

```
Host                                          Guest VM (MicroVM)
┌──────────────────────┐                     ┌──────────────────────────┐
│  cargo test          │                     │  /sbin/init (PID 1)      │
│  └─ a3s-box attest ──┼── RA-TLS (4091) ──►│  └─ attest_server        │
│  └─ a3s-box seal   ──┼── RA-TLS (4091) ──►│     (SNP report in X.509)│
│  └─ a3s-box unseal ──┼── RA-TLS (4091) ──►│                          │
│  └─ a3s-box inject ──┼── RA-TLS (4091) ──►│  └─ /run/secrets/*       │
│  └─ a3s-box exec   ──┼── vsock  (4089) ──►│  └─ exec_server          │
└──────────────────────┘                     └──────────────────────────┘
```

### Running Integration Tests

**Prerequisites:**
- `a3s-box` binary built (`cargo build -p a3s-box-cli`)
- macOS with Apple HVF or Linux with KVM
- Internet access (to pull OCI images from Docker Hub on first run)
- `DYLD_LIBRARY_PATH` set (macOS only)

**Steps:**

```bash
cd crates/box/src

# 1. Build the binary
cargo build -p a3s-box-cli

# 2. Set library paths (macOS only)
export DYLD_LIBRARY_PATH="$(ls -td target/debug/build/libkrun-sys-*/out/libkrun/lib | head -1):$(ls -td target/debug/build/libkrun-sys-*/out/libkrunfw/lib | head -1)"

# 3a. Run VM lifecycle integration tests
cargo test -p a3s-box-cli --test nginx_integration -- --ignored --nocapture

# 3b. Run TEE integration tests (single-threaded — shares one VM)
cargo test -p a3s-box-cli --test tee_integration -- --ignored --nocapture --test-threads=1

# 3c. Run a single test
cargo test -p a3s-box-cli --test tee_integration -- --ignored --nocapture test_tee_seal_unseal_lifecycle
```

**Limitations:**
- Requires hardware virtualization (Apple HVF on macOS, KVM on Linux) — cannot run in CI containers without nested virtualization
- TEE tests use simulation mode (`--tee-simulate`) — not real AMD SEV-SNP hardware
- First run downloads OCI images (~5MB Alpine, ~25MB nginx) — needs internet
- Each test boots a real MicroVM (~200ms cold start + ~3s attestation server ready)
- TEE tests must run single-threaded (`--test-threads=1`) because they share the same box name
- `guest-init` tests require cross-compilation to `aarch64-unknown-linux-musl` and cannot run on the host
- Sealed data from simulation mode is NOT portable to real SEV-SNP hardware (different key derivation)

### Project Structure

```
box/
├── src/
│   ├── cli/            # Docker-like CLI (a3s-box binary)
│   │   └── src/
│   │       ├── commands/   # 47 subcommands (run, create, start, stop, pause, unpause, restart, rm, kill, rename, ps, stats, logs, exec, top, inspect, cp, images, pull, push, rmi, image-inspect, image-prune, tag, save, load, export, build, commit, diff, events, container-update, network, volume, df, system-prune, login, logout, history, port, wait, attach, version, info, update, attest, monitor)
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
