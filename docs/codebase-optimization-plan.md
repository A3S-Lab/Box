# Codebase Optimization Plan — Structure and Supply-Chain Domains

Status: **active** (2026-09-28). Phase 0, Phase 1, and Phase 2 S-4 and S-5 are
**executed on the working tree**, verified as recorded in §7. S-6 stays on the
HVF cutover train; Phase 3 stays behind the B5 gate.

This plan extends the axiom in
[architecture-optimization-plan.md](architecture-optimization-plan.md) §1.4 —
*"optimization means reducing false architecture, not adding surface"* — to
two domains that the execution-focused axes A–E do not cover:

- **Structure domain (S-x)**: code that collects rent without serving the
  mission — dead subsystems, single-implementation "extension points",
  business logic in the presentation layer, a split error model, oversized
  files, tracked build artifacts.
- **Supply-chain domain (C-x)**: false architecture across repositories — a
  forked shared FFI crate, the migration-period dual MicroVM implementation,
  manual multi-repo release orchestration.

Rule 0 was applied to this plan itself: every item below names the evidence
that proves the problem is real, not hypothetical.

---

## 1. Relationship to existing plans

| Document | Role vs this plan |
| --- | --- |
| [architecture-optimization-plan.md](architecture-optimization-plan.md) | Authoritative for the execution domain (Axes A–E) and all B-gates. This plan only does **precondition and structure work** for it; it never reorders or rewrites an axis. |
| ROADMAP.md | Milestone authority. On any conflict, ROADMAP open checkboxes win (architecture plan §8). |

PRs under this plan cite `S-x` / `C-x` IDs **plus** the affected
architecture-plan §4.2/§4.3 rows when a cutover-adjacent file is touched.

Axiom references below map to architecture plan §1: AX1 = §1.1 (isolation is
an explicit product choice), AX2 = §1.2 (two planes, one ownership rule),
AX3 = §1.3 (fail closed beats invent-Ok), AX4 = §1.4 (reduce false
architecture).

---

## 2. Phase 0 — Hygiene week (zero behavior change) — **EXECUTED**

| ID | Item | Evidence | Axiom | Acceptance | Status |
| --- | --- | --- | --- | --- | --- |
| P0.1a | Remove stray tracked binaries `check_spawn_sig`, `move_test` | Mach-O arm64 executables at repo root; zero references in the whole tree | AX4 | `git ls-files` clean; CI guard added | **Done** — `git rm`; guard `scripts/check_no_tracked_artifacts.py` wired into `ci.yml` fmt job |
| P0.1b | Remove tracked archives `libkrun-source.tar`, `krun-windows-x64.tar.xz` | — | AX4 | — | **Deferred to C-1**: both are build-time inputs extracted by `src/deps/libkrun-sys/build.rs:374/1090`; deleting them breaks builds. They die with the C-1 convergence, not with hygiene. The guard allowlists exactly these two paths until then |
| P0.2 | Align workspace-internal dep requirements `3.2` → `3.3` | `netproxy/Cargo.toml`, `runtime/Cargo.toml`, `sdk/Cargo.toml` | AX4 | `grep '"3.2"' */Cargo.toml` empty | **Done** |
| P0.3 | Archive stale v2 announcements in `docs/`; disambiguate the two `shim` names | — | — | — | **Rejected (anti-overfit)**: hypothetical problem; renaming/moving churns paths and links for zero mission value. Revisit only if a real consumer is confused |

## 3. Phase 1 — Deletion (behavior-preserving) — **EXECUTED**

| ID | Item | Evidence | Axiom | Acceptance | Status |
| --- | --- | --- | --- | --- | --- |
| S-1 (P1.1) | Delete dead `BoxAutoscaler` operator subsystem: `core/src/operator.rs` (453 lines), `runtime/src/operator.rs`, the `operator` feature, re-exports | `BoxAutoscaler` referenced nowhere outside its own definitions across `.rs`, `.proto`, `.yaml`, `.ts`, `.py`, `.go` | AX4 | zero references; tests removed with the code (TDD rule) | **Done** |
| S-2 (P1.2) | Delete single-implementation, zero-consumer traits: `AuditSink`, `CredentialProvider`, `EventBus`, `ImageRegistry`+`PulledImage`, `MetricsCollector`+`NoopMetrics`, and the four `traits::store` backend traits | Each had exactly 1 impl and 0 external consumers (impl blocks were pure delegation over inherent methods); zero `dyn` usage | AX4 | `core/src/traits/` keeps only load-bearing seams (`ExecutionManager`, `ExecutionSessionManager`, `CacheBackend` — two real implementations — and the `StoredImage` type) | **Done** — concrete types (`AuditLog`, `CredentialStore`, `ImagePuller`, `RuntimeMetrics`, `ImageStore`, `NetworkStore`, `SnapshotStore`, `VolumeStore`) keep inherent APIs; the metric counters they delegate to are incremented directly by `vm/boot.rs`, `vm/execution.rs`, `vm/layout.rs`, so no metrics surface changed |
| S-3 (P1.3) | Move `windows_file`/`windows_symlink` under `platform/` | — | — | — | **Rejected for now**: compile-time `#[cfg(windows)]` already gates them; a move is churn without a consumer. Revisit when `core`'s platform layer is next touched for real work |

Net Phase 0+1 diff: 26 files, +64/−1669 lines, plus the new CI guard script.

## 4. Phase 2 — Consolidation (S-4 and S-5 executed; S-6 not started)

| ID | Item | Hook | Axiom | Acceptance | Status |
| --- | --- | --- | --- | --- | --- |
| S-4 (P2.1) | Sink `PoolRegistry`/lease/guard logic from `cli/src/commands/pool.rs` into `runtime/src/pool/` | B4 warm-pool unification; cite Axis E | AX2 (presentation owns no domain) | `pool.rs` is presentation only; behavior pinned by the existing pool tests | **Done** — registry, socket accept, and daemon startup live in `runtime/src/pool/{registry,serve,daemon}.rs`. CLI `pool.rs` is clap, autostart, and stdout (555 lines). Lease/registry/protocol tests moved with the code |
| S-5 (P2.2) | Converge CLI error model on `BoxError`; `Box<dyn Error>` only at `fn main` | — | AX3 (typed fail-closed is matchable) | 77 files → 0 outside `main`; variant-match tests | **Done.** Image commands, `login`/`logout`, `volume`, `network`, and lifecycle (`start`/`stop`/`restart`/`rm`/`kill`/`pause`/`unpause`/`wait`) return `BoxError`. Illegal lifecycle state is `StateError`. Unknown signals and a zero wait timeout are `ConfigError`. A wait deadline is `TimeoutError`. Signal delivery failure is `ExecError`. Compose project loading, `ps`/`config`/`logs`, dependency waits, health preflight, image prefetch, and Secret projection return `BoxError`: a missing or invalid Compose file, unknown service, invalid restart policy, invalid image reference, rejected Secret reference, and non-Linux Secret projection are `ConfigError`; a missing project box, duplicate service boxes, and a Secret store or pull-task failure are `StateError`; an unreadable Compose file or environment file is `IoError`; a health or completion deadline is `TimeoutError`; a pull failure is `OciImageError`. `compose up`/`down`, rollback, and sandbox preflight also return `BoxError`: a failed network create or connect is `NetworkError`, a failed service start or rollback is `StateError`, a sandbox feature refusal is `ConfigError`, and a directory create failure is `IoError`. The Compose command family itself no longer returns `Box<dyn Error>`. `top`, `exec`, and `cp` return `BoxError`. A missing box, a container that disappeared during refresh, a box that is not running, and an OCI copy target that is neither running nor paused are `StateError`. An invalid exec user, workdir, or request id, and a `cp` between two host paths or two boxes, are `ConfigError`. A guest copy failure is `ExecError`. Host file and `tar` I/O stays `IoError`. A retryable unavailable exec or copy stays `StateError` and still names the request id to reuse. `attach` returns `BoxError`: Windows interactive attach is `ConfigError`, and a missing console log, a box that is not running, or a managed generation that is no longer running is `StateError`. `shell` returns `BoxError`: Windows `shell` and an invalid shell user or workdir are `ConfigError`; a missing box is `StateError`. `logs` returns `BoxError`: an invalid `--since` or `--until` and a box with logging disabled are `ConfigError`; a missing box or a managed generation that changed while opening logs is `StateError`; reading the log file stays `IoError`. `inspect` returns `BoxError`: a missing container or image, a container that disappeared during refresh, and an ambiguous name are `StateError`. `diff` returns `BoxError`: a missing baseline, a box removed while its lifecycle lock is held, a host process that is not live, a paused MicroVM, and a missing rootfs are `StateError`. An empty guest archive or an unsafe archive path is `ExecError`. Live diff on a platform without that capture path is `ConfigError`. Reading the baseline file stays `IoError`; a baseline that does not parse is `SerializationError`. `snapshot` returns `BoxError`: pruning without `--keep` or `--max-bytes`, an invalid live snapshot name, and a Windows snapshot that defines a health check are `ConfigError`. An active MicroVM, a box removed while the lifecycle lock is held, a missing rootfs or image configuration, a snapshot that is still in use, and a missing or ambiguous snapshot are `StateError`. `export` returns `BoxError`: a paused MicroVM, a box removed while its lifecycle lock is held, a host process that is not live, a missing guest archive endpoint, and a missing rootfs are `StateError`. An empty guest archive is `ExecError`. Live export on a platform without that capture path is `ConfigError`. Creating the output archive stays `IoError`. `commit` returns `BoxError`: Windows live commit and a Sandbox host-rootfs commit that requires Linux are `ConfigError`. A paused MicroVM, a box removed while its lifecycle lock is held, a host process that is not live, a missing rootfs or guest metadata, and a rootfs that changed during capture are `StateError`. An empty guest archive, an unsafe metadata path, and a duplicate guest path are `ExecError`. Creating the temporary image directory stays `IoError`. `run` returns `BoxError`. Detached TTY, invalid memory or log-driver options, an oversized `--timeout`, and Windows interactive PTY are `ConfigError`. A managed run that disappeared after startup, and a failed start whose rollback also fails, are `StateError`. A sandbox log drain that misses its deadline is `TimeoutError`. `create` returns `BoxError`. Invalid runtime options, restart policy, memory, port maps, labels, and shm size are `ConfigError`. A missing create network is `NetworkError`. Shared boot returns `BoxError`: an illegal lifecycle state is `StateError`, a missing boot network is `NetworkError`, and invalid persisted boot configuration is `ConfigError`. `pool` returns `BoxError`. An invalid pool size, maximum, boot concurrency, warm count, or memory, and Windows pool commands, are `ConfigError`. A warm-pool daemon that fails to spawn or exits early is `PoolError`. An autostart deadline is `TimeoutError`. `build` returns `BoxError`. An invalid context path, a context that is not a directory, a bad build arg or platform, a zero or oversized `--run-pool-timeout`, and a missing Dockerfile are `ConfigError`. `monitor` returns `BoxError`. Windows `monitor --install` and `--uninstall`, and an active health check on Windows, are `ConfigError`. Lost managed lifecycle metadata, a changed generation, and an illegal restart state are `StateError`. `monitor_metrics::serve` returns `BoxError`; a bind failure is `IoError`. `container-update` returns `BoxError`. Invalid memory, reservation, swap, restart policy, cpuset, and Windows live Tier 2 updates are `ConfigError`. A box removed during the update, a changed execution, and a running Tier 1 resize are `StateError`. A legacy guest cgroup failure is `ExecError`. `prune` and `system prune` return `BoxError`. Cleanup failures keep the callee variant and record a refused success. Removing a pruned box from state is `StateError`. Unused-image removal failure is `OciImageError`. Unused-network removal failure is `NetworkError`. Rootfs cache pruning keeps `CacheError`. `ps`, `df`, `port`, `version`, and `info` return `BoxError`. `ps` JSON encoding stays `SerializationError`. A missing `port` container is `StateError`, and a corrupt persisted port mapping is `ConfigError`. `info` keeps the inventory and image-store variants while refusing an invented empty inventory. `stats` returns `BoxError`. Stats for a box that is not running or paused are `StateError`. A missing sandbox is `StateError`. A guest stats failure is `ExecError`. SDK runtime errors keep their `BoxError` variant. `events` returns `BoxError`. Inventory refresh keeps the callee `BoxError`. JSON encoding stays `SerializationError`. Unparseable `--since`/`--until` stay ignored (`parse_time_arg` returns None). `rename` returns `BoxError`. A name already in use is `StateError`. A missing box is `StateError`. `audit` returns `BoxError`. An unknown action or outcome is `ConfigError`. JSON encoding stays `SerializationError`. `seal`, `unseal`, and `inject-secret` return `BoxError`. An invalid sealing policy, a missing secret, and an invalid secret format are `ConfigError`. A missing box or runtime socket is `StateError`. Reading a seal input or secrets file is `IoError`. TEE client failures keep the runtime `BoxError` variant. Windows refuses these commands as `ConfigError`. `sdk-bridge` returns `BoxError`; a stdin failure is `IoError` and JSON encoding is `SerializationError`. `attest` returns `BoxError`. An invalid nonce and an unparseable attestation policy are `ConfigError`. A missing box or runtime socket is `StateError`. Reading a policy file is `IoError`. TEE verification keeps the runtime `BoxError` variant. Windows refuses `attest` as `ConfigError`. `scale-api` returns `BoxError`. A missing advertise host, a non-ACL catalog, and an invalid endpoint host are `ConfigError`. A catalog read failure is `IoError`. Scale authority conflict and state failures are `StateError`. Serving the API is `IoError`. `port-forward` returns `BoxError`. A non-Linux host is `ConfigError`. A box that is not running, not managed, or not a Sandbox is `StateError`. Binding, accepting, and relaying are `IoError`. A closed connection limiter is `StateError`. Shared seams return `BoxError` directly: an unreadable env file is `IoError`, invalid memory reservation, memory swap, or cpuset is `ConfigError`, an ambiguous stored image is `StateError`, an offline rootfs that is paused or still live is `StateError`, an empty guest archive is `ExecError`, Windows health checks are `ConfigError`, a pool daemon that fails to spawn or whose task fails is `PoolError`, and an invalid warm count is `ConfigError`. The local execution manager is mapped with `execution_error`. `dispatch` returns `BoxError`; only `main` prints it. CLI sources contain no `Box<dyn Error>`. Guest and CRI are not in this change |
| S-6 (P2.3) | Split the oversized file the HVF cutover must edit: `runtime/src/local_execution/oci_backend.rs` (3,068) | **Same train as the Axis B HVF cutover** — split the file the cutover must edit, do not split twice | AX4 | that file is split in the cutover PR; all tests green | Not started. `exec_server.rs` and `cri` `runtime_service/mod.rs` are not on this train: guest-init is removed at B5, and CRI waits for Axis E |

S-4 is done, so `pool.rs` is not split again. S-6 for `oci_backend.rs` pairs
with the HVF cutover PR that touches it.

## 5. Phase 3 — Supply chain (planned; B5 preconditions)

| ID | Item | Hook | Axiom | Acceptance |
| --- | --- | --- | --- | --- |
| C-1 (P3.1) | Converge `a3s-libkrun-sys` to a single source: Box stops in-tree vendoring (currently 3.3.0 in-tree vs `=3.1.0` pinned by OCI-Runtime), publishes, OCI-Runtime bumps to the same line; vendored dir + tarballs removed; P0.1b guard allowlist deleted | Preconditions **B5** (legacy VMM removal). Proposed **§4.5 checklist addition**: "shared `a3s-libkrun-sys` has a single published source line consumed by both Box and OCI-Runtime (vendored fork deleted)" | AX4 (DRY at the FFI base) | both repos reference the same version; vendored tree gone |
| C-2 (P3.2) | Quantify B5 sunset: measurable exit metrics on top of the existing §4.5 ten-entry checklist | Axis B.4 | AX1 (dual implementation is the peak silent-fallback risk window) | B5 closing PR with evidence |
| C-3 (P3.3) | Automate the cross-repo release chain (libkrun-sys → OCI pin → Box pin) | — | — | one protocol change ships across all three repos in ≤ 1 day |
| — (continuous) | Contract-consumer metric: keep or dissolve the 24-file `a3s_runtime_driver` adapter based on real use/power adoption | — | AX4 | quarterly review |

Phase 3 stays async: it is gated by Axis B's HVF cutover anyway; starting it
early only lengthens the in-flight fork window.

## 6. Explicit non-goals (anti-overfit)

- No rewriting of the execution domain — Axes A–E remain authoritative.
- No building a "second implementation" to justify keeping a trait — delete.
- No batch-splitting of every oversized file — only files the cutover touches.
- No new framework/abstraction layer to "solve" structure debt — that adds
  surface, violating AX4.
- No cosmetic moves (naming, file relocation) without a real consumer hurt —
  see P0.3.

## 7. Verification record (2026-09-28, Windows x86_64 host, `fix/win-soak-settle-sandbox-egress` + diff)

| Gate | Command | Result |
| --- | --- | --- |
| Type check (lib+tests) | `cargo check -p a3s-box-core -p a3s-box-runtime --lib --tests` | PASS |
| Workspace compile (libs+bins) | `cargo check --workspace --exclude a3s-box-cri` | PASS (cri excluded: `protoc` not installed on this host — environmental, pre-existing) |
| Core tests | `cargo test -p a3s-box-core` | PASS (incl. doc-tests) |
| Runtime unit tests | `cargo test -p a3s-box-runtime --lib` | 1476 passed / 64 failed / 1 ignored |
| Failure attribution | identical run on stashed HEAD baseline | **64-failure set byte-identical** → all pre-existing, host-dependent (WHPX/BindFlt lifecycle tests in `vm::`, `local_execution::vm_backend`, `oci_backend` snapshot paths); none in touched modules; baseline carries exactly the +21 tests this plan deleted (operator + trait tests) |
| Format | `cargo fmt --all -- --check` | PASS |
| Clippy | `cargo clippy -p a3s-box-core -p a3s-box-runtime -p a3s-box-sdk --lib --tests -- -D warnings` | 10 pre-existing lints, **all in files this plan did not touch** (`vm/spec/virtiofs_ro.rs`, `host_sockets.rs`, `oci/mod.rs`, `local_execution/oci_migration.rs`, `local_execution/vm_backend.rs`, `oci/layers.rs`) — Windows-host/toolchain drift; the pinned-toolchain Linux CI gate is authoritative |
| Hygiene guard | bash replication of `check_no_tracked_artifacts.py` live check | PASS (only the two allowlisted C-1 tarballs remain) |
| Hygiene guard self-test | `python3 scripts/check_no_tracked_artifacts.py --self-test` | runs in CI; not runnable on this host (local Python install lacks stdlib) |

### S-4 (2026-09-28, working tree, no PR)

Windows `cargo test` does not compile the `cfg(not(windows))` daemon, so the
lease, socket, and prewarm tests ran in WSL with
`CARGO_TARGET_DIR=/tmp/a3s-box-s4`. Line counts after the move: CLI
`pool.rs` 555, `registry.rs` 797, `serve.rs` 372, `daemon.rs` 270.

| Gate | Command | Result |
| --- | --- | --- |
| Runtime pool tests (Windows) | `cargo test -p a3s-box-runtime --lib pool::` | 78 passed / 0 failed (unix-only tests not compiled) |
| CLI pool tests (Windows) | `cargo test -p a3s-box-cli --lib commands::pool::` | 7 passed / 0 failed |
| Linux typecheck | WSL `cargo check -p a3s-box-runtime -p a3s-box-cli --lib --tests` | PASS |
| Linux runtime pool tests | WSL `cargo test -p a3s-box-runtime --lib pool::` | 100 passed / 0 failed |
| Linux CLI pool tests | WSL `cargo test -p a3s-box-cli --lib commands::pool::` | 13 passed / 0 failed (includes prewarm cleanup, warm-count rejection, autostart lock, stop/status) |

### S-5 CLI errors (2026-09-28, Windows x86_64, working tree)

Image commands, registry login/logout, volume,
network, and lifecycle commands now return `BoxError`. Compose project loading,
`ps`/`config`/`logs`, dependency waits, health preflight, image prefetch, and
Secret projection also return `BoxError`. `compose up`/`down`, rollback, and
sandbox preflight return `BoxError` as well. `compose` itself returns `BoxError`;
 The Compose command family,
including sandbox boot and teardown, no longer returns `Box<dyn Error>`.
`top`, `exec`, and `cp` return `BoxError`. A missing box and a container that
disappeared during refresh are `StateError`. An invalid exec user, workdir, or
request id, and a `cp` between two host paths or two boxes, are `ConfigError`.
A box that is not running, or an OCI copy target that is neither running nor
paused, is `StateError`. A guest copy failure is `ExecError`. Host file and
`tar` I/O stays `IoError`. A retryable unavailable exec or copy stays
`StateError` and still names the request id to reuse. `attach` returns
`BoxError`: Windows interactive attach is `ConfigError`, and a missing console
log, a box that is not running, or a managed generation that is no longer
running is `StateError`. `shell` returns `BoxError`: Windows `shell` and an
invalid shell user or workdir are `ConfigError`; a missing box is `StateError`.
`logs` returns `BoxError`: an invalid `--since` or `--until` and a box with
logging disabled are `ConfigError`; a missing box or a managed generation that
changed while opening logs is `StateError`; reading the log file stays
`IoError`. `inspect` returns `BoxError`: a missing container or image, a
container that disappeared during refresh, and an ambiguous name are
`StateError`. `diff` returns `BoxError`: a missing baseline, a box removed
while its lifecycle lock is held, a host process that is not live, a paused
MicroVM, and a missing rootfs are `StateError`. An empty guest archive or an
unsafe archive path is `ExecError`. Live diff on a platform without that
capture path is `ConfigError`. Reading the baseline file stays `IoError`; a
baseline that does not parse is `SerializationError`. `snapshot` returns
`BoxError`: pruning without `--keep` or `--max-bytes`, an invalid live snapshot
name, and a Windows snapshot that defines a health check are `ConfigError`. An
active MicroVM, a box removed while the lifecycle lock is held, a missing
rootfs or image configuration, a snapshot that is still in use, and a missing
or ambiguous snapshot are `StateError`. `export` returns `BoxError`: a paused
MicroVM, a box removed while its lifecycle lock is held, a host process that
is not live, a missing guest archive endpoint, and a missing rootfs are
`StateError`. An empty guest archive is `ExecError`. Live export on a platform
without that capture path is `ConfigError`. Creating the output archive stays
`IoError`. `commit` returns `BoxError`: Windows live commit and a Sandbox
host-rootfs commit that requires Linux are `ConfigError`. A paused MicroVM, a
box removed while its lifecycle lock is held, a host process that is not live,
a missing rootfs or guest metadata, and a rootfs that changed during capture
are `StateError`. An empty guest archive, an unsafe metadata path, and a
duplicate guest path are `ExecError`. Creating the temporary image directory
   stays `IoError`. `run` returns `BoxError`. A detached TTY, invalid memory or
   log-driver options, an oversized `--timeout`, and Windows interactive PTY
   are `ConfigError`. A managed run that disappeared after startup, and a
   failed start whose rollback also fails, are `StateError`. A sandbox log
   drain that misses its deadline is `TimeoutError`. `create` returns `BoxError`. Invalid runtime
   options, restart policy, memory, port maps, labels, and shm size are
   `ConfigError`. A missing create network is `NetworkError`. Shared boot
   returns `BoxError`: an illegal lifecycle state is `StateError`, a missing
   boot network is `NetworkError`, and invalid persisted boot configuration is
   `ConfigError`. `start`
   and `restart` propagate boot's `BoxError` directly. `pool` returns
   `BoxError`. An invalid pool size, maximum, boot concurrency, warm count, or
   memory, and Windows pool commands, are `ConfigError`. A warm-pool daemon
   that fails to spawn or exits early is `PoolError`. An autostart deadline is
   `TimeoutError`. `run`
   propagates pool autostart's `BoxError` directly. `build` returns
   `BoxError`. An invalid context path, a context that is not a directory, a
   bad build arg or platform, a zero or oversized `--run-pool-timeout`, and a
   missing Dockerfile are `ConfigError`. `monitor` returns `BoxError`. Windows
   `monitor --install` and `--uninstall`, and an active health check on
   Windows, are `ConfigError`. Lost managed lifecycle metadata, a changed
   generation, and an illegal restart state are `StateError`. `monitor_metrics::serve` returns `BoxError`; a bind failure is `IoError`. `container-update` returns `BoxError`. Invalid
   memory, reservation, swap, restart policy, cpuset, and Windows live Tier 2
   updates are `ConfigError`. A box removed during the update, a changed
   execution, and a running Tier 1 resize are `StateError`. A legacy guest
   cgroup failure is `ExecError`. `prune` and `system prune` return `BoxError`. Cleanup
   failures keep the callee variant and record a refused success. Removing a
   pruned box from state is `StateError`. Unused-image removal failure is
   `OciImageError`. Unused-network removal failure is `NetworkError`. Rootfs
   cache pruning keeps `CacheError`. `ps`, `df`, `port`, `version`, and `info` return
   `BoxError`. `ps` JSON encoding stays `SerializationError`. A missing
   `port` container is `StateError`, and a corrupt persisted port mapping is
   `ConfigError`. `info` keeps the inventory and image-store variants while
   refusing an invented empty inventory. `stats` returns `BoxError`. Stats for a box that
   is not running or paused are `StateError`. A missing sandbox is
   `StateError`. A guest stats failure is `ExecError`. SDK runtime errors
   keep their `BoxError` variant. `events` returns `BoxError`. Inventory refresh keeps the callee `BoxError`. JSON encoding stays `SerializationError`. Unparseable
   `--since`/`--until` stay ignored (`parse_time_arg` returns None).
   `rename` returns
   `BoxError`. A name already in use is `StateError`. A missing box is
   `StateError`. `audit`
   returns `BoxError`. An unknown action or outcome is `ConfigError`. JSON
   encoding stays `SerializationError`. `seal`, `unseal`, and `inject-secret` return
   `BoxError`. An invalid sealing policy, a missing secret, and an invalid
   secret format are `ConfigError`. A missing box or runtime socket is
   `StateError`. Reading a seal input or secrets file is `IoError`. TEE
   client failures keep the runtime `BoxError` variant. Windows refuses
   these commands as `ConfigError`. `sdk-bridge` returns `BoxError`; a
   stdin failure is `IoError` and JSON encoding is `SerializationError`.
   `attest` returns
   `BoxError`. An invalid nonce and an unparseable attestation policy are
   `ConfigError`. A missing box or runtime socket is `StateError`. Reading a
   policy file is `IoError`. TEE verification keeps the runtime `BoxError`
   variant. Windows refuses `attest` as `ConfigError`. `scale-api` returns
   `BoxError`. A missing advertise host, a non-ACL catalog, and an invalid
   endpoint host are `ConfigError`. A catalog read failure is `IoError`.
   Scale authority conflict and state failures are `StateError`. Serving the
   API is `IoError`. `port-forward` returns `BoxError`. A non-Linux host is
   `ConfigError`. A box that is not running, not managed, or not a Sandbox
   is `StateError`. Binding, accepting, and relaying are `IoError`. A closed
   connection limiter is `StateError`. `dispatch` returns `BoxError`, and only `main` prints it.

| Gate | Command | Result |
| --- | --- | --- |
| Image command tests | `cargo test -p a3s-box-cli --lib commands::image` | 24 passed / 0 failed |
| `rmi` tests | `cargo test -p a3s-box-cli --lib commands::rmi` | 3 passed / 0 failed |
| `pull` | `cargo test -p a3s-box-cli --lib commands::pull` | 6 passed / 0 failed |
| `push` | `cargo test -p a3s-box-cli --lib commands::push` | 8 passed / 0 failed |
| `save` | `cargo test -p a3s-box-cli --lib commands::save` | 6 passed / 0 failed |
| `load` | `cargo test -p a3s-box-cli --lib commands::load` | 4 passed / 0 failed |
| `history` | `cargo test -p a3s-box-cli --lib commands::history` | 10 passed / 0 failed |
| `import` | `cargo test -p a3s-box-cli --lib commands::import` | 4 passed / 0 failed |
| `login` | `cargo test -p a3s-box-cli --lib commands::login` | 4 passed / 0 failed |
| `logout` | `cargo test -p a3s-box-cli --lib commands::logout` | 4 passed / 0 failed |
| `volume` | `cargo test -p a3s-box-cli --lib commands::volume` | 20 passed / 0 failed |
| `network` | `cargo test -p a3s-box-cli --lib commands::network` | 26 passed / 0 failed on Windows (the parent-directory write-denial prune test is `cfg(unix)`) |
| `start` | `cargo test -p a3s-box-cli --lib commands::start` | 9 passed / 0 failed |
| `stop` | `cargo test -p a3s-box-cli --lib commands::stop` | 7 passed / 0 failed |
| `restart` | `cargo test -p a3s-box-cli --lib commands::restart` | 5 passed / 0 failed |
| `rm` | `cargo test -p a3s-box-cli --lib commands::rm` | 8 passed / 0 failed |
| `kill` | `cargo test -p a3s-box-cli --lib commands::kill` | 18 passed / 0 failed |
| `pause` | `cargo test -p a3s-box-cli --lib commands::pause` | 7 passed / 0 failed |
| `unpause` | `cargo test -p a3s-box-cli --lib commands::unpause` | 6 passed / 0 failed |
| `wait` | `cargo test -p a3s-box-cli --lib commands::wait` | 8 passed / 0 failed |
| `top` | `cargo test -p a3s-box-cli --lib commands::top` | 7 passed / 0 failed |
| `exec` | `cargo test -p a3s-box-cli --lib commands::exec` | 9 passed / 0 failed |
| `cp` | `cargo test -p a3s-box-cli --lib commands::cp` | 19 passed / 0 failed on Windows |
| `attach` | `cargo test -p a3s-box-cli --lib commands::attach` | 9 passed / 0 failed |
| `shell` | `cargo test -p a3s-box-cli --lib commands::shell` | 3 passed / 0 failed on Windows; 3 passed / 0 failed on Linux |
| `logs` | `cargo test -p a3s-box-cli --lib commands::logs` | 20 passed / 0 failed on Windows |
| `inspect` | `cargo test -p a3s-box-cli --lib commands::inspect` | 4 passed / 0 failed |
| `diff` | `cargo test -p a3s-box-cli --lib commands::diff` | 14 passed / 0 failed on Windows; Linux `cargo check -p a3s-box-cli` exit 0 |
| `snapshot` | `cargo test -p a3s-box-cli --lib commands::snapshot` | 11 passed / 0 failed on Windows; Linux `cargo check -p a3s-box-cli` exit 0 |
| `export` | `cargo test -p a3s-box-cli --lib commands::export` | 4 passed / 0 failed on Windows; Linux `cargo check -p a3s-box-cli` exit 0 |
| `commit` | `cargo test -p a3s-box-cli --lib commands::commit` | 23 passed / 0 failed on Windows; Linux `cargo check -p a3s-box-cli` exit 0 |
| `run` | `cargo test -p a3s-box-cli --lib commands::run` | 62 passed / 0 failed on Windows; Linux `cargo check -p a3s-box-cli` exit 0 |
| `create` | `cargo test -p a3s-box-cli --lib commands::create` | 2 passed / 0 failed on Windows |
| boot | `cargo test -p a3s-box-cli --lib boot::tests` | 29 passed / 0 failed on Windows |
| `start` after boot `BoxError` | `cargo test -p a3s-box-cli --lib commands::start` | 9 passed / 0 failed on Windows |
| `restart` after boot `BoxError` | `cargo test -p a3s-box-cli --lib commands::restart` | 5 passed / 0 failed on Windows |
| `pool` | `cargo test -p a3s-box-cli --lib commands::pool::` | 9 passed / 0 failed on Windows; 14 passed / 0 failed on Linux |
| `build` | `cargo test -p a3s-box-cli --lib commands::build` | 20 passed / 0 failed on Windows |
| `monitor` | `cargo test -p a3s-box-cli --lib commands::monitor` | 27 passed / 0 failed on Windows; Linux `cargo check -p a3s-box-cli` exit 0 |
| `container-update` | `cargo test -p a3s-box-cli --lib commands::container_update` | 14 passed / 0 failed on Windows; 28 passed / 0 failed on Linux |
| `prune` | `cargo test -p a3s-box-cli --lib commands::prune` | 3 passed / 0 failed on Windows |
| `system prune` | `cargo test -p a3s-box-cli --lib commands::system_prune` | 8 passed / 0 failed on Windows |
| `ps` | `cargo test -p a3s-box-cli --lib commands::ps::` | 29 passed / 0 failed on Windows |
| `df` | `cargo test -p a3s-box-cli --lib commands::df::` | 4 passed / 0 failed on Windows |
| `port` | `cargo test -p a3s-box-cli --lib commands::port::` | 4 passed / 0 failed on Windows |
| `info` | `cargo test -p a3s-box-cli --lib commands::info::` | 5 passed / 0 failed on Windows |
| `version` | `cargo test -p a3s-box-cli --lib commands::version::` | 1 passed / 0 failed on Windows |
| `stats` | `cargo test -p a3s-box-cli --lib commands::stats::` | 13 passed / 0 failed on Windows |
| `events` | `cargo test -p a3s-box-cli --lib commands::events::` | 14 passed / 0 failed on Windows |
| `rename` | `cargo test -p a3s-box-cli --lib commands::rename::` | 4 passed / 0 failed on Windows |
| `audit` | `cargo test -p a3s-box-cli --lib commands::audit::` | 5 passed / 0 failed on Windows |
| `seal` | `cargo test -p a3s-box-cli --lib commands::seal::` | 7 passed / 0 failed on Windows; Linux `cargo check -p a3s-box-cli` exit 0 |
| `unseal` | `cargo test -p a3s-box-cli --lib commands::unseal::` | 4 passed / 0 failed on Windows; Linux `cargo check -p a3s-box-cli` exit 0 |
| `inject-secret` | `cargo test -p a3s-box-cli --lib commands::inject_secret::` | 2 passed / 0 failed on Windows; Linux `cargo check -p a3s-box-cli` exit 0 |
| `sdk-bridge` | `cargo test -p a3s-box-cli --lib commands::sdk_bridge::` | 0 tests; the command returns `BoxError` and compiled in that run |
| `attest` | `cargo test -p a3s-box-cli --lib commands::attest::` | 7 passed / 0 failed on Windows; 7 passed / 0 failed on Linux |
| `scale-api` | `cargo test -p a3s-box-cli --lib commands::scale_api::` | 5 passed / 0 failed on Windows |
| `port-forward` | `cargo test -p a3s-box-cli --lib commands::port_forward::` | 4 passed / 0 failed on Windows; 3 passed / 0 failed on Linux |
| Compose read, wait, and secrets | `cargo test -p a3s-box-cli --lib commands::compose` | 45 passed / 0 failed on Windows |
| Shared seams and CLI `BoxError` | `cargo test -p a3s-box-cli --lib` plus search of `cli/src` for `dyn std::error::Error` | 922 passed / 0 failed on Windows; 0 `Box<dyn Error>` matches. Linux: pool 14 passed, rootfs ownership 1 passed, missing env file is `IoError`, inverted cpuset stays `ConfigError`; WSL `cargo check -p a3s-box-cli --tests` exit 0 |

SDK/CLI/CRI compile coverage: CLI, SDK, shim, netproxy, guest/init compile in
the workspace check; CRI compiles in CI where `protoc` exists.

## 8. Change control

- Update the Status/§ tables when a phase executes; cite the closing PR and
  the verification row it satisfies.
- Structure/supply-chain PRs cite `S-x`/`C-x` and, when cutover-adjacent, the
  architecture-plan §4.2/§4.3 rows preserved.
- Conflicts resolve per architecture plan §8: ROADMAP open checkboxes win.
