# Windows/WHPX MicroVM OCI cutover evidence binder

Status: **Windows/WHPX omit-isolation production cutover tip-proven** (gates 1–8)

Scope: Windows x86_64 `DedicatedVm` via A3S OCI Runtime WHPX Host. Linux/KVM
production cutover is tracked in
[microvm-kvm-ga-evidence.md](microvm-kvm-ga-evidence.md). HVF is out of this
binder. Sandbox shared-kernel GA is Linux-only
([sandbox-ga-evidence.md](sandbox-ga-evidence.md)).

Pinned OCI Runtime revision is the workflow `A3S_OCI_RUNTIME_REV` value in
[`.github/workflows/ci.yml`](../.github/workflows/ci.yml).

## Activation today (honest)

| Setting | Behavior |
| --- | --- |
| `A3S_BOX_OCI_MIGRATION` absent | Windows: omit-isolation → packaged Box-owned DedicatedVm when Host artifacts are discoverable (gate 5 tip-proven); missing packages soft-fall to Box-libkrun/WHPX |
| `off` / `legacy` | VM-only backend — gate 6 tip-proven (`runtime_route=box_vm`) |
| `sandbox` / `on` | Rejected on Windows (Sandbox is Linux-only) — gate 7 tip-proven |
| `microvm` / `all` + `A3S_BOX_OCI_WHPX_ENDPOINT` | Qualification: MicroVM → OCI DedicatedVm (external Host pipe) |
| `microvm` / `all` + `A3S_BOX_WHPX_OCI_BOX_OWNED=1` + service root/bin/shim/vm-rootfs/**manifest** | Qualification: Box-owned WHPX Host ensure (requires `system-image.json`) |
| Packaged absent-env production default | **tip-proven** — unset migration + packaged Host stamps `oci_sdk`; soft-fall without package |

Production cutover for this binder means: on Windows x86_64 with WHPX, **absent**
qualification endpoint env, new omit-isolation records stamp `oci_sdk` +
`DedicatedVm` using packaged Box-owned Host artifacts (`a3s-oci.exe`,
`a3s-oci-krun-shim.exe`, bootstrap `vm-rootfs`, and `system-image/system-image.json`),
with fail-closed soft/hard rules that never silently reinterpret Sandbox (Sandbox remains
unsupported on Windows).

## What is already greened (do not invent more)

| Gate | Evidence |
| --- | --- |
| Box-owned minimal WHPX bundle | ROADMAP B1: create/start/wait exercised on real Windows/WHPX |
| CI Build Windows WHPX | Hosted workflow builds WHPX product path |
| Linux/KVM omit→DedicatedVm production | See KVM binder gates 1–8 (not a WHPX claim) |
| WHPX packaged production cutover | Gates 1–8 tip-proven below (this binder) |

## Production cutover checklist

Each row must be tip-proven with env **unset** for `A3S_BOX_OCI_WHPX_ENDPOINT`
unless the row explicitly remains qualification-only.

| # | Gate | Status |
| --- | --- | --- |
| 1 | Packaged artifact discovery for WHPX Host (runtime/shim/vm-rootfs/system-image) without qualification-only env | **tip-proven** — `oci_whpx_packaged` + install layout (OCI `packaging/windows/README.md`); local WHPX Host package beside `a3s-box.exe` discovered with endpoint unset |
| 2 | Opt-in `A3S_BOX_OCI_MIGRATION=microvm\|all` uses that packaged Host (Box-owned ensure) | **tip-proven** — endpoint unset → Box-owned ensure; Windows `pid_start_time` via `GetProcessTimes`; ready schema `a3s.oci.box-whpx-service-ready.v2`; mutable service root materializes `bootstrap-vm-rootfs/` disjoint from immutable `system-image/`; `create` returned MicroVM id on real WHPX |
| 3 | Create/start/exec/FS/stop/delete parity on real WHPX under (2) | **tip-proven** — durable install must keep Host `bin/` and `system-image/` disjoint (flattening CI `bin/*` beside `system-image/` fails `WindowsSystemImage::load`); with `%USERPROFILE%\\.a3s\\bin` + `share\\a3s\\system-image` (or install-root `bin/` + sibling `system-image/`), empty `bootstrap-vm-rootfs` seed, endpoint unset: `create` → `start` (long-running `/bin/sleep`) → `exec` / FS-via-exec → `stop` → `rm -f` on real WHPX; shares cleaned |
| 4 | Owner-death / Live reopen under (2) without inventing exit; keep B2 flag false | **tip-proven** — under (2) with endpoint unset: `create` → status `created` / `oci_sdk`; kill Box-owned Host (`service-ready.json` owner pid); Box record stays `created` with no invented `exit_code`/`finished_at`; next `start` re-ensures a new Host (ready schema `a3s.oci.box-whpx-service-ready.v2`, new owner pid) → `running` → `exec`/`stop`/`rm -f`. Matches KVM binder’s `box_owned_ensure_proven` bar. **Non-claim:** mid-run Host death with retained Live stream/FS reattach (`b2_process_session_recovery_closed` stays false) |
| 5 | Windows default absent-env: omit-isolation stamps OCI DedicatedVm | **tip-proven** — unset `A3S_BOX_OCI_MIGRATION` + packaged Host → create/start/wait stamps `runtime_route=oci_sdk` and after start `oci_runtime.isolation=dedicated-vm` (`driver=libkrun-whpx`); guest exit 0 |
| 6 | Explicit `off` keeps Box-libkrun/WHPX | **tip-proven** — `A3S_BOX_OCI_MIGRATION=off` create stamps `runtime_route=box_vm` |
| 7 | No silent Sandbox↔MicroVM fallback (Sandbox stays unsupported/fail-closed on Windows) | **tip-proven** — `--isolation sandbox` hard-fails (`Sandbox isolation is supported only on Linux`); no box record stamped |
| 8 | Docs: README “Still open” closes WHPX production only after 1–7 | **done in this cutover** — README Still-open no longer lists Windows/WHPX omit→OCI as open; HVF + Enterprise GA remain open |

## Next binder work (not tip-proven)

| # | Gate | Status |
| --- | --- | --- |
| 9 | Mid-run Host death with retained Live stream + filesystem reattach under packaged Box-owned WHPX (running DedicatedVm; no invented exit; `b2_process_session_recovery_closed` stays false) | **open** — observation harness landed (`windows-whpx-live-session-qualification`, schema `a3s.box.windows-whpx-live-session.v1`): keyed exec/FS before taskkill, retained-stream + filesystem proofs after reattach, honest `b2=false` / `fixture_stream_continuity_claimed=false`. Not tip-proven on real WHPX; mid-run Live also needs OCI WHPX durable session-owner (KVM analogue). Gate stays open until a tip-proven digest is published |

Do **not** treat gate 9 as Enterprise GA or as flipping B2. It only closes the
explicit mid-run Live non-claim under Windows/WHPX.

## Explicit non-claims

- Enterprise GA / BX0.3 TEE.
- Flipping `b2_process_session_recovery_closed`.
- HVF DedicatedVm **production** cutover.
- B5 deletion of Box-libkrun / guest-init.
- Treating CI “Build Windows WHPX” alone as production cutover.
- Mid-run Host death Live stream/FS reattach (B2).
- CNI / Axis C–E surfaces in the same cutover PR.
