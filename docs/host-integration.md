# Host Integration Smoke Guide

This guide defines the macOS and Linux validation path for a3s-box. Run these
commands from the crate repository root (`crates/box`), not from the monorepo
root.

Use the [Cross-Capability Soak Test Plan](soak-test-plan.md) to decide which
capability scenarios and `R0`, `G2`, `R24`, or `E72` profile a change requires.
This guide supplies host commands; it does not by itself prove that every
capability scenario has soak coverage.

## Validation ladder

| Level | Host requirements | Command |
| --- | --- | --- |
| Stub baseline | macOS or Linux with Rust, C compiler, and protoc | `scripts/host-integration-smoke.sh` |
| Core MicroVM smoke | macOS Apple Silicon/HVF or Linux KVM, libkrun, Linux guest init, runnable image | `scripts/host-integration-smoke.sh --core` |
| Native local SDK smoke | Same as the selected MicroVM or certified Linux Sandbox backend, plus Python 3.10+, Node.js 20+, and Go 1.25+ | `scripts/local-sdk-smoke.sh microvm` or `scripts/local-sdk-smoke.sh sandbox` |
| Host command and warm-pool smoke | Same as core smoke; optional registry credentials for push coverage | `scripts/host-integration-smoke.sh --host` |
| Linux Dockerfile `RUN` | Linux, root, chroot-capable filesystem, local Alpine OCI archive | `sudo -E scripts/host-integration-smoke.sh --linux-run --no-pure` |
| CRI smoke | macOS or Linux MicroVM host, `crictl`, CRI images | `scripts/host-integration-smoke.sh --cri` |
| Host soak | Same as the selected host-backed suites; enough time to expose leaks and lost updates | `scripts/host-integration-smoke.sh --no-pure --core --host --soak` |
| Production cluster validation | Explicitly enrolled production Linux nodes with `/dev/kvm`, containerd RuntimeClass wiring, labels, taints, and rollback prepared | See [`production-cluster-tests.md`](./production-cluster-tests.md) |
| Cross-capability promotion | The host and isolation lanes required by the affected public capabilities | See [`soak-test-plan.md`](./soak-test-plan.md) |

The default command runs formatting, clippy, unit tests, and integration test
compilation with `A3S_DEPS_STUB=1`. It does not require a hypervisor and should
be safe on developer laptops and CI workers. Host-backed `--core` and `--host`
runs require an OCI archive by default; set `A3S_BOX_ALLOW_REGISTRY_PULL=1` only
when you intentionally want live registry pulls.

## macOS core smoke

Use Apple Silicon. Intel macOS is not a supported runtime target.

```bash
cd crates/box

# Optional but recommended for offline/reproducible runs.
export A3S_BOX_TEST_ALPINE_TAR=/path/to/alpine-oci.tar
export A3S_BOX_SMOKE_IMAGE_TAR="$A3S_BOX_TEST_ALPINE_TAR"
export A3S_BOX_SMOKE_SKIP_PULL=1
export A3S_BOX_SMOKE_TIMEOUT_SECS=300

scripts/host-integration-smoke.sh --core
```

If you do not have an offline archive and want to pull from the registry during
the run, add:

```bash
export A3S_BOX_ALLOW_REGISTRY_PULL=1
```

The host runner rebuilds guest init for the matching static Linux musl target on
both macOS and Linux before every real suite. Install that Rust target first if
it is missing; for example, on Apple Silicon or Linux arm64:

```bash
rustup target add aarch64-unknown-linux-musl
scripts/host-integration-smoke.sh --core
```

If direct cross-build linking fails on the host, install `cargo-zigbuild` and
use `cargo zigbuild -p a3s-box-guest-init --target aarch64-unknown-linux-musl`
instead.

On macOS, `src/target/debug/a3s-box-guest-init` is a host Mach-O binary; on
Linux it is normally a dynamically linked host ELF. Neither is accepted as a
guest PID 1 artifact. The runner builds and selects
`src/target/<linux-musl-target>/debug/a3s-box-guest-init` from the current
checkout.

## Linux core smoke

Use a host with `/dev/kvm` available to the current user. For offline runs, use
the same OCI archive variables as macOS.

```bash
cd crates/box
export A3S_BOX_TEST_ALPINE_TAR=/path/to/alpine-oci.tar
export A3S_BOX_SMOKE_IMAGE_TAR="$A3S_BOX_TEST_ALPINE_TAR"
export A3S_BOX_SMOKE_SKIP_PULL=1

scripts/host-integration-smoke.sh --core
```

Use `A3S_BOX_ALLOW_REGISTRY_PULL=1` instead of the archive variables only for
network-backed validation; offline archive runs are the release gate default.

If `/dev/kvm` is permission denied, add the user to the `kvm` group and start a
new login session:

```bash
sudo usermod -aG kvm "$USER"
```

## Native local SDK matrix

The native Rust, synchronous/asynchronous Python, TypeScript, and Go SDK smoke
exercises the same local `Sandbox`, commands, files, process inventory,
normalized runtime statistics, bounded event replay, backpressured continuous
event streams, replay-safe resource updates, pause/resume, kill, and cleanup
behavior against a real backend. Each stream is opened before an idempotently
replayed resource update and must yield the same sole `resources-updated` event
later proven by the bounded replay. Before
starting the box it also verifies the bridge capability inventory, built-image
get/inspect/history/tag/remove calls, and unused volume/network pruning; cache
eviction is exercised after cleanup.

Build the host binary, shim, and matching Linux guest init first. Then use a
dedicated state directory:

```bash
export A3S_HOME=/var/tmp/a3s-local-sdk-smoke

# Development-tree override only; installed packages normally find a3s-box on PATH.
export A3S_BOX_BINARY="$PWD/src/target/debug/a3s-box"

scripts/local-sdk-smoke.sh microvm
```

On a supported Linux shared-kernel host, the A3S Box release archive and
Homebrew package install the matching `a3s-oci` and `a3s-oci-agent` artifacts
beside `a3s-box`. Installed users do not set runtime paths. For a development
checkout, build the pinned OCI Runtime revision and point the smoke gate at the
two resulting artifacts:

```bash
git clone https://github.com/A3S-Lab/OCI-Runtime.git /tmp/a3s-oci-runtime
git -C /tmp/a3s-oci-runtime checkout 0f0face67c282cf535e352b4fe83af02055b7e08
cargo build --manifest-path /tmp/a3s-oci-runtime/Cargo.toml \
  --locked --release -p a3s-oci-cli -p a3s-oci-agent

export A3S_BOX_OCI_RUNTIME_PATH=/tmp/a3s-oci-runtime/target/release/a3s-oci
export A3S_BOX_OCI_AGENT_PATH=/tmp/a3s-oci-runtime/target/release/a3s-oci-agent
scripts/local-sdk-smoke.sh sandbox
```

A3S OCI Runtime is the sole shared-kernel Sandbox backend. Production
configuration has no backend override, and release archives contain only the
pinned `a3s-oci` and `a3s-oci-agent` artifacts for Sandbox execution.

`A3S_BOX_BINARY` is only an optional executable-path override for development;
it is not a service endpoint or credential. Normal local SDK consumers set
none of these variables.

## Resilient registry pull validation

The deterministic fault-injection suite proves interrupted-body Range resume,
no-progress timeout, the exact retry bound, layer concurrency, actual-byte
progress, verified cross-image reuse, and corrupt-candidate fallback without
depending on an external registry:

```bash
cargo test -p a3s-box-runtime resilient_pull_tests
```

Run a separate live-registry smoke on an enrolled Linux host before release.
Use a dedicated state directory so the result cannot be satisfied by an
unrelated cached manifest:

```bash
export A3S_HOME=/var/tmp/a3s-box-registry-smoke
export A3S_BOX_REGISTRY_SMOKE_IMAGE=ghcr.io/example/large-image:release-candidate
export A3S_REGISTRY_PULL_MAX_ATTEMPTS=4
export A3S_REGISTRY_PULL_RETRY_INITIAL_MS=250
export A3S_REGISTRY_PULL_RETRY_MAX_MS=4000
export A3S_REGISTRY_PULL_NO_PROGRESS_TIMEOUT_SECS=30
export A3S_REGISTRY_PULL_MAX_CONCURRENT=4

timeout 900 a3s-box pull "$A3S_BOX_REGISTRY_SMOKE_IMAGE"
```

The command must either finish with verified content or fail within the
configured attempt and no-progress bounds with the downloaded offset in its
error. Progress output must show actual transferred bytes rather than only the
declared layer size. If the registry interrupts a response, the next request
must use the persisted offset; if it does not support Range, the client must
restart that blob safely. Keep authentication and redirect checks in the
fixture suite because credentials must never be forwarded to another origin.

## Linux Dockerfile `RUN` smoke

Dockerfile `RUN` uses an isolated Linux chroot path. It is intentionally
Linux-only and requires root. The smoke test must use a local Alpine OCI
archive because it validates the chroot build path, not registry access.

```bash
cd crates/box
sudo -E env A3S_BOX_TEST_ALPINE_TAR=/path/to/alpine-oci.tar \
  scripts/host-integration-smoke.sh --linux-run --no-pure
```

macOS does not run Dockerfile `RUN` on the host. The native build engine uses
its isolated warm-pool path instead: start a pool daemon, then pass `--run-pool`
(or set
`A3S_BOX_BUILD_RUN_POOL_SOCKET`). The build stage rootfs is mounted into a
leased pool VM and each shell/exec-form `RUN` executes through the guest exec
server with the current Dockerfile `WORKDIR`, `ENV`, and `USER`:

```bash
a3s-box pool start --image alpine:latest --size 1 --socket /tmp/a3s-build-pool.sock
a3s-box build --run-pool --run-pool-socket /tmp/a3s-build-pool.sock \
  -t a3s/web:v1 .

# Or let the build command start the helper daemon explicitly.
a3s-box build --run-pool-autostart \
  --run-pool-image alpine:latest \
  -t a3s/web:v1 .
```

The target platform must match the helper VM architecture until the native
engine advertises an explicit emulation capability. Publish only after the
local build succeeds:

```bash
a3s-box tag a3s/web:v1 10.0.0.2:5000/a3s/web:v1
a3s-box push --plain-http 10.0.0.2:5000/a3s/web:v1
```

`RUN --mount=type=cache` is available on this path as a persistent overlay:
writes under the cache target are visible to matching `RUN` commands keyed by
`id=` (or by `target=` when `id` is omitted) but are restored before layer
diffing, so cache contents are not committed to the image. Docker/BuildKit's
default omitted `sharing=shared` and explicit `sharing=shared` are accepted, as
is `sharing=locked`; because the warm-pool overlay hydrates and publishes cache
directories around each RUN, access to the same cache key is serialized across
builds to avoid writeback races. Successful RUNs publish cache writes; failed
RUNs restore the rootfs without publishing partial cache contents. New cache
directories can be seeded from `from=<stage-or-image>,source=<dir>`; an existing
cache is not re-seeded. Cache-root `mode=`, `uid=`, and `gid=` are applied when
present. `sharing=private` remains unsupported.
Set
`A3S_BOX_BUILD_RUN_CACHE_DIR` to override the default
`~/.a3s/buildcache/run-cache` location.

The build rootfs volume is part of the pool key. Because that path is unique to
one build stage and is destroyed after the stage completes, the daemon fills
volume-bound build leases on demand (`min_idle=0`) instead of pre-warming a full
idle pool for every temporary stage rootfs.

`RUN --mount=type=bind` is also available for build-context sources, previous
build stages, and external images on the warm-pool path. Omitted `source=`
mounts the context root, relative `target=` paths resolve from the current
Dockerfile `WORKDIR`, `.dockerignore` is honored for context sources, stage/image
sources ignore `.dockerignore`, and writes under the bind target are discarded
before layer diffing.

`RUN --mount=type=tmpfs` creates an empty temporary target for the duration of a
RUN, restores any original target contents afterwards, and discards tmpfs writes
before layer diffing. Relative `target=` paths resolve from `WORKDIR`. The
Docker/BuildKit `size=` option is rejected until the warm-pool overlay can
enforce it honestly.

`RUN --network=default` and `RUN --security=sandbox` are accepted as Docker's
default no-op values. Non-default per-RUN network/security modes are rejected
until the warm-pool exec path can enforce them.

The unsafe host execution path is only for local experiments and requires
`A3S_BOX_UNSAFE_HOST_RUN=1`; it is not part of the product smoke matrix.

## Large Workspace Verification

For package-manager-heavy monorepo checks, prefer an explicit cache profile
instead of a raw host mount:

```bash
a3s-box run --rm --timeout 120 --cpus 4 --memory 8g \
  --package-cache pnpm \
  --virtiofs-cache=always \
  -v "$PWD:/workspace" \
  -w /workspace \
  --tmpfs /workspace/node_modules:size=4g \
  node:24-bookworm -- \
  sh -lc 'corepack enable && corepack prepare pnpm@11.10.0 --activate && pnpm --filter @a3s-lab/ui build'
```

`--package-cache pnpm` keeps the pnpm store, Corepack home, pnpm home, and npm
cache in the named `a3s-cache-pnpm` volume across `--rm` runs.
For npm-only checks, use `--package-cache npm` to keep the npm cache in
`a3s-cache-npm`.
Non-interactive `run` commands also default
`COREPACK_ENABLE_DOWNLOAD_PROMPT=0`, with an explicit `--env` value taking
precedence. Image pulls report layer progress, and runtime startup emits a
15-second stderr heartbeat after the image is ready so external CI timeouts can
distinguish registry work from VM or Sandbox startup.
`--tmpfs .../node_modules` prevents large dependency trees from being written
through the host workspace mount. `--virtiofs-cache=always` is intended for
release verification jobs where the host checkout is stable for the duration of
the run; omit it or use `none` when host-side edits must be visible immediately.

The software behavior reported by the July 2026 production workflows now has
repository regression coverage:

| Report | Covered behavior |
| --- | --- |
| [#164](https://github.com/A3S-Lab/Box/issues/164) | macOS netproxy has end-to-end outbound TCP and DNS-response tests; `wait` and `compose wait` add a command-wide timeout, periodic progress, and a non-zero bounded failure without terminating the workload. |
| [#165](https://github.com/A3S-Lab/Box/issues/165) | Non-interactive normal and pooled runs suppress the Corepack download prompt unless explicitly overridden, while cold runtime startup emits phase-specific progress. |
| [#166](https://github.com/A3S-Lab/Box/issues/166) | Dockerfile `WORKDIR` and declared/default/overridden `ARG` values reach `RUN`; a cached `scratch` rebuild must still export its copied layer; registry push exposes `--plain-http`, `--insecure`, and `--tls-verify=false`. |
| [#167](https://github.com/A3S-Lab/Box/issues/167) | Foreground `run --rm` handles `SIGINT`/`SIGTERM`, removes the workload, preserves the signal-derived exit status, and lets a later bounded `wait` recover that archived status. |
| [#204](https://github.com/A3S-Lab/Box/issues/204) | The physical Apple Silicon/HVF gate drives two authenticated PostgreSQL SCRAM-SHA-256 sessions through one published port, queries both sessions, verifies the guest remains responsive, and rejects `ENOBUFS` or `NETDEV WATCHDOG` shim logs. |

The deterministic CLI lane runs the build/export and wait regressions on every
push. Real Unix signal cleanup remains part of the host-backed core smoke.
These tests do not replace the issue-specific production-host reruns requested
in [#164](https://github.com/A3S-Lab/Box/issues/164),
[#165](https://github.com/A3S-Lab/Box/issues/165),
[#166](https://github.com/A3S-Lab/Box/issues/166), and
[#167](https://github.com/A3S-Lab/Box/issues/167), and the physical-HVF
PostgreSQL run required by [#204](https://github.com/A3S-Lab/Box/issues/204);
record those runs before closing the reports.

For repeated short checks, run a warm-pool daemon with the same image, resource
shape, and workspace mount, then either pass `--pool` explicitly or export
`A3S_BOX_RUN_POOL_SOCKET` so compatible foreground `run --rm` commands use the
daemon automatically:

```bash
a3s-box pool start --image node:24-bookworm --size 2 --max 4 \
  --boot-concurrency 2 \
  --socket /tmp/a3s-node-pool.sock

export A3S_BOX_RUN_POOL_SOCKET=/tmp/a3s-node-pool.sock
a3s-box run --rm --cpus 4 --memory 8g \
  --package-cache pnpm \
  -v "$PWD:/workspace" \
  -w /workspace \
  node:24-bookworm -- \
  sh -lc 'corepack enable && pnpm --version'

a3s-box pool stop --socket /tmp/a3s-node-pool.sock
```

To inspect cold-start cost, expose the pool daemon's optional metrics endpoint:

```bash
a3s-box pool start --image node:24-bookworm --size 2 \
  --metrics-addr 127.0.0.1:9101
curl -s http://127.0.0.1:9101/metrics
```

`a3s_box_vm_boot_duration_seconds` reports the end-to-end boot histogram and
`a3s_box_vm_boot_phase_duration_seconds{phase=...}` breaks it down into the
bounded `capability`, `layout`, `spec`, `rootfs`, `prepare`, `launch`, and
`readiness` phases (only phases used by a selected backend emit samples). The
phase label is deliberately stable, so it is safe to aggregate in Prometheus
without introducing image or box-id cardinality.

`--boot-concurrency` bounds simultaneous VM boots during initial fill and
background replenishment. The default is `2`; lower it on CPU-constrained
hosts, or raise it only after measuring host CPU and memory headroom.
`a3s_box_warm_pool_boots_inflight` exposes the current aggregate boot pressure,
while `a3s_box_warm_pool_boot_failures_total` counts failed warm-pool and
on-demand pool-miss boots. Use these alongside the boot phase histograms when
diagnosing host CPU saturation or replenishment churn.

For an explicit one-command local loop, `a3s-box run --pool-autostart --rm ...`
starts a daemon on `--pool-socket` if none is already running. Foreground
`--timeout` is passed through to the warm-pool exec request.

`pool status` reports idle sandboxes, active checked-out sandboxes, and active
leases for each pool key. During `build --run-pool`, a nonzero leased count means
a Dockerfile stage currently holds a helper VM. `a3s-box info` performs the same
best-effort daemon probe against the configured run/build pool sockets and the
default socket, then prints aggregate max/idle/active/leased counts when a daemon
is reachable. `pool start --lease-ttl <duration>` is the abandoned-lease
guardrail for daemon-backed build leases; it reclaims only idle leases, never a
lease with an exec currently running. The default is `1h`; use `0` to disable it
for long manual debugging sessions.

For cold package stores, registry downloads and project-level supply-chain
policy checks can still dominate the first run. Prime the named cache volume
before a release window when the monorepo depends on thousands of packages.

## Host command matrix

The host matrix extends the core smoke with VM lifecycle commands, canonical
`compose.acl` discovery and teardown, copy, stats, snapshots, network
operations, image tagging/saving, local build, and optional registry push
coverage.

```bash
cd crates/box
export A3S_BOX_TEST_ALPINE_TAR=/path/to/alpine-oci.tar
export A3S_BOX_HOST_SMOKE_TIMEOUT_SECS=300

scripts/host-integration-smoke.sh --host
```

Enable registry push coverage only against a disposable tag template:

```bash
export A3S_BOX_PUSH_TEST_REF='registry.example/a3s/box-push-test:{tag}'
export A3S_BOX_PUSH_USERNAME='...'
export A3S_BOX_PUSH_PASSWORD='...'
scripts/host-integration-smoke.sh --host
```

## CRI smoke

The CRI smoke is experimental and intentionally opt-in. It starts the
`a3s-box-cri` server, drives it through `crictl`, and launches a pod sandbox
with two containers.

```bash
cd crates/box
export A3S_BOX_CRI_CRICTL=/path/to/crictl
export A3S_BOX_CRI_SMOKE_IMAGE=busybox:latest
export A3S_BOX_CRI_SMOKE_AGENT_IMAGE=ghcr.io/a3s-box/code:v0.1.0

scripts/host-integration-smoke.sh --cri
```

Use `A3S_BOX_CRI_SMOKE_SKIP_PULL=1` and `A3S_BOX_CRI_SMOKE_IMAGE_DIR` when the
image store is preloaded and the run must stay offline.

## Host soak

Use `--soak` after the single-pass host-backed suites are already green. The
runner repeats the selected real suites, runs `bench/bench.sh leak` and
`bench/bench.sh race` by default, samples host resource counts, and writes an
evidence directory under `src/target/a3s-box-soak/`.

When `A3S_BOX_TEST_ALPINE_TAR` or `A3S_BOX_SMOKE_IMAGE_TAR` is set, the runner
reloads that archive with the configured `IMAGE` tag before each benchmark
step. This keeps the leak and race gates reproducibly offline even when an
earlier host suite intentionally runs `system-prune --all`.

```bash
cd crates/box
export A3S_BOX_TEST_ALPINE_TAR=/path/to/alpine-oci.tar
export A3S_BOX_SMOKE_SKIP_PULL=1
export A3S_BOX_HOST_SMOKE_TIMEOUT_SECS=300
export IMAGE=docker.m.daocloud.io/library/alpine:latest
export CHURN=2500
export RACE=32
export RACE_WORKLOAD_SECS=300

scripts/host-integration-smoke.sh \
  --no-pure \
  --core \
  --host \
  --soak \
  --soak-duration 7200 \
  --soak-verify-min-duration-secs 7200 \
  --soak-verify-min-sample-span-secs 7200 \
  --soak-verify-min-samples 4
```

For a short rehearsal, cap the loop instead of waiting for the time limit:

```bash
scripts/host-integration-smoke.sh \
  --no-pure \
  --core \
  --host \
  --soak \
  --soak-iterations 1 \
  --soak-duration 0
```

The evidence directory contains `metadata.txt`, `resource-samples.tsv`, per-step
iteration logs, CLI state snapshots, `summary.txt`, and `verify.out`. Keep the
directory with the release candidate when the soak is used as a gate. The runner
verifies the bundle before returning success, including resource counters and
required snapshot/log files. `metadata.txt` must include parseable `started_at`,
non-negative integer soak gate fields, and `selected_suites` flags for `core`,
`host`, `linux_run`, `cri`, and `bench`; every selected suite must have its
corresponding per-iteration log, with bench requiring both `bench-leak` and
`bench-race` logs. `resource-samples.tsv` timestamps must be parseable and
monotonic, counters must be non-negative integers, there must be exactly one
`start` row and one `final` row, and `summary.txt` duration must not be shorter
than the sampled time span. Pass the `--soak-verify-*` options on release-gate
runs so the runner also enforces minimum duration, sample span, and sample count
before returning success. Saved bundles keep those gate values in `metadata.txt`,
so later verifier runs enforce the recorded gates even when the re-check command
does not repeat every `--min-*` option.
Failed host soaks write `result=fail` plus the failed
iteration count and, when available, `exit_code`, `failed_at`, and
`failed_command`; to re-check a saved bundle, run:

```bash
deploy/scripts/verify-soak-evidence.sh \
  --kind host \
  --min-duration-secs 7200 \
  --min-sample-span-secs 7200 \
  --min-samples 4 \
  <evidence-dir>
```

Before using the verifier as a release gate after script changes, run the local
self-test:

```bash
deploy/scripts/soak-evidence-self-test.sh
```

### Windows WHPX soak

Windows uses a native PowerShell runner because the Linux host and cluster
resource samplers do not describe WHPX process ownership or supported feature
boundaries. Run it on an otherwise idle host:

```powershell
.\scripts\windows-whpx-soak.ps1 `
  -ImageTar C:\images\alpine-3.20.tar `
  -Iterations 0 `
  -DurationSeconds 7200
```

The twelve-test default matrix contains only Windows-supported real tests,
including non-interactive post-boot exec, bidirectional single-file copy,
`top`, guest PID-aware stats, a 4,096-byte workload argument, a read-only
volume-provided init script with success and exact failure paths, POSIX
ownership and mode replay through restart and commit, and the full 2,048-file,
five-pass virtio-fs stress.
It writes per-test logs plus `summary.json`, operation/resource TSVs,
start/final process inventories, and `verify.out` beneath
`src/target/a3s-box-whpx-soak/`. It fails on count drift, resource guardrails,
or any residual A3S Box/A3S OCI Runtime CLI, VM shim, or forwarding worker.
Use `-ListTests` to inspect the matrix. `-SkipBuild` is intended only when the
matching musl guest-init and Windows binaries were already built from the same
commit.

## Result recording

When a host-backed run passes, record:

- host OS and architecture;
- `a3s-box info` output;
- exact command and environment variables;
- image archive digest or registry image digests;
- test summary line from Cargo.

Keep macOS HVF and Linux KVM records separate because bridge networking and
Dockerfile `RUN` behavior intentionally differ by platform.

## Production Cluster Validation

The single-host ladder above is the prerequisite for production-cluster testing.
For production Linux servers, use
[`production-cluster-tests.md`](./production-cluster-tests.md). It adds the
cluster safety model, node admission checklist, RuntimeClass smoke, integration
matrix, 2-hour guardrail soak, 24-hour release soak, 72-hour endurance soak,
stop conditions, and evidence bundle required before widening RuntimeClass use.
The cluster RuntimeClass churn loop is executable through
`deploy/scripts/runtimeclass-soak.sh`; saved host or cluster evidence bundles
can be re-checked with `deploy/scripts/verify-soak-evidence.sh`. Cluster
evidence is accepted only when the final state has selected nodes, completed
jobs, no failed pods/jobs, no unexpected pod restarts, and no Pending/Unknown
pods or active jobs left behind. The `final-pod-runtimeclasses.tsv` and
`job-runtimeclass.txt` artifacts must also cover the same unique final pod set
and prove `runtimeClassName: a3s-box`; `runtimeclass.yaml` must show the
RuntimeClass object is named `a3s-box`, uses handler `a3s-box`, and keeps
`scheduling.nodeSelector.a3s-box.io/runtime: "true"`;
`smoke-exec.txt` must prove `kubectl exec` succeeds on every selected node;
`complex-exec.txt` must prove exec succeeds against every long-lived workload
(`redis`, `postgres`, `nginx`, and `python`);
`complex-logs.txt` must include workload-prefixed `REDIS_SOAK`, `PG_SOAK`,
`NGINX_SOAK`, and `PY_SOAK` markers;
`job-pod-statuses.tsv` must prove exactly the declared number of Succeeded churn
pods with zero restarts on selected nodes, and those pods must be covered by the
final pod evidence; `job-logs.txt` must include `A3S_BOX_JOB_START`,
`A3S_BOX_JOB_RUNTIME_CLASS=a3s-box`, and `A3S_BOX_JOB_DONE` markers exactly
matching the declared Job completion count;
`selected-node-labels.tsv` must prove every selected node carries the required
production-soak labels; `final-pod-nodes.tsv` must show all final pods stayed on
the unique, count-matched selected node list; and `final-pod-statuses.tsv` must
show only `Running` or `Succeeded` final pods with zero restarts. `events.tsv`
must contain only `Normal` validation namespace events. Structural Kubernetes
artifacts must not contain captured `kubectl` connection or API errors. When
`--cleanup` is enabled, the runner waits up to `--cleanup-timeout` seconds and
`post-cleanup-counts.tsv` must be a single timestamped row, not earlier than the
final resource sample, showing zero generated smoke, complex, and churn
workloads left behind.
