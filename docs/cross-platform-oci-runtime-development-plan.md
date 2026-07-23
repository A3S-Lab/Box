# Cross-Platform OCI Runtime Development Plan

Status: **In development**

Scope: repository extraction, runtime architecture, delivery order, migration
from certified `crun`, and release gates

Target repository: `git@github.com:A3S-Lab/OCI-Runtime.git`

Target monorepo path: `crates/oci-runtime`

Primary Rust packages: `a3s-oci-sdk` and `a3s-oci-runtime`

## Executive decision

A3S should create an independent cross-platform OCI runtime repository and use
it to remove A3S Box's long-term dependency on an external `crun` binary.

The project is not a Windows port or fork of `crun`. It is an A3S-owned runtime
for Linux OCI workloads with two execution paths that share one Linux
container executor:

- a native Linux driver executes the container directly with Linux namespaces,
  mounts, cgroup v2, seccomp, capabilities, and process supervision, without
  requiring KVM;
- a libkrun driver starts a Linux utility VM through KVM, HVF, or WHPX and asks
  a guest agent to execute the same container logic inside the guest.

The release objective is complete OCI Runtime Specification 1.3.0 conformance
for every normative requirement applicable to Linux containers and each
advertised native or utility-VM driver. A restricted A3S bundle profile may be
used as an early implementation fixture, but it is not an acceptable
production compatibility boundary and cannot be used to remove `crun`.

Development remains incremental:

1. build and test the complete public Rust SDK and OCI model boundary;
2. land lifecycle and driver operations behind truthful capability states;
3. retain certified `crun` as a rollback and differential oracle;
4. migrate A3S Box only after the complete applicable OCI configuration,
   lifecycle, hook, security, recovery, and soak gates pass.

Unsupported or inapplicable properties must fail before runtime state mutation.
They must never be silently ignored or omitted while crossing the SDK, host,
transport, durable-state, or guest-agent boundaries.

KVM is an optional Linux VM backend, not an installation, startup, Sandbox, or
SDK prerequisite. Native Linux support on a host where `/dev/kvm` is absent,
inaccessible, or unusable is a blocking release requirement.

## Why this is a separate repository

The OCI runtime is a reusable infrastructure boundary, not an A3S Box product
surface. It has an independent security model, release artifact set, guest
binary, conformance matrix, and lifecycle state store.

Keeping it separate provides:

- a small dependency graph that does not include the Box SDK, image registry,
  build engine, Compose, snapshots, or product control plane;
- independent versioning and certification of host, shim, guest agent, kernel,
  and libkrun artifacts;
- standard OCI lifecycle and bundle tests without routing through Box;
- reuse by Box, containerd integration, and future A3S products without a
  dependency on `a3s-box-core`;
- a one-way dependency graph that prevents the runtime from importing product
  policy.

The Rust crate dependency direction must be:

```text
a3s-box ---------> a3s-oci-sdk <--------- a3s-oci-runtime
                                                |
                                                v
                               a3s-libkrun-sys + platform driver
```

`a3s-oci-runtime` must not depend on `a3s-box-core` or `a3s-box-runtime`.
`a3s-oci-sdk` must not depend on runtime drivers.

## Product and security terminology

Backend identity and isolation properties must remain separate. The runtime
must report at least these isolation classes:

| Isolation class | Host boundary | Kernel sharing |
| --- | --- | --- |
| `dedicated-vm` | Hardware VM | One workload or pod owns the guest kernel |
| `shared-guest-kernel` | Hardware VM | Containers in one trust domain share a guest Linux kernel |
| `shared-host-kernel` | No VM boundary | Containers share the Linux host kernel |

Windows and macOS cannot provide `shared-host-kernel` for Linux workloads.
Their low-overhead Sandbox implementation is `shared-guest-kernel`.

A shared utility VM must be scoped to one explicit trust domain. The runtime
must never place unrelated tenants into one global VM merely to improve
density. A caller that requires a hardware boundary between workloads must
select `dedicated-vm`.

The public Box values can remain `microvm` and `sandbox`, but persisted state,
capability output, audit events, and documentation must expose the effective
isolation class.

### Linux without KVM contract

On a supported Linux host where KVM is unavailable:

- `a3s-box run --isolation sandbox` must run through the native Linux driver;
- the Rust, Python, and TypeScript SDK packages must load, create a client, and
  complete their Sandbox lifecycle suites when typed Sandbox isolation is
  selected;
- commands, files, exec, PTY, logs, stats, pause/resume, stop, kill, and cleanup
  must have the same public SDK behavior as on a KVM-capable Linux host;
- image and bundle preparation must not initialize libkrun or open `/dev/kvm`;
- `info` and SDK inspection must report that KVM is unavailable, why its probe
  failed, the selected native driver, and the effective
  `shared-host-kernel` isolation class.

An explicit MicroVM request must fail before image pulls or runtime state
mutation when KVM is unavailable. It must never silently downgrade to
shared-host-kernel isolation. The existing SDK MicroVM default remains a
MicroVM request; callers on a non-KVM Linux host select the typed Sandbox
option explicitly.

## Goals

- Implement the OCI create, start, state, kill, and delete lifecycle with
  durable, crash-reconcilable state.
- Implement every normative OCI Runtime Specification 1.3.0 requirement
  applicable to Linux containers and the drivers advertised by the runtime.
- Provide a complete async, strongly typed, `Send + Sync` Rust SDK for A3S Box
  without CLI construction or driver-specific imports.
- Support run, exec, pause, resume, process I/O, terminal resize, resource
  updates, and events required by A3S Box.
- Run Linux OCI bundles on:
  - Linux x86_64 and aarch64 through the native Linux driver without KVM;
  - Linux x86_64 and aarch64 through the optional KVM driver when supported;
  - macOS arm64 through the existing libkrun HVF path;
  - Windows x86_64 through the existing A3S libkrun WHPX path.
- Use one Linux container executor implementation in the native Linux driver
  and the utility-VM guest agent.
- Allow multiple containers to share one utility VM without sharing PID,
  mount, IPC, UTS, user, cgroup, or network namespaces unless the submitted OCI
  configuration explicitly requests supported sharing.
- Preserve Box's existing exec, PTY, log, rootfs metadata, resource, and
  lifecycle behavior through a narrow adapter.
- Keep runtime installation, Box startup, capability inspection, and all
  non-VM SDK operations functional when KVM is absent or inaccessible.
- Fail closed when a requested security or resource control cannot be applied.
- Ship version-matched host binaries, guest agent, kernel/firmware, libkrun
  library, protocol schema, and provenance records.
- Preserve complete OCI models and semantics so a future containerd shim does
  not require a second lifecycle implementation.

## Non-goals for the first production release

- Native Windows container images or Windows process-isolated containers.
- Full Docker Engine compatibility.
- A registry, image puller, builder, content store, or snapshotter.
- Arbitrary hypervisor binaries selected from untrusted OCI configuration.
- Arbitrary device passthrough, GPU, TEE, live migration, or snapshot-fork.
- Host PID namespace sharing in a utility-VM backend.
- Cross-tenant sharing of a utility VM.
- CRI or containerd shim v2 before the standalone OCI lifecycle is stable.
- Removing `crun` before rollback and differential gates pass.

## Standards baseline

The runtime is pinned to OCI Runtime Specification 1.3.0 until an explicit
standards-update commit advances the schemas, Rust models, property inventory,
fixtures, validators, and conformance evidence together. It must record the
exact supported version and feature set in `features` output.

The initial implementation must:

- represent the complete official OCI `Spec`, `Process`, `LinuxResources`,
  `State`, and `Features` models in the Rust SDK;
- classify every OCI 1.3.0 schema property as applicable, inapplicable to the
  selected workload platform, or unsupported by the selected driver;
- accept all applicable configuration after its semantic and enforcement
  implementation passes;
- implement the specified `creating`, `created`, `running`, and `stopped`
  states;
- guarantee that `create` prepares the container but does not execute the
  user process;
- take an immutable copy or digest-bound representation of `config.json` at
  create time;
- undo create-time resources when create fails;
- preserve the required start, kill, delete, and hook ordering;
- reject unsupported configuration before mutating runtime state.

OCI Runtime Specification 1.3 includes a VM-specific configuration section
for hypervisor, kernel, initrd, image, and parameter intent. The runtime must
preserve those fields but may select only certified A3S hypervisor, kernel,
firmware, and system-image artifacts. It must reject an untrusted request to
execute an arbitrary path.

References:

- <https://github.com/opencontainers/runtime-spec/blob/main/runtime.md>
- <https://github.com/opencontainers/runtime-spec/blob/main/config.md>
- <https://github.com/opencontainers/runtime-spec/blob/main/config-linux.md>
- <https://github.com/opencontainers/runtime-spec/blob/main/config-vm.md>
- <https://github.com/opencontainers/runtime-spec/releases/tag/v1.3.0>

## Target architecture

```text
Box / OCI CLI / future containerd shim
                  |
             a3s-oci-sdk
                  |
           OCI runtime service
                  |
     durable state + operation journal
                  |
       +----------+---------------------+
       |                                |
NativeLinuxDriver                 LibkrunVmDriver
       |                                |
       |                       sandbox shim / VM owner
       |                                |
       |                    KVM / HVF / WHPX + virtio
       |                                |
       |                         a3s-oci-agent
       |                                |
       +---------- LinuxExecutor -------+
                         |
              namespaces / mounts / cgroups
              seccomp / capabilities / processes
```

### Runtime frontend

The runtime frontend owns:

- OCI bundle loading and schema validation;
- container and sandbox identifiers;
- lifecycle state transitions;
- operation idempotency and generation fencing;
- durable state and crash reconciliation;
- driver selection from explicit policy and capability evidence;
- optional hypervisor probing that cannot make the native Linux path fail;
- I/O attachment descriptors;
- feature and capability reporting;
- audit events and structured errors.

The Rust library is the source of truth. The `a3s-oci` command is a thin
frontend over the same library and implements runc-style lifecycle commands:

```text
a3s-oci create --bundle <path> <container-id>
a3s-oci start <container-id>
a3s-oci state <container-id>
a3s-oci kill <container-id> <signal>
a3s-oci delete <container-id>
a3s-oci run --bundle <path> <container-id>
a3s-oci exec <container-id> ...
a3s-oci pause <container-id>
a3s-oci resume <container-id>
a3s-oci features
```

The OCI specification defines operations rather than a mandatory CLI syntax.
CLI compatibility is therefore a tested integration surface, not the internal
architecture.

### Linux executor

`LinuxExecutor` is the security-critical implementation shared by the native
driver and guest agent. It owns:

- ID validation and protected runtime paths;
- rootfs and mount namespace creation;
- pivot-root or equivalent rootfs transition;
- PID, IPC, UTS, network, user, mount, and cgroup namespaces;
- UID/GID mappings and supplemental groups;
- cgroup v2 controller delegation and limit enforcement;
- capability bounding, effective, permitted, inheritable, and ambient sets;
- `no_new_privs` and seccomp installation;
- masked and read-only paths;
- device policy;
- init process supervision and zombie reaping;
- pidfd-based signal and exit observation where available;
- exec by joining the target container namespaces and cgroup;
- pause and resume through cgroup freezer semantics;
- terminal, pipes, console sockets, and file descriptor ownership;
- deterministic cleanup of mounts, namespaces, cgroups, and processes.

Requested controls are mandatory. The executor must not inherit the current
guest-init behavior where some cgroup or security failures degrade to warnings.
An explicitly requested limit or isolation control either has enforcement
evidence or launch fails.

### Utility VM driver

The libkrun VM driver owns:

- platform capability probing;
- selection of version-pinned libkrun and kernel artifacts;
- utility VM creation and lifetime;
- vCPU and memory configuration;
- a runtime-owned virtio-fs share;
- host-to-guest control transport over vsock and the Windows named-pipe bridge;
- dynamic process I/O relays;
- network proxy integration;
- VM crash detection and restart policy;
- one shim or daemon process that owns every OS handle required by the VM.

The utility VM boots a minimal, immutable system root with
`a3s-oci-agent` as the trusted service. Workload root filesystems remain
separate from the utility VM system root.

### Guest agent

The guest agent is a multi-container service, not a single-workload PID 1.
Its state model must replace process-wide globals with an indexed registry:

```text
ContainerId -> {
    generation,
    lifecycle_state,
    immutable_spec,
    namespace_fds,
    cgroup_path,
    rootfs_mount,
    init_pid,
    init_pidfd,
    process_registry,
    io_registry,
    exit_status
}
```

The agent protocol must provide:

- version and feature negotiation;
- create, start, state, kill, delete, exec, pause, resume, update, and wait;
- streaming stdin, stdout, and stderr with explicit close semantics;
- PTY resize and signal delivery;
- operation IDs and generation fences for safe retry;
- bounded frames, payloads, queues, and timeouts;
- structured errors with stable error classes;
- a complete inventory used during host crash recovery;
- heartbeat and graceful agent shutdown.

Every mutating request must be idempotent or explicitly reject a stale
operation generation.

### Shared utility VM ownership

One utility VM may contain multiple containers only when they share one
declared sandbox or trust-domain key.

The owner process must maintain:

- one machine-level lock;
- a durable sandbox record;
- a unique VM and agent generation;
- a reference count derived from durable container records, not only memory;
- idle shutdown with bounded cleanup;
- restart reconciliation that compares host records with agent inventory;
- a terminal quarantine state when the agent and host disagree in a way that
  cannot be repaired safely.

The first shared-VM release must not use one machine-wide VM for every caller.

## Repository layout

The initial repository should use only the crates required by different
compilation or deployment boundaries:

```text
OCI-Runtime/
|-- Cargo.toml
|-- crates/
|   |-- core/             # pure lifecycle, state, errors, protocol types
|   |-- sdk/              # complete async Rust client and OCI model boundary
|   |-- linux-executor/   # Linux-only isolation and process implementation
|   |-- agent/            # static Linux guest binary
|   |-- runtime/          # host orchestration and native/libkrun drivers
|   `-- cli/              # a3s-oci binary
|-- tests/
|   |-- conformance/
|   |-- differential/
|   |-- security/
|   `-- soak/
|-- docs/
|   |-- architecture.md
|   |-- threat-model.md
|   |-- protocol.md
|   |-- state-and-recovery.md
|   `-- oci-conformance.md
`-- scripts/
    |-- build-guest.ps1
    |-- package-runtime.ps1
    `-- real-host-smoke.*
```

Do not create separate crates for ordinary internal modules. Add a crate only
when a Linux guest binary, host binary, platform dependency, or reusable pure
contract requires a real build boundary.

The Linux packaging profile must support native-only builds and tests. A
combined native/KVM artifact may be shipped, but loading the binary and
selecting the native driver must not initialize libkrun or require access to
`/dev/kvm`.

## Reuse from A3S Box

The new repository should reuse behavior and tests, not copy Box ownership or
product dependencies blindly.

| Existing Box area | Runtime destination | Required change |
| --- | --- | --- |
| `core/src/vmm.rs` | `runtime` driver contracts | Generalize VM and process concepts; remove Box IDs, Box log config, TEE, and product paths |
| `shim` and libkrun launch | `runtime` utility-VM owner | Preserve handle inheritance, process identity, and cleanup behavior |
| `guest/init/src/namespace.rs` | `linux-executor` | Replace single-process assumptions; implement complete create/start barrier and namespace handles |
| `guest/init/src/cgroup.rs` | `linux-executor` | Replace best-effort enforcement with requested-control failure |
| `guest/init/src/user.rs` | `linux-executor` | Preserve fail-closed user resolution and supplemental groups |
| guest reaper and exec/PTY servers | `agent` and `linux-executor` | Replace global container state with per-container and per-process registries |
| sandbox `CrunController`/`CrunHandler` | `core` lifecycle tests | Reuse state, signal, pause/resume, cleanup, and recovery expectations rather than the process invocation |
| sandbox OCI compiler | Remains in Box | It is Box policy that generates a restricted OCI bundle |
| OCI image store/build/pull/push | Remains in Box | Image distribution is outside the OCI runtime boundary |
| Box managed execution store | Remains in Box | The runtime has its own lower-level state; Box retains product lifecycle state |

Code must be moved through reviewable commits that preserve tests. Do not
delete the Box implementation until the Box dependency has switched to a
released, pinned runtime revision.

## Filesystem design

Dynamic container creation cannot depend on hot-plugging one virtio-fs device
per container. A utility VM should boot with one runtime-owned parent directory
shared into a protected guest path. Each bundle and rootfs is stored beneath a
validated child directory.

Required controls:

- the host canonicalizes and allowlists every shared root;
- the agent resolves paths relative to pre-opened directory descriptors;
- Linux uses `openat2` resolution restrictions where available and a
  fail-closed equivalent otherwise;
- symlink, junction, reparse-point, and rename races have adversarial tests;
- a workload mount namespace sees only its requested rootfs and mounts;
- read-only mounts are enforced in the guest mount namespace;
- the trusted agent can see the parent share, but workload processes cannot;
- Windows POSIX UID/GID/mode metadata is captured and replayed across utility
  VM generations using the existing Box metadata-manifest behavior until the
  filesystem backend provides durable native metadata.

The MVP may require extracted directory rootfs input. Block-backed and
snapshotter-backed root filesystems are later drivers.

## Networking plan

Networking must be delivered in explicit stages:

1. `none`: a separate network namespace with loopback only;
2. sandbox-local: per-container veth pairs connected to a bridge inside the
   utility VM;
3. outbound access through one guest-to-host userspace network path;
4. dynamic published TCP ports through a host proxy and guest control service;
5. DNS, IPv6, UDP, policy, and named bridge integration.

The first shared-kernel proof must not share the agent's network namespace with
workloads. Unsupported networking modes fail before container creation.

The host proxy must support mappings added and removed after VM boot. Static
kernel-command-line or VM-creation-only port mappings are insufficient for a
long-lived shared utility VM.

## Durable state and recovery

Runtime state is separate from Box state and is stored beneath an explicit
runtime root:

```text
<runtime-root>/
|-- sandboxes/<sandbox-id>/
|   |-- state.json
|   |-- owner.lock
|   |-- vm.json
|   `-- control.endpoint
`-- containers/<container-id>/
    |-- state.json
    |-- operation.json
    |-- config.json
    |-- bundle.json
    `-- io.json
```

Every record:

- has a schema version and runtime build identity;
- is written through temporary-file, fsync, and atomic replacement semantics;
- includes a generation and pending operation;
- records the sandbox/VM generation and immutable config digest;
- records enough ownership evidence to reject PID or handle reuse;
- is protected by directory ownership and ACLs appropriate to the platform.

Recovery compares three sources:

1. durable host state;
2. live shim/VM process identity;
3. guest agent sandbox, container, and process inventory.

The reconciler may complete an idempotent pending operation, mark a known exit,
or clean a proven orphan. It must quarantine ambiguous state instead of
deleting resources speculatively.

## Box integration

Box must integrate through a narrow adapter implementing its local execution
backend contract. The first integration adds a new internal backend identity;
it does not overload the existing `Crun` value.

Required Box changes after the runtime repository reaches its first release:

- add an `A3sOci` execution backend identity;
- persist runtime version, driver, isolation class, feature set, and state root;
- translate Box's generated OCI bundle into runtime create/start operations;
- bridge Box exec, files, PTY, logs, stats, pause/resume, and stop semantics;
- report driver capability evidence through `info` and SDK inspection;
- resolve explicit Sandbox isolation to the native driver on Linux regardless
  of KVM availability;
- keep Box and every SDK loadable and usable for Sandbox isolation when the KVM
  device is missing, inaccessible, or rejected by the capability probe;
- preserve `crun` as an explicit internal rollback path during the migration;
- prohibit automatic fallback after a launch has mutated state;
- pin the runtime git revision, package version, artifact digest, guest agent,
  kernel, and libkrun build together.

The public SDK continues to select typed isolation intent. It must not expose a
raw string that chooses `crun`, WHPX, KVM, or another implementation.

## Development milestones

### M0: Contracts and threat model

Deliverables:

- independent repository skeleton;
- architecture and threat-model documents;
- OCI 1.3.0 conformance contract and generated schema-property inventory;
- complete async `a3s-oci-sdk` operation and model boundary;
- lifecycle state machine and recovery journal schemas;
- protocol schema with version negotiation;
- platform support and packaging matrix;
- ADR for trust-domain and utility-VM ownership.

Exit criteria:

- no runtime code depends on Box;
- every OCI 1.3.0 property has an implementation owner and applicability
  classification;
- no complete OCI model field is lost at the SDK service boundary;
- create/start ordering and rollback behavior are testable as pure state
  transitions;
- security review accepts the trust boundary and state directory model.

### M1: Native Linux lifecycle without KVM

Deliverables:

- `a3s-oci` create/start/state/kill/delete commands;
- native use of `LinuxExecutor`;
- protected runtime roots and durable lifecycle state;
- rootless user mappings;
- cgroup v2 delegation, seccomp, and capability probes;
- pre-opened listener and console descriptor handling required by Box;
- structured capability results that distinguish an absent `/dev/kvm`,
  insufficient permission, and an unusable KVM implementation;
- a native-only build and CI profile that does not initialize libkrun.

Exit criteria:

- a local Alpine bundle completes the exact create/start split on a host with
  `/dev/kvm` absent or deliberately inaccessible;
- natural exit and signal exit codes, state reconciliation, and deterministic
  cleanup pass without any attempt to open `/dev/kvm`;
- every control required by the initial A3S Sandbox profile has active
  enforcement evidence or fails before state mutation;
- rootful and rootless native smoke lanes pass on x86_64 and aarch64 Linux;
- installing and invoking `a3s-oci features` succeeds without KVM or libkrun
  initialization.

### M2: Single-container WHPX lifecycle

Deliverables:

- the M1 lifecycle commands routed through the libkrun driver;
- libkrun WHPX utility VM boot;
- static guest agent and version handshake;
- one extracted Linux rootfs over the protected virtio-fs share;
- non-interactive stdout/stderr and exit code propagation;
- durable host and guest lifecycle state.

Exit criteria:

- a local Alpine bundle completes the exact create/start split;
- natural exit and signal exit codes reach Windows unchanged;
- runtime restart reconciles running and stopped containers;
- failure injection leaves no VM, handle, mount, or state leak;
- tests run without WSL.

### M3: Shared utility VM

Deliverables:

- per-sandbox utility VM owner;
- multi-container guest registry;
- per-container PID, mount, IPC, UTS, user, cgroup, and network namespaces;
- independent cgroup resources and process supervision;
- concurrent lifecycle operations with generation fencing.

Exit criteria:

- two Alpine containers run concurrently in one WHPX VM;
- both report the same guest kernel;
- namespace and cgroup evidence differs for each container;
- killing, pausing, exhausting the PID limit, or deleting one container does
  not affect the other;
- an unrelated trust-domain key creates a different VM;
- agent restart and VM crash recovery have deterministic outcomes.

### M4: A3S lifecycle parity

Deliverables:

- exec and streaming stdin/stdout/stderr;
- PTY, resize, and signal forwarding;
- pause/resume and resource updates;
- file transfer and filesystem metadata operations;
- logs, wait, stats, health probe support, and graceful stop;
- initial networking stages and dynamic TCP publishing;
- Windows metadata capture and replay across VM generations.

Exit criteria:

- the existing Box SDK session behavior can be implemented without platform
  short-circuits;
- lifecycle operations pass restart and failure-injection tests;
- I/O backpressure and client disconnects do not leak processes or handles;
- unsupported features fail before state mutation.
- the A3S Sandbox bundle profile passes against both `crun` and the native
  `a3s-oci` driver on a Linux host without KVM;
- results match for lifecycle state, exit, signals, filesystem, user, resource,
  seccomp, capability, and cleanup tests;
- rootless and rootful certification lanes pass on x86_64 and aarch64 Linux.

### M5: Complete OCI 1.3 conformance

Deliverables:

- complete common, process, Linux, VM, state, feature, lifecycle, and hook
  semantics applicable to the advertised Linux-container drivers;
- generated property and normative-requirement coverage;
- upstream OCI schema and lifecycle validation;
- differential, security-negative, recovery, and soak evidence;
- exact feature output generated from the tested implementation.

Exit criteria:

- every applicable OCI Runtime Specification 1.3.0 MUST and MUST NOT has
  retained passing evidence;
- no schema property is unclassified or silently ignored;
- all unsupported workload-platform combinations fail before state mutation;
- feature output contains no untested capability;
- real WHPX and native Linux lanes pass the same lifecycle and configuration
  suites.

### M6: Box experimental backend

Deliverables:

- released and digest-pinned runtime and SDK artifacts;
- Box `A3sOci` backend adapter using `a3s-oci-sdk` only;
- explicit experimental capability reporting;
- dual-runtime differential CI;
- migration and rollback documentation.

Exit criteria:

- Box Rust, Python, and TypeScript SDK suites pass through the new backend;
- Box CLI and all three SDK Sandbox suites pass on Linux with `/dev/kvm`
  absent and inaccessible;
- real WHPX, HVF, KVM, and native Linux smoke lanes pass;
- no Box image, SDK, or product logic has moved into the runtime;
- rollback to certified `crun` is possible before a new execution starts.

### M7: Replace certified crun

Deliverables:

- security review and remediation;
- release soak evidence;
- upgrade and downgrade compatibility;
- default Box Sandbox resolution to `A3sOci`;
- removal plan for crun packaging and environment variables.

Exit criteria:

- all mandatory gates below pass for two consecutive release candidates;
- there are no unresolved critical or high security findings;
- crash and host-reboot recovery leaves no protected-resource leak;
- performance budgets are approved from measured baselines;
- the previous Box release can coexist with or safely reject the new runtime
  state schema;
- `crun` remains available for one deprecation release, then is removed.

### After M7: containerd integration

Containerd integration begins only after the complete OCI release and Box
migration gates pass.

Deliverables may include:

- containerd shim v2 with separate sandbox and task lifecycles;
- snapshotter and block-rootfs integration.

Containerd support must reuse the same lifecycle and SDK/service contracts
rather than introduce a second runtime implementation.

## Validation gates

### Pure and protocol gates

- lifecycle model property tests;
- operation replay, stale generation, and idempotency tests;
- schema compatibility and malformed-frame tests;
- bounded allocation, queue, path, and timeout tests;
- deterministic cleanup tests for every partial create step.

### Linux executor gates

- namespace identity and sharing matrix;
- mount propagation, readonly, masked path, device, and rootfs escape tests;
- UID/GID mapping and supplemental group tests;
- cgroup memory, CPU, PID, freezer, and OOM evidence;
- default and denied seccomp syscall tests;
- capability bounding and `no_new_privs` tests;
- pidfd signal and PID reuse tests;
- exec namespace and cgroup membership tests.

### Platform integration gates

| Platform | Required real-host evidence |
| --- | --- |
| Windows x86_64/WHPX | single and shared VM lifecycle, named-pipe/vsock transport, handle cleanup, virtio-fs metadata, dynamic port proxy, crash recovery |
| macOS arm64/HVF | single and shared VM lifecycle, virtio-fs, vsock, network, crash recovery |
| Linux x86_64/KVM | single and shared VM lifecycle, virtio-fs, vsock, network, crash recovery |
| Linux aarch64/KVM | architecture and rootless parity for supported features |
| Linux x86_64/no KVM | Box CLI plus Rust, Python, and TypeScript SDK Sandbox lifecycle; rootful/rootless OCI profile; security-negative matrix; cgroup delegation; host reboot recovery |
| Linux aarch64/no KVM | the same native behavior and SDK contract, with architecture-specific user-namespace, seccomp, and cgroup evidence |

The no-KVM lanes must run in two modes: `/dev/kvm` does not exist, and
`/dev/kvm` exists but is inaccessible. They must fail the test if the native
path attempts to initialize libkrun or open the device. A separate negative
test proves that an explicit MicroVM request returns a stable capability error
before image, bundle, or runtime-state mutation.

### Differential gates

Run the same immutable bundle and operation sequence through certified `crun`
and the native `a3s-oci` driver. Compare:

- state transitions and JSON state;
- process exit and signal results;
- stdout, stderr, PTY, and wait behavior;
- rootfs and mount visibility;
- user, group, capability, and seccomp behavior;
- cgroup limits, pause/resume, and OOM result;
- exec behavior;
- cleanup and residual host resources.

A difference must be explained by an OCI-permitted implementation choice or
the submitted workload platform being inapplicable; otherwise it must be
fixed.

### Security-negative gates

- bundle and container ID path traversal;
- symlink, hard-link, junction, reparse-point, and rename races;
- malicious mount sources and destinations;
- config replacement after create;
- stale operation and generation replay;
- PID, process handle, and sandbox owner reuse;
- namespace and cgroup escape attempts;
- host control endpoint access from a workload;
- cross-container rootfs, process, I/O, and network access;
- oversized or malformed protocol frames;
- guest compromise containment at the VM boundary;
- cleanup under process kill, VM crash, disk-full, and host reboot conditions.

### Soak and performance gates

- repeated create/start/exit/delete without resource growth;
- multi-container churn in one utility VM;
- concurrent exec and file operations;
- VM idle shutdown and cold restart;
- two-hour and twenty-four-hour platform profiles;
- host handle/file descriptor, process, thread, RSS, disk, mount, cgroup, and
  endpoint trend recording;
- measured cold VM, warm shared-VM container, exec, and delete latency budgets.

Performance budgets must be based on recorded real-host baselines. They must not
be invented from unit tests or one platform and applied universally.

## Known blockers and early decisions

### Windows SMP

The current reliable A3S WHPX path is limited to one vCPU. M2 and M3 may use
one vCPU for correctness, but shared-VM production promotion requires either:

- a validated multi-vCPU WHPX/libkrun path; or
- explicit capacity limits demonstrating that one vCPU meets the supported
  shared-sandbox workload profile.

The limitation must remain visible in capability output.

### Dynamic mounts

The first implementation uses one runtime-owned parent virtio-fs share because
libkrun devices are configured before VM entry. Per-container hot-plug is not a
dependency for the MVP.

### Dynamic networking

Existing per-VM static port mappings cannot serve a long-lived shared VM.
Dynamic port publishing requires a host proxy and guest control plane designed
for mappings that appear and disappear while the VM runs.

### Guest executor maturity

Existing guest-init code proves the needed primitives but is a single-container
implementation. It contains process-wide state and best-effort enforcement
that must not define the new runtime contract.

### OCI completeness tracking

M0 must publish a machine-readable feature description and a generated OCI
1.3.0 property inventory. A3S Box may depend only on features advertised by
the exact tested runtime build. During development, known but unenforced
properties are rejected before create. Production promotion requires every
applicable property and normative requirement to reach the conformant state.

## First implementation slices

The first two vertical slices are deliberately narrow and ordered so the shared
executor is proven before it becomes guest infrastructure.

### Slice 1: Native Linux without KVM

1. build `a3s-oci` and the lifecycle/state core;
2. run one fixed local Alpine bundle through `LinuxExecutor`;
3. implement the exact create/start boundary, state, kill, wait, and delete;
4. run with `/dev/kvm` absent and again with an inaccessible device;
5. prove required namespace, cgroup, user, capability, and seccomp controls;
6. terminate the runtime process and reconcile the surviving state;
7. verify that libkrun was not initialized and no residual process, mount,
   cgroup, endpoint, or temporary state remains after deletion.

### Slice 2: WHPX shared guest kernel

1. build `a3s-oci-agent` and a versioned protocol around the same
   `LinuxExecutor`;
2. boot one WHPX utility VM through `a3s-libkrun-sys`;
3. share one protected runtime directory;
4. create and start two fixed local Alpine bundles in that VM;
5. return state, logs, and exit codes;
6. prove the same guest kernel and distinct PID, mount, UTS, and cgroup
   identities;
7. kill and delete one container while the other continues;
8. terminate the owner process and reconcile the surviving state;
9. verify no residual Windows process, handle, endpoint, or temporary state
   after final deletion.

Image pulls, arbitrary networking, Compose, CRI, TEE, snapshots, and Box SDK
integration are excluded from these first two slices. Box and SDK integration
is a blocking M6 gate, including the no-KVM Linux matrix.

## Migration and rollback

- The new runtime is introduced behind an internal experimental backend.
- Until the native driver passes M5, existing non-KVM Linux Sandbox executions
  continue to use certified `crun`.
- New executions persist their runtime identity and can never be recovered by
  guessing a different backend.
- No automatic fallback occurs after create begins.
- Differential CI runs both runtimes from the same generated bundle.
- Box pins an exact runtime revision and artifact digest.
- A failed experimental release can route only new executions back to `crun`;
  existing `a3s-oci` state is reconciled or explicitly quarantined.
- Runtime state schemas support an explicit read/upgrade path and reject
  unsupported downgrade rather than corrupting state.
- `crun` packaging is removed only after the M7 deprecation release.

## Definition of done

The project replaces `crun` for A3S Box only when:

- Box no longer invokes or packages `crun` for supported Sandbox executions;
- `a3s-box --isolation sandbox` and the Rust, Python, and TypeScript SDK
  Sandbox APIs pass end to end on Linux where KVM is absent or inaccessible;
- the native Linux path neither opens `/dev/kvm` nor initializes libkrun;
- Linux native and shared-utility-VM paths use the same reviewed
  `LinuxExecutor`;
- Windows provides real shared-guest-kernel OCI execution without WSL;
- every applicable OCI Runtime Specification 1.3.0 property and normative
  requirement has retained conformance evidence;
- every requested namespace, mount, cgroup, seccomp, capability, user, and
  resource control has active evidence or fails closed;
- create/start/state/kill/delete, exec, I/O, pause/resume, update, wait, and
  cleanup survive process and host failure;
- Box SDK compatibility and all real-host release lanes pass;
- security-negative and soak gates have retained evidence;
- artifacts are reproducible, version matched, digest pinned, and accompanied
  by source provenance;
- public documentation distinguishes dedicated VM, shared guest kernel, and
  shared host kernel isolation.

Until all criteria pass, certified `crun` remains a supported rollback backend
and the new runtime remains experimental.
