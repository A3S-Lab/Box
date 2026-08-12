# A3S Box isolation and mechanism performance report

Date: 2026-07-31

Status: **Performance data collected; release blockers found**

## Executive summary

The matrix exercised the real A3S Box `3.2.0` CLI, its dedicated-kernel
MicroVM backend on Linux/KVM and macOS/HVF, and its shared-kernel Sandbox
backend on Linux. It covered lifecycle, exec, idle memory, compute, rootfs/CoW,
tmpfs, bind mounts, named volumes, initialization, networking, concurrency,
warm pools, snapshot-fork, and TEE simulation.

The result is not an all-green performance qualification:

| Lane | Sample outcome | Cleanup outcome | Decision |
| --- | --- | --- | --- |
| Linux/KVM MicroVM plus Sandbox preflight | 181 pass, 1 fail | Snapshot-fork left 3 live shims, 13 overlay mounts, and 13 box directories | MicroVM data is usable; pool cleanup is a blocker |
| Linux persistent Sandbox | 153 pass, 5 fail, 1 skip | Zero processes, mounts, and box directories | Compute/storage data is usable; bind metadata is a blocker |
| Linux foreground Sandbox qualification | 0 pass, 3 fail | Each failed probe required recovery cleanup | No foreground Sandbox performance claim |
| macOS/HVF MicroVM | 163 pass, 7 fail | Snapshot-fork left 9 live shims, 13 APFS mounts, and 13 box directories | Core data is usable; TSI and pool cleanup are blockers |

The Linux measurements are real A3S OS production-host data. The production
host was shared but lightly loaded and the benchmark was pinned to CPUs 4–7
with reduced CPU and I/O priority. No global AppArmor or user-namespace
security setting was changed.

The macOS lane ran on different hardware under materially higher background
load. It is reported separately and is not a KVM-versus-HVF hardware ranking.

## Revisions and artifacts

| Component | Revision or version |
| --- | --- |
| A3S Box | `3.2.0`, commit `d895fa75c7ef24f35257bf3c221f3b681820cd90` |
| A3S OCI Runtime | commit `6b9e0cf2137f1ab9da52e71f566267c97fd9cfa2` |
| Linux image | `docker.io/library/alpine:3.22`, archive SHA-256 `c18b0d9489eeebc11596b9d5a1b6a5301cd763ea0cfdeed2df103d084466ab7c` |
| macOS/HVF image | `docker.io/library/alpine:3.22`, ARM64 archive SHA-256 `b00c22537264b9f57edade2e85f2ebc6a9153eb09eba7bb1ef1c6575bafdd3ec` |

Exact executable, shim, guest-init, OCI Runtime, libkrun, firmware, image, and
published-data hashes are in the
[artifact manifest](../bench/results/isolation-performance-20260731/artifacts.json).

## Host conditions

| Property | Linux/KVM and Sandbox | macOS/HVF |
| --- | --- | --- |
| Host | A3S OS production server | Shared local macOS host |
| OS/kernel | Ubuntu 24.04.1, Linux `6.8.0-49-generic` | Darwin `25.3.0` |
| CPU | 8 vCPU, Intel Xeon Icelake | 10-core Apple M2 Pro |
| Memory | 31.3 GiB | 16 GiB |
| Hypervisor | KVM | Hypervisor.framework |
| CPU placement | CPUs 4–7 | No affinity API |
| Benchmark nice level | 10 | 15 |
| Starting load average | `1.395 / 2.718 / 2.011` for KVM; `1.043 / 0.950 / 1.036` for persistent Sandbox | `5.371 / 9.294 / 10.723` |
| Hardware TEE | No SEV/TDX device | Not measured |

## Method

The image archive was loaded before timed samples. A “cold lifecycle” therefore
means a new execution from a cached local image, not registry pull or image
unpack time.

The formal parameters were:

| Workload | Parameters |
| --- | --- |
| Lifecycle | 20 samples |
| Persistent exec | 30 samples |
| CPU SHA-256 stream | 256 MiB × 5 |
| Memory zero-copy stream | 512 MiB × 5 |
| Storage write/read | 256 MiB × 5 per mechanism; write uses `conv=fsync` |
| Metadata | 2,000 create/delete operations × 5 |
| Host HTTP requests | 50 requests × 5 |
| Host HTTP transfer | 64 MiB as repeated 64 KiB downloads × 5 |
| Concurrency | 4 concurrent executions × 5 |
| Warm pool | size 4, 20 acquisitions |
| Snapshot-fork | pool size 4 × 3 fills |
| TEE simulation | 10 lifecycles |

Latency percentiles use nearest-rank p50 and p95. Throughput values below are
p50 rates. Failed samples are excluded from latency/rate aggregates and remain
visible in pass counts.

## Lifecycle and execution

| Mechanism | Linux/KVM MicroVM | Linux persistent Sandbox | macOS/HVF MicroVM |
| --- | ---: | ---: | ---: |
| Cached cold no-op lifecycle p50 / p95 | 2,219 / 2,374 ms | Not valid on foreground path | 2,199 / 2,322 ms |
| Detached start p50 / p95 | — | 1,016 / 1,119 ms | — |
| Detached remove p50 / p95 | — | 715 / 766 ms | — |
| Volume-backed init p50 / p95 | 2,268 / 2,472 ms | Start 966 / 1,116 ms; remove 565 / 668 ms | 2,202 / 2,400 ms |
| Persistent no-op exec p50 / p95 | 113.943 / 114.198 ms | 113.898 / 114.175 ms | 128.928 / 181.720 ms |
| Four-way lifecycle p50 / p95 | 4,435 / 4,932 ms | Start 1,575 / 1,774 ms; remove 2,174 / 2,472 ms | 3,188 / 3,332 ms |
| Reported idle memory | 77.625 MiB | 7.195 MiB | 129.313 MiB |

The Sandbox values describe a detached box that remains alive for exec and
storage workloads. They must not be presented as a successful foreground
`run --rm` result.

## Compute

| Workload, p50 throughput | Linux/KVM MicroVM | Linux Sandbox | macOS/HVF MicroVM |
| --- | ---: | ---: | ---: |
| SHA-256 stream | 163.420 MiB/s | 174.575 MiB/s | 147.764 MiB/s |
| Memory zero-copy stream | 4,491.726 MiB/s | 4,490.300 MiB/s | 4,029.118 MiB/s |

These values include `a3s-box exec` setup in each timed sample. They do not
isolate raw guest CPU or memory bandwidth.

## Storage and CoW

### Synchronous writes

| Mechanism, p50 throughput | Linux/KVM MicroVM | Linux Sandbox | macOS/HVF MicroVM |
| --- | ---: | ---: | ---: |
| Rootfs/CoW | 357.750 MiB/s | 616.447 MiB/s | 566.902 MiB/s |
| Explicit tmpfs | 1,194.372 MiB/s | 1,193.970 MiB/s | 1,381.892 MiB/s |
| Bind mount | 384.751 MiB/s | 617.505 MiB/s | 761.056 MiB/s |
| Named volume | 333.392 MiB/s | 700.980 MiB/s | 744.501 MiB/s |

### Warm reads

| Mechanism, p50 throughput | Linux/KVM MicroVM | Linux Sandbox | macOS/HVF MicroVM |
| --- | ---: | ---: | ---: |
| Rootfs/CoW | 702.309 MiB/s | 2,239.751 MiB/s | 1,098.220 MiB/s |
| Explicit tmpfs | 2,244.852 MiB/s | 2,244.561 MiB/s | 1,982.414 MiB/s |
| Bind mount | 702.485 MiB/s | 2,244.709 MiB/s | 1,084.590 MiB/s |
| Named volume | 699.902 MiB/s | 2,245.760 MiB/s | 1,084.628 MiB/s |

The initial Sandbox implementation of this benchmark used `/tmp` for
“rootfs.” Qualification proved that Sandbox intentionally mounts `/tmp` as a
64 MiB tmpfs: a 256 MiB sample stopped at exactly 64 MiB with `ENOSPC`. The
published Sandbox run corrects the rootfs path to `/var/tmp`; the invalid
qualification run is not used in the tables.

Metadata create/delete results were:

- Linux/KVM: 5/5 passed, p50 584.785 files/s.
- Linux Sandbox: 0/5 passed. The guest created the mapped bind directory but
  could not remove it, returning `Operation not permitted`.
- macOS/HVF: 4/5 passed, p50 1,221.522 files/s; one sample failed with guest
  `Out of memory`.

## Networking

| Mechanism | Linux/KVM MicroVM | Linux Sandbox | macOS/HVF MicroVM |
| --- | ---: | ---: | ---: |
| TSI host HTTP, 50 requests | 5/5; p50 159.064 requests/s | Not exposed by current profile | 0/5 |
| TSI repeated 64 KiB downloads | 5/5; p50 12.995 MiB/s | Not exposed by current profile | 4/5; p50 21.381 MiB/s |
| Linux named bridge repeated downloads | 5/5; p50 10.114 MiB/s | Not exposed | Not available |
| First-attempt failures | 0 in every KVM sample | — | 7–24 in each passing bulk sample; p50 9 |

The macOS guest repeatedly reported `wget: connection closed prematurely`.
All five short-request samples failed, and one bulk sample exhausted its retry.
The surviving bulk rates therefore do not constitute a reliable-network
qualification.

## Warm pool and snapshot-fork

| Mechanism | Linux/KVM | macOS/HVF |
| --- | ---: | ---: |
| Cold pool fill, 4 VMs | 6,829 ms | 6,809 ms |
| Warm acquire p50 / p95 | 2,068 / 2,268 ms | 1,953 / 2,097 ms |
| Snapshot-fork fill p50 / p95, 4 VMs | 3,020 / 3,120 ms | 29,131 / 29,162 ms |
| Snapshot-fork versus cold fill | 2.26× faster | 4.28× slower |

The first three post-probe acquisitions were lower:

- KVM: 867, 214, and 164 ms;
- HVF: 240, 394, and 351 ms.

Subsequent acquisitions settled near two seconds. With a pool size of four and
one probe consuming a slot, the measured steady state did not preserve the
initial low acquisition latency.

The snapshot-fork cleanup result is a release blocker:

- KVM returned with 13 box directories, 13 overlay mounts, and 3 live shims.
- HVF returned with 13 box directories, 13 APFS mounts, and 9 live shims.

The exact benchmark-owned processes, mounts, sockets, and state directories
were captured and then removed. Both hosts were verified at zero for the
benchmark resources after operator cleanup.

## TEE simulation

| Lane | Samples | p50 / p95 lifecycle |
| --- | ---: | ---: |
| Linux/KVM | 10/10 | 2,320 / 2,874 ms |
| macOS/HVF | 10/10 | 2,167 / 2,305 ms |

These rows use `--tee-simulate`. The Linux host exposed neither a qualifying
SEV-SNP device nor a TDX device. No hardware confidentiality, attestation, or
TEE overhead claim can be made from this report.

## Sandbox foreground failure

The standard Sandbox preflight failed before a valid performance sample:

```text
apply mounts[6] failed: Permission denied
```

`mounts[6]` was the cgroup v2 mount at `/sys/fs/cgroup`. The production host
kept `kernel.apparmor_restrict_unprivileged_userns=1`; it was not changed for
the benchmark. The connection account reported UID 0, so the failure cannot be
explained only as a missing `sudo` invocation. Subsequent exact-artifact probes
progressed past this mount and failed later during log draining, making the
foreground path state- or order-sensitive as well as unsuccessful.

Three additional foreground probes (`true`, `sleep 1`, and `echo`) reached
workload startup but all failed during terminal cleanup:

```text
Sandbox logs did not finish draining; state was preserved for recovery
```

Their 4.659–5.422 second elapsed values are time-to-failure, not lifecycle
performance. The same exact artifacts passed detached `run -d`, `exec`,
`stats`, and `rm -f`, which is why the persistent Sandbox mechanisms were
measured separately.

## Release blockers and follow-up

1. Fix foreground Sandbox cgroup-mount/log-drain behavior, then rerun the normal
   `run --rm` lifecycle matrix.
2. Fix snapshot-fork server shutdown so every child shim, filesystem mount,
   socket, and box directory is reclaimed on KVM and HVF.
3. Investigate the HVF snapshot-fork regression; 29.1 seconds is slower than
   the 6.8-second cold fill.
4. Fix Sandbox mapped-bind metadata deletion.
5. Fix or bound HVF TSI connection resets and rerun request reliability before
   publishing throughput claims.
6. Re-run HVF on an otherwise idle host with a defined background-load gate.
7. Run a separate hardware SEV-SNP qualification before making a TEE claim.

## Reproduction and data

The harness, parameter reference, output contract, and persistent Sandbox mode
are documented in [the benchmark README](../bench/README.md).

Sanitized samples and summaries:

- [Linux/KVM standard matrix](../bench/results/isolation-performance-20260731/linux-kvm-standard/)
- [Linux persistent Sandbox matrix](../bench/results/isolation-performance-20260731/linux-sandbox-persistent/)
- [Linux foreground Sandbox qualification](../bench/results/isolation-performance-20260731/linux-sandbox-foreground-qualification.csv)
- [macOS/HVF MicroVM matrix](../bench/results/isolation-performance-20260731/macos-hvf-microvm/)

The unredacted command logs and operator cleanup evidence are retained only in
the repository-ignored local evidence directory.
