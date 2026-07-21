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

### A3S SDKs with E2B-compatible APIs

A3S Box implements the remote runtime and protocol service. The A3S Python and
TypeScript packages re-export the pinned official E2B objects and add typed A3S
endpoint configuration, so applications keep the familiar `Sandbox`,
`AsyncSandbox`, Commands, Filesystem, PTY, Volume, filesystem Snapshot, and
Code Interpreter surfaces. No E2B-hosted service is involved, and native A3S
SDK applications do not set `E2B_API_URL`.

Point the native SDK at the deployed A3S Box service. A conventional
`https://api.<domain>` endpoint automatically derives `<domain>` for Sandbox
routing:

```bash
export A3S_BOX_ENDPOINT=https://api.box.example.com
export A3S_BOX_API_KEY=a3s_your_key
```

Set `A3S_BOX_DOMAIN` only when the control endpoint does not follow the
conventional `api.<domain>` form. The service advertises the direct Sandbox
authority, including a non-standard public TLS port, so normal deployments do
not need a client-side Sandbox URL override. `A3S_BOX_SANDBOX_URL` remains a
single-Sandbox fixture escape hatch. Normal A3S SDK applications use only the
`A3S_BOX_*` connection settings; the endpoint is always an A3S Box deployment.
`E2B_API_URL` appears only in the optional unchanged-official-SDK migration
path documented below.

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

Both packages pass the production matrix described below. The release workflow
builds wheel and npm tarball artifacts, but the packages are not yet published
to PyPI or npm. The verified subset and remaining full-compatibility gates are
stated under [SDKs and Compatibility](#sdks-and-compatibility).

## Features

- **Two explicit isolation classes**: Hardware-backed MicroVM execution by
  default and opt-in shared-kernel Sandbox execution through certified `crun`
- **Docker-like lifecycle**: Create, start, stop, restart, kill, pause, wait,
  remove, inspect, exec, PTY, attach, logs, health checks, and restart policies
- **OCI image workflow**: Pull with bounded retry, Range resume, concurrent
  layers, and verified cross-image blob reuse; push, authenticate, verify
  digests and optional cosign signatures, tag, inspect, save, load, import,
  remove, and cache images
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
  a provider-neutral A3S Runtime adapter, an optional programmable pipeline
  runner, and a production-tested E2B protocol subset with Python and
  TypeScript packages
- **Operations and cluster integration**: Structured logs, stats, events,
  Prometheus endpoints, health monitoring, CRI, and containerd RuntimeClass

### Capability matrix

| Area | Current capability | Status and boundary |
| --- | --- | --- |
| MicroVM runtime | libkrun-backed OCI execution on Linux/KVM and Apple Silicon/HVF | Primary local runtime. Each box has its own guest kernel. Host-backed validation is required for releases. |
| OCI Sandbox | Explicit `--isolation sandbox` execution through certified `crun 1.28` on Linux | Preview. Shares the host kernel, never replaces or emulates MicroVM isolation, and still has open security-negative and performance release gates. |
| A3S Runtime provider | Sandbox-backed Task and Service lifecycle, recovery, `none` networking, bounded tmpfs mounts, CPU/memory/PID controls, structured logs, idempotent exec, and security fencing | The real-provider R17 suite passes Base, Recovery, Networking, Mounts, Resources, Logs, Exec, and Security profiles. Artifacts must be digest-pinned; the provider advertises only `tmpfs` mounts and `none` networking. |
| Lifecycle and exec | Foreground/detached runs, managed create/start/restart/kill, exec, PTY, logs, health, wait, and cleanup | Implemented for MicroVM. The managed Sandbox path and its structured logs are implemented, while complete parity and adversarial validation remain in progress. |
| OCI images | Resumable bounded registry pulls, concurrent layers, verified cross-image blob reuse, push, credentials, digest verification, optional cosign verification/signing, indexed archive selection, and tag operations | Implemented. `pull` validates declared size and SHA-256 before atomic blob publication; `load` selects one Linux platform from direct or nested OCI indexes and verifies the selected manifest, config, and layers before publishing it. Registry throughput and redirect behavior still require validation against each production registry. |
| Dockerfile builds | Built-in Dockerfile subset, layer cache, BuildKit-in-MicroVM, and warm-pool `RUN` execution | Implemented subset, not a full Buildx replacement. One target platform is recorded per build. |
| Storage | Bind mounts, named volumes, tmpfs, `cp`, `diff`, `export`, `commit`, filesystem snapshots, and CoW restore | Implemented. Filesystem snapshots do not contain live VM RAM or device state. |
| Networking and Compose | TSI, bridge networks, TCP publishing, peer discovery, and Compose lifecycle/config/logs | Implemented subset for MicroVM workloads. UDP publishing, host-IP binds, ranges, and live network hot-plug are not implemented. |
| Warm pool and snapshot-fork | Pre-booted MicroVMs, one-shot runs, build leases, metrics, and CoW memory restore | Implemented. Native snapshot-fork is Linux/KVM-only and disabled by default. |
| Rust SDK | Typed, direct runtime-backed management and guest-control APIs | Implemented in `a3s-box-sdk`. The optional `pipeline-cli` feature retains the CLI-driven programmable pipeline. |
| E2B protocol and language SDKs | Pinned contracts, durable lifecycle, memory-preserving pause/resume, owner-scoped filesystem Snapshots and Volumes, v1/v2 listing and runtime-backed structured logs, current metrics, TLS routing, envd file/environment operations, Filesystem, Process, PTY, and Python Code Interpreter contexts | Production-tested preview subset on A3S OS with certified `crun`: pinned official Python sync/async and TypeScript clients, plus A3S Python sync/async and TypeScript packages, pass the same matrix, including Snapshot capture/list/restore/delete, filesystem and OCI-default fidelity, bidirectional Volume mounts, pause/resume process survival, and generation-fenced logs. Templates/builds, filesystem-only pause, historical metrics, signed files, public-port breadth, MCP, cancellation/backpressure, deeper Snapshot/Volume failure recovery, and the rest of the pinned contract remain gates; `full_compatibility=false`. PyPI/npm publication is also pending. |
| TEE | SEV-SNP-oriented attestation, RA-TLS, sealing, secret injection, and simulation | Host-specific. Hardware claims require a supported SEV-SNP host and real attestation evidence. Simulation is development-only; TDX is not productized. |
| Kubernetes | CRI server plus a containerd runtime-v2 shim and `runtimeClassName: a3s-box` | Preview. Core lifecycle, streaming, logs, resources, and RuntimeClass paths exist; complete CRI conformance is not claimed. |
| Windows | Native x86_64 WHPX/libkrun MicroVM execution | Host-specific. Foreground and detached workloads, live split logs, exit codes, TCP publishing, bind mounts, named-volume persistence, stats, stopped-box commit, and stopped-box filesystem snapshot flows have passed real-host validation. Running-box commit has no guest archive channel. The reliable path is currently limited to one vCPU; container health checks, bridge networking, interactive execution, TEE, snapshot-fork, and CRI remain unsupported. |

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
| Windows | x86_64, Windows Hypervisor Platform, Developer Mode (or `SeCreateSymbolicLinkPrivilege`), and matching `krun.dll`/`libkrunfw.dll` assets | Native release path; currently one vCPU, with the command boundaries documented below |

Linux release archives intentionally contain GNU/glibc host executables because
the CLI, CRI service, and VM shim dynamically load the bundled `libkrun`. The
separately downloadable `containerd-shim-a3s-box-v2-linux-<arch>` and the guest
init are built for the corresponding static musl target.

For a Windows source build, build the Linux guest PID 1 with Zig before the
native binaries:

```powershell
winget install --id zig.zig --exact --version 0.16.0
cd src
powershell.exe -NoProfile -ExecutionPolicy Bypass -File deps/libkrun-sys/vendor/libkrun/scripts/build-windows-init.ps1
cargo install cargo-zigbuild
cargo zigbuild --release -p a3s-box-guest-init --target x86_64-unknown-linux-musl
cargo build --release -p a3s-box-cli -p a3s-box-shim
```

The nested script creates libkrun's stripped Linux init payload; `cargo
zigbuild` creates A3S Box's static Linux guest-init executable. Neither generated
binary is committed. The Linux kernel is supplied by the packaged
`libkrunfw.dll`; A3S Box does not compile a kernel during this build. See
[docs/windows-whpx.md](docs/windows-whpx.md) for package layout, validation
commands, and current platform limits.

Always run `a3s-box info` before host-backed tests. It reports virtualization,
platform, networking and port-publishing support, package caches, TEE,
virtio-fs, and warm-pool availability without starting a workload.

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

On Windows, omit `--cpus` or use `--cpus 1`; higher counts are rejected before
the image is pulled until the WHPX SMP path is reliable.

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

`compose.acl` is the canonical project format; explicit Compose YAML remains a
bounded compatibility input. Both formats pass through the same pure,
deterministic normalizer. Unknown fields fail with stable diagnostic codes and
JSON Pointer-style paths instead of being silently ignored. Embedding and
Runtime-boundary details are documented in
[Compose Normalization](docs/compose-normalization.md).

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
Auto-removed boxes retain terminal exit metadata, plus logs when enabled and
available, under the removed-box retention limits. Both `wait` and `logs` can
therefore resolve a removed box by name or ID after its live state is gone.

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

### A3S Runtime provider

The `a3s-box-runtime` crate includes a provider-neutral A3S Runtime driver
backed by the certified shared-kernel Sandbox. It maps digest-pinned Tasks and
Services onto durable Box executions with generation fencing, recovery,
structured logs, bounded idempotent exec, CPU/memory/PID controls, and tmpfs
mounts. Tmpfs requests preserve byte limits and `ro`/`rw` intent, reject
protected destinations, start empty for each Sandbox generation, and are
removed with their owner process and provider record.

The driver advertises only capabilities it maps losslessly: Sandbox isolation,
`none` networking, and tmpfs mounts. Bind, volume, and artifact mounts remain
unadvertised, and unsupported input fails before provider mutation. The
opt-in R17 gate runs every activated profile against real `crun`, including
provider-effect cancellation replay, partial-creation adoption, client and
provider restart recovery, external deletion, duplicate detection, and cleanup
inventory equality.

### OCI images and builds

```bash
a3s-box pull alpine:latest
a3s-box pull --verify-key cosign.pub ghcr.io/example/app:v1
a3s-box image-inspect alpine:latest
a3s-box tag alpine:latest local/alpine:dev
a3s-box save -o alpine.tar alpine:latest
a3s-box load -i alpine.tar --tag local/alpine:dev --platform linux/amd64
a3s-box push registry.example/app:v1
```

Registry authentication comes from `a3s-box login`, Docker-compatible
configuration, or explicit registry environment credentials. Manifest,
configuration, and layer digests are checked during pull. Authentication is
retained only across same-origin redirects, and decompression limits protect
image and build extraction.

Registry configuration and layer transfers use a bounded retry policy. A
partial blob is resumed with `Range: bytes=<offset>-` when the registry returns
`206 Partial Content`; a registry that ignores Range and returns `200 OK`
causes the partial file to be reset before the full response is written. Each
authentication, response-header, body-chunk, and file-write wait has a
no-progress deadline. Independent layers are downloaded concurrently up to the
configured bound, and `a3s-box pull` reports actual downloaded bytes, retry
attempts, backoff delays, cache reuse, and completion.

Before any blob becomes visible, its declared size and canonical SHA-256 digest
must match. The image store also searches indexed image layouts for matching
configuration and layer blobs. A candidate is materialized through a Linux
reflink when supported or a private byte copy otherwise, verified again, and
published atomically without linking the destination directly to the source
cache entry.

| Registry pull setting | Default | Meaning |
| --- | ---: | --- |
| `A3S_REGISTRY_PULL_MAX_ATTEMPTS` | `4` | Total attempts per config or layer, including the first request |
| `A3S_REGISTRY_PULL_RETRY_INITIAL_MS` | `250` | Initial transient-failure backoff |
| `A3S_REGISTRY_PULL_RETRY_MAX_MS` | `4000` | Exponential-backoff cap |
| `A3S_REGISTRY_PULL_NO_PROGRESS_TIMEOUT_SECS` | `30` | Maximum wait without header, body, or file-write progress |
| `A3S_REGISTRY_PULL_MAX_CONCURRENT` | `4` | Maximum simultaneous layer downloads |

All overrides must be positive, and the maximum retry delay must not be lower
than the initial delay. Invalid overrides are ignored in favor of safe
defaults. Rust callers can supply the same validated policy explicitly with
`RegistryPullPolicy::try_new` and `ImagePuller::with_pull_policy`.

`load` accepts direct manifests and nested OCI or Docker image indexes. It
selects `--platform OS/ARCH[/VARIANT]`, defaulting to Linux on the host
architecture, and normalizes the stored layout to the selected manifest.
Declared sizes and SHA-256 digests for the selected index path, manifest,
configuration, and layers are verified before the tag becomes visible.

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
path is selected. A3S Box does not claim complete Dockerfile or Buildx
compatibility and does not build multi-platform indexes; archive loading can
import one selected platform from an existing index.

### Filesystems, volumes, and snapshots

```bash
a3s-box volume create data
a3s-box run -d --name app -v data:/data alpine:latest -- sleep 3600
a3s-box cp ./input.txt app:/data/input.txt
a3s-box diff app
a3s-box export app -o rootfs.tar
a3s-box commit app app:checkpoint
a3s-box stop app
a3s-box snapshot create app --name checkpoint-1
a3s-box snapshot restore checkpoint-1 --name restored-app
```

Named volumes persist independently of a box. Host bind mounts use virtio-fs
for MicroVMs, while Sandbox mounts are validated against the selected UID/GID
mapping. `--package-cache pnpm|npm` creates reusable named caches for
short-lived Node.js workloads, and tmpfs is useful for high-churn dependency
trees.

Filesystem snapshots capture configuration and rootfs state, not live RAM or
device state. Direct CLI/SDK snapshots require a stopped source box so a guest
cannot race host filesystem traversal; managed Sandbox snapshots quiesce the
backend before capture. On overlay-capable hosts, restore uses a read-only snapshot lower
plus a private writable upper; in-use snapshots are protected from pruning.
Snapshots created by current builds also retain resolved OCI image defaults
and Unix rootfs metadata. Older records missing those defaults remain visible
for inspection and deletion, but restore fails closed because the original
entrypoint, environment, user, and working directory cannot be reconstructed
safely. Live MicroVM memory cloning is the separate snapshot-fork mechanism
below.

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

### E2B-compatible protocol preview

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

The production-tested subset remains intentionally narrower than full
compatibility:

| Surface | Implemented preview | Not yet a release claim |
| --- | --- | --- |
| Control plane | Owner-scoped create, connect, get, memory-preserving pause, connect/resume, v1 running list, v2 filtered running/paused list, timeout replacement, monotonic refresh, kill, current single/batch metrics, and structured v1/v2 logs for runtime-envd Sandboxes, with SQLite WAL persistence, restart reconciliation, cleanup, and a complete requested timeout measured from runtime and envd readiness | Templates/builds, filesystem-only pause, network updates, historical metric retention, cache attribution, full pagination edge cases, and host-reboot recovery semantics |
| Credentials and routing | PBKDF2 account-key hashes, encrypted scope-bound Sandbox tokens, generation-fenced leases, wildcard TLS, direct/shared routes, CORS, HTTP/2, and PID-fenced Sandbox access | Certificate rotation and the complete streaming, upgrade, signed-file, and public-port route matrix |
| Volumes | Owner-scoped create, connect/get, list, and delete with durable records, encrypted tokens, startup reconciliation, authenticated content operations, and named Sandbox mounts; all six production clients pass bidirectional I/O, UID/GID mapping, public mount metadata, in-use deletion conflicts, and cleanup | Complete large-file, concurrent-mutation, service-crash, host-reboot, and negative-path breadth before treating Volume coverage as a standalone compatibility claim |
| Filesystem Snapshots | Owner-scoped capture, source-filtered list, restore, and delete with durable records, startup reconciliation, quiesced rootfs capture, copy-on-write restore, OCI image-default and Unix metadata fidelity, and active-use deletion conflicts; all six production clients restore captured state after deleting the source Sandbox | Filesystem only: no process memory or device state. Named-reference, pagination, large-rootfs, concurrent mutation, service-crash, host-reboot, and broader negative-path coverage remain release gates |
| envd | Authenticated running/terminal health; fail-closed runtime initialization; production-validated `/metrics`, `/envs`, metadata-preserving multipart upload, and octet-stream download through wildcard TLS routing | Multi-file, large-file, invalid-path/user, not-found, insufficient-space, and remaining envd edge semantics |
| Process and PTY | Official and A3S Python sync/async and TypeScript clients pass foreground and background commands, list, stdin send/close, wait, PTY create/resize/input/wait, and ordered output on real Sandboxes | Additional signals, binary framing, reconnect, cancellation, backpressure, and adversarial concurrent-stream coverage |
| Filesystem | The same six clients pass remove, make-directory, write, read, stat, list, rename, exists, and cleanup through production TLS routing; the envd HTTP path separately passes upload/download with metadata | Watch, multi-file and ownership edge cases, signed URLs, large-file behavior, and negative-path breadth |
| Code Interpreter and MCP | Official and A3S Python sync/async and TypeScript clients execute Python, validate stdout/results, and pass context create/list/run/restart/remove | Other languages, rich MIME/error/cancellation breadth, MCP execution, and the rest of the pinned interpreter contract |
| Python and TypeScript packages | Typed packages re-export the pinned official surfaces, use `A3S_BOX_*` connection configuration, and pass the production matrix with all `E2B_*` connection variables removed | PyPI/npm publication and conformance for the unimplemented protocol surfaces above |

The production `a3s-box-e2b` process accepts only `.acl` configuration parsed
by `a3s-acl`. For runtime-envd templates, create does not become visible until
envd accepts initialization; failed initialization kills the execution and
marks its lifecycle as failed and keeps it hidden instead of returning a
partially usable Sandbox.

Memory-preserving pause maps to certified `crun pause`; a later `connect` or
deprecated `resume` request maps to `crun resume`. The production matrix starts
a background process before pausing and proves that the same process continues
after resume. Filesystem-only pause (`memory: false`) is rejected explicitly
until cold-pause semantics are implemented.

```bash
cd src
cargo run --locked -p a3s-box-compat --bin a3s-box-e2b -- \
  --config /etc/a3s-box/e2b.acl
```

The Python package under [`sdk/python`](sdk/python/README.md) and the TypeScript
package under [`sdk/typescript`](sdk/typescript/README.md) pass the production
matrix and are built as GitHub Release assets. They are not yet published to
PyPI or npm. Passing this subset is not evidence for unimplemented protocol
surfaces, so the generated manifest continues to report
`full_compatibility=false`.

#### Zero-source-change migration from an official E2B SDK

An application that deliberately keeps the unmodified official `e2b` package
can point that client at A3S Box with the configuration names already defined
by the official SDK:

```bash
export E2B_API_URL=https://api.box.example.com
export E2B_DOMAIN=box.example.com
export E2B_API_KEY=e2b_a1b2c3
```

These are client-side compatibility names only. `E2B_API_URL` points to the
A3S Box control endpoint; A3S Box does not depend on or contact an E2B-hosted
service. New A3S integrations should use the A3S SDK example at the beginning
of this README and its `A3S_BOX_*` settings.

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
| `sdk/python`, `sdk/typescript` | Production-tested A3S language SDK packages; public registry publication pending |

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
