# Host Sandbox Backend Design

Status: **A3S OCI Runtime is the sole Sandbox backend**

Scope: Linux shared-kernel isolation for explicit `sandbox` requests

Target platform: qualified Linux x86_64 and aarch64 hosts, including hosts
without `/dev/kvm`

## Executive decision

A3S Box has two execution classes:

| Request | Backend | Isolation boundary |
| --- | --- | --- |
| default or `microvm` | libkrun | dedicated guest kernel |
| `sandbox` | A3S OCI Runtime | shared Linux host kernel |

The caller selects the isolation class, not a concrete runtime. Every new
`sandbox` request resolves to A3S OCI Runtime. There is no public or internal
rollback selector and no automatic fallback between Sandbox and MicroVM
isolation.

Omitting `--isolation` preserves the default MicroVM behavior. Missing KVM does
not silently change that request. A caller that accepts shared-host-kernel
isolation must request it explicitly:

```text
--isolation sandbox
```

A Sandbox cannot provide a VM boundary against kernel exploits. Workloads that
need TEE, attestation, VM snapshots, device assignment, or a hardware boundary
remain MicroVM-only.

## Design principles

1. Public policies express isolation, filesystem, network, initialization, and
   resource intent rather than runtime command-line flags.
2. The resolved backend and effective controls are durable before a workload
   starts.
3. Capability and artifact checks fail closed.
4. Lifecycle operations are generation-fenced and use typed runtime requests.
5. Runtime paths, sockets, mounts, cgroups, processes, and logs have one owner.
6. Recovery validates durable identity before inspecting or changing live host
   state.
7. Shared-kernel isolation is described as weaker than a MicroVM boundary.

## Goals

- Run the normal Box lifecycle on qualified Linux hosts without KVM.
- Preserve the CLI, SDK, exec, PTY, files, logs, metrics, health, snapshots,
  volumes, and restart contracts where the security model permits them.
- Enforce user, mount, PID, IPC, UTS, and network namespaces; seccomp;
  capabilities; `no_new_privs`; and cgroup v2 limits.
- Preserve image UID/GID metadata.
- Create a private network namespace for every current Box Sandbox and qualify
  the underlying runtime's private, exact-host, and donor-shared namespace
  operations before any future Box API exposes them.
- Support read-write and read-only bind volumes plus isolated tmpfs mounts.
- Support inline scripts, executable init files, and direct argument vectors.
- Recover or clean launch failures without leaked runtime state.

## Non-goals

- Equating a shared Linux kernel with MicroVM isolation.
- Automatic isolation selection or backend fallback.
- Host PID namespace, arbitrary devices, or an unrestricted privileged mode.
- Treating a runtime binary found on `PATH` as a supported backend.
- Allowing SDKs or protocol adapters to invoke the runtime directly.

## Threat model

The Sandbox backend contains accidental damage and malicious user-space
workloads that do not possess a working Linux kernel exploit. Its trusted
computing base includes:

- the host Linux kernel;
- A3S Box and the Sandbox shim;
- the pinned A3S OCI Runtime and agent artifacts;
- the execution-plan and OCI-bundle compilers;
- the host rootfs, content, and state stores;
- explicitly exposed host paths and network services.

It protects host processes, filesystem paths, runtime sockets, devices,
cgroups, and unrelated network namespaces from the workload. It does not
protect against host-kernel vulnerabilities, hardware side channels, a hostile
host administrator, or data deliberately mounted into the Sandbox.

MicroVM remains the default for hostile multi-tenant workloads.

## Public isolation contract

`BoxConfig` records `IsolationLevel`, not a runtime brand. The execution plan
resolves `IsolationLevel::Sandbox` to `ExecutionBackend::A3sOci`. CLI and SDK
callers cannot override this mapping with an environment variable, executable
path, or runtime selector.

The resolved execution plan records enough information to reproduce and audit
the decision:

- isolation class and backend;
- image and rootfs identity;
- owner and execution generation;
- resource, namespace, mount, and initialization intent;
- runtime and agent artifact digests.

Unsupported hosts reject `sandbox` before workload launch. Box never converts
the request to a MicroVM or a direct host process.

## Architecture

```text
CLI / Rust SDK / Python SDK / TypeScript SDK
                         |
                         v
              LocalExecutionManager
                         |
                         v
             durable execution plan
                         |
                         v
              Sandbox bundle compiler
                         |
                         v
           A3sOciController + typed SDK
                         |
             authenticated owner session
                         |
                         v
              A3S OCI Runtime + agent
                         |
                         v
                  guest-init / envd
```

Public callers depend on backend-neutral execution interfaces.
`A3sOciController` owns launch and `A3sOciHandler` owns the live lifecycle.
Callers never execute a runtime binary directly.

### Bundle compilation

Box compiles the resolved plan into a protected OCI bundle containing:

- the rootfs and resolved image entrypoint;
- namespace and UID/GID mappings;
- capabilities, seccomp, and `no_new_privs`;
- cgroup v2 CPU, memory, PID, and I/O settings;
- bind, volume, and tmpfs mounts;
- environment and working directory;
- guest-init bootstrap material;
- Box control sockets and log paths.

For the versioned `control-workload-v1` layout, `linux.resources` is the exact
workload source of truth. Box adds only typed control-plane headroom
annotations. A3S OCI Runtime derives the outer envelope, creates the fixed
`a3s-control` and `a3s-workload` children, and passes their pre-opened
`cgroup.procs` descriptors to Guest Init. The cgroup mount inside the Sandbox is
read-only; Guest Init moves trusted bootstrap processes into the control child
and joins every main, exec, streaming exec, and PTY process to the workload
child through the inherited descriptor. Workload OOM kills therefore do not
select the long-lived control transport or an otherwise healthy Service.

Every host path is canonicalized and checked against an allowlisted Box-owned
root before it enters the bundle. The checks run during planning and again
immediately before launch.

### Owner protocol

Each execution generation starts one authenticated A3S OCI owner. The owner:

- creates and starts the container through the typed SDK;
- returns structured container state and PID identity;
- performs pause, resume, complete resource update, stop, delete, and inspection;
- scopes requests to the durable generation;
- atomically updates the exact workload and derived management envelope;
- removes its session, complete cgroup topology, and runtime state during
  terminal cleanup.

Runtime and agent executables come from Box's pinned release artifacts. Startup
records their canonical paths and SHA-256 digests. Box does not discover or
download another host runtime.

### Control and logs

The Sandbox shim creates Box-visible control listeners and retains ownership of
the log lifecycle. Structured stdout and stderr entries carry timestamps,
stream identity, execution generation, and sequence information. Rotation and
removed-log retention remain Box behavior, independent of the container
runtime's internal diagnostics.

The guest-init transport exposes the same exec, PTY, files, health, and envd
contracts used by higher layers. Authentication and generation fencing prevent
a stale execution from accepting a new generation's requests.

## Network intent

The current Box Sandbox compiler always creates a fresh network namespace and
guest-init brings up loopback only. Named bridge networking and published ports
are rejected during plan resolution. Exact-host and donor-shared namespace
profiles are not public Box modes.

The pinned A3S OCI Runtime is nevertheless qualified for private, exact-host,
and donor-shared namespace operations. That gate validates namespace identity,
ownership, and cleanup in the runtime layer before Box considers exposing a
broader network contract. It is capability evidence, not an advertised Box
feature.

## Storage and initialization

The bundle compiler distinguishes:

- shared read-write bind mounts;
- read-only bind mounts whose write denial is verified;
- execution-private tmpfs mounts;
- runtime-managed named and anonymous volumes.

Mount destinations are normalized, traversal is rejected, and overlapping
reserved paths fail validation. Volume ownership and UID/GID mapping are
preserved across stop/restart and released exactly once at terminal cleanup.

Box initialization stages its protected guest-init plus a bounded executable,
argument-vector, and environment configuration. SDK script helpers deliver
script bytes through the authenticated exec channel after readiness rather
than interpolating them into a host shell.

The pinned runtime's native matrix additionally exercises inline shell,
executable-file, and direct-argument-vector OCI initialization profiles. This
locks the runtime behavior used below Box without making each profile a
separate public Box selector.

Exit status is preserved exactly. Initialization failure, timeout, or hook
failure transitions the generation through cleanup and must not leave a
container, mount, cgroup, owner session, or socket behind.

## Mandatory security controls

Production Sandbox launch requires:

- isolated mount, PID, IPC, and UTS namespaces;
- a requested and validated network namespace profile;
- non-root container identity with subordinate UID/GID mappings where needed;
- a minimal capability set, securebits, and `no_new_privs`;
- an allowlisted seccomp profile;
- cgroup v2 resource limits and freezer support;
- a masked/read-only system filesystem policy;
- an empty or explicitly allowlisted device set;
- canonical, descriptor-relative handling of host paths;
- generation-fenced control sockets and lifecycle requests.

If a required control cannot be applied or verified, launch fails. A warning is
not a substitute for an advertised security property.

## Durable state and recovery

The durable runtime record contains:

- Box ID, owner, and execution generation;
- current A3S OCI record schema;
- bundle, rootfs, runtime, socket, and log roots;
- container ID and init PID/process identity;
- runtime and agent digests;
- lifecycle state and cleanup progress.

Recovery validates record ownership, canonical paths, process identity,
runtime-reported container state, and generation before reconstructing a live
handler. Pause and resume are idempotent typed operations whose result must
match the expected runtime state.

Only the current A3S OCI runtime-record schema and fixed runtime-root layout are
accepted. Box has no compatibility decoder, old path recognition, or state
migration branch. Operators must drain non-A3S OCI Sandbox generations before
upgrading.

Cleanup is restartable. It removes only identity-validated resources owned by
the recorded generation and tolerates already-absent terminal resources.

## Packaging

Qualified Linux archives contain:

```text
a3s-box
a3s-box-shim
a3s-box-guest-init
a3s-oci
a3s-oci-agent
OCI-RUNTIME-REVISION
```

The revision marker, Cargo dependency, checked-out integration source, and
packaged artifact digests must agree. Release validation rejects an archive
containing a removed external Sandbox runtime.

Non-Linux packages keep the MicroVM backend. A shared Linux executor behind a
utility VM is qualified in the A3S OCI Runtime repository and is not advertised
as shared-host-kernel isolation on the macOS or Windows host.

## Validation gates

The exact pinned A3S OCI Runtime revision must pass native real-container tests
for:

- private, host, and donor network namespaces;
- read-write and read-only binds;
- isolated tmpfs;
- inline, file, and direct-argv initialization;
- nonzero init status, timeout, and hook failures;
- pause, resume, stop, delete, and post-stop hooks;
- PID, namespace, mount, cgroup, socket, owner, and session cleanup.

Box then certifies every advertised R17 provider profile and runs the real Rust,
Python, TypeScript, and Go SDK lifecycle against those artifacts. The resource
profile observes exact CPU, memory, and PID limits, forces throttling, PID
exhaustion, and a workload-only OOM, verifies the long-lived Service and exec
transport remain available, and requires complete cgroup cleanup. The SDK
matrix covers images, files, logs, metrics, named volumes, snapshots,
pause/resume, filesystem-only restart, and final cleanup.

Release gates also require formatting, strict linting, unit tests, supported
platform build checks, deterministic archives, and an artifact-content guard.
The complete qualification and exact revision are maintained in
[`cross-platform-oci-runtime-development-plan.md`](cross-platform-oci-runtime-development-plan.md).

## Acceptance

The replacement is complete only when:

- every new Sandbox plan resolves to A3S OCI Runtime;
- no runtime selector or executable override is public;
- no removed controller, handler, download, or invocation path exists;
- only the current A3S OCI runtime-record schema is accepted;
- Linux release artifacts contain the pinned runtime and agent only;
- native network, storage, initialization, and cleanup matrices pass;
- Box's real SDK lifecycle passes on Linux;
- supported cross-platform build gates pass.
