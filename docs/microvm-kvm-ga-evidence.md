# Linux/KVM MicroVM OCI cutover evidence binder

Status: **Linux/KVM omit-isolation production cutover tip-proven** (gates 1–8);
Enterprise GA and HVF production cutover remain open. Windows/WHPX omit→DedicatedVm
lifecycle cutover and mid-run Live (binder gate 9) are tip-proven separately
([microvm-whpx-ga-evidence.md](microvm-whpx-ga-evidence.md)).

Scope: Linux/KVM `DedicatedVm` via A3S OCI Runtime. HVF production cutover is
out of this binder. Sandbox shared-kernel GA is
[sandbox-ga-evidence.md](sandbox-ga-evidence.md).

Pinned OCI Runtime revision is the workflow `A3S_OCI_RUNTIME_REV` value in
[`.github/workflows/ci.yml`](../.github/workflows/ci.yml).

## Activation today (honest)

| Setting | Behavior |
| --- | --- |
| `A3S_BOX_OCI_MIGRATION` absent | Linux: Sandbox → `SandboxViaOci`; omit-isolation / `microvm` → packaged Box-owned DedicatedVm when `a3s-oci` + `a3s-oci-krun-shim` + `system-image.json` are discoverable (gate 5 tip-proven). Missing packages soft-fall to Box-libkrun instead of hard-failing. |
| `off` / `legacy` | VM-only backend (Box-libkrun for omit-isolation) — gate 6 tip-proven |
| `sandbox` / `on` | Sandbox → OCI; MicroVM stays Box-libkrun |
| `microvm` / `all` + `A3S_BOX_OCI_KVM_ENDPOINT` | Qualification: MicroVM → OCI DedicatedVm (external Host) |
| `microvm` / `all` without endpoint | Explicit opt-in packaged Box-owned Host (same artifacts as absent default) |
| WHPX / HVF production composition | **Not claimed** |

Production cutover for this binder means: on Linux (including WSL2 `/dev/kvm`),
**absent** qualification endpoint env, new omit-isolation / `microvm` records
stamp `oci_sdk` + `DedicatedVm` using packaged Box-owned Host artifacts, with
the same fail-closed soft/hard rules Sandbox GA uses.

## What is already greened (do not invent more)

| Gate | Evidence |
| --- | --- |
| KVM Live tip v5 (observation) | Existing-host WSL2 digest SHA-256 `81ecd79ee341ea1705ffd0f16cfb0d0aca76998d8cfd36dca9a04fa982bd7cd5` (Box `68f99abb…`; OCI `f7ab2b7a…` / #347 virtiofs optional fchown). `b2_process_session_recovery_closed=false`. |
| Packaged opt-in Live tip v5 (Box-owned) | WSL2 digest SHA-256 `9dff1de47585472644e37e2971bf7df487bf353c5b07bc01bbf5b0ed84eb0e5f` (Box `51f0deea…` / #643; OCI `f7ab2b7a…`; tip system image). `kvm_microvm_live_claimed`, `retained_stream_handle_proven`, `retained_filesystem_proven`, `box_owned_ensure_proven`; B2 stays false. |
| Packaged opt-in create/start/exec/stop/delete | On `main` after OCI #348 + Box #646/#647 + pin `f08555c9…`: endpoint unset + `A3S_BOX_OCI_MIGRATION=microvm` → `oci_sdk` / `dedicated-vm`; create/start/exec/stop/rm and run with guest `exit 0` → rc 0 / `exit_code` 0. |
| Vertical-slice KVM OCI qualification | `scripts/linux-kvm-oci-qualification.sh` + examples (qualification endpoint / Box-owned Host). Phase-2 recovery handoff timeout still open. |
| Box-owned KVM Host ensure + Live reopen | `oci_kvm_owner` + Live harness `--box-owned`. |

## Production cutover checklist (open)

Each row must be tip-proven with env **unset** for `A3S_BOX_OCI_KVM_ENDPOINT`
unless the row explicitly remains qualification-only.

| # | Gate | Status |
| --- | --- | --- |
| 1 | Packaged artifact discovery for KVM Host (runtime/shim/system-image) without qualification-only env | **tip-proven** — #643 (`oci_kvm_packaged`); owner record used discovered paths |
| 2 | Opt-in `A3S_BOX_OCI_MIGRATION=microvm\|all` uses that packaged Host (Box-owned ensure) | **tip-proven** — endpoint unset stamps `oci_sdk` + `dedicated-vm` |
| 3 | Create/start/exec/FS/stop/delete parity on WSL2 `/dev/kvm` under (2) | **tip-proven on `main` tips** — OCI #348 + Box #646/#647 landed; FS via Live; pin `f08555c9…` |
| 4 | Owner-death / Live reopen under (2) without inventing exit; keep B2 flag false | **tip-proven** — Live digest `9dff1de4…` (`box_owned_ensure_proven`) |
| 5 | Linux default absent-env: omit-isolation stamps OCI DedicatedVm (Sandbox remains SandboxViaOci) | **tip-proven** — unset `A3S_BOX_OCI_MIGRATION` + packaged artifacts → create/start stamps `oci_sdk` + `dedicated-vm`; `run --rm` guest exit 0 |
| 6 | Explicit `off` keeps Box-libkrun | **tip-proven** — `A3S_BOX_OCI_MIGRATION=off` create stamps `box_vm` |
| 7 | No silent Sandbox↔MicroVM fallback | **tip-proven** — unset env: omit-isolation stamps `microvm`/`oci_sdk` (not sandbox); `--isolation sandbox` hard-fails when Sandbox owner is not launch-ready (never silent MicroVM) |
| 8 | Docs: README “Still open” MicroVM production closed for **Linux/KVM only** | **done in this cutover** — README Still-open no longer lists Linux/KVM omit→OCI as open; WHPX/HVF + Enterprise GA remain open |

## Explicit non-claims

- Enterprise GA / BX0.3 TEE.
- Flipping `b2_process_session_recovery_closed`.
- HVF or WHPX DedicatedVm **production** cutover.
- B5 deletion of Box-libkrun / guest-init.
- Treating KVM Live tip digests alone as production cutover.
- Fixture `process_restart` as driver Live evidence.
- CNI / Axis C–E surfaces in the same cutover PR.

## Operator proof sketch (gate 5 default)

```bash
# Packaged a3s-oci + a3s-oci-krun-shim + system-image.json on PATH / beside a3s-box.
# Leave A3S_BOX_OCI_MIGRATION and A3S_BOX_OCI_KVM_ENDPOINT unset.
unset A3S_BOX_OCI_MIGRATION A3S_BOX_OCI_KVM_ENDPOINT
a3s-box create --cpus 1 --memory 512m --network none --name g5 alpine:3.20 -- /bin/true
a3s-box start g5
# Expect managed_execution.runtime_route=oci_sdk and oci_runtime.isolation=dedicated-vm
a3s-box stop g5 && a3s-box rm -f g5

# Gate 6 regression
export A3S_BOX_OCI_MIGRATION=off
a3s-box create --cpus 1 --memory 512m --network none --name offbox alpine:3.20 -- /bin/true
# Expect runtime_route=box_vm
```
