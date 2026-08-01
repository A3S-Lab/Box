<p align="center">
  <img src="assets/readme/hero.svg" width="100%" alt="A3S Box resolves OCI workloads to a dedicated-kernel MicroVM by default or an explicitly selected shared-kernel Sandbox">
</p>

<p align="center">
  <strong>A Docker-like local OCI runtime with hardware-backed isolation as the default contract.</strong>
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
  <a href="#run-your-first-box">Quick start</a> ·
  <a href="#local-runtime-boundary">Local boundary</a> ·
  <a href="#isolation-is-part-of-the-request">Isolation</a> ·
  <a href="#native-sdks">SDKs</a> ·
  <a href="#platform-boundaries">Platforms</a> ·
  <a href="#architecture">Architecture</a> ·
  <a href="#development">Development</a>
</p>

---

**A3S Box** runs Linux OCI workloads through a Docker-like CLI and typed local
SDKs. Every workload enters one of two explicit execution classes:

- a dedicated-kernel [libkrun](https://github.com/containers/libkrun) MicroVM
  by default; or
- a shared-kernel [A3S OCI Runtime](https://github.com/A3S-Lab/OCI-Runtime)
  Sandbox only when a caller requests `--isolation sandbox` and the Linux host
  passes the full capability probe. A3S OCI Runtime is the sole Sandbox
  implementation; Box no longer discovers, packages, or invokes an external
  OCI runtime binary.

Box never silently falls back to the lower-isolation backend. The requested
isolation class, effective backend, policy, and controls are persisted with the
workload so lifecycle recovery cannot reinterpret the request.

## Local runtime boundary

A3S Box is a local runtime, not a hosted Sandbox service. The repository ships
the CLI, native local SDKs, CRI/containerd adapters, and the host/guest runtime
components required to execute workloads on the machine where Box is
installed. It does not ship a remote Sandbox API, gateway, account service,
connection adapter, or dedicated service image.

Every public lifecycle request terminates at the same local
`ExecutionManager`. Images, networks, volumes, snapshots, logs, generation
fencing, policy checks, and cleanup therefore have one owner and one durable
state model. Applications that need remote orchestration should place their own
authenticated service in front of the native SDK instead of treating Box as a
network control plane.

## Run your first box

Install the current stable runtime on Linux or macOS:

```bash
curl --proto '=https' --tlsv1.2 -fsSL \
  https://raw.githubusercontent.com/A3S-Lab/Box/main/install.sh | sh
```

On Windows x86_64, use PowerShell:

```powershell
irm https://raw.githubusercontent.com/A3S-Lab/Box/main/install.ps1 | iex
```

Open a new terminal if needed, then verify the host:

```text
a3s-box --version
a3s-box info
```

The installers detect the platform, verify the release SHA-256 before
extraction, and refuse unsupported architectures or unsafe replacement
targets. See [Installation](docs/installation.md) for pinned versions, custom
destinations, offline packages, Homebrew, PATH behavior, and uninstall steps.

Run one command inside a MicroVM. Omitting `--isolation` selects the default
hardware-backed path:

```bash
a3s-box run --rm alpine:3.20 -- sh -lc 'echo "inside $(uname -s)"; uname -r'
```

Keep a service running, inspect it, and clean it up with familiar lifecycle
commands:

```bash
a3s-box run -d --name web --memory 1g -p 8080:80 nginx:alpine
a3s-box ps
a3s-box logs -f web
a3s-box exec web -- nginx -v
a3s-box stop web
a3s-box rm web
```

On a certified Linux host, explicitly request the shared-kernel preview:

```bash
a3s-box run --rm \
  --isolation sandbox \
  --cpus 2 \
  --memory 512m \
  alpine:3.20 -- sh -lc 'id; cat /proc/self/status'
```

An explicit `--isolation microvm` value is intentionally rejected: omission is
the only public way to select the default. This prevents scripts from treating
backend names as interchangeable compatibility modes.

Windows users must still enable and validate WHPX by following the
[WHPX setup guide](docs/windows-whpx.md). Source builds use `just release`; see
the host requirements below before running real workloads.

## Isolation is part of the request

| Contract | Default MicroVM | Explicit Sandbox |
| --- | --- | --- |
| Runtime backend | libkrun | A3S OCI Runtime |
| Kernel boundary | Dedicated guest Linux kernel | Shared host Linux kernel |
| Isolation class | `hardware-vm` | `shared-kernel` |
| Intended workload | Untrusted workloads and stronger tenant boundaries | Trusted or semi-trusted tools, benchmarks, and automation |
| Required host | Linux/KVM, Apple Silicon/HVF, or Windows/WHPX | Certified Linux host with namespaces, seccomp, subordinate IDs, and delegated cgroup v2 |
| Bridge networking and published ports | Supported within platform limits | CLI static publication is rejected; declared Runtime Services receive generation-fenced node-local TCP endpoints |
| TEE, warm pool, and snapshot-fork | Available on qualifying MicroVM hosts | Rejected |
| Automatic fallback | Never | Never |

These are the complete execution-isolation choices in the current public
contract, and both have real-runtime coverage. Network policy values such as
`none`, `strict`, and `custom` are not additional execution classes:
`strict` and `custom` remain unsupported admission modes and are covered by
negative tests. TEE is a host-specific capability of a qualifying MicroVM, not
a third execution backend.

Sandbox resource control also has one owner. `linux.resources` contains the
exact workload CPU, memory, and PID contract; A3S OCI Runtime derives a bounded
outer control-plane envelope, owns the fixed `a3s-control` and `a3s-workload`
cgroups, and performs live updates and cleanup across both levels. Guest Init
joins workload processes only through runtime-provided descriptors while the
Sandbox cgroup filesystem remains read-only.

> [!IMPORTANT]
> A shared-kernel Sandbox does not defend against a working host-kernel exploit,
> a hostile host administrator, hardware side channels, or data deliberately
> exposed through a bind mount. Use the default MicroVM boundary when those
> risks matter.

The complete admission and threat model is documented in
[Host Sandbox Backend Design](docs/host-sandbox-backend-design.md).

## What Box manages

Box is Docker-like, not Docker-identical. Unsupported options fail before
runtime state changes instead of being silently stored or weakened.

- **Lifecycle and execution** — run, create, start, stop, restart, pause, wait,
  remove, inspect, exec, PTY, attach, structured logs, health checks, and
  restart policies.
- **OCI images and builds** — bounded resumable pulls, concurrent verified
  layers, registry credentials, optional cosign verification, save/load,
  multi-stage builds, selected `RUN --mount` forms, and content-addressed
  caching. Closed A3S ACL build plans add canonical identities, source-root
  path confinement, an enforced network-none Linux `RUN` policy, and
  plan-bound typed OCI descriptors with durable image-layout paths over the
  same native build engine. The sole recorded-plan boundary persists immutable
  operation, source, and plan identity in one ImageStore-derived journal and
  exposes typed start, nonblocking inspect, cancel, exact replay, and terminal
  cleanup. Content-addressed plans also export one portable Box-native OCI
  cache artifact from the existing `BuildCache`; the cache key binds source,
  plan, platform, and cache-schema semantics. The operation record persists
  the admitted cache policy, and replay validates the exact cache manifest,
  config, layers, blob inventory, and byte count.
  `hydrate_recorded_build_cache` revalidates that same typed artifact and
  imports it through the existing cache lock and blob/key publication
  boundary. Hydration is idempotent, rejects a valid local key conflict before
  publishing imported keys, copies rather than links layer data, and preserves
  the imported layer set while enforcing the configured cache cap. A
  crash-released execution lease distinguishes live work from a dead caller
  without becoming another state store. Every supervised build uses a
  hash-derived operation workspace below that journal; cancellation, failure,
  and caller-death recovery fence the current Linux `RUN` process tree before
  reclaiming it. The existing journal lock orders cache publication,
  cancellation, and ImageStore publication, so restarted callers can adopt
  both artifacts committed before the terminal receipt. The journal, native
  engine, `BuildCache`, and ImageStore remain the only lifecycle, execution,
  cache, and image authorities. Recorded single-platform receipts can be
  combined with `assemble_recorded_build_outputs` into one deterministic OCI
  image index only when their source and non-platform plan intent match.
  Assembly sorts unique platforms, deduplicates shared blobs, and reuses the
  same output validator and sole ImageStore commit boundary; it owns no
  scheduler, queue, journal, manifest store, or publisher. The CLI exposes only
  this native engine: Linux `RUN` uses the isolated local path and macOS uses
  the same engine through `--run-pool`; publication remains the separate
  `a3s-box push` image operation.
- **Storage** — bind mounts, named volumes, tmpfs, file copy, diff, export,
  commit, filesystem snapshots, copy-on-write restore, and Box-owned read-only
  aliases for caller-provided Artifact trees below private provider roots.
- **Networking and Compose** — TSI, named bridge networks, peer discovery, TCP
  publishing, generation-fenced Sandbox loopback forwarding, and an explicit
  Compose subset. ACL is the canonical project format; YAML is a bounded
  compatibility input.
- **Startup acceleration** — rootfs and layer caches, pre-booted warm pools,
  build leases, one-shot pool routing, and opt-in Linux/KVM snapshot-fork.
- **Security and operations** — resource and syscall controls, audit evidence,
  stats, events, Prometheus metrics, health monitoring, SEV-SNP-oriented TEE
  workflows, sealing, TEE secret injection, and caller-authorized transient
  Runtime Secret materialization.

A few common workflows:

```bash
# Images and builds
a3s-box pull alpine:3.20
a3s-box build -t local/app:dev .

# Durable storage and a stopped-filesystem snapshot
a3s-box volume create data
a3s-box run -d --name app -v data:/data alpine:3.20 -- sleep 3600
a3s-box stop app
a3s-box snapshot create app --name checkpoint-1

# Named networking
a3s-box network create backend --subnet 10.89.0.0/24
a3s-box run -d --name api --network backend -p 8080:80 local/app:dev

# Explicit host-loopback access to a Sandbox workload
a3s-box run -d --name sandbox-api --isolation sandbox local/app:dev
a3s-box port-forward sandbox-api --host-port 18080 --guest-port 8080

# Deterministic Compose normalization and lifecycle
a3s-box compose -f compose.acl config
a3s-box compose -f compose.acl up -d
a3s-box compose -f compose.acl down
```

<details>
<summary><strong>CLI command groups</strong></summary>

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

`a3s-box system-prune --all` also removes rootfs cache entries that no active
Box references and interrupted image-pull work directories whose owner process
has exited. It preserves active rootfs markers, live pulls, concurrent
publication staging paths, and cache lock files.

## Native SDKs

Rust, Python, TypeScript, and Go operate the same local images, Sandboxes, volumes,
networks, snapshots, logs, and runtime state as the CLI. These packages expose
no remote connection configuration: they require the installed runtime and do
not read an endpoint, domain, or API key.

| Language | Package | Runtime access |
| --- | --- | --- |
| Rust | [`a3s-box-sdk`](https://crates.io/crates/a3s-box-sdk) | Direct typed calls into the runtime and generation-fenced execution manager |
| Python | [`a3s-box`](https://pypi.org/project/a3s-box/) | Sync and async APIs over the installed versioned machine bridge |
| TypeScript | [`@a3s-lab/box`](https://www.npmjs.com/package/@a3s-lab/box) | Promise APIs over the installed versioned machine bridge; Node.js 20+ |
| Go | [`github.com/A3S-Lab/Box/sdk/go/v3`](https://pkg.go.dev/github.com/A3S-Lab/Box/sdk/go/v3) | Context-aware, concurrency-safe APIs over the installed versioned machine bridge; Go 1.25+ |

```bash
cargo add a3s-box-sdk
python -m pip install a3s-box
npm install @a3s-lab/box
go get github.com/A3S-Lab/Box/sdk/go/v3
```

The high-level `Sandbox`, `commands`, and `files` namespaces are intentionally
small:

```python
from a3s_box import Sandbox

with Sandbox.create("python:3.12-alpine") as sandbox:
    result = sandbox.commands.run("python -c 'print(6 * 7)'")
    print(result.stdout)
    sandbox.files.write("/workspace/note.txt", "hello")
```

The same clients expose fluent programmable CI/CD builders without adding a
second workflow engine:

```go
client, err := box.NewClient(ctx)
if err != nil { return err }

image, err := client.Image("./ci").Tag("local/ci:latest").Build(ctx)
if err != nil { return err }

cache, err := client.Volume("go-cache").Create(ctx)
if err != nil { return err }

sandbox, err := client.Sandbox(image.Reference).
    CPUs(4).
    MemoryMiB(4096).
    Mount(box.NamedVolume(cache.Name, "/go/pkg/mod")).
    Start(ctx)
if err != nil { return err }
defer sandbox.Close(context.Background())

result, err := sandbox.Script("go test ./...\n").Env("CI", "true").Run(ctx)
if err != nil { return err }
if result.ExitCode != 0 { return errors.New(result.StderrString()) }
```

Python, TypeScript, and Go never parse human CLI output; they exchange one
checked, structured request and response with `a3s-box sdk-bridge`. Protocol v2
validates the exact 48-operation inventory before normal calls and fails closed
on missing, duplicate, or malformed capabilities and typed results. Sandbox
handles retain the effective `microvm` or `sandbox` isolation reported by the
runtime, and stale generations return the stable `conflict` code. Use runtime
and SDK packages from the same release because incompatible protocol versions
are rejected before mutation.

Read the [cross-language SDK contract](docs/sdk-api-and-programmable-cicd.md)
or go directly to the [Rust](src/sdk/README.md),
[Python](sdk/python/README.md),
[TypeScript](sdk/typescript/README.md), and
[Go](sdk/go/README.md) package guides.

## Platform boundaries

| Path | Status | Host and current boundary |
| --- | --- | --- |
| Linux MicroVM | Primary local runtime; conditional real-host gate | KVM and libkrun; the self-hosted KVM job must be armed explicitly and hosted CI does not prove a real boot |
| macOS MicroVM | Implemented and build-checked | Apple Silicon and Hypervisor.framework; real HVF host validation is still required, and Intel macOS is unsupported |
| Windows MicroVM | Implemented and real-host soak validated | x86_64 WHPX; currently one vCPU, with no interactive PTY, bridge networking, TEE, snapshot-fork, or CRI |
| Linux Sandbox | Preview and real-runtime CI validated | Explicit `--isolation sandbox` through the packaged A3S OCI Runtime; shares the host kernel and rejects VM-only features |
| Kubernetes | Preview | CRI v1 server plus containerd runtime-v2 shim and opt-in `runtimeClassName: a3s-box`; complete CRI conformance is not claimed |
| TEE | Host-specific | SEV-SNP-oriented attestation, RA-TLS, sealing, secret injection, and development-only simulation; TDX is not productized |

### What the validation covers

| Execution path | Real-runtime evidence | Remaining boundary |
| --- | --- | --- |
| Default MicroVM on Windows/WHPX | [`scripts/windows-whpx-soak.ps1`](scripts/windows-whpx-soak.ps1) covers lifecycle and foreground exit, post-boot non-interactive exec, bidirectional single-file copy, `top`, guest PID-aware stats, published-port networking, read-only bind mounts, named volumes, volume-backed initialization success and failure, metadata-preserving commit, filesystem commit/snapshot restore, and repeated virtio-fs traversal. The current qualification completed all 12 cases and returned the start and final runtime inventories to zero. | This proves the tested x86_64 Windows/WHPX host and workload matrix, not KVM, HVF, or TEE hardware. |
| Explicit Linux Sandbox | The required `SDK Local Sandbox (A3S OCI Runtime)` CI job runs the pinned runtime's native Linux network, storage, and initialization profiles; certifies every advertised R17 Base, Recovery, Networking, Mounts, Health, Resources, Logs, Exec, Security, and Outputs profile, including private read-only Artifact attachment without weakening its provider root, bounded digest-bound Task outputs, transient Secret nondisclosure, restart recovery, log reauthorization and redaction, and cleanup; then exercises the Rust, Python, TypeScript, and Go local SDKs and verifies process/cgroup cleanup. | Sandbox remains a shared-kernel preview and intentionally rejects VM-only features. |
| Linux/KVM MicroVM | A self-hosted real-KVM workflow covers the core lifecycle, SDK, CRI, leak, race, snapshot-fork, and soak paths when `KVM_CI=true`. | The job is conditionally skipped without the enrolled runner; a green hosted build alone is not real-KVM evidence. |
| macOS/HVF MicroVM | Hosted macOS arm64 compilation checks the supported target. | A real Apple Silicon/HVF boot and soak are separate release evidence. |
| SEV-SNP-oriented TEE | Unit and simulation tests cover application flow and protocol behavior. | No hardware security claim is made without a qualifying SEV-SNP host and attestation evidence. |

An implemented API is not a production guarantee for every host or threat
model. Unit tests, fixture servers, and simulated TEE results are not real-host
evidence. Review [Host Integration](docs/host-integration.md),
[Cross-Capability Soak Tests](docs/soak-test-plan.md), and
[CRI Conformance](docs/cri-conformance.md) before promoting a deployment.

## Integrations

- **A3S Runtime provider** — Maps digest-pinned Tasks and Services onto the
  A3S OCI shared-kernel Sandbox with generation fencing, recovery, structured
  logs, bounded idempotent exec, resource controls, tmpfs, exact node-local TCP
  endpoints, and HTTP, TCP, or command health probes for Services. Endpoint
  listeners and probes are bound to the Box execution generation and reuse the
  canonical Sandbox port connector or exec session boundary. Runtime health is
  sampled directly into the current observation and is never copied into the
  CLI monitor policy, so Box creates no second lifecycle store, durable endpoint
  registry, health registry, or health worker for the same Service. The artifact
  contract accepts OCI image manifests and multi-platform OCI image indexes; it
  does not advertise a Docker manifest media type. Caller-owned read-only
  Artifact directories are descriptor-pinned behind one Box-owned mount alias,
  so a private provider parent stays private and no Artifact is copied, chowned,
  or chmodded. The alias is removed on failed boot, stop, removal, and crash
  recovery. A caller can compose this same driver with one
  `BoxSecretMaterializer`; Box then advertises
  `SecretReferences`, resolves shared Runtime references through that caller,
  and mounts environment or file material read-only from a pre-mounted private
  Linux tmpfs. Secret bytes never enter durable Box state, creation intent, or
  OCI configuration. Recovery validates the existing generation without
  rematerializing it, stop retains restartable material, and remove or stale
  generation retirement cleans it. Log reads reauthorize every reference and
  redact exact workload material before cursor construction. A registry target
  is resolved only for an uncached pull, handed to the image boundary in memory,
  and zeroized after use; cached images do not request it. The Runtime driver
  passes explicit anonymous auth when no registry target is present, so it never
  falls back to a Box-local or process-environment credential source.
- **Kubernetes RuntimeClass** — The CRI server and containerd shim let selected
  Linux/KVM pods use `runtimeClassName: a3s-box`; installers and soak manifests
  live under [`deploy/`](deploy/).
- **Confidential workloads** — Qualifying MicroVM hosts can use attestation,
  RA-TLS, sealed data, and secret injection. Simulation tests application flow
  only and provides no hardware security.
- **Coding agents** — The first-party
  [A3S Box Skill](integrations/skills/a3s-box/SKILL.md) teaches supported agents
  CLI lifecycle, snapshots, warm pools, networking boundaries, and recovery.
  Its installer supports project or user scope across A3S Code, Codex, Claude
  Code, and the shared `.agents/skills` convention, including remote streamed
  installation with a durable local copy.

## Architecture

Every shipped entry point submits the same backend-neutral local execution
request:

```text
CLI · Rust SDK · Python · TypeScript · Go · CRI · containerd shim
                              │
                      ExecutionManager
           durable state · generation fencing
                              │
              capability probe + policy resolver
                    ┌─────────┴─────────┐
                    │                   │
           default MicroVM     explicit Sandbox
              libkrun              a3s-oci
           guest kernel          host kernel
                    └─────────┬─────────┘
                              │
         images · storage · networks · snapshots
              logs · audit · metrics · TEE
```

The runtime persists caller policy before allocation. Python, TypeScript, and Go
reach the same `ExecutionManager` through the machine bridge instead of
constructing CLI commands; CRI and RuntimeClass adapters also reuse the same
resolver. Lifecycle ownership, unsupported-feature rejection, audit evidence,
and cleanup therefore stay inside one runtime boundary. No separate HTTP
control plane or remote lifecycle service sits beside this path.

This diagram describes the current migration baseline. The target architecture
routes both `microvm` and `sandbox` isolation through the public A3S OCI Runtime
SDK, leaving Box as the product, image, storage, network, and policy owner. The
phased cutover and removal gates are defined in [ROADMAP.md](ROADMAP.md).

Repository components are grouped by responsibility:

- `src/core` — execution policy, protocol types, state, events, logs, and
  errors;
- `src/runtime` — canonical manager, backends, images, builds, storage,
  networking, pools, snapshots, and TEE;
- `src/cli`, `src/sdk`, `src/cri`, `containerd-shim` — public entry points and
  cluster adapters;
- `src/shim`, `src/guest/init`, `src/netproxy` — host/guest control and
  platform integration;
- `sdk/python`, `sdk/typescript`, `sdk/go` — native local language packages over the
  checked machine bridge.

## Documentation

- [Product and OCI Runtime migration roadmap](ROADMAP.md)
- [Host integration and real-runtime validation](docs/host-integration.md)
- [Cross-capability soak test plan](docs/soak-test-plan.md)
- [Shared-kernel Sandbox threat model](docs/host-sandbox-backend-design.md)
- [Windows WHPX support](docs/windows-whpx.md)
- [SDK API and programmable CI/CD](docs/sdk-api-and-programmable-cicd.md)
- [Compose normalization](docs/compose-normalization.md)
- [Copy-on-write snapshot-fork design](docs/cow-snapshot-fork-design.md)
- [Kubernetes CRI conformance](docs/cri-conformance.md)
- [Monitor service](docs/monitor-service.md)
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

Language packages have their own test suites:

```bash
cd sdk/python
python -m pip install -e .
python -m unittest discover -s tests

cd ../typescript
npm ci
npm run build
npm test

cd ../go
gofmt -w .
go vet ./...
go test -race ./...
```

Host-backed MicroVM, Sandbox, networking, build, CRI, and endurance tests need
an explicitly prepared machine and isolated runtime state. Use
[`scripts/host-integration-smoke.sh`](scripts/host-integration-smoke.sh) and
[`scripts/local-sdk-smoke.sh`](scripts/local-sdk-smoke.sh), and record the host,
backend, image digest, runtime version, and evidence bundle for every release
gate.

## License

The A3S Box runtime is available under the [MIT License](LICENSE). Individual
SDK packages, vendored sources, generated fixtures, and release artifacts
retain the license metadata shipped with their directories or archives.
