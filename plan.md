# Plan: OCI Image Format & Label-Based Configuration

## Context

A3S Box already has:
- `AgentLabels` struct in `labels.rs` that parses `a3s.box.*` labels from OCI images
- `OciImageConfig` with a `labels` field populated from OCI image config
- CLI `run` command with `--cpus`, `--memory`, `-e`, `-v` flags

**The gap:** Labels are parsed but **never applied**. The `AgentLabels` struct exists but is never used in the boot flow. The CLI always uses hardcoded defaults, ignoring image labels entirely.

## Goals

1. **Apply OCI labels to BoxConfig** — image labels configure the box, CLI flags override
2. **Extend label schema** — add resource labels (vcpus, memory, disk, timeout)
3. **Define Boxfile format** — simple declarative file for building Box images
4. **Add `a3s-box build` command** — build OCI images from a Boxfile

## Priority Order

We'll implement in this order (each step is independently useful):

1. **Label → Config integration** (most impactful, closes the existing gap)
2. **Extended label schema** (resource labels)
3. **Boxfile format definition** (spec + parser)
4. **`build` command** (uses Boxfile to produce OCI image directory)

---

## Step 1: Apply OCI Labels to BoxConfig

### Problem
`vm.rs` reads `OciImageConfig` for entrypoint/env but never reads `AgentLabels`. The CLI `run.rs` creates `BoxConfig` with hardcoded defaults.

### Solution
In `vm.rs::build_instance_spec()`, after resolving the OCI config, parse labels via `AgentLabels` and apply them to the entrypoint environment. The labels should flow as:

```
OCI Image Labels → AgentLabels → Entrypoint env vars (for guest agent)
```

Specifically:
- `a3s.box.llm.provider` → `A3S_AGENT_ENV_LLM_PROVIDER`
- `a3s.box.llm.model` → `A3S_AGENT_ENV_LLM_MODEL`
- `a3s.box.llm.api_key` → `A3S_AGENT_ENV_LLM_API_KEY`
- `a3s.box.llm.base_url` → `A3S_AGENT_ENV_LLM_BASE_URL`
- `a3s.box.env.*` → already handled via `oci_config.env`

### Files to Modify
- `runtime/src/vm.rs` — In `build_instance_spec()`, parse `AgentLabels` from `oci_config.labels` and inject LLM config as env vars
- `runtime/src/oci/labels.rs` — Add `to_env_vars()` method that converts labels to env var pairs

### Tests
- Test `AgentLabels::to_env_vars()` produces correct env pairs
- Test that LLM labels are injected into entrypoint env in `build_instance_spec`

---

## Step 2: Extended Label Schema (Resource Labels)

### New Labels
```dockerfile
# Resource configuration
LABEL a3s.box.vcpus="4"
LABEL a3s.box.memory="2048"       # MB
LABEL a3s.box.disk="8192"         # MB
LABEL a3s.box.timeout="7200"      # seconds

# Runtime hints
LABEL a3s.box.workspace="/app"
LABEL a3s.box.skills="/skills"
```

### Solution
- Extend `AgentLabels` with resource fields
- Add `AgentLabels::to_resource_config()` → `ResourceConfig`
- In CLI `run.rs`, after pulling the image, read labels and use them as defaults (CLI flags override)

### Priority: CLI flags > `-e` env vars > OCI labels > BoxConfig defaults

### Files to Modify
- `runtime/src/oci/labels.rs` — Add resource fields + `to_resource_config()`
- `cli/src/commands/run.rs` — Read labels from pulled image, apply as defaults
- `cli/src/commands/create.rs` — Same as run.rs

### Tests
- Test resource label parsing
- Test CLI flag override of label values

---

## Step 3: Boxfile Format Definition

### Format
A simple TOML-based declarative file (not Dockerfile — we're not building layers, we're configuring an existing OCI image):

```toml
# Boxfile
[image]
from = "alpine:latest"          # Base OCI image

[agent]
type = "code"                   # Agent type
version = "0.1.0"
binary = "/usr/bin/a3s-code"    # Path to agent binary in image

[resources]
vcpus = 2
memory = "1g"
disk = "4g"
timeout = 3600

[llm]
provider = "anthropic"
model = "claude-sonnet-4-20250514"

[env]
RUST_LOG = "info"
CUSTOM_VAR = "value"

[workspace]
path = "/a3s/workspace"

[[volumes]]
host = "./data"
guest = "/data"
readonly = false
```

### Implementation
- New module: `runtime/src/boxfile.rs` — Parse Boxfile into `BoxfileConfig` struct
- `BoxfileConfig` can produce `AgentLabels` (for OCI label generation) and `BoxConfig` (for direct use)

### Files to Create
- `runtime/src/boxfile.rs` — Boxfile parser + types

### Files to Modify
- `runtime/src/lib.rs` — Add `boxfile` module export
- `runtime/Cargo.toml` — Add `toml` dependency

### Tests
- Parse valid Boxfile
- Parse minimal Boxfile (only `[image]` section)
- Error on missing `[image].from`
- Round-trip: Boxfile → labels → Boxfile

---

## Step 4: `a3s-box build` Command

### Usage
```bash
# Build from Boxfile in current directory
a3s-box build .

# Build from specific Boxfile
a3s-box build -f Boxfile.toml .

# Build with tag
a3s-box build -t myapp:latest .
```

### What it does
1. Read Boxfile
2. Pull base image (`[image].from`)
3. Copy the base image to a new OCI directory
4. Inject `a3s.box.*` labels into the OCI config
5. Store in local image cache with the given tag

### Files to Create
- `cli/src/commands/build.rs` — Build command implementation

### Files to Modify
- `cli/src/main.rs` or `cli/src/lib.rs` — Register `build` subcommand
- `runtime/src/oci/image.rs` — Add method to write/modify OCI config labels

### Tests
- Build from minimal Boxfile
- Build with all sections
- Error on missing base image

---

## Implementation Order

| Step | Scope | Lines (est.) | Dependencies |
|------|-------|-------------|--------------|
| 1 | Label → Config integration | ~80 | None |
| 2 | Extended label schema | ~120 | Step 1 |
| 3 | Boxfile format | ~250 | None (independent) |
| 4 | Build command | ~200 | Steps 2 + 3 |

**Total: ~650 lines of new code**

## Verification

After each step:
```bash
cd /Users/roylin/Desktop/ai-lab/a3s/crates/box/src
cargo check --workspace
cargo test --workspace
```
