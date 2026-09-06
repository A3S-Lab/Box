# Local Dify Validation — 2026-09-06

This is a local qualification snapshot, not a cross-platform release or soak
certificate. The workload is Dify 1.16.1 on the locally built Box 3.2.4 package
with the unreleased shutdown, health-worker, and macOS staging fixes.

## Environment and identity

- Source baseline: `fddb5b0df7edf8fd335c6e4b99e56dfce1dc6341`.
- Host: macOS 26.6.2, Apple Silicon `Mac17,6`, 18 logical CPUs, 64 GiB RAM.
- Build toolchain: Rust/Cargo 1.98.1 in the Box `src/` workspace.
- Backend: libkrun/HVF, with signed runtime dylibs and static Linux ARM64 guest
  init. The local version string remains `3.2.4`; no new release was published.
- Installed CLI SHA-256:
  `58b58b5d7294ee86201f18ca4289f9e0deff87958c4feee0c1c891a9fa48bd9b`.
- Installed, entitlement-signed shim SHA-256:
  `51e98eb5b5ab8ac98911f215a9bf64aa1afd5133692aa51ab9e9d0aa86d7e3f1`.

## Deployment and measurements

The local package redeployed the existing Compose project without deleting its
seven named volumes. All 15 long-running services use the new package. The
`init_permissions` one-shot completed with exit code 0; the existing setup state
still reports `finished`. Two superseded local installation directories were
moved to the user's Trash for recoverable cleanup.

| Measurement | Result | Scope |
| --- | --- | --- |
| Unchanged `compose up --detach` | 2.99 s wall time | Cached images; all 15 long-running services reused; permission-init one-shot rerun |
| HTTP health | 12/12 responses were 200; 16.001–24.518 ms | One request approximately every five seconds |
| CPU | 15.24% of one logical CPU | Aggregate CPU-time delta for 15 shims and four health workers over 60.687 s |
| Resident memory | 9,121.97–9,123.88 MiB | Sum of RSS for those 19 processes; includes VM workload memory |
| Process stability | All 19 PIDs unchanged | The same observation window |
| Health persistence | Four healthy services, with advancing timestamps | PostgreSQL, Redis, sandbox, and API, after the Compose launcher exited |
| Host socket directories | 19 before and after isolated HVF regressions | Existing baseline retained; no new directory remained |

Resource sampling started at `2026-09-06T01:01:35.445088Z`. CPU is computed as
`100 × sum(process CPU-time deltas) / elapsed wall time`, not by adding stale
`ps %cpu` snapshots. On this 18-CPU host it is about 0.85% of total logical CPU
capacity. RSS is an aggregate process measure, not proportional set size or
incremental virtualization overhead.

The public web page also returned HTTP 200 after redirects. These are local
idle-workload observations, not throughput or latency-under-load benchmarks.
The 2.99-second result is convergence, not a full cold deployment. No matched
before/after cold-start or memory-reduction percentage is claimed. Roughly
8.91 GiB of RSS remains a resource-optimization target.

## Regression evidence

- `scripts/host-integration-smoke.sh --pure` passed formatting, workspace
  all-target/all-feature clippy with warnings denied, library tests, and
  integration tests. Host-dependent tests remain explicitly ignored in this
  baseline, not silently counted as executed.
- The additional `drain_idle_signals_and_joins_in_progress_maintenance` test
  passed. It holds maintenance cleanup open and verifies that draining signals
  shutdown but cannot return until cleanup completes.
- All 28 shim unit tests passed, including byte-for-byte signed dylib staging,
  versioned aliases, same-file/hard-link protection, and raw-disk ownership.
- Before isolating the immediate lock-release assertion, 100 serial shim-suite
  runs passed but 39 of 100 parallel runs failed. After isolation, all 100
  parallel runs passed. The production disk-lock behavior was not relaxed.
- Final real-HVF health-worker regression passed in 7.01 s. It sends SIGTERM to
  the completed launcher's isolated process group, changes guest readiness,
  and requires a new healthy probe.
- Final real-HVF multi-image warm-pool regression passed in 32.73 s. It covers
  direct and routed pool runs, concurrent requests, lazy initialization, and
  shutdown with no new host socket directory left behind.
- The host soak runner's new independent periodic sampler was exercised during
  a one-iteration real-HVF host matrix (198 s, 160 periodic samples). Every
  resource row had the complete nine-column schema; the host evidence verifier
  passed. It now records Box-process RSS bytes and open-file totals alongside
  shim, mount, box-directory, socket-directory, and A3S-home counters.
- Every installed runtime dylib passed `codesign --verify`; the shim's
  hypervisor entitlement signature also verified.

For focused reproduction after preparing the host assets:

```bash
cd src
export A3S_BOX_TEST_ALPINE_TAR=/path/to/alpine-oci.tar
cargo test --locked -p a3s-box-cli -p a3s-box-shim \
  --test core_smoke --test host_smoke -- --ignored --nocapture --test-threads=1 \
  real_core_detached_health_worker_survives_run_cli_exit test_real_pool_warm_run
```

## Remaining gates

- A matched cold-deployment benchmark and longer steady-state/load observation
  are still needed before claiming startup or memory efficiency improvements.
- This macOS run does not certify Linux KVM, Windows WHPX, or confidential
  hardware. PR #243 still needs its Windows-specific validation after its
  unrelated base clippy failure is corrected.
- Issue #172 remains open for the joint Box/Cloud sole-provider acceptance,
  including Cloud's Docker/Bollard removal, clean-host recovery, and hardware
  confidentiality evidence. Local Dify success does not satisfy that gate.
