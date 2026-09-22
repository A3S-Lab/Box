# Linux/KVM MicroVM OCI cutover evidence binder

Status: **qualification-only** (not production for omit-isolation)

Scope: Linux/KVM `DedicatedVm` via A3S OCI Runtime. HVF and WHPX production
cutover are out of this binder. Sandbox shared-kernel GA is
[sandbox-ga-evidence.md](sandbox-ga-evidence.md).

Pinned OCI Runtime revision is the workflow `A3S_OCI_RUNTIME_REV` value in
[`.github/workflows/ci.yml`](../.github/workflows/ci.yml).

## Activation today (honest)

| Setting | Behavior |
| --- | --- |
| `A3S_BOX_OCI_MIGRATION` absent | Linux default: `SandboxViaOci` for Sandbox; **omit-isolation MicroVM stays Box-libkrun** |
| `off` / `legacy` | VM-only backend |
| `sandbox` / `on` | Sandbox → OCI; MicroVM stays Box-libkrun |
| `microvm` / `all` + `A3S_BOX_OCI_KVM_ENDPOINT` | Qualification: MicroVM → OCI DedicatedVm (external or Box-owned Host) |
| WHPX / HVF production composition | **Not claimed** |

Production cutover for this binder means: on Linux (including WSL2 `/dev/kvm`),
**absent** qualification endpoint env, new omit-isolation / `microvm` records
stamp `oci_sdk` + `DedicatedVm` using packaged Box-owned Host artifacts, with
the same fail-closed soft/hard rules Sandbox GA uses.

## What is already greened (do not invent more)

| Gate | Evidence |
| --- | --- |
| KVM Live tip v5 (observation) | Existing-host WSL2 digest SHA-256 `81ecd79ee341ea1705ffd0f16cfb0d0aca76998d8cfd36dca9a04fa982bd7cd5` (Box `68f99abb…`; OCI `f7ab2b7a…` / #347 virtiofs optional fchown). `b2_process_session_recovery_closed=false`. |
| Vertical-slice KVM OCI qualification | `scripts/linux-kvm-oci-qualification.sh` + examples (qualification endpoint / Box-owned Host). |
| Box-owned KVM Host ensure + Live reopen | `oci_kvm_owner` + Live harness `--box-owned`. |

## Production cutover checklist (open)

Each row must be tip-proven with env **unset** for `A3S_BOX_OCI_KVM_ENDPOINT`
unless the row explicitly remains qualification-only.

| # | Gate | Status |
| --- | --- | --- |
| 1 | Packaged artifact discovery for KVM Host (runtime/shim/system-image) without qualification-only env | open |
| 2 | Opt-in `A3S_BOX_OCI_MIGRATION=microvm\|all` uses that packaged Host (Box-owned ensure) | open |
| 3 | Create/start/exec/FS/stop/delete parity on WSL2 `/dev/kvm` under (2) | open |
| 4 | Owner-death / Live reopen under (2) without inventing exit; keep B2 flag false | open |
| 5 | Linux default absent-env: omit-isolation stamps OCI DedicatedVm (Sandbox remains SandboxViaOci) | open — blocked on 1–4 |
| 6 | Explicit `off` keeps Box-libkrun | required regression |
| 7 | No silent Sandbox↔MicroVM fallback | required invariant |
| 8 | Docs: README “Still open” MicroVM production closed for **Linux/KVM only** | open — with 5 |

## Explicit non-claims

- Enterprise GA / BX0.3 TEE.
- Flipping `b2_process_session_recovery_closed`.
- HVF or WHPX DedicatedVm **production** cutover.
- B5 deletion of Box-libkrun / guest-init.
- Treating KVM Live tip digests alone as production cutover.
- Fixture `process_restart` as driver Live evidence.
- CNI / Axis C–E surfaces in the same cutover PR.

## Operator proof sketch (after gate 2)

```bash
# After packaged discovery lands: no A3S_BOX_OCI_KVM_ENDPOINT.
export A3S_BOX_OCI_MIGRATION=microvm   # or all
# Prove omit-isolation create/start/exec/stop on a host with /dev/kvm.
a3s-box run --rm alpine:3.20 -- /bin/true
```

Until gates 1–4 pass on tip `main`, keep MicroVM default on Box-libkrun.
