# A3S Box isolation performance data — 2026-07-31

This directory contains the sanitized sample data behind the
[2026-07-31 isolation and mechanism performance report](../../../docs/isolation-performance-report-20260731.md).
The Linux measurements ran on the shared A3S OS production host. The macOS
measurements ran on a separate shared Apple Silicon host and must not be used
for a cross-host hardware ranking.

## Result sets

| Result set | Rows | Pass | Fail | Skip | Important boundary |
| --- | ---: | ---: | ---: | ---: | --- |
| `linux-kvm-standard` | 182 | 181 | 1 | 0 | MicroVM matrix passed; the requested foreground Sandbox preflight failed |
| `linux-sandbox-persistent` | 159 | 153 | 5 | 1 | Bind metadata deletion failed; host networking is not exposed by this Sandbox profile |
| `macos-hvf-microvm` | 170 | 163 | 7 | 0 | TSI reliability and one bind-metadata sample failed |
| `linux-sandbox-foreground-qualification.csv` | 3 | 0 | 3 | 0 | All three foreground lifecycle probes failed during log draining |

Each result-set directory contains:

- `samples.csv` — individual observations;
- `summary.json` — grouped nearest-rank p50/p95 statistics;
- `summary.md` — a generated compact table;
- `metadata.json` — sanitized host, parameter, revision, hash, and resource
  accounting metadata.

[`artifacts.json`](./artifacts.json) records exact Box, OCI Runtime, shim,
guest-init, libkrun, firmware, and image hashes. It also contains SHA-256
digests for every published result file.

## Interpretation rules

- `cold_noop_lifecycle` starts a new execution from a locally loaded image. It
  excludes registry pull and image loading.
- Linux Sandbox foreground failures have no valid performance number. The
  elapsed values in the qualification CSV are time-to-failure only.
- Sandbox persistent start and remove are separate operations and are not
  directly equivalent to the foreground MicroVM lifecycle total.
- TEE rows use `--tee-simulate`; they are not hardware SEV-SNP or TDX
  measurements.
- The macOS host began the run at load averages `5.371 / 9.294 / 10.723` and
  had no CPU affinity control. Treat its absolute values as noisy host-local
  observations.
- Both MicroVM pool matrices left benchmark-owned resources. The metadata
  retains the observed deltas and records the verified operator cleanup to
  zero.

