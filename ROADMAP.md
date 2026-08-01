# A3S Box Roadmap

Status: **Active migration**

Primary execution dependency: **A3S OCI Runtime through `a3s-oci-sdk`**

## Product Contract

A3S Box is the local product engine for Linux OCI workloads. It owns the
Docker-like user experience and product resources, while A3S OCI Runtime owns
the complete process and isolation boundary.

The target dependency direction is strictly one way:

```text
A3S Box
   |
   | a3s-oci-sdk over bounded local IPC
   v
A3S OCI Runtime host service
   |
   +-- Native Linux driver
   +-- libkrun/KVM driver
   +-- libkrun/HVF driver
   `-- libkrun/WHPX driver
             |
             `-- authenticated Linux guest agent
```

Box must not import OCI Runtime driver internals. OCI Runtime must not import
Box product types.

## Responsibility Boundary

| Area | A3S Box owns | A3S OCI Runtime owns |
| --- | --- | --- |
| Public interfaces | Docker-like CLI, local language SDKs, Compose, optional standalone CRI compatibility | Low-level OCI CLI, SDK, local service, and containerd runtime-v2 shim |
| Product state | Requested configuration, desired state, restart policy, health policy, and the mapping to an exact runtime generation | Actual OCI state, process and VM identity, operation journal, exit status, reconciliation, and quarantine |
| Images | Pull, push, build, tag, signing, content verification, cache, and rootfs preparation | Immutable bundle consumption and digest revalidation; no registry or build behavior |
| Storage | Named volumes, image layers, snapshots, commits, artifacts, retention, and ownership policy | Rootfs and mount attachment, guest transport, quiesce, checkpoint, and cleanup primitives |
| Networking | Network objects, IPAM, DNS, aliases, publication policy, and host-facing endpoint lifetime | Namespace joins, VM NIC and transport attachment, and exact runtime cleanup |
| OCI | Compile product configuration into an OCI bundle | Validate and enforce the exact OCI configuration or reject it before launch |
| Execution | Reconcile desired product state and forward lifecycle requests | Create, state, start, kill, delete, wait, exec, signals, PTY, pause/resume, update, and stats |
| Isolation | Request a minimum isolation class and reject unsupported product combinations | Select a launch-ready driver without weakening the requested isolation |
| Security | Admission policy, secret authorization, materialization policy, and attestation policy | Namespaces, cgroups, seccomp, capabilities, hooks, safe mount application, secret attachment, and attestation mechanisms |
| Operations | Health monitoring, restart scheduling, Compose orchestration, warm-pool policy, log retention/search/redaction | Raw process I/O, ordered runtime events, reusable-session primitives, and leak-free teardown |

## State Ownership Rules

1. Box persists a product revision and the exact `(container ID, runtime
   generation)` returned by OCI Runtime. It does not persist a guessed runtime
   PID, VM handle, socket, pipe, or cgroup identity.
2. OCI Runtime is authoritative for actual process state and terminal status.
   Box may cache observations, but recovery must reconcile them through the SDK.
3. Box operation IDs are stable across retry. A retry carries the same payload
   and operation ID; a changed product revision uses a new operation ID.
4. Product cleanup is complete only after OCI Runtime confirms execution
   cleanup and Box completes its image, network, volume, secret, and log work.
5. Neither repository performs silent isolation fallback.

## Current Baseline

The current implementation has two execution paths:

- MicroVM workloads are managed directly by Box through its libkrun shim,
  guest init, VM controller, and platform-specific integration.
- Explicit Linux Sandbox workloads use A3S OCI Runtime through the pinned SDK.

This split remains supported while migration is in progress, but it is not the
target architecture. New platform execution features belong in OCI Runtime and
must not introduce a third Box execution path.

Upstream clean commit `A3S-Lab/OCI-Runtime@2d91cd0` now proves the protected
WHPX share across exact owner termination, both Recover fault boundaries,
host-service reopen, exact signal-9 wait replay, stopped-only delete, and
complete runtime cleanup. That closes the Windows restart-evidence prerequisite
for B0/B1, but it does not complete either milestone: the WHPX candidate remains
`probe-only` until its immutable system root and in-process native-handle gates
pass, and Box has not yet selected the unified backend for production MicroVM
launches.

Box now also contains an explicitly constructed `OciLocalExecutionBackend` and
SDK-only lifecycle adapter. The opt-in path preflights launch-ready isolation
and `a3s.oci.attachments.v1` support before reservation or bundle preparation,
keeps Box and runtime generations in separate durable fields, derives a
versioned manifest for rootfs, mounts, networking, process I/O, secret
classifications, and optional extensions, and persists exact
endpoint/target/driver/configuration/attachment evidence. Create rejects
missing or drifted attachment evidence and reconciles lost create/start
responses without issuing a second create. This is migration scaffolding, not
a production routing change. Its in-process contract suite covers both
isolation mappings, corrupt evidence, cleanup, adapter reopen, stopped-only
deletion, and exact normal or signaled exit status. A separate local-transport
contract now restarts the real Windows named-pipe or Unix-socket server behind
one retained backend: the first reconciliation exposes the disconnect, while
the next reconnects, renegotiates, and recovers the original operation and
generation without a second create or start. A second cross-platform contract
launches two distinct owner fixture processes, reopens synchronized runtime
state on the replacement, and proves the same retained manager still records
only one create and start. OCI Runtime commit `24d6a967` independently reopens
its durable `HostRuntimeService` and operation journal across two processes.
Real driver state and production owner wiring remain below.

The same adapter now routes memory-retaining pause and resume through exact
SDK targets. Every freezer mutation first requires the advertised operation,
persists a claim-scoped mutation identity and combines it with the current Box
generation without changing the runtime generation, validates the complete
returned runtime binding, and reconciles a lost response after backend
recreation without issuing the mutation twice. Filesystem-only pause continues
to use the existing
stop/reprepare lifecycle rather than being mislabeled as an OCI freezer action.

The backend-neutral session boundary now also routes captured and streaming
exec, stdin, cursor-checked stdout/stderr, signal/wait, PTY, and resize through
the exact OCI target. Capability preflight and immutable Box/runtime generation
checks happen before exec mutation; keyed one-shot calls retain one process
identity across a lost response and backend recreation. Initial and streaming
stdin mutations are replay-safe, timeouts retain an exact SIGKILL watchdog even
if the caller drops its future, and raw process output never enters Box's
structured log store. Legacy VM records retain their existing socket transport,
while OCI-bound sessions no longer depend on Unix-domain sockets.

The same cross-platform session facade now maps file upload/download and
filesystem stat, recursive mkdir, move, bounded listing, and recursive removal
onto the exact OCI target. Box generation and advertised-operation checks run
before SDK dispatch. Mutations carry one stable context across the adapter's
single retry of an explicitly retryable lost response, while downloads and
metadata reads remain context-free; the contract suite proves one mutation
effect, response target/shape validation, and identical behavior on Unix and
non-Unix hosts. Cross-process and real-driver filesystem-session recovery are
still release gates rather than claimed evidence.

## Delivery Milestones

### B0 - Boundary And Contract Freeze

- [x] Keep product and runtime identifiers distinct in every durable record.
- [x] Define one Box-to-OCI adapter using only public `a3s-oci-sdk` types.
- [x] Map `microvm` to `DedicatedVm` and `sandbox` to `SharedHostKernel` without
  persisting a hard-coded hypervisor driver choice.
- [x] Define versioned attachment contracts for rootfs, mounts, networking,
  process I/O, secrets, and optional runtime extensions.
- [x] Reject an unavailable isolation class before image or product-state
  mutation.

Exit gate: the target architecture compiles behind an opt-in migration flag,
and dependency checks prove that Box imports no OCI Runtime implementation
crate.

The dependency check currently resolves only `a3s-oci-sdk` and its public core
types from the OCI Runtime repository. The exit gate remains open until the
explicit production migration flag is complete.

### B1 - OCI Runtime Vertical Slice

- [x] Add an `OciLocalExecutionBackend` implementing the canonical
  `LocalExecutionBackend` contract.
- [x] Route create, state, start, wait, kill, delete, and exact exit status
  through the SDK.
- [x] Persist only the runtime endpoint, exact container ID/generation,
  selected driver/isolation, and immutable configuration and attachment
  digests required for reconciliation.
- [x] Reopen the local runtime service and reconcile interrupted Box operations
  without launching a duplicate workload.
- [ ] Exercise the path on native Linux and Windows/WHPX.

Exit gate: the same minimal bundle completes an exact, replay-safe lifecycle
through Box on Linux and Windows, including Box and runtime process restart.

The local contract suite proves Box-adapter recreation plus interrupted
`create`/`start` recovery against a shared SDK service, including exact
attachment-manifest negotiation and durable digest drift checks. It also
restarts the real platform IPC server behind a retained backend and proves
next-call reconnect plus exact reconciliation without duplicate launch. The
process contract then replaces the owner with a distinct child process that
reopens disk-backed state, while OCI Runtime separately proves the real durable
host service and journal reopen across processes. The remaining unchecked gate
requires production owner wiring and the same bundle on real native Linux and
WHPX hosts; deterministic process evidence is not promoted as platform-driver
evidence.

### B2 - Interactive And Observable Execution

- [x] Route memory-retaining pause/resume through exact-generation OCI SDK
  operations with capability checks and replay-safe recovery.
- [x] Route captured and streaming exec, process signal/wait, stdin, captured
  output, PTY, and resize through exact-generation OCI SDK operations with
  capability preflight and replay-safe one-shot identities.
- [x] Route process inventory, resource update, stats, and ordered events
  through the OCI SDK.
- [x] Route bounded file upload/download and filesystem stat, recursive mkdir,
  move, bounded listing, and recursive removal through exact-generation OCI
  SDK operations with capability preflight and replay-safe mutation contexts.
- [x] Keep raw runtime output separate from Box log retention, indexing,
  cursor, search, and redaction policy.
- [x] Drive Box command health probes through the canonical runtime exec
  boundary.
- [x] Preserve exact terminal status and Box/runtime generation fencing across
  backend recreation and keyed replay.
- [ ] Prove process-session recovery across an out-of-process runtime-service
  restart on real native Linux and utility-VM drivers.

Exit gate: the existing Box execution, health, logs, resources, recovery, and
SDK suites pass through `OciLocalExecutionBackend` on every advertised driver.

The in-process contract suite now covers repeated pause/resume cycles, stable
claim-scoped operation identities, missing-operation rejection, immutable
runtime binding, and backend recreation after lost freezer responses. It also
covers keyed exec replay after a lost response, exact normal and signaled
status, generation and capability rejection before mutation, replay-safe stdin,
PTY output/resize/signal, raw-log separation, second-rootfs rejection, timeout
cleanup, and caller-cancellation cleanup. File and filesystem contracts add
generation/capability rejection before dispatch, exact target and response
shape checks, bounded payload conversion, and one-effect retry after a lost
mutation response. The same suite now validates exact process targets,
normalized stats, strict ordered-event cursors, durable
`updating_resources` claims, immutable create identity after mutable resource
intent, local completion replay, lost-response recovery after backend
recreation, and terminal-exit races. Native-driver process-session restart
evidence remains part of the exit gate. The retained-backend core-lifecycle
process restart contract covers the shared owner/transport prerequisite, but
not live process-session recovery in a native or utility-VM driver.

### B3 - Storage And Networking Attachments

- [ ] Keep image distribution, builds, named volumes, snapshots, and commits in
  Box while passing immutable, descriptor-bound attachments to OCI Runtime.
- [ ] Keep network objects, IPAM, DNS, aliases, and publication policy in Box;
  delegate namespace, VM NIC, and guest transport attachment to OCI Runtime.
- [ ] Support Windows bind mounts and named volumes without weakening Linux
  ownership, mode, symlink, or read-only semantics.
- [ ] Add quiesce/resume integration for consistent stopped and online product
  snapshots.

Exit gate: image, volume, snapshot, commit, copy, bridge/service networking,
and cleanup suites pass without Box accessing a runtime-owned VM handle or
guest endpoint.

### B4 - Orchestration And Ecosystem

- [ ] Run Compose, restart policy, health monitoring, and warm-pool scheduling
  over the unified adapter.
- [ ] Keep `a3s-box-cri` only as an optional full product adapter; it must use
  the same execution adapter and must not spawn the Box CLI.
- [ ] Make the OCI Runtime-owned containerd shim the preferred Kubernetes
  RuntimeClass integration.
- [ ] Preserve secret authorization and materialization in Box while handing
  only bounded, non-durable attachments to OCI Runtime.

Exit gate: Compose and the supported CRI profiles use the same runtime path as
the CLI and SDK, with no duplicate lifecycle store or runtime subprocess
adapter.

### B5 - Legacy Runtime Removal

- [ ] Remove Box's direct libkrun dependency and bundled VMM implementation.
- [ ] Remove the legacy Box guest init, host/guest control servers, and direct
  WHPX/KVM/HVF lifecycle code after parity gates pass.
- [ ] Remove the Box-owned containerd shim after the OCI Runtime shim is
  packaged and upgrade-compatible.
- [ ] Migrate old Box records or fail with an explicit, actionable compatibility
  error; never reinterpret an old isolation boundary.

Exit gate: the production Box dependency graph contains `a3s-oci-sdk` but no
libkrun, OCI Runtime implementation, guest agent, or hypervisor integration.

### B6 - Supported Cross-Platform Product

- [ ] Qualify Linux x86_64/aarch64, Apple Silicon, and Windows x86_64 against a
  generated capability matrix from the exact release artifacts.
- [ ] Run lifecycle, SDK, network, storage, recovery, security, race, leak, and
  long-duration soak gates for each advertised driver.
- [ ] Verify clean installation, upgrade, rollback refusal, uninstall, and
  runtime-state migration on every supported host.
- [ ] Publish the exact Box, OCI Runtime, guest, kernel, libkrun, protocol, and
  evidence revisions together.

Exit gate: every advertised Box feature either passes its real-host gate or is
rejected before mutation with a stable error. Build-only or simulated evidence
does not promote a platform.

## Prioritization

Work proceeds in this order:

1. lifecycle correctness, state ownership, recovery, and cleanup;
2. process I/O, PTY, signals, resources, mounts, and network attachment;
3. containerd and Box product migration;
4. snapshot-fork, TEE, GPU, and other hardware-specific extensions.

Windows ARM64 remains capability-gated by the available Windows hypervisor
interface. It must not delay a supported Windows x86_64 runtime.

## Integration Policy

Each milestone lands as small, tested commits in its owning repository.
OCI Runtime commits must be pushed before Box advances its pinned SDK revision.
The A3S monorepo updates both gitlinks only after the cross-repository contract
and focused integration suites pass. Completed work moves to `CHANGELOG.md`;
this file retains only current milestones and release gates.
