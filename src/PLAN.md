# Phase 1: Docker CLI Alignment — Quick Wins

## Overview
Add commonly-expected Docker flags to a3s-box CLI commands.

## Changes

### 1. `run.rs` + `create.rs` — Add `--entrypoint`, `--workdir`, `--user`, `--hostname`, `--label`

**CLI flags (both run and create):**
- `--entrypoint <CMD>` — Override image ENTRYPOINT
- `-w/--workdir <DIR>` — Override working directory
- `-u/--user <USER>` — Run as specific user (uid, uid:gid, user, user:group)
- `-h/--hostname <NAME>` — Set hostname inside the VM
- `-l/--label <KEY=VALUE>` — Add metadata label (repeatable)

**Core changes needed:**
- `BoxConfig` (core/config.rs): Add `entrypoint_override`, `workdir_override`, `user_override`, `hostname` fields
- `InstanceSpec` (runtime/vmm/spec.rs): Add `hostname` field
- `vm.rs` `build_instance_spec()`: Use overrides when set
- `BoxRecord` (cli/state.rs): Add `labels` and `hostname` fields
- `krun/context.rs`: Pass hostname to libkrun if supported

**Note:** `--entrypoint` in Docker replaces the ENTRYPOINT entirely (not CMD). Our `--cmd` (positional args in `run`) replaces CMD. This matches Docker semantics: `docker run --entrypoint /bin/sh image -c "echo hi"`.

### 2. `pull.rs` — Add `-q/--quiet`

Suppress progress output, only print the image digest on success.

### 3. `logs.rs` — Add `-t/--timestamps`

Prefix each log line with its timestamp. Since we read from a plain file, we'll prefix with the current read time (similar to `docker logs -t` for non-json-log drivers).

### 4. `exec.rs` — Add `-u/--user`

- `ExecRequest` (core/exec.rs): Add `user: Option<String>` field
- Guest exec server would need to support this (setuid before exec)
- CLI just passes it through

## Files to Modify

| File | Change |
|------|--------|
| `core/src/config.rs` | Add override fields to `BoxConfig` |
| `core/src/exec.rs` | Add `user` field to `ExecRequest` |
| `runtime/src/vmm/spec.rs` | Add `hostname` to `InstanceSpec` |
| `runtime/src/vm.rs` | Apply overrides in `build_instance_spec()` |
| `cli/src/commands/run.rs` | Add 5 new flags, wire to BoxConfig |
| `cli/src/commands/create.rs` | Add 5 new flags, wire to BoxRecord |
| `cli/src/commands/pull.rs` | Add `-q` flag |
| `cli/src/commands/logs.rs` | Add `-t` flag |
| `cli/src/commands/exec.rs` | Add `-u` flag |
| `cli/src/state.rs` | Add `labels`, `hostname` to BoxRecord |

## Verification
- `cargo build -p a3s-box-cli` succeeds
- `cargo clippy -p a3s-box-cli` — 0 warnings
- `cargo test -p a3s-box-cli -p a3s-box-runtime -p a3s-box-core` — all pass
