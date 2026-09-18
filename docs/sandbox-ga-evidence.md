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
6. Native Live v7 retained stream + keyed mutating filesystem continuity
   (MakeDir / Move / Remove + upload before kill; ListDir + download of the
   moved tree after reattach), verified by
   `scripts/verify-linux-native-live-session-report.py`. **CI-greened** on run
   `34805883757`: SHA-256
   `71b106e90635780f904679c21f03459c748070aadfd0dbf99a0ea0888107b2fd`
   (linux-x86_64) /
   `43044eed12fb53b4452d5dab948b3ec5528e1236d56ce35a435335e422a3cb21`
   (linux-arm64). Does **not** flip B2. KVM tip v5 existing-host greened
   digest SHA-256
   `cb8e6c287c669086249e0f6fd38f447deb2d0466ea1803aa2e7c2b5d66579373`
   (WSL2 `/dev/kvm`; Box `d8854a4c…`; OCI `fb390b69…`; tip-rebuilt system
   image); does **not** flip B2.

## Operator setuid launcher evidence (self-hosted)

Product fail-closed: non-root Host spawn without `A3S_BOX_CI_SETPRIV_WRAPPER`
requires a root-owned setuid launcher (mode `4755`). Lab setpriv remains the
hosted CI path and is **not** a substitute.

Self-hosted / lab proof on a **suid-capable** root filesystem:

```bash
sudo bash scripts/proof-linux-sandbox-setuid-launcher.sh \
  --a3s-oci /absolute/path/to/a3s-oci \
  --box-bin /absolute/path/to/bin \
  --report /absolute/path/to/setuid-launcher-proof.json
```

The script installs via `prepare-linux-sandbox-host.sh --install-launcher`,
refuses nosuid installs where `chmod 4755` does not stick, unsets the CI
setpriv wrapper, and runs a non-root `a3s-box run --isolation sandbox` smoke.
Report schema: `a3s.box.linux-sandbox-setuid-launcher-proof.v1`. Verify with
`python3 scripts/verify-linux-sandbox-setuid-launcher-proof.py REPORT.json`
(fail-closed honesty checker; CI runs `--self-test`). This gate is
not required to flip B2 or MicroVM cutover.

## Proven Sandbox surfaces

| Surface | Evidence |
| --- | --- |
| Lifecycle create/start/stop/restart/remove | SDK Local Sandbox + no-KVM smoke |
| Captured/streaming exec, files, filesystem | Same |
| Named volumes, bind/tmpfs (R17 mounts) | R17 profile gate |
| Network: private netns, loopback-only; R17 Service host-loopback relays | R17 networking profile + design |
| Pause/resume, filesystem snapshots | SDK Local Sandbox |
| Native Live v7 retained stream + keyed mutating FS across owner SIGKILL | Live-session gate + verifier (CI-greened digests above) |
| Stopped-only owner crash recovery | no-KVM recovery report |
| Fresh ensure reaps supervised orphans after Host SIGKILL | SDK sandbox smoke + #339 |
| Operator setuid launcher (self-hosted suid FS) | proof script + honesty verifier |

## Explicit non-claims

- Named **bridge** networks, peer discovery, and static published ports as
  Sandbox GA product surfaces (rejected for Sandbox; MicroVM/TSI differ).
- Compose / CRI / containerd as Sandbox GA-closed.
- Default omit-isolation → MicroVM cutover to OCI.
- WHPX/KVM MicroVM **production** OCI composition (qualification-only remains).
- Flipping `b2_process_session_recovery_closed`.
- Fixture `process_restart` as driver Live evidence.
- Inventing KVM Live digests or flipping `b2_process_session_recovery_closed`
  from observation greening (Native Live v7 is CI-greened; KVM tip v5 is
  existing-host greened
  `cb8e6c287c669086249e0f6fd38f447deb2d0466ea1803aa2e7c2b5d66579373`).
- Cloud `BX0.3` / hardware TEE.
- CI setpriv as a substitute for an operator setuid install at
  `/usr/local/libexec/a3s-box-sandbox-oci-launcher` (use the self-hosted
  setuid proof gate instead; hosted CI remains setpriv-on-nosuid).

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
