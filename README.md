# A3S Box

<p align="center">
  <strong>OCI Workload Runtime for MicroVMs and Sandboxes</strong>
</p>

<p align="center">
  <em>Run Linux OCI workloads in a hardware-backed MicroVM by default, or explicitly choose a low-overhead shared-kernel Sandbox on certified Linux hosts</em>
</p>

<p align="center">
  <a href="#overview">Overview</a> •
  <a href="#features">Features</a> •
  <a href="#quick-start">Quick Start</a> •
  <a href="#isolation-model">Isolation Model</a> •
  <a href="#runtime-model">Runtime Model</a> •
  <a href="#sdks-and-compatibility">SDKs</a> •
  <a href="#integrations">Integrations</a> •
  <a href="#architecture">Architecture</a> •
  <a href="#development">Development</a>
</p>

---

## Overview

**A3S Box** is an OCI workload runtime with a Docker-like CLI and two explicit
execution backends. The default path boots each workload in its own
[libkrun](https://github.com/containers/libkrun) MicroVM. Linux operators can
instead request `--isolation sandbox` to run through a certified
[crun](https://github.com/containers/crun) OCI runtime with namespaces,
seccomp, capabilities, `no_new_privs`, and cgroup v2.

The two modes are deliberately not presented as equivalent. A MicroVM has a
separate guest kernel and a hardware virtualization boundary. A Sandbox shares
the host Linux kernel and is intended for agent tools, benchmarks, and
development automation whose threat model does not include a working kernel
exploit. Box never falls back from MicroVM to Sandbox when virtualization is
unavailable.

The local CLI and Rust SDK are the primary product surfaces. OCI Sandbox
execution, the E2B protocol service, Kubernetes CRI/RuntimeClass integration,
TEE workflows, and Windows support have different maturity and host
requirements; the capability matrix below states those boundaries explicitly.

### Basic usage

```bash
# Default: one libkrun MicroVM with its own Linux kernel
a3s-box run --rm alpine:latest -- uname -a

# Explicit opt-in: shared-kernel OCI Sandbox on a certified Linux host
a3s-box run --rm --isolation sandbox alpine:latest -- id
```

### E2B-compatible Python and TypeScript SDKs

The A3S Python and TypeScript packages re-export the pinned official E2B
objects and add typed A3S endpoint configuration. Applications keep the
familiar `Sandbox`, `AsyncSandbox`, Commands, Filesystem, PTY, and Code
Interpreter surfaces while `a3s-box-e2b` owns the remote runtime and isolation
policy. No E2B-hosted service is involved.

Point the native SDK at the deployed A3S Box service. A conventional
`https://api.<domain>` endpoint automatically derives `<domain>` for Sandbox
routing:

```bash
export A3S_BOX_ENDPOINT=https://api.box.example.com
export A3S_BOX_API_KEY=a3s_your_key
```

Non-standard self-hosted routing can additionally set `A3S_BOX_DOMAIN` and
`A3S_BOX_SANDBOX_URL`. The `E2B_*` variables are needed only when an unchanged
official E2B SDK is used directly for protocol compatibility.

Python uses async lifecycle management so the remote Sandbox is always cleaned
up:

```python
import asyncio

from a3s_box import A3SConnectionConfig, AsyncSandbox


async def main() -> None:
    connection = A3SConnectionConfig.from_environment()
    sandbox = await AsyncSandbox.create(
        "code-interpreter-v1",
        timeout=60,
        **connection.python_options(),
    )
    async with sandbox:
        result = await sandbox.commands.run("python -c 'print(6 * 7)'")
        print(result.stdout)


asyncio.run(main())
```

TypeScript uses the same endpoint and E2B object model:

```typescript
import { A3SConnectionConfig, Sandbox } from '@a3s-lab/box'

const connection = A3SConnectionConfig.fromEnvironment(process.env)
const sandbox = await Sandbox.create('code-interpreter-v1', {
  ...connection.typescriptOptions(),
  timeoutMs: 60_000,
})

try {
  const result = await sandbox.commands.run('node -e "console.log(6 * 7)"')
  console.log(result.stdout)
} finally {
  await sandbox.kill()
}
```

Both packages are source-tree previews until the complete official and native
client matrix passes and the packages are published. The current verified
surface and remaining compatibility gates are stated under
[SDKs and Compatibility](#sdks-and-compatibility).

## Features

- **Two explicit isolation classes**: Hardware-backed MicroVM execution by
  default and opt-in shared-kernel Sandbox execution through certified `crun`
- **Docker-like lifecycle**: Create, start, stop, restart, kill, pause, wait,
  remove, inspect, exec, PTY, attach, logs, health checks, and restart policies
- **OCI image workflow**: Pull, push, authenticate, verify digests and optional
  cosign signatures, tag, inspect, save, load, import, remove, and cache images
- **Image builds**: Build a documented Dockerfile/Containerfile subset with
  multi-stage builds, layer caching, selected `RUN --mount` forms, BuildKit VM
  execution on macOS, and warm-pool execution
- **Storage primitives**: Bind mounts, named volumes, tmpfs, file copy, diff,
  export, commit, filesystem snapshots, and copy-on-write snapshot restore
- **Networking and Compose**: TSI, user-defined bridge networks, TCP port
  publishing, peer discovery, and a useful Compose subset
- **Startup acceleration**: Rootfs/layer caching, pre-booted warm pools,
  one-shot pool routing, build leases, and Linux/KVM snapshot-fork
- **Security and confidential computing**: Fail-closed option validation,
  resource and syscall controls, audit records, AMD SEV-SNP-oriented
  attestation, RA-TLS, sealing, and secret injection
- **Typed SDKs and protocols**: Direct runtime-backed Rust management APIs,
  an optional programmable pipeline runner, and an E2B protocol compatibility
  preview with official-client fixtures plus Python and TypeScript source packages
- **Operations and cluster integration**: Structured logs, stats, events,
  Prometheus endpoints, health monitoring, CRI, and containerd RuntimeClass

### Capability matrix

| Area | Current capability | Status and boundary |
| --- | --- | --- |
| MicroVM runtime | libkrun-backed OCI execution on Linux/KVM and Apple Silicon/HVF | Primary local runtime. Each box has its own guest kernel. Host-backed validation is required for releases. |
| OCI Sandbox | Explicit `--isolation sandbox` execution through certified `crun 1.28` on Linux | Preview. Shares the host kernel, never replaces or emulates MicroVM isolation, and still has open security-negative and performance release gates. |
| Lifecycle and exec | Foreground/detached runs, managed create/start/restart/kill, exec, PTY, logs, health, wait, and cleanup | Implemented for MicroVM. The managed Sandbox path and its structured logs are implemented, while complete parity and adversarial validation remain in progress. |
| OCI images | Registry pull/push, credentials, digest verification, optional cosign verification/signing, local cache, archive and tag operations | Implemented. Registry-dependent paths still require end-to-end validation against the target registry. |
| Dockerfile builds | Built-in Dockerfile subset, layer cache, BuildKit-in-MicroVM, and warm-pool `RUN` execution | Implemented subset, not a full Buildx replacement. One target platform is recorded per build. |
| Storage | Bind mounts, named volumes, tmpfs, `cp`, `diff`, `export`, `commit`, filesystem snapshots, and CoW restore | Implemented. Filesystem snapshots do not contain live VM RAM or device state. |
| Networking and Compose | TSI, bridge networks, TCP publishing, peer discovery, and Compose lifecycle/config/logs | Implemented subset for MicroVM workloads. UDP publishing, host-IP binds, ranges, and live network hot-plug are not implemented. |
| Warm pool and snapshot-fork | Pre-booted MicroVMs, one-shot runs, build leases, metrics, and CoW memory restore | Implemented. Native snapshot-fork is Linux/KVM-only and disabled by default. |
| Rust SDK | Typed, direct runtime-backed management and guest-control APIs | Implemented in `a3s-box-sdk`. The optional `pipeline-cli` feature retains the CLI-driven programmable pipeline. |
| E2B protocol and language SDKs | Pinned contracts, durable lifecycle, TLS routing, runtime envd initialization, foreground Process commands, and core Filesystem operations | Preview only. Production evidence covers lifecycle and foreground commands across the pinned base clients, plus environment propagation and core Filesystem operations through the official Python sync client. Concurrent Process/PTY, the extended async Python and TypeScript matrix, Code Interpreter execution, MCP, signed files, and full unchanged-client conformance remain release gates. Python/npm packages are not published. |
| TEE | SEV-SNP-oriented attestation, RA-TLS, sealing, secret injection, and simulation | Host-specific. Hardware claims require a supported SEV-SNP host and real attestation evidence. Simulation is development-only; TDX is not productized. |
| Kubernetes | CRI server plus a containerd runtime-v2 shim and `runtimeClassName: a3s-box` | Preview. Core lifecycle, streaming, logs, resources, and RuntimeClass paths exist; complete CRI conformance is not claimed. |
| Windows | Native x86_64 WHPX/libkrun code paths | Integration surface requiring host-specific validation. Current standard release automation focuses on Linux and macOS; Windows CRI is out of scope. |

An implemented API is not automatically a production guarantee for every host
or threat model. Real-runtime validation evidence and remaining platform gaps
are maintained in [Host Integration](docs/host-integration.md),
[Production Cluster Tests](docs/production-cluster-tests.md), and
[CRI Conformance](docs/cri-conformance.md).

## Quick Start

### Installation

Install the current macOS or Linux release from the Homebrew tap:

```bash
brew install a3s-lab/tap/a3s-box
a3s-box info
```

Or build from source:

```bash
git clone https://github.com/A3S-Lab/Box.git
cd Box
just release
```

Development builds also need a static Linux `a3s-box-guest-init` matching the
guest architecture. The repository `just build-guest` recipes build and stage
that PID 1 binary. Do not use a host macOS binary as a guest artifact.

Host requirements:

| Host | MicroVM requirement | Notes |
| --- | --- | --- |
| Linux | KVM and libkrun | The current primary production-host path |
| macOS | Apple Silicon and Hypervisor.framework | Intel macOS is unsupported |
| Linux Sandbox | Certified `crun 1.28`, user namespaces, subordinate IDs, seccomp, and delegated cgroup v2 | Does not require KVM; explicitly select `--isolation sandbox` |
| Windows | x86_64, WHPX, and matching libkrun assets | Host-specific integration path; not part of the current standard release matrix |

Always run `a3s-box info` before host-backed tests. It reports virtualization,
networking, package cache, TEE, virtio-fs, and warm-pool availability without
starting a workload.

### Run a MicroVM

```bash
# One-shot command; stdin is closed unless -i is requested
a3s-box run --rm --timeout 60 alpine:latest -- sh -lc 'echo hello; uname -r'

# Interactive shell
a3s-box run -it --name dev alpine:latest -- /bin/sh

# Detached service with resources and a TCP port
a3s-box run -d --name web --cpus 2 --memory 1g -p 8080:80 nginx:alpine

a3s-box ps
a3s-box exec web -- nginx -v
a3s-box logs -f web
a3s-box stop web
a3s-box rm web
```

Omitting `--isolation` is the only public way to select the default MicroVM
backend. An explicit `--isolation microvm` value is rejected so scripts cannot
confuse a backend name with a user-selectable compatibility mode.

### Run a shared-kernel Sandbox

```bash
a3s-box run --rm \
  --isolation sandbox \
  --cpus 2 \
  --memory 512m \
  alpine:latest -- sh -lc 'id; cat /proc/self/status'
```

This command is Linux-only and fails closed unless the complete certified host
capability probe succeeds. The current Sandbox surface rejects VM-only or
unsafe combinations, including TEE, warm pools, snapshot-fork, privileged
mode, pause/unpause, published ports, named bridge networking, custom sysctls,
vsock sidecars, and unconfined seccomp.

### Compose a local workload

```bash
a3s-box compose -f compose.yaml config
a3s-box compose -f compose.yaml up -d
a3s-box compose -f compose.yaml ps
a3s-box compose -f compose.yaml logs -f
a3s-box compose -f compose.yaml down
```

Compose is a MicroVM-oriented subset today. Although the parser accepts
`--isolation sandbox`, the current Compose plan creates a default named bridge,
so the fail-closed Sandbox resolver rejects it before launch.

## Isolation Model

Backend selection is deterministic and persisted with every managed execution:

| Property | Default MicroVM | `--isolation sandbox` |
| --- | --- | --- |
| Runtime backend | libkrun | Certified crun |
| Isolation class | `hardware-vm` | `shared-kernel` |
| Kernel | Dedicated guest Linux kernel | Host Linux kernel |
| Intended workload | Untrusted workloads and stronger tenant boundaries | Trusted or semi-trusted tools, benchmarks, and automation |
| Required host | KVM, HVF, or WHPX path | Certified Linux Sandbox host |
| TEE and attestation | Supported on qualifying hardware | Rejected |
| Warm pool and snapshot-fork | Supported | Rejected |
| Published ports and named bridge | Supported within documented platform limits | Rejected in the current Sandbox release |
| Privileged mode | Explicitly modeled for the MicroVM path | Rejected |
| Automatic fallback | Never | Never |

Before a Sandbox starts, Box requires evidence for:

- user, mount, PID, IPC, UTS, and network namespaces;
- seccomp, a bounded capability set, and `no_new_privs`;
- cgroup v2 delegation for resource enforcement;
- non-root subordinate UID/GID mappings, including image metadata coverage;
- the pinned runtime artifact and protected OCI state directories.

The Sandbox threat model excludes a working Linux kernel exploit, hardware
side channels, a hostile host administrator, and data deliberately exposed
through a bind mount. Use the default MicroVM backend when those boundaries
matter. The detailed contract and remaining release gates live in
[Host Sandbox Backend Design](docs/host-sandbox-backend-design.md).

## Runtime Model

### Command surface

Box commands are Docker-like, not Docker-identical:

| Category | Commands |
| --- | --- |
| Lifecycle | `run`, `create`, `start`, `stop`, `restart`, `rm`, `kill`, `pause`, `unpause`, `wait`, `rename`, `prune` |
| Execution | `exec`, `shell`, `attach`, `top` |
| Images and builds | `pull`, `push`, `build`, `images`, `rmi`, `tag`, `image-inspect`, `history`, `image-prune`, `save`, `load`, `import` |
| Filesystems | `cp`, `diff`, `export`, `commit`, `volume`, `snapshot` |
| Networking and orchestration | `network`, `port`, `compose` |
| Security and TEE | `attest`, `seal`, `unseal`, `inject-secret` |
| Observability | `ps`, `logs`, `inspect`, `stats`, `events`, `df`, `audit`, `monitor` |
| System | `container-update`, `system-prune`, `pool`, `login`, `logout`, `version`, `info` |

Box references accept a name, full ID, or unique short-ID prefix. Unsupported
options fail early instead of being silently persisted.

### Lifecycle and execution

```bash
a3s-box create --name job alpine:latest -- sleep 300
a3s-box start job
a3s-box exec job -- sh -lc 'echo running'
a3s-box restart job
a3s-box wait job
a3s-box rm job
```

Managed executions use durable reservations, generation-fenced state
transitions, idempotent operation IDs, and restart reconciliation. The runtime
persists caller policy instead of rebuilding it from defaults during a retry.
Health checks and restart policies are owned by generation-fenced background
workers, and structured `json-file` logging keeps stdout and stderr distinct.

Common runtime controls include:

- CPU, memory, PID, cpuset, quota/share, swap, and ulimit settings;
- environment files and values, entrypoint, user, workdir, hostname, and labels;
- named or host volumes, tmpfs, read-only rootfs, and shared-memory sizing;
- health command/timing, stop signal/timeout, persistence, and restart policy;
- capability add/drop, default seccomp, and `no-new-privileges`.

Some controls are platform- or backend-specific. `--device` and GPU
passthrough are not implemented, custom seccomp profiles are not accepted by
the local CLI, and the Sandbox resolver rejects every VM-only feature before
pulling an image or allocating runtime state.

### OCI images and builds

```bash
a3s-box pull alpine:latest
a3s-box pull --verify-key cosign.pub ghcr.io/example/app:v1
a3s-box image-inspect alpine:latest
a3s-box tag alpine:latest local/alpine:dev
a3s-box save -o alpine.tar alpine:latest
a3s-box load -i alpine.tar --tag local/alpine:dev
a3s-box push registry.example/app:v1
```

Registry authentication comes from `a3s-box login`, Docker-compatible
configuration, or explicit registry environment credentials. Manifest,
configuration, and layer digests are checked during pull. Authentication is
retained only across same-origin redirects, and decompression limits protect
image and build extraction.

The built-in builder supports a documented subset of Dockerfile and
Containerfile behavior:

- `FROM`, multi-stage targets, `COPY`/`ADD`, shell/exec `RUN`, and
  `.dockerignore`;
- `WORKDIR`, `ENV`, `ENTRYPOINT`, `CMD`, `EXPOSE`, `LABEL`, `USER`,
  `ARG`, `SHELL`, `STOPSIGNAL`, `HEALTHCHECK`, `ONBUILD`, and `VOLUME`;
- content-addressed layer caching and selected cache, bind, and tmpfs
  `RUN --mount` forms;
- one target platform per build, optional registry push, and explicit
  plain-HTTP support for trusted private registries.

```bash
a3s-box build -t app:dev .
a3s-box build --target builder --no-cache -t app:builder .

# macOS: run BuildKit inside an A3S Linux MicroVM
a3s-box build --builder=buildkit-vm --platform linux/arm64 -t app:dev .

# Built-in engine: execute RUN through a leased warm MicroVM
a3s-box pool start --image alpine:latest --size 1 --socket /tmp/a3s-build.sock
a3s-box build --run-pool --run-pool-socket /tmp/a3s-build.sock -t app:dev .
```

Linux host `RUN` uses the isolated root-capable build path. macOS automatically
uses the BuildKit VM path for Dockerfiles containing `RUN` unless a warm-pool
path is selected. A3S Box does not claim complete Dockerfile, Buildx, or
multi-platform-index compatibility.

### Filesystems, volumes, and snapshots

```bash
a3s-box volume create data
a3s-box run -d --name app -v data:/data alpine:latest -- sleep 3600
a3s-box cp ./input.txt app:/data/input.txt
a3s-box diff app
a3s-box export app -o rootfs.tar
a3s-box commit app app:checkpoint
a3s-box snapshot create app --name checkpoint-1
a3s-box snapshot restore checkpoint-1 --name restored-app
```

Named volumes persist independently of a box. Host bind mounts use virtio-fs
for MicroVMs, while Sandbox mounts are validated against the selected UID/GID
mapping. `--package-cache pnpm|npm` creates reusable named caches for
short-lived Node.js workloads, and tmpfs is useful for high-churn dependency
trees.

Filesystem snapshots capture configuration and rootfs state, not live RAM or
device state. On overlay-capable hosts, restore uses a read-only snapshot lower
plus a private writable upper; in-use snapshots are protected from pruning.
Live MicroVM memory cloning is the separate snapshot-fork mechanism below.

### Networking and Compose

| Mode | Behavior | Boundary |
| --- | --- | --- |
| TSI | Proxies guest socket operations through the host | Simple outbound networking; plain TSI has no user-defined peer network |
| Bridge | Gives a MicroVM a user-defined network interface and peer discovery | Linux uses `passt`; macOS uses the built-in `netproxy` |
| None | Disables workload networking | Useful for deliberately offline execution |

```bash
a3s-box network create backend --subnet 10.89.0.0/24
a3s-box run -d --name api --network backend -p 8080:80 myapi:latest
a3s-box network inspect backend
a3s-box port api
```

Published ports support TCP `host_port:guest_port[/tcp]` mappings. UDP,
host-IP binds, single-port shorthand, ranges, live connect/disconnect, and
strict packet-filter policy are not implemented. macOS bridge networking
supports peer traffic, DNS, published TCP, and outbound TCP; non-DNS outbound
UDP and ICMP are not proxied.

The Compose subset includes image, command, entrypoint, environment,
`env_file`, ports, volumes, dependency ordering with started/healthy
conditions, networks, DNS, tmpfs, workdir, hostname, extra hosts, labels,
health checks, restart policies, CPU/memory, capabilities, and privileged mode.
Shell environment values override the project `.env` file during
interpolation.

### Warm pool and snapshot-fork

```bash
a3s-box pool start --image alpine:latest --size 8
a3s-box pool run --image alpine:latest -- echo warm
a3s-box run --pool --rm alpine:latest -- echo one-shot
a3s-box pool status
a3s-box pool stop
```

A warm pool keeps MicroVMs booted behind a Unix socket. It supports bounded
capacity, backpressure, multiple images, abandoned-lease recovery, one-shot
`run --pool` routing, build-stage leases, deferred-main execution, optional
Linux KSM page merging, and Prometheus metrics.

On Linux/KVM, `pool start --snapshot-fork` boots one template, captures its
file-backed guest RAM and KVM device/vCPU state, then restores additional slots
with private copy-on-write mappings. It is opt-in, hardware-dependent, and not
available to the shared-kernel Sandbox backend. Published benchmark numbers
must identify the host, image, backend, and pool state; Box does not treat one
machine's latency as a universal guarantee.

### Observability and safety

- `logs` and `attach` preserve stdout/stderr identity; removed boxes can retain
  an archived final log according to their lifecycle policy.
- `stats`, `events`, `inspect`, `df`, and `audit` expose runtime state and
  enforcement choices.
- `monitor --metrics-addr` serves Prometheus metrics and `/healthz`; warm pools
  expose their own optional metrics endpoint.
- State updates, image indexes, snapshots, rootfs caches, and lifecycle
  transitions use locking or generation fencing to reduce cross-process races.
- Registry digests, path traversal, archive extraction limits, runtime process
  identity, and cleanup ownership are validated rather than inferred.

See [Monitor Service](docs/monitor-service.md) for systemd/launchd operation and
[Host Integration](docs/host-integration.md) for real-runtime smoke and soak
procedures.

## SDKs and Compatibility

### Rust SDK

`a3s-box-sdk` provides typed, direct runtime-backed APIs. The default client
does not spawn the `a3s-box` CLI and uses the same stores, execution manager,
registry, build, volume, network, snapshot, exec, PTY, and attestation
implementations as the runtime.

```toml
[dependencies]
a3s-box-sdk = "3.0"
```

```rust,no_run
use a3s_box_sdk::{A3sBoxClient, ListBoxesOptions};

fn main() -> Result<(), a3s_box_sdk::ClientError> {
    let client = A3sBoxClient::new();
    for item in client.list_boxes(ListBoxesOptions::all())? {
        println!("{} {}", item.name, item.status);
    }
    Ok(())
}
```

Managed create, start, run, inspect, pause, resume, restart, kill, and
reconciliation all use the canonical generation-fenced execution manager.
Additional APIs cover local state, images, builds, registries, volumes,
networks, snapshots, logs, stats, file transfer, exec, PTY, and attestation.

The historical programmable CI pipeline remains behind the optional
`pipeline-cli` feature. It shells out only for lifecycle-heavy operations not
yet exposed by its pipeline abstraction. See
[the SDK README](src/sdk/README.md) for the current API coverage.

### E2B protocol preview

A3S Box pins the public control, envd, volume-content, Process, Filesystem, MCP,
Python, TypeScript, and Code Interpreter contracts under
[`compat/e2b`](compat/e2b/README.md). Generated inventories and digests prevent
silent upstream protocol drift.

The current pin targets:

| Client | Version |
| --- | ---: |
| Python `e2b` | 2.32.0 |
| TypeScript `e2b` | 2.33.0 |
| Python `e2b-code-interpreter` | 2.8.1 |
| TypeScript `@e2b/code-interpreter` | 2.6.1 |

Current implementation evidence is intentionally narrower than full
compatibility:

| Surface | Implemented preview | Not yet a release claim |
| --- | --- | --- |
| Control plane | Owner-scoped create, connect, get, list, timeout replacement, and kill with SQLite WAL persistence and reconciliation | Complete template, snapshot, volume, metrics, pagination, and recovery semantics |
| Credentials and routing | PBKDF2 account-key hashes, encrypted scope-bound Sandbox tokens, generation-fenced leases, wildcard TLS, direct/shared routes, CORS, and PID-fenced Sandbox network access | Complete certificate rotation and every streaming/upgrade/public-port route |
| envd | Authenticated `GET /health` has running and terminal behavior. Runtime templates receive a fail-closed `POST /init` with the lifecycle ID, merged environment, timestamp, and default user before create succeeds. | `/metrics`, `/envs`, HTTP file transfer, volume-content endpoints, and the complete envd semantic matrix |
| Process | Start/connect/list/input/close/SIGKILL and PTY primitives exist. Pinned Python sync/async and TypeScript clients pass a foreground non-PTY command on production; the extended Python sync path also proves environment propagation and background Start. | Concurrent streaming plus unary Process calls, full signals, binary framing, stdin/close completion, PTY, reconnect, cancellation, ordering, and backpressure |
| Filesystem | Runtime-backed remove, make-directory, write, read, stat, list, rename, exists, and cleanup operations pass through the production TLS route with the pinned official Python sync client. | Async Python and TypeScript evidence, watch and edge-case semantics, signed URLs, and HTTP file transfer |
| Code Interpreter and MCP | Contracts, generated inventories, official package artifacts, and black-box fixtures are pinned. | Code execution, context lifecycle, rich results, MCP execution, and the complete unchanged-client matrix |
| Python and TypeScript packages | Typed source packages re-export the pinned official SDK surfaces, add endpoint configuration helpers, and are included in the native-package production harness. | PyPI/npm publication and a passing complete native-package conformance matrix |

The production `a3s-box-e2b` process accepts only `.acl` configuration parsed
by `a3s-acl`. For runtime-envd templates, create does not become visible until
envd accepts initialization; failed initialization kills the execution and
marks its lifecycle as failed and keeps it hidden instead of returning a
partially usable Sandbox.

```bash
cd src
cargo run --locked -p a3s-box-compat --bin a3s-box-e2b -- \
  --config /etc/a3s-box/e2b.acl
```

The Python package under [`sdk/python`](sdk/python/README.md) and the TypeScript
package under [`sdk/typescript`](sdk/typescript/README.md) are source-tree
previews and are not published to PyPI or npm. Their existence is not evidence
of full protocol compatibility. Until the complete black-box matrix passes,
the generated manifest must continue to report `full_compatibility=false`.

See [E2B Protocol Compatibility and SDK Design](docs/e2b-compatible-sdk-design.md)
for the release definition, architecture, ACL schema, and remaining gates.

## Integrations

### Kubernetes CRI and RuntimeClass

The Linux CRI server exposes the CRI v1 RuntimeService and ImageService over a
Unix socket. Implemented paths include pod/container lifecycle, exec and attach
streaming, logs and reopen, image operations, resources, selected Linux
security context controls, stats, DNS, volumes, and networking.

A separate containerd runtime-v2 shim lets selected pods use:

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: hello-a3s-box
spec:
  runtimeClassName: a3s-box
  containers:
    - name: app
      image: busybox:latest
      command: ["sleep", "3600"]
```

RuntimeClass is opt-in per Linux/KVM node and requires the runtime package,
containerd handler, node label, and matching release assets. The installer and
soak manifests live under [`deploy/`](deploy/). Host namespace sharing,
per-container PID namespaces, some mount propagation, guest AppArmor, and
other architecture-dependent CRI cases are not equivalent to a shared-kernel
container runtime. Review [CRI Conformance](docs/cri-conformance.md) before
cluster evaluation.

### TEE workflows

```bash
# Hardware path: requires a supported SEV-SNP host and libkrun build
a3s-box run -d --name secure --tee image:latest -- sleep 3600
a3s-box attest secure --ratls
a3s-box inject-secret secure --secret API_KEY=value --set-env

# Development-only simulation
a3s-box run -d --name simulated --tee --tee-simulate image:latest -- sleep 3600
a3s-box attest simulated --allow-simulated
```

TEE support includes SNP report parsing/verification, RA-TLS certificate
evidence, AES-256-GCM sealing with HKDF-SHA256, and secret injection.
Simulation validates application flow only and provides no hardware security.
TEE is MicroVM-only; Intel TDX remains a stub rather than a productized path.

### Coding-agent skill

[`integrations/skills/a3s-box/SKILL.md`](integrations/skills/a3s-box/SKILL.md)
teaches skill-capable coding agents the CLI lifecycle, `--` separator, snapshots,
warm pools, networking boundaries, and recovery steps. The installer links the
same source skill into supported agent directories:

```bash
cd integrations/skills
./install.sh all
```

## Architecture

Every consumer submits the same backend-neutral execution request. The
resolver persists the requested isolation, selected backend, effective
isolation class, policy, and required controls before runtime allocation:

```text
CLI / Rust SDK / E2B service / CRI / containerd shim
                         │
                 ExecutionManager
          durable state + generation fencing
                         │
          capability probe + policy resolver
                 ┌───────┴────────┐
                 │                │
       default MicroVM     --isolation sandbox
                 │                │
          krun backend       crun backend
                 │                │
       shim + libkrun       protected OCI bundle
                 │                │
       guest Linux kernel   host Linux kernel
                 │                │
        guest-init + workload services
                 └───────┬────────┘
                         │
       images / rootfs / volumes / networks
       snapshots / logs / audit / metrics
```

The E2B compatibility service calls `ExecutionManager` instead of invoking
`crun` or libkrun directly. That keeps lifecycle ownership, isolation
selection, feature rejection, credentials, audit evidence, and cleanup inside
the runtime boundary.

Main components:

| Component | Responsibility |
| --- | --- |
| `src/core` | Shared execution policy, configuration, protocol types, state primitives, events, logs, and errors |
| `src/runtime` | Canonical execution manager, MicroVM/Sandbox backends, OCI images/builds, storage, networking, pools, snapshots, and TEE clients |
| `src/cli` | Docker-like `a3s-box` command line |
| `src/compat` | Pinned external contracts and the E2B control/data-plane service |
| `src/shim` | libkrun bridge process and platform-specific host integration |
| `src/guest/init` | Guest PID 1, exec, PTY, filesystem, and attestation services |
| `src/netproxy` | macOS user-space bridge, DNS, inbound TCP, and outbound TCP |
| `src/cri` | Kubernetes CRI server |
| `containerd-shim` | containerd runtime-v2 adapter for RuntimeClass |
| `src/sdk` | Direct runtime-backed Rust SDK and optional pipeline runner |
| `src/lambda` | Workload-execution integration retained for higher-level runtimes |
| `sdk/python`, `sdk/typescript` | Unpublished E2B-oriented language SDK previews |

MicroVM guest control uses vsock-backed channels for control/health, exec, PTY,
attestation, and optional sidecars. These are guest-to-host control channels,
not public host TCP endpoints. Sandbox execution projects equivalent local
control sockets through its isolated runtime directory.

## Development

The repository root is orchestration-only. Run Rust checks from `src/`:

```bash
cd src
cargo fmt --all -- --check
cargo test -p a3s-box-core
cargo test -p a3s-box-runtime --lib
cargo test -p a3s-box-cli --test command_coverage
cargo test -p a3s-box-sdk
cargo run -p a3s-box-compat --bin a3s-box-e2b-contract -- verify
```

The contract verification command requires `protoc`. Python and TypeScript SDK
checks run in their own package directories:

```bash
cd sdk/python
python -m pip install -e .
python -m unittest discover -s tests

cd ../typescript
npm run build
npm test
```

Host-backed MicroVM, Sandbox, networking, build, CRI, and endurance tests must
run on an explicitly prepared host with isolated runtime state. The validation
entry points are:

- [`scripts/host-integration-smoke.sh`](scripts/host-integration-smoke.sh) for
  macOS/HVF and Linux/KVM;
- [`scripts/e2b-production-smoke.sh`](scripts/e2b-production-smoke.sh) for the
  destructive A3S OS Sandbox compatibility gate;
- [Production Cluster Tests](docs/production-cluster-tests.md) for enrolled
  RuntimeClass nodes and soak evidence.

Do not infer a production claim from unit tests, fixture servers, or simulated
TEE results. Record the host, backend, image digest, runtime version, and
evidence bundle for every real-runtime release gate.

## License

MIT. Vendored protocol sources, generated fixtures, and language packages
retain the license metadata shipped in their respective directories.
