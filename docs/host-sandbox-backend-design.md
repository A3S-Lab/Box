# Host Sandbox Backend Design

Status: **Implementation in progress**

Scope: architecture, implemented OCI runtime foundation, and remaining
lifecycle/security/performance gates

The certified `crun` launch path, protected OCI bundle construction, managed
create/start/kill lifecycle, exec, health, named volumes, shared memory, and
durable two-phase managed restart are implemented. This document remains the
source for unfinished hardening, lifecycle parity, and a3s-bench gates; it is
not a claim that the complete validation matrix has passed.

Target platform: Linux without `/dev/kvm`

## Executive decision

A3S Box should support Linux hosts without `/dev/kvm` through a second,
first-class execution backend. The backend should compile an A3S execution plan
into an OCI bundle and launch it with a pinned `crun` release.

The public API must describe the requested isolation posture, not the mechanism
used to implement it. The only new CLI form is:

```text
--isolation sandbox
```

Omitting `--isolation` preserves the existing MicroVM behavior. No explicit
selector is added for the default backend. `sandbox` is always an explicit
caller choice and is never selected because `/dev/kvm` is missing. The MVP has
no automatic backend selection or backend fallback. The resolved backend and
effective controls must be persisted before the workload starts.

This is not a replacement for the MicroVM backend. A host sandbox shares the
Linux kernel with the workload and therefore cannot provide a VM boundary
against kernel exploits. Workloads that need TEE, attestation, VM snapshots,
device assignment, or a hardware boundary remain MicroVM-only.

The intended product posture is a low-isolation sandbox for agent tools,
benchmark workloads, and development automation where filesystem, process,
network, syscall, and resource containment are useful, but a hostile
kernel-level adversary is outside the threat model.

## Design principles

1. Policies express filesystem, network, resource, and isolation intent rather
   than raw platform flags.
2. `probe` and `doctor` report real host capabilities before policy execution.
3. Strict postures fail closed when required controls are unavailable.
4. Posture selection, denials, and enforcement evidence are auditable.
5. The envelope records the policy digest, resolved posture, selected backend,
   and enforcement evidence.
6. Shared-kernel isolation is documented as weaker than a MicroVM for hostile
   tenants.
7. Privileged operations, process supervision, and Landlock setup are separated
   into small companion binaries.

A3S Box needs create/start/exec, restart, cgroup updates, persistent rootfs
state, volumes, PTY, health checks, and crash reconciliation. An OCI runtime
already implements most of that Linux container lifecycle correctly, so the
host-sandbox backend should compile policy into OCI rather than construct an
ad-hoc process wrapper command line.

`crun` is the initial production backend because it supports OCI bundles,
rootless user mappings, namespaces, capabilities, seccomp, cgroup v2, and
passing pre-opened file descriptors into a container. Only one tested version
should be accepted in the first release. The resolver must not silently select
an arbitrary host `crun` or `runc` binary.

## Goals

- Run the normal A3S Box process lifecycle on Linux hosts without `/dev/kvm`.
- Preserve the existing CLI, SDK, state, exec, PTY, log, and health protocols
  wherever the security model permits it.
- Make isolation selection deterministic, inspectable, and fail closed.
- Keep VM-specific concepts out of the public execution contract.
- Produce a package that does not build, link, or ship libkrun when only the
  host-sandbox backend is selected.
- Enforce user, mount, PID, IPC, UTS, and network isolation; seccomp;
  capabilities; `no_new_privs`; and cgroup v2 limits.
- Preserve image UID/GID metadata instead of flattening the image to one host
  user.
- Survive launch failures, shim termination, and host reboot without leaked
  mounts, cgroups, sockets, bundles, or processes.

## Non-goals for the first release

- Claiming that a shared-kernel sandbox is equivalent to a MicroVM.
- TEE, attestation, sealed storage, or confidential-computing workflows.
- Snapshot-fork, KSM, warm-VM pools, or live migration.
- `--privileged`, arbitrary device or GPU passthrough, or host PID namespace.
- Named bridge networking, TSI, inbound port publishing, or sidecar/vsock
  services.
- CRI RuntimeClass support before the standalone lifecycle is stable.
- Automatic backend selection or fallback.
- gVisor/runsc or any user-space-kernel backend.
- Running a nested OCI runtime inside a fully privileged Docker container as the
  production security boundary.

## Threat model

The host-sandbox backend is intended to contain accidental damage and malicious
user-space workloads that do not possess a working Linux kernel exploit. The
trusted computing base includes:

- the host Linux kernel;
- the A3S Box runtime and sandbox shim;
- the execution-plan and OCI-bundle compilers;
- the pinned `crun` binary and its required libraries;
- the host rootfs and content stores;
- any explicitly exposed host path or network service.

The backend must protect host processes, filesystem paths, runtime sockets,
devices, cgroups, and network namespaces from the workload. It does not protect
the host from kernel vulnerabilities, hardware side channels, a hostile host
administrator, or data deliberately mounted into the sandbox.

MicroVM must remain the default for untrusted multi-tenant workloads. The
host-sandbox mode deliberately trades isolation strength for lower overhead and
broader Linux compatibility.

## Public isolation contract

### Isolation selection

| CLI input | Resolution |
| --- | --- |
| option omitted | Select krun only. Missing KVM/HVF/WHPX or a required VM feature is an error. |
| `--isolation sandbox` | Select the certified OCI host-sandbox backend only. The caller has explicitly accepted a shared kernel. |

The effective default is `microvm`, preserving the current security posture,
but it is an internal and persisted value rather than a new CLI spelling. The
public value `sandbox` stays concise, while state and audit output always
expose `isolation_class=shared-kernel`.

The equivalent ACL setting is explicit only for the sandbox backend:

```acl
runtime {
  isolation = "sandbox"
}
```

Omitting `runtime.isolation` selects MicroVM. Configuration parsers reject any
other explicit value so a typo cannot weaken or silently change isolation.

### Isolation class

Backend identity and isolation strength are separate fields:

| Backend | Isolation class |
| --- | --- |
| krun | `hardware-vm` |
| crun | `shared-kernel` |

This prevents a backend name from hiding a security-boundary change.

### Requirement extraction

Before choosing a backend, `RequirementExtractor` converts all user-facing
options into explicit requirements. Examples include:

- hardware isolation;
- TEE, attestation, or sealed storage;
- snapshot/fork or warm-pool semantics;
- devices, GPU, or privileged mode;
- network intent and published ports;
- filesystem mounts and ownership mappings;
- exec, PTY, health, restart, and persistence lifecycle requirements;
- CPU, memory, PID, and other resource guarantees;
- requested seccomp and capability posture.

Backend selection must not inspect scattered CLI flags directly. A requirement
is either enforced or rejected; it is never ignored.

## Architecture

```text
CLI / SDK / CRI / a3s-bench
             |
       ExecutionRequest
             |
   +---------+------------------+
   | CapabilityProbe            |
   | RequirementExtractor       |
   | ExecutionPolicy            |
   +---------+------------------+
             |
       BackendResolver
      (pure and deterministic)
             |
     ResolvedExecutionPlan
   + policy digest + audit record
             |
       persist resolution
             |
   +---------+-------------------------+
   |                                   |
KrunBackend                    OciSandboxBackend
   |                                   |
a3s-box-krun-shim              a3s-box-sandbox-shim
   |                                   |
libkrun + guest-init           pinned crun + guest-init
vsock control                  inherited Unix listener FDs
```

### Core model

`ExecutionRequest` is backend neutral and contains process, rootfs, mount,
network, resource, security, lifecycle, and isolation intent.

`CapabilitySnapshot` is the result of active host probes. It includes the probe
version and enough evidence to explain why a control is available or missing.

`ResolvedExecutionPlan` is immutable after resolution and contains at least:

```text
request_digest
requested_isolation
resolved_backend
isolation_class
process_plan
rootfs_plan
mount_plan
network_plan
resource_plan
security_plan
resolved_controls
unenforced_controls
capability_snapshot_digest
backend_artifact_digest
```

The plan is compiled into the existing VM-specific `InstanceSpec` for krun or
an OCI `config.json` and bundle for crun. VM concepts such as virtio-fs tags,
vsock ports, guest kernel paths, and krun snapshot sockets remain inside the
krun compiler.

### Backend interface

The neutral execution interface owns lifecycle operations rather than VMM
operations:

```text
prepare(plan) -> PreparedExecution
start(prepared) -> ExecutionHandle
exec(handle, process)
signal(handle, signal)
wait(handle)
stats(handle)
stop(handle, timeout)
delete(handle)
```

`ExecutionHandle` is a versioned tagged enum. Common state includes the box ID,
shim PID/pidfd, control endpoint, state directory, and plan digest. Backend
variants store either krun state or OCI container/bundle state.

The existing `VmmProvider` remains available behind `KrunBackend` during the
migration. Existing third-party implementations are not broken in the first
phase. `VmManager` can remain as a compatibility facade while new code moves to
an `ExecutionManager`.

## Capability probing and resolution

The current platform code treats every Unix host as krun-capable. That must be
replaced by active probes. Kernel version checks and file-existence checks alone
are insufficient.

### Probe inputs

The Linux probe should test, in a disposable child and clean up after itself:

- `/dev/kvm` open and a minimal KVM capability query;
- exact krun shim and libkrun artifact availability;
- exact `crun` path, version, digest, and required feature set;
- creation of user, mount, PID, IPC, UTS, and network namespaces;
- complete subordinate UID/GID mapping through `newuidmap`/`newgidmap` when
  rootless;
- cgroup v2 controller availability and actual write delegation;
- seccomp filter installation with `no_new_privs`;
- Landlock ABI and a minimal ruleset when required;
- overlayfs, rootless overlayfs, or the selected copy fallback;
- idmapped mount support when the rootfs plan requires it;
- Unix listener FD preservation through the certified crun build;
- required network helper availability for each network intent.

`a3s-box probe --json` should return raw capability evidence. `a3s-box doctor`
should evaluate the requested isolation mode and return actionable errors. A
successful `doctor` result is not cached forever; the launch path performs the
security-critical probes again or validates a short-lived, versioned snapshot.

### Resolver rules

`BackendResolver` is a pure function of the request, extracted requirements,
execution policy, and capability snapshot. Its output is either one complete
plan or one machine-readable error. It must never partially mutate box state.

Recommended denial classes are:

```text
BACKEND_UNAVAILABLE
UNSUPPORTED_REQUIREMENT
REQUIRED_CONTROL_UNAVAILABLE
BACKEND_ARTIFACT_MISMATCH
```

The resolver runs before rootfs preparation or any long-lived side effect. Its
decision is persisted before backend preparation begins.

## OCI host-sandbox backend

### Process layout

```text
a3s-box CLI or service
        |
        +-- a3s-box-sandbox-shim <box-id>
                |
                +-- crun create/start <box-id>
                        |
                        +-- guest-init (container PID 1)
                                |
                                +-- user workload
```

The shim owns the OCI container lifecycle, control listeners, stdio pipes,
ready handshake, and durable cleanup metadata. The workload must not be able to
replace the shim or mutate its bundle after validation.

### Structured log lifecycle

Each running Sandbox generation owns a separate packaged log worker alongside
`crun run`. The runtime opens independent raw console files for container
stdout and stderr, and the worker tails both into Docker-compatible
`logs/container.json` records without losing the `stdout` or `stderr` stream
field. Runtime and init diagnostics use their dedicated log and are never
projected as workload output.

The worker becomes ready only after both console readers are open. It watches
the exact `crun run` PID and Linux process start time recorded for that
generation, so PID reuse cannot end or extend another generation's logging.
Once that writer is gone or is an unreaped zombie, its output descriptors are
closed and the worker drains both files through final EOF, including a trailing
partial line. The runtime record persists the worker PID and start time so an
explicit stop, kill, detached natural-exit reconciliation, or crash recovery can
wait for the same generation to finish before deleting its artifacts.

Auto-remove archival happens only after that drain completes. Consequently,
`a3s-box logs <name-or-id>` reads complete structured output from
`removed-logs`, even after the box directory and crun state have been removed.
Failure to prove that the worker finished is a cleanup error: state is retained
for recovery instead of silently archiving an incomplete log.

### OCI bundle compiler

The compiler writes a protected, per-box bundle from the resolved plan. It must
generate, rather than accept unchecked user JSON:

- process args, environment, workdir, user, rlimits, and terminal mode;
- root path and read-only setting;
- user, mount, PID, IPC, UTS, cgroup, and network namespaces;
- full UID/GID mappings;
- validated bind mounts, proc, devpts, tmpfs, and minimal `/dev` nodes;
- capability bounding/permitted/effective/ambient sets;
- mandatory `noNewPrivileges`;
- a host-sandbox-specific seccomp profile;
- cgroup v2 path and resource settings;
- masked and read-only kernel paths;
- A3S annotations containing the plan and artifact digests.

The bundle directory is owned by the shim/service and is not mounted writable
inside the container. The generated config is hashed after writing and checked
again immediately before `crun create`.

### guest-init bootstrap modes

guest-init currently assumes that it is booting inside a MicroVM. It should
gain two independent internal settings:

```text
BootstrapMode: MicroVm | HostSandbox
ControlTransport: Vsock | InheritedUnixFd
```

In `HostSandbox` mode, the OCI runtime has already created the final rootfs and
mounted proc, devpts, tmpfs, workspace, and user volumes. guest-init therefore
must skip:

- virtio-fs discovery and mounts;
- rootfs pivoting;
- guest-global cgroup hierarchy setup;
- TEE, vsock sidecars, and VM network initialization.

It remains PID 1 and continues to supervise the main process, reap children,
apply the requested workload UID/GID, handle signals, and provide the existing
exec, PTY, health, and copy protocols.

Bootstrap settings should be supplied through a sealed memfd or protected
read-only descriptor, not ordinary user-controlled environment variables.

### Control transport

The sandbox shim creates the host-visible Unix listeners before starting crun
and passes the open descriptors with crun's `--preserve-fds` support. guest-init
constructs `ExecListener` and `PtyListener` from those descriptors, sets
`FD_CLOEXEC`, and never exposes them to the workload process.

This approach preserves the existing host socket paths and wire protocols while
avoiding a bind mount of the A3S control directory into the sandbox. If the
certified crun build cannot pass listeners reliably, that build fails the
capability probe; a less secure socket-directory mount is not a silent fallback.

Init diagnostics should use a dedicated inherited log descriptor. They must not
be mixed with the workload's stdout or stderr.

## Rootfs and ownership

The current `RootfsProvider` remains the source of the prepared rootfs view, but
ownership mapping becomes part of `RootfsPlan` rather than a backend afterthought.

Requirements:

- Determine the UID/GID range needed by the image before materialization.
- Require a mapping that covers all preserved image owners.
- Prefer idmapped mounts when supported and validated.
- Otherwise extract or replay ownership inside the correct user namespace.
- Never silently convert every image owner to the invoking host UID.
- Reject a launch when ownership cannot be represented safely.
- Keep overlay lower, upper, work, and merged paths in the durable cleanup
  ledger.

The same content store can serve both backends, but the materialized view and
ownership strategy may differ. The resolved plan records the selected strategy.

## Network intent

The public model should express intent rather than TSI, passt, or a particular
helper:

```text
default | none | egress | host
```

Initial resolution:

| Intent | MicroVM | Host sandbox MVP |
| --- | --- | --- |
| `default` | Existing TSI behavior | Isolated network namespace with loopback only |
| `none` | No guest egress | Isolated network namespace with loopback only |
| `egress` | Existing supported VM egress | Deferred until a pinned pasta/slirp design is implemented |
| `host` | Existing host-equivalent behavior where supported | Explicitly share the host network namespace; warn and audit |

`host` must never imply host filesystem or runtime-socket access. Port
publishing and named bridge networks remain unsupported for the host-sandbox
MVP. A later egress proxy should be represented as a compiler result, not a new
public isolation backend.

## Mandatory security controls

The host-sandbox backend is available only when all mandatory controls can be
applied. It must not use the weaker seccomp deny list that currently runs inside
the VM as its host security boundary.

### Namespaces and identity

- A user namespace is mandatory, including for a root-run production service.
- Mount, PID, IPC, and UTS namespaces are mandatory.
- A network namespace is mandatory unless the caller explicitly requests
  `host` networking.
- Container root must not map to host root.
- Complete UID/GID mapping is mandatory for multi-owner images.

### Privileges and syscalls

- `no_new_privs` is mandatory.
- Start from an empty or minimal capability set; additions are allowlisted.
- Reject `--privileged`.
- Reject `seccomp=unconfined` and arbitrary custom profiles in the MVP.
- Compile and pin an OCI seccomp profile for guest-init plus the workload.
- Treat Landlock as defense in depth. Record its ABI and coverage; never use it
  to compensate for a missing mandatory namespace or seccomp control.

### Resources

- Use cgroup v2 only.
- Attach guest-init to the final cgroup before it starts the workload.
- Enforce a baseline PID limit even when the caller did not specify one.
- Reject each requested memory, CPU, PID, or cpuset guarantee that cannot be
  applied; do not log-and-continue.
- Record the exact controller files and values that were written.

### Filesystem

- Validate mount sources against policy before bundle generation.
- Reject symlink traversal and revalidate source identity before launch.
- Mask or omit sensitive proc/sys paths and mount sysfs read-only only when
  required.
- Do not expose host `/run`, container runtime sockets, A3S control sockets,
  arbitrary devices, D-Bus sockets, or the Docker/Podman/containerd/OrbStack
  sockets by default.
- Reject mounts of `/`, `/proc`, `/sys`, `/dev`, runtime state directories, and
  other protected paths unless a future explicit high-risk policy defines them.

## State, audit, and lifecycle

### Durable state

`BoxConfig` records the request. `BoxRecord` records the resolution and runtime
evidence. At minimum it needs:

```text
requested_isolation
resolved_backend
isolation_class
execution_policy_digest
resolved_controls
unenforced_controls
capability_snapshot
backend_artifact_digest
backend_state
```

Old records deserialize with `requested_isolation=microvm`. `inspect` and
structured logs display both requested and resolved values.

### Lifecycle states

```text
requested -> resolved -> preparing -> created -> running
                                      |          |
                                      v          v
                                    failed    stopped/dead
                                                   |
                                                   v
                                                deleted
```

Every side effect is appended to a durable resource ledger before the next side
effect starts. Failure unwinds the ledger in reverse order. Entries cover:

- bundle and state directories;
- rootfs overlay mounts and temporary mounts;
- cgroup paths;
- control sockets and preserved descriptors;
- network namespace/helper processes;
- crun container ID and init PID/pidfd;
- named and anonymous volume attachments.

Use pidfds where supported rather than trusting a persisted numeric PID. On
restart, reconciliation compares the plan digest, runtime state, pidfd or
process start identity, cgroup membership, mounts, and crun state before marking
a box alive. Cleanup operations are idempotent.

### Audit envelope

Each launch emits a stable structured record containing:

- request and policy digests;
- requested isolation and resolved backend;
- isolation class and explicit shared-kernel acknowledgement;
- capability probe version and evidence digest;
- every required control and its enforcement evidence;
- every optional control that was unavailable;
- pinned runtime version and artifact digest;
- lifecycle outcome and cleanup outcome.

An audit or diagnostic mode must not silently weaken enforcement. If a future
seccomp learning mode permits syscalls for observation, it must be a separate,
explicitly unsafe execution posture and cannot satisfy production acceptance.

## Feature compatibility

| Feature | krun MicroVM | Host-sandbox launch target |
| --- | --- | --- |
| run, foreground, detach | Supported | MVP |
| exec and non-TTY streams | Supported | MVP |
| PTY, shell, attach | Supported | MVP |
| logs and exit code | Supported | Split structured stdout/stderr, final drain, detached recovery, and auto-remove archival implemented |
| stop, kill, wait, restart | Supported | MVP |
| health checks | Supported | MVP |
| numeric user/workdir/env | Supported | MVP |
| bind mounts, named volumes, tmpfs | Supported | MVP with path policy |
| read-only rootfs | Supported | MVP |
| memory, CPU, PID limits | Guest cgroup | MVP through OCI cgroup v2 |
| default network | TSI | None/loopback |
| explicit host network | Platform-dependent | MVP with warning/audit |
| isolated outbound egress | Supported modes | Post-MVP pasta/slirp or proxy |
| published ports, named bridge | Supported modes | Deferred |
| commit and diff | Supported | Post-MVP parity gate |
| pause/unpause | Supported behavior | Implemented through certified `crun pause`/`crun resume`, with durable generation fencing and cgroup v2 freezer validation |
| TEE/attestation/sealing | MicroVM-only | Rejected |
| snapshot-fork/warm VM pool | MicroVM-only | Rejected |
| device/GPU/privileged | Restricted/roadmap | Rejected |
| CRI RuntimeClass | Existing roadmap | Deferred |

Unsupported combinations fail before rootfs preparation and state mutation.

## Packaging and deployment

Runtime pieces should remain independently distributable:

```text
a3s-box                     common CLI/control plane; no libkrun linkage
a3s-box-krun-shim           optional libkrun-linked MicroVM shim
a3s-box-sandbox-shim        OCI lifecycle shim; no libkrun linkage
crun                        pinned, verified host-sandbox artifact
guest-init                  shared protocol implementation with two modes
```

Recommended release profiles are `full` and `sandbox`. CI verifies that
the sandbox archive and image contain no libkrun library or krun shim and
that the common CLI and sandbox shim have no dynamic libkrun dependency.

Docker can provide reproducible BuildKit builds and transport release artifacts
to A3S OS. The host runtime itself should execute at the host service layer with
the specific namespace, cgroup, and mount authority it needs. Requiring
`--privileged`, mounting the entire host filesystem, or exposing the Docker
socket to a nested runtime is not an acceptable production design.

For server validation, the A3S OS host should clone the repository once and use
`git fetch`/`git pull` for each revision. Source trees and large build outputs do
not need to be uploaded from a developer laptop.

## a3s-bench integration

The current a3s-bench Box integration performs preflight only. It should not be
used as proof that the new backend executes workloads until a real Box runner is
implemented.

The future runner should submit the same `ExecutionRequest` used by the CLI and
must not invoke crun directly. A benchmark result records:

```text
requested_isolation
resolved_backend
isolation_class
execution_policy_digest
runtime_version
```

Backend correctness is first validated directly through A3S Box. Bench
integration becomes a later acceptance layer after run/exec/stop and cleanup
are reliable.

## Remote SDK compatibility boundary

The E2B-compatible control and data planes described in
[`e2b-compatible-sdk-design.md`](e2b-compatible-sdk-design.md) are consumers of
the backend-neutral `ExecutionManager`; they are not part of the OCI backend.
They must never call `crun` or the sandbox shim directly.

The host-sandbox backend supports lifecycle, commands, PTY, files, Code
Interpreter, memory-preserving pause/resume, and filesystem-only pause/resume
incrementally, but it is not certified for the complete remote SDK surface.
Protocol compatibility and backend certification are recorded separately so a
matching HTTP shape cannot conceal a missing runtime guarantee. The warm-pause
path has certified A3S OS evidence; the extended cold-pause client matrix still
requires its next certified-host run.

## Delivery phases and gates

### Phase 0: architecture and threat model

- Approve this document, public isolation terminology, and security boundary.
- Produce the requirement/feature matrix and threat-model review.
- Select and record the certified crun version and artifact verification policy.

Gate: security and runtime owners agree on what `sandbox` does and does not
promise. No production behavior changes.

### Phase 1: neutral plan, probe, resolver, and audit

- Add neutral execution types and the pure resolver.
- Add active `probe` and `doctor` output.
- Persist requested and resolved isolation fields.
- Adapt the existing `VmmProvider` through `KrunBackend`.

Gate: all resolver combinations are unit tested and existing requests still
resolve to krun by default.

### Phase 2: guest-init separation

- Separate bootstrap mode from control transport.
- Add sealed bootstrap descriptor parsing.
- Add inherited Unix listener support for exec and PTY.
- Keep all existing wire protocols unchanged.

Gate: guest-init transport tests run without a VM, and the MicroVM path has no
behavior regression.

### Phase 3: minimal OCI backend

- Add sandbox shim and protected OCI bundle compiler.
- Implement create/start, readiness, exec, logs, stop, wait, and delete.
- Add full UID/GID mapping and basic rootfs providers.

Gate: a no-KVM Linux host passes run/detach/exec/PTY/logs/stop/rm with no leaked
resources.

### Phase 4: security and failure hardening

- Complete seccomp, capability, cgroup, Landlock, mount-path, and network gates.
- Add resource ledger, pidfd tracking, crash recovery, and reboot cleanup.
- Add negative escape and resource-exhaustion tests.

Gate: mandatory controls are proven by negative tests, not just configuration
inspection.

### Phase 5: lifecycle parity and a3s-bench

- Add health/restart, volumes/tmpfs, read-only rootfs, persistence, commit/diff,
  and concurrency coverage in the agreed order.
- Implement the a3s-bench Box execution adapter.
- Run compatibility and performance suites on the A3S OS server.

Gate: the documented compatibility matrix matches observed behavior and every
benchmark result identifies the actual isolation backend.

## Validation matrix

### Functional

- run, foreground, detach, exit-code propagation, and auto-remove;
- exec, PTY, shell, attach, logs/follow, signals, stop, restart, and health;
- bind mounts, named/anonymous volumes, tmpfs, numeric users, read-only rootfs;
- memory OOM, CPU quota/weight, PID limit/fork bomb, and cpuset when supported;
- state reconciliation after shim SIGKILL and host reboot;
- concurrent start/stop/delete and PID-reuse scenarios.

### Security negatives

- read/write attempts against unmounted host files and protected paths;
- visibility of host PIDs, IPC objects, cgroups, devices, and network interfaces;
- access to Docker, containerd, Podman, OrbStack, D-Bus, and A3S control sockets;
- namespace creation, mount, keyring, ptrace, BPF, perf, io_uring, and other
  syscall cases covered by the selected seccomp threat model;
- symlink swaps and mount-source time-of-check/time-of-use attempts;
- capability escalation, setuid binaries, and nested runtime attempts;
- inability to escape resource limits under exec and restart.

### Cleanup

- failure after each preparation step;
- crun create/start failure;
- guest-init readiness timeout;
- shim SIGTERM and SIGKILL;
- workload fork storm and OOM;
- host reboot followed by reconciliation;
- verification that no process, mount, cgroup, socket, bundle, overlay, or
  anonymous volume remains unexpectedly.

### Performance

Measure each phase independently:

```text
probe + resolve
rootfs materialization
OCI bundle compilation
crun create/start
guest-init ready
first exec
stop/delete cleanup
```

Report cold-start p50/p95/p99, first-exec latency, steady RSS, CPU overhead,
filesystem throughput, network throughput, and high-concurrency behavior. The
initial overhead gate should compare A3S Box host-sandbox orchestration with the
same pinned crun and identical rootfs, excluding image download. Absolute
targets should be set from an A3S OS production-host baseline rather than a
developer laptop.

### Environments

- no-KVM Linux CI runner for deterministic unit and integration coverage;
- A3S OS production-like server for privileged kernel controls, cleanup, soak,
  and performance validation;
- existing KVM/HVF hosts for MicroVM regression coverage;
- developer laptops only for lightweight formatting and pure tests.

## Alternatives considered

### Bubblewrap as the production backend

Bubblewrap is useful for a proof of concept and demonstrates the Linux
namespace and mount mechanisms directly. Its own documentation describes it as
a mechanism for constructing sandboxes, not a complete policy or lifecycle
runtime. A3S Box would have to reimplement OCI ownership mapping, cgroups,
capabilities, seccomp, exec lifecycle, state reconciliation, and cleanup. It is
therefore not the initial production backend.

### gVisor systrap first

Systrap does not require KVM and provides a user-space kernel boundary. It is a
different, stronger-isolation product with syscall compatibility and performance
trade-offs. It is outside the low-isolation host-sandbox scope.

### QEMU TCG

TCG retains a VM boundary without KVM but has substantially different
performance and device plumbing. It is appropriate for CI, cross-architecture
compatibility, or diagnostics, not the low-isolation production execution path.

### Direct host process or chroot

A chroot is not a security boundary and does not provide the required process,
network, syscall, identity, or resource isolation. This option is rejected.

### Select any installed OCI runtime

Different runtime versions have different rootless, seccomp, cgroup, idmapped
mount, and FD-passing behavior. Silent runtime selection makes security and
reproduction unverifiable. The first release accepts only an explicitly
certified artifact.

## Evidence and references

- [Bubblewrap security model](https://github.com/containers/bubblewrap/blob/main/README.md#sandbox-security)
- [crun command and cgroup reference](https://github.com/containers/crun/blob/main/crun.1.md)
- [OCI Runtime Specification](https://github.com/opencontainers/runtime-spec)
- [gVisor platform guide](https://gvisor.dev/docs/user_guide/platforms/)

Relevant current A3S Box extension points:

- `src/core/src/vmm.rs`: VM-specific `InstanceSpec` and `VmmProvider`.
- `src/core/src/platform.rs`: static platform capability reporting.
- `src/runtime/src/vm/mod.rs`: unconditional krun shim selection.
- `src/runtime/src/rootfs/provider.rs`: reusable rootfs provider boundary.
- `src/guest/init/src/main.rs`: VM-specific bootstrap and mounts.
- `src/guest/init/src/exec_server.rs`: vsock-only exec listener.
- `src/guest/init/src/pty_server.rs`: vsock-only PTY listener.
- `src/cli/src/state/mod.rs`: persisted box state requiring resolution fields.
- [A3S-Lab/Bench README](https://github.com/A3S-Lab/Bench/blob/main/README.md):
  current a3s-bench Box provider preflight boundary.
