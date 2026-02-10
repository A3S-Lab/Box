# A3S Box CLI Command Summary

This document summarizes the existing CLI commands and their implementation details to guide the development of new Docker-compatible commands.

---

## Table of Contents

1. [Existing Commands Overview](#existing-commands-overview)
2. [Key Data Structures](#key-data-structures)
3. [Guest Communication](#guest-communication)
4. [Process Lifecycle](#process-lifecycle)
5. [Implementation Patterns](#implementation-patterns)
6. [Answers to Specific Questions](#answers-to-specific-questions)

---

## Existing Commands Overview

### 1. `run` - Pull + Create + Start
**File:** `cli/src/commands/run.rs`

**Key Arguments:**
- `--name`: Assign a name to the box
- `--cpus`: Number of CPUs (default: 2)
- `--memory`: Memory (e.g., "512m", "2g", default: "512m")
- `-v, --volume`: Volume mount (host:guest), repeatable
- `-e, --env`: Environment variable (KEY=VALUE), repeatable
- `-p, --publish`: Port mapping (host_port:guest_port), repeatable
- `--dns`: Custom DNS servers, repeatable
- `-d, --detach`: Run in background
- `--rm`: Auto-remove on stop
- `cmd`: Command override (last positional args)

**Key Functions:**
- `parse_env_vars()`: Parses KEY=VALUE pairs into HashMap
- `tail_file()`: Follows console log in foreground mode
- Creates `VmManager`, boots VM, saves `BoxRecord` to state

**Port Mappings:** Stored in `BoxConfig.port_map` (Vec<String>) and passed to runtime

---

### 2. `stop` - Graceful Shutdown
**File:** `cli/src/commands/stop.rs`

**Key Arguments:**
- `boxes`: Box name(s) or ID(s) (required, multiple)
- `-t, --timeout`: Seconds to wait before force-killing (default: 10)

**Signal Flow:**
1. Send `SIGTERM` to PID
2. Poll with `is_process_alive()` every 100ms
3. After timeout, send `SIGKILL`
4. Update state to "stopped", clear PID
5. Auto-remove if `auto_remove` flag is set

**Key Functions:**
- `is_process_alive(pid)`: Uses `libc::kill(pid, 0)` to check liveness
- `stop_one()`: Handles single box stop with timeout

---

### 3. `kill` - Send Signal
**File:** `cli/src/commands/kill.rs`

**Key Arguments:**
- `boxes`: Box name(s) or ID(s) (required, multiple)
- `-s, --signal`: Signal to send (default: "KILL")

**Supported Signals:**
- KILL/SIGKILL, TERM/SIGTERM, INT/SIGINT, HUP/SIGHUP
- QUIT/SIGQUIT, USR1/SIGUSR1, USR2/SIGUSR2
- **STOP/SIGSTOP, CONT/SIGCONT** ✅ (for pause/unpause)
- Numeric signals (e.g., "9")

**Key Functions:**
- `parse_signal()`: Converts signal name/number to libc constant
- Only updates state to "stopped" for SIGKILL/SIGTERM
- Other signals (like STOP/CONT) don't change state

**Important:** SIGSTOP/SIGCONT are already supported! Can be used for pause/unpause.

---

### 4. `restart` - Stop + Start
**File:** `cli/src/commands/restart.rs`

**Key Arguments:**
- `boxes`: Box name(s) or ID(s) (required, multiple)
- `-t, --timeout`: Stop timeout (default: 10)

**Flow:**
1. Stop the box (SIGTERM → SIGKILL after timeout)
2. Rebuild `BoxConfig` from stored record
3. Create new `VmManager` with existing box_id
4. Boot VM and update state

**Note:** Uses `VmManager::with_box_id()` to reuse existing box directory

---

### 5. `exec` - Execute Command in Guest
**File:** `cli/src/commands/exec.rs`

**Key Arguments:**
- `box`: Box name or ID
- `--timeout`: Timeout in seconds (default: 5)
- `-e, --env`: Environment variables (KEY=VALUE), repeatable
- `-w, --workdir`: Working directory inside the box
- `cmd`: Command and arguments (last positional, required)

**Communication:**
- Connects to **exec Unix socket** at `{box_dir}/sockets/exec.sock`
- Uses `ExecClient::connect()` → `exec_command()`
- Sends HTTP POST /exec with JSON `ExecRequest`
- Receives JSON `ExecOutput` (stdout, stderr, exit_code)
- Exits with command's exit code

**Key Types:**
- `ExecRequest`: cmd, timeout_ns, env, working_dir
- `ExecOutput`: stdout (Vec<u8>), stderr (Vec<u8>), exit_code (i32)

---

### 6. `cp` - Copy Files Host ↔ Guest
**File:** `cli/src/commands/cp.rs`

**Key Arguments:**
- `src`: Source path (HOST_PATH or BOX:CONTAINER_PATH)
- `dst`: Destination path (HOST_PATH or BOX:CONTAINER_PATH)

**Implementation:**
- Uses **exec channel** (same as `exec` command)
- Encodes files as base64 for safe transfer
- `copy_from_box()`: Runs `base64 < file` in guest, decodes on host
- `copy_to_box()`: Encodes on host, runs `echo | base64 -d > file` in guest
- Supports binary files via base64 encoding
- **Directory support**: Recursively copies directories using `tar` + base64
  - `copy_dir_from_box()`: Runs `tar -cf - dir | base64` in guest, extracts on host
  - `copy_dir_to_box()`: Creates tar on host, sends `base64 -d | tar -xf -` to guest
  - Auto-detects directories via `std::fs::metadata` (host) or `test -d` (box)

**Limitations:**
- No box-to-box copying
- Subject to exec timeout and output size limits

---

### 7. `logs` - View Console Logs
**File:** `cli/src/commands/logs.rs`

**Key Arguments:**
- `box`: Box name or ID
- `-f, --follow`: Follow log output
- `--tail`: Number of lines to show from the end

**Implementation:**
- Reads from `record.console_log` file (`{box_dir}/logs/console.log`)
- Follow mode: polls file every 200ms for new content
- Tail mode: reads last N lines from file
- No streaming from live PTY (just file tailing)

**Note:** Console log is written by the VM, not interactive PTY

---

### 8. `stats` - Resource Usage
**File:** `cli/src/commands/stats.rs`

**Key Arguments:**
- `box`: Box name or ID (optional, shows all if omitted)
- `--no-stream`: Single snapshot instead of live updates

**Implementation:**
- Uses `sysinfo` crate to read process stats by PID
- Collects: CPU%, memory usage, memory limit
- Requires two `refresh_process()` calls 200ms apart for CPU delta
- Streams updates every 1 second (clears screen with ANSI escape)

**Key Functions:**
- `collect_stats()`: Reads CPU and memory from PID
- `print_stats()`: Formats table with comfy_table

**Limitations:**
- Only shows host process stats (shim PID)
- No per-container network/disk I/O stats

---

### 9. `inspect` - Detailed Box Info
**File:** `cli/src/commands/inspect.rs`

**Key Arguments:**
- `box`: Box name or ID

**Implementation:**
- Resolves box, serializes `BoxRecord` to pretty JSON
- Shows all stored metadata

---

## Key Data Structures

### BoxRecord (State File)
**File:** `cli/src/state.rs`

```rust
pub struct BoxRecord {
    pub id: String,                    // Full UUID
    pub short_id: String,              // First 12 hex chars
    pub name: String,                  // User-assigned or auto-generated
    pub image: String,                 // OCI image reference
    pub status: String,                // "created" | "running" | "paused" | "stopped" | "dead"
    pub pid: Option<u32>,              // Shim process PID
    pub cpus: u32,                     // Number of vCPUs
    pub memory_mb: u32,                // Memory in MB
    pub volumes: Vec<String>,          // Volume mounts ("host:guest")
    pub env: HashMap<String, String>,  // Environment variables
    pub cmd: Vec<String>,              // Entrypoint override
    pub box_dir: PathBuf,              // ~/.a3s/boxes/<id>/
    pub socket_path: PathBuf,          // gRPC socket (agent health)
    pub exec_socket_path: PathBuf,     // Exec socket (command execution)
    pub console_log: PathBuf,          // Console output file
    pub created_at: DateTime<Utc>,     // Creation timestamp
    pub started_at: Option<DateTime<Utc>>, // Start timestamp
    pub auto_remove: bool,             // Auto-remove on stop
}
```

**Important:** Port mappings are NOT stored in BoxRecord! They're in BoxConfig but not persisted.

---

### BoxConfig (Runtime Configuration)
**File:** `core/src/config.rs`

```rust
pub struct BoxConfig {
    pub agent: AgentType,              // How agent is loaded
    pub business: BusinessType,        // Business code type
    pub workspace: PathBuf,            // Workspace directory
    pub skills: Vec<PathBuf>,          // Skill directories
    pub resources: ResourceConfig,     // CPU, memory, disk, timeout
    pub log_level: LogLevel,           // Debug, Info, Warn, Error
    pub debug_grpc: bool,              // gRPC debug logging
    pub tee: TeeConfig,                // TEE configuration
    pub cmd: Vec<String>,              // Command override
    pub volumes: Vec<String>,          // Extra volume mounts
    pub extra_env: Vec<(String, String)>, // Extra environment variables
    pub cache: CacheConfig,            // Cache configuration
    pub pool: PoolConfig,              // Warm pool configuration
    pub port_map: Vec<String>,         // Port mappings: "host_port:guest_port"
    pub dns: Vec<String>,              // Custom DNS servers
}
```

**Port Mappings:** Stored in `port_map` field (Vec<String> of "host:guest" pairs)

---

## Guest Communication

### 1. Exec Socket (Command Execution)
**Path:** `{box_dir}/sockets/exec.sock`
**Protocol:** HTTP over Unix socket
**Port:** VSOCK port 4089 (guest) → Unix socket (host)

**Client:** `ExecClient` (`runtime/src/grpc.rs`)
- `connect(socket_path)`: Verify socket is connectable
- `exec_command(request)`: Send HTTP POST /exec with JSON body

**Request/Response Types:** (`core/src/exec.rs`)
```rust
pub struct ExecRequest {
    pub cmd: Vec<String>,              // Command and args
    pub timeout_ns: u64,               // Timeout in nanoseconds
    pub env: Vec<String>,              // Additional env vars (KEY=VALUE)
    pub working_dir: Option<String>,   // Working directory
}

pub struct ExecOutput {
    pub stdout: Vec<u8>,               // Captured stdout
    pub stderr: Vec<u8>,               // Captured stderr
    pub exit_code: i32,                // Process exit code
}
```

**Constants:**
- `DEFAULT_EXEC_TIMEOUT_NS`: 5 seconds
- `MAX_OUTPUT_BYTES`: 16 MiB per stream

---

### 2. Agent Socket (Health Checking)
**Path:** `{box_dir}/sockets/grpc.sock`
**Protocol:** HTTP over Unix socket
**Port:** VSOCK port 4088 (guest) → Unix socket (host)

**Client:** `AgentClient` (`runtime/src/grpc.rs`)
- `connect(socket_path)`: Verify socket is connectable
- `health_check()`: Send GET /healthz, check for HTTP 200

**Note:** Agent-level operations (sessions, generation, skills) are in `a3s-code` crate, not Box runtime.

---

### 3. Console Log (Output Only)
**Path:** `{box_dir}/logs/console.log`
**Type:** File (not a socket)

**Usage:**
- Written by VM (stdout/stderr from guest init)
- Read by `logs` command (file tailing)
- No interactive PTY access

---

## Process Lifecycle

### PID Management
- **PID stored in BoxRecord:** Shim process PID (host-side)
- **PID source:** `VmManager::pid()` returns `Option<u32>`
- **Liveness check:** `libc::kill(pid, 0) == 0`

### State Reconciliation
**File:** `cli/src/state.rs` - `StateFile::reconcile()`

On every `StateFile::load()`:
1. Check all "running" boxes
2. If PID is dead (`kill(pid, 0) != 0`), mark as "dead"
3. If PID is None, mark as "dead"
4. Save state if any changes

### State Transitions
```
Created → Running → Stopped
                 ↘ Dead (if process dies unexpectedly)
```

**Status Values:**
- `"created"`: Config captured, VM not started
- `"running"`: VM booted, agent initialized
- `"stopped"`: Gracefully stopped
- `"dead"`: Process died unexpectedly (detected by reconciliation)

---

## Implementation Patterns

### 1. Box Resolution
**File:** `cli/src/resolve.rs`

**Resolution Order:**
1. Exact name match
2. Exact ID match
3. Unique ID prefix match (on full ID or short ID)

**Functions:**
- `resolve(&state, query)`: Returns `&BoxRecord`
- `resolve_mut(&mut state, query)`: Returns `&mut BoxRecord`

**Errors:**
- `ResolveError::NotFound`: No matching box
- `ResolveError::Ambiguous`: Multiple matches for prefix

---

### 2. Output Formatting
**File:** `cli/src/output.rs`

**Helper Functions:**
- `new_table(headers)`: Create styled table with comfy_table
- `format_bytes(bytes)`: Human-readable size (KB, MB, GB)
- `format_ago(datetime)`: Relative time ("5 minutes ago")
- `parse_memory(s)`: Parse "512m", "2g" → MB
- `parse_size_bytes(s)`: Parse "10g", "1t" → bytes

---

### 3. Signal Handling
**Pattern:** Direct `libc::kill()` calls

```rust
unsafe {
    libc::kill(pid as i32, libc::SIGTERM);
}
```

**Supported Signals:**
- SIGTERM (15): Graceful shutdown
- SIGKILL (9): Force kill
- SIGSTOP (19): Pause process ✅
- SIGCONT (18): Resume process ✅
- SIGINT (2): Interrupt
- SIGHUP (1): Hangup
- SIGQUIT (3): Quit
- SIGUSR1/SIGUSR2: User-defined

---

### 4. State Persistence
**File:** `~/.a3s/boxes.json`

**Atomic Writes:**
1. Serialize to JSON
2. Write to `boxes.json.tmp`
3. Rename to `boxes.json`

**Auto-Reconciliation:**
- Loads state on every command
- Marks dead PIDs automatically
- Saves if state changed

---

## Answers to Specific Questions

### Q1: How does exec communicate with the guest? (for `top`)
**Answer:** Via **exec Unix socket** at `{box_dir}/sockets/exec.sock`

**Implementation:**
1. Connect to exec socket with `ExecClient::connect()`
2. Send HTTP POST /exec with JSON `ExecRequest`
3. Receive JSON `ExecOutput` with stdout/stderr/exit_code
4. Parse output and display

**For `top` command:**
```rust
let request = ExecRequest {
    cmd: vec!["top".to_string(), "-b".to_string(), "-n".to_string(), "1".to_string()],
    timeout_ns: 5_000_000_000,
    env: vec![],
    working_dir: None,
};
let output = exec_client.exec_command(&request).await?;
println!("{}", String::from_utf8_lossy(&output.stdout));
```

---

### Q2: Are port mappings stored anywhere? (for `port`)
**Answer:** **Partially** - stored in `BoxConfig.port_map` but NOT in `BoxRecord`

**Current State:**
- `BoxConfig.port_map`: Vec<String> of "host_port:guest_port" pairs
- Passed to runtime during VM creation
- **NOT persisted** in `~/.a3s/boxes.json` (BoxRecord)

**For `port` command:**
- **Option 1:** Add `port_map` field to `BoxRecord` (requires state migration)
- **Option 2:** Parse from `BoxConfig` if available (not persisted)
- **Option 3:** Query runtime for active port mappings (if runtime tracks them)

**Recommendation:** Add `port_map: Vec<String>` to `BoxRecord` for persistence.

---

### Q3: Can we send SIGSTOP/SIGCONT to the shim PID? (for `pause/unpause`)
**Answer:** **YES!** ✅ Already supported in `kill` command.

**Implementation:**
```rust
// Pause
unsafe { libc::kill(pid as i32, libc::SIGSTOP); }

// Unpause
unsafe { libc::kill(pid as i32, libc::SIGCONT); }
```

**For `pause/unpause` commands:**
- Use existing `kill` command logic
- Send SIGSTOP to pause, SIGCONT to resume
- **Don't update state** (box remains "running")
- Consider adding `paused: bool` field to BoxRecord for visibility

**Example:**
```bash
a3s-box kill -s STOP my_box   # Pause
a3s-box kill -s CONT my_box   # Unpause
```

---

### Q4: How does the process lifecycle work? (for `wait`)
**Answer:** PID-based lifecycle with state reconciliation

**Process Lifecycle:**
1. **Boot:** `VmManager::boot()` spawns shim process, returns PID
2. **Running:** PID stored in `BoxRecord`, status = "running"
3. **Stop:** Send SIGTERM → SIGKILL, clear PID, status = "stopped"
4. **Dead:** Reconciliation detects dead PID, status = "dead"

**For `wait` command:**
- Poll `is_process_alive(pid)` every 100ms
- When process exits, read exit code (if available)
- Update state to "stopped" or "dead"
- Return exit code to caller

**Exit Code Retrieval:**
- Use `libc::waitpid()` to get exit status
- Or check if process is zombie before reaping

**Example:**
```rust
pub async fn wait_for_box(pid: u32) -> Result<i32> {
    loop {
        if !is_process_alive(pid) {
            // Process exited, try to get exit code
            let mut status: i32 = 0;
            unsafe {
                let result = libc::waitpid(pid as i32, &mut status, libc::WNOHANG);
                if result > 0 {
                    return Ok(libc::WEXITSTATUS(status));
                }
            }
            return Ok(0); // Default if can't get status
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
}
```

---

### Q5: Is there console/PTY access? (for `attach`)
**Answer:** **Limited** - console log file only, no interactive PTY

**Current State:**
- Console output written to `{box_dir}/logs/console.log`
- `logs` command tails this file
- **No interactive PTY** for stdin/stdout/stderr

**For `attach` command:**
- **Option 1:** Implement PTY support in runtime (major change)
- **Option 2:** Use `exec` with interactive shell (workaround)
- **Option 3:** Tail console log + send commands via exec (hybrid)

**Workaround with `exec`:**
```bash
# Pseudo-interactive shell
a3s-box exec -it my_box /bin/sh
```

**Recommendation:** Start with console log tailing, add PTY support later if needed.

---

## Summary Table: Docker CLI Alignment

| Feature | Status | Implementation Notes |
|---------|--------|---------------------|
| `pause` | ✅ **Implemented** | Sends SIGSTOP, updates state to "paused" |
| `unpause` | ✅ **Implemented** | Sends SIGCONT, updates state to "running" |
| `top` | ✅ **Implemented** | Uses `exec` to run `ps aux` in guest |
| `rename` | ✅ **Implemented** | Updates `name` field in BoxRecord with uniqueness validation |
| `--label` | ✅ **Implemented** | Labels on run/create, `--filter label=` on ps, `{{.Labels}}` format |
| `exec -u/--user` | ✅ **Implemented** | User specification via `su` wrapper in guest exec server |
| `pull -q/--quiet` | ✅ **Implemented** | Quiet mode for pull (path only) |
| `cp` directories | ✅ **Implemented** | Recursive directory copy via tar + base64 |
| `--restart` enforcement | ✅ **Implemented** | Reconciliation-based: always, on-failure, unless-stopped policies |
| `--health-cmd` | ✅ **Implemented** | Health check config with interval/timeout/retries/start-period |
| Health status in `ps` | ✅ **Implemented** | Shows `running (healthy)` / `running (unhealthy)` in status column |
| `port` | ⚠️ Pending | Already persists `port_map` in BoxRecord |
| `wait` | ⚠️ Pending | Poll PID + `waitpid()` for exit code |
| `attach` | ❌ Hard | Requires PTY support in runtime |
| `update` | ⚠️ Medium | Update resource limits (requires runtime support) |
| `diff` | ❌ Hard | Requires filesystem layer tracking |
| `commit` | ❌ Hard | Requires OCI image building |
| `export` | ❌ Hard | Requires rootfs export |
| `import` | ⚠️ Medium | Use existing OCI image import |

---

## Next Steps

1. **✅ Phase 1 Complete: Docker CLI Alignment Quick Wins**
   - ✅ `pause` / `unpause` commands
   - ✅ `top` command
   - ✅ `rename` command
   - ✅ `--label` support on run/create, `--filter label=` on ps
   - ✅ `exec -u/--user` for user specification
   - ✅ `pull -q/--quiet` for quiet mode
   - ✅ `cp` directory support (recursive via tar)

2. **✅ Phase 2 Complete: Restart & Health Check**
   - ✅ `--restart` policy enforcement (reconciliation-based: always, on-failure, unless-stopped)
   - ✅ `stopped_by_user` tracking for unless-stopped policy
   - ✅ Health check config (`--health-cmd`, `--health-interval`, `--health-timeout`, `--health-retries`, `--health-start-period`)
   - ✅ Health status display in `ps` output (e.g., "running (healthy)")
   - ✅ `pending_restarts()` API for restart candidates

3. **Phase 3: Interactive & OCI**
   - Interactive PTY (`-it`) for exec and attach
   - `diff` (filesystem layer tracking)
   - `commit` (OCI image building from box state)
   - `export` / `import` (rootfs tarball operations)

---

## Code References

### Key Files
- **State Management:** `cli/src/state.rs`
- **Box Resolution:** `cli/src/resolve.rs`
- **Output Helpers:** `cli/src/output.rs`
- **Exec Client:** `runtime/src/grpc.rs`
- **Exec Types:** `core/src/exec.rs`
- **Config Types:** `core/src/config.rs`
- **VM Manager:** `runtime/src/vm.rs`

### Key Constants
- `AGENT_VSOCK_PORT`: 4088 (guest agent)
- `EXEC_VSOCK_PORT`: 4089 (exec server)
- `DEFAULT_EXEC_TIMEOUT_NS`: 5 seconds
- `MAX_OUTPUT_BYTES`: 16 MiB per stream
- `DEFAULT_SHUTDOWN_TIMEOUT_MS`: 10 seconds

### Key Patterns
- **Async-first:** All I/O uses Tokio
- **Error handling:** Centralized `BoxError` enum
- **State persistence:** Atomic writes to `~/.a3s/boxes.json`
- **Signal handling:** Direct `libc::kill()` calls
- **Guest communication:** HTTP over Unix sockets

---

### `build` - Build an Image from a Dockerfile
**File:** `cli/src/commands/build.rs`

**Key Arguments:**
- `path`: Build context directory (default: ".")
- `-t, --tag`: Image name and tag (e.g., "myimage:latest")
- `-f, --file`: Dockerfile path (default: `<PATH>/Dockerfile`)
- `--build-arg`: Build-time variables (KEY=VALUE), repeatable
- `-q, --quiet`: Suppress build output

**Build Engine:** `runtime/src/oci/build/engine.rs`

**Supported Dockerfile Instructions:**
- `FROM` — Pull base image from registry
- `RUN` — Execute commands (Linux only via chroot; skipped on macOS)
- `COPY` — Copy files from build context into image
- `WORKDIR` — Set working directory
- `ENV` — Set environment variables
- `ENTRYPOINT` — Set entrypoint (exec and shell form)
- `CMD` — Set default command (exec and shell form)
- `EXPOSE` — Declare ports
- `LABEL` — Set metadata labels
- `USER` — Set user
- `ARG` — Build-time variables with optional defaults

**Process:**
1. Parse Dockerfile (`runtime/src/oci/build/dockerfile.rs`)
2. Pull base image via `ImagePuller`
3. Extract base layers into temp rootfs
4. Execute each instruction, creating layers for COPY/RUN
5. Assemble OCI image (config, manifest, index.json)
6. Store in local image store via `ImageStore::put()`

**Output:** Standard OCI image layout, usable with `run`, `save`, `image-inspect`, `history`

---

**Document Version:** 1.1
**Last Updated:** 2025-07-14
**Author:** AI Assistant (Claude)
