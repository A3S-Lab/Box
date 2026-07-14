# a3s-box benchmarks

The performance and "leak-free" claims in the README/CHANGELOG (cold boot,
snapshot-fork, warm-pool acquire, churn leak-freeness) were previously prose
with no reproducible source. [`bench.sh`](./bench.sh) makes them **independently
reproducible**: it drives the real `a3s-box` CLI end-to-end and reports
wall-clock latencies plus a hard leak assertion — so anyone can re-measure on
their own hardware instead of trusting a number in a doc.

## Requirements

Most modes require a Linux host with **`/dev/kvm`** and `a3s-box` on `PATH`
(or set `A3S_BOX`). The `foreground` comparison also supports macOS/HVF, which
is the environment used by the original foreground-latency regression report.

## Usage

```bash
bench/bench.sh            # default Linux/KVM suite, including foreground latency
bench/bench.sh cold       # cold-boot latency only
bench/bench.sh foreground # cached foreground no-op, optionally versus Docker
bench/bench.sh warm       # warm-pool acquire latency
bench/bench.sh fork       # snapshot-fork pool fill (cold-fill vs CoW restore)
bench/bench.sh leak       # churn + leak assertion (exit != 0 on leak)
PNPM_PROJECT=/path/to/app bench/bench.sh pnpm
just bench-pnpm           # reduced pnpm fixture
```

Tunables (env):

| Var | Default | Meaning |
|-----|---------|---------|
| `A3S_BOX` | `a3s-box` | binary under test |
| `IMAGE` | `alpine:latest` | OCI image to benchmark |
| `RUNS` | `20` | samples per latency benchmark |
| `FOREGROUND_RUNS` | `RUNS` | recorded cached foreground no-op samples per runtime |
| `FOREGROUND_WARMUPS` | `1` | warm-up runs per runtime before foreground sampling |
| `FOREGROUND_DOCKER` | `1` | compare Docker when its CLI and daemon are available |
| `FOREGROUND_MAX_P50_MS` | `0` | optional absolute a3s-box p50 gate; `0` reports without gating |
| `FOREGROUND_MAX_DOCKER_RATIO` | `0` | optional p50 ratio gate; `0` reports without gating |
| `POOL_SIZE` | `16` | warm-pool / fork fill size |
| `CHURN` | `30` | create/run/remove cycles for the leak test |
| `PNPM_PROJECT` | unset | project directory with `package.json` and `pnpm-lock.yaml` for `pnpm` mode |
| `PNPM_IMAGE` | `node:22-alpine` | Node image used by `pnpm` mode |
| `PNPM_VERSION` | `10.30.3` | version passed to `corepack prepare` |
| `PNPM_RUNS` | `3` | samples for the `pnpm` benchmark |
| `PNPM_CACHE` | `1` | use `--package-cache pnpm`; set `0` for a cold store path |
| `PNPM_CPUS` | `4` | CPUs assigned to pnpm boxes and Docker baselines |
| `PNPM_MEMORY` | `4g` | memory assigned to pnpm boxes and Docker baselines |
| `PNPM_NODE_MODULES` | `both` | benchmark `project`, `tmpfs`, or `both` `node_modules` targets |
| `PNPM_TMPFS_SIZE` | `4g` | tmpfs size for `/work/node_modules` when tmpfs mode is enabled |
| `PNPM_DOCKER` | `1` | compare Docker cold/hot baselines when Docker is available |
| `PNPM_RESET_A3S_CACHE` | `0` | set `1` to remove `a3s-cache-pnpm` before cold A3S samples |

## What it measures

- **cold** — `run --rm IMAGE -- true` wall-clock, reported as p50 / p90 / min
  over `RUNS` samples.
- **foreground** — the latency-sensitive `run --rm --no-stdin --timeout 180
  IMAGE -- true` path with an explicit warm-up, exact samples, mean, p50, p95,
  and minimum. When Docker is available, the harness runs the matching cached
  Docker no-op and reports the p50 ratio. Set `FOREGROUND_MAX_DOCKER_RATIO` only
  on a stable dedicated runner when the comparison should be a hard gate.
- **warm** — `pool start` then `pool run` acquire latency (p50 / p90 / min).
- **fork** — `pool start --size N` fill time **without** vs **with**
  `--snapshot-fork`, as total + amortized-per-VM, so the CoW speedup is a
  measured ratio rather than an asserted one.
- **leak** — snapshots host-side resources a leak would grow (orphan
  `a3s-box-shim` processes, overlay mounts under `~/.a3s/boxes`, box dirs),
  runs `CHURN` `run --rm` cycles, then asserts they return to baseline.
  **Exits non-zero on any leak**, so it is CI-gateable.
- **pnpm** — runs `node:22-alpine` against a real project mount or the reduced
  fixture at [`fixtures/pnpm`](./fixtures/pnpm). It reports p50/p90 for VM boot,
  `corepack + pnpm` setup, `pnpm fetch` (registry download plus extraction/import
  into the pnpm store),
  offline install to project-mounted `node_modules`, offline install to tmpfs
  `node_modules`, and full `pnpm install --frozen-lockfile`. When Docker is
  available it also reports Docker cold/hot baselines and A3S/Docker ratios.

The pnpm benchmark intentionally separates the two likely slow paths:

- store population: `pnpm fetch --frozen-lockfile` (download plus store extraction);
- filesystem materialization: `pnpm install --offline --ignore-scripts`.

If project-mounted `node_modules` is much slower than tmpfs, the bottleneck is
small-file and metadata traffic through the project mount. Use tmpfs for
throwaway install/build jobs:

```bash
a3s-box run --rm --cpus 4 --memory 4g --package-cache pnpm \
  -v "$PWD:/work" -w /work --tmpfs /work/node_modules:size=4g \
  node:22-alpine -- sh -lc 'corepack enable && pnpm install --frozen-lockfile'
```

For a true cold A3S package-cache sample, run with `PNPM_RESET_A3S_CACHE=1`;
this removes the shared `a3s-cache-pnpm` volume before cold samples, so do not
use it while another box is relying on that cache. The Docker cold baseline uses
only the benchmark-owned `a3s-bench-pnpm-store` volume.

## Wiring into CI

The self-hosted KVM job runs the foreground benchmark with an absolute p50 gate
and the leak assertion with its resource-count gate (see
[`../docs/ci-kvm-runner.md`](../docs/ci-kvm-runner.md)). Keep absolute latency
limits on a stable dedicated runner; use the Docker ratio gate for manual
macOS/HVF comparisons on the host class from issue #33.

## Updating the published numbers

When you quote a number in the README, regenerate it here first and paste the
harness output. A claim with a reproducible command behind it is worth more than
a polished number with no source.
