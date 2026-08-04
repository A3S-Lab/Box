# A3S OCI Runtime Integration and Qualification

Status: **Sandbox migration implemented; unified execution migration active**

This document records the qualified shared-host-kernel integration with
[A3S OCI Runtime](https://github.com/A3S-Lab/OCI-Runtime). The migration has
already delivered one product outcome:

> Explicit `sandbox` isolation is executed only by A3S OCI Runtime.

The current default Box isolation remains a directly managed libkrun MicroVM.
A caller must explicitly request shared-host-kernel isolation. The next
migration routes both isolation choices through the public OCI Runtime SDK as
defined in the repository [roadmap](../ROADMAP.md). Box never silently changes
the requested isolation class during either stage.

## Current backend baseline

| Request | Backend | Isolation boundary | Supported hosts |
| --- | --- | --- | --- |
| default or `microvm` | libkrun | dedicated guest kernel | Linux/KVM, macOS/HVF, Windows/WHPX |
| `sandbox` | A3S OCI Runtime | shared Linux host kernel | qualified Linux x86_64 and aarch64 hosts |

Linux containers on macOS and Windows require a utility VM. They cannot provide
shared-host-kernel isolation. Utility-VM qualification belongs to A3S OCI
Runtime and does not cause Box to reinterpret a `sandbox` request as a
MicroVM request.

The target model keeps the same isolation contract but moves MicroVM execution
behind A3S OCI Runtime: `microvm` requests `DedicatedVm`, while `sandbox`
requests `SharedHostKernel`. The current direct libkrun path is removed only
after the replacement passes the cross-platform parity and recovery gates.

The runtime crate contains the phased-cutover router used to reach that target.
An explicit policy selects only new records, stamps `box_vm` or `oci_sdk`
before capability preflight, persists it with the successful reservation before
launch side effects, and never retries a selected failure on the other backend.
Recovery and cleanup use the persisted route; pre-routing records are inferred
from their exact OCI binding or retained Box endpoint evidence. The shipped CLI
and SDK still construct the current backend while production bundle preparation
is connected to OCI Runtime's multi-container Native Linux owner.

## Completed migration

Box now:

- resolves every new `sandbox` request to `ExecutionBackend::A3sOci`;
- exposes no public Sandbox runtime selector;
- reuses one authenticated, identity-fenced A3S OCI multi-container Native
  Linux owner across Box processes while binding each Sandbox to an exact OCI
  generation;
- packages the pinned `a3s-oci` and `a3s-oci-agent` artifacts in Linux
  releases;
- records exact runtime and agent digests with the durable generation;
- recovers, pauses, resumes, updates resources, stops, and deletes through the
  typed A3S OCI SDK;
- keeps exact workload limits in `linux.resources` while the runtime alone
  derives, updates, and cleans the control-plane cgroup envelope;
- has no external-runtime discovery environment variable, controller, handler,
  package, download, or differential CI lane;
- rejects any reintroduction of the removed integration symbols in CI.

The exact integration revision is
`08c145d8ce5d06d5f28587226be822a2ab43b299`, which retains the qualified
control/workload cgroup and read-only bind behavior and adds deterministic
multi-driver registration, isolation selection, and durable recorded-driver
routing required by the unified execution migration. Runtime service startup
also fails closed when historical state references a missing driver or a
driver whose advertised isolation has drifted. Startup then calls only the
exact recorded driver's idempotent recovery hook and commits any legal state
observation before serving. Windows additionally has a protected, local-only
named-pipe SDK host listener ready for the WHPX driver, and utility-VM
ownership can explicitly close every guest-agent client clone before reaping
the guest and hypervisor shim. Durable Native Linux owner recovery remains
host-only; the utility-VM guest executor uses transient recovery state and does
not attempt host journal writes before protocol negotiation. The shared
session keeps one VM owner, lends
clone-safe clients to concurrent operations, and returns one cached cleanup
report to every shutdown caller. Native Linux and the new WHPX driver candidate
share one eighteen-operation adapter. The candidate binds one VM to each exact
dedicated-VM generation, permits parallel launches for different IDs, requires
a protected per-generation virtio-fs share disjoint from the guest system root,
and reconciles owner-death cleanup as a stopped, generation-fenced tombstone.
Bundles and one-time token/recovery handoff stay inside that exact share;
authenticated guest shutdown evidence is normalized into protected host
storage, matched to the exact target and durable configuration during startup,
and committed to the durable wait cache before service. Missing or invalid
evidence retains the explicit stopped-only fallback instead of inventing an
exit status. Upstream clean commit `2d91cd04f6ec1ecd9ea3fce4673be6fdc2b6f631`
now passes the versioned real-WHPX owner-death gate across both Recover fault
boundaries, host-service reopen, exact signal-9 wait replay, stopped-only
delete, and complete protected-share cleanup. The candidate remains
non-registerable at `probe-only` readiness until immutable-system-root and
in-process native-handle reclamation qualification pass.

## Upgrade behavior

This is a hard state-format cutover. Box accepts only execution plans and
runtime records written for the current A3S OCI implementation. It does not
decode removed selectors, recognize old runtime roots, or migrate an existing
shared-kernel generation.

Operators must drain or stop every pre-A3S OCI Sandbox generation before
installing the OCI-only release. An unsupported record is rejected as invalid
state before recovery or cleanup touches its recorded resources. MicroVM
generations are unaffected.

## Qualification gates

### Box integration gate

The `SDK Local Sandbox (A3S OCI Runtime)` and
`SDK Local Sandbox (A3S OCI Runtime, aarch64)` checks run on native Ubuntu
x86_64 and aarch64 hosts without KVM. Each must:

1. check out the exact pinned OCI Runtime revision;
2. run its native Linux qualification script;
3. build Box, the shim, guest init, the runtime, and the agent;
4. execute the unchanged Rust, Python, TypeScript, and Go local SDK lifecycles;
5. cover image management, named volumes, files, logs, metrics, pause/resume,
   exact CPU/memory/PID enforcement, stop/restart, filesystem snapshots, and
   complete cleanup;
6. kill the exact OCI owner under a running Sandbox, prove the launcher and init
   terminate, then use distinct Box processes to rebind the endpoint, reconcile
   stopped-only state without a synthetic exit status, delete the old
   generation, and restart exactly the next Box and OCI generations;
7. prove no Box shim, OCI owner, agent, runtime root, socket, or Box directory
   remains.

### Native configuration matrix

The pinned OCI Runtime qualification must exercise real containers for:

- private network namespaces;
- exact host-network inheritance;
- donor-shared network namespaces;
- shared read-write bind volumes;
- read-only bind enforcement;
- isolated tmpfs instances;
- inline initialization scripts;
- executable initialization files;
- direct argument-vector initialization;
- exact nonzero init exits;
- create/start timeout and hook failures;
- post-stop hook behavior;
- PID, mount, cgroup, namespace, owner, and session cleanup.

The same script runs on Linux x86_64 and Linux aarch64 in the OCI Runtime
repository. Box additionally runs it for the exact dependency revision on both
native architecture integration lanes before exercising the production owner
composition.

### Cross-platform gates

Box keeps build and unit coverage for:

- Linux x86_64;
- Linux aarch64;
- macOS arm64;
- Windows x86_64/WHPX.

A3S OCI Runtime separately qualifies its utility-VM path on macOS/HVF and
Windows/WHPX. Those checks prove the shared Linux executor can run behind the
authenticated guest protocol; they do not advertise shared-host-kernel
Sandbox isolation on non-Linux hosts.

## Release invariants

A release is blocked unless:

- formatting, strict Clippy, and workspace unit tests pass;
- the local SDK real-runtime gate passes against the pinned revision;
- network, storage, and initialization profiles pass;
- Linux archives contain executable `a3s-oci` and `a3s-oci-agent`;
- `OCI-RUNTIME-REVISION` equals the dependency revision;
- no removed external Sandbox runtime is present in a release archive;
- supported platform build checks pass;
- failure cleanup returns host state to the recorded baseline.

## Completed Sandbox acceptance criteria

The replacement is complete when all of the following are true:

- [x] New Sandbox execution has one backend: A3S OCI Runtime.
- [x] Public configuration and SDKs have no rollback selector.
- [x] Box source contains no external runtime controller or handler.
- [x] CI does not download or execute the removed runtime.
- [x] Linux release packaging contains only the pinned A3S OCI artifacts.
- [x] Only the current A3S OCI runtime-record schema is accepted.
- [x] The pinned native matrix covers network, volume, tmpfs, and init profiles.
- [x] Rust, Python, TypeScript, and Go exercise the real Box lifecycle.
- [x] Sandbox resource creation, live update, OOM isolation, and cleanup have
  one A3S OCI owner and no guest cgroupfs write path.
- [x] Linux x86_64/aarch64, macOS arm64, and Windows x86_64 builds remain gated.
- [x] The change's pull-request and main-branch CI runs are green.

The accepted production-adapter evidence remains Box
`2cbe588b2bc6255ffa700bd0f9dbce451dafe02e`: its
[pull-request CI run](https://github.com/A3S-Lab/Box/actions/runs/30747729543)
and its
[main-branch CI run](https://github.com/A3S-Lab/Box/actions/runs/30748314872)
both completed successfully. The dual-architecture qualification evidence is
Box `a16772c399528a6c4eaf584767f6000ca4e53f16`: its
[pull-request CI run](https://github.com/A3S-Lab/Box/actions/runs/30754808084)
and its
[main-branch CI run](https://github.com/A3S-Lab/Box/actions/runs/30755797650)
both completed successfully with the x86_64 and aarch64 real-host lanes. That
later revision changes only the CI matrix, deterministic recovery conformance
fixture, and plan documentation relative to the qualified production adapter.
The Windows product-gate evidence is Box
`52a2cfe4ee6693c9cc3a88df1b922bc1825b2deb`: its
[pull-request CI run](https://github.com/A3S-Lab/Box/actions/runs/30889251291)
produced the exact Windows binaries that passed the real x86_64 WHPX lifecycle
against OCI Runtime `08c145d8ce5d06d5f28587226be822a2ab43b299` artifacts from
[main run](https://github.com/A3S-Lab/OCI-Runtime/actions/runs/30881404238).
The machine-readable reports recorded exact replay, manager restart, running
state, exit code 23, `libkrun-whpx`/`dedicated-vm`, complete path cleanup, and
zero residual processes.
Later documentation-only commits do not replace these evidence revisions.
