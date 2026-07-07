# a3s-box benchmarks

The performance and "leak-free" claims in the README/CHANGELOG (cold boot,
snapshot-fork, warm-pool acquire, churn leak-freeness) were previously prose
with no reproducible source. [`bench.sh`](./bench.sh) makes them **independently
reproducible**: it drives the real `a3s-box` CLI end-to-end and reports
wall-clock latencies plus a hard leak assertion — so anyone can re-measure on
their own hardware instead of trusting a number in a doc.

## Requirements

A Linux host with **`/dev/kvm`** (real microVMs only boot there) and `a3s-box`
on `PATH` (or set `A3S_BOX`). The boot benchmarks are meaningless without KVM.

## Usage

```bash
bench/bench.sh            # all four benchmarks
bench/bench.sh cold       # cold-boot latency only
bench/bench.sh warm       # warm-pool acquire latency
bench/bench.sh fork       # snapshot-fork pool fill (cold-fill vs CoW restore)
bench/bench.sh leak       # churn + leak assertion (exit != 0 on leak)
PNPM_PROJECT=/path/to/app bench/bench.sh pnpm
```

Tunables (env):

| Var | Default | Meaning |
|-----|---------|---------|
| `A3S_BOX` | `a3s-box` | binary under test |
| `IMAGE` | `alpine:latest` | OCI image to benchmark |
| `RUNS` | `20` | samples per latency benchmark |
| `POOL_SIZE` | `16` | warm-pool / fork fill size |
| `CHURN` | `30` | create/run/remove cycles for the leak test |
| `PNPM_PROJECT` | unset | project directory with `package.json` and `pnpm-lock.yaml` for `pnpm` mode |
| `PNPM_IMAGE` | `node:22-alpine` | Node image used by `pnpm` mode |
| `PNPM_VERSION` | `10.30.3` | version passed to `corepack prepare` |
| `PNPM_RUNS` | `3` | samples for the `pnpm` benchmark |
| `PNPM_CACHE` | `1` | use `--package-cache pnpm`; set `0` for a cold store path |

## What it measures

- **cold** — `run --rm IMAGE -- true` wall-clock, reported as p50 / p90 / min
  over `RUNS` samples.
- **warm** — `pool start` then `pool run` acquire latency (p50 / p90 / min).
- **fork** — `pool start --size N` fill time **without** vs **with**
  `--snapshot-fork`, as total + amortized-per-VM, so the CoW speedup is a
  measured ratio rather than an asserted one.
- **leak** — snapshots host-side resources a leak would grow (orphan
  `a3s-box-shim` processes, overlay mounts under `~/.a3s/boxes`, box dirs),
  runs `CHURN` `run --rm` cycles, then asserts they return to baseline.
  **Exits non-zero on any leak**, so it is CI-gateable.
- **pnpm** — runs `node:22-alpine` against a real project mount and reports
  p50/p90 for VM boot baseline, `corepack + pnpm` toolchain setup, and
  `pnpm install --frozen-lockfile`. The final line gives a rough p50 breakdown:
  boot, toolchain, and the remaining install/network/filesystem work. Compare
  `PNPM_CACHE=0` with the default `PNPM_CACHE=1` to quantify the persistent
  pnpm store path.

## Wiring into CI

The leak assertion's non-zero exit makes it a natural gate on the self-hosted
KVM runner (see [`../docs/ci-kvm-runner.md`](../docs/ci-kvm-runner.md)): add a
`bench/bench.sh leak` step to the `integration-kvm` job to catch a resource leak
regression automatically, instead of relying on a manual churn run.

## Updating the published numbers

When you quote a number in the README, regenerate it here first and paste the
harness output. A claim with a reproducible command behind it is worth more than
a polished number with no source.
