# A3S Box Runtime

MicroVM runtime implementation for A3S Box.

## Overview

This package provides the actual runtime implementation for A3S Box, including:

- **VM Management**: MicroVM lifecycle management with libkrun
- **OCI Image Support**: Pull, store, and extract OCI container images
- **Rootfs Builder**: Construct guest root filesystems from OCI layers
- **gRPC Communication**: Guest agent health checking over Unix socket
- **Filesystem Operations**: virtio-fs mount management
- **Metrics Collection**: Runtime metrics and monitoring

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
