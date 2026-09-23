# Windows/WHPX MicroVM OCI cutover evidence binder

Status: **qualification-only** (not production for omit-isolation)

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
| `A3S_BOX_OCI_MIGRATION` absent | Windows: omit-isolation stays Box-libkrun/WHPX; no packaged DedicatedVm soft-activate |
| `off` / `legacy` | VM-only backend |
| `sandbox` / `on` | Rejected on Windows (Sandbox is Linux-only) |
| `microvm` / `all` + `A3S_BOX_OCI_WHPX_ENDPOINT` | Qualification: MicroVM → OCI DedicatedVm (external Host pipe) |
| `microvm` / `all` + `A3S_BOX_WHPX_OCI_BOX_OWNED=1` + service root/bin/shim/vm-rootfs/**manifest** | Qualification: Box-owned WHPX Host ensure (requires `system-image.json`) |
| Packaged absent-env production default | **Not claimed** — needs gates below |

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

## Production cutover checklist (open)

Each row must be tip-proven with env **unset** for `A3S_BOX_OCI_WHPX_ENDPOINT`
unless the row explicitly remains qualification-only.

| # | Gate | Status |
| --- | --- | --- |
| 1 | Packaged artifact discovery for WHPX Host (runtime/shim/vm-rootfs) without qualification-only env | **blocked** — no durable Windows install layout yet. Qualification downloads CI `windows-whpx-qualification` + guest-agent + rootfs archive each run (`scripts/windows-whpx-oci-qualification.ps1`). Gate 1 needs a shipped layout (e.g. `a3s-oci.exe`, `a3s-oci-krun-shim.exe`, and a durable `vm-rootfs` with `usr\bin\a3s-oci-agent` beside `a3s-box` / under `~/.a3s`) before discovery code is tip-proven. Do not invent soft-default cutover without that package. |
| 2 | Opt-in `A3S_BOX_OCI_MIGRATION=microvm\|all` uses that packaged Host (Box-owned ensure) | open / qualification exists with explicit BOX_OWNED + paths |
| 3 | Create/start/exec/FS/stop/delete parity on real WHPX under (2) | open |
| 4 | Owner-death / Live reopen under (2) without inventing exit; keep B2 flag false | open |
| 5 | Windows default absent-env: omit-isolation stamps OCI DedicatedVm | open — blocked on 1–4 |
| 6 | Explicit `off` keeps Box-libkrun/WHPX | required regression |
| 7 | No silent Sandbox↔MicroVM fallback (Sandbox stays unsupported/fail-closed on Windows) | required invariant |
| 8 | Docs: README “Still open” closes WHPX production only after 1–7 | open |

## Explicit non-claims

- Enterprise GA / BX0.3 TEE.
- Flipping `b2_process_session_recovery_closed`.
- Linux/KVM or HVF cutover (separate binders).
- B5 deletion of Box-libkrun / guest-init.
- Treating CI “Build Windows WHPX” alone as production cutover.
- CNI / Axis C–E surfaces in the same cutover PR.
