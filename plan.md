# Plan: `a3s-box build` — Dockerfile-based Image Building

## Goal

Implement `a3s-box build` command that parses a Dockerfile, executes instructions to produce an OCI image, and stores it in the local image store. This is the Docker `build` equivalent for a3s-box.

## Design Decisions

### Scope: Subset of Dockerfile Instructions

We do NOT need a full Docker BuildKit reimplementation. a3s-box targets AI agent sandboxing, so we support the most commonly used Dockerfile instructions:

| Instruction | Priority | Notes |
|---|---|---|
| `FROM` | P0 | Base image (pulls from registry) |
| `RUN` | P0 | Execute commands in build container |
| `COPY` | P0 | Copy files from build context |
| `WORKDIR` | P0 | Set working directory |
| `ENV` | P0 | Set environment variables |
| `ENTRYPOINT` | P0 | Set entrypoint |
| `CMD` | P0 | Set default command |
| `EXPOSE` | P0 | Declare ports |
| `LABEL` | P0 | Set metadata labels |
| `USER` | P0 | Set user |
| `ARG` | P1 | Build-time variables |

**Phase 1 (this PR):** FROM, COPY, RUN, WORKDIR, ENV, ENTRYPOINT, CMD, EXPOSE, LABEL, USER, ARG

### Architecture

```
crates/box/src/
├── runtime/src/
│   └── oci/
│       ├── build/              # NEW: Build engine
│       │   ├── mod.rs          # Public API + re-exports
│       │   ├── dockerfile.rs   # Dockerfile parser
│       │   ├── engine.rs       # Build engine (executes instructions)
│       │   └── layer.rs        # Layer creation (tar.gz from diff)
│       └── mod.rs              # Add `pub mod build;`
├── cli/src/
│   └── commands/
│       ├── build.rs            # NEW: CLI command
│       └── mod.rs              # Register build command
```

### How `RUN` Works Without Docker

For Phase 1, we use **host-side execution**:

- `RUN` on Linux: execute via `chroot <rootfs> sh -c "cmd"` (requires root or unshare)
- `RUN` on macOS: **skip execution, emit warning** (base images are Linux; can't chroot into Linux rootfs on macOS)
- `COPY`, `WORKDIR`, `ENV`, `CMD`, `ENTRYPOINT`, `LABEL`, `EXPOSE`, `USER`: work everywhere (filesystem ops + metadata)

This means on macOS, `a3s-box build` works for Dockerfiles that only use COPY/ENV/CMD/ENTRYPOINT/LABEL (common for interpreted languages). Full RUN support requires Linux.

### Layer Strategy

Each `RUN` and `COPY` instruction creates a new layer:
1. Snapshot the rootfs state (file list + mtimes)
2. Execute the instruction
3. Diff the filesystem to find changed/added/deleted files
4. Create a tar.gz layer from the diff
5. Update the OCI config with the new layer's diff_id

### Output

The build produces a standard OCI image layout stored in the local image store, identical to what `a3s-box pull` produces.

## Implementation Steps

### Step 1: Dockerfile Parser (`runtime/src/oci/build/dockerfile.rs`)
### Step 2: Layer Creator (`runtime/src/oci/build/layer.rs`)
### Step 3: Build Engine (`runtime/src/oci/build/engine.rs`)
### Step 4: CLI Command (`cli/src/commands/build.rs`)
### Step 5: Wire up modules, error types, re-exports
### Step 6: Documentation & Tests
