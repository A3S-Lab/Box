# A3S Box

<p align="center">
  <strong>OCI Workload Runtime for MicroVMs and Sandboxes</strong>
</p>

<p align="center">
  <em>Run Linux OCI workloads in a hardware-backed MicroVM by default, or explicitly choose a low-overhead shared-kernel Sandbox on certified Linux hosts.</em>
</p>

---

## Overview

A3S Box is an OCI workload runtime with a Docker-like CLI and two explicit
execution backends. The default path boots each workload in its own
[libkrun](https://github.com/containers/libkrun) MicroVM. Linux operators can
instead request `--isolation sandbox` to run through certified
[crun](https://github.com/containers/crun) with namespaces, seccomp,
capabilities, `no_new_privs`, and cgroup v2.

The two modes are deliberately not presented as equivalent. A MicroVM has a
separate guest kernel and a hardware-virtualization boundary. A Sandbox shares
the host Linux kernel and is intended for agent tools, benchmarks, and
development automation whose threat model does not include a working kernel
exploit. Box never falls back from MicroVM to Sandbox when virtualization is
unavailable.

The local CLI and Rust SDK are the primary product surfaces. OCI Sandbox
execution, the E2B protocol service, Kubernetes integration, TEE workflows,
and Windows support have different maturity and host requirements; the status
table below states those boundaries explicitly.

## Current status

A3S Box is not a full Docker, containerd, or Kubernetes replacement. An
implemented API is also not automatically a production guarantee for every
host or threat model. Release claims require the host-backed gates documented
in [Host Integration](docs/host-integration.md), while cluster and CRI gaps
remain explicit in
[Production Cluster Tests](docs/production-cluster-tests.md) and
[CRI Conformance](docs/cri-conformance.md).

| Area | Status today |
| --- | --- |
| Local CLI runtime | Implemented for macOS Apple Silicon/HVF and Linux/KVM style hosts. Real macOS HVF core and host smoke suites have passed with Alpine pulled from the registry; offline archive runs remain the release-gate default. |
| OCI images | Pull, load, save, tag, inspect, history, remove, and local cache resolution are implemented. Push and cosign signing/verification paths exist and require registry access for end-to-end validation. |
| Dockerfile build | Honest subset. `FROM`, metadata instructions, `COPY`/`ADD`, and shell/exec-form `RUN` are implemented by the host engine on Linux. `--run-pool` can execute `RUN` through a leased warm-pool VM by mounting the mutable build rootfs into the guest. On macOS, auto `RUN` builds still delegate to BuildKit inside an A3S Linux VM (`--builder=buildkit-vm`) unless `--run-pool` is selected; unsafe host execution remains an explicit experiment-only escape hatch. |
| Lifecycle and exec | `run`, `create`, `start`, `stop`, `restart`, `rm`, `wait`, foreground/detached runs, non-PTY exec, PTY exec, logs, stats, and inspect are implemented. |
| OCI Sandbox | Linux-only, explicit `--isolation sandbox` shared-kernel execution through certified `crun`. Structured `json-file` logs preserve stdout/stderr identity for foreground, detached, natural-exit, stop, kill, and auto-remove paths. Generation-owned log workers are PID-start-time fenced, drained before archival, and recovered during cleanup. The security-negative matrix and performance gate remain release work; this mode does not claim MicroVM-equivalent isolation. |
| E2B protocol preview | The ACL-configured service covers durable lifecycle, memory-preserving pause/resume, v1/v2 running/paused listing and runtime-backed structured logs, monotonic refresh, current single/batch control metrics, owner-scoped Volume control/content and Sandbox mounts, TLS routing, terminal health, and runtime envd metrics/environment/HTTP file transfer. The immutable runtime-image gate drives pinned Python sync/async and TypeScript clients through Filesystem operations, foreground/background commands, stdin, PTY resize, pause/connect-resume with same-process survival, generation-fenced v1/v2 logs with cursor/direction/level/search/limit behavior, bidirectional Volume mounts with UID/GID mapping, and Code Interpreter execution/context lifecycle on real `crun` Sandboxes. Typed source packages are built but unpublished. Filesystem-only pause, historical metrics, sustained log-retention/rotation races, deeper Volume failure/recovery and concurrent-mutation cases, multi-file and large-file behavior, exhaustive Process/PTY, signed-file, public-port, rich interpreter, MCP, and full release matrices remain incomplete; `full_compatibility=false`. |
| Warm pool and snapshot-fork | A warm pool serves pre-booted sandboxes over a socket. Native snapshot-fork (Copy-on-Write microVM cloning) snapshots one booted template and restores many forks from it, each mapping the template RAM `MAP_PRIVATE`. Verified on `/dev/kvm`: ~4× faster than a cold boot per fork, 100 forks in under ~1 s (~8 ms amortized each). Requires `/dev/kvm`; opt in with `pool start --snapshot-fork` or the `KRUN_SNAPSHOT_*` / `KRUN_RESTORE_FROM` env. |
| Networking | Default TSI networking, TCP `host:guest` publishing, user-defined bridge networks, network inspect/connect/disconnect/rm, and `/etc/hosts` peer discovery are implemented with documented platform boundaries. |
| Compose | Canonical `compose.acl` applications and an explicit Docker Compose-compatible YAML subset are implemented, including convergent `up`, project-scoped lifecycle commands, dependency conditions, health checks, networks, volumes, ports, and runtime/security settings. |
| TEE | AMD SEV-SNP-oriented attestation, RA-TLS, sealing, and secret injection flows exist, plus simulation mode for development. Hardware-backed operation depends on SEV-SNP-capable hosts and libkrun support. TDX is not a productized path. |
| Kubernetes CRI | Reachable by `crictl`/kubelet over its Unix socket. Verified on a `/dev/kvm` host: pod + container lifecycle (`RunPodSandbox` → `CreateContainer` → `StartContainer` → `Stop`/`Remove`), `exec` over Kubernetes SPDY/3.1 `remotecommand` (TTY and non-TTY, stdin/stdout/stderr, exit codes), and container log capture to `log_path`. Not yet conformant: `attach` and the stricter `critest` specs (log format, Linux SecurityContext, seccomp/AppArmor, namespaces, mount propagation). Linux-only; not the core completion target. **RuntimeClass:** a one-command per-node installer (`deploy/scripts/install-runtimeclass.sh`) registers the `io.containerd.a3s-box.v2` runtime, and `runtimeClassName: a3s-box` is validated end-to-end (pod start + `kubectl exec`) across a 5-node cluster — see [Deploy as a Kubernetes RuntimeClass](#deploy-as-a-kubernetes-runtimeclass). |
| Windows | Native x86_64 WHPX/libkrun code paths exist and do not require WSL. Windows remains a host-specific integration surface; standard release automation currently focuses on Linux and macOS, and Windows CRI is out of scope. |

## Isolation model

A3S Box takes a Linux OCI image and resolves it to either the default MicroVM
backend or the explicitly selected shared-kernel Sandbox backend. Backend
selection is deterministic, persisted with managed executions, and never
silently falls back. Use the default MicroVM backend when a separate guest
kernel or hardware virtualization boundary is required.

| Property | Default MicroVM | `--isolation sandbox` |
| --- | --- | --- |
| Runtime | libkrun | Certified `crun` |
| Isolation class | Hardware VM with a dedicated guest kernel | Shared host kernel |
| Intended workload | Stronger tenant boundaries and untrusted workloads | Trusted or semi-trusted tools, benchmarks, and automation |
| TEE, warm pool, snapshot-fork | Supported on qualifying hosts | Rejected |
| Automatic fallback | Never | Never |

A3S Box is not:

- a full Docker daemon;
- a general-purpose Kubernetes runtime with all CRI edge cases completed;
- a full Dockerfile/buildx implementation;
- a network policy engine yet;
- a TEE guarantee on hardware that cannot produce and verify real attestation evidence.

## Verified core behavior

The ignored `core_smoke` suite covers the core CLI path on a real MicroVM host:

- pull/load image into an isolated `A3S_HOME`;
- detached and foreground `run`;
- non-TTY `exec`, PTY, `attach`, `logs`, `stop`, `wait`, and `rm`;
- TCP published ports with host loopback HTTP reachability;
- bridge network endpoint allocation, peer `/etc/hosts`, connect/disconnect, and force removal cleanup;
- named volumes, `cp`, `diff`, `export`, `commit`, `snapshot`, restart-policy monitor recovery, and Compose health/volume flow;
- warm pool (`pool start`/`pool run`/`run --pool`): pre-warmed sandboxes served over a socket, with backpressure and multi-image lazy pools; `A3S_BOX_RUN_POOL_SOCKET` can auto-route compatible foreground `run --rm` commands through the daemon; `--deferred` runs each command as the box's real main for full box semantics (real exit code + json-file console logs) with no cold boot; `--snapshot-fork` fills the pool by Copy-on-Write restore from one booted template instead of cold booting each sandbox.

The most recent local record, on June 29, 2026: all 15 ignored `core_smoke`
tests passed on macOS Apple Silicon/HVF with Alpine pulled from the registry,
the ignored `host_smoke` VM command matrix plus Compose smoke passed, and a
one-iteration real host soak rehearsal (`--core --host --soak --soak-no-bench`)
produced a passing evidence bundle at
`src/target/a3s-box-soak/20260629T120916Z-22543` with 407 seconds of runtime,
4 resource samples, zero failed iterations, and no shim/mount/socket/box-dir
growth.

On July 18, 2026, the focused canonical `compose.acl` host smoke also passed on
macOS arm64/HVF with `docker.io/library/alpine:latest`. It covered unchanged
`up` convergence, pull/project views, exec/top/port/copy, stop/start/restart,
pause/unpause, kill/wait/remove, `down -v`, and final Box/socket cleanup. This
focused result does not replace the full macOS/Linux host matrix release gate.

For **v2.4.0**, the merged tree was additionally validated on a real Linux
`/dev/kvm` host: the composed-main CI integration suite passed; a **2-hour
endurance soak of 4584 real-microVM operations** (high-frequency
create/run/remove churn plus a full run → exec → snapshot → stop → rm lifecycle
every tenth op) finished with **zero leak** — orphan shims, overlay mounts, box
directories, and disk all returned to baseline; and complex **stateful**
containers passed: a named volume's data survived stop/start, a Redis instance's
key survived a `restart` (`SET` → `SAVE` → `restart` → `GET`), and an nginx box
served HTTP.

For production Linux server clusters, the next validation tier is the
explicitly enrolled cluster plan in
[`docs/production-cluster-tests.md`](docs/production-cluster-tests.md). It keeps
production nodes behind labels and taints, separates node-local CLI integration
from RuntimeClass smoke, and defines 2-hour, 24-hour, and 72-hour soak gates
with stop conditions and evidence recording.

## Install

```bash
# macOS / Linux via Homebrew tap
brew install a3s-lab/tap/a3s-box

# From source
git clone https://github.com/A3S-Lab/Box.git
cd Box
just release
```

For development builds that you plan to run locally, build the static Linux
guest init as well: `just build-guest debug`. `a3s-box` refreshes this binary
into each guest rootfs as PID 1; without it, cached images may keep an older
guest init and miss newer runtime behavior such as staged environment variables.

On macOS, use Apple Silicon. On Linux, use a host with KVM/libkrun support. On Windows, enable Windows Hypervisor Platform for the native WHPX backend:

```powershell
Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform
```

Run `a3s-box info` first; it reports virtualization, platform, bridge backend, port-publishing support, TEE availability, package-cache state, the current virtio-fs cache mode, and any reachable warm-pool daemon on the default or configured sockets.

## Quick start

```bash
# Run a command in a MicroVM
a3s-box run --name hello alpine:latest -- echo "hello from a3s-box"

# Interactive shell
a3s-box run -it --name dev alpine:latest -- /bin/sh

# Detached service with resources and a published TCP port
a3s-box run -d --name web --cpus 2 --memory 1g -p 8080:80 nginx:alpine

# Inspect, exec, logs, and stop
a3s-box ps
a3s-box exec web -- nginx -v
a3s-box logs -f web
a3s-box stop web
a3s-box rm web
```

## Command surface

A3S Box exposes 56 top-level commands. They are Docker-like, not Docker-identical.

| Category | Commands |
| --- | --- |
| Lifecycle | `run`, `create`, `start`, `stop`, `restart`, `rm`, `kill`, `pause`, `unpause`, `wait`, `rename`, `prune` |
| Execution | `exec`, `attach`, `top`, `shell` |
| Images | `pull`, `push`, `build`, `images`, `rmi`, `tag`, `image-inspect`, `history`, `image-prune`, `save`, `load`, `commit` |
| Filesystem | `cp`, `export`, `diff` |
| Networking | `network`, `port` |
| Volumes | `volume` |
| Snapshots | `snapshot` |
| Compose | `compose` |
| TEE | `attest`, `seal`, `unseal`, `inject-secret` |
| Observability | `ps`, `logs`, `inspect`, `stats`, `events`, `df`, `audit` |
| System | `system-prune`, `container-update`, `monitor`, `pool`, `login`, `logout`, `version`, `info`, `help` |

Box references accept name, full ID, or unique short ID prefix.

## Lifecycle and execution

```bash
a3s-box run [OPTIONS] IMAGE [-- CMD...]
a3s-box create [OPTIONS] IMAGE [-- CMD...]
a3s-box start BOX [BOX...]
a3s-box stop BOX [BOX...]
a3s-box restart BOX [BOX...]
a3s-box rm [-f] BOX [BOX...]
a3s-box wait BOX [BOX...]
```

Important supported options:

- `--name`, `--label`, `--restart no|always|on-failure[:N]|unless-stopped`;
- `--cpus`, `--memory`, `--timeout <seconds>` for foreground runs, `--pids-limit`, `--cpuset-cpus`, `--ulimit`, CPU quota/shares, memory reservation/swap;
- `-e/--env`, `--env-file`, `--entrypoint`, `-u/--user`, `-w/--workdir`, `--hostname`, `--add-host`;
- `-i/--interactive` to keep stdin open; non-interactive runs close guest stdin by default, and `--no-stdin` makes that explicit;
- `--package-cache pnpm|npm` to mount persistent package-manager caches for repeated throwaway Node boxes;
- `--health-cmd`, `--health-interval`, `--health-timeout`, `--health-retries`, `--health-start-period`, `--no-healthcheck`;
- `--stop-signal`, `--stop-timeout`, `--persistent`, `--log-driver json-file|none`;
- `--cap-add`, `--cap-drop`, `--security-opt seccomp=default|seccomp=unconfined|no-new-privileges`, `--privileged`.

For CI-style one-shot commands, prefer foreground `run --rm --timeout <seconds>` and avoid `-i` unless the command truly needs stdin. A timed-out foreground run stops/removes the box according to the usual `--rm` behavior and exits with code 124. `a3s-box wait` prints a low-frequency stderr keepalive while it is blocking so long CI or SSH sessions do not look idle; use `--no-heartbeat` or `--heartbeat-interval <seconds>` to tune it.

The manager-backed `create` path treats `start` as first activation of its
nonterminal reservation. Once that managed execution is stopped or failed,
ordinary `start` rejects it instead of reusing a stale generation. Explicit
managed restart persists separate teardown and startup phases, advances the
generation only after the old runtime is terminal, and retains its operation ID
for idempotent crash recovery. CLI `restart` uses that manager path for managed
records; legacy records retain their existing stop-and-boot behavior.

Health checks for detached `run`, Compose services, and boxes brought back by
`start`/`restart` are owned by a generation-fenced background worker, not by the
short-lived creating CLI. The worker stops when that box generation stops or is
replaced; an installed `a3s-box monitor` detects the worker lock and does not
duplicate its probes.

Unsupported or guarded options fail early instead of being silently stored: host devices, GPUs, AppArmor labels, SELinux labels, custom seccomp profiles, unsupported users, invalid workdirs, unsupported port syntax, and unsupported network policies.

## Images and builds

```bash
a3s-box pull alpine:latest
a3s-box pull --verify-key cosign.pub ghcr.io/org/image:v1
a3s-box images
a3s-box images --filter reference='alpine*' --filter label=tier=web
a3s-box inspect alpine:latest          # polymorphic: resolves a container or an image
a3s-box image-inspect alpine:latest
a3s-box tag alpine:latest local-alpine:dev
a3s-box save -o alpine.tar alpine:latest
a3s-box load -i alpine.tar --tag local-alpine:dev
a3s-box push registry.example/org/image:v1
a3s-box push --plain-http localhost:5000/org/image:v1
```

Docker Hub aliases share cache resolution, so `alpine`, `alpine:latest`, and `docker.io/library/alpine:latest` can resolve to the same local image when unambiguous. Digest-only references resolve locally when the digest matches exactly or by unique prefix.

Authenticated pulls use credentials from `a3s-box login`, Docker configuration,
or `REGISTRY_USERNAME` / `REGISTRY_PASSWORD`. If a registry advertises Basic
authentication only after a protected manifest or blob request, Box retries an
unauthorized request with preemptive Basic authentication when both credential
fields are non-empty. Manifest, config, and layer digests are verified; layers
remain streamed to disk. Same-origin redirects retain authentication, while
cross-origin redirects never receive the registry Authorization header. The
same pull path is used by explicit `pull` and an implicit image pull during
`run`.

Use `a3s-box push --plain-http` for an explicit HTTP registry. `--insecure` is accepted as an alias, and `--tls-verify=false` maps to the same behavior for Docker-compatible scripts.

Build support is intentionally explicit:

```bash
a3s-box build -t app:dev .
a3s-box build -t app:dev -f Containerfile .
a3s-box build -t app:dev --build-arg VERSION=1.2.3 --platform linux/amd64 .
a3s-box build -t builder --target builder --no-cache .   # stop at a stage, skip the cache
a3s-box build --builder=buildkit-vm --platform linux/arm64 -t app:dev . # safe macOS RUN path
a3s-box build --builder=buildkit-vm --push --plain-http -t 10.0.0.2:5000/app:v1 .
a3s-box pool start --image alpine:latest --size 1 --socket /tmp/a3s-build-pool.sock
a3s-box build --run-pool --run-pool-socket /tmp/a3s-build-pool.sock -t app:dev .
```

Supported Dockerfile subset: `FROM` including `scratch`, shell/exec-form `RUN` (including `RUN --mount=type=cache,target=...` with Docker's default `sharing=shared`, or explicit `sharing=locked`, optional `from=<stage-or-image>,source=...` cache seeding, `RUN --mount=type=bind,source=...,target=...` from the build context or `RUN --mount=type=bind,from=<stage-or-image>,source=...,target=...`, `RUN --mount=type=tmpfs,target=...` without `size=`, and Docker's no-op defaults `RUN --network=default` / `RUN --security=sandbox`), shell-form `COPY`/`ADD` (incl. `COPY --from=<stage>`, `COPY`/`ADD --chown=user[:group]`), `WORKDIR`, `ENV`, `ENTRYPOINT`, `CMD`, `EXPOSE`, `LABEL`, `USER`, `ARG`, `SHELL`, `STOPSIGNAL`, `HEALTHCHECK`, `ONBUILD` metadata triggers, and `VOLUME`. A context-root `.dockerignore` is honored.

Build flags: `-t/--tag`, `-f/--file`, `--build-arg`, `--platform`, `--target <stage>` (build only up to a stage), `--no-cache` (rebuild every layer), `--builder auto|host|buildkit-vm`, `--push`, `--plain-http`, `--buildkit-image <image>`, `--buildkit-cpus <n>`, `--buildkit-memory <size>`, `--run-pool`, `--run-pool-socket <path>`, `--run-pool-autostart`, `--run-pool-image <image>`, `--run-pool-cpus <n>`, `--run-pool-memory <size>`, `--run-pool-timeout <duration>`, `--run-cache-dir <path>`, `-q/--quiet`.

Boundaries:

- `RUN` uses isolated Linux `chroot`, requires root-capable Linux, validates shell/workdir preconditions, and has a Linux-only ignored smoke test;
- `--run-pool` is the built-in engine's isolated VM path for `RUN`: it leases one warm-pool VM per build stage, mounts that stage's mutable rootfs at `/run/a3s/build-rootfs`, executes shell-form RUN through the configured shell and exec-form RUN as argv with the Dockerfile `WORKDIR`, `ENV`, and `USER`, then diffs the host rootfs into OCI layers. It requires a running `a3s-box pool start` daemon. `RUN --mount=type=cache` is treated as a persistent cache overlay keyed by `id=` (or by `target=` when `id` is omitted); cache contents are visible during matching `RUN` commands but are restored before layer diffing, so they are not committed to the image. Successful RUNs publish cache writes; failed RUNs restore the rootfs without publishing partial cache contents. A new cache can be seeded from `from=<stage-or-image>,source=<dir>`; once the persistent cache exists, it is not re-seeded. The warm-pool path accepts Docker's default omitted `sharing=shared`, explicit `sharing=shared`, and `sharing=locked`; because the host overlay hydrates and publishes cache directories around each RUN, access to the same cache key is serialized across builds to avoid writeback races. Cache-root `mode=`, `uid=`, and `gid=` are supported; cache `sharing=private` remains unsupported. `RUN --mount=type=bind` can mount sources from the build context, a previous build stage, or an external image with `from=<stage-or-image>`, defaults `source=.` when omitted, resolves relative targets from `WORKDIR`, honors `.dockerignore` only for context sources, and discards writes before layer diffing. `RUN --mount=type=tmpfs` creates an empty temporary target, resolves relative targets from `WORKDIR`, restores the original target after RUN, and discards writes before layer diffing; `tmpfs size=` is not supported yet. `RUN --network=default` and `RUN --security=sandbox` are accepted as Docker's default no-op values; non-default per-RUN network/security modes are rejected until the warm-pool exec path can enforce them.
- macOS `RUN` auto-selects the BuildKit VM backend unless `--run-pool` or `A3S_BOX_UNSAFE_HOST_RUN=1` is set; explicit `--builder=host` keeps the built-in host engine behavior;
- the BuildKit VM backend loads `type=oci` output back into the A3S image store by default; `--push` writes directly to the tagged registry reference, uses the same credentials as `a3s-box push`, and `--plain-http` marks that registry as trusted HTTP for BuildKit;
- Apple Silicon `--builder=buildkit-vm --platform linux/amd64` is handled by BuildKit's Linux builder path and may use emulation, so expect slower builds than native `linux/arm64`;
- `A3S_BOX_UNSAFE_HOST_RUN=1` enables unsafe macOS host-side experiments only;
- `--platform` records one target platform; multi-platform image indexes are not implemented.

Builds use a Docker/BuildKit-style **layer cache**: each instruction extends a
rolling chain key (its text plus, for `COPY`/`ADD`, the content hash of the
source files), and a layer-producing step whose chain key was seen before is
reused instead of re-run. A changed instruction or input rebuilds that layer
and everything after it. The cache lives at `~/.a3s/buildcache` and is size-capped
(default 2 GiB, override with `A3S_BOX_BUILDCACHE_MAX_BYTES`; oldest blobs evicted first).

## Filesystems, volumes, and snapshots

```bash
a3s-box volume create data
a3s-box run -d --name app -v data:/data alpine:latest -- sleep 3600
a3s-box run --rm --cpus 4 --memory 4g --package-cache pnpm --virtiofs-cache=always \
  -v "$PWD:/work" -w /work --tmpfs /work/node_modules:size=4g \
  node:22-alpine -- sh -lc 'corepack enable && pnpm install --frozen-lockfile'
a3s-box cp ./file.txt app:/data/file.txt
a3s-box diff app
a3s-box export app -o rootfs.tar
a3s-box commit app -t app:snapshot
a3s-box snapshot create app checkpoint-1
a3s-box snapshot restore checkpoint-1 --name restored-app
a3s-box snapshot prune --keep 5          # bound disk: keep the 5 newest
```

`--package-cache pnpm` creates/reuses the named volume `a3s-cache-pnpm` and sets cache-friendly defaults: `PNPM_CONFIG_STORE_DIR=/a3s-cache/pnpm/store`, `npm_config_store_dir=/a3s-cache/pnpm/store`, `COREPACK_HOME=/a3s-cache/pnpm/corepack`, `PNPM_HOME=/a3s-cache/pnpm/home`, `npm_config_cache=/a3s-cache/pnpm/npm-cache`, `PNPM_CONFIG_PREFER_OFFLINE=true`, `npm_config_prefer_offline=true`, and `COREPACK_ENABLE_DOWNLOAD_PROMPT=0`. Dependency downloads and the Corepack-prepared pnpm toolchain survive across `--rm` boxes without making the whole rootfs persistent. `--package-cache npm` creates/reuses `a3s-cache-npm` and sets `npm_config_cache=/a3s-cache/npm/cache` plus `npm_config_prefer_offline=true` for npm-only jobs. Override any of those with `-e KEY=VALUE` when a build needs a specific registry or cache policy. For throwaway install/build jobs, mounting `node_modules` as tmpfs avoids pushing thousands of small files through the project bind mount; prime the named cache volume before a release window when cold registry downloads or project-level supply-chain policy checks are known to dominate the first run. Use `bench/bench.sh pnpm` or `just bench-pnpm` to compare A3S project-mount, A3S tmpfs, and Docker cold/hot baselines. Auto-removed boxes also archive their last logs under `~/.a3s/removed-logs/`, and `a3s-box logs <name-or-id>` can read that archive after the box directory is gone.

Host directory volumes are mounted with virtio-fs `cache=none` by default to favor stable traversal on macOS/HVF workloads with large source trees. Use `--virtiofs-cache=always` for release verification jobs where the host source tree is not changing during the run, or `--virtiofs-cache=auto|default` for local experiments; `A3S_VIRTIOFS_CACHE` remains available as a process-wide fallback and `a3s-box info` prints the active fallback setting. On macOS, each Linux rootfs lives below a private directory in a case-sensitive APFS sparse image. Cached sparse images are cloned with APFS copy-on-write, preserving Linux path identity without exposing APFS volume-management entries to the guest.

Image extraction and `commit` preserve Linux uid, gid, mode, and symlink
metadata even when the macOS backing filesystem cannot represent OCI ownership
directly. Box records rootless layer metadata during extraction and guest-init
replays it before mounting any host workspace or volume; stopped persistent
boxes use a guest-captured terminal manifest so a committed image reflects the
container's final Linux-visible metadata rather than the host user's APFS
ownership.

The `snapshot` command produces configuration/filesystem-oriented Box snapshots, not a live RAM checkpoint. The live RAM Copy-on-Write facility is a separate, lower-level mechanism described in [Warm pool and snapshot-fork](#warm-pool-and-snapshot-fork).

`snapshot restore` is **copy-on-write**: the restored box shares the snapshot's rootfs as a read-only overlay lower with its own per-box upper, so forking a warmed snapshot is near-instant, space-cheap (a few MB per fork), and isolated — this is what the [SDK](#sdk) pipeline API forks per step. (On a non-overlay host it falls back to a full copy.) `snapshot create` still deep-copies the box rootfs into the store, so a scheduled snapshot workflow can fill the disk: `snapshot prune --keep N` / `--max-bytes B` evicts the oldest beyond a cap, and `A3S_BOX_MAX_SNAPSHOTS` / `A3S_BOX_MAX_SNAPSHOT_BYTES` auto-prune on every `create` (unset = unbounded). Because a restored box keeps referencing its snapshot, `snapshot rm` / `prune` refuse to delete a snapshot still in use by a box (`--force` overrides).

## SDK

`a3s-box-sdk` is the Rust SDK for A3S Box, published to crates.io. Its default
`A3sBoxClient` calls runtime stores, sockets, and the same generation-fenced
execution manager as the CLI without spawning the CLI. It exposes typed managed
lifecycle, image, volume, network, snapshot, diagnostics, exec, and file APIs;
see [`src/sdk/README.md`](src/sdk/README.md) for the direct client.

The optional `pipeline-cli` feature provides a **programmable CI/CD pipeline**
API (`a3s_box_sdk::pipeline`): a pipeline is a Rust program and each step runs
in its **own MicroVM** (one kernel per step), forking a warmed snapshot via
copy-on-write `snapshot restore`. The DAG is your code, not YAML.

```rust
use a3s_box_sdk::pipeline::{warm_base, WarmBase, FileCache, Step};

let cache = FileCache::new(".ci-cache")?;             // skip a step when inputs are unchanged
let mut base = warm_base(                              // clone + install deps ONCE, then snapshot
    WarmBase::new("node:20", "git clone $REPO /w && cd /w && npm ci").cache(&cache),
)?;
base.step(Step::new("test", "cd /w && npm test"))?;   // nonzero exit -> Err (fail-fast)
base.step(Step::new("build", "cd /w && npm run build"))?;
base.dispose();
```

The former MicroVM workload-execution SDK (`ExecutionRegistry`/`VmExecutor`, for embedding Box into higher-level runtimes such as a3s-lambda) is now the **`a3s-box-lambda`** crate.

### E2B protocol and Python/TypeScript SDK compatibility

E2B compatibility is under active development and is not yet a released
compatibility claim. The first implementation gate pins the official control,
envd, volume-content, Process, Filesystem, MCP, Python, TypeScript, and Code
Interpreter contracts under [`compat/e2b/`](compat/e2b/README.md). CI regenerates
their endpoint, field, error, descriptor, and public-export inventories and
rejects unreviewed protocol drift.

| Client | Pinned version |
| --- | ---: |
| Python `e2b` | 2.32.0 |
| TypeScript `e2b` | 2.33.0 |
| Python `e2b-code-interpreter` | 2.8.1 |
| TypeScript `@e2b/code-interpreter` | 2.6.1 |

The typed [`a3s-box` Python package](sdk/python/README.md) and
[`@a3s-lab/box` TypeScript package](sdk/typescript/README.md) re-export those
pinned official SDK surfaces and provide per-call A3S endpoint configuration.
CI builds, installs, and tests both packages, and release automation produces
wheel, source, and npm tarball artifacts. They are source-tree previews and
are not yet published to PyPI or npm. The destructive production runner can
repeat its complete runtime-image matrix through both A3S packages after the
unchanged official clients pass.

Native SDK users configure `A3S_BOX_ENDPOINT` and `A3S_BOX_API_KEY`; conventional
`https://api.<domain>` endpoints derive the Sandbox routing domain automatically.
Lifecycle responses advertise the public direct Sandbox authority, including a
configured non-standard TLS port, so normal deployments do not require a
process-global Sandbox URL override.

The Phase 2 preview includes an owner-scoped Rust lifecycle router for create,
connect, get, memory-preserving pause, connect/resume, v1/v2 running/paused
list, timeout, monotonic refresh, current single/batch metrics,
generation-fenced v1/v2 structured logs, and kill; owner-scoped Volume
create/connect/list/delete and authenticated content operations; a SQLite WAL
repository with generation-fenced transitions and restart reconciliation; and
a canonical runtime `ExecutionManager` with a production VM/Sandbox backend. CI
runs the pinned official Python sync/async, TypeScript, and Code Interpreter
clients against the router through an in-memory repository and fake execution
manager.
An opt-in A3S OS gate installs those same checksum-pinned packages without
modification and runs them against the ACL-configured production process and
real `crun` Sandboxes. Python sync, Python async, and TypeScript each cover
create, memory-preserving pause, paused-state listing, connect-based resume,
survival of the same background process, filtered list, timeout replacement,
current metrics with historical-range filtering, kill, and not-found behavior.
The tested base packages are Python `e2b` 2.32.0 in sync and async modes and
TypeScript `e2b` 2.33.0. All three run one foreground `commands.run` through
the ConnectRPC JSON transport as the image's default non-root user and verify
its stdout, empty stderr, and successful exit on a real `crun` execution. Python
`e2b-code-interpreter` 2.8.1 and TypeScript
`@e2b/code-interpreter` 2.6.1 participate in the runtime-image execution gate.
That mode additionally exercises Filesystem create/read/stat/list/rename/remove,
background commands with process listing and stdin close, PTY allocation and
resize, and Code Interpreter execution plus context create/list/restart/remove.
Each Python sync/async, TypeScript, and Code
Interpreter object also calls its official `is_running`/`isRunning` method
through the production TLS gateway. Running checks follow the template's
broker/runtime placement; post-kill checks use host-resolved terminal health,
returning `true` while running and `false` after termination.

Managed creation requests also persist a typed caller policy for names,
restart and health behavior, logging, stop behavior, and local resource
metadata. An idempotent retry therefore cannot silently reuse a reservation
with different policy, and the canonical record mapper does not replace caller
choices with runtime defaults.
Separately, an A3S OS smoke harness proves that `--isolation sandbox` create
persists a recoverable `created` reservation without allocating backend
resources, then starts through certified `crun`, preserves memory across
pause/connect-resume, proves that the same process survives, rejects
filesystem-only pause explicitly, and owns kill and cleanup without MicroVM
fallback. The same host validation proves that structured Sandbox logs retain
both stdout and stderr, drain final records before natural-exit or auto-remove
archival, and leave no generation log worker, crun state, box directory, or
socket behind.

The first production E2B data-plane slices are now implemented, while the full
envd surface remains incomplete. CLI `create` persists its reservation and
complete caller policy through the canonical manager, and the first `start` of
that reservation consumes its persisted generation through the same manager.
The production backend prepares named-volume and network ownership
idempotently and rolls back only resources acquired by a failed start attempt.
Ordinary `start` does not revive a terminal managed execution. CLI `restart`
uses a durable two-phase operation that terminates the old runtime before
advancing the generation, recovers ambiguous kill/start responses from backend
evidence, and rebinds local resources without duplicating ownership. CLI `run`
reserves and starts through the same manager, freezes image-defined health and
stop defaults into the durable request, and leaves network, volume, rootfs,
stop, and auto-remove ownership to the managed backend. The Rust SDK exposes
typed create, start, run, inspect, pause, resume, restart, kill, and
reconciliation calls through that same manager.

The `a3s-box-e2b` process accepts only `.acl` configuration parsed by `a3s-acl`.
It composes SQLite lifecycle state, the canonical runtime manager, production
credential providers, startup reconciliation, periodic expiry reaping, and
graceful shutdown. Account keys use salted PBKDF2-SHA256 hashes; scope-separated
sandbox tokens use AES-256-GCM, independent HMAC validation, and versioned key
rotation. Route policy is persisted with each lifecycle record, and strict
wildcard/shared parsing projects immutable leases fenced by generation, expiry,
port, and token scope without a second mutable routing state.

Each template persists an explicit envd placement. Broker mode handles the
implemented envd routes on the host. Runtime mode forwards health, Process,
Filesystem, and file HTTP requests to port `49983` inside the exact
generation-fenced Sandbox. A runtime-mode sandbox remains unpublished until
that port accepts a fenced connection; readiness failure stops the execution
and leaves its lifecycle record hidden.

Sandbox expiry is measured from the later of runtime start and observed envd
readiness, so cold startup does not consume the caller's requested usable
timeout. Startup reconciliation applies the same rule when recovering a
creating record whose execution was committed before the service restarted.

Memory-preserving pause maps to certified `crun pause`; a later `connect` or
deprecated `resume` request maps to `crun resume`. The production matrix starts
a background process before pausing and proves that the same process continues
after resume. Filesystem-only pause (`memory: false`) is rejected explicitly
until cold-pause semantics are implemented.

The v1 and v2 Sandbox log routes read the canonical generation-fenced
`json-file` runtime logs. They support cursor, direction, level, search, and
limit semantics, read bounded rotated gzip files oldest-first, ignore an
incomplete live tail, and stably order concurrent stdout/stderr entries by
timestamp.

Owner-scoped Volume records use durable SQLite state and an independently
scoped encrypted content token. The authenticated content routes implement
directory, file, path, and metadata operations, while Sandbox creation resolves
public Volume names to runtime-managed mounts. Official and A3S Python
sync/async and TypeScript clients prove bidirectional mount I/O, public mount
metadata, UID/GID mapping, in-use deletion conflicts, and final cleanup against
real `crun` executions.

The runtime-image smoke also validates the pinned `/metrics` schema,
create-time environment through `/envs`, metadata-preserving multipart upload,
byte-identical octet-stream download, invalid-token rejection, and cleanup
through the authenticated wildcard TLS route.

The production wildcard TLS gateway supports HTTP/1.1 and HTTP/2 clients over
both direct and shared sandbox routes. It validates each lease, applies CORS,
strips edge credentials, and enters the real `crun` network namespace through a
generation- and PID-fenced connector. As with the official E2B sandbox proxy,
the plaintext Sandbox origin is contacted with HTTP/1.1, including when the
downstream client uses HTTP/2; the origin is not required to provide h2c.
Authenticated terminal `GET /health` remains host-resolved after kill so a
scope-valid envd token receives the `502` response expected by official SDK
running-state methods without reopening a route lease. An invalid token remains
unauthorized. Ordinary traffic continues through the fenced Sandbox
network-namespace proxy.

The first Process broker slice is also implemented. It uses generation-scoped
synthetic process IDs and supports Start, JSON-framed Connect, List, SendInput,
CloseStdin, SIGKILL, PTY Start/resize, and ordered start, output, keepalive, and
end events. The pinned Python sync/async and TypeScript runtime-image clients
cover foreground and background commands, process listing, stdin/close, wait,
and one PTY resize flow. Client-streaming `StreamInput`, SIGTERM and other
signals, binary Connect framing, the complete PTY/reconnect/backpressure
matrix, and durable process recovery across service restart are not yet
compatibility claims.

An A3S OS production smoke test exercises this path on a real `crun` OCI
Sandbox created with `--isolation sandbox`. It verifies lifecycle operations,
v1 running-list behavior, monotonic refresh with an optional body, current
batch metrics, generation-fenced v1/v2 structured logs with forward/backward
ordering, envd health over both TLS route forms, runtime metrics/environment and
HTTP file transfer, a real traffic-token-protected workload service on port
`49999`, invalid and scope-swapped token denial, service-restart recovery,
stale-route fencing after kill, authenticated terminal health, and complete
runtime cleanup. The default
`localhost.localdomain` wildcard is DNS- and
TLS-preflighted before a Sandbox starts. The same A3S OS gate runs the unchanged
official clients through both running and post-kill health checks and the
runtime data-plane cases described above. Those client paths additionally prove
v2 paused-state listing and memory-preserving pause/connect-resume with
same-process survival, owner-scoped Volume create/connect/list/content/delete,
bidirectional Sandbox mounts, UID/GID mapping, and in-use deletion conflicts.
Failed runs can preserve the Sandbox PID, `crun` state, OCI bundle, and service
logs for diagnosis. Filesystem-only pause, historical metrics, multi-file and
large-file behavior, deeper Volume failure/recovery and concurrent-mutation
cases, exhaustive Process and PTY matrices, Filesystem watches and signed URLs,
official public-port coverage, rich multi-language Code Interpreter behavior,
MCP, native package publication, and the complete production package matrix
remain open release gates.

The server, native Python/TypeScript packages, and unchanged-official-client
black-box suites follow the phased design in
[`docs/e2b-compatible-sdk-design.md`](docs/e2b-compatible-sdk-design.md). Until
that complete matrix passes, generated manifests explicitly report
`full_compatibility=false`.

## Warm pool and snapshot-fork

A **warm pool** keeps a set of sandboxes pre-booted and serves them over a Unix
socket, so a request is answered by an already-running microVM instead of a cold
boot. It supports backpressure, multi-image lazy pools, and a `--deferred` mode
that runs each request as the box's real main process (real exit code +
json-file console logs).

```bash
a3s-box pool start --image alpine:latest --size 8     # pre-warm 8 sandboxes
a3s-box pool start --image alpine:latest --lease-ttl 30m  # reclaim abandoned leases
a3s-box pool start --image alpine:latest --size 8 --snapshot-fork   # CoW fill
a3s-box pool start --image alpine:latest --metrics-addr 127.0.0.1:9101   # + Prometheus /metrics
a3s-box pool run alpine:latest -- echo hi             # served from the pool
a3s-box run --pool --rm alpine:latest -- echo hi      # Docker-like run shape
a3s-box run --pool-autostart --rm alpine:latest -- echo hi
a3s-box run --pool --rm -v "$PWD:/work:ro" -w /work alpine:latest -- cat README.md
a3s-box build --run-pool --run-pool-socket /tmp/a3s-box-pool.sock -t app:dev .
a3s-box build --run-pool-autostart --run-pool-image alpine:latest -t app:dev .
a3s-box pool status
a3s-box pool stop
```

`run --pool` is intentionally a foreground one-shot path today: it requires
`--rm` and supports the common hot-loop dimensions (`--user`, `--workdir`,
`--env`, `--env-file`, `--volume`, `--cpus`, `--memory`, and
`--package-cache`, plus foreground `--timeout`). Image, volumes, vCPUs, and
memory are part of the warm-pool key because virtio-fs mounts and VM resources
are fixed at boot. Set
`A3S_BOX_RUN_POOL_SOCKET=/path/to/pool.sock` to auto-route compatible
foreground `run --rm` commands through the same daemon; incompatible runs keep
the normal cold-start path unless `--pool` was requested explicitly. Use
`--pool-autostart` to start a daemon on `--pool-socket` when one is not already
running. Options that require persistent box state or a named lifecycle, such as
`--name`, stay on the normal run path instead of being silently ignored by the
one-shot pool path.

`build --run-pool` uses the same daemon but with a lease protocol instead of a
one-shot sandbox: a build stage keeps one warm VM while it executes every
Dockerfile `RUN` in that stage with the current Dockerfile `WORKDIR`, `ENV`, and
`USER`, then releases it. The stage rootfs remains the single source of truth;
the VM only provides isolated Linux execution. Because each stage rootfs mount is
unique and short-lived, volume-bound build leases are filled on demand instead
of pre-warming a whole idle pool for every stage. Use
`--run-pool-autostart --run-pool-image <helper-image>` when the build command
should start the helper daemon itself.

`pool status` reports idle sandboxes, active checked-out sandboxes, and active
leases per pool key, which is useful when Dockerfile `RUN` stages are holding a
warm VM. `a3s-box info` also performs a best-effort daemon probe against
`A3S_BOX_RUN_POOL_SOCKET`, `A3S_BOX_BUILD_RUN_POOL_SOCKET`, and the default
socket, then prints the aggregate max/idle/active/leased counts when one is
reachable. `pool start --lease-ttl <duration>` reclaims unreleased internal
leases that have been idle for too long (default: `1h`, `0` disables this);
running lease exec requests are never reclaimed mid-command. `pool stop` sends a
stop request over the daemon socket, drains idle and leased VMs, removes the
socket, and exits. It succeeds when no daemon is running so cleanup scripts can
call it unconditionally.

`pool start --metrics-addr` serves a Prometheus `/metrics` endpoint with warm-pool hit/miss, VM-boot, and cache metrics for the long-running daemon (alongside `monitor --metrics-addr`'s box-state metrics + `/healthz`).

**Snapshot-fork** (`--snapshot-fork`, Linux `/dev/kvm` only) is native
Copy-on-Write microVM cloning. The pool cold-boots one template sandbox,
snapshots its file-backed guest RAM together with KVM vCPU and virtio device
state, and then restores the rest of the pool from that snapshot. Each fork maps
the template RAM `MAP_PRIVATE`, so it pays only for the pages it dirties. On a
`/dev/kvm` host this is ~4× faster than a cold boot per fork, completes 100
forks in under ~1 s (~8 ms amortized each, ~13 MB RSS per fork), and `exec`
runs real commands over virtio-fs inside the restored guest. It is off by
default.

The same mechanism is available below the pool through environment variables:
`KRUN_SNAPSHOT_MEM_FILE` and `KRUN_SNAPSHOT_SOCK` capture a snapshot from a
booted template, and `KRUN_RESTORE_FROM` restores a fork from it. Per-VM
`BoxConfig`/`InstanceSpec` fields (`snapshot_mem_file`, `snapshot_sock`,
`restore_from`) take precedence over the env when set.

## Pruning stopped boxes

```bash
a3s-box prune --force            # remove every created/stopped/dead box
a3s-box container-prune --force  # alias
```

`prune` is the box-only counterpart to `system-prune` (which also removes images
and networks). Running and paused boxes are never touched.

## Networking

A3S Box has three network modes:

| Mode | What it does | Current boundary |
| --- | --- | --- |
| TSI default | Guest socket operations are proxied through the host. Use this for simple outbound access. On macOS, publishing a TCP port automatically selects an isolated netproxy-backed interface so application bytes and guest loopback remain reliable while the CLI/network-mode contract stays unchanged. | Plain TSI boxes have no user-defined peer network and no in-guest loopback. Use a bridge network for peer discovery; publishing a port on macOS activates the isolated compatibility data path automatically. |
| Bridge | Creates a real guest network interface for user-defined networks and peer discovery. | Linux uses `passt` with outbound NAT. macOS uses built-in `netproxy` for peer networking, published TCP ports, DNS forwarding, and outbound TCP connections through the host stack. Non-DNS outbound UDP and ICMP are not proxied on macOS. |
| None | No network. | Useful for intentionally isolated workloads. |

```bash
a3s-box network create backend --subnet 10.89.0.0/24
a3s-box run -d --name api --network backend -p 8080:80 myapi:latest
a3s-box network inspect backend
a3s-box network connect backend stopped-box
a3s-box network disconnect backend stopped-box
a3s-box network rm --force backend
a3s-box network prune --force   # remove all networks not used by any box
a3s-box port api
```

Published ports support TCP only in `host_port:guest_port[/tcp]` form. UDP, host-IP binds such as `127.0.0.1:8080:80`, single-port shorthand, and ranges are rejected during CLI or Compose validation. `network connect` and `network disconnect` apply to inactive boxes; live hot-plug is not implemented. Strict/custom network policy modes are rejected until packet filtering is implemented.

## Compose applications

`compose.acl` is the canonical project file and is discovered automatically:

```acl
service "api" {
  image = "ghcr.io/a3s-lab/api:latest"
  command = ["serve"]
  environment = { PORT = "8080" }
  ports = ["8080:8080"]
  depends_on = ["db"]
}

service "db" {
  image = "postgres:17"
  volumes = ["data:/var/lib/postgresql/data"]
}

volume "data" {
  driver = "local"
}
```

```bash
a3s-box compose config
a3s-box compose up -d
a3s-box compose ps
a3s-box compose logs -f
a3s-box compose exec api -- sh
a3s-box compose restart api
a3s-box compose stop
a3s-box compose start
a3s-box compose down
```

The project command surface includes `up`, `down`, `ps`, `logs`, `config`,
`start`, `stop`, `restart`, `rm`, `kill`, `pause`, `unpause`, `wait`, `exec`,
`top`, `port`, `cp`, `images`, `pull`, `ls`, and `volumes`. Service-scoped
operations resolve the immutable project and service labels, then reuse the
same lifecycle commands as individual boxes instead of maintaining a second
state machine.

`compose up` is convergent. It records a deterministic digest of the effective
service and runtime configuration, reuses an unchanged running service, and
recreates a changed or inactive service. Supplying service names limits the
operation to those services and their transitive dependencies. Without `-d`,
`up` attaches to prefixed project logs and stops the selected services on
Ctrl-C; detached mode returns after convergence.

Supported Compose keys: `image`, `command`, `entrypoint`, `environment`, `env_file`, `ports`, `volumes`, `depends_on` with `service_started`, `service_healthy`, or `service_completed_successfully`, `networks`, `dns`, `tmpfs`, `working_dir`, `hostname`, `extra_hosts`, `labels`, `healthcheck`, `restart`, `cpus`, `mem_limit`, `cap_add`, `cap_drop`, and `privileged`.

A3S ACL uses a closed schema: unknown root blocks, nested blocks, attributes,
types, and functions are rejected instead of being silently ignored. Explicit
`compose.yaml`, `compose.yml`, `docker-compose.yaml`, and `docker-compose.yml`
files remain supported as a local Docker Compose-compatible subset, not as a
claim of full Compose Specification parity.

Compose scalar values support `$VAR`, `${VAR}`, `${VAR-default}`,
`${VAR:-default}`, `${VAR+replacement}`, and `${VAR:+replacement}` (plus the
standard required-value forms). Values come from the project `.env` file next
to the selected Compose file, with the invoking shell environment taking
precedence. Expansion happens before typed service and port validation;
mapping keys are not expanded, and `$$` emits a literal dollar sign.
ACL values may also use `env("NAME")`; it resolves from that same merged
environment and fails when the variable is absent.

## TEE workflows

```bash
# Hardware path: requires SEV-SNP-capable Linux host and libkrun support
a3s-box run -d --name secure --tee myimage:latest -- sleep 3600

# Development path: simulated reports and secrets flow
a3s-box run -d --name dev --tee --tee-simulate myimage:latest -- sleep 3600
a3s-box attest dev --ratls --allow-simulated
a3s-box inject-secret dev --secret API_KEY=secret --set-env --allow-simulated
a3s-box seal dev --data "value" --context app/key --policy measurement-and-chip
a3s-box unseal dev --context app/key
```

TEE features include SNP report parsing/verification, RA-TLS certificate extensions, AES-256-GCM sealing with HKDF-SHA256, and RA-TLS secret injection. Treat simulation as a developer workflow only; it does not prove hardware isolation. TDX is not productized.

## Coding-agent skill

`integrations/skills/a3s-box/SKILL.md` is an [Agent Skills](https://agentskills.io)
`SKILL.md` that teaches an AI coding agent to drive this CLI — the `--` separator,
the box lifecycle, snapshots, the warm pool, the networking footguns, and an
errors→fix table for recovery. It is **one file** in the cross-tool Agent Skills
format, so the same skill works in Claude Code, OpenAI Codex, Gemini CLI, Cursor,
Sourcegraph Amp, OpenCode, Zed, and a3s-code — no per-agent variant.

### Install

The installer symlinks the single `SKILL.md` into each agent's skills directory
(one source of truth):

```bash
cd integrations/skills

./install.sh all                   # this project: .agents + .claude + .codex + .a3s
./install.sh --home agents claude  # user-wide:    ~/.agents + ~/.claude
./install.sh --dir ./agent/skills  # any SKILL.md-format skills dir
./install.sh --copy all            # copy instead of symlink
```

Two roots reach almost every skill-capable agent; install the targets you use:

| Target | Skills root | Reached by |
|--------|-------------|------------|
| `agents`   | `.agents/skills/` | OpenAI Codex · Gemini CLI · Amp · Cursor · OpenCode · Zed |
| `claude`   | `.claude/skills/` | Claude Code · Claude Agent SDK · Cline · Cursor/OpenCode (compat) |
| `codex`    | `.codex/skills/`  | Codex (project-specific path) |
| `a3s-code` | `.a3s/skills/`    | a3s-code |
| `all`      | all of the above  | |

Manual equivalent (no installer):
`ln -s "$(pwd)/a3s-box/SKILL.md" <root>/a3s-box/SKILL.md`.

### Use

Reload the agent so it rescans its skills directory. The skill then:

- **surfaces as the `/a3s-box` slash command** (the directory name is the command), and
- **is auto-invoked** when you ask the agent to run, build, exec into, snapshot, or
  sandbox something with a3s-box — its `description` is matched against your request.

The agent reads the skill body and drives `a3s-box` for you — e.g. *"build this repo
and run it in a sandbox"* → `a3s-box build` → `a3s-box run -d` → verify it's up. The
skill restricts itself to `Bash(a3s-box*)`, so it can only invoke this CLI.

### Agents without a skill mechanism

GitHub Copilot, Windsurf/Devin, Continue.dev, Aider, and Jules/Factory have only
always-on instruction files (no on-demand skills). To make one aware of a3s-box, add
a one-line pointer to `integrations/skills/a3s-box/SKILL.md` in that tool's
instructions file, or in a repo-root **`AGENTS.md`** (which most of them read).

See [`integrations/skills/README.md`](integrations/skills/README.md) for the full
agent matrix, the no-skill-agent details, and why this is a skill rather than a
Claude Code plugin.

## Kubernetes CRI

The CRI server is reachable by standard gRPC clients — `crictl`, the kubelet, and `critest` — over its Unix domain socket, and runs the core pod + container lifecycle and `exec` end to end. It is Linux-only and not yet fully `critest`-conformant.

Verified on a `/dev/kvm` host via `crictl`:

- CRI v1 RuntimeService/ImageService over the Unix socket. A vendored `h2` patch (`third_party/h2`, wired via `[patch.crates-io]`) relaxes the percent-encoded socket-path `:authority` that `grpc-go >= 1.57` sends, which upstream `h2` otherwise rejects with `PROTOCOL_ERROR` before any RPC runs.
- Pod sandbox + container lifecycle: `runp` → `create` → `start` → `ps` → `stop` → `rm` → `stopp` → `rmp`.
- `exec` over the Kubernetes SPDY/3.1 `remotecommand` protocol — `kubectl exec` / `crictl exec`, TTY and non-TTY, stdin/stdout/stderr, and exit-code propagation.
- Container stdout/stderr captured to the CRI `log_path` and readable via `crictl logs`.
- RuntimeClass image overrides.

Not yet complete: `attach`, and the stricter `critest` conformance specs (log format, Linux SecurityContext, seccomp/AppArmor, namespace sharing, mount propagation). Track conformance in `docs/cri-conformance.md`.

For an explicit cluster evaluation:

```bash
helm install a3s-box deploy/helm/a3s-box/ -n a3s-box-system --create-namespace
```

Windows CRI is intentionally unsupported.

## Deploy as a Kubernetes RuntimeClass

Run selected pods as a3s-box MicroVMs by setting `runtimeClassName: a3s-box`. Each
pod's containers become libkrun MicroVMs under containerd, and `kubectl exec` works
against them. This is opt-in per node — a node must have the runtime installed **and**
carry the label `a3s-box.io/runtime=true` before a3s-box pods schedule there.

**1. Create the RuntimeClass (once per cluster):**

```bash
kubectl apply -f - <<'EOF'
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: a3s-box
handler: a3s-box
scheduling:
  nodeSelector:
    a3s-box.io/runtime: "true"   # only labeled nodes run a3s-box pods
EOF
```

**2. Provision each node** — run the installer as root on every node that should
host a3s-box workloads. It installs the a3s-box CLI + helpers, libkrun, and the
containerd runtime-v2 shim (`containerd-shim-a3s-box-v2`), registers the
`io.containerd.a3s-box.v2` runtime via an `/etc/containerd/conf.d` drop-in, restarts
containerd, and warms the one-time per-node boot cache. Requires containerd ≥ 2.0
and `/dev/kvm`.

```bash
# one-liner (downloads the pinned release artifacts from GitHub):
curl -fsSL https://raw.githubusercontent.com/A3S-Lab/Box/main/deploy/scripts/install-runtimeclass.sh | sudo bash

# or from a checkout:
sudo deploy/scripts/install-runtimeclass.sh                  # default version
sudo deploy/scripts/install-runtimeclass.sh --version v3.0.2 # pin a version
```

Then label the node from a machine with `kubectl`:

```bash
kubectl label node <node-name> a3s-box.io/runtime=true
```

Notes:
- **Control-plane nodes** carry a `NoSchedule` taint and are normally excluded —
  leave them unlabeled unless you intentionally run workloads there.
- The installer warms up with `busybox:latest` so the *first* pod boots fast (the
  first box on a fresh node builds a one-time cache that can exceed the shim's boot
  window). Use `--warmup-image <ref>` to point at a mirror, or `--no-warmup` to skip.
- **Air-gapped:** pre-stage the release tarball
  (`a3s-box-<ver>-linux-<arch>.tar.gz`) and `containerd-shim-a3s-box-v2-linux-<arch>`
  in a directory and pass `--from-dir <dir>` (no network needed).

**3. Run a pod:**

```bash
kubectl apply -f - <<'EOF'
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
EOF

kubectl exec hello-a3s-box -- sh -c 'echo "hello from $(hostname)"; uname -m'
```

For validation cohorts, use the checked-in manifests instead of pinning raw
node names: `deploy/shim/test-pod.yaml` for a single smoke pod,
`deploy/shim/runtimeclass-smoke.yaml` for one smoke pod per selected node, and
`deploy/shim/soak-complex.yaml` for the long-lived Redis/Postgres/nginx/Python
soak set. For the guarded cluster soak loop, run
`deploy/scripts/runtimeclass-soak.sh`; it applies the smoke/complex workloads,
submits short RuntimeClass job completions, writes an evidence bundle, and
verifies it before returning success. The verifier requires
`metadata.txt` with parseable `started_at`, boolean skip/cleanup flags, and
positive Job completions when churn jobs run,
`runtimeclass.yaml` with `metadata.name: a3s-box`, `handler: a3s-box`, and
`scheduling.nodeSelector.a3s-box.io/runtime: "true"`,
`resource-samples.tsv` with parseable monotonic timestamps, non-negative integer
counters, one final sample, and a `summary.txt` duration that is not shorter
than the sampled time span,
`smoke-exec.txt` proving `kubectl exec` on every selected node,
`complex-exec.txt` proving exec against every long-lived workload
(`redis`, `postgres`, `nginx`, and `python`),
`complex-logs.txt` with workload-prefixed `REDIS_SOAK`, `PG_SOAK`,
`NGINX_SOAK`, and `PY_SOAK` markers,
`final-pod-runtimeclasses.tsv`, and, when churn jobs run, `job-runtimeclass.txt`,
proving `runtimeClassName: a3s-box`, `job-pod-statuses.tsv` proving exactly the
declared number of Succeeded churn pods with zero restarts on selected nodes and
covered by the final pod evidence, plus `job-logs.txt` with `A3S_BOX_JOB_START`,
`A3S_BOX_JOB_RUNTIME_CLASS=a3s-box`, and `A3S_BOX_JOB_DONE` markers exactly
matching the declared Job completion count; it
rejects failed pods/jobs, unexpected pod restarts, Pending/Unknown final pods,
active final jobs, pod artifacts that do not cover the same unique final pod
set, per-pod final status artifacts with unresolved phases or restarts,
incomplete or duplicate selected-node evidence, selected nodes missing the
required production-soak labels in `selected-node-labels.tsv`, pods assigned
outside `selected-node-names.txt`, missing artifacts, wrong RuntimeClass
handlers, incomplete per-node exec proof, incomplete Job completions,
Kubernetes `Warning` events in `events.tsv`, and nonzero generated workload
counts or timestamps earlier than the final resource sample in the single-row
`post-cleanup-counts.tsv` when `--cleanup` is enabled.
Structural Kubernetes artifacts, including post-cleanup listings, must be real
command output, not captured `kubectl` connection or API errors.
Use `--preflight-only` first when checking cluster
readiness without applying workloads. Pass the runner
`--verify-*` gate options for 2-hour, 24-hour, or 72-hour gates, then use
`deploy/scripts/verify-soak-evidence.sh` with the matching `--min-*` and
`--max-sample-gap-secs` options to re-check saved bundles. Recorded verifier
gates in `metadata.txt` are also enforced during later re-checks, even when the
re-check command omits those options. Successful runner verification, or the
concrete verifier failure, is captured in `verify.out`.
Failed cluster runs also write `summary.txt` with `result=fail`, `exit_code`,
`failed_at`, and `failed_command` so the partial evidence bundle can be triaged
later.

## Architecture

```text
Host
  a3s-box CLI
    state: boxes, images, volumes, networks, audit log under A3S_HOME
    runtime: image store, rootfs builder, VmManager, network backend, TEE client
      |
      | shim process + libkrun
      v
Guest MicroVM
  guest-init (PID 1)
    exec server 4089
    PTY server 4090
    attestation server 4091
    user workload process
```

Vsock/control services:

| Port | Service |
| ---: | --- |
| 4088 | gRPC control / health (guest↔host) |
| 4089 | exec server |
| 4090 | PTY server |
| 4091 | attestation / RA-TLS |
| 4092 | optional sidecar vsock port |

These are **vsock** ports (guest↔host), not host TCP endpoints. For
host-scrapable Prometheus metrics + a health probe, run the monitor with
`a3s-box monitor --metrics-addr 127.0.0.1:9100` (serves `/metrics` and
`/healthz`) — see [`docs/monitor-service.md`](docs/monitor-service.md).

Crates:

| Crate | Purpose |
| --- | --- |
| `core` | Shared config, errors, events, port/network/volume/PTY/DNS/workload types |
| `compat` | Pinned external protocol inventories and compatibility service |
| `runtime` | VM lifecycle, image store, rootfs preparation, Compose, networking, TEE clients |
| `cli` | `a3s-box` command line |
| `shim` | libkrun bridge subprocess |
| `guest/init` | guest PID 1 and guest services |
| `netproxy` | macOS user-space bridge, DNS, inbound TCP, and outbound TCP proxy |
| `cri` | experimental CRI server |
| `sdk` | Rust execution registry abstractions for Box workloads |

## Development and validation

Run checks from `crates/box/src`, not the monorepo root.

```bash
cd crates/box/src
cargo fmt --all
cargo run -p a3s-box-compat --bin a3s-box-e2b-contract -- verify
cargo test -p a3s-box-runtime --lib --quiet
cargo test -p a3s-box-cli --test command_coverage --quiet
cargo test -p a3s-box-cli --test host_smoke --quiet
cargo test -p a3s-box-cli --test core_smoke --quiet
cargo test -p a3s-box-cli --test host_smoke test_real_compose_acl_smoke -- --ignored --exact --nocapture
```

Or run the macOS/Linux validation ladder from `crates/box`:

```bash
cd crates/box
deploy/scripts/soak-evidence-self-test.sh
scripts/host-integration-smoke.sh
```

Opt-in real runtime smoke:

```bash
cd crates/box
A3S_BOX_SMOKE_IMAGE_TAR=/path/to/alpine.tar \
A3S_BOX_SMOKE_TIMEOUT_SECS=300 \
scripts/host-integration-smoke.sh --core
```

Opt-in Linux Dockerfile `RUN` smoke:

```bash
cd crates/box
A3S_BOX_TEST_ALPINE_TAR=/path/to/alpine.tar \
sudo -E scripts/host-integration-smoke.sh --linux-run --no-pure
```

The Linux `RUN` smoke must run as root on a root-capable Linux builder.
See `docs/host-integration.md` for the macOS HVF, Linux KVM, host command
matrix, warm-pool Dockerfile `RUN` smoke, CRI smoke, and host soak procedures.

## Environment variables

| Variable | Description |
| --- | --- |
| `A3S_HOME` | Data directory. Default: `~/.a3s`. |
| `A3S_IMAGE_CACHE_SIZE` | Image cache size. Default: `10g`. |
| `A3S_TEE_SIMULATE` | Enables simulated TEE report behavior. |
| `A3S_REGISTRY_PROTOCOL` | Legacy registry protocol override for local/insecure registry tests. Prefer `a3s-box push --plain-http` for push. |
| `A3S_EXEC_READY_TIMEOUT_MS` | Safety cap for guest exec-server readiness probing during boot. Default: 15000. |
| `A3S_VIRTIOFS_CACHE` | Process-wide fallback virtio-fs cache mode for host directory volumes: `none` by default, or `auto`, `always`, `default`. Prefer per-run `--virtiofs-cache` when scripting release verification. |
| `A3S_BOX_CRI_AGENT_IMAGE` | Default CRI sandbox agent/rootfs image. |
| `A3S_BOX_SMOKE_IMAGE_TAR` | OCI archive used by the ignored core MicroVM smoke suite. |
| `A3S_BOX_TEST_ALPINE_TAR` | Shared offline Alpine OCI archive for core and host smoke suites. |
| `A3S_BOX_ALLOW_REGISTRY_PULL` | Set to `1` to let the host integration runner use live registry pulls when no OCI archive is provided. |
| `A3S_BOX_HOST_SMOKE_TIMEOUT_SECS` | Boot timeout override for ignored host smoke tests. |
| `A3S_BOX_SOAK_DURATION_SECS` | Default duration for `scripts/host-integration-smoke.sh --soak`. Default: 7200. |
| `A3S_BOX_SOAK_ITERATIONS` | Optional iteration cap for `--soak`; `0` means time-based only. |
| `A3S_BOX_SOAK_OUTPUT_DIR` | Evidence directory for `--soak` metadata, resource samples, logs, and CLI snapshots. |
| `A3S_BOX_SOAK_VERIFY_MIN_DURATION_SECS` | Optional host soak evidence gate for minimum `summary.txt` duration. |
| `A3S_BOX_SOAK_VERIFY_MIN_SAMPLES` | Optional host soak evidence gate for minimum resource sample count. |
| `A3S_BOX_SOAK_VERIFY_MIN_SAMPLE_SPAN_SECS` | Optional host soak evidence gate for first-to-last sample span. |
| `A3S_BOX_SOAK_VERIFY_MAX_SAMPLE_GAP_SECS` | Optional host soak evidence gate for maximum consecutive sample gap. |
| `A3S_BOX_CLUSTER_SOAK_JOBS` | RuntimeClass cluster soak job completions for `deploy/scripts/runtimeclass-soak.sh`. Default: 500. |
| `A3S_BOX_CLUSTER_SOAK_PARALLELISM` | RuntimeClass cluster soak Job parallelism. Default: 25. |
| `A3S_BOX_CLUSTER_SOAK_DURATION_SECS` | RuntimeClass cluster observation window after workload submission. Default: 7200. |
| `A3S_BOX_CLUSTER_SOAK_OUTPUT_DIR` | Evidence directory for RuntimeClass cluster soak bundles. |
| `A3S_BOX_CLUSTER_SOAK_VERIFY_MIN_DURATION_SECS` | Optional RuntimeClass soak evidence gate for minimum `summary.txt` duration. |
| `A3S_BOX_CLUSTER_SOAK_VERIFY_MIN_SAMPLES` | Optional RuntimeClass soak evidence gate for minimum resource sample count. |
| `A3S_BOX_CLUSTER_SOAK_VERIFY_MIN_SAMPLE_SPAN_SECS` | Optional RuntimeClass soak evidence gate for first-to-last sample span. |
| `A3S_BOX_CLUSTER_SOAK_VERIFY_MAX_SAMPLE_GAP_SECS` | Optional RuntimeClass soak evidence gate for maximum consecutive sample gap. |
| `A3S_BOX_CLUSTER_SOAK_CLEANUP_TIMEOUT_SECS` | RuntimeClass cleanup wait before collecting `post-cleanup-counts.tsv`. Default: 300. |
| `A3S_BOX_BUILDKIT_IMAGE` | BuildKit image used by `--builder=buildkit-vm`. Default: `moby/buildkit:latest`. |
| `A3S_BOX_BUILDKIT_CPUS` | CPU count for the BuildKit VM helper box. Default: `4`. |
| `A3S_BOX_BUILDKIT_MEMORY` | Memory limit for the BuildKit VM helper box. Default: `8g`. |
| `A3S_BOX_RUN_POOL_SOCKET` | Auto-route compatible foreground `a3s-box run --rm` commands through the warm-pool daemon at this socket. Explicit `--pool-socket` still applies when `--pool` is passed. |
| `A3S_BOX_BUILD_RUN_POOL_SOCKET` | Enable Dockerfile `RUN` warm-pool execution and use this pool daemon socket, equivalent to `a3s-box build --run-pool-socket <path>`. |
| `A3S_BOX_BUILD_RUN_CACHE_DIR` | Override the persistent Dockerfile `RUN --mount=type=cache` directory used by the warm-pool build path. Defaults to `~/.a3s/buildcache/run-cache`. |
| `A3S_BOX_UNSAFE_HOST_RUN` | Opt into unsafe macOS host execution for Dockerfile `RUN` experiments. |
| `A3S_BOX_BUILDCACHE_MAX_BYTES` | Cap on the total size of cached build layers at `~/.a3s/buildcache` (oldest evicted first). Default: 2 GiB. |
| `A3S_BOX_MAX_LAYER_BYTES` | Cap on total decompressed bytes per OCI image layer during `pull` (decompression-bomb guard). Default: 16 GiB. |
| `A3S_BOX_MAX_BUILD_EXTRACT_BYTES` | Cap on total decompressed bytes when a build `ADD`/`COPY` auto-extracts a local tar archive (decompression-bomb guard). Default: 4 GiB. |
| `A3S_BOX_MAX_SNAPSHOTS` | Auto-prune on every `snapshot create` to keep at most N newest snapshots per box (unset = unbounded). |
| `A3S_BOX_MAX_SNAPSHOT_BYTES` | Auto-prune on every `snapshot create` to keep snapshots under a total byte cap (unset = unbounded). |
| `A3S_BOX_SECCOMP_PROFILE_ROOT` | Root directory a CRI `localhostProfile` seccomp path is confined to (paths outside it, or containing `..`, are rejected). Default: `/var/lib/kubelet/seccomp`. |
| `A3S_REGISTRY_MIRRORS` | Registry mirror map (`host=mirror,host=mirror`); pulls fetch layers/manifests from the mirror while keeping the canonical image reference. |
| `KRUN_SNAPSHOT_MEM_FILE` | Path the booted template writes its file-backed guest RAM to when capturing a snapshot-fork template. |
| `KRUN_SNAPSHOT_SOCK` | Control socket the template listens on for the `snapshot <path>` command (Linux `/dev/kvm` only). |
| `KRUN_RESTORE_FROM` | Path to a snapshot the microVM restores from as a Copy-on-Write fork instead of cold booting. |
| `RUST_LOG` | Rust tracing log level. |

## License

MIT
