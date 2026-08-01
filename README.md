<p align="center">
  <img src="assets/readme/hero.svg" width="100%" alt="A3S Box resolves a local OCI workload to its requested MicroVM or Sandbox isolation boundary">
</p>

<p align="center">
  <strong>The local product plane for Linux OCI workloads: Docker-like workflows, typed SDKs, and isolation that never changes behind your back.</strong>
</p>

<p align="center">
  <a href="https://github.com/A3S-Lab/Box/actions/workflows/ci.yml"><img alt="CI status" src="https://img.shields.io/github/actions/workflow/status/A3S-Lab/Box/ci.yml?branch=main&amp;style=flat-square&amp;label=CI"></a>
  <a href="https://github.com/A3S-Lab/Box/releases/latest"><img alt="Latest A3S Box release" src="https://img.shields.io/github/v/release/A3S-Lab/Box?display_name=tag&amp;sort=semver&amp;style=flat-square&amp;color=62d78b"></a>
  <a href="https://pypi.org/project/a3s-box/"><img alt="A3S Box Python package" src="https://img.shields.io/pypi/v/a3s-box?style=flat-square&amp;color=3775a9"></a>
  <a href="https://www.npmjs.com/package/@a3s-lab/box"><img alt="A3S Box TypeScript package" src="https://img.shields.io/npm/v/@a3s-lab/box?style=flat-square&amp;color=cb3837"></a>
  <a href="https://pkg.go.dev/github.com/A3S-Lab/Box/sdk/go/v3"><img alt="A3S Box Go package" src="https://pkg.go.dev/badge/github.com/A3S-Lab/Box/sdk/go/v3.svg"></a>
  <a href="LICENSE"><img alt="MIT License" src="https://img.shields.io/badge/license-MIT-f5b95f?style=flat-square"></a>
</p>

<p align="center">
  <a href="#start-with-one-workload">Start</a> ·
  <a href="#choose-the-boundary-intentionally">Isolation</a> ·
  <a href="#what-box-owns">Capabilities</a> ·
  <a href="#one-state-model-four-native-sdks">SDKs</a> ·
  <a href="#platform-status">Platforms</a> ·
  <a href="#architecture-current-and-target">Architecture</a> ·
  <a href="#development">Development</a>
</p>

---

**A3S Box** turns an image, a command, and product policy into a
lifecycle-managed workload on the local machine. It owns the developer
experience and product resources: images, builds, networks, volumes,
snapshots, health, restart policy, logs, and cleanup.

The current 3.2 execution model has two explicit paths:

- omitting `--isolation` selects a dedicated-kernel MicroVM managed by Box
  through libkrun;
- `--isolation sandbox` selects the shared-host-kernel path on a qualified
  Linux host, with lifecycle execution delegated through the pinned
  [A3S OCI Runtime](https://github.com/A3S-Lab/OCI-Runtime) SDK.

There is no silent fallback between them. The request, resolved backend, and
policy are persisted so restart recovery cannot reinterpret the workload.

> [!NOTE]
> The architecture is actively converging on one public
> `a3s-oci-sdk` boundary for both isolation classes. A reusable SDK-only
> lifecycle adapter and explicitly constructed local backend now exist. That
> boundary negotiates `a3s.oci.attachments.v1`, submits the bundle rootfs,
> mounts, networking, process I/O, secret classifications, and optional
> extensions as one validated manifest, and persists its exact digest for
> recovery. That exact-generation boundary now carries memory-retaining
> pause/resume and Box process sessions: captured and streaming exec, stdin,
> cursor-checked output, signals and wait, PTY resize, exact exit status, and
> bounded timeout cleanup. One-shot request IDs replay to the same process
> after backend recreation, while raw runtime output remains separate from
> structured Box logs. Legacy VM records keep their socket sessions; OCI-bound
> records use the SDK transport on Unix and Windows. Process inventory,
> resource update, stats, events, Box file sessions, production MicroVM routing,
> and real-host cutover gates are not complete; the current split above remains
> authoritative.
> Follow the checked gates in the [migration roadmap](ROADMAP.md).

## Start with one workload

Install the stable release on Linux or macOS:

```bash
curl --proto '=https' --tlsv1.2 -fsSL \
  https://raw.githubusercontent.com/A3S-Lab/Box/main/install.sh | sh
```

On Windows x86_64, use PowerShell:

```powershell
irm https://raw.githubusercontent.com/A3S-Lab/Box/main/install.ps1 | iex
```

Open a new terminal if needed, then inspect the exact host capability before
launch:

```text
a3s-box --version
a3s-box info
```

Run a disposable Alpine workload. The omitted isolation flag is intentional:

```bash
a3s-box run --rm alpine:3.20 -- sh -lc 'echo "inside $(uname -s)"; uname -r'
```

Then exercise the familiar long-running lifecycle:

```bash
a3s-box run -d --name web --memory 1g -p 8080:80 nginx:alpine
a3s-box ps
a3s-box logs -f web
a3s-box exec web -- nginx -v
a3s-box stop web
a3s-box rm web
```

The installers verify release SHA-256 values before extraction and reject
unsupported architectures or unsafe replacement targets. Pinned versions,
offline packages, Homebrew, PATH behavior, and uninstall steps live in
[Installation](docs/installation.md).

## Choose the boundary intentionally

| Contract | Default MicroVM | Explicit Sandbox |
| --- | --- | --- |
| Request | omit `--isolation` | `--isolation sandbox` |
| Effective isolation | `hardware-vm` | `shared-kernel` |
| Current execution owner | Box → libkrun | Box → `a3s-oci-sdk` → native Linux service |
| Kernel boundary | Dedicated guest Linux kernel | Shared host Linux kernel |
| Qualified hosts | Linux/KVM, Apple Silicon/HVF, Windows x86_64/WHPX within the platform gates below | Certified Linux x86_64/aarch64 host |
| Best fit | Untrusted workloads and stronger tenant boundaries | Trusted or semi-trusted tools, benchmarks, and automation |
| VM-only features | TEE, warm pool, snapshot-fork where qualified | Rejected |
| Fallback | Never | Never |

An explicit `--isolation microvm` spelling is rejected. Omission is the only
public way to select the default, which prevents scripts from treating backend
names as interchangeable compatibility modes.

On a certified Linux host, request the shared-kernel preview explicitly:

```bash
a3s-box run --rm \
  --isolation sandbox \
  --cpus 2 \
  --memory 512m \
  alpine:3.20 -- sh -lc 'id; cat /proc/self/status'
```

> [!IMPORTANT]
> A shared-kernel Sandbox does not defend against a working host-kernel
> exploit, a hostile host administrator, hardware side channels, or data
> deliberately exposed through a bind mount. Use the default MicroVM boundary
> when those risks matter.

The complete admission rules and threat model are in
[Host Sandbox Backend Design](docs/host-sandbox-backend-design.md).

## What Box owns

Box is Docker-like, not Docker-identical. Unsupported controls fail before
runtime mutation instead of being stored and silently weakened.

| Product area | Current surface |
| --- | --- |
| Workloads | create, start, stop, restart, kill, pause, wait, inspect, exec, attach, PTY, health, and restart policy |
| Images and builds | pull, push, tag, save/load, verified layers, selected Dockerfile/Containerfile builds, content-addressed cache, and signed-image policy |
| Storage | bind mounts, named volumes, tmpfs, copy, diff, export, commit, filesystem snapshots, and copy-on-write restore |
| Networking and Compose | TSI, named bridges, peer discovery, TCP publication, generation-fenced Sandbox forwarding, and a bounded ACL/YAML Compose subset |
| Operations | structured logs, stats, events, audit evidence, metrics, monitoring, resource updates, and cleanup |
| Acceleration and security | rootfs/layer caches, warm pools, opt-in Linux/KVM snapshot-fork, and host-gated SEV-SNP-oriented workflows |

A few end-to-end workflows:

```bash
# Build and run
a3s-box pull alpine:3.20
a3s-box build -t local/app:dev .
a3s-box run -d --name app local/app:dev

# Durable data and a stopped-filesystem snapshot
a3s-box volume create data
a3s-box run -d --name data-app -v data:/data alpine:3.20 -- sleep 3600
a3s-box stop data-app
a3s-box snapshot create data-app --name checkpoint-1

# Named networking and deterministic Compose normalization
a3s-box network create backend --subnet 10.89.0.0/24
a3s-box compose -f compose.acl config
a3s-box compose -f compose.acl up -d
```

<details>
<summary><strong>CLI command map</strong></summary>

| Area | Commands |
| --- | --- |
| Lifecycle | `run`, `create`, `start`, `stop`, `restart`, `rm`, `kill`, `pause`, `unpause`, `wait`, `rename`, `prune` |
| Execution | `exec`, `shell`, `attach`, `top` |
| Images and builds | `pull`, `push`, `build`, `images`, `rmi`, `tag`, `image-inspect`, `history`, `image-prune`, `save`, `load`, `import` |
| Filesystems | `cp`, `diff`, `export`, `commit`, `volume`, `snapshot` |
| Networking | `network`, `port`, `port-forward`, `compose` |
| Security and TEE | `attest`, `seal`, `unseal`, `inject-secret` |
| Observability | `ps`, `logs`, `inspect`, `stats`, `events`, `df`, `audit`, `monitor` |
| System | `container-update`, `system-prune`, `pool`, `login`, `logout`, `version`, `info` |

</details>

## One state model, four native SDKs

Rust, Python, TypeScript, and Go operate the same local resources and durable
state as the CLI. They do not expose a remote endpoint, domain, or API-key
setting.

| Language | Install | Runtime access | Guide |
| --- | --- | --- | --- |
| Rust | `cargo add a3s-box-sdk` | Direct typed calls into the runtime and generation-fenced execution manager | [Rust SDK](src/sdk/README.md) |
| Python | `python -m pip install a3s-box` | Sync and async APIs over the installed machine bridge | [Python SDK](sdk/python/README.md) |
| TypeScript | `npm install @a3s-lab/box` | Promise APIs over the installed machine bridge; Node.js 20+ | [TypeScript SDK](sdk/typescript/README.md) |
| Go | `go get github.com/A3S-Lab/Box/sdk/go/v3` | Context-aware APIs over the installed machine bridge; Go 1.25+ | [Go SDK](sdk/go/README.md) |

Python, TypeScript, and Go exchange structured protocol-v3 messages with
`a3s-box sdk-bridge`; they never parse human CLI output. The exact
48-operation handshake fails closed on missing, duplicate, malformed, or
incompatible capabilities. See the
[cross-language SDK contract](docs/sdk-api-and-programmable-cicd.md).

## Platform status

| Path | Current evidence | Boundary that remains visible |
| --- | --- | --- |
| Linux MicroVM | Primary local path through KVM/libkrun; self-hosted lifecycle, SDK, CRI, race, leak, snapshot-fork, and soak gate when `KVM_CI=true` | Hosted CI cannot prove a real KVM boot when the enrolled runner is absent |
| macOS MicroVM | Apple Silicon/HVF build and packaging path | Real Apple Silicon/HVF release validation remains a separate host gate; Intel macOS is unsupported |
| Windows MicroVM | Real x86_64 WHPX soak covering lifecycle, exec, copy, stats, ports, bind/named volumes, commit, snapshots, and cleanup | One vCPU; no interactive PTY, bridge networking, TEE, snapshot-fork, or CRI |
| Linux Sandbox | Real A3S OCI Runtime CI profiles plus Rust, Python, TypeScript, and Go local SDK exercises | Shared-kernel preview; VM-only controls are rejected |
| Kubernetes | CRI v1 server and containerd runtime-v2 shim preview | Complete CRI conformance is not claimed |
| TEE | SEV-SNP-oriented application and protocol flows | Simulation is not hardware security evidence |

Real-host evidence is deliberately separate from unit, build-only, fixture, or
simulation results. Review [Host Integration](docs/host-integration.md),
[Cross-Capability Soak Tests](docs/soak-test-plan.md), and
[CRI Conformance](docs/cri-conformance.md) before promoting a deployment.

Windows hosts must also follow the [WHPX setup guide](docs/windows-whpx.md).

## Architecture: current and target

Every shipped entry point reaches one backend-neutral local
`ExecutionManager`:

```text
CLI · Rust · Python · TypeScript · Go · Compose · CRI · containerd shim
                                  │
                         ExecutionManager
                  desired state · generations · policy
                    ┌─────────────┴─────────────┐
                    │                           │
         images · builds · storage      isolation resolver
        networks · logs · health        ┌───────┴────────┐
                                       │                │
                           current MicroVM      current Sandbox
                           Box + libkrun        a3s-oci-sdk
                           dedicated kernel    shared host kernel
```

The target dependency direction removes the direct execution split:

```text
A3S Box product plane
        │  prepared OCI bundle + desired isolation
        ▼
a3s-oci-sdk over bounded local IPC
        ▼
A3S OCI Runtime host service
        ├── native Linux driver
        └── KVM / HVF / WHPX utility-VM drivers
```

Box remains the product, image, storage, network, health, and policy owner.
OCI Runtime becomes authoritative for actual process/VM state, raw process
I/O, operation replay, exact terminal status, driver selection, and runtime
cleanup. The adapter retains only the exact runtime identity, immutable
configuration and attachment digests, endpoint, driver, and isolation evidence
needed to detect recovery drift. Solid current behavior and the phased cutover
gates are kept separate in
[ROADMAP.md](ROADMAP.md); unfinished migration work is never presented as a
platform capability.

This repository is a local runtime, not a hosted Sandbox control plane. Teams
that need remote orchestration should put an authenticated service in front of
the native SDK rather than treating Box as a network API.

## Repository map

```text
src/core/          policy, protocol types, lifecycle state, logs, and errors
src/runtime/       execution manager, backends, images, storage, networks, pools
src/cli/           a3s-box command-line interface
src/sdk/           native Rust SDK and machine bridge
src/cri/           CRI v1 adapter
src/shim/          current host/guest MicroVM control
src/guest/init/    current guest init and execution service
sdk/               Python, TypeScript, and Go packages
containerd-shim/   RuntimeClass integration
```

## Documentation

- [Product and OCI Runtime migration roadmap](ROADMAP.md)
- [Installation and packaging](docs/installation.md)
- [Host integration and real-runtime validation](docs/host-integration.md)
- [Cross-capability soak plan](docs/soak-test-plan.md)
- [Shared-kernel Sandbox threat model](docs/host-sandbox-backend-design.md)
- [Windows WHPX support](docs/windows-whpx.md)
- [SDK API and programmable CI/CD](docs/sdk-api-and-programmable-cicd.md)
- [Compose normalization](docs/compose-normalization.md)
- [Copy-on-write snapshot-fork](docs/cow-snapshot-fork-design.md)
- [Kubernetes CRI conformance](docs/cri-conformance.md)
- [Changelog](CHANGELOG.md)

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

Language packages keep independent test suites:

```bash
cd sdk/python
python -m pip install -e .
python -m unittest discover -s tests

cd ../typescript
npm ci
npm run build
npm test

cd ../go
go vet ./...
go test -race ./...
```

Host-backed MicroVM, Sandbox, networking, build, CRI, and endurance tests need
an explicitly prepared machine and isolated runtime state. Use
[`scripts/host-integration-smoke.sh`](scripts/host-integration-smoke.sh) and
[`scripts/local-sdk-smoke.sh`](scripts/local-sdk-smoke.sh), and retain the
host, backend, image digest, runtime revision, and evidence bundle for release
gates.

## License

A3S Box is available under the [MIT License](LICENSE). Vendored sources,
generated fixtures, SDK packages, and release archives retain the license
metadata shipped with their directories or artifacts.
