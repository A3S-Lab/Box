# A3S Box Architecture Optimization Plan

Status: **active planning baseline** (2026-09-23)  
Evidence tip: `main` @ gate-9 tip-prove (WHPX Live digest `e329bb9d…`;
Linux/KVM and Windows/WHPX omit→DedicatedVm cutover tip-proven; WHPX mid-run
Live gate 9 tip-proven; `b2_process_session_recovery_closed` stays false;
Enterprise GA open)  
Companion docs: [ROADMAP.md](../ROADMAP.md), [microvm-kvm-ga-evidence.md](microvm-kvm-ga-evidence.md),
[microvm-whpx-ga-evidence.md](microvm-whpx-ga-evidence.md),
[cross-platform-oci-runtime-development-plan.md](cross-platform-oci-runtime-development-plan.md),
[productization-plan.md](productization-plan.md), [soak-test-plan.md](soak-test-plan.md)

This plan answers one question: **what architectural work makes Box more true to its isolation and ownership axioms, without overfitting to demos, competitor feature lists, or unproven gates.**

It does **not** claim Enterprise GA, close B2/B3/B4, or flip `b2_process_session_recovery_closed`.

---

## 1. First principles (non-negotiable)

### 1.1 Isolation is an explicit product choice

| Request | Boundary | Trust model |
| --- | --- | --- |
| omit / `microvm` | Dedicated guest kernel (libkrun today; OCI DedicatedVm target) | Guest is untrusted; host+hypervisor are trusted |
| `--isolation sandbox` | Shared host kernel (OCI Native Linux) | Defends ordinary escape; **not** a host-kernel exploit defense |

Never silently reinterpret one as the other. Persisted route wins over current rollout policy.

### 1.2 Two planes, one ownership rule

```text
Product plane (Box)                  Execution plane (target: OCI Runtime)
─────────────────────                ────────────────────────────────────
images, builds, volumes              process/VM state, raw I/O
networks, DNS/IPAM, publish policy   NIC/netns attach, guest transport
snapshots (product), health/restart  exact exit, driver, cleanup
Compose/CRI desired state            descriptor-confined FS, replay
logs retention/redaction/audit       ordered runtime events / raw streams
warm-pool & scale-api policy         boot/session primitives
secrets *authorization*              bounded non-durable attachments
attestation *policy*                 attestation mechanisms / TEE drivers
```

New platform execution features belong in OCI Runtime. Box must not grow a third execution path. Shipped product-plane commands in §4.3 stay Box-owned across cutover.

### 1.3 Fail closed beats invent-Ok

Unsupported controls, incomplete cleanup, ambiguous recovery, and unproven claims fail before mutation or stay explicitly open. Green CI on a subset is evidence for that subset only.

### 1.4 Optimization means reducing false architecture, not adding surface

An edit is aligned only if it makes one of these more true:

1. the isolation boundary the caller asked for;
2. durable ownership and recovery under process loss;
3. the product/runtime cutover (one ExecutionManager → OCI);
4. a default threat posture that matches MicroVM assumptions.

Feature parity with other microVM projects is **not** an axiom.

---

## 2. Current architecture (evidence, not aspiration)

### 2.1 What is production today

- **Linux Sandbox GA** via `SandboxViaOci` when `A3S_BOX_OCI_MIGRATION` is absent: lifecycle, exec, filesystem, pause/resume, snapshot, restart, cleanup, Native Live observation (tip harness v7). See [sandbox-ga-evidence.md](sandbox-ga-evidence.md).
- **Linux/KVM omit-isolation → OCI DedicatedVm** production cutover tip-proven
  (binder gates 1–8). See [microvm-kvm-ga-evidence.md](microvm-kvm-ga-evidence.md).
- **Windows/WHPX omit-isolation → OCI DedicatedVm** production cutover tip-proven
  (binder gates 1–8; Created-state owner-death / Host re-ensure only). See
  [microvm-whpx-ga-evidence.md](microvm-whpx-ga-evidence.md).
- Explicit `A3S_BOX_OCI_MIGRATION=off` keeps Box-libkrun on both hosts.
- HVF DedicatedVm **production** cutover remains open (qualification compositions
  exist; no production binder).

### 2.2 What is partial / open (do not optimize as if closed)

| Gate | Honest state |
| --- | --- |
| B2 process-session recovery exit | Open; reports keep `b2_process_session_recovery_closed=false`. Native + KVM Live observation-greened on WSL; **Windows/WHPX mid-run Live tip-proven** (gate 9 digest `e329bb9d…`) but multi-driver B2 exit criteria remain open — do **not** flip B2 from WHPX gate 9 alone |
| B3 storage/network qualification | Open; NetworkStore DNS A + AAAA NODATA (UDP); macOS and Linux TCP/53 terminate known names; first-match IPv4 egress (CIDR/protocol/port) is enforced on netproxy and passt_bridge; passt is started with `--no-map-gw` so the gateway is not rewritten to host loopback; the shim refuses a path-only virtio-net attach that would skip that proxy; IPv6 Ethernet is dropped until an IPv6 policy exists, including one 802.1Q or 802.1ad tag; Sandbox keep-authority refuses Bridge networks that store `--egress` rather than ignoring them; domain match, full AAAA, CNI, Sandbox egress enforcement, macOS host `:ro`, and MicroVM live host-path snapshots remain open |
| B4 Compose/CRI/warm-pool unified adapter | Open; Sandbox Compose path partial; MicroVM Compose cutover and warm-pool unification remain |
| B5 legacy VMM removal | Blocked until HVF production cutover + B2 mid-run Live bar on WHPX match the honesty already tip-proven for KVM/Sandbox observation |
| B6 cross-platform artifact matrix | Open |
| Enterprise GA / BX0.3 TEE | Unproven — cutover binders alone do not constitute Enterprise GA |

### 2.3 Structural tension to resolve (not paper over)

```text
Today:  CLI/SDK → ExecutionManager → ┌─ box_vm (libkrun) ─┐
                                     └─ oci_sdk (Sandbox) ─┘

Target: CLI/SDK → ExecutionManager → oci_sdk → OCI Runtime
                                      ├─ SharedHostKernel
                                      └─ DedicatedVm (KVM/HVF/WHPX)
```

Until B5, dual backends are supported **migration debt**, not a permanent product architecture.

---

## 3. Optimization axes (ordered)

Work proceeds in this order. Later axes do not steal capacity from earlier ones unless they unblock them.

### Axis A — Recovery and lifecycle honesty (highest leverage)

**Problem.** Process I/O, owner death, and short-task exit retention must be exact; B2 exit gate remains open by design until harness + real-driver evidence close it.

**Optimize toward:**

- One retained manager across owner replacement without inventing exit status.
- Keyed replay for mutating FS and exec under lost responses.
- Short-lived workloads that exit before exec-ready still persist authenticated terminal state (already partially landed; keep regression-locked). Guest-init starts the exec accept loop immediately after the container fork returns (before stdio relay / PTY warm-up) so heartbeat can win that race without violating fork-safety; Linux directory-rootfs tip proof for `#576` cleared on WSL (2026-09-22: `run --rm alpine:3.20 -- /bin/true` 5/5 rc 0 with `LD_LIBRARY_PATH=/usr/local/lib/a3s-box`; tracker `#631` closed).

**Evidence required:** Native Live + KVM Live matrices on WSL; Windows/WHPX
mid-run Live tip-proven on real WHPX (`a3s.box.windows-whpx-live-session.v1`
digest `e329bb9d…`, gate 9 in [microvm-whpx-ga-evidence.md](microvm-whpx-ga-evidence.md)).
Still no self-certifying `b2_process_session_recovery_closed=true` from a single
report (multi-driver B2 exit criteria remain open).

**Refuse:** Weakening live-session tests; fixture `process_restart` as driver Live evidence; treating WHPX Created-state Host re-ensure (cutover gate 4) as mid-run Live.

### Axis B — Finish the execution cutover (remove the false dual core)

**Problem.** Two execution owners increase recovery branches, packaging surface, and claim ambiguity.

**Optimize toward:**

1. ~~Production MicroVM via OCI DedicatedVm on Linux/KVM~~ **tip-proven**
   ([microvm-kvm-ga-evidence.md](microvm-kvm-ga-evidence.md)).
2. ~~Production MicroVM via OCI DedicatedVm on Windows/WHPX~~ **tip-proven**
   for omit→DedicatedVm lifecycle **and** mid-run Live (gate 9) in
   [microvm-whpx-ga-evidence.md](microvm-whpx-ga-evidence.md);
   **next:** multi-driver B2 exit criteria (do not flip B2 from WHPX alone);
   HVF production cutover.
3. Production MicroVM via OCI DedicatedVm on Apple Silicon/HVF (same stamp rules).
4. Only then delete Box-direct libkrun lifecycle (B5).

**Evidence required:** Real-host parity for create/start/exec/FS/stop/delete/recovery on each driver; no silent Sandbox↔MicroVM fallback. WHPX mid-run Live must not invent exit and must keep `b2_process_session_recovery_closed=false` until the multi-driver B2 exit criteria land.

**Refuse:** Declaring cutover from qualification-only endpoints; “OCI-shaped” wrappers that still own VMM state inside Box; claiming Enterprise GA from KVM/WHPX cutover binders alone.

### Axis C — Network threat posture on the MicroVM data plane

**Problem.** Product networking (bridge, IPAM, publish, NetworkStore DNS) exists, but the **default MicroVM egress threat model** is weaker than the isolation story implies. Competitor-shaped allowlists are useful only where they encode this axiom.

**Optimize toward (in `netproxy` / passt_bridge, not a new stack):**

1. **Default egress profile** for untrusted MicroVM workloads: allow public internet optionally; deny private, link-local, cloud metadata, and host pivot unless explicitly allowed. passt's host pivot is its default gateway-to-loopback map, disabled with `--no-map-gw`. The profile does not drop the gateway address, because published-port replies use it. The shim refuses a path-only virtio-net attach, which would skip that proxy.
2. **First-match policy** (CIDR / protocol / port) on the host side of smoltcp
   and the Linux L2 mux, stored on the network object (`--egress`). Domain
   match is rejected: a name is not a packet field. IPv6 Ethernet is dropped
   until an IPv6 policy exists (not an IPv6 DSL). One 802.1Q or 802.1ad tag
   does not hide that header.
   This is not a CNI plugin. Operator deny and the default untrusted profile
   are applied before passt_bridge diverts TCP/53 or answers a NetworkStore name.
3. **DNS completeness without lying:** UDP A + AAAA NODATA already; Linux TCP/53
   now uses a real smoltcp TCP owner on passt_bridge (same honesty bar as macOS
   `#577`); no fake one-packet TCP answers. Explicit `--dns` must be an IP:
   garbage fails closed. Bridge requires IPv4 DNS so guest `resolv.conf` cannot
   disagree with the host proxy; TSI keeps IPv6 nameservers. Full AAAA RRs
   still open.
4. **Host-held secrets (optional path):** placeholders in guest; substitution only on host-terminated TLS to allow-listed SNI/DNS — complementary to today’s Compose `secret_environment` tmpfs (which *does* place secret bytes in the guest). Do not deprecate tmpfs secrets until the TLS path is proven; do not claim MITM where pin-bypass exists. Spike and refusal: `docs/host-held-secrets-spike.md` (no implementation).

**Evidence required:** Packet-level tests for deny defaults; policy unit tests; platform-scoped integration; CHANGELOG/ROADMAP non-claims for incomplete platforms.

**Refuse:** Invented CNI; claiming Linux TCP/53 or full AAAA early; copying microsandbox APIs wholesale into Sandbox loopback-only GA.

### Axis D — Storage attachments and snapshot honesty

**Problem.** Product snapshots and Sandbox quiesce exist; MicroVM live host-path snapshots and full B3 storage gate do not.

**Optimize toward:**

- Descriptor-bound volume/image/snapshot handoff to OCI (B3 exit).
- macOS host `:ro` denial when native mechanism exists (no guest-honor-only).
- Snapshot-fork / warm-pool as **Linux x86_64/KVM-scoped** acceleration with soak `POL-01` still honest until closed.

**Refuse:** Advertising memory branch/fork on HVF/WHPX; treating disk-only snapshots as resumable VMs; closing B3 on DNS-only progress.

### Axis E — Orchestration unification (only after A–C can carry it)

**Problem.** Compose/CRI/warm-pool still split across adapters.

**Optimize toward:** Same `LocalExecutionManager` path for Compose MicroVM as for CLI/SDK; CRI as optional full product adapter; OCI-owned containerd shim preferred for RuntimeClass.

**Refuse:** CRI conformance claims from partial adapters; warm-pool success that skips destroy/lease reconciliation.

---

## 4. Capability preservation (anti-loss)

Anti-overfit forbids **new** surface justified only by competitor checklists. It does **not** authorize shrinking **already shipped** Box product capabilities. Fail closed on unsupported hosts remains honesty; deleting a working path without a proven replacement is capability loss.

Authority for “shipped” is the current tree: README CLI map + What Box owns, ROADMAP Responsibility Boundary, SDK bridge 52-op inventory, and platform status tables — not aspirational design docs.

### 4.1 Invariant

Before B5 (legacy VMM / guest-init / Box shim removal), every row below must have a **proven home** on each host where it is advertised today. “Moved to OCI Runtime” without real-host parity is not a home.

Unshipped or pending SDK rows (e.g. live registry progress, directory export streaming, full PTY in non-Rust SDKs) are **not** preservation targets until they land; cutover must not advertise them as preserved either.

### 4.2 Must-survive matrix (architecture)

| Shipped capability | Product plane (Box) | Execution plane (target) | Loss vector if ignored |
| --- | --- | --- | --- |
| Omit-isolation → MicroVM default | Isolation contract, reservation stamp | OCI `DedicatedVm` on Linux/KVM → HVF → WHPX | B5 deletes default path while production still Box-libkrun |
| Linux Sandbox GA | Attachments, logs, health, SDK rails | OCI `SharedHostKernel` (already production) | Low if Sandbox route stays default for `--isolation sandbox` |
| Isolation no-silent-fallback | Router stamp + persisted route | Driver must not weaken class | Reinterpreting sandbox↔microvm under recovery |
| Images / builds / import / save-load / signed pull policy | Box registry & build | Immutable bundle consume only | Moving build into OCI Runtime |
| Registry `login` / `logout` / credentials | Box | — | Dropping credential helpers mid-cutover |
| Bind / named / tmpfs / anon volumes | Box ownership + descriptor handoff | OCI attachments | B3 incomplete ≠ drop; refuse invent-Ok cleanup |
| Stopped FS snapshots, commit/diff/export/cp | Box product snapshots + CLI `cp` | Quiesce/checkpoint via OCI where needed | Closing B3 on DNS-only under-invests storage |
| macOS guest-native ext4 + `a3s-box-mkext4` + maintenance MicroVM | Rootfs assembly, raw-disk lifecycle | HVF DedicatedVm equivalent control | B5 before HVF parity drops macOS stopped diff/export/commit |
| Windows BindFlt `:ro` + guest rootfs metadata | Host denial + portable metadata | WHPX DedicatedVm attach | Inventing Unix ownership on NTFS without metadata path |
| Linux MicroVM `:ro` host RO-bind | Private RO aliases before virtio-fs | Same attach contract via OCI | Guest-honor-only remount regression |
| MicroVM bridge / TSI / passt / netproxy / NetworkStore | IPAM, DNS, aliases, publish, ACL | NIC/netns + L2 mux as product data plane | Treating Sandbox “bridge rejected” as license to delete MicroVM networking |
| `port` / `port-forward` / published TCP+UDP | Host-facing endpoint lifetime | Transport attach + cleanup | scale-api / publish silent skip |
| NetworkStore A + AAAA NODATA + macOS TCP/53 | netproxy / passt_bridge | Same | “Refuse early claims” ≠ rip landed DNS |
| Health probes + restart policy + generation-fenced workers | Box scheduling & policy | Exact kill/wait/restart via OCI | Orphan CLI health tasks or double monitors |
| Log retention / redaction / console projection | Box log plane | Raw I/O stays OCI; no structured-log bleed | Init logs lost when worker not generation-fenced |
| Compose `secret_environment` (Linux tmpfs) | Auth + materialization | Bounded secret mounts | Deprecating before host-TLS path is proven |
| Warm-pool scheduling | Pool policy, leases, drain | Sessions via unified adapter; boots via DedicatedVm | Unifying without lease/destroy honesty |
| Snapshot-fork (Linux x86_64/KVM) | Pool `--snapshot-fork` UX | OCI DedicatedVm memory snapshot **or** explicit sunset | Silent cold-boot while still advertising fork |
| Pause/resume, exec, PTY, shell, attach, FS, stats, events, top | Desired state / projection | Exact generation via OCI | Cutover from qualification-only endpoints |
| Live `container-update` / resource updates | Durable intent + replay id | OCI update + cgroup apply | Losing operation identity across reopen |
| Compose (MicroVM + Sandbox) | ACL normalize, project state | Same `LocalExecutionManager` (B4) | Axis E delay colliding with B5 |
| CRI preview + containerd RuntimeClass | Optional CRI adapter | Prefer OCI Runtime shim | B5 removes Box shim before OCI shim ships |
| `scale-api` | Local Gateway authority | Linux MicroVM/port relay semantics or fail closed | Assuming Sandbox loopback replaces relays |
| TEE CLI (`attest`/`seal`/`unseal`/`inject-secret`) | Admission + attestation policy | Confidential provider / hardware gate | Deprioritization ≠ delete; simulation ≠ BX0.3 |
| Observability: `ps`/`logs`/`inspect`/`stats`/`events`/`df`/`audit`/`monitor` | Product observations | Runtime stats/events primitives | Dropping audit/monitor because “not Axis A–C” |
| Prune family (`prune`/`system-prune`/`image-prune`/`volume prune`/`network prune`) | Box inventory & wipe policy | Runtime delete confirmation first | Invent-clean wipe while mounts remain |
| Installers + versioned SDK/runtime artifacts | Release packaging | Pinned OCI digests | Shipping Box without matching runtime pins |
| Four SDKs + 52-op handshake | Machine bridge / Rust SDK | Same durable resources | Shrinking inventory without version bump |
| Fail-closed unsupported controls (GPU, devices, seccomp custom, …) | Admission reject before mutate | OCI reject before launch | Silent ignore after cutover |

### 4.3 CLI command crosswalk (no orphan commands)

Every README CLI command maps to §4.2. If a command is removed, the matrix row must be explicitly sunset.

| CLI area | Commands | §4.2 home |
| --- | --- | --- |
| Lifecycle | `run`, `create`, `start`, `stop`, `restart`, `rm`, `kill`, `pause`, `unpause`, `wait`, `rename`, `prune` | MicroVM/Sandbox lifecycle + prune family |
| Execution | `exec`, `shell`, `attach`, `top` | Pause/resume, exec, PTY, shell, attach, top |
| Images/builds | `pull`, `push`, `build`, `images`, `rmi`, `tag`, `image-inspect`, `history`, `image-prune`, `save`, `load`, `import` | Images/builds + prune |
| Filesystems | `cp`, `diff`, `export`, `commit`, `volume`, `snapshot` | Snapshots / volumes / macOS ext4 |
| Networking | `network`, `port`, `port-forward`, `compose` | Bridge/TSI/netproxy + Compose + ports |
| Security/TEE | `attest`, `seal`, `unseal`, `inject-secret` | TEE CLI |
| Observability | `ps`, `logs`, `inspect`, `stats`, `events`, `df`, `audit`, `monitor` | Observability row |
| System | `scale-api`, `container-update`, `system-prune`, `pool`, `login`, `logout`, `version`, `info` | scale-api, container-update, warm-pool, registry login, installers/info |

### 4.4 Platform-scoped honesty (not loss)

These are **already** narrower than a universal matrix; preserving them means keeping the documented limits, not inventing parity:

- Windows MicroVM: 1 vCPU; no interactive PTY, bridge, TEE, snapshot-fork, or CRI.
- macOS `:ro` host denial: refuse until native mechanism exists.
- Sandbox GA: loopback-only networking unless explicit keep-authority; never silent MicroVM.
- Snapshot-fork / memory branch: Linux x86_64/KVM only until another hypervisor proves it.
- Intel macOS: unsupported.
- Structured build/live pull progress, volume content helpers, directory export streaming: pending SDK work — do not claim preserved.

### 4.5 B5 entry checklist (capability non-loss gate)

Do not start B5 code deletion until all are true on current evidence:

1. Default omit-isolation production route is OCI `DedicatedVm` on every host that ships MicroVM today (or that host’s installer refuses MicroVM with a stable error).
2. Guest exec, PTY/attach, FS, pause/resume, exact exit, stats/events, and owner-death recovery parity proven for those DedicatedVm drivers (B2 evidence rules; no invent exit).
3. Product networking (bridge/DNS/`port`/`port-forward`/publish) still works through Box-owned policy + OCI attachment without a Box-owned VMM process.
4. Health workers and restart policy still schedule against durable generations without duplicate monitors or lost console projection.
5. macOS raw-ext4 maintenance path has an OCI/HVF equivalent **or** macOS release notes explicitly sunset those commands with fail-closed replacements.
6. KVM snapshot-fork either has an OCI-backed primitive **or** is removed from CLI/docs with fail-closed errors (no silent cold-boot while advertising fork).
7. OCI Runtime containerd shim is packaged and upgrade-compatible before Box shim deletion.
8. Warm-pool, Compose MicroVM, and `scale-api` endpoint relays use paths that do not require Box-direct libkrun (unified manager and/or explicit host fail-closed).
9. `container-update`, prune family, and registry credential flows remain on the product plane with runtime delete/update acknowledgement.
10. Release artifacts still pin matching Box + OCI Runtime + guest/agent digests (`version`/`info` honest).

### 4.6 Review rule

Architecture and cutover PRs must state which §4.2 rows and §4.3 commands they preserve, migrate, or intentionally sunset. “Out of scope / anti-overfit” is invalid as the sole reason to remove a shipped row. Orphan CLI/SDK operations relative to §4.3 are a documentation or cutover defect.

---

## 5. Explicit non-goals (anti-overfit)

Do **not** schedule work whose primary justification is:

- Matching another project’s README feature matrix (branch, TLS MITM, MCP, cloud backend) without an Axis A–C dependency.
- Closing B2/B3/B4 because related CI jobs are green.
- Growing a Box-owned third VMM or second network stack beside netproxy/passt.
- Making shared-kernel Sandbox “as strong as MicroVM” with policy theater.
- Remote multi-tenant control plane inside this repository (Box stays local; authn frontends live elsewhere).
- Time-boxed “looks GA” packaging that hides open gates.

Borrow from peers **only** as threat-model or data-plane techniques that attach to Axes C–D (default egress, host-held secrets, CoW fork product shape on proven platforms).

**Clarification:** Non-goals block *additive* overfitting. They do not waive §4. Removing or weakening a §4.2 row requires an explicit sunset + fail-closed replacement, not an anti-overfit citation.

---

## 6. Decision record: what “done” means for this plan

This planning document is complete when:

1. The axioms in §1 are the review checklist for architecture PRs.
2. Axes A–E are ordered and mapped to ROADMAP milestones without inventing new gates.
3. Non-goals in §5 are enforceable in review (overfit = out of scope) without authorizing §4 capability loss.
4. Near-term work picks the next slice from Axis A or C (or B when evidence is ready), not from E or competitor checklists.
5. B5 work is blocked on the §4.5 checklist.
6. §4.3 stays aligned with the README CLI map (no orphan commands).

Implementation completion is **not** this document’s job. Each axis closes only when its evidence table is satisfied on current `main` / release artifacts.

---

## 7. Near-term recommended slices (honest width)

| Priority | Slice | Axis | Out of scope in the same PR |
| --- | --- | --- | --- |
| P0 | Keep B2 evidence honest; fix real Live/recovery failures only | A | Flipping `b2_process_session_recovery_closed` |
| P0 | MicroVM default egress deny for private/metadata/host (netproxy + tests) — landed `#580` | C | CNI; Sandbox bridge GA |
| P0 | Operator Sandbox setuid cgroup + egid adopt (`#628`) — tip-proven on Ubuntu Orb + closed on `main` via `#636` / Orb proof note; KVM Live / soak still open | A | Claiming GA from Sandbox-only short-rm |
| P1 | Linux passt_bridge TCP/53 NetworkStore answers with real TCP termination — landed this branch | C | Full AAAA RRs |
| P1 | First-match MicroVM egress (CIDR/protocol/port) on netproxy + passt_bridge — landed `#586`. Sandbox keep-authority refuses networks that store those rules | C | Domain match; IPv6 policy DSL; CNI; Sandbox egress enforcement |
| P1 | Design-only host-held secret substitution on netproxy TLS — spike in `docs/host-held-secrets-spike.md` (no code; tmpfs secrets stay) | C | Replacing Compose tmpfs secrets |
| P2 | OCI DedicatedVm production cutover gates for Linux/KVM — binder: [microvm-kvm-ga-evidence.md](microvm-kvm-ga-evidence.md); gates 1–8 tip-proven / docs closed for Linux/KVM omit→OCI; Enterprise GA + HVF/WHPX production open | B | Deleting libkrun before §4.5; HVF/WHPX production |
| P2 | OCI DedicatedVm production cutover gates for Windows/WHPX — binder: [microvm-whpx-ga-evidence.md](microvm-whpx-ga-evidence.md); tracking #650; qualification-only today | B | Claiming WHPX from CI build alone; Enterprise GA |
| P2 | Warm-pool / snapshot-fork soak toward `POL-01` close on KVM only | D | Cross-hypervisor fork claims |
| P3 | Compose MicroVM on unified manager; CRI shim ownership | E | Conformance badges |

---

## 8. Relationship to existing plans

| Document | Role vs this plan |
| --- | --- |
| ROADMAP.md | Milestone checklist and exit gates; this plan orders *why* and *what not* |
| cross-platform-oci-runtime-development-plan.md | Cutover mechanics for Axis B |
| productization-plan.md | Historical productization gates; do not treat old “current notes” as closed B-gates |
| soak-test-plan.md | Evidence profiles for promotion; Partial rows stay Partial |
| sdk-api-and-programmable-cicd.md | SDK required surface; pending rows are not §4 preservation targets |
| cow-snapshot-fork-design.md / native-snapshot-fork-feasibility.md | KVM-scoped acceleration designs under Axis D |

When documents conflict, **ROADMAP open checkboxes and fail-closed claims win** over aspirational design prose.

---

## 9. Change control

- Update this file when an axis’s evidence tip moves, a §4.2 row is intentionally sunset, the README CLI map changes, or a non-goal is rescinded.
- Do not mark axes complete here without linking the closing PR, soak/profile IDs, and ROADMAP checkbox change in the same change set.
- Architecture PRs should cite the axis ID (A–E) and affected §4.2 / §4.3 rows in the description.
