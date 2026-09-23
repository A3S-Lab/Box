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
| 1 | Native Linux Sandbox Live: retained stream + keyed mutating FS across Host owner death; no invented exit; schema at or above `a3s.box.linux-native-live-session.v7` | **same-tip pin-honest** — digest `aa30adca0518aff54029582cc081a8e5530623c7094bda308a3f10113d8fa482` (Box `1356d4bb` / OCI pin `b26155b1`; WSL2 Sandbox CI host). Still reports `b2_process_session_recovery_closed=false`. Prior digests (`bac4f83d…` on `e5203dbe`, CI greened) remain historical. |
| 2 | Linux/KVM DedicatedVm Live: retained stream + FS + `kvm_microvm_live_claimed` under Box-owned ensure; tip schema `a3s.box.linux-kvm-live-session` (v5+) | **same-tip pin-honest** — digest `9a0b744f8e829e172480f13d28ba6e66ea8f51c0253daf6bbb0392f16815dd11` (Box `1356d4bb` / OCI pin `b26155b1`; tip system-image asset). B2 flag stays false. Prior `5bdc5588…` on `357fc359` remains historical. |
| 3 | Windows/WHPX DedicatedVm Live: mid-run Host death with retained stream + FS under packaged Box-owned Host; schema `a3s.box.windows-whpx-live-session.v1` | **same-tip pin-honest** — digest `0696d7e15d4596c026b9bf11a898f0c81f8755d234b1c9b59fe7a180ea1daeaf` (Box `1356d4bb` / OCI `b26155b1`). B2 flag stays false. Prior `f366c8d4…` on `79414cc3` remains historical. |
| 4 | Pin honesty: Native + KVM + WHPX tip digests recorded against the same Box `main` tip and CI `A3S_OCI_RUNTIME_REV` | **closed on tip** — Box `1356d4bb` + OCI pin `b26155b1`: Native `aa30adca…` + KVM `9a0b744f…` + WHPX `0696d7e1…`. |
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
