# CRI Conformance Baseline

This is the versioned `critest` scoreboard for `a3s-box-cri`. It records where
the CRI implementation stands against the upstream Kubernetes CRI conformance
suite so regressions are visible and progress is measurable.

> **Prerequisite:** the CRI server must be reachable over its Unix socket. This
> required a vendored `h2` UDS-authority patch (`third_party/h2`, see the
> workspace `[patch.crates-io]`); before it, `crictl`/`critest` could not connect
> at all — every RPC failed CRI-API validation with `PROTOCOL_ERROR`.

## Latest run

| Field | Value |
|-------|-------|
| Date | 2026-06-01 |
| `critest` | v1.30.1 |
| a3s-box | 2.0.5 (+ unreleased CRI maturity work, heading to 2.0.6) |
| Host | Linux KVM node (`/dev/kvm`), Ubuntu 24.04 |
| Result | **38 Passed · 44 Failed · 15 Skipped** (ran 82 of 97 specs, ~17 min) |

This is up from the original **21 Passed / 59 Failed** baseline (2.0.4): streaming
exec/attach, container logs + reopen, and most of the Linux SecurityContext now
pass. Several remaining "failures" are **test-environment artifacts**, not runtime
defects (see below).

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

> **Caveat:** all `critest` test images were mapped to a single cached `alpine`
> image (`test-images.yaml`), because the node has no general registry egress.
> Specs that pull a distinct image (`registry.k8s.io/e2e-test-images/{nginx,
> httpd,nonewprivs}`, `gcr.io/.../test-image-predefined-group`) therefore fail at
> image pull as **test-setup artifacts**, not runtime defects. Only
> `defaultTestContainerImage` + `webServerImage` are overridable via the images
> file, so these pulls cannot be redirected to the mirror.

## What passes (38)

- **Pod + container lifecycle:** `RunPodSandbox` (boots a microVM),
  `PodSandboxStatus`, `CreateContainer`, `StartContainer`, `ContainerStatus`,
  `ListContainers`/`ListPodSandbox`, `StopContainer`, **`RemoveContainer`
  (incl. force-removing a running container)**, `StopPodSandbox`,
  `RemovePodSandbox`, plus `RuntimeStatus`/`Version`.
- **Streaming:** `Exec`/`Attach` over SPDY/3.1 (tty on/off, stdin on/off),
  `ExecSync`.
- **Container logs:** writing to `log_path` and `ReopenContainerLog` (rotation).
- **Linux SecurityContext:** `RunAsUser`, `RunAsGroup`, `RunAsUserName`
  (passwd lookup), reject `RunAsGroup` without `RunAsUser`, `SupplementalGroups`,
  `ReadonlyPaths`, seccomp `unconfined`/nil. `/proc` + `/sys` are mounted inside
  the container chroot.
- Basic `ImageStatus`/`ListImages`.

## Real gaps (failures grouped by cause)

| Category | Failing specs (examples) | Root cause | Fixable here? |
|----------|--------------------------|------------|---------------|
| **Registry egress** (~22) | Multiple-Containers exec/log/network, image pull/list/status, DNS, port-mapping, port-forward, HostNetwork/PID/IPC, Privileged, ReadOnlyRootfs, NoNewPrivs, image-group SupplementalGroups | webserver/helper images on `registry.k8s.io`/`gcr.io` are unreachable and not overridable via the images file | ❌ environmental |
| **Writable volume mounts** (~6) | starting container with volume (+ host-path symlink), mount propagation rshared/rslave/rprivate, non-recursive readonly mounts | only read-only mounts (materialized by copy) are supported; writable host-path volumes need a virtio-fs share into the VM, which libkrun configures at boot (containers are created post-boot) | ⚠️ architectural |
| **seccomp filters** (~3) | seccomp `RuntimeDefault` (wants `Seccomp: 2`), localhost profile, SYS_ADMIN-block | no BPF seccomp filter is installed in the guest child | ✅ guest feature |
| **sysctls** (~2) | safe/unsafe sysctls | pod sysctls not applied in the guest (`/proc/sys` writes) | ✅ guest feature |
| **AppArmor** (~2) | unloaded profile, profile blocking writes | LSM not wired in the guest | ✅ guest feature |
| **Capabilities** (~3) | add/drop capability, drop ALL | capabilities not managed per-container; `brctl` bridge test also needs a `CONFIG_BRIDGE` guest kernel | ⚠️ murky / possibly kernel-limited |
| **MaskedPaths** (1) | mask `/bin/ls` | masking code is correct, but under the alpine/busybox substitution `/bin/ls` and `/bin/sh` are the same `busybox` binary, so masking `ls` breaks `sh`; the expected stderr needs `sh` to run | ❌ test-image artifact |

## Stability

- **Graceful shutdown:** on SIGTERM/SIGINT the CRI drains the gRPC server and
  reaps every sandbox VM (kills the shim, unmounts its overlay, removes the
  rootfs dir) — no more orphaned microVMs/overlays across restarts.
- **TODO:** SIGKILL/crash-case startup reaping (reconcile already marks orphaned
  sandboxes NotReady, but does not yet reap the dead instance's leftover shim
  processes + overlay mounts).

## Methodology

The baseline is captured so each fix is measurable. Re-run after each CRI feature
lands and update the "Latest run" table; the goal is to drive Failed → 0
(excluding documented environmental/test-image artifacts) and graduate
`a3s-box-cri` to a conformant, mature CRI runtime.
