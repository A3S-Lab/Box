# A3S Box Runtime

MicroVM runtime implementation for A3S Box.

## Overview

This package provides the actual runtime implementation for A3S Box, including:

- **VM Management**: MicroVM lifecycle management with libkrun
- **Session Management**: Multi-session context tracking and management
- **Skill Management**: Deno-style skill package loading and execution
- **gRPC Communication**: Guest agent communication over vsock
- **Filesystem Operations**: virtio-fs mount management
- **Metrics Collection**: Runtime metrics and monitoring

## Architecture

The runtime package builds on top of `a3s-box-core` which provides foundational types:

```
┌─────────────────────────────────────┐
│         a3s-box-runtime             │
│  (VM, Session, Skill, gRPC, etc.)   │
└─────────────────────────────────────┘
                 │
                 ▼
┌─────────────────────────────────────┐
│          a3s-box-core               │
│  (Config, Error, Event, Queue)      │
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

### Session Manager

Manages multiple concurrent sessions:

```rust
use a3s_box_runtime::SessionManager;

let manager = SessionManager::new(event_emitter);

// Create a session
let session_id = manager.create_session("session-1", None).await?;

// Use the session
// ...

// Destroy session
manager.destroy_session(&session_id).await?;
```

### Skill Manager

Manages skill packages (Deno-style):

```rust
use a3s_box_runtime::SkillManager;

let manager = SkillManager::new(workspace_path, event_emitter);

// Install a skill
manager.install("https://example.com/skill.tar.gz").await?;

// Load a skill
let skill = manager.load("skill-name").await?;

// Uninstall
manager.uninstall("skill-name").await?;
```

## VM States

The runtime manages the following VM states:

- **Created**: Config captured, no VM started
- **Ready**: VM booted, agent initialized, gRPC healthy
- **Busy**: A session is actively processing
- **Compacting**: A session is compressing its context
- **Stopped**: VM terminated, resources freed

## gRPC Communication

The runtime communicates with the guest agent over vsock (port 4088):

- Command execution
- Session management
- Skill loading
- Context management
- Health checks

## License

MIT
