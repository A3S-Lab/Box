# A3S Box Roadmap

Status: **Active migration**

Primary execution dependency: **A3S OCI Runtime through `a3s-oci-sdk`**

## A3S Cloud substrate obligations

**Status as of 2026-09-10.**

Cloud Wave 1 treats Box as the sole node-local execution and image-build
provider (`BX0`). Product availability claims stay provisional until this gate
exits. Portfolio detail:
[Cloud foundations roadmap](https://github.com/A3S-Lab/Cloud/blob/main/docs/project-roadmaps/foundations-and-execution.md)
and
[architecture optimization roadmap](https://github.com/A3S-Lab/Cloud/blob/main/docs/architecture-optimization-roadmap.md).

| Priority | This repository must deliver | Forbidden |
| --- | --- | --- |
| `BX0.3` | Sandbox + hardware MicroVM/TEE isolation and attestation evidence | Silent isolation downgrade; Docker execution fallback |
| `BX0.4`–`BX0.5` | Digest-pinned Task/Service lifecycle, build, recovery, cleanup, clean-host EXIT | Treating historical `R0`/`N0`/`D0`/`E0` as Box-current |
| Pairing | Re-certify against the locked Runtime revision after each slice | Product semantics, placement, or a second node channel |

Monorepo index:
[cloud-substrate-dependency-roadmap.md](https://github.com/A3S-Lab/a3s/blob/main/docs/cloud-substrate-dependency-roadmap.md).

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
ordinary advertised Runtime profile through real MicroVMs: Base, Recovery,
Networking, Mounts, Health, Resources, Logs, Exec, Security, and Outputs. A
second explicitly simulated SEV-SNP run adds the capability-triggered Evidence
profile and checks the exact Runtime spec, semantics profile, and identity
attachment binding; live RA-TLS certificate/report; immutable artifact digest;
same-generation continuity; driver reconstruction; tamper rejection;
confidential Task completion; the
attestation-before-execution gate, and cleanup. It also exercises an
authenticated private-registry pull. The suite includes client/provider
restart, external process loss, endpoint relay and cleanup, read-only and
ephemeral mount behavior, bounded exec, durable logs, exact outputs, resource
limits, hostile-input rejection, least privilege, Secret nondisclosure,
duplicate-resource rejection, and final inventory equality. Mount evidence
reads the concrete Sandbox OCI bundle or the persisted MicroVM intent plus
guest mount namespace as appropriate. Resource evidence reads the Sandbox
control/workload hierarchy or the MicroVM's persisted sizing plus guest cgroup
as appropriate. Security evidence reads the Sandbox OCI/process boundary or
the exact MicroVM shim identity plus guest security state and staged non-secret
manifest as appropriate. Simulation remains visibly distinct from hardware.
The separately armed `integration-sev-snp` job requires an AMD SEV-SNP runner
and a pinned launch measurement; neither unexecuted workflow wiring nor
simulation is hardware certification. Executed KVM and SEV-SNP evidence
therefore remain open while their repository gates are disabled.

Box now pins `a3s-runtime` 0.5.0 at
`aeb1dd96b5d3f823464d51fcabe99b6052874aee` and implements the atomic
`ServiceLifecycle` contract without a second lifecycle store. Independent
readiness and liveness thresholds use the existing generation-fenced port and
exec boundaries; unhealthy liveness applies the declared restart policy to the
durable Box generation; and exact `SIGTERM` grace is persisted before forced
termination. The Health profile activates all four capability-triggered
lifecycle cases. Unit and compile evidence is complete in-repository, while a
current-revision destructive Sandbox run and enrolled KVM run remain the
release evidence gate.

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
SDK-only lifecycle adapter. The CLI asks the active migration router to
preflight the selected isolation before named-volume creation or image-cache
access. An OCI route requires a launch-ready driver at that boundary, then
repeats launch-readiness and `a3s.oci.attachments.v1` checks during record
preflight before reservation or bundle preparation,
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
`807d87e9ecbb27add773ad32a2318cd66c1f562c` is the exact source for the Rust
SDK dependency, CI-built runtime and agent, and release artifacts. It retains
the matching long-lived, multi-container Native Linux host owner, the
generation-safe bundle handoff used by the preparation context above, and
stable aggregate workload block-I/O metrics. Connection-local protocol or
disconnect failures no longer terminate that shared owner or its unrelated
containers. The same revision keeps durable Native Linux owner recovery out
of the transient utility-VM guest executor, so
WHPX reaches protocol negotiation without attempting host-only journal writes.
Box now first validates the
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
bridge and async Rust SDK constructor default Linux Sandbox records to
`SandboxViaOci` when `A3S_BOX_OCI_MIGRATION` is absent (explicit `off` keeps the
legacy route; explicit `sandbox` hard-fails if the owner is not launch-ready).
The qualification-only `A3S_BOX_OCI_MIGRATION=microvm|all` MicroVM path still
requires an explicit `A3S_BOX_OCI_KVM_ENDPOINT` for an externally launched
`box-kvm-qualification-service`, or `A3S_BOX_KVM_OCI_BOX_OWNED=1` plus service
root/bin/shim/manifest so Box identity-fences and (re)spawns that Host under
`{service_root}/runtime.sock`. Box-owned KVM Host spawn forces
`A3S_OCI_KVM_SESSION_OWNER=1`; fresh qualification construction reaps
session-owner/shim orphans on dead-Host reclaim, while retained-manager Live
reopen does not. Box-owned KVM ensure does not claim production
MicroVM cutover. Windows WHPX qualification likewise accepts an externally
launched `box-whpx-qualification-service` pipe, or
`A3S_BOX_WHPX_OCI_BOX_OWNED=1` plus service root/bin/shim/vm-rootfs so Box
identity-fences and (re)spawns that Host under a deterministic named pipe.
Box-owned WHPX ensure does not claim WHPX MicroVM production cutover.
Box-owned native Linux Host spawn forces
`A3S_OCI_NATIVE_SESSION_SUPERVISOR=1` so production SandboxViaOci create uses
Host → Supervisor → Launcher; external Hosts that omit the env remain
Host-bound. Fresh SandboxViaOci construction reaps supervised orphans when
reclaiming a dead Host (stopped-only); retained-manager Live reopen does not.
Non-root Host spawn without `A3S_BOX_CI_SETPRIV_WRAPPER` fail-closes unless the
Sandbox OCI launcher is root-owned mode `4755`; hosted CI setpriv on nosuid
runners remains lab-only (self-hosted
`scripts/proof-linux-sandbox-setuid-launcher.sh`). This does not flip B2
harness close or MicroVM cutover.
Core lifecycle, run/exec/PTY, wait,
pause/resume and cleanup commands now detect the persisted OCI route instead
of requiring Box guest sockets. The blocking native-Linux x86_64 and aarch64 CI
lanes now pass the Rust, Python, TypeScript, and Go Sandbox suites through this
exact composition, including lifecycle, exec, filesystem, route-aware stats,
pause/resume, snapshot restore, restart, and cleanup.
Both blocking lanes also send `SIGKILL` to the exact recorded Native Linux OCI
owner while a real Sandbox generation is running. They verify the owner
terminates while supervised children remain until a fresh Box SDK-bridge
process reaps them during owner ensure, rebinds a distinct owner, and
reconciles the exact runtime tombstone as stopped without a fabricated exit
code before deleting only that stopped generation. A second fresh Box process
restarts exactly the next Box and OCI generations.

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
recovery against the durable owner fixture is covered by
`retained_backend_recovers_filesystem_session_after_runtime_owner_process_restart`
(mkdir + keyed upload + move/remove survive owner SIGKILL; list/download after
reconnect). That fixture is **not** real-driver B2 evidence —
`b2_process_session_recovery_closed` stays false.

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

The early boundary is exercised through
`selected_oci_isolation_preflight_fails_closed_without_fallback_or_state` at
the migration router and
`isolation_preflight_rejects_probe_only_driver_without_product_mutation`
against the SDK-backed backend. The `run` and `create` CLI paths invoke that
boundary before named-volume or image operations; record creation deliberately
repeats the mutable capability check before durable reservation.

Exit gate: the target architecture compiles behind an opt-in migration flag,
and dependency checks prove that Box imports no OCI Runtime implementation
crate.

The dependency check resolves only `a3s-oci-sdk` and its public core types from
the OCI Runtime repository. The typed opt-in composition and durable selection
now satisfy this boundary gate. Linux Sandbox production activation for
`--isolation sandbox` defaults to `SandboxViaOci` when
`A3S_BOX_OCI_MIGRATION` is absent; the unified MicroVM cutover remains a later
gate. See [Sandbox GA evidence](docs/sandbox-ga-evidence.md).

**Sandbox GA claim surface (2026-09-12):** Linux defaults new Sandbox records
to `SandboxViaOci` when `A3S_BOX_OCI_MIGRATION` is absent. Hosted
`sdk-local-sandbox` (x86_64/aarch64) proves the production owner route
including Native Live v4 retained stream and filesystem continuity with the
env unset. Explicit `off` preserves the VM-only backend; explicit `sandbox`
hard-fails when the owner is not launch-ready. Public docs must not call this
path a “preview” while CI proves it. Operator host prep is documented in
[Installation](docs/installation.md#linux-sandbox-host-preparation). Sandbox GA
does **not** require MicroVM cutover, WHPX/KVM MicroVM production composition,
flipping `b2_process_session_recovery_closed`, fixture-as-driver Live, or Cloud
`BX0.3` hardware-TEE exit. Default omit-isolation → MicroVM remains until a
separate MicroVM cutover. Full B3/B4 exit gates remain open; Sandbox
GA must advertise only the surfaces already proven (not Compose/CRI/bridge as
closed).

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
- [x] Exercise the same Box-owned minimal bundle on Windows/WHPX.
  - [x] Negotiate the required handoff extension and bind provider preparation
    to the exact runtime container/create-operation path without coupling Box
    and runtime generations.
  - [x] Produce the operation-scoped portable bundle from Box image policy,
    convert bounded rootfs metadata to the public OCI schema, and expose an
    explicit named-pipe qualification composition with a fail-closed feature
    profile.
  - [x] Run the Box-owned bundle through a real Windows WHPX create/start/wait/
    delete gate and retain machine-readable evidence in blocking CI.
- [x] Add the Linux KVM qualification-only vertical-slice executable and local
  runner (`linux-kvm-oci-qualification`,
  `scripts/linux-kvm-oci-qualification.sh`) that prove create replay, Box
  manager reopen, start, exact exit status, delete replay, residual cleanup,
  and Host Service SIGKILL/restart (stopped-only reconcile, no invented exit
  status) against `box-kvm-qualification-service` when service restart inputs
  are available. Existing-host WSL2 evidence with OCI pin
  `35c3370d5aefc1d10c30b77c899044c26865c8c6` retained report SHA-256
  `de79aab7db7676e08543dbe011019cae5f7ccb32b315756f3b4a7a7f9b7ae5ed` for
  schema `a3s.box.linux-kvm-oci-qualification.v2` (exact exit `23` plus Host
  Service restart → stopped-only, no invented exit). Prior evidence on
  `dddd8e96`/`019b2d2`, `d4a05290`/`fdaf2e7`, `2357b38f`/`f9d6eeb`,
  `8ad0a66b`/`f5118ff`, `c0cf2617`/`26ab488`, `3d1f073b`/`7d27d1`,
  `3122b6f8`/`49d409`, `5431090f`/`f14c940`, `a318f4ec`/`60b1888`,
  `d57739f0`/`95d624f`, `8b2804c5`/`3cea73d7`, and `e0fd63db`/`01786abf`
  remains historical. This does not close fresh-host promotion, AArch64
  promotion, live-session gate, or default MicroVM cutover.

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
owner/Box process restart lanes now pass. Box now also owns the portable WHPX
bundle producer, explicit qualification-only named-pipe composition, and the
artifact-bound `windows-whpx-oci-qualification.ps1` gate. The gate imports a
fixed rootfs without registry access, replays create, recreates the Box manager,
observes the exact WHPX binding and exit status, replays deletion, and rejects
Box directories, runtime shares, bundle handoffs, or host processes left after
the lifecycle. Its two versioned JSON reports bind the Box commit, exact pinned
OCI commit, workflow runs, file sizes, and SHA-256 digests. Deterministic
Windows layer extraction and `a3s-box info` share one serialized privilege
scope which temporarily enables only an already assigned
`SeCreateSymbolicLinkPrivilege`, restores the token immediately, and otherwise
retains the Developer Mode/fail-closed path so Linux OCI links are never
flattened. The hardware runner now preflights that exact staged binary before
starting the runtime and distinguishes a missing privilege from ACL or endpoint
security denial in both CLI and extraction diagnostics. Windows handoff
validation treats only the
ordinary and verbatim namespace spellings of the exact same operation path as
equivalent, and the hardware executable bounds preparation/start at 30 minutes.
The first artifact-bound gate passed on real x86_64 Windows/WHPX on August 4,
2026: Box `52a2cfe4ee6693c9cc3a88df1b922bc1825b2deb` from CI run
[`30889251291`](https://github.com/A3S-Lab/Box/actions/runs/30889251291)
ran against pinned OCI Runtime
`08c145d8ce5d06d5f28587226be822a2ab43b299` artifacts from main run
[`30881404238`](https://github.com/A3S-Lab/OCI-Runtime/actions/runs/30881404238).
The report recorded exact create replay, manager-restart reconciliation,
`libkrun-whpx`/`dedicated-vm`, observed running state, exit code 23, replay-safe
deletion, complete lifecycle-directory cleanup, and zero residual A3S
processes.

The exact post-merge Box main artifact
`aaf9e615ee8bb5e22a5214ca09d7e426701f2d58` from main CI run
[`30898682738`](https://github.com/A3S-Lab/Box/actions/runs/30898682738)
subsequently passed the same complete lifecycle gate against the pinned OCI
Runtime main artifacts, binding final source, workflow, binary digests, and
hardware evidence to main revisions.

The deterministic live-session fixtures are
still not proof that a real driver can transparently
retain process or filesystem sessions after its owner dies.

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
- [x] Route CLI `top` and `stats` through the persisted OCI route, retaining
  exact-generation process dispatch plus normalized CPU, memory, PID, network,
  and block-I/O projection for running and paused workloads.
- [x] Route CLI `cp` through the persisted OCI route, retaining
  exact-generation filesystem classification, bounded single-file transfer,
  directory archive execution, and Unix permission restoration without socket
  fallback for running and freezer-paused OCI Sandbox workloads (exec/PTY stay
  Running-only). Does **not** flip `b2_process_session_recovery_closed`.
- [x] Route live CLI `container-update` through the persisted OCI route with
  exact-generation, replay-safe resource intent and no socket fallback.
- [x] Route the remaining socket-oriented CLI projections (`attach` and init
  stdout/stderr log projection) through the persisted OCI route.
- [x] Prove process-session recovery across an out-of-process runtime-service
  restart on real native Linux and utility-VM drivers.
  **Observation matrix greened** by aggregated existing-host WSL2 evidence on Box
  `5f74b5c2356aca534bf35fa5b4d80043c95a691c` + OCI
  `61f77712e420c176dfc1a5d7ba2457e8c8299dcf`: Native Live v4 stream+filesystem
  report SHA-256
  `3d158d6755afcd187f883234768f54aaa1dbd330e56a2ef763d11164fd6b5818`
  (`retained_stream_handle_proven=true`, `retained_filesystem_proven=true`)
  and KVM MicroVM Live v2 stream+filesystem report SHA-256
  `1f8cede9c5906b915a067d8347ede99ceb647b3eb93408610daca7c0ea5f758f`
  (`retained_stream_handle_proven=true`, `retained_filesystem_proven=true`,
  `kvm_microvm_live_claimed=true`). That satisfies W2's Native + one
  utility-VM driver matrix for live process, I/O, and filesystem reattach.
  The B2 **exit gate remains open**: harness report schemas keep
  `b2_process_session_recovery_closed=false` by design (individual reports
  never self-certify B2 close). Does **not** flip
  default create Host-bound policy, cutover, HostRuntimeService registration,
  fixture `process_restart` continuity, fresh-host promotion, or broader
  Utility-VM product claims beyond the observation-scoped KVM MicroVM gate.
  - [x] Observation harness `a3s.box.linux-native-live-session.v2`
    (`linux-native-live-session-qualification` drop-manager path): supervised
    create + Host owner SIGKILL + Live rebind with keyed captured exec
    before/after reopen, state/inventory/stats/kill; no invented exit. Requires
    `A3S_OCI_NATIVE_SESSION_SUPERVISOR=1`. Drops the Box manager before owner
    death, so it does **not** prove retained streaming handle continuity and
    does **not** close B2. **Existing-host WSL2 evidence** on OCI
    `07e653f9fb594e7454f6f732f3d3ceb80928b254`: report SHA-256
    `614dcb5dc08572fb9d6166c2ed00f736db4e7508b145283a047569eeb89323db`
    (`status=passed`, `keyed_captured_exec_after_reopen=true`,
    `live_kill_after_reopen=true`, `removed=true`; retained-stream / B2 /
    fixture / KVM / utility-VM claims stay false).
  - [x] Observation harness `a3s.box.linux-native-live-session.v3`
    (`linux-native-live-session-qualification`) for Native Linux Sandbox with
    supervised create + Host owner SIGKILL + Live rebind while retaining the
    Box manager. Proves retained streaming `start_process` handle continuity
    (Unavailable on owner death → reconcile Ready → same-handle
    stdin/signal/Exit) plus keyed captured exec before/after, state/inventory/
    stats/kill; no invented exit. Requires `A3S_OCI_NATIVE_SESSION_SUPERVISOR=1`.
    Sets `retained_stream_handle_proven=true` **only** when that path passes;
    keeps `fixture_stream_continuity_claimed` and
    `b2_process_session_recovery_closed` false. Does not close utility-VM /
    KVM MicroVM Live or default create Host-bound policy. **Existing-host
    WSL2 evidence** on Box `2c304534beafa6d9284000d992494a780082f3aa` + OCI
    `7001ce5a4c32cd6e2bbb9a833fc45fd05d2318c9`: report SHA-256
    `bc36ff5b895b6320b57322328f78910be82eddfb2203b97bd2c86455b1929d02`
    (`status=passed`, `retained_stream_handle_proven=true`,
    `keyed_captured_exec_after_reopen=true`, `live_kill_after_reopen=true`,
    `removed=true`; B2 / fixture / KVM / utility-VM claims stay false).
    Prior v2 (manager-drop) evidence on OCI
    `07e653f9fb594e7454f6f732f3d3ceb80928b254` remains keyed-capture /
    live-kill only (`614dcb5dc08572fb9d6166c2ed00f736db4e7508b145283a047569eeb89323db`).
  - [x] Observation harness `a3s.box.linux-native-live-session.v4`
    (`linux-native-live-session-qualification`) extends v3 with filesystem
    continuity across owner SIGKILL via public Box `transfer_file` (upload
    before kill, download after reattach on the same generation;
    `retained_filesystem_proven` / `file_upload_before_kill` /
    `file_download_after_reattach`). Keeps retained streaming handle proof.
    Requires OCI pin `05a3b2bddff0668703caafc48f38514a139ee81a` (OCI main tip;
    Native Live filesystem landed in OCI-Runtime #290 / prior pin
    `61f77712…`). Does **not** close B2
    (`b2_process_session_recovery_closed` stays false) and does not claim
    fixture `process_restart` continuity.
  - [x] Observation harness `a3s.box.linux-native-live-session.v5` extends v4
    with a harness-stable keyed file-upload identity
    (`file_upload_request_id` =
    `a3s.box.live-session.keyed-file.before-owner-kill`) and shared
    Unavailable retry for upload/download (parity with keyed exec). Fail-closed
    verifier requires the keyed id. Does **not** close B2 or claim MicroVM
    guest `file_replay` on the Sandbox OCI Live path. Existing-host greening of
    a v5 report digest (with the keyed upload id) remains pending; published
    digests above remain v4-scoped.
  - [x] Observation harness `a3s.box.linux-native-live-session.v6` extends v5
    with keyed MakeDir before owner SIGKILL
    (`mkdir_request_id` =
    `a3s.box.live-session.keyed-mkdir.before-owner-kill`) and ListDir after
    reattach, so mutating filesystem continuity matches the durable
    `process_restart` fixture bar. Fail-closed verifier requires the mkdir id
    and `list_dir_after_reattach`. Does **not** close B2, claim guest
    `filesystem_replay` on Sandbox OCI Live, or MicroVM cutover. Existing-host
    greening of a v6 digest remains pending.
  - [x] Observation harness `a3s.box.linux-native-live-session.v7` extends v6
    with keyed Move / Remove before owner SIGKILL
    (`move_request_id` /
    `a3s.box.live-session.keyed-move.before-owner-kill`,
    `remove_request_id` /
    `a3s.box.live-session.keyed-remove.before-owner-kill`) and ListDir /
    download of the moved tree after reattach. Fail-closed verifier requires
    those ids. Does **not** close B2 or claim guest `filesystem_replay`.
    **CI-greened** on PR #358 / run `34805883757` (report
    `box_commit_sha` `76dc560e4bf40f88fa50614798772fc694c3e8f3`; landed tip
    `6ef55b09…`; OCI `931def0b8c32313802262bbdbebfda669956ca4f`): report
    SHA-256 `71b106e90635780f904679c21f03459c748070aadfd0dbf99a0ea0888107b2fd`
    (linux-x86_64) and
    `43044eed12fb53b4452d5dab948b3ec5528e1236d56ce35a435335e422a3cb21`
    (linux-arm64) (`status=passed`, `retained_filesystem_proven=true`,
    `retained_stream_handle_proven=true`, `move_before_kill=true`,
    `remove_before_kill=true`, keyed Move/Remove ids present; B2 / fixture /
    KVM / utility-VM claims stay false). Prior published digests above remain
    v4-scoped historical evidence.
  - [x] Retained streaming process-handle continuity on a real Native Linux
    owner restart (v3/v4 harness path above; fixture `process_restart` remains
    non-driver evidence). Does **not** alone close the parent (needs KVM
    MicroVM Live siblings); harness reports keep
    `b2_process_session_recovery_closed=false`.
  - [x] Retained filesystem continuity on a real Native Linux owner restart
    (v4 harness path above). **Existing-host WSL2 evidence** on Box
    `d07648d0a7ed0d0801ba190cba3efc361dd9d0f0` (#300: matched-cred harness +
    same-request_id keyed-exec retries) + OCI
    `05a3b2bddff0668703caafc48f38514a139ee81a`: CI run `34542747784` report
    SHA-256 `36f91361c3851ea995ac7530bcfe05f0c9216d6e9a5b7e130b48f151cd4b9a75`
    (linux-x86_64) and
    `8454044deabe77a08d7f193cd970e8f3117566a8651b3f9d4023cb2223321423`
    (linux-arm64) (`status=passed`, `retained_filesystem_proven=true`,
    `retained_stream_handle_proven=true`, `live_kill_after_reopen=true`,
    `removed=true`; B2 / fixture / KVM / utility-VM claims stay false). Prior
    pin `05a3b2b…` single-arch digest
    `9d4f9703b2735f45b34ec98561b443b8076b97d818c57a39d4b4019d7f28bbfd`; prior
    pin `61f77712…` evidence SHA-256
    `3d158d6755afcd187f883234768f54aaa1dbd330e56a2ef763d11164fd6b5818`.
  - [x] KVM MicroVM Live process-session continuity via observation harness
    `a3s.box.linux-kvm-live-session.v1` (`linux-kvm-live-session-qualification`)
    with `A3S_OCI_KVM_SESSION_OWNER=1`, Box manager retained across Host Service
    SIGKILL/restart, and `kvm_microvm_live_claimed` only when retained streaming
    handle continuity passes. **Existing-host WSL2 `/dev/kvm` greened** on Box
    `0a6ce8d73d1030d08676beef6521487d562bc842` + OCI
    `f532e2d818cc302849a7c92653ca8256e3ba277e`: report SHA-256
    `d3d4f3c81659ba646ebb5218bc57db9bb77d765284713d5d41900001848e705e`
    (`status=passed`, `retained_stream_handle_proven=true`,
    `kvm_microvm_live_claimed=true`). Does **not** alone close B2
    (`b2_process_session_recovery_closed` stays false; fixture continuity stays
    false). Broader Utility-VM product claims beyond this KVM MicroVM gate remain
    observation-scoped.
  - [x] Observation harness `a3s.box.linux-kvm-live-session.v2`
    (`linux-kvm-live-session-qualification`) extends v1 with filesystem
    continuity across Host Service SIGKILL via public Box `transfer_file`
    (upload before kill, download after reattach on the same generation;
    `retained_filesystem_proven` / `file_upload_before_kill` /
    `file_download_after_reattach`). Keeps retained streaming handle proof
    and `kvm_microvm_live_claimed`. Requires OCI pin
    `05a3b2bddff0668703caafc48f38514a139ee81a` (OCI main tip; KVM Live
    filesystem landed in OCI-Runtime #289 / prior pin `61f77712…`). Does
    **not** close B2 (`b2_process_session_recovery_closed` stays false) and
    does not claim fixture `process_restart` continuity. Does not flip
    cutover / HostRuntimeService registration. **Existing-host WSL2 `/dev/kvm`
    greened** report SHA-256 `2fe8c2cb53ab6f8a30f8c736cfdc04f41c9fe766b9830dc94d44f09de17454d7`
    (`retained_filesystem_proven=true`; B2 stays false). Prior pin
    `61f77712…` evidence SHA-256
    `1f8cede9c5906b915a067d8347ede99ceb647b3eb93408610daca7c0ea5f758f`.
  - [x] Observation harness `a3s.box.linux-kvm-live-session.v3` extends v2 with
    a harness-stable keyed file-upload identity (`file_upload_request_id` =
    `a3s.box.live-session.keyed-file.before-owner-kill`) and shared Unavailable
    retry for upload/download. Fail-closed verifier requires the keyed id.
    Does **not** close B2 or claim MicroVM cutover. Existing-host greening of a
    v3 report digest (with the keyed upload id) remains pending; published
    digests above remain v2-scoped.
  - [x] Observation harness `a3s.box.linux-kvm-live-session.v4` extends v3 with
    keyed MakeDir before Host Service SIGKILL
    (`mkdir_request_id` =
    `a3s.box.live-session.keyed-mkdir.before-owner-kill`) and ListDir after
    reattach. Fail-closed verifier requires the mkdir id and
    `list_dir_after_reattach`. Does **not** close B2 or claim MicroVM cutover.
    Existing-host greening of a v4 digest remains pending.
  - [x] Observation harness `a3s.box.linux-kvm-live-session.v5` extends v4 with
    keyed Move / Remove before Host Service SIGKILL
    (`move_request_id` /
    `a3s.box.live-session.keyed-move.before-owner-kill`,
    `remove_request_id` /
    `a3s.box.live-session.keyed-remove.before-owner-kill`) and ListDir /
    download of the moved tree after reattach. Fail-closed verifier requires
    those ids. Does **not** close B2 or invent digests. Existing-host
    greening of a v5 digest remains pending.
  - [x] Lifecycle evidence honesty (anti-overfit; does **not** close B2): refuse
  inventing kill exit / `stopped_by_user` on `AlreadyStopped`; refuse inventing
  `Paused` over terminal cold pause/resume evidence; warm pause/resume publish
  terminal or Failed-on-vanish instead of stuck transitional states; snapshot
  recover refuses Sandbox-only publish on non-Sandbox generations; abandoned
  managed `Creating`/`Starting` claims are force-removable via CLI
  `rm --force` / `kill` / `stop` matching manager kill edges (#372), and
  manager inspect retires durable `Starting` when backend observation is
  `NotFound` (lifecycle-lock serialized; no `recover_start` / invented exit),
  with CLI `inspect` / `ps` / `prune` / `system-prune` driving that observe
  before status projection or reclaim selection (managed Starting/Killing/
  Pausing/Resuming/Snapshotting/UpdatingResources; Pausing/Resuming/
  Snapshotting/UpdatingResources NotFound → Failed without invented exit or
  published snapshot), the same home-scoped refresh on `info` / `df` /
  `events` / `compose wait` / `compose ps` / `compose logs` / `stats` /
  `top` / `port` / `logs` / `attach` / `monitor` poll so read-only inventory
  projections and restart decisions stay present-tense with `ps`,
  and resume of durable managed `Removing` via remove-retry (`finish_remove`)
  on those surfaces and `wait` (not inspect NotFound retirement; prune filter
  stays `stopped|dead|created`), resume of durable managed
  `RestartStopping` / `RestartStarting` via `reconcile(create operation)` on
  those surfaces and `wait` (inspect keeps Creating; no inspect NotFound
  retirement), and `wait` driving manager inspect for any managed record while
  refusing to invent exit `0` for transitional durable statuses **or** for
  terminal durable statuses whose `exit_code` is still absent after inspect
  retire (same `wait_poll_action` gate as legacy; archive wait also refuses
  inventing `0`); health workers fail closed when durable state cannot be
  loaded; interaction surfaces `exec` / `shell` / `cp` refresh one managed
  claim before require-running (inspect/top/port/attach parity); `inspect`
  projects `State.ExitCode` as null when durable exit is absent (no invent `0`);
  Runtime Service `runtime_state` refuses inventing `Stopped` over durable
  `Failed`/`dead` when exit is absent (operator `Stopped`+absent exit and
  authenticated exit `0` remain `Stopped`); Windows StateFile reconcile keeps
  only authenticated durable/persisted exit after guest-result collect
  failure (no invent `0`/`1` when both absent); CRI SPDY exec projects
  kubectl error-stream status only from authenticated `ExecEvent::Exit` /
  `PtyExit` (absent → synthetic `255`, never invent success `0`); Linux
  `resolve_workload_exit_code` refuses provider/shim `0` when terminal
  status is absent and no legacy rootfs marker exists (PendingOrInvalid
  parity); boot-failure delayed terminal poll
  (`wait_for_delayed_terminal_exit`) uses the same refusal so cleanup cannot
  re-invent success after resolve filtered provider `0`; Windows live
  boot-failure cleanup likewise refuses provider/stop `0` when the guest never
  completed and durable status is absent; Windows operator destroy/stop uses
  `collect_windows_guest_result` instead of raw provider exit so clean `0`
  without durable status stays Absent; Windows `wait_for_exec_ready` refuses
  inventing exec-ready success on bare provider/`has_exited` without durable
  guest status (Unix fail-closed parity; authenticated WHPX completion still
  surfaces as `BoxBootError`); managed terminal observation
  (`finish_registered_terminal`) refuses inventing `Stopped` from cached
  shim/provider `0` without durable guest status (Unavailable + retain runtime);
  `VmManager::{try_wait_exit,exit_code,has_exited}` authenticate cached
  shim/provider `0` at source (pool deferred-main / start-during-startup
  cannot bypass terminal observation); Windows `cleanup_boot_failure`
  collect-Err fallback refuses inventing clean provider `0`; Unix
  `wait_for_exec_ready` refuses inventing Ready/`Ok(())` when durable guest
  exit is already persisted before the exec heartbeat (Windows `#407`/`#408`
  fail-closed parity); Windows managed observe/`promote_if_ready` refuses
  inventing Ready/Running from layout path presence alone (named-pipe
  heartbeat + `guest-control.ready`); Windows pool `wait_for_exec_available`
  fails closed instead of unconditional `Ok(())`;
  `attach_running_process` refuses inventing Ready from shim PID / layout
  path alone (authenticated exec heartbeat, else `Created`); Unix snapshot-
  restore boot refuses inventing Ready after a failed one-shot exec probe
  (leave `Created`, no `box.ready`, #414 / #413 parity); Linux Sandbox
  recover `attach_sandbox` refuses inventing Ready from OCI runtime record
  alone (authenticated exec heartbeat, else `Created`, #415); managed MicroVM
  `start` refuses inventing a start handle / durable Running when boot left
  `Created` without authenticated Ready (#416 / #414 seam); pool warm publish
  aligns Ready after `wait_for_exec_available`; managed observe refuses
  inventing `Running` from durable `Pausing` + backend `Creating`, and
  inventing `Paused` from durable `Resuming` + backend `Creating` (project
  `Creating` until pause/resume evidence authenticates); managed MicroVM
  `promote_if_ready` retains the authenticated Unix `ExecClient` after
  heartbeat instead of dropping a one-shot probe (#418 / #413/#415 parity);
  Unix `health_check` for Ready/Busy/Compacting requires exec heartbeat
  (retained or reconnect), not shim PID alone (#419 / #418); Windows
  `health_check` likewise requires retained-client or `guest-control.ready` +
  named-pipe heartbeat (#420 / #419 parity); managed MicroVM start re-proves
  exec health when in-memory Ready/Busy/Compacting is already set — stale
  Ready without heartbeat must not invent a start handle (#421 / #419/#420);
  `handle_from_manager` requires Ready/Busy/Compacting (Sandbox paused keeps
  durable `Paused` without inventing a handle; pause/resume fails closed)
  (#422 / #415/#416); WarmPool `acquire` re-proves exec health before idle
  handoff (stale Ready destroyed, not leased) (#423 / #421); MicroVM `pause`
  demotes to `Paused` after SIGSTOP and `resume` re-proves exec before Ready
  (#424 / #421/#422); Windows `set_boot_completion_state` refuses inventing
  Ready without a retained authenticated `ExecClient` (cold wait / pool
  available / attach retain the heartbeat client; call-order alone is not
  proof) (#425 / #414); Scale inventory refuses inventing
  `InstancePhase::Ready` from inspect `Running` alone while durable managed
  status is transitional — Ready needs durable `Running` (#427 / #417);
  CRI sandbox acquire refuses inventing Ready for a soft-Created VM without
  authenticated exec (#428 / #414/#413); WarmPool `release` refuses inventing
  idle Ready without exec re-auth (#429 / #423); Sandbox boot refuses inventing
  Ready without cross-platform exec wait + boot-completion proof (#430 /
  #425/#414); CRI PodSandboxStatus refuses inventing Ready from durable state
  without VM health re-proof (#431 / #428/#423); CRI ListPodSandbox /
  StreamPodSandboxes refuse inventing Ready from durable state without the
  same health re-proof (#432 / #431); Rootfs-maintenance boot refuses inventing
  Ready without `set_boot_completion_state` retained-client proof (#433 /
  #425/#430); CRI ContainerStatus/ListContainers refuse inventing Running from
  durable state without sandbox VM health (#434 / #432/#431); CRI Status verbose
  counts refuse inventing Ready/Running tallies without the same health
  re-proof (#435 / #432/#434); CRI ListPodSandboxMetrics /
  StreamPodSandboxMetrics refuse inventing Ready/Running gauges without the
  same health re-proof (#436 / #435/#434); CRI CreateContainer / PortForward /
  UpdatePodSandboxResources refuse inventing Ready for mutation gates without
  the same health re-proof (#437 / #436/#431); CRI ExecSync / Exec / Attach /
  UpdateContainerResources refuse inventing Running for mutation gates without
  the same container health re-proof (#438 / #434/#437); CRI StartContainer
  refuses inventing "already running" from durable Running without the same
  container health re-proof (#441 / #438/#434); CRI ContainerStats /
  ListContainerStats refuse inventing Running usage without the same
  container health re-proof (#442 / #434/#436); CRI PodSandboxStats /
  ListPodSandboxStats refuse inventing Ready/Running pod usage without the
  same health re-proof (#443 / #442/#436); CRI RemovePodSandbox /
  StopPodSandbox / CreateContainer late Ready re-check refuse inventing live
  Ready/Running without the same health re-proof (#444 / #443/#437/#434); CRI
  RemoveContainer refuses inventing force-stop of durable Running without the
  same container health re-proof (#445 / #444/#434); CRI StopContainer refuses
  inventing live stop / VM teardown / sandbox NotReady from durable Running
  alone (#446 / #445/#434); CRI ReopenContainerLog refuses inventing rotation
  success without sandbox VM health re-proof, an active supervisor reopen
  handle, and a completed reopen ack (#447 / #438/#434); CRI StopPodSandbox
  refuses inventing SIGKILL exit 137 for Created never-started containers
  (#448 / #447/#444); CRI `load_state` restart reconciliation refuses inventing
  exit 255 for Created never-started containers (#449 / #448); CRI
  `sandbox_vm_usage` / PodSandboxStats refuse inventing CPU/RSS from a shim PID
  alone when VM health is inventable (#450 / #419/#443); CRI
  `UpdatePodSandboxResources` refuses inventing Ok when linux/annotations
  request unsupported pod-level mutation (#451 / #437); CRI
  `UpdateRuntimeConfig` refuses inventing Ok when NetworkConfig.pod_cidr is
  non-empty (#452 / #451); CRI PullImage honors AuthConfig.identity_token and
  refuses inventing anonymous pull for unsupported registry_token / malformed
  auth (#453 / #452);

  default TSI unpublished guest listeners are allowlist-gated so they do not
  bind host `0.0.0.0` without `-p` (#371); unpublished guest `listen()` still
  succeeds in-guest via TSI `-EPERM` so scale host relays can reach the
  workload, and `ready_replicas` counts a declared endpoint only after the
  advertised URL serves a request and withdraws the lease when a later
  reconcile probe fails (#370); Runtime Service (R17) advertised
  host endpoints are likewise published only after a live listener probe
  proves the guest relay opened, and a failed probe during `apply` retires
  the generation instead of leaving an orphan Running Service. Retained R17
  leases are re-probed rather than republished from identity match, and
  inspect retires the generation when re-probe fails (same inventory honesty
  as apply).

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
one exec dispatch. The same fixture owner also recovers keyed file upload and
mutating filesystem state across owner replacement without flipping B2. Native
Linux and KVM MicroVM Live reattachment on real hosts is observation-greened under
the process-session recovery parent above
(existing-host WSL2 digests); the B2 exit gate remains open and harness reports
still keep
`b2_process_session_recovery_closed=false`. Box-owned native Host spawn forces
supervised create for production SandboxViaOci; external Hosts without the env
remain Host-bound. Fresh construction reaps supervised orphans on dead-Host
reclaim; Live retained-manager reopen does not. Operator setuid launcher
evidence is fail-closed at spawn (CI setpriv is lab-only). Cutover, fresh-host
promotion, and broader Utility-VM product claims remain open. The production
Linux smoke now
drives the Rust, Python, TypeScript, and Go SDK lifecycle, exec, filesystem,
route-aware stats, pause/resume, snapshot, restart and cleanup surfaces; the
CLI `top`, `stats`, `cp`, live update, attach, and init-log projections now
share that exact route. The init-log worker starts before runtime start,
retains an exact endpoint and generation, reconnects after runtime-service
owner replacement, and must publish final drain evidence before deletion.

The standalone CRI adapter now reconciles persisted sandboxes to `NotReady`
after a service restart, marks containers without a live VM exited, reclaims
their bridge-network endpoints, and removes leaked CRI rootfs trees. This is
resource-safe restart reconciliation, not process-session reattachment (that
Live observation matrix is observation-greened above; the B2 exit gate remains
open and harness reports still keep
`b2_process_session_recovery_closed=false`).

### B3 - Storage And Networking Attachments

- [ ] Keep image distribution, builds, named volumes, snapshots, and commits in
  Box while passing immutable, descriptor-bound attachments to OCI Runtime.
  **Partial:** Native Linux SandboxViaOci prepare now binds Box-owned named and
  anonymous volumes, the Box-owned `/workspace` bind (`a3s.box.workspace`), and
  staged caller bind aliases under `sandbox/attachments/{slot}`
  (`a3s.box.bind.{slot}`) into `a3s.oci.attachments.v2` (caller-owned
  DetachOnly). Remaining unclassified external binds fail closed at create.
  Legacy CLI/SDK stop/remove volume detach and anonymous volume removal fail
  closed like managed cleanup. Image/build/snapshot/commit descriptor handoff,
  network v3, Windows volume parity, and the B3 exit gate remain open. Does
  **not** flip `b2_process_session_recovery_closed`.
- [ ] Keep network objects, IPAM, DNS, aliases, and publication policy in Box;
  delegate namespace, VM NIC, and guest transport attachment to OCI Runtime.
  **Partial:** Native Linux SandboxViaOci prepare classifies prepared
  `linux.netDevices` into `a3s.oci.attachments.v3` (caller-owned identities;
  cleanup mode follows namespace path). Opt-in
  `A3S_BOX_OCI_NATIVE_KEEP_NETWORK_DEVICE_AUTHORITY=1` (matched root) keeps a
  Privileged owner, stages one host veth + emits `linux.netDevices` for Bridge
  (NetworkStore IPAM), enslaves the peer on a Box-owned Linux bridge for L2,
  and installs the NetworkStore gateway on the bridge plus a default route on
  the container end, with host `ip_forward` + per-subnet iptables MASQUERADE for
  egress and NetworkStore peer `/etc/hosts` discovery, plus optional static TCP
  DNAT publication (CLI/SDK and Compose sandbox under keep-authority Bridge).
  Product admission (CLI / MicroVM Compose / SDK) resolves `0:guest` to a
  concrete ephemeral host port before boot so backends do not silently drop
  unresolved auto-assign. MicroVM bridge `passt` rejects unresolved
  `host_port=0` / invalid publish entries instead of silent skip. Keep-authority
  DNAT teardown fails closed when a present publish rule cannot be deleted
  (lease retained); prepare and Sandbox cleanup propagate lease teardown
  failures instead of soft-skip/warn; boot-failure and managed path cleanup
  tear down the lease before wiping `boxes/{id}` (retain dir on teardown
  failure); orphan crash-recovery reap, VM destroy, SDK/CLI/Compose path
  cleanup, and foreground `--rm` use the same lease-before-wipe contract; veth
  delete contract as DNAT/MASQUERADE; staging rollback surfaces combined
  present-link / idle-bridge delete failures; idle-bridge MASQUERADE removal
  uses the same present-rule delete contract. GA default remains delegated
  rootless/
  `base_v2` loopback-only. CNI, UDP/auto-assign publish, DNS server/proxy,
  multi-device, rootless/MicroVM/Windows parity, and the B3 exit gate remain
  open. Does **not** flip `b2_process_session_recovery_closed`.
- [ ] Support Windows bind mounts and named volumes without weakening Linux
  ownership, mode, symlink, or read-only semantics.
  **Partial:** Windows stopped `snapshot create` requires guest rootfs metadata
  via `save_managed` (aligned with stopped Windows `commit`). Bind/named-volume
  prepare and managed VolumeStore paths refuse symlink/reparse host sources
  before following them. Stopped directory-backed MicroVM `export`/`diff`
  require guest rootfs metadata (same honesty contract as stopped `commit`).
  Linux MicroVM `:ro` volumes host-enforce write denial via private RO bind
  aliases before virtio-fs; Windows/macOS host `:ro` denial and full Linux
  UID/GID storage on Windows binds remain open. Does **not** flip
  `b2_process_session_recovery_closed`.
- [ ] Add quiesce/resume integration for consistent stopped and online product
  snapshots. **Partial:** managed Linux Sandbox live/paused/stopped snapshots,
  host-rootfs commit/export/diff, and paused `cp`/filesystem now share the
  managed quiesce/host-rootfs surface; SandboxViaOci `diff` baselines use the
  same OCI-mapped metadata contract as live/stopped capture; Windows stopped
  snapshots retain guest metadata via `save_managed`; stopped directory MicroVM
  export/diff retain guest metadata; managed SandboxViaOci captures are labeled
  and refuse MicroVM-shaped CLI/SDK `snapshot restore`; missing
  `rootfs_snapshot.json` fails closed on `diff` and baseline-create errors abort
  boot instead of soft success; boot fails closed when `.snapshot-lower` is
  present but the lower directory is missing (no image-pull fallthrough);
  snapshot delete/prune inventory fails closed on `.snapshot-lower` I/O errors;
  MicroVM live host-path snapshots and the full B3 storage/network
  qualification gate remain open. Does **not** flip
  `b2_process_session_recovery_closed`.
- [x] Persist normalized image-declared anonymous-volume identities before OCI
  bundle preparation, enforce exact single-owner claims, and keep recovery and
  removal driven by the durable Box record.

The production provider accepts explicit bind/named/tmpfs mounts and now plans
image-declared anonymous volumes without creating execution artifacts. Bundle
preparation must reproduce the exact persisted plan before Runtime mutation;
name collisions, ownership drift, duplicate destinations, and unsafe identities
fail closed for Sandbox and MicroVM materialization alike. Image content accounting, cross-process index refresh, per-digest
content publication/removal locking, cache-key confinement, and volume cleanup
are now covered by focused race and recovery tests. Warm-pool initial fill and
replenishment share a configurable `max_concurrent_boots` limit (default `2`),
so startup parallelism has an explicit host resource budget; the pool daemon
shares that limiter across image pools and on-demand misses, and failed
background replenishment retries use capped exponential backoff. B3 lazy pool
initialization is serialized per exact image/resource shape, while
different shapes may initialize concurrently; shutdown fencing prevents a
late initializer from publishing a pool after drain begins. Lazy first-use
creation waits for one ready VM and fills the remaining idle target in the
background. Compose startup prefetches unique service images with a bounded
two-way pull fan-out before creating networks or VMs, so independent Dify image
downloads do not add serial registry latency while a failed pull leaves no
partial project resources. The `a3s_box_warm_pool_initial_fill_duration_seconds`
histogram records first-ready latency for lazy pools and complete fill time for
eager pools. Compose health-dependency convergence polls at 500ms while leaving
the health worker cadence and caller timeout unchanged; detached workers also
probe immediately after `start_period` rather than waiting an extra interval.
The complete
B3 storage/network qualification gate remains open, and production CPU and
tail-latency measurements remain an open gate.

Exit gate: image, volume, snapshot, commit, copy, bridge/service networking,
and cleanup suites pass without Box accessing a runtime-owned VM handle or
guest endpoint.

### B4 - Orchestration And Ecosystem

- [ ] Run Compose, restart policy, health monitoring, and warm-pool scheduling
  over the unified adapter.
  **Partial:** Compose `--isolation sandbox` up/down now uses
  `LocalExecutionManager` / SandboxViaOci (same create/start/remove path as
  CLI/SDK), including session-exec health probes and `service_healthy` waits.
  Opt-in `A3S_BOX_OCI_NATIVE_KEEP_NETWORK_DEVICE_AUTHORITY=1` also creates
  NetworkStore named bridges for Sandbox Compose and admits static TCP
  published ports (CLI/SDK DNAT contract; UDP/`host_port=0` still refused).
  Start-failure cleanup surfaces `remove_execution` errors instead of
  soft-discard. Warm-pool, MicroVM Compose cutover, and the B4 exit gate remain
  open. Does **not** flip `b2_process_session_recovery_closed`.
- [ ] Keep `a3s-box-cri` only as an optional full product adapter; it must use
  the same execution adapter and must not spawn the Box CLI.
- [ ] Make the OCI Runtime-owned containerd shim the preferred Kubernetes
  RuntimeClass integration.
- [ ] Preserve secret authorization and materialization in Box while handing
  only bounded, non-durable attachments to OCI Runtime.
  **Partial:** Native Linux SandboxViaOci prepare classifies managed Secret
  binds with `mark_secret_mount` (index-only). Auth/materialization remain in
  Box. Compose/CRI/unified-adapter and the B4 exit gate remain open. Does
  **not** flip `b2_process_session_recovery_closed`.

Exit gate: Compose and the supported CRI profiles use the same runtime path as
the CLI and SDK, with no duplicate lifecycle store or runtime subprocess
adapter.

The current optional CRI adapter additionally owns its Unix-socket takeover
check and explicitly stops its streaming listener during shutdown. These
hardening steps reduce split-brain and port-leak failures, but do not satisfy
the unified-adapter or OCI Runtime-owned shim gates.

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