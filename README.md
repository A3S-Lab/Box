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
  <a href="LICENSE"><img alt="MIT License" src="https://img.shields.io/badge/license-MIT-f5b95f?style=flat-square"></a>
</p>

<p align="center">
  <a href="#run-your-first-box">Quick start</a> ·
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

## Run your first box

Install the current stable runtime on macOS or Linux:

```bash
brew install a3s-lab/tap/a3s-box
a3s-box info
```

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

Windows users should install the matching x86_64 archive from the
[latest release](https://github.com/A3S-Lab/Box/releases/latest) and follow the
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
| Bridge networking and published ports | Supported within platform limits | Rejected in the current release |
| TEE, warm pool, and snapshot-fork | Available on qualifying MicroVM hosts | Rejected |
| Automatic fallback | Never | Never |

These are the complete execution-isolation choices in the current public
contract, and both have real-runtime coverage. Network policy values such as
`none`, `strict`, and `custom` are not additional execution classes:
`strict` and `custom` remain unsupported admission modes and are covered by
negative tests. TEE is a host-specific capability of a qualifying MicroVM, not
a third execution backend.

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
  caching.
- **Storage** — bind mounts, named volumes, tmpfs, file copy, diff, export,
  commit, filesystem snapshots, and copy-on-write restore.
- **Networking and Compose** — TSI, named bridge networks, peer discovery, TCP
  publishing, and an explicit Compose subset. ACL is the canonical project
  format; YAML is a bounded compatibility input.
- **Startup acceleration** — rootfs and layer caches, pre-booted warm pools,
  build leases, one-shot pool routing, and opt-in Linux/KVM snapshot-fork.
- **Security and operations** — resource and syscall controls, audit evidence,
  stats, events, Prometheus metrics, health monitoring, SEV-SNP-oriented TEE
  workflows, sealing, and secret injection.

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
| Networking | `network`, `port`, `compose` |
| Security and TEE | `attest`, `seal`, `unseal`, `inject-secret` |
| Observability | `ps`, `logs`, `inspect`, `stats`, `events`, `df`, `audit`, `monitor` |
| System | `container-update`, `system-prune`, `pool`, `login`, `logout`, `version`, `info` |

</details>

## Native SDKs

Rust, Python, and TypeScript operate the same local images, Sandboxes, volumes,
networks, snapshots, logs, and runtime state as the CLI. Local use requires no
endpoint, domain, or API key.

| Language | Package | Runtime access |
| --- | --- | --- |
| Rust | [`a3s-box-sdk`](https://crates.io/crates/a3s-box-sdk) | Direct typed calls into the runtime and generation-fenced execution manager |
| Python | [`a3s-box`](https://pypi.org/project/a3s-box/) | Sync and async APIs over the installed versioned machine bridge |
| TypeScript | [`@a3s-lab/box`](https://www.npmjs.com/package/@a3s-lab/box) | Promise APIs over the installed versioned machine bridge; Node.js 20+ |

```bash
cargo add a3s-box-sdk
python -m pip install a3s-box
npm install @a3s-lab/box
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

```typescript
import { A3SBoxClient } from '@a3s-lab/box'

const client = new A3SBoxClient()
const image = await client.image('./ci').tag('local/ci:latest').build()
const sandbox = await client
  .sandbox(image.reference)
  .cpus(4)
  .memoryMb(4096)
  .start()

try {
  const result = await sandbox
    .script('npm ci\nnpm test\n')
    .interpreter('/bin/sh', '-se')
    .env('CI', 'true')
    .run()
  if (result.exitCode !== 0) throw new Error(result.stderr)
} finally {
  await sandbox.kill()
}
```

Python and TypeScript never parse human CLI output; they exchange one checked,
structured request and response with `a3s-box sdk-bridge`. Use runtime and SDK
packages from the same release because the bridge rejects incompatible
protocol versions.

Read the [cross-language SDK contract](docs/sdk-api-and-programmable-cicd.md)
or go directly to the [Rust](src/sdk/README.md),
[Python](sdk/python/README.md), and
[TypeScript](sdk/typescript/README.md) package guides.

## Platform boundaries

| Path | Status | Host and current boundary |
| --- | --- | --- |
| Linux MicroVM | Primary local runtime; conditional real-host gate | KVM and libkrun; the self-hosted KVM job must be armed explicitly and hosted CI does not prove a real boot |
| macOS MicroVM | Implemented and build-checked | Apple Silicon and Hypervisor.framework; real HVF host validation is still required, and Intel macOS is unsupported |
| Windows MicroVM | Implemented and real-host soak validated | x86_64 WHPX; currently one vCPU, with no interactive exec, bridge networking, TEE, snapshot-fork, or CRI |
| Linux Sandbox | Preview and real-runtime CI validated | Explicit `--isolation sandbox` through the packaged A3S OCI Runtime; shares the host kernel and rejects VM-only features |
| Kubernetes | Preview | CRI v1 server plus containerd runtime-v2 shim and opt-in `runtimeClassName: a3s-box`; complete CRI conformance is not claimed |
| TEE | Host-specific | SEV-SNP-oriented attestation, RA-TLS, sealing, secret injection, and development-only simulation; TDX is not productized |

### What the validation covers

| Execution path | Real-runtime evidence | Remaining boundary |
| --- | --- | --- |
| Default MicroVM on Windows/WHPX | [`scripts/windows-whpx-soak.ps1`](scripts/windows-whpx-soak.ps1) covers lifecycle and foreground exit, published-port networking, read-only bind mounts, named volumes, volume-backed initialization success and failure, metadata-preserving commit, filesystem commit/snapshot restore, and repeated virtio-fs traversal. The current qualification completed all 12 cases and returned the start and final runtime inventories to zero. | This proves the tested x86_64 Windows/WHPX host and workload matrix, not KVM, HVF, or TEE hardware. |
| Explicit Linux Sandbox | The required `SDK Local Sandbox (A3S OCI Runtime)` CI job runs the pinned runtime's native Linux network, storage, and initialization profiles, then exercises the Rust, Python, and TypeScript local SDKs and verifies process cleanup. | Sandbox remains a shared-kernel preview and intentionally rejects VM-only features. |
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
  A3S OCI shared-kernel Sandbox with generation fencing, recovery,
  structured logs, bounded idempotent exec, resource controls, and tmpfs.
- **Kubernetes RuntimeClass** — The CRI server and containerd shim let selected
  Linux/KVM pods use `runtimeClassName: a3s-box`; installers and soak manifests
  live under [`deploy/`](deploy/).
- **Confidential workloads** — Qualifying MicroVM hosts can use attestation,
  RA-TLS, sealed data, and secret injection. Simulation tests application flow
  only and provides no hardware security.
- **Coding agents** — The first-party
  [A3S Box Skill](integrations/skills/a3s-box/SKILL.md) teaches supported agents
  CLI lifecycle, snapshots, warm pools, networking boundaries, and recovery.

## Architecture

Every entry point submits the same backend-neutral execution request:

```text
CLI · Rust SDK · local machine bridge · CRI · containerd shim
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

The runtime persists caller policy before allocation. Python and TypeScript
reach the same `ExecutionManager` through the machine bridge instead of
constructing CLI commands; CRI and RuntimeClass adapters also reuse the same
resolver. Lifecycle ownership, unsupported-feature rejection, audit evidence,
and cleanup therefore stay inside one runtime boundary.

Repository components are grouped by responsibility:

- `src/core` — execution policy, protocol types, state, events, logs, and
  errors;
- `src/runtime` — canonical manager, backends, images, builds, storage,
  networking, pools, snapshots, and TEE;
- `src/cli`, `src/sdk`, `src/cri`, `containerd-shim` — public entry points and
  cluster adapters;
- `src/shim`, `src/guest/init`, `src/netproxy` — host/guest control and
  platform integration;
- `sdk/python`, `sdk/typescript` — native local language packages over the
  checked machine bridge.

## Documentation

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
