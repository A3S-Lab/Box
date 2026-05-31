# CRI Conformance Baseline

This is the versioned `critest` scoreboard for `a3s-box-cri`. It records where
the CRI implementation stands against the upstream Kubernetes CRI conformance
suite so regressions are visible and progress is measurable.

> **Prerequisite:** the CRI server must be reachable over its Unix socket. Until
> the vendored `h2` UDS-authority patch (`third_party/h2`, see the workspace
> `[patch.crates-io]`), `crictl`/`critest` could not connect at all — every RPC
> failed at CRI-API validation with `PROTOCOL_ERROR`.

## Latest run

| Field | Value |
|-------|-------|
| Date | 2026-05-31 |
| `critest` | v1.30.1 |
| a3s-box | 2.0.4 |
| Host | Linux KVM node (`/dev/kvm`), Ubuntu 24.04 |
| Result | **21 Passed · 59 Failed · 17 Skipped** (ran 80 of 97 specs, ~16 min) |

How it was run:

```bash
a3s-box-cri --socket /tmp/a3s-box.sock \
  --image-dir ~/.a3s/images \
  --agent-image docker.1ms.run/library/alpine:latest &

critest --runtime-endpoint unix:///tmp/a3s-box.sock \
        --image-endpoint  unix:///tmp/a3s-box.sock \
        --test-images-file test-images.yaml \
        --ginkgo.skip="PortForward"
```

> **Caveat:** all `critest` test images were mapped to a single cached
> `alpine` image (`test-images.yaml`), because the node has no general registry
> egress. Tests that require multiple distinct images or registry pulls
> (~8 specs in `image.go`) therefore fail as **test-setup artifacts**, not real
> runtime defects. They are excluded from the "real gaps" below.

## What passes (21)

The core pod + container lifecycle is conformant: `RunPodSandbox` (boots a
microVM), `PodSandboxStatus`, `CreateContainer`, `StartContainer`,
`ContainerStatus`, `ListContainers`/`ListPodSandbox`, `StopContainer`,
`RemoveContainer`, `StopPodSandbox`, `RemovePodSandbox`, plus basic
`RuntimeStatus`/`Version` and basic `ImageStatus`/`ListImages`.

## Real gaps (failures grouped by cause)

| Category | Failing specs (examples) | Root cause | Roadmap |
|----------|--------------------------|------------|---------|
| **Linux SecurityContext** | RunAsUser/RunAsGroup/RunAsUserName, add/drop capabilities, Privileged, ReadonlyRootfs, ReadonlyPaths/MaskedPaths, SupplementalGroups | OCI `SecurityContext` is not applied to the in-VM process (`guest-init`) | new: security-context |
| **seccomp / AppArmor / sysctls** | seccomp default/unconfined/localhost, AppArmor permissive/blocking, safe/unsafe sysctls | LSM + sysctl plumbing not implemented in the guest | new: lsm-sysctl |
| **Streaming** | exec (tty on/off, stdin on/off), attach | exec/attach return an empty error; SPDY streaming demux not wired to the VM agent | #5 streaming |
| **Container logs** | starting container with log, reopening container log | CRI log writer does not capture container stdout/stderr to `log_path` | #5 log-writer |
| **Volumes / mounts** | starting with volume, volume host-path symlink, mount propagation (rshared/rslave/rprivate), non-recursive readonly mounts | OCI mount spec (volumes, propagation, ro) not fully honored | new: mounts |
| **Namespaces** | HostNetwork true/false, HostPID, HostIPC, PodPID, ContainerPID | Pod/host namespace sharing modes not mapped into the microVM | new: namespaces |
| **Networking** | DNS config, port mapping (host+container, container-only) | DNS config injection + CRI port mapping over TSI not wired | #5 networking |

## Methodology

The baseline is intentionally captured *before* fixing the gaps so each fix can
be measured against it. Re-run after each CRI feature lands and update the
"Latest run" table; the goal is to drive Failed → 0 (excluding documented
test-setup artifacts) and graduate `a3s-box-cri` to a conformant CRI runtime.
