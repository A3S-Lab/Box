# A3S Box Cross-Capability Soak Test Plan

This document is the authoritative plan for long-running A3S Box validation.
It maps every public capability in the root README to a workload, host class,
evidence contract, and promotion gate. Host-specific execution details remain
in [Host Integration](host-integration.md), [Production Cluster Tests](production-cluster-tests.md),
and [Windows WHPX](windows-whpx.md).

A planned scenario is not proof that a capability is stable. A release claim
requires a real-host evidence bundle produced by the named runner, accepted by
an automated verifier, and retained with the exact release candidate.

## Objectives

The soak program must detect failures that short functional tests rarely find:

- host process, mount, socket, file descriptor, handle, memory, and disk leaks;
- lost state updates, stale generations, unsafe PID reuse, and cleanup races;
- cache corruption, unbounded cache growth, and cross-workload data leakage;
- latency or throughput degradation after warm-up;
- recovery failures after client, shim, daemon, runtime, or host interruption;
- platform-specific regressions across KVM, HVF, WHPX, and certified `crun`;
- divergence between the Rust, Python, and TypeScript native SDKs;
- incomplete cleanup after success, failure, cancellation, and rollback.

The program covers implemented product behavior only. Unsupported combinations
must be exercised as negative tests and must fail before mutating runtime
state.

## Current Baseline

The repository already has useful soak foundations. The cross-capability
program composes and extends them instead of creating a second lifecycle or
evidence system.

| Entry point | Existing coverage | Current limitation |
| --- | --- | --- |
| `scripts/host-integration-smoke.sh --soak` | Repeats real MicroVM core, host command, Dockerfile `RUN`, Compose, CRI, leak, and state-race suites; records host resource counts and verifies evidence | Sampling follows whole-suite iterations; it does not yet report per-capability operation rates, daemon RSS/FD slopes, or independent periodic samples |
| `scripts/local-sdk-smoke.sh` | One real Rust/Python/TypeScript pass for MicroVM or certified Sandbox execution, including builders, lifecycle, resources, diagnostics, and snapshots | Functional smoke only; no duration loop, concurrency mix, cancellation, or longitudinal resource gates |
| `scripts/macos-fault-soak.sh` | Isolated Apple Silicon/HVF lifecycle churn with CLI/shim termination and resource samples | Covers lifecycle recovery, not the complete image/build/storage/network/SDK surface |
| `scripts/windows-whpx-soak.ps1` | Repeats the eleven Windows-supported real tests and rejects residual A3S Box processes | Uses a Windows-specific summary and has no shared cross-capability verifier or long-term handle/RSS slope gate |
| `deploy/scripts/runtimeclass-soak.sh` | RuntimeClass jobs, long-lived services, exec, logs, events, cleanup, node selection, sampling, and verified cluster evidence | Cluster-focused; it does not replace node-local image, build, SDK, warm-pool, or TEE lanes |
| `src/sdk/tests/soak_kvm.rs` | Sustained legacy pipeline fork/snapshot churn, leak checks, and orchestrator RSS bound on real KVM | Covers the optional pipeline layer, not the complete native SDK operation inventory |
| `deploy/scripts/verify-soak-evidence.sh` | Strict host and cluster evidence validation, including duration, sample span, sample gap, cleanup, and Kubernetes structure | Needs a capability-result schema before it can prove every row in this plan |

## Profiles

Use the same profile names in metadata, dashboards, and release decisions.
Short rehearsals validate runner mechanics but are never release evidence.

| Profile | Minimum duration | Sample cadence | Required use |
| --- | ---: | ---: | --- |
| `R0` rehearsal | One complete scenario cycle, normally less than 30 minutes | Start, each phase, final | Before changing a runner, verifier, image, host configuration, or fault injector |
| `G2` guardrail | 7,200 seconds | At most 300 seconds; at least 24 periodic samples plus start/final | Nightly on dedicated hosts and for every affected release-candidate capability |
| `R24` release | 86,400 seconds | At most 300 seconds; at least 288 periodic samples plus start/final | Before release on the primary supported host for every changed capability class |
| `E72` endurance | 259,200 seconds | At most 300 seconds; at least 864 periodic samples plus start/final | Major runtime, kernel, libkrun, `crun`, storage, networking, or state-schema changes |

The last operation may finish after the requested duration, but the measured
sample span itself must meet the profile duration. A sample gap greater than
600 seconds invalidates `G2`, `R24`, and `E72` evidence.

## Host And Isolation Matrix

Every run uses a dedicated state root and workload prefix. Images must be
digest-pinned or supplied as a recorded OCI archive with a SHA-256 digest.

| Lane | Required profiles | Product claim |
| --- | --- | --- |
| Linux x86_64/KVM MicroVM | `G2` for affected changes, `R24` for releases, `E72` for low-level runtime changes | Primary Linux MicroVM runtime |
| Linux arm64/KVM MicroVM | `G2` for affected changes and before publishing an arm64 release | Linux arm64 build and runtime behavior |
| Apple Silicon/HVF MicroVM | `G2` for affected changes, `R24` before macOS runtime releases | Native macOS runtime behavior |
| Windows x86_64/WHPX MicroVM | `G2` for affected supported features, `R24` before Windows runtime releases | Only the documented Windows feature subset |
| Certified Linux `crun` Sandbox | `G2` for affected changes and every release, `R24` before widening Sandbox use | Shared-kernel Sandbox behavior and fail-closed policy, never MicroVM equivalence |
| Kubernetes RuntimeClass cohort | `G2` on three nodes, `R24` on the release cohort, `E72` before broad rollout | CRI and containerd integration |
| A3S Runtime Provider host | `G2` after provider/runtime changes, `R24` before provider promotion | All advertised R17 profiles on the certified Sandbox backend |
| AMD SEV-SNP host | `G2` after TEE changes, `R24` before any hardware-backed claim | Real attestation, sealing, and secret delivery; simulation is excluded |

Admission requires a clean worktree, exact artifact digests, synchronized UTC
time, sufficient disk and file-descriptor/handle limits, observable host
metrics, no unrelated A3S Box workloads in the selected state root, and a
tested cleanup or rollback command.

## Capability Coverage Matrix

Status has a narrow meaning:

- **Existing**: a time-based real-host runner and evidence already cover the
  scenario's core loop.
- **Partial**: related real operations exist, but at least one required load,
  fault, metric, platform, or verifier gate is missing.
- **Planned**: only functional/design coverage exists; no soak claim is
  allowed yet.

| ID | README capability | Soak workload and fault model | Required pass evidence | Current status |
| --- | --- | --- | --- | --- |
| `RUN-01` | MicroVM runtime | Mixed foreground/detached create, start, exec, pause, resume, restart, wait, stop, remove, and concurrent state updates; kill owned CLI/shim processes | Zero unexpected failures or lost records; exact return to owned shim/mount/socket/box baseline; state remains readable after faults | Existing on KVM/HVF; Windows has its supported subset |
| `RUN-02` | OCI Sandbox | Hot `crun` lifecycle churn with CPU/memory/PID limits, pause/resume, logs, files, and repeated unsupported-option negatives; kill workload and runtime processes | Limits remain enforced; negative requests cause no state mutation; no cgroup, namespace, mount, process, rootfs, or state leak | Partial: real CI smoke and lifecycle benchmark exist, but no time-based Sandbox lane |
| `RUN-03` | Lifecycle and exec | Long-lived foreground/background commands, stdin, stdout/stderr pressure, exit codes, PTY resize/input, health checks, signals, timeout, reconnect, and cancellation | Ordered bounded output, correct exit/signal result, no deadlock, recovery within the declared RTO, and no process-group leak | Partial |
| `PRO-01` | A3S Runtime provider | Repeat every advertised R17 Base, Recovery, Networking, Mounts, Resources, Logs, Exec, and Security profile with driver restart between cycles | Advertised profile set stays constant; inventory before/after matches; idempotent operations do not duplicate resources | Planned: destructive real conformance is one-shot today |
| `IMG-01` | OCI images | Concurrent pull/list/inspect/history/tag/save/load/push/remove/prune; interrupted and slow registry responses, auth redirects, Range resume, corrupt cache candidates, and shared layers | Every published blob matches declared size/digest; retry bounds hold; credentials never cross origins; cache and disk return to the declared budget after prune | Partial |
| `BLD-01` | Dockerfile builds | Repeated native and BuildKit-VM builds, multi-stage targets, cache hit/miss/invalidation, concurrent shared/locked caches, failed `RUN`, warm-pool `RUN`, save/load/push | Output filesystem and OCI metadata match expectations; failed builds publish no partial cache/layer; cache growth and latency remain bounded | Partial |
| `STO-01` | Storage and volumes | Bind/named/tmpfs churn, large and small files, permission/ownership replay, concurrent readers/writers, in-use delete, diff/export/commit, process interruption | Content hashes and metadata remain correct; read-only/in-use fences hold; no mount/temp-file/volume leak; cleanup returns to baseline | Partial |
| `SNP-01` | Filesystem snapshots | Capture, list/get/size, concurrent restore fan-out, independent mutation, delete fencing, source removal, restart, and interrupted capture/restore | Restores preserve expected image defaults and Unix metadata; copies are independent; no partial snapshot is published; final snapshot count/bytes return to baseline | Partial |
| `NET-01` | Networking and Compose | TSI outbound churn, named bridge peers, DNS/aliases, TCP publication and port reuse, connection pressure, Compose up/logs/down, netproxy/passt termination | Success/latency stay within threshold; no cross-network reachability; published ports close on cleanup; no network, forwarder, FD, or route leak | Partial |
| `POL-01` | Warm pool and snapshot-fork | Long-running pool daemon, acquire/release churn, min/max resize, lease expiry, deferred main, build leases, snapshot-fork fan-out, daemon/client termination | No double lease or stale template reuse; idle/active/leased counts reconcile; snapshot count is flat; p95 acquire latency and RSS slope stay within bounds | Partial |
| `SDK-01` | Rust, Python, and TypeScript SDKs | Run the checked native operation inventory in all languages under sequential and concurrent loops on MicroVM and Sandbox; retry restart IDs and stale generations; terminate clients mid-operation | Identical typed outcomes and stable error codes; no inventory drift; deterministic cleanup; bridge timeout/process/RSS/FD counts stay bounded | Partial: one-pass native smoke and legacy KVM pipeline soak exist |
| `OBS-01` | Observability and safety | Continuous logs, stats, events, monitor health/metrics, audit reads, log rotation, slow readers, daemon restart, and state reconciliation | Sample sequence is monotonic; stream identity and timestamps remain usable; no writer deadlock or unbounded log growth; daemon RSS under 50 MiB/day after warm-up | Partial |
| `TEE-01` | TEE | Repeated real SNP attestation, policy verification, RA-TLS, seal/unseal, rollback rejection, secret injection/rotation, restart, and negative evidence | Every report verifies against the recorded policy; stale/tampered evidence fails closed; secrets do not appear in logs/evidence and remain unavailable outside the intended guest | Planned on hardware; simulation never satisfies this row |
| `K8S-01` | Kubernetes CRI and RuntimeClass | Short Job churn plus Redis/Postgres/nginx/Python services, exec/log/stats, deletion during boot, CRI/shim/containerd restart, cleanup and optional node reboot | Existing cluster verifier passes; success rate at least 99.5% excluding declared faults; no Warning events, unexpected restarts, Pending/Unknown workloads, or residual resources | Existing |
| `WIN-01` | Windows | Repeat every supported WHPX core-smoke test, including long argv, ports, bind/named volumes, stats, commit, snapshots, and 2,048-file virtio-fs scans | Every test passes within its timeout; no A3S CLI/shim/forwarder process remains; handle/RSS/disk trends remain within recorded bounds | Existing core loop; longitudinal trend verification is partial |
| `UPG-01` | Packaging, upgrade, and recovery | Start long-lived resources on version N, upgrade to N+1, reconcile/restart/use/remove them, then exercise supported rollback on a canary; include host reboot where allowed | State migration is atomic; unknown future schema fails closed; supported resources remain usable; rollback policy is explicit; cleanup needs no manual repair | Planned |

This matrix is the coverage audit for the current root README. Adding a public
capability requires adding or updating a row here in the same change.

## Workload Composition

Each `R24` and `E72` lane must combine, rather than serially repeat, these
classes where the platform supports them:

- 40% short lifecycle and command churn;
- 20% long-lived services with periodic exec, logs, stats, and health checks;
- 15% storage, snapshot, image, or build mutation;
- 15% network, port, and concurrency pressure;
- 10% declared fault injection.

Use bounded concurrency and record the requested/achieved operation rate.
Warm-up ends after both 15 minutes and two complete workload cycles. Memory,
FD/handle, disk, and latency regression calculations exclude warm-up, while
functional failures during warm-up still fail the run.

## Fault Catalog

Every injected fault has an ID, target owned by the run, start/end timestamp,
expected outcome, recovery timeout, and result in `faults.tsv`.

| Fault | Allowed target | Expected recovery |
| --- | --- | --- |
| `F01` | Client/CLI process termination | Canonical runtime state remains readable; owned workload is reconciled or explicitly removable |
| `F02` | VM or Sandbox shim/runtime termination | Stale process identity is never reused; generation-fenced cleanup succeeds |
| `F03` | Pool, monitor, CRI, or provider process restart | Durable resources reconcile without duplicate allocation |
| `F04` | containerd restart on an enrolled canary | RuntimeClass workloads reconcile according to documented policy |
| `F05` | Registry disconnect, truncation, timeout, or redirect | Retry/no-progress bounds hold and partial content is never published as valid |
| `F06` | netproxy/passt termination or bounded connection loss | Existing failure is visible; cleanup closes ports/routes; later workloads recover |
| `F07` | Build or snapshot interruption | No partial layer, cache write, or snapshot becomes visible |
| `F08` | Host reboot on an isolated canary | State reconciles without signalling reused PIDs or leaking host resources |
| `F09` | Version upgrade/rollback | Migration and compatibility policy match `UPG-01` evidence |

Never inject disk exhaustion, host reboot, containerd restart, or network
partition on a shared production node. Use an isolated canary, filesystem
quota, or deterministic fixture instead.

## Common Evidence Contract

All new or extended runners must produce these files. Existing host/cluster
names remain valid while they are migrated to this superset.

| Artifact | Required content |
| --- | --- |
| `metadata.txt` | Schema version, run/profile/scenario IDs, commit, dirty flag, artifact/image digests, host/isolation details, runner versions, exact thresholds, start time, and selected workload/fault configuration |
| `capability-results.tsv` | One row per selected scenario with attempted/succeeded/expected-fault/unexpected-failure counts and final result |
| `operations.tsv` | Timestamp, scenario, language/API, operation, latency, result, generation, and sanitized error code |
| `resource-samples.tsv` | Monotonic timestamp plus owned/global process, RSS, CPU, FD/handle, shim, mount, socket, box, volume, snapshot, cache, disk, and network counters available on that host |
| `latency.tsv` | Warm-up flag and raw latency samples used for p50/p95/p99 comparison |
| `faults.tsv` | Fault ID, owned target, expected result, timestamps, recovery latency, and pass/fail |
| `inventory-start.json` / `inventory-final.json` | Typed runtime resource inventory before and after the selected scenarios |
| Per-step logs | Bounded stdout/stderr with credentials and secret values removed |
| `summary.txt` or `summary.json` | Requested and measured duration, sample span/gap, operation totals, failure counts, stop condition, cleanup result, and evidence path |
| `verify.out` | Output from the repository verifier run against this exact bundle |

Thresholds are fixed before the run and stored in metadata. A failed run may
not be made passing by editing its bundle or relaxing thresholds afterward.

## Common Hard Gates

Unless a scenario defines a stricter rule, all evidence must satisfy:

1. zero unexpected operation failures in `R0` and `G2`;
2. at least 99.5% successful non-fault operations in `R24` and `E72`, with
   every excluded failure tied to a passing declared fault record;
3. zero lost updates, corrupt state, digest mismatch, policy bypass, secret
   disclosure, deadlock, or manual cleanup;
4. final owned shim, mount, socket, box, network, temporary snapshot, and
   process counts equal the start baseline;
5. no positive post-warm-up daemon RSS slope above 50 MiB/day;
6. no sustained FD/handle growth and no final count above the declared
   baseline allowance;
7. p95 latency no more than 30% above the calibrated baseline for the same
   host, artifact, image, and profile;
8. disk/cache growth remains within the scenario's declared budget and returns
   below it after prune/cleanup;
9. every fault recovers inside its predeclared RTO;
10. the automated verifier accepts duration, sample coverage, capability
    coverage, cleanup, and platform-specific evidence.

Any security invariant, state corruption, or cross-workload data leak stops the
run immediately. Operational stop conditions in
[Production Cluster Tests](production-cluster-tests.md#stop-conditions) also
apply to enrolled clusters.

## Ownership

Ownership names the repository area responsible for implementing the runner
and interpreting failures. Release engineering owns scheduling and evidence
retention but cannot waive a capability owner's failed or missing gate.

| Scenario IDs | Primary repository owner | Required co-review |
| --- | --- | --- |
| `RUN-01`, `RUN-02`, `RUN-03`, `OBS-01` | Core/runtime, CLI, shim, and guest-control maintainers | Platform owner for KVM, HVF, WHPX, or `crun` |
| `PRO-01` | A3S Runtime Provider adapter maintainers | Certified Sandbox and A3S Runtime maintainers |
| `IMG-01`, `BLD-01` | OCI image, registry, and build maintainers | Security review for credentials/signatures; pool review for VM-backed `RUN` |
| `STO-01`, `SNP-01` | Storage, rootfs, volume, and snapshot maintainers | Platform filesystem owner |
| `NET-01` | Runtime networking, netproxy, and Compose maintainers | CRI review when Kubernetes networking is involved |
| `POL-01` | Warm-pool, snapshot-fork, and SDK pipeline maintainers | Runtime lifecycle and storage review |
| `SDK-01` | Rust SDK and Python/TypeScript package maintainers | Runtime bridge and release packaging review |
| `TEE-01` | TEE and security maintainers | Hardware/platform security owner |
| `K8S-01` | CRI and containerd-shim maintainers | Cluster operations owner |
| `WIN-01` | Windows/WHPX and libkrun integration maintainers | CLI, storage, and release packaging review |
| `UPG-01` | Release engineering and state-schema maintainers | Every capability owner whose durable resource crosses the version boundary |

## Scheduling And Promotion

| Changed area | Minimum pre-merge/nightly action | Release action |
| --- | --- | --- |
| `core`, runtime lifecycle, shim, guest init | `R0` plus affected real-host smoke; nightly `G2` KVM and Sandbox | `R24` primary KVM and affected desktop host; `E72` for state/backend changes |
| Image or registry | Deterministic fault suite and `R0 IMG-01` | `G2` against each production registry class; `R24` for transfer/cache changes |
| Build or warm pool | `R0 BLD-01` and `POL-01` | `G2`; `R24` for cache, lease, snapshot-fork, or BuildKit changes |
| Storage, snapshot, networking, Compose | Affected `R0` scenario | `G2` on every affected backend; `R24` for data-format or netproxy/passt changes |
| Native SDK or machine bridge | Package tests and `R0 SDK-01` on MicroVM and Sandbox | `G2` three-language matrix on both isolation classes |
| A3S Runtime Provider | One real R17 conformance pass | `G2 PRO-01`; `R24` before provider promotion |
| CRI/containerd | RuntimeClass smoke | Three-node `G2`; cohort `R24`; `E72` before broad rollout |
| TEE | Simulation regression plus hardware `R0` | Hardware `G2` and `R24`; no hardware claim without both |
| Windows-specific runtime | Windows build and one real iteration | WHPX `G2`; `R24` before publishing Windows runtime assets |
| Packaging/state schema/low-level dependency | Install/upgrade rehearsal | `G2 UPG-01`; `E72` when rollback or reboot behavior changes |

Promotion follows `R0` → `G2` → `R24` → `E72`. A later profile cannot waive a
missing earlier evidence bundle because the shorter profiles validate cleanup
and fault mechanics before consuming a long test window.

## Commands Available Today

These commands exercise the existing baseline. Rows marked Partial or Planned
remain incomplete even when these commands pass.

```bash
# One-iteration host rehearsal.
scripts/host-integration-smoke.sh \
  --no-pure --core --host --soak --soak-iterations 1 --soak-duration 0

# Two-hour KVM/HVF host guardrail.
scripts/host-integration-smoke.sh \
  --no-pure --core --host --soak \
  --soak-duration 7200 \
  --soak-verify-min-duration-secs 7200 \
  --soak-verify-min-sample-span-secs 7200 \
  --soak-verify-min-samples 24 \
  --soak-verify-max-sample-gap-secs 600

# Current certified Sandbox rehearsal and lifecycle benchmark.
scripts/local-sdk-smoke.sh sandbox
RUNS=100 SANDBOX_RUNS=100 bench/bench.sh sandbox

# Existing legacy pipeline fork/snapshot churn on KVM.
A3S_SDK_SOAK_FORKS=2000 \
  cargo test --manifest-path src/Cargo.toml \
  -p a3s-box-sdk --features pipeline-cli --test soak_kvm \
  -- --ignored --nocapture --test-threads=1
```

macOS, Windows, and cluster commands are maintained in their platform guides:

- [macOS fault soak](production-cluster-tests.md#macos-single-host-fault-soak);
- [Windows WHPX soak](windows-whpx.md#whpx-soak-validation);
- [RuntimeClass soak](production-cluster-tests.md#guardrail-soak-2-hours).

## Delivery Plan

### Phase 0: Coverage Contract

- Keep this matrix synchronized with the root README.
- Use stable scenario/profile/fault IDs in future scripts and evidence.
- Link every platform guide back to this cross-capability plan.

Completion: every current README capability has a workload, platform, evidence,
hard gate, status, and implementation owner queue.

### Phase 1: Shared Runner And Evidence

- Add independent five-minute sampling to host and Windows runners.
- Record daemon RSS, CPU, FD/handle, disk, resource inventory, and operation
  latency with a shared schema.
- Add `capability-results.tsv`, `faults.tsv`, and start/final typed inventories.
- Extend `verify-soak-evidence.sh` and its self-test to require declared
  scenario coverage and cleanup.
- Add selectable native SDK, certified Sandbox, and R17 Provider soak lanes.

Completion: `RUN-02`, `PRO-01`, `SDK-01`, `OBS-01`, and `WIN-01` produce
verified `R0` and `G2` bundles.

### Phase 2: Resource Workloads

- Add deterministic registry proxy faults and production-registry profiles.
- Add concurrent build/cache, volume/file, snapshot fan-out, networking/port,
  Compose, and warm-pool lease workloads.
- Define per-scenario disk/cache budgets and stable-host latency baselines.

Completion: `IMG-01`, `BLD-01`, `STO-01`, `SNP-01`, `NET-01`, and `POL-01`
produce verified `G2` bundles on every supported backend.

### Phase 3: Security, Upgrade, And Endurance

- Add real SEV-SNP hardware scheduling and secret-safe evidence checks.
- Add N→N+1 state/resource migration, canary rollback, service restart, and
  allowed host-reboot scenarios.
- Run `R24` for all release lanes and `E72` for low-level runtime changes.

Completion: `TEE-01` and `UPG-01` have verified hardware/canary evidence, and
all release-required rows have retained `R24` bundles.

### Phase 4: Automation And Governance

- Schedule trusted-host nightly `G2` workflows by changed capability.
- Store signed evidence artifacts and compare them with calibrated baselines.
- Publish a release-candidate coverage report listing pass, fail, missing, and
  expired evidence by scenario and host class.
- Block promotion when a required scenario has no matching verified evidence.

Completion: the release decision is reproducible from retained evidence without
terminal history or manual interpretation.

## Definition Of Done For A Soak Scenario

A scenario moves from Partial or Planned to Existing only when all of the
following are true:

- a real-host runner executes the declared workload and faults;
- a short self-test validates runner failure paths without waiting for `G2`;
- evidence includes the common schema and scenario-specific artifacts;
- an automated verifier rejects missing, malformed, too-short, gapped, leaked,
  edited, or incomplete evidence;
- `R0` and `G2` bundles pass on every required host/isolation lane;
- cleanup returns the dedicated state root to its declared baseline;
- the documentation links to the exact command and current support boundary.

Passing unit tests, a single smoke pass, or an unverified long-running shell
loop does not satisfy this definition.
