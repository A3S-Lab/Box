# Plan: 9.5 Dockerfile Completion (P2)

## Context

The build engine (`runtime/src/oci/build/`) supports FROM, RUN, COPY, WORKDIR, ENV, ENTRYPOINT, CMD, EXPOSE, LABEL, USER, ARG. The remaining instructions (ADD, HEALTHCHECK, SHELL, STOPSIGNAL, ONBUILD) are currently parsed as skipped labels with a warning. Multi-stage builds parse `FROM ... AS alias` but don't execute stages.

Health checks are already implemented at the CLI/runtime level (`--health-cmd`, `HealthCheck` struct in state.rs). We need to wire HEALTHCHECK from Dockerfile → OciImageConfig → runtime.

## Scope

1. **ADD instruction** — Like COPY but supports URL download and auto-extract tar archives
2. **HEALTHCHECK instruction** — Parse into OciImageConfig, apply at runtime
3. **SHELL instruction** — Override default shell for RUN instructions
4. **STOPSIGNAL instruction** — Set stop signal in OciImageConfig, use in stop/kill
5. **ONBUILD instruction** — Store triggers, execute when image is used as base
6. **Multi-stage builds** — Execute multiple FROM stages, support `COPY --from=stage`

Image signing (cosign/notation) remains deferred.

---

## Feature 1: SHELL Instruction

**Simplest — changes how RUN commands are wrapped.**

### 1.1 Add `Shell` variant to `Instruction` enum

```rust
/// `SHELL ["executable", "param1", ...]`
Shell { exec: Vec<String> },
```

### 1.2 Parse SHELL in `dockerfile.rs`

Parse JSON array form: `SHELL ["/bin/bash", "-c"]`

### 1.3 Add `shell` to `BuildState`

```rust
shell: Vec<String>,  // default: ["/bin/sh", "-c"]
```

### 1.4 Use shell in RUN execution

When executing `RUN command`, wrap as `shell[0] shell[1..] command` instead of hardcoded `/bin/sh -c`.

### Files
- `runtime/src/oci/build/dockerfile.rs` — Parse SHELL
- `runtime/src/oci/build/engine.rs` — Track shell, use in RUN

---

## Feature 2: STOPSIGNAL Instruction

### 2.1 Add `StopSignal` variant to `Instruction` enum

```rust
/// `STOPSIGNAL <signal>`
StopSignal { signal: String },
```

### 2.2 Parse STOPSIGNAL in `dockerfile.rs`

Simple: `STOPSIGNAL SIGTERM` or `STOPSIGNAL 15`

### 2.3 Add `stop_signal` to `OciImageConfig`

```rust
pub stop_signal: Option<String>,
```

Parse from OCI config JSON (`StopSignal` field).

### 2.4 Add `stop_signal` to `BuildState` and emit in config

### 2.5 Use stop_signal in CLI stop/kill commands

When stopping a box, use the configured signal instead of default SIGTERM.

### Files
- `runtime/src/oci/build/dockerfile.rs` — Parse STOPSIGNAL
- `runtime/src/oci/build/engine.rs` — Track stop_signal, emit in config
- `runtime/src/oci/image.rs` — Add `stop_signal` to OciImageConfig
- `cli/src/commands/stop.rs` — Use stop_signal from OciImageConfig

---

## Feature 3: HEALTHCHECK Instruction

### 3.1 Add `HealthCheck` variant to `Instruction` enum

```rust
/// `HEALTHCHECK [OPTIONS] CMD command` or `HEALTHCHECK NONE`
HealthCheck {
    cmd: Option<String>,       // None = HEALTHCHECK NONE (disable)
    interval: Option<u64>,     // --interval=30s
    timeout: Option<u64>,      // --timeout=30s
    retries: Option<u32>,      // --retries=3
    start_period: Option<u64>, // --start-period=0s
},
```

### 3.2 Parse HEALTHCHECK in `dockerfile.rs`

Parse options (`--interval`, `--timeout`, `--retries`, `--start-period`) and CMD.

### 3.3 Add `health_check` to `OciImageConfig`

```rust
pub health_check: Option<OciHealthCheck>,
```

Parse from OCI config JSON (`Healthcheck` field).

### 3.4 Wire to runtime

In `vm.rs` or CLI `run.rs`, if no `--health-cmd` is specified but OciImageConfig has a health_check, use it as default.

### Files
- `runtime/src/oci/build/dockerfile.rs` — Parse HEALTHCHECK
- `runtime/src/oci/build/engine.rs` — Track health_check, emit in config
- `runtime/src/oci/image.rs` — Add health_check to OciImageConfig, parse from OCI JSON
- `cli/src/commands/run.rs` — Fall back to OCI health_check if no --health-cmd

---

## Feature 4: ADD Instruction

### 4.1 Add `Add` variant to `Instruction` enum

```rust
/// `ADD [--chown=<user>] <src>... <dst>`
Add {
    src: Vec<String>,
    dst: String,
    chown: Option<String>,
},
```

### 4.2 Parse ADD in `dockerfile.rs`

Similar to COPY but also accepts URLs.

### 4.3 Implement ADD in build engine

- If src is a URL (starts with `http://` or `https://`): download to dst
- If src is a local tar archive (`.tar`, `.tar.gz`, `.tgz`, `.tar.bz2`, `.tar.xz`): extract to dst
- Otherwise: same as COPY

### Files
- `runtime/src/oci/build/dockerfile.rs` — Parse ADD
- `runtime/src/oci/build/engine.rs` — Execute ADD (download URLs, extract tars, copy files)

---

## Feature 5: ONBUILD Instruction

### 5.1 Add `OnBuild` variant to `Instruction` enum

```rust
/// `ONBUILD <instruction>`
OnBuild { instruction: Box<Instruction> },
```

### 5.2 Parse ONBUILD in `dockerfile.rs`

Parse the inner instruction recursively: `ONBUILD RUN echo hello`

### 5.3 Store ONBUILD triggers in OCI config

Add `onbuild: Vec<String>` to OciImageConfig. Store raw instruction strings.

### 5.4 Execute ONBUILD triggers in build engine

When processing FROM, check if the base image has ONBUILD triggers. If so, parse and execute them before continuing with the current Dockerfile.

### Files
- `runtime/src/oci/build/dockerfile.rs` — Parse ONBUILD
- `runtime/src/oci/build/engine.rs` — Store triggers, execute on FROM
- `runtime/src/oci/image.rs` — Add `onbuild` to OciImageConfig

---

## Feature 6: Multi-stage Builds

### 6.1 Refactor build engine to support stages

Split the instruction list by FROM. Each FROM starts a new stage. Track stages by index and alias.

### 6.2 Implement `COPY --from=<stage>`

Already parsed (`from: Option<String>` in Copy). Need to resolve stage alias → stage rootfs path, then copy from that rootfs instead of the build context.

### 6.3 Only the final stage produces the output image

Earlier stages are intermediate — their rootfs is kept for COPY --from but not included in the final image.

### Files
- `runtime/src/oci/build/engine.rs` — Stage tracking, COPY --from resolution, final stage output

---

## Implementation Order

1. **SHELL** — Simplest, self-contained change to build engine
2. **STOPSIGNAL** — Simple string field, small wiring
3. **HEALTHCHECK** — Moderate complexity, reuses existing HealthCheck infra
4. **ADD** — Extends COPY with URL download + tar extraction
5. **ONBUILD** — Recursive parsing + trigger execution
6. **Multi-stage builds** — Most complex, refactors build engine

## Documentation Updates

- `crates/box/README.md`: Mark items ✅ in Phase 9.5 roadmap, update features
- `README.md`: Update P2 Dockerfile Completion status
- Update test counts

## Verification

```bash
cd crates/box/src && cargo test --lib
cargo test -p a3s-box-cli --lib
# Expect all tests pass
```
