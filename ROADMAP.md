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

The provider-neutral Runtime conformance fixture now selects either concrete
Box isolation explicitly. Hosted Linux continues to run every advertised
profile through Sandbox. The self-hosted KVM workflow is wired to run every
advertised Runtime profile through real MicroVMs: Base, Recovery, Networking,
Mounts, Health, Resources, Logs, Exec, Security, and Outputs. It also exercises
an authenticated private-registry pull. The suite includes client/provider
restart, external process loss, endpoint relay and cleanup, read-only and
ephemeral mount behavior, bounded exec, durable logs, exact outputs, resource
limits, hostile-input rejection, least privilege, Secret nondisclosure,
duplicate-resource rejection, and final inventory equality. Mount evidence
reads the concrete Sandbox OCI bundle or the persisted MicroVM intent plus
guest mount namespace as appropriate. Resource evidence reads the Sandbox
control/workload hierarchy or the MicroVM's persisted sizing plus guest cgroup
as appropriate. Security evidence reads the Sandbox OCI/process boundary or
the exact MicroVM shim identity plus guest security state and staged non-secret
manifest as appropriate. This wiring is not certification while the repository
`KVM_CI` gate is disabled; executed KVM evidence remains open.

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
responses without issuing a second create. This contract began as migration
scaffolding; Linux now has an explicit production Sandbox route, while the
default route remains unchanged. Its in-process contract suite covers both
isolation mappings, corrupt evidence, cleanup, adapter reopen, stopped-only
deletion, and exact normal or signaled exit status. A separate local-transport
contract now restarts the real Windows named-pipe or Unix-socket server behind
one retained backend: the first reconciliation exposes the disconnect, while
the next reconnects, renegotiates, and recovers the original operation and
generation without a second create or start. A second cross-platform contract
launches two distinct owner fixture processes, reopens synchronized runtime
state on the replacement, and proves the same retained manager still records
only one create and start. That contract now also keeps one live exec stream and
input handle across the observed disconnect, then continues inventory, stdin,
output, signal, exact wait, and cleanup through the replacement owner with one
exec dispatch. OCI Runtime independently proves the same process target through
its durable `HostRuntimeService` and operation journal across two processes.
Real-driver promotion and production cutover gates remain below.

The SDK boundary now also gives every bundle provider the exact runtime
container ID, create operation context, requested isolation, and negotiated
attachment capabilities before product mutation. A provider opting into
`dev.a3s.bundle-handoff` resolves only
`bundle-handoffs/<container>/<create-operation>/bundle`, binds that path and
annotation into the submitted attachment digest, and fails before Box state or
bundle preparation if the runtime does not advertise version 1. The same
operation context is then sent unchanged in SDK `create`; Box never predicts
the independently allocated runtime generation. This closes the product/runtime
contract seam but does not claim the real Windows production lifecycle gate.

The backend-neutral manager now also has one durable migration router with
three explicit creation policies: retain both current paths, route only
Sandbox through the unified OCI adapter, or route both isolation choices.
Every new record is stamped with `box_vm` or `oci_sdk` before backend preflight
and persists that choice with its reservation before launch side effects. All
later lifecycle, session, observability, filesystem, restart, and cleanup calls
use that record-level choice and never fall back after an error. Records written
before this field are recovered from an exact OCI binding or the absence of a
Box-owned exec endpoint. The pinned OCI Runtime revision
`0451739d15795be5829a4774198e276f861561be` retains the matching long-lived,
multi-container Native Linux host owner and adds the generation-safe bundle
handoff used by the preparation context above. Box now first validates the
exact managed home and durably prepares snapshot-lower, named-volume, and
network ownership. Its production provider then prepares the product-owned
rootfs, mounts, resources, DNS/hostname files and OCI bundle while compiling
the image process directly, without the legacy guest-init FD 3/4/5 contract.
It resolves PATH, working directory, named or numeric users/groups,
supplementary groups,
HOME and capabilities against the prepared rootfs before mutation is handed to
Runtime. A provably failed launch rolls back every preparation-owned effect;
unknown launch ownership retains them for exact reconciliation.
The same pinned revision executes file and filesystem calls through a bounded,
parent-bound helper inside the retained user and mount namespaces, so
descriptor-confined operations preserve container IDs on rootfs, bind,
ID-mapped and tmpfs mounts.

The Linux owner composition validates an absolute private root, serializes
startup across processes, records the exact PID start identity and pinned
runtime/agent paths plus SHA-256 digests, refuses an unowned socket or live
artifact drift, and reuses only a launch-ready SDK endpoint. The CLI, machine
bridge and async Rust SDK constructor honor the explicit
`A3S_BOX_OCI_MIGRATION=sandbox` opt-in; no setting means no owner probe or
startup and preserves the legacy route. Core lifecycle, run/exec/PTY, wait,
pause/resume and cleanup commands now detect the persisted OCI route instead
of requiring Box guest sockets. The blocking native-Linux x86_64 and aarch64 CI
lanes now pass the Rust, Python, TypeScript, and Go Sandbox suites through this
exact composition, including lifecycle, exec, filesystem, route-aware stats,
pause/resume, snapshot restore, restart, and cleanup.
Both blocking lanes also send `SIGKILL` to the exact recorded Native Linux OCI
owner while a real Sandbox generation is running. They verify the owner,
launcher, and init identities terminate; a fresh Box SDK-bridge process then
rebinds a distinct owner and reconciles the exact runtime tombstone as stopped
without a fabricated exit code before deleting only that stopped generation.
A second fresh Box process restarts exactly the next Box and OCI generations.

The same adapter now routes memory-retaining pause and resume through exact
SDK targets. Every freezer mutation first requires the advertised operation,
persists a claim-scoped mutation identity and whether the freezer is currently
applied, and combines the identity with the current Box generation without
changing the runtime generation. Recovery reuses the identity while the
mutation is pending and never replays thaw after its durable applied phase is
cleared. Returned runtime bindings are validated in full. Filesystem-only pause
continues to use the existing stop/reprepare lifecycle rather than being
mislabeled as an OCI freezer action.

The backend-neutral session boundary now also routes captured and streaming
exec, stdin, cursor-checked stdout/stderr, signal/wait, PTY, and resize through
the exact OCI target. Capability preflight and immutable Box/runtime generation
checks happen before exec mutation; keyed one-shot calls retain one process
identity across a lost response and backend recreation. Initial and streaming
stdin mutations are replay-safe, timeouts retain an exact SIGKILL watchdog even
if the caller drops its future, and raw process output never enters Box's
structured log store. Legacy VM records retain their existing socket transport,
while OCI-bound sessions no longer depend on Unix-domain sockets. Relative
direct-argv executables resolve through the request, image, or default container
`PATH` against the prepared rootfs, then cross the SDK boundary as normalized
absolute Linux paths; this applies equally to captured exec, streaming exec,
and PTY sessions without invoking a shell.

The same cross-platform session facade now maps file upload/download and
filesystem stat, recursive mkdir, move, bounded listing, and recursive removal
onto the exact OCI target. Box generation and advertised-operation checks run
before SDK dispatch. Mutations carry one stable context across the adapter's
single retry of an explicitly retryable lost response, while downloads and
metadata reads remain context-free; the contract suite proves one mutation
effect, response target/shape validation, and identical behavior on Unix and
non-Unix hosts. The pinned real-driver prerequisite now also proves binary
upload/download, changed-request conflict fencing, stat/list/move, exact
mutation replay, recursive removal, and post-cleanup `NotFound` through native
Linux and utility-VM lifecycle harnesses. Cross-process filesystem-session
recovery remains a release gate rather than claimed evidence.

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

The dependency check resolves only `a3s-oci-sdk` and its public core types from
the OCI Runtime repository. The typed opt-in composition and durable selection
now satisfy this boundary gate. Linux Sandbox production activation remains
explicitly opt-in; default activation and the unified MicroVM cutover remain
later gates.

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
- [x] Exercise the production composition on native Linux x86_64 and aarch64
  through the blocking real-host Rust, Python, TypeScript, and Go SDK lifecycle,
  session, snapshot, restart, and cleanup lanes.
- [x] Kill the real Native Linux owner under a running Sandbox on x86_64 and
  aarch64 and prove fresh Box processes reconcile stopped-only state, exact
  cleanup, endpoint rebinding, and next-generation restart without inventing
  terminal evidence.
- [ ] Exercise the same Box-owned minimal bundle on Windows/WHPX.
  - [x] Negotiate the required handoff extension and bind provider preparation
    to the exact runtime container/create-operation path without coupling Box
    and runtime generations.

Exit gate: the same minimal bundle completes an exact, replay-safe lifecycle
through Box on Linux and Windows, including Box and runtime process restart.

The local contract suite proves Box-adapter recreation plus interrupted
`create`/`start` recovery against a shared SDK service, including exact
attachment-manifest negotiation and durable digest drift checks. It also
restarts the real platform IPC server behind a retained backend and proves
next-call reconnect plus exact reconciliation without duplicate launch. The
process contract then replaces the owner with a distinct child process that
reopens disk-backed state, while OCI Runtime separately proves the real durable
host service and journal reopen across processes. OCI Runtime now exposes the
long-lived Native Linux owner, and Box now supplies fail-closed mixed-backend
routing, verified product-resource preparation, a direct-process production
bundle provider, protected owner startup, and explicit CLI/SDK construction.
The real-host Native Linux x86_64 and aarch64 production composition and
owner/Box process restart lanes now pass. The remaining B1 platform gate is the
equivalent WHPX production path; the deterministic live-session fixtures are
not promoted as proof that a real driver can transparently retain process or
filesystem sessions after its owner dies.

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
- [ ] Route the remaining socket-oriented CLI projections (`attach`, `cp`,
  `top`, `stats`, live `container-update`, and init stdout/stderr log
  projection) through the persisted OCI route.
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
recreation, and terminal-exit races. The cross-platform deterministic owner
contract now keeps the original Box process stream and input handle alive,
exposes the first broken request, reconnects to a replacement process, and
continues inventory, stdin, output, close, signal, exact wait, and cleanup with
one exec dispatch. Native Linux and utility-VM driver reattachment on real
hosts remains part of the unchecked exit gate. The production Linux smoke now
drives the Rust, Python, TypeScript, and Go SDK lifecycle, exec, filesystem,
route-aware stats, pause/resume, snapshot, restart and cleanup surfaces; the
command-specific projections listed above remain deliberately unchecked.

### B3 - Storage And Networking Attachments

- [ ] Keep image distribution, builds, named volumes, snapshots, and commits in
  Box while passing immutable, descriptor-bound attachments to OCI Runtime.
- [ ] Keep network objects, IPAM, DNS, aliases, and publication policy in Box;
  delegate namespace, VM NIC, and guest transport attachment to OCI Runtime.
- [ ] Support Windows bind mounts and named volumes without weakening Linux
  ownership, mode, symlink, or read-only semantics.
- [ ] Add quiesce/resume integration for consistent stopped and online product
  snapshots.

The production provider currently accepts explicit bind/named/tmpfs mounts but
rejects newly introduced image-declared anonymous volumes before Runtime
mutation. That rejection remains until the complete B3 ownership and cleanup
contract is qualified.

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
