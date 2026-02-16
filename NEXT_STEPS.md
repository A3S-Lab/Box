# Box Transport Migration Plan

## Goal
Migrate Box guest servers from ad-hoc protocols to `a3s-transport` Frame protocol.

## Step 1: Add a3s-transport dependency to Box workspace
- Add `a3s-transport` as path dependency in workspace Cargo.toml
- Add it to `guest/init` and `core` crate dependencies

## Step 2: Migrate Exec Server (guest + host)
**Guest side** (`guest/init/src/exec_server.rs`):
- Replace HTTP/1.1 parsing with Frame-based read/write
- Use `a3s_transport::frame::{Frame, FrameType}` for wire format
- Keep `execute_command()` logic unchanged

**Host side** (`runtime/src/grpc.rs` ExecClient):
- Replace HTTP request building with Frame-based send/recv
- Use `a3s_transport::codec::FrameWriter/FrameReader` over UnixStream

## Step 3: Migrate PTY protocol (core/src/pty.rs)
- Replace custom `write_frame`/`read_frame` with re-exports from `a3s-transport`
- PTY frame types (0x01-0x05) already match transport Frame types
- Keep `PtyRequest`, `PtyResize`, `PtyExit` types in core (domain-specific)

## Priority
Step 2 first — exec server migration eliminates HTTP parsing overhead and simplifies both sides significantly.
