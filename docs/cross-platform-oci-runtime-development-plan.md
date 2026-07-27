# A3S OCI Runtime Integration and Qualification

Status: **M7 implemented; release qualification is enforced by CI**

This document is the source of truth for the A3S Box integration with
[A3S OCI Runtime](https://github.com/A3S-Lab/OCI-Runtime). The migration has
one product outcome:

> Explicit `sandbox` isolation is executed only by A3S OCI Runtime.

The default Box isolation remains a libkrun MicroVM. A caller must explicitly
request shared-host-kernel isolation. Box never silently changes the requested
isolation class.

## Final backend model

| Request | Backend | Isolation boundary | Supported hosts |
| --- | --- | --- | --- |
| default or `microvm` | libkrun | dedicated guest kernel | Linux/KVM, macOS/HVF, Windows/WHPX |
| `sandbox` | A3S OCI Runtime | shared Linux host kernel | qualified Linux x86_64 and aarch64 hosts |

Linux containers on macOS and Windows require a utility VM. They cannot provide
shared-host-kernel isolation. Utility-VM qualification belongs to A3S OCI
Runtime and does not cause Box to reinterpret a `sandbox` request as a
MicroVM request.

## Completed migration

Box now:

- resolves every new `sandbox` request to `ExecutionBackend::A3sOci`;
- exposes no public Sandbox runtime selector;
- starts one authenticated A3S OCI native Linux owner per Box generation;
- packages the pinned `a3s-oci` and `a3s-oci-agent` artifacts in Linux
  releases;
- records exact runtime and agent digests with the durable generation;
- recovers, pauses, resumes, stops, and deletes through the typed A3S OCI SDK;
- has no external-runtime discovery environment variable, controller, handler,
  package, download, or differential CI lane;
- rejects any reintroduction of the removed integration symbols in CI.

The exact integration revision is
`503625b176de7f22b2e31c782b82e97897e8c368`, released as A3S OCI Runtime
`v0.2.0`.

## Upgrade behavior

New executions migrate automatically to A3S OCI Runtime even when a serialized
pre-migration Box configuration still contains the obsolete runtime selector.

An already-running legacy Sandbox is different: its persisted plan and runtime
record remain recognizable solely for safe diagnostics. The OCI-only release
does not execute the recorded external binary, signal an unverified PID, or
delete a potentially live rootfs. Recovery fails closed with an instruction to
stop that Sandbox with the previous Box release before upgrading.

Operators must therefore drain or stop all legacy Sandbox generations before
installing the OCI-only release. MicroVM generations are unaffected.

## Qualification gates

### Box integration gate

The `SDK Local Sandbox (A3S OCI Runtime)` job runs on Ubuntu without KVM and
must:

1. check out the exact pinned OCI Runtime revision;
2. run its native Linux qualification script;
3. build Box, the shim, guest init, the runtime, and the agent;
4. execute the unchanged Rust, Python, and TypeScript local SDK lifecycles;
5. cover image management, named volumes, files, logs, metrics, pause/resume,
   stop/restart, filesystem snapshots, and complete cleanup;
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

## Acceptance criteria

The replacement is complete when all of the following are true:

- [x] New Sandbox execution has one backend: A3S OCI Runtime.
- [x] Public configuration and SDKs have no rollback selector.
- [x] Box source contains no external runtime controller or handler.
- [x] CI does not download or execute the removed runtime.
- [x] Linux release packaging contains only the pinned A3S OCI artifacts.
- [x] Legacy live records fail closed without executing their recorded binary.
- [x] The pinned native matrix covers network, volume, tmpfs, and init profiles.
- [x] Rust, Python, and TypeScript exercise the real Box lifecycle.
- [x] Linux x86_64/aarch64, macOS arm64, and Windows x86_64 builds remain gated.
- [ ] The change's pull-request and main-branch CI runs are green.

The final unchecked item is evidence, not an implementation fallback. It must
be updated only from completed GitHub Actions runs for the exact commit.
