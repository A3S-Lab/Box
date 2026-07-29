# A3S Box Runtime

Local MicroVM, Sandbox, and shared A3S Runtime provider implementation for A3S
Box.

## Overview

This package provides the actual runtime implementation for A3S Box, including:

- **VM Management**: MicroVM lifecycle management with libkrun
- **OCI Image Support**: Pull, store, and extract OCI container images
- **Rootfs Builder**: Construct guest root filesystems from OCI layers
- **gRPC Communication**: Guest agent health checking over Unix socket
- **Filesystem Operations**: virtio-fs mount management
- **Metrics Collection**: Runtime metrics and monitoring
- **A3S Runtime Provider**: generation-fenced Tasks and Services over the
  explicit Linux Sandbox backend
- **Transient Secrets**: caller-authorized environment and file Secret
  materialization without durable plaintext

## Architecture

The runtime package builds on top of `a3s-box-core` which provides foundational types:

```
┌─────────────────────────────────────┐
│         a3s-box-runtime             │
│  (VM, OCI, Rootfs, gRPC, etc.)     │
└─────────────────────────────────────┘
                 │
                 ▼
┌─────────────────────────────────────┐
│          a3s-box-core               │
│  (Config, Error, Event)            │
└─────────────────────────────────────┘
```

## Shared A3S Runtime provider

`BoxRuntimeDriver` implements the shared `a3s-runtime` contract. It maps
digest-pinned OCI Tasks and Services onto the canonical local execution manager
instead of creating a second Box lifecycle path. The driver owns provider
identity, generation fencing, recovery, Service endpoints, health observations,
logs, exec, resource controls, and cleanup.

Callers that need Runtime Secrets compose the driver with exactly one
`BoxSecretMaterializer`:

```rust
use std::sync::Arc;

use a3s_box_runtime::{BoxRuntimeDriver, BoxRuntimeDriverConfig};

let config = BoxRuntimeDriverConfig {
    secret_root: "/run/a3s-box/runtime-secrets".into(),
    ..Default::default()
};
let driver = BoxRuntimeDriver::with_secret_materializer(
    config,
    Arc::new(node_secret_materializer),
)?;
```

The configured `secret_root` must already be a canonical provider-owned `0700`
Linux tmpfs mount. Box never creates it and never falls back to disk. The caller
owns reference authorization and transport; Box owns only node-local
materialization, deterministic read-only Sandbox mounts, recovery validation,
log redaction, and lifecycle cleanup.

Environment and file targets are supported. Environment values use a
non-sensitive `A3S_BOX_SECRET_ENV_V1` binding manifest that Guest Init validates
and consumes immediately before process creation. Registry credential targets
remain unsupported. Secret bytes are zeroized in memory where they cross the
materializer boundary and never enter durable Box records, creation intent, or
OCI configuration. Stopping retains material for an explicit restart; removal
and stale-generation retirement remove it. A reconstructed driver validates the
existing live generation without rematerializing it. Every log read reauthorizes
the references, redacts exact values longest-first, and hashes only redacted
content into its cursor.

## Components

### VM Manager

Manages the microVM lifecycle:

```rust
use a3s_box_runtime::VmManager;
use a3s_box_core::{BoxConfig, EventEmitter};

let config = BoxConfig::default();
let emitter = EventEmitter::new();
let vm = VmManager::new(config, emitter);

// Boot the VM (lazy initialization)
vm.boot().await?;

// Check health
let healthy = vm.health_check().await?;

// Destroy when done
vm.destroy().await?;
```

## VM States

The runtime manages the following VM states:

- **Created**: Config captured, no VM started
- **Ready**: VM booted, agent initialized, health check passing
- **Busy**: A session is actively processing
- **Compacting**: A session is compressing its context
- **Stopped**: VM terminated, resources freed

## gRPC Communication

The runtime communicates with the guest agent over Unix socket (bridged to vsock port 4088):

- Health checks

Agent-level operations (sessions, generation, skills) are handled by the
a3s-code crate, not the Box runtime.

## License

MIT
