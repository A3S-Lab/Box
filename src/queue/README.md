# A3S Box Queue

Lane-based command queue utilities and monitoring for A3S Box.

## Overview

This package provides high-level utilities and monitoring capabilities for the core command queue system. It builds on top of the `a3s-box-core` queue implementation to provide:

- **QueueManager**: High-level API for managing the command queue with builder pattern
- **QueueMonitor**: Real-time monitoring and health checking for queue metrics

## Features

### Queue Manager

The `QueueManager` provides a convenient builder pattern for setting up and managing the command queue:

```rust
use a3s_box_queue::QueueManagerBuilder;
use a3s_box_core::event::EventEmitter;

// Create a queue manager with default lanes
let emitter = EventEmitter::new();
let manager = QueueManagerBuilder::new(emitter)
    .with_default_lanes()
    .build()
    .await?;

// Start the scheduler
manager.start().await?;

// Get statistics
let stats = manager.stats().await?;
println!("Pending: {}, Active: {}", stats.total_pending, stats.total_active);
```

### Default Lanes

The package provides six default priority lanes:

- **P0 - System**: Highest priority (max 5 concurrent)
- **P1 - Control**: Control operations (max 3 concurrent)
- **P2 - Query**: Query operations (max 10 concurrent)
- **P3 - Session**: Session management (max 5 concurrent)
- **P4 - Skill**: Skill execution (max 3 concurrent)
- **P5 - Prompt**: User prompts (max 2 concurrent)

### Queue Monitor

The `QueueMonitor` provides real-time monitoring and alerting:

```rust
use a3s_box_queue::QueueMonitor;

let monitor = Arc::new(QueueMonitor::new(queue));
monitor.start().await;
```

The monitor will:
- Check queue health at regular intervals
- Warn when lanes reach capacity
- Alert on high pending/active command counts
- Log detailed lane statistics

## Architecture

This package is designed to work with the core queue implementation in `a3s-box-core`. The core provides:
- Basic queue and lane structures
- Command execution framework
- Built-in priority-based scheduler

This package adds:
- Builder pattern for easy setup
- Statistics aggregation
- Health monitoring and alerting
- Convenient high-level APIs

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
a3s-box-queue = { path = "../queue" }
```

## License

MIT
