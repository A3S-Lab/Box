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
| 1 | Native Linux Sandbox Live: retained stream + keyed mutating FS across Host owner death; no invented exit; schema at or above `a3s.box.linux-native-live-session.v7` | **pin-honest tip** — digest `bac4f83d85533f1824766c542177bd0119423c50466c3786c74f44adf73e1f6d` (Box `e5203dbe` / OCI pin `b26155b1`; WSL2 Sandbox CI host). Still reports `b2_process_session_recovery_closed=false`. Prior CI digests remain historical. |
| 2 | Linux/KVM DedicatedVm Live: retained stream + FS + `kvm_microvm_live_claimed` under Box-owned ensure; tip schema `a3s.box.linux-kvm-live-session` (v5+) | **pin-honest tip** — digest `5bdc558847a0b4643e67648accefce4a4658d6e6ee3cd6a4a6941bb8063f6cad` (Box `357fc359` / OCI pin `b26155b1`; tip system-image asset). B2 flag stays false. Prior WSL observation digests remain historical. |
| 3 | Windows/WHPX DedicatedVm Live: mid-run Host death with retained stream + FS under packaged Box-owned Host; schema `a3s.box.windows-whpx-live-session.v1` | **tip-proven** — pin-honest digest `f366c8d45e95c7992c9e96395db03d8307bba300be5c90e4d3b9cfbc876fe9fc` (Box `79414cc3` / OCI `b26155b1`). B2 flag stays false. |
| 4 | Pin honesty: Native + KVM + WHPX tip digests recorded against the same CI `A3S_OCI_RUNTIME_REV` (or a documented re-tip after pin bump) | **pin-honest on OCI pin `b26155b1`** — Native `bac4f83d…` (Box `e5203dbe`) + KVM `5bdc5588…` (Box `357fc359`) + WHPX `f366c8d4…` (Box `79414cc3`). Box tip SHAs differ per tip window; optional same-SHA triad re-tip before deliberate gate 5 flip. |
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
