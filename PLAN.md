# Plan: A3S Box Embedded Sandbox Mode

## Goal

Expose A3S Box as an embeddable library (`a3s-box-sdk`) with a simple async API, similar to [boxlite](https://github.com/boxlite-ai/boxlite). No daemon, no CLI — just `create → exec → stop` from your Rust code (and later Python/Node.js via FFI).

## Current State

A3S Box already has the building blocks:
- `a3s-box-runtime`: `VmManager` handles full VM lifecycle (create, boot, exec, stop)
- `VmmProvider` trait: pluggable VM backend (libkrun)
- `ExecClient` / `PtyClient`: Frame-based command execution
- OCI image pulling, rootfs building, caching — all in runtime
- TEE support (attestation, sealed storage, secret injection)

The problem: these are tightly coupled to the CLI layer. `VmManager` requires manual wiring of providers, rootfs builders, image stores, etc.

## Design

### New crate: `a3s-box-sdk` (at `src/sdk/`)

A thin facade over `a3s-box-runtime` that provides a batteries-included API:

```rust
use a3s_box_sdk::{BoxSdk, SandboxOptions, ExecResult};

// One-time init (sets up image cache, rootfs cache, etc.)
let sdk = BoxSdk::new().await?;

// Create a sandbox from an OCI image
let sandbox = sdk.create(SandboxOptions {
    image: "python:3.12-slim".into(),
    cpus: 2,
    memory_mb: 512,
    ..Default::default()
}).await?;

// Execute commands
let result: ExecResult = sandbox.exec("python", &["-c", "print('hello')"]).await?;
println!("{}", result.stdout);

// Interactive PTY session
let pty = sandbox.pty("/bin/bash", 80, 24).await?;

// Stop and cleanup
sandbox.stop().await?;
```

### Key types

```
BoxSdk          — Runtime singleton (image store, caches, VMM provider)
SandboxOptions  — Image, CPU, memory, env, mounts, network, TEE config
Sandbox         — A running MicroVM instance (exec, pty, stop, state)
ExecResult      — stdout, stderr, exit_code
```

### Architecture

```
a3s-box-sdk (new)
  └── depends on: a3s-box-runtime, a3s-box-core, a3s-transport

BoxSdk::new()
  ├── ImageStore (OCI pull + cache)
  ├── RootfsCache (content-addressable)
  ├── LayerCache
  └── VmmProvider (libkrun)

sdk.create(options)
  ├── Pull image (if not cached)
  ├── Build rootfs
  ├── Create VmManager
  ├── Boot VM
  ├── Wait for agent health
  └── Return Sandbox handle

sandbox.exec(cmd, args)
  ├── ExecClient.execute(ExecRequest)
  └── Return ExecResult

sandbox.stop()
  ├── VmManager.stop()
  └── Cleanup rootfs/sockets
```

### What changes

1. **New crate `src/sdk/`** — The public API surface
2. **No changes to runtime** — SDK wraps runtime, doesn't modify it
3. **Runtime lib.rs** — May need to make a few internal types `pub` if they're currently private

### Implementation steps

1. Create `src/sdk/Cargo.toml` with deps on runtime, core, transport
2. Create `src/sdk/src/lib.rs` — `BoxSdk`, `Sandbox`, `SandboxOptions`, `ExecResult`
3. Implement `BoxSdk::new()` — wire up ImageStore, caches, provider
4. Implement `BoxSdk::create()` — pull image, build rootfs, boot VM
5. Implement `Sandbox::exec()` — delegate to ExecClient
6. Implement `Sandbox::pty()` — delegate to PtyClient
7. Implement `Sandbox::stop()` — delegate to VmManager
8. Tests
9. Update README with embedded usage example

### Future (not in this PR)

- Python SDK via PyO3
- Node.js SDK via napi-rs
- C FFI for other languages
- Warm pool integration for instant sandbox creation
