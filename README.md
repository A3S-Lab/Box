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
  <a href="#native-sdks">SDKs</a> •
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

The local CLI and native SDKs are the primary product surfaces. OCI Sandbox
execution, Kubernetes CRI/RuntimeClass integration, TEE workflows, and Windows
support have different maturity and host requirements; the capability matrix
below states those boundaries explicitly.

### Basic usage

```bash
# Default: one libkrun MicroVM with its own Linux kernel
a3s-box run --rm alpine:latest -- uname -a

# Explicit opt-in: shared-kernel OCI Sandbox on a certified Linux host
a3s-box run --rm --isolation sandbox alpine:latest -- id
```

### Local SDKs

The A3S Rust, Python, and TypeScript packages expose familiar `Sandbox`,
`commands`, and `files` namespaces while executing through the A3S Box runtime
installed on the same machine.

Local SDK use is zero-configuration: do not set an endpoint, domain, or API
key. The Rust SDK calls the runtime directly. Python and TypeScript find
`a3s-box` on `PATH` and talk to its versioned, machine-only bridge instead of
parsing human CLI output.

Rust:

```rust
use a3s_box_sdk::Sandbox;

# async fn example() -> Result<(), a3s_box_sdk::ClientError> {
let sandbox = Sandbox::create("python:3.12-alpine").await?;
let result = sandbox
    .commands
    .run("python -c 'print(6 * 7)'")
    .await?;
println!("{}", result.stdout);
sandbox.files.write("/workspace/note.txt", "hello").await?;
sandbox.kill().await?;
# Ok(()) }
```

Python:

```python
from a3s_box import Sandbox

with Sandbox.create("python:3.12-alpine") as sandbox:
    result = sandbox.commands.run("python -c 'print(6 * 7)'")
    print(result.stdout)
    sandbox.files.write("/workspace/note.txt", "hello")
```

TypeScript:

```typescript
import { Sandbox } from '@a3s-lab/box'

const sandbox = await Sandbox.create('python:3.12-alpine')

try {
  const result = await sandbox.commands.run(
    'python -c "print(6 * 7)"'
  )
  console.log(result.stdout)
} finally {
  await sandbox.kill()
}
```

The same packages also expose builder-style programmable CI/CD without
introducing a separate workflow engine. Builders manage host-level images,
named volumes, and bridge networks, then create the same local `Sandbox`
handle:

```typescript
import { A3SBoxClient } from '@a3s-lab/box'

const client = new A3SBoxClient()
const image = await client
  .image('./ci')
  .dockerfile('Dockerfile')
  .tag('local/ci-base:latest')
  .build()
const cache = await client.volume('build-cache').create()
const network = await client.network('ci-net').subnet('10.89.40.0/24').create()
const sandbox = await client
  .sandbox(image.reference)
  .mountNamed(cache.name, '/cache')
  .network(network.name)
  .publishTcp(8080, 8080)
  .start()

try {
  const result = await sandbox
    .script('npm ci\nnpm test\n')
    .interpreter('/bin/sh', '-se')
    .run()
  if (result.exitCode !== 0) throw new Error(result.stderr)
} finally {
  await sandbox.kill()
}
```

Rust, synchronous/asynchronous Python, and TypeScript expose the same builder
concepts. Script source travels over stdin to an explicit interpreter. Mounts,
network selection, ports, tmpfs, workdir, persistence, cleanup, and snapshot
restore remain typed values and are validated before runtime mutation. Named
bridge networking and port publication are MicroVM-only in the current
runtime; the Sandbox backend rejects them rather than silently weakening the
request.

Use a runtime and language SDK built from the same release or source revision;
the machine bridge is versioned and rejects incompatible requests.

Python and TypeScript default to `alpine:3.20`. Rust
`Sandbox::create(...)` requires an image argument, while
`SandboxCreateOptions::default()` supplies `alpine:3.20`. All three default to
MicroVM isolation, and the image is a local OCI image reference. Set
`A3S_BOX_BINARY` only for Python or TypeScript when the matching `main`
executable is not on `PATH`. Select `ExecutionIsolation::Sandbox`,
`isolation="sandbox"`, or `isolation: 'sandbox'` respectively to opt into the
shared-kernel backend on a certified Linux host.

The local handles also expose generation-fenced lifecycle and inspection
operations. `stop()` leaves the durable Sandbox available for an idempotent
`restart(...)`; `remove()` deletes a terminal Sandbox; and `kill()` composes
stop plus removal. `logs(...)` returns a bounded structured stdout/stderr
snapshot, while `stats()` returns the current host resource snapshot when the
Sandbox is active. `A3SBoxClient` lists and gets Sandboxes and filesystem
snapshots, and reports runtime diagnostics and disk usage. Python provides the
same surface in synchronous and asynchronous forms.

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
- **Typed native SDKs**: Direct runtime-backed Rust management APIs, a
  provider-neutral A3S Runtime adapter, Python sync/async and TypeScript
  packages over a checked machine bridge, and fluent programmable CI/CD
  builders
- **Operations and cluster integration**: Structured logs, stats, events,
  Prometheus endpoints, health monitoring, CRI, and containerd RuntimeClass

### Capability matrix

| Area | Current capability | Status and boundary |
| --- | --- | --- |
| MicroVM runtime | libkrun-backed OCI execution on Linux/KVM and Apple Silicon/HVF | Primary local runtime. Each box has its own guest kernel. Host-backed validation is required for releases. |
| OCI Sandbox | Explicit `--isolation sandbox` execution through certified `crun 1.28` on Linux | Preview. Shares the host kernel, never replaces or emulates MicroVM isolation, and still has open security-negative and performance release gates. |
| A3S Runtime provider | Sandbox-backed Task and Service lifecycle, recovery, `none` networking, bounded tmpfs mounts, CPU/memory/PID controls, structured logs, idempotent exec, and security fencing | The real-provider R17 suite passes Base, Recovery, Networking, Mounts, Resources, Logs, Exec, and Security profiles. Artifacts must be digest-pinned; the provider advertises only `tmpfs` mounts and `none` networking. |
| Lifecycle and exec | Foreground/detached runs, managed create/start/restart/kill/pause/resume, exec, PTY, logs, health, wait, and cleanup | Implemented for MicroVM. The managed Sandbox path uses certified `crun` lifecycle operations, including freezer-backed pause/resume, while complete parity and adversarial validation remain in progress. |
| OCI images | Resumable bounded registry pulls, concurrent layers, verified cross-image blob reuse, push, credentials, digest verification, optional cosign verification/signing, indexed archive selection, and tag operations | Implemented. `pull` validates declared size and SHA-256 before atomic blob publication; `load` selects one Linux platform from direct or nested OCI indexes and verifies the selected manifest, config, and layers before publishing it. Registry throughput and redirect behavior still require validation against each production registry. |
| Dockerfile builds | Built-in Dockerfile subset, layer cache, BuildKit-in-MicroVM, and warm-pool `RUN` execution | Implemented subset, not a full Buildx replacement. One target platform is recorded per build. |
| Storage | Bind mounts, named volumes, tmpfs, `cp`, `diff`, `export`, `commit`, filesystem snapshots, and CoW restore | Implemented. Filesystem snapshots do not contain live VM RAM or device state. |
| Networking and Compose | TSI, bridge networks, TCP publishing, peer discovery, and Compose lifecycle/config/logs | Implemented subset for MicroVM workloads. UDP publishing, host-IP binds, ranges, and live network hot-plug are not implemented. |
| Warm pool and snapshot-fork | Pre-booted MicroVMs, one-shot runs, build leases, metrics, and CoW memory restore | Implemented. Native snapshot-fork is Linux/KVM-only and disabled by default. |
| Rust SDK | Typed, direct runtime-backed management and guest-control APIs; local `Sandbox`, commands, files, generation-fenced lifecycle, bounded structured logs, stats, and filesystem snapshots; fluent image, volume, network, Sandbox, and script builders | The local facade and builder-style programmable CI/CD API are implemented and package-tested, default to MicroVM, and explicitly accept supported shared-kernel Sandbox configurations. Sandbox stop/restart/remove, management list/get, runtime diagnostics/disk usage, and snapshot list/get use direct runtime calls. The optional historical `pipeline-cli` feature remains separate. |
| Local Python and TypeScript SDKs | Local `Sandbox`, commands, files, lifecycle, logs, and stats, plus fluent image-build, named-volume, bridge-network, typed Sandbox, stdin-backed script builders, full local image lifecycle, runtime inspection, snapshot inspection, and resource pruning over the installed local runtime | Implemented and package-tested on `main`. The packages share the Rust implementation through the checked versioned bridge, expose a runtime capability inventory and typed registry credentials/signature policy, and require no endpoint or API key. The real macOS/HVF MicroVM three-language smoke passes; the expanded Ubuntu/crun 1.28 Sandbox matrix is a blocking CI gate and covers the local-store image and pruning operations. Linux/KVM MicroVM validation remains host-specific, structured build/registry progress is pending, and the native packages are not yet published to PyPI or npm. |
| TEE | SEV-SNP-oriented attestation, RA-TLS, sealing, secret injection, and simulation | Host-specific. Hardware claims require a supported SEV-SNP host and real attestation evidence. Simulation is development-only; TDX is not productized. |
| Kubernetes | CRI server plus a containerd runtime-v2 shim and `runtimeClassName: a3s-box` | Preview. Core lifecycle, streaming, logs, resources, and RuntimeClass paths exist; complete CRI conformance is not claimed. |
| Windows | Native x86_64 WHPX/libkrun MicroVM execution | Implemented and real-host validated from source; published Windows archives remain pending. Foreground and detached workloads, long arguments staged outside the guest kernel command line, live split logs, exit codes, TCP publishing, bind mounts, named-volume persistence, stats, stopped-box commit, and stopped-box filesystem snapshot flows pass. Running-box commit has no guest archive channel. The reliable path is currently limited to one vCPU; container health checks, bridge networking, interactive execution, TEE, snapshot-fork, and CRI remain unsupported. |

An implemented API is not automatically a production guarantee for every host
or threat model. Real-runtime validation evidence and remaining platform gaps
are maintained in [Host Integration](docs/host-integration.md),
[Production Cluster Tests](docs/production-cluster-tests.md), and
[CRI Conformance](docs/cri-conformance.md). The
[Cross-Capability Soak Test Plan](docs/soak-test-plan.md) maps every capability
above to its long-running workload, host matrix, evidence contract, and release
gate.

## Quick Start

### Installation

Install the current stable macOS or Linux CLI/runtime from the Homebrew tap:

```bash
brew install a3s-lab/tap/a3s-box
a3s-box info
```

To evaluate the current source implementation, build the runtime and language
packages from the same checkout:

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
| Windows | x86_64, Windows Hypervisor Platform, Developer Mode (or `SeCreateSymbolicLinkPrivilege`), and matching `krun.dll`/`libkrunfw.dll` assets | Source-build path; published Windows archives remain pending. Currently one vCPU, with the command boundaries documented below |

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
mode, published ports, named bridge networking, custom sysctls, vsock sidecars,
and unconfined seccomp.

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

## Native SDKs

A3S Box exposes one local runtime through Rust, Python, and TypeScript. All
three SDKs manage the same OCI images, Sandboxes, volumes, networks, snapshots,
logs, and runtime state used by the CLI. They do not introduce a separate
scheduler or remote control plane.

| Language | Package | Runtime access |
| --- | --- | --- |
| Rust | `a3s-box-sdk` | Calls the runtime libraries and generation-fenced execution manager directly |
| Python | `a3s-box` | Synchronous and asynchronous APIs over the installed `a3s-box` machine bridge |
| TypeScript | `@a3s-lab/box` | Promise-based APIs over the installed `a3s-box` machine bridge |

MicroVM isolation is the default in every language. Shared-kernel Sandbox
execution must be selected explicitly and is available only on a certified
Linux host. Keep the runtime and SDKs on the same release or source revision.

### Sandbox execution and lifecycle

`Sandbox` is the common high-level handle. It provides:

- local creation and reconnection;
- foreground commands with argv or an explicit shell, environment, working
  directory, user, stdin, and timeout options;
- text and binary file read/write plus stat, list, directory creation, move,
  and remove operations;
- pause, resume, stop, restart, remove, and deterministic cleanup;
- bounded structured stdout/stderr log snapshots and current host resource
  statistics;
- filesystem snapshot creation and restoration through typed creation
  options.

Lifecycle transitions use the same durable, generation-fenced execution
manager as the CLI. `stop()` preserves the Sandbox record, `restart()` starts a
new generation, `remove()` deletes a created or terminal Sandbox, and `kill()`
performs stop and removal. Reuse the same operation ID when retrying a restart
whose outcome is unknown.

```rust,no_run
use a3s_box_sdk::{
    OperationId, Sandbox, SandboxLogOptions, SandboxRestartOptions,
};

# async fn example() -> Result<(), a3s_box_sdk::ClientError> {
let sandbox = Sandbox::create("alpine:3.20").await?;

let output = sandbox.commands.run("printf 'ready\\n'").await?;
assert_eq!(output.exit_code, 0);

let logs = sandbox.logs(SandboxLogOptions::tail(100)).await?;
let stats = sandbox.stats().await?;
println!("{} log entries; stats available: {}", logs.len(), stats.is_some());

sandbox.stop().await?;
sandbox
    .restart(
        SandboxRestartOptions::default()
            .operation_id(OperationId::new("ci-restart-1")?)
            .stop_timeout_seconds(10),
    )
    .await?;
sandbox.kill().await?;
# Ok(()) }
```

Log tails accept 1 through 10,000 entries. Statistics are a point-in-time host
snapshot and are available only while the runtime can identify an active local
process.

### Builder-style programmable CI/CD

The runtime client provides fluent builders for code-first build and delivery
automation. A program can build a base image, create reusable storage and
network resources, configure an isolated Sandbox, run scripts through stdin,
inspect results, snapshot its filesystem, and clean up without adopting a
second workflow language.

```python
from a3s_box import A3SBoxClient

client = A3SBoxClient()
image = (
    client.image("./ci")
    .dockerfile("Dockerfile")
    .tag("local/ci-base:latest")
    .build_arg("NODE_VERSION", "24")
    .build()
)
cache = client.volume("npm-cache").label("purpose", "ci-cache").create()
network = client.network("ci-net").subnet("10.89.40.0/24").create()

with (
    client.sandbox(image.reference)
    .cpus(4)
    .memory_mb(4096)
    .mount_named(cache.name, "/root/.npm")
    .network(network.name)
    .publish_tcp(8080, 8080)
    .workdir("/workspace")
    .start()
) as sandbox:
    result = (
        sandbox.script("npm ci\nnpm test\n")
        .interpreter("/bin/sh", "-se")
        .env("CI", "true")
        .run()
    )
    if result.exit_code != 0:
        raise RuntimeError(result.stderr)
```

The same builder concepts are available in Rust, synchronous/asynchronous
Python, and TypeScript:

| Builder | Available configuration |
| --- | --- |
| Image | Build context, Dockerfile, target, tag, platforms, build arguments, cache control, and quiet mode |
| Volume | Name, labels, size limit, explicit creation, and typed mounting |
| Network | Name, subnet, labels, explicit creation, and typed selection |
| Sandbox | Image, isolation, CPU, memory, lifetime, environment, metadata, user, hostname, workdir, bind/named/tmpfs mounts, networking, DNS, host aliases, TCP ports, read-only root, persistence, cleanup, and snapshot restore |
| Script | Source over stdin, explicit interpreter argv, environment, workdir, user, timeout, and structured exit result |

Named volumes and networks must exist before a Sandbox selects them. Script
source is sent to the chosen interpreter over stdin and is never interpolated
into a host shell command. Named bridge networks and published ports are
currently MicroVM-only; shared-kernel Sandbox requests containing either are
rejected before runtime state changes.

See [SDK API and Programmable CI/CD](docs/sdk-api-and-programmable-cicd.md) for
the cross-language contract and current completion gates.

### Runtime and resource management

The following table distinguishes the implemented native surface from work
that is not yet exposed consistently in every language:

| Area | Available now | Current boundary |
| --- | --- | --- |
| Images and builds | Build, pull, get, list, inspect, history, tag, push, remove, and cache eviction; typed registry credentials, signature verification policy, platform, and registry protocol options | Build and registry progress is returned at completion rather than streamed |
| Volumes | Create, get, list, remove, prune, and typed bind/named mounts | Direct volume-content helpers are not yet public |
| Networks | Create, get, list, remove, prune, typed selection, and TCP publication | Bridge attachment and published ports are MicroVM-only; UDP, port ranges, and live hot-plug are not implemented |
| Sandboxes | Create, connect, inspect, list, pause, resume, stop, idempotent restart, kill, remove, logs, stats, and filesystem snapshot creation | Cancellation-aware cleanup and event streaming remain pending |
| Files and artifacts | Binary/text read and write, stat, list, mkdir, move, and remove | Confined artifact export, hashes, and large-file streaming remain pending |
| Filesystem snapshots | Create, list, get, size, restore into a new Sandbox, delete, and live-use fencing | Filesystem state only; snapshots do not include VM memory or device state |
| Diagnostics | Runtime/core/SDK versions, home path, virtualization availability, disk usage, and exact machine-bridge capability inventory | Historical health, event, and audit query APIs are not yet available in every language |
| Commands | Foreground argv, explicit-shell, and stdin-backed script execution with structured stdout, stderr, exit code, and truncation state | Cross-language background process handles, output streaming, signals, and PTY are still pending |

Private registry operations may require registry credentials, but normal local
Sandbox and resource management require only the installed A3S Box runtime.

### Language packages

- Rust [`a3s-box-sdk`](src/sdk/README.md) is the implementation source of
  truth. `A3sBoxClient` calls runtime services directly and
  `A3sBoxClient::from_home(path)` supports isolated state directories.
- Python [`a3s-box`](sdk/python/README.md) provides synchronous
  `Sandbox`/`A3SBoxClient`, asynchronous `AsyncSandbox`/`A3SAsyncBoxClient`,
  and context-manager cleanup.
- TypeScript [`@a3s-lab/box`](sdk/typescript/README.md) provides promise-based
  `Sandbox`/`A3SBoxClient` APIs for Node.js 20 or newer. Use `try`/`finally` to
  own cleanup.

The optional historical Rust pipeline runner remains behind the
`pipeline-cli` feature and is separate from the fluent runtime builders.

### Versioned local machine bridge

Python and TypeScript invoke the hidden `a3s-box sdk-bridge` command with one
structured request and response. They never parse human CLI output. The bridge
returns a protocol version, stable machine error codes, typed response data,
and an exact operation inventory through `client.capabilities()`.

The SDK rejects an incompatible protocol version. Build or install the runtime
and language packages from the same release. Set `A3S_BOX_BINARY` only when the
matching `a3s-box` executable is not on `PATH`; normal local use needs no
additional service configuration.

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
CLI / Rust SDK / local machine bridge / CRI / containerd shim
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

The Python and TypeScript SDKs reach `ExecutionManager` through the local
machine bridge instead of constructing CLI commands or invoking `crun` or
libkrun directly. Lifecycle ownership, isolation selection, feature rejection,
audit evidence, and cleanup therefore remain inside the runtime boundary.

Main components:

| Component | Responsibility |
| --- | --- |
| `src/core` | Shared execution policy, configuration, protocol types, state primitives, events, logs, and errors |
| `src/runtime` | Canonical execution manager, MicroVM/Sandbox backends, OCI images/builds, storage, networking, pools, snapshots, and TEE clients |
| `src/cli` | Docker-like `a3s-box` command line |
| `src/shim` | libkrun bridge process and platform-specific host integration |
| `src/guest/init` | Guest PID 1, exec, PTY, filesystem, and attestation services |
| `src/netproxy` | macOS user-space bridge, DNS, inbound TCP, and outbound TCP |
| `src/cri` | Kubernetes CRI server |
| `containerd-shim` | containerd runtime-v2 adapter for RuntimeClass |
| `src/sdk` | Direct runtime-backed Rust SDK, native local Sandbox facade, fluent builders, and optional pipeline runner |
| `src/lambda` | Workload-execution integration retained for higher-level runtimes |
| `sdk/python`, `sdk/typescript` | Native local A3S language SDKs using the versioned machine bridge |

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
```

Python and TypeScript SDK checks run in their own package directories:

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
- [`scripts/local-sdk-smoke.sh`](scripts/local-sdk-smoke.sh) for the
  zero-credential Rust/Python/TypeScript MicroVM and Sandbox API matrix;
- [Cross-Capability Soak Test Plan](docs/soak-test-plan.md) for the capability,
  platform, duration, fault, evidence, and promotion matrix;
- [Production Cluster Tests](docs/production-cluster-tests.md) for enrolled
  RuntimeClass nodes and soak evidence.

Do not infer a production claim from unit tests, fixture servers, or simulated
TEE results. Record the host, backend, image digest, runtime version, and
evidence bundle for every real-runtime release gate.

## License

MIT. Third-party sources, generated fixtures, and language packages retain the
license metadata shipped in their respective directories.
