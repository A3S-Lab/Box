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

## Completed migration

Box now:

- resolves every new `sandbox` request to `ExecutionBackend::A3sOci`;
- exposes no public Sandbox runtime selector;
- starts one authenticated A3S OCI native Linux owner per Box generation;
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
`9b5141b647a04cadbdbbc32310bde33c54937ac3`, which retains the qualified
control/workload cgroup and read-only bind behavior and adds deterministic
multi-driver registration, isolation selection, and durable recorded-driver
routing required by the unified execution migration. Runtime service startup
also fails closed when historical state references a missing driver or a
driver whose advertised isolation has drifted. Windows additionally has a
protected, local-only named-pipe SDK host listener ready for the WHPX driver,
and utility-VM ownership can explicitly close every guest-agent client clone
before reaping the guest and hypervisor shim.

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

The `SDK Local Sandbox (A3S OCI Runtime)` job runs on Ubuntu without KVM and
must:

1. check out the exact pinned OCI Runtime revision;
2. run its native Linux qualification script;
3. build Box, the shim, guest init, the runtime, and the agent;
4. execute the unchanged Rust, Python, TypeScript, and Go local SDK lifecycles;
5. cover image management, named volumes, files, logs, metrics, pause/resume,
   exact CPU/memory/PID enforcement, stop/restart, filesystem snapshots, and
   complete cleanup;
6. prove no Box shim, OCI owner, agent, runtime root, socket, or Box directory
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
repository. Box additionally runs it for the exact dependency revision on its
Linux x86_64 integration lane.

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
- [ ] The change's pull-request and main-branch CI runs are green.

The final unchecked item is evidence, not an implementation fallback. It must
be updated only from completed GitHub Actions runs for the exact commit.
