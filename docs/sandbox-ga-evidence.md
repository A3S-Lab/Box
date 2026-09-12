# Linux Sandbox GA evidence binder

Status: **production for explicit `--isolation sandbox` on certified Linux**

Scope: shared-host-kernel Sandbox only. Omit-isolation remains MicroVM and is
out of this binder.

Pinned OCI Runtime revision is the workflow `A3S_OCI_RUNTIME_REV` value in
[`.github/workflows/ci.yml`](../.github/workflows/ci.yml).

## Activation

| Setting | Behavior |
| --- | --- |
| `A3S_BOX_OCI_MIGRATION` absent | Linux default: `SandboxViaOci` for new Sandbox records |
| `off` / `legacy` | VM-only backend |
| `sandbox` / `on` | Same composition; hard-fail if the OCI owner is not launch-ready |
| Default owner start fails | Soft-compose fail-closed unavailable OCI backend; MicroVM continues; Sandbox preflight fails |

## CI binder (`sdk-local-sandbox`, x86_64 and aarch64)

Authoritative hosted gate. Steps that matter for GA:

1. Package the no-KVM product layout (`scripts/package-no-kvm-product.sh`).
2. Prepare the qualification host (`scripts/prepare-linux-sandbox-ci-host.sh` →
   `prepare-linux-sandbox-host.sh --ci`). CI uses setpriv identity on nosuid
   runners; that is **lab** evidence, not proof that every operator already has
   a setuid libexec launcher.
3. Certify advertised R17 profiles through the production owner route.
4. Exercise production Box-to-OCI owner composition with
   `A3S_BOX_OCI_MIGRATION` **unset**.
5. Packaged SDK smoke with `/dev/kvm` absent and inaccessible
   (`scripts/no-kvm-packaged-sdk-smoke.sh`).
6. Native Live v4 retained stream + filesystem continuity, verified by
   `scripts/verify-linux-native-live-session-report.py`.

## Proven Sandbox surfaces

| Surface | Evidence |
| --- | --- |
| Lifecycle create/start/stop/restart/remove | SDK Local Sandbox + no-KVM smoke |
| Captured/streaming exec, files, filesystem | Same |
| Named volumes, bind/tmpfs (R17 mounts) | R17 profile gate |
| Network: private netns, loopback-only; R17 Service host-loopback relays | R17 networking profile + design |
| Pause/resume, filesystem snapshots | SDK Local Sandbox |
| Native Live v4 retained stream + FS across owner SIGKILL | Live-session gate + verifier |
| Stopped-only owner crash recovery | no-KVM recovery report |

## Explicit non-claims

- Named **bridge** networks, peer discovery, and static published ports as
  Sandbox GA product surfaces (rejected for Sandbox; MicroVM/TSI differ).
- Compose / CRI / containerd as Sandbox GA-closed.
- Default omit-isolation → MicroVM cutover to OCI.
- WHPX/KVM MicroVM **production** OCI composition (qualification-only remains).
- Flipping `b2_process_session_recovery_closed`.
- Fixture `process_restart` as driver Live evidence.
- Cloud `BX0.3` / hardware TEE.
- CI setpriv as a substitute for an operator setuid install at
  `/usr/local/libexec/a3s-box-sandbox-oci-launcher`.

## Operator host preparation

1. Install the Box package (`install.sh` / packaged layout).
2. Run `sudo bash scripts/prepare-linux-sandbox-host.sh --install-launcher /path/to/a3s-oci`
   (or the installed `a3s-oci` path). See [Installation](installation.md).
3. Export `A3S_BOX_SANDBOX_DELEGATED_CGROUP_ROOT` as printed.
4. Run `a3s-box run --rm --isolation sandbox …` **without** setting
   `A3S_BOX_OCI_MIGRATION`.

## How to refresh

- Re-run `sdk-local-sandbox` on the release tip.
- Keep Native Live report verification fail-closed; do not edit reports by hand.
- Update this binder when the advertised matrix grows; never remove a non-claim
  without matching CI + design evidence.
