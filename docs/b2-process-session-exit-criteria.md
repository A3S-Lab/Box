# B2 process-session recovery exit criteria

Status: **open**. Harness reports keep
`b2_process_session_recovery_closed=false` by design until every gate below is
tip-proven on current `main` / release pins and a deliberate schema + ROADMAP
change flips the flag.

This binder does **not** claim Enterprise GA, BX0.3 TEE, HVF production cutover,
or B5 libkrun deletion.

## Why a separate binder

Individual Live harnesses (Native, KVM, WHPX) are observation / cutover evidence.
None may self-certify B2 close. Closing B2 requires a multi-driver bar plus an
explicit product decision in ROADMAP and report schemas.

Related:

- [architecture-optimization-plan.md](architecture-optimization-plan.md) Axis A / P1
- [sandbox-ga-evidence.md](sandbox-ga-evidence.md) Native Live
- [microvm-kvm-ga-evidence.md](microvm-kvm-ga-evidence.md) KVM Live
- [microvm-whpx-ga-evidence.md](microvm-whpx-ga-evidence.md) WHPX Live gate 9

## Exit gates

| # | Gate | Honest state |
| --- | --- | --- |
| 1 | Native Linux Sandbox Live: retained stream + keyed mutating FS across Host owner death; no invented exit; schema at or above `a3s.box.linux-native-live-session.v7` | **observation-greened** (CI / WSL digests in Sandbox binder). Still reports `b2_process_session_recovery_closed=false`. |
| 2 | Linux/KVM DedicatedVm Live: retained stream + FS + `kvm_microvm_live_claimed` under Box-owned ensure; tip schema `a3s.box.linux-kvm-live-session` (v5+) | **observation-greened** (WSL digests in KVM binder). B2 flag stays false. |
| 3 | Windows/WHPX DedicatedVm Live: mid-run Host death with retained stream + FS under packaged Box-owned Host; schema `a3s.box.windows-whpx-live-session.v1` | **tip-proven** — pin-honest digest `f366c8d45e95c7992c9e96395db03d8307bba300be5c90e4d3b9cfbc876fe9fc` (Box `79414cc3` / OCI `b26155b1`). B2 flag stays false. |
| 4 | Pin honesty: Native + KVM + WHPX tip digests recorded against the same Box `main` tip and CI `A3S_OCI_RUNTIME_REV` (or a documented re-tip after pin bump) | **partial** — WHPX pin-honest re-tip digest `f366c8d4…` on Box `79414cc3` + OCI pin `b26155b1` (Host binaries `#357` tip). Native/KVM digests still on older pins; refresh those before flip. |
| 5 | Deliberate close: ROADMAP B2 exit checkbox + harness schemas allow `b2_process_session_recovery_closed=true` only when gates 1–4 pass; CHANGELOG states non-claims (no Enterprise GA / HVF / B5) | **open** — do not flip from this document alone. |

## Flip checklist (when gates 1–4 are green)

1. Re-run Native, KVM, and WHPX Live tip-prove on the flip candidate SHAs.
2. Record digests + pins in this binder and the three evidence binders.
3. In one change set: ROADMAP B2 exit checkbox, harness schema/docs allowing
   `true`, verifier acceptance, CHANGELOG with explicit non-claims.
4. Leave Enterprise GA / BX0.3 / HVF / B5 open unless their binders close.

## Explicit non-claims

- Enterprise GA / BX0.3 TEE attestation.
- HVF DedicatedVm production cutover.
- B5 deletion of Box-libkrun / guest-init.
- Fixture `process_restart` continuity as driver Live evidence.
- Treating any single-driver tip digest as B2 close.
