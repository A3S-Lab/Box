# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**A3S Box** is a meta-agent sandbox runtime based on microVMs. It embeds a full-featured coding agent inside hardware-isolated virtual machines, exposing Python and TypeScript SDKs. Think "SQLite for sandboxing" — a lightweight library embedded directly in applications without requiring a daemon or root privileges.

The developer writes business logic in Python/TypeScript + SKILL.md files; A3S Box provides a sandboxed coding agent that can execute arbitrary code, edit files, and run tools — all confined to a microVM with its own Linux kernel.

## Build & Development Commands

```bash
# Build
cargo build                           # Build entire workspace
cargo build -p a3s-box-core           # Build specific crate
cargo build --release                 # Release build

# Test
cargo test --all                      # All tests
cargo test -p a3s-box-core --lib      # Unit tests for a specific crate
cargo test -p a3s-box-runtime --lib -- --test-threads=1  # Single-threaded (avoids gvproxy Go runtime issues)
cargo test -p a3s-box-core --lib -- test_name            # Run a single test by name

# Format & Lint
cargo fmt --all                       # Format code
cargo fmt --all -- --check            # Check formatting
cargo clippy                          # Lint (enforced in CI)

# Proto compilation happens automatically via build.rs in runtime/
```

## Architecture

```
Host Process (Python/TypeScript SDK)
  │
  ▼ (gRPC over vsock:4088)
a3s-box-runtime (Rust library)
  ├── VmManager (BoxState lifecycle: Created → Ready → Busy → Compacting → Stopped)
  ├── SessionManager (multi-session support)
  ├── SkillManager (Deno-style package management)
  ├── CommandQueue (lane-based priority scheduling, 6 built-in lanes)
  └── gRPC Client → guest
  │
  ▼ (libkrun + virtio-fs)
Guest (inside microVM)
  ├── a3s-box-code (Rust agent binary)
  ├── gRPC Server (AgentService on vsock:4088)
  ├── LLM Client (Anthropic, OpenAI, etc.)
  ├── Tool Executor (bash, read, write, edit, grep, glob)
  └── Session management
```

### Workspace Crates

| Crate | Type | Purpose |
|-------|------|---------|
| `core` | lib | Foundational types: `BoxConfig`, `BoxError`, `BoxEvent`, `CommandQueue`, `Lane` |
| `runtime` | lib | VM lifecycle, session management, skill loading, gRPC client, virtio-fs mounts, metrics |
| `code` | bin | Guest agent: LLM providers, tool execution, session management inside the VM |
| `queue` | lib | `QueueManager` (builder pattern) and `QueueMonitor` (health checking) |
| `cli` | bin | Clap-based CLI: `create`, `build`, `cache-warmup` commands |
| `sdk/python` | cdylib | Python bindings via PyO3 |
| `sdk/typescript` | cdylib | TypeScript bindings via NAPI-RS |

Dependency flow: all crates depend on `core` for shared types and error handling.

### Lane-Based Command Queue

All SDK calls enter a priority queue before reaching the guest. Six built-in lanes (priority 0-5): system, control, query, session, skill, prompt. Skills can declare custom lanes (`skill:<name>`) for isolated provisioning. Higher priority lanes (lower number) never get blocked by lower priority work.

### Skill System (Deno-style)

Skills are SKILL.md files with YAML frontmatter pointing to remote CLI tool URLs. On first activation, tools are downloaded and cached in `/a3s/cache/`. Subsequent activations use the cache. Three-level progressive disclosure: metadata at boot → full context + tools on activation → instant from cache.

### gRPC Protocol

Defined in `src/runtime/proto/agent.proto`. Compiled by `tonic-build` in `src/runtime/build.rs`. Services: session CRUD, generate/stream (text and structured), skill management, session commands (compact/clear/configure), introspection, and control (cancel/health).

## Code Style

**Rust:** Follow [Microsoft Rust Guidelines](https://microsoft.github.io/rust-guidelines). `cargo fmt` for formatting, `cargo clippy` for linting (enforced in CI).

Key guidelines:
- **M-PANIC-IS-STOP**: Panics terminate, don't use for error handling
- **M-CONCISE-NAMES**: Avoid "Service", "Manager", "Factory" in type names
- **M-UNSAFE**: Minimize and document all unsafe blocks

**Code Conventions:**

- **Async-first**: All I/O uses Tokio. No blocking operations in async context.
- **Error handling**: Centralized `BoxError` enum (thiserror) in `core/src/error.rs`. Use `Result<T>` type alias. Always include full context in error messages with `map_err`.
- **Event keys**: Dot-separated lowercase: `<domain>.<subject>.<action>` (e.g., `session.context.warning`, `prompt.tool.called`).
- **State machine**: `BoxState` enum with `RwLock` synchronization: `Created → Ready → Busy → Compacting → Stopped`.
- **Public types** must be `Send + Sync`.
- **No panics** in production code.
- **Naming**: crates are kebab-case, modules are snake_case, types are PascalCase.

**Python SDK:** Async/await for all I/O. Context managers (`async with`) for automatic cleanup. Type hints encouraged.

## Important Notes

### Platform Support

- macOS ARM64 (Apple Silicon), Linux x86_64/ARM64, Windows (via WSL2)
- macOS Intel is **NOT** supported

### Architecture Quirks

- **gRPC communication**: Host-guest communication via vsock (not TCP), port 4088
- **Runtime data**: `~/.a3s/` directory stores images, boxes, cache, and databases
- **Lazy boot**: VM starts on first API call, not on `create_box()`

### Common Pitfalls

- Running on Intel Mac → UnsupportedEngine error
- Not handling async/await properly → runtime errors
- Exceeding resource limits → box killed (OOM)

### Quick Debugging

1. Enable logging: `RUST_LOG=debug`
2. Check `~/.a3s/` disk space and permissions
3. Verify platform support (KVM on Linux, Hypervisor.framework on macOS)

---

## Mandatory Code Design Rules

**CRITICAL: These rules are MANDATORY for all code contributions.**

### Meta-Principle

**0. DON'T BE YES MAN** — Challenge assumptions, question designs, identify flaws

- **Challenge yourself too, not just the user**
- Before implementing: "Does this already exist?" (search first)
- Before adding logic: "Does this layer need to know this?"
- After writing: "What breaks if I delete this?"

### Core Principles (Must Know)

**1. Single Responsibility** — One function, one job
**2. Boring Code** — Obvious > clever
**3. Search Before Implement** — grep before writing, read before coding
**4. Only What's Used** — No future-proofing, delete dead code immediately
**5. DRY** — Don't Repeat Yourself (single source of truth)
**6. Explicit Errors** — Self-documenting error messages

### Supporting Principles (Reference When Needed)

**7. Minimal Knowledge** — Components only know interfaces, not internals
**8. No Premature Optimization** — Measure first, optimize later
**9. Explicit Paths** — Calculate from known roots, never assume
**10. Prepare Before Execute** — Setup before irreversible operations
**11. Validate Early** — Check preconditions before expensive work
**12. Thoughtful Naming** — Consider 5+ alternatives, choose the clearest
**13. Structured Code** — Organized hierarchy, clear layers, predictable organization
**14. Idiomatic by Default** — Follow each language's standard patterns and libraries first; only diverge with a clear, documented reason

### Quick Examples

**DON'T BE YES MAN**

```
// ❌ Yes Man behavior
User: "Let's add a Supervisor component to track metrics"
AI: "Great idea! I'll implement it right away."
[Creates empty struct with no implementation, violates YAGNI]

// ✅ Critical thinking
User: "Let's add a Supervisor component to track metrics"
AI: "Wait - what data would Supervisor need? The process handle is in VmController.
     Creating a separate component means duplicating PID tracking (violates DRY).
     Supervisor has no access to the data it needs. This design won't work.

     Better approach: VmController already owns the process, it should provide metrics.
     Do you want me to implement metrics() in VmController instead?"
```

**When to push back:**

- Design violates any of the 15 golden rules
- User requests create duplicate state/logic
- Implementation requires blocking operations (sleep, sync I/O in async contexts)
- Code adds complexity without clear benefit
- "Future-proofing" that isn't needed now (YAGNI)

**How to push back:**

1. Identify specific rule violations
2. Explain why current approach won't work
3. Propose concrete alternative
4. Let user decide

**Single Responsibility**

```rust
// ❌ One function doing everything
fn setup_and_start_vm(image: &str) -> Result<VM> { /* ... */ }

// ✅ Each function has one job
fn pull_image(image: &str) -> Result<Manifest> { /* ... */ }
fn create_workspace(manifest: &Manifest) -> Result<Workspace> { /* ... */ }
fn start_vm(workspace: &Workspace) -> Result<VM> { /* ... */ }
```

**Boring Code**

```rust
// ❌ Clever, hard to understand
fn metrics(&self) -> RawMetrics {
    self.process.as_ref()
        .and_then(|p| System::new().process(Pid::from(p.id())))
        .map(|proc| RawMetrics { cpu: proc.cpu_usage(), mem: proc.memory() })
        .unwrap_or_default()
}

// ✅ Boring, obvious
fn metrics(&self) -> RawMetrics {
    if let Some(ref process) = self.process {
        let mut sys = System::new();
        sys.refresh_process(pid);
        if let Some(proc_info) = sys.process(pid) {
            return RawMetrics {
                cpu_percent: Some(proc_info.cpu_usage()),
                memory_bytes: Some(proc_info.memory()),
            };
        }
    }
    RawMetrics::default()
}
```

**Search Before Implement**

BEFORE writing ANY code, search for existing implementations:

```bash
# ❌ Writing transformation without searching
# (adds duplicate unix→vsock transformation in runtime/vm.rs)

# ✅ Search first, find existing code
$ grep -r "transform.*guest" src/
src/runtime/engines/krun/engine.rs:113:fn transform_guest_args(...)
# → Found it! Use existing code, don't duplicate.
```

**Search patterns to try:**

- Similar functionality: `grep -r "transform.*args" src/`
- Function names: `grep -r "function_name" src/`
- Constants/config: `grep -r "VSOCK_PORT\|4088" src/`
- Layer ownership: `grep -r "GUEST_AGENT" src/` (shows which modules use it)

**DRY (Don't Repeat Yourself)**

```rust
// ❌ Duplicated constants
const VSOCK_PORT: u32 = 4088;  // host
const VSOCK_PORT: u32 = 4088;  // guest

// ✅ Shared in core
use a3s_box_core::VSOCK_GUEST_PORT;
```

**Explicit Error Context**

```rust
// ❌ Generic error
std::fs::create_dir_all(&dir)?;

// ✅ Self-documenting
std::fs::create_dir_all(&socket_dir).map_err(|e| {
    BoxError::Other(format!(
        "Failed to create socket directory {}: {}", socket_dir.display(), e
    ))
})?;
```

**Explicit Path Calculation**

```rust
// ❌ Assumes relationship
let box_dir = rootfs_dir.join(box_id);

// ✅ Calculate from known root
let home_dir = rootfs_dir.parent().ok_or(...)?;
let box_dir = home_dir.join(dirs::BOXES_DIR).join(box_id);
```

**Minimal Knowledge**

```rust
// ❌ Component knows about other's internals
mod krun_engine {
    use crate::networking::constants::GUEST_MAC;
    fn configure_network(&self, socket_path: &str) {
        self.ctx.add_net_path(socket_path, GUEST_MAC);
    }
}

// ✅ Component only knows interface
mod krun_engine {
    fn configure_network(&self, socket_path: &str, mac_address: [u8; 6]) {
        self.ctx.add_net_path(socket_path, mac_address);
    }
}
```

Minimal Knowledge applies to comments too:

```rust
// ❌ Comment reveals implementation details
// Pass transport as-is - krun engine will transform unix:// to vsock://
let uri = transport.to_uri();

// ✅ Comment maintains abstraction
// Engine handles any transport-specific transformations
let uri = transport.to_uri();
```

**Prepare Before Execute**

```rust
// ❌ Setup mixed with critical operation
fn start_vm() -> Result<()> {
    let ctx = create_ctx()?;
    ctx.start();  // Process takeover - can't recover from errors!
}

// ✅ All setup before point of no return
std::fs::create_dir_all(&socket_dir)?;  // Can fail safely
let ctx = create_ctx()?;                 // Can fail safely
ctx.configure()?;                        // Can fail safely
ctx.start();                             // Point of no return
```

**Structured Code**

```rust
// ❌ Flat, disorganized
mod rootfs {
    pub fn prepare() { ... }
    pub fn extract() { ... }
    pub fn mount() { ... }
    pub struct PreparedRootfs { ... }
    pub struct SimpleRootfs { ... }
}

// ✅ Hierarchical, organized by responsibility
mod rootfs {
    mod operations;  // Low-level primitives
    mod prepared;    // High-level orchestration (uses operations)
    mod simple;      // Alternative implementation

    pub use operations::{extract_layer_tarball, mount_overlayfs_from_layers};
    pub use prepared::PreparedRootfs;
    pub use simple::SimpleRootfs;
}
```

File organization pattern:

```
src/
  ├── lib.rs              // Public API only
  ├── errors.rs           // Shared error types
  ├── feature/
  │   ├── mod.rs          // Public interface + re-exports
  │   ├── operations.rs   // Low-level primitives
  │   ├── types.rs        // Feature-specific types
  │   └── impl.rs         // High-level implementation
```

### Pre-Submission Checklist

**Pre-Implementation (BEFORE writing code):**

- [ ] Searched for similar functionality (`grep -r "pattern" src/`)
- [ ] Read ALL files that would be affected (completely, not skimmed)
- [ ] Identified correct layer for new logic (ownership analysis)
- [ ] Verified no duplicate logic exists
- [ ] Questioned: "Does this component need to know this?"
- [ ] Applied Rule #0 to OWN design (not just user's request)

**Core Principles:**

- [ ] Each function has single responsibility (one job)
- [ ] Code is boring and obvious (not clever)
- [ ] Only code that's actually used exists (no future-proofing, no dead code)
- [ ] No duplicated knowledge (DRY - single source of truth)
- [ ] Every error has full context (self-documenting)

**Supporting Principles:**

- [ ] Components only know interfaces (minimal knowledge / loose coupling)
- [ ] No optimization without measurement
- [ ] Paths calculated from known roots (never assume)
- [ ] Setup completed before irreversible operations
- [ ] Preconditions validated early
- [ ] Names considered carefully (5+ alternatives evaluated)
- [ ] Code has clear hierarchy and predictable organization

---

## Lessons from Real Mistakes

### Case Study: Duplicate Transformation Logic

**The Mistake:**
Added `unix://` → `vsock://` transformation in `runtime/vm.rs` when it already existed in `runtime/engines/krun/engine.rs`.

**Why it happened:**

- Didn't search before implementing (violated Rule #3)
- Became "yes man" to own design (violated Rule #0)
- Didn't question which layer should own the logic (violated Rule #7)

**How rules should have prevented it:**

| Rule | What Should Have Happened |
|------|---------------------------|
| **#0 (Don't Be Yes Man)** | "Does this already exist?" → Search first |
| **#3 (Search Before Implement)** | `grep -r "transform.*vsock" src/` → Found in engines/krun/engine.rs |
| **#5 (DRY)** | Check for existing transformation logic |
| **#7 (Minimal Knowledge)** | "Why does vm.rs know about krun details?" → Wrong layer |

**The Fix:**

```rust
// ❌ WRONG: vm.rs duplicating krun logic
fn create_guest_entrypoint(&self, transport: &Transport) -> GuestEntrypoint {
    let guest_transport = match transport {
        Transport::Unix { .. } => Transport::vsock(4088),  // Duplicate!
        ...
    };
}

// ✅ RIGHT: Pass as-is, let engine handle it
fn create_guest_entrypoint(&self, transport: &Transport) -> GuestEntrypoint {
    let uri = transport.to_uri();  // engines/krun/engine.rs transforms later
    format!("exec a3s-box-agent --listen {}", uri)
}
```

**Key Lesson:**
Rules are not a QA checklist to run after coding. They are a **design thinking framework** to apply BEFORE and DURING coding. Always:

1. Search first (`grep`)
2. Read affected files completely
3. Question ownership/layering
4. THEN code

---

## How to Use These Rules

**❌ WRONG: Checklist after coding**

1. Write code
2. Check if it follows rules
3. Fix violations

**✅ RIGHT: Active thinking before coding**

1. Search for existing solutions (`grep -r "pattern" src/`)
2. Read affected files completely (don't skim)
3. Analyze ownership/layering ("Who should know this?")
4. Question necessity ("What breaks if I don't add this?")
5. THEN code (following rules)

**The rules are not a QA checklist—they're a design thinking framework.**
