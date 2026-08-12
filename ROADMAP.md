# A3S Box Optional Security Policy Roadmap

Status: **In progress**

Scope: request-scoped, opt-in security policies for A3S Box MicroVM and
shared-kernel Sandbox executions.

Related plans:

- [Optional Security Policy Threat Model](docs/optional-security-policy-threat-model.md)
- [Productization Plan](docs/productization-plan.md)
- [Cross-Capability Soak Test Plan](docs/soak-test-plan.md)
- [Host Sandbox Backend Design](docs/host-sandbox-backend-design.md)
- [SDK API and Programmable CI/CD Plan](docs/sdk-api-and-programmable-cicd.md)

## Outcome

A3S Box will add optional policy controls for host bind mounts, outbound
network access, and verifiable execution evidence without changing the default
runtime behavior.

Every optional control follows the same contract:

1. omitting the policy preserves the current behavior;
2. policy selection is part of an individual execution request, not a global
   daemon environment switch;
3. the request uses typed policy objects in Rust, Python, TypeScript, and Go;
4. an enabled control is enforced completely or the execution fails before
   workload launch;
5. no policy failure can silently change a MicroVM request into a
   shared-kernel Sandbox request, or the reverse;
6. the resolved policy and its digest survive restart and recovery;
7. public support is claimed only after real-runtime evidence exists for the
   advertised host and isolation class.

The default dedicated-kernel MicroVM remains the boundary for untrusted and
multi-tenant workloads. Optional process, mount, and network policies are
defense in depth; they do not weaken or replace that boundary.

## Current Baseline

A3S Box already provides:

- a dedicated-kernel libkrun MicroVM by default;
- an explicit Linux shared-kernel Sandbox through A3S OCI Runtime;
- no automatic fallback between isolation classes;
- a durable `ResolvedExecutionPlan` with the requested isolation, selected
  backend, isolation class, and mandatory controls;
- Sandbox capability probes for namespaces, seccomp, capability bounding,
  `no_new_privs`, subordinate IDs, and delegated cgroup v2;
- canonical host-path validation and typed read-write/read-only SDK mounts;
- TSI, disabled, and bridge network transports;
- JSON-lines operational audit events;
- checked Rust, Python, TypeScript, and Go local SDK protocol parity;
- retained 7,200-second `SEC-01` restricted-egress `G2` qualification on real
  Apple Silicon/HVF and Linux x86_64/KVM hosts.

The remaining gaps addressed by this roadmap are:

- the qualified Unix MicroVM egress boundary is limited to the documented TSI
  restricted-egress subset on Apple Silicon/HVF and Linux x86_64/KVM; it does
  not qualify other policy controls, transports, host classes, or the broader
  Box release matrix;
- A3S OCI Sandbox and Bridge transports still have no qualified egress
  enforcement boundary;
- `NetworkPolicy::Strict` and `NetworkPolicy::Custom` are represented but are
  rejected because packet filtering is not implemented;
- implemented host-mount admission and required receipts still need public
  bridge-v3/SDK delivery and retained real-host qualification evidence;
- the operational audit trail remains best-effort and deliberately separate
  from the implemented required, immutable security receipt;
- credentials injected into a guest remain visible to the guest workload;
- signed organization policy assignment belongs in A3S Cloud but has no Box
  verification contract yet.

## Design Rules

### Optional by default

The proposed request shape is deliberately small:

```rust
pub struct SandboxSecurityPolicy {
    pub egress: Option<EgressPolicy>,
    pub host_mounts: Option<HostMountPolicy>,
    pub receipt: Option<ReceiptPolicy>,
}
```

Absence of `SandboxSecurityPolicy`, or absence of one of its fields, means the
corresponding optional feature is disabled. New policy fields must use Serde
defaults so existing durable records retain their current meaning.

Convenience profiles may compose these typed objects, but profiles must expand
to explicit policy values before execution resolution. Business names such as
"coding", "research", or "CI" must not become runtime backends.

### Mandatory runtime invariants

The following existing safety properties are not optional:

- no silent isolation fallback;
- path canonicalization, traversal rejection, and ownership checks;
- execution generation fencing;
- runtime and agent artifact verification;
- capability checks before runtime mutation;
- fail-closed rejection of unsupported combinations;
- identity-validated cleanup of mounts, sockets, processes, cgroups, and
  runtime state.

### Transport is not authorization

Network transport and access policy remain separate concepts:

```text
Transport:    TSI | Bridge | None
Authorization: Unrestricted | DenyAll | Allowlist
```

The existing `NetworkMode` continues to select transport. A new
`EgressPolicy` authorizes destinations. The existing peer-oriented
`NetworkPolicy` is not overloaded with internet-domain semantics.

### Policy support is backend-specific and explicit

A policy may be accepted only when the selected backend advertises and proves
the required enforcement capability. Until a combination is implemented, the
resolver rejects it before image preparation or state mutation.

## Release A: Policy Foundation, Host Mount Admission, and Receipts

Estimated implementation effort: **12-16 engineer-days**, excluding real-host
soak duration.

### SEC-001 — Policy contract and threat model

- [x] Add the typed `SandboxSecurityPolicy` request model.
- [x] Define `HostMountPolicy`, `EgressPolicy`, and `ReceiptPolicy` as typed
      values rather than backend names or unstructured maps.
- [x] Document which threats each policy addresses and which threats remain
      outside the boundary.
- [x] Define canonical policy normalization and a deterministic SHA-256 policy
      digest.
- [x] Reject unknown variants, empty allowlists, ambiguous wildcard forms,
      invalid paths, invalid ports, and contradictory settings.
- [x] Add a regression fixture proving that an omitted policy produces the
      same resolved runtime configuration as the current release.

Exit gate:

- policy normalization is deterministic;
- invalid policies fail before runtime mutation;
- no-policy requests retain current behavior and serialization meaning.

### SEC-002 — Resolution, persistence, and recovery

- [x] Add the request policy to `CreateExecutionRequest` through its typed
      `BoxConfig`.
- [x] Add the normalized policy, policy digest, and required policy controls
      to `ResolvedExecutionPlan`.
- [x] Persist the request and resolved policy with the execution generation.
- [x] Validate the persisted plan against the original request during every
      recovery path.
- [x] Preserve the policy across create/start and restart.
- [x] Define snapshot restore semantics: a filesystem snapshot contains no
      authority to select a future execution policy; the restoring request
      supplies its own policy.
- [x] Ensure warm-pool lease reuse cannot retain a previous request's policy
      proxy, mount exception, credential, or receipt state.

Exit gate:

- restart and recovery reproduce the original resolved policy exactly;
- tampered or inconsistent state fails closed;
- unsupported policy/backend combinations create no runtime resources.

### MNT-101 — Host mount risk classification

The first release applies only to host bind mounts. Image filesystems, named
volumes, snapshots, and tmpfs mounts are not implicitly classified as host
secrets.

- [x] Classify host root mounts and broad user-home mounts.
- [x] Detect common credential paths such as `.ssh`, `.aws`, `.gnupg`, cloud
      provider configuration, and `.env*` files beneath a requested bind.
- [x] Detect Docker, containerd, Podman, and compatible host-control Unix
      sockets.
- [x] Detect sockets, devices, FIFOs, and other non-regular mount sources.
- [x] Provide an audit-only mode and an enforcing mode.
- [x] Provide typed, exact-path exceptions for intentionally exposed
      resources.
- [x] Require a distinct typed authorization for host-control sockets; a
      generic path allowlist must not grant them accidentally.
- [x] Keep the default policy disabled for compatibility.

### MNT-102 — Two-phase mount enforcement

- [x] Evaluate the mount policy during side-effect-free execution planning.
- [x] Revalidate source identity immediately before launch to detect symlink,
      replacement, and type changes.
- [x] Compile only approved exports into MicroVM host shares.
- [x] Compile only approved bind mounts into the A3S OCI Sandbox bundle.
- [x] Reject broad parent mounts containing protected descendants in the
      first version instead of claiming incomplete path carveouts.
- [x] Record every resolved bind source, target, access mode, classification,
      and exception in the security receipt.

The admission, backend-compilation, and receipt paths are implemented and
covered by core/runtime regression tests. The receipt carries the exact
`ResolvedHostMount` values used by both launch backends.

Required negative tests:

- symlink swap between planning and launch;
- source rename or deletion;
- root and whole-home binds;
- direct and parent-directory credential exposure;
- Docker/containerd socket exposure;
- device and FIFO sources;
- overlapping and duplicate targets;
- failed validation followed by exact resource-baseline recovery.

Exit gate:

- every accepted mount is represented identically in the resolved plan and
  backend configuration;
- every denied mount fails before workload launch;
- failure leaves no VM, Sandbox, share, mount, socket, cgroup, or durable
  partial record.

### RCT-101 — Security execution receipt

Define an immutable, versioned `SecurityReceiptV1` containing at least:

- request/configuration digest and policy digest;
- image and rootfs identity;
- requested isolation, selected backend, and isolation class;
- runtime and agent artifact digests;
- execution generation and owner identity;
- resolved bind mounts and access modes;
- effective egress policy;
- effective UID/GID mappings, capabilities, seccomp posture, and cgroup
  limits;
- host capability evidence digest;
- preparation result and launch timestamp.

Tasks:

- [x] Write receipts atomically under the execution-owned state root.
- [x] Add `ReceiptPolicy::Required`; a required receipt write failure blocks
      launch.
- [x] Keep the existing operational event log best-effort and separate from
      the required receipt contract.
- [x] Expose the receipt through `inspect` and typed SDK responses.
- [x] Redact environment values, credentials, secret contents, and sensitive
      proxy headers.
- [x] Validate receipt identity and generation during recovery.
- [x] Add tamper, truncation, interrupted-write, and stale-generation tests.

The implementation publishes owner-only, no-clobber generation files after
backend preparation and before the launch or resume operation. Runtime tests
cover publication failure rollback, exact mount and Sandbox control evidence,
secret redaction, create/resume/restart generation behavior, and recovery
rejection. Public platform support remains subject to the real-host evidence
gate described above; host-independent tests do not substitute for a real KVM,
HVF, WHPX, or A3S OCI launch.

Exit gate:

- a required receipt is durable before the workload is reported ready;
- a missing or invalid required receipt prevents recovery as a valid running
  execution;
- receipt output contains no secret values.

## Release B: Optional Egress Governance

Estimated implementation effort: **12-18 engineer-days**, excluding real-host
soak duration.

### NET-101 — Egress policy semantics

- [x] Add `EgressPolicy::DenyAll` and `EgressPolicy::Allowlist`.
- [x] Support explicit IP/CIDR, TCP/UDP protocol, and port rules.
- [x] Support exact HTTP/HTTPS hostnames and a narrowly defined subdomain
      pattern.
- [x] Normalize internationalized and case-insensitive hostnames safely.
- [x] Deny unmatched destinations by default when an allowlist is enabled.
- [x] Make IP-literal handling explicit; default allowlist behavior denies IP
      literals unless an IP/CIDR rule permits them.
- [x] Define bounded connection, DNS, timeout, and decision-log behavior.

First-release protocol boundary:

- HTTP and HTTPS hostname policies use a mandatory CONNECT-capable proxy;
- raw TCP and UDP are authorized by IP/CIDR and port, not by an unverified
  hostname claim;
- TLS interception is not included;
- arbitrary-protocol FQDN enforcement is not claimed.

The normalized contract binds every hostname rule to HTTP or HTTPS, an exact
canonical hostname pattern, and a port. A leading `*.` matches exactly one
non-empty label and never matches the suffix itself or a deeper descendant.
WHATWG/UTS #46 processing converts internationalized names to canonical
lowercase ASCII. Numeric and bracketed IP literals, including non-canonical
WHATWG IPv4 forms, are evaluated only against explicit TCP IP/CIDR rules.

Every allowlist includes immutable per-generation limits. The defaults are
256 active and 64 pending connections; 1,024 DNS cache entries, 32 answers per
query, and 1,024 queries per minute; positive DNS TTLs clamped to 1-300
seconds and negative TTLs to 30 seconds; 5-second DNS, 10-second connect, and
5-minute idle timeouts; and at most 10,000 decision records, 4 KiB per record,
and 8 MiB total. Policy resolution enforces hard ceilings. The engine must
deny new connection setup when a connection, DNS, timeout, or decision-log
budget is exhausted, without using an unrestricted fallback; it must reserve
space for a terminal redacted budget-exhaustion decision.

NET-101 completes the policy semantics shared by every backend. The resolver
currently accepts the implemented rule subset only for Unix MicroVM requests
using `NetworkMode::Tsi`: HTTP/HTTPS hostname rules plus raw IPv4 TCP IP/CIDR
rules. It rejects A3S OCI Sandbox, Bridge and disabled transports, raw UDP,
raw IPv6, published ports, sidecars, warm pools, snapshot-fork, and VM restore
before mutation.

### NET-102 — Policy engine and host egress proxy

- [x] Compile normalized rules into one immutable per-generation policy.
- [x] Implement bounded allow/deny evaluation before outbound connection
      creation.
- [x] Emit structured, redacted decisions with execution and generation
      identity.
- [x] Fail launch if a required proxy listener or policy channel is absent.
- [x] Bound concurrent connections, pending connection setup, DNS state, and
      log volume.
- [x] Ensure proxy termination is visible and cleanup closes every owned
      listener and connection.

The host-independent implementation compiles one policy per execution
generation, creates no-clobber owner-only decision and policy-channel
artifacts, binds the HTTP proxy to host loopback, authenticates guest proxy
requests with a generation token, and fails closed when policy or decision-log
budgets are exhausted. VM health observes proxy-task failure. Boot failure,
normal destroy, and explicit proxy stop close both background services and
remove the policy socket.

### NET-103 — MicroVM TSI enforcement

- [x] Enforce raw IPv4 TCP IP/CIDR/port rules at the inherited host netproxy
      boundary.
- [x] Disable direct outbound reachability when a hostname allowlist is
      enabled.
- [x] Route allowed HTTP/HTTPS requests through the mandatory host proxy.
- [x] Replace caller proxy variables, remove `ALL_PROXY`, constrain
      `NO_PROXY`, and keep direct TSI disabled independently of guest
      environment values.
- [x] Fence all proxy state and decisions by execution generation.
- [x] Reject warm-pool reuse, snapshot-fork, snapshot restore, sidecars, and
      published ports before they could inherit or bypass policy state.
- [x] Qualify the complete 14-case bypass and cleanup matrix on a real Apple
      Silicon/HVF host, including proxy-variable bypass, direct/raw IP,
      DNS/UDP/IPv6/DoH/QUIC attempts, policy-channel loss, generation restart,
      concurrent policy isolation, decisions, and cleanup.
- [x] Run the same complete `R0` matrix on real Linux x86_64/KVM.
- [x] Retain verified 7,200-second `G2` bundles for both KVM and HVF.

The selected public transport remains `NetworkMode::Tsi` for compatibility,
but an enabled restriction is materialized as a fixed IPv4 virtio-net
boundary: guest `10.90.0.2/24`, gateway `10.90.0.1`, and authenticated proxy
port `3128`. Direct libkrun TSI socket impersonation is disabled while a plain
vsock retains the control plane. No guest DNS forwarder is installed.
Host-independent unit and simulated end-to-end tests exercise the policy
channel, gateway route, raw TCP authorization, HTTP/HTTPS proxy, adversarial
environment replacement, proxy failure, and lifecycle cleanup. The real-host
runner and evidence verifier now enforce the exact 14-case matrix per
iteration. Apple Silicon/HVF and Linux x86_64/KVM `R0` and retained `G2`
evidence pass. The 2026-07-31 `G2` qualification contains 286 passing HVF
iterations over 7,203 seconds and 336 passing KVM iterations over 7,215
seconds, with zero failed iterations and no final resource-count growth.

### NET-104 — A3S OCI Sandbox enforcement

- [ ] Retain the private network namespace as the outer network boundary.
- [ ] Expose only the authenticated, generation-scoped host proxy path for a
      hostname allowlist.
- [ ] Compile IP/CIDR/port policy into an enforceable Sandbox network plan.
- [ ] Extend A3S OCI Runtime capability evidence with the exact egress control
      it applied.
- [ ] Fail closed if namespace, proxy, routing, or filtering preparation
      cannot be proved.
- [ ] Verify terminal cleanup removes namespace, route, filter, proxy, socket,
      owner session, and cgroup state.

### NET-105 — Bridge transport boundary

`Bridge + EgressPolicy` remains rejected until host gateway enforcement is
implemented and qualified.

- [ ] Select and document a host-gateway nftables/eBPF or mandatory egress
      gateway design.
- [ ] Preserve network and execution generation identity in every rule.
- [ ] Make rule installation and deletion atomic with endpoint lifecycle.
- [ ] Add restart, stale-rule, IP reuse, concurrent network, and cleanup tests.
- [ ] Advertise Bridge policy support only after real KVM/HVF/WHPX evidence
      exists for each claimed platform.

Required bypass tests:

- direct IPv4 and IPv6;
- IP literals under hostname-only policy;
- DNS rebinding and expired DNS associations;
- custom DNS, DNS over HTTPS, and QUIC attempts;
- removed proxy environment variables and `NO_PROXY=*`;
- raw TCP and UDP;
- proxy termination and restart;
- concurrent executions with different policies;
- restart, warm-pool lease reuse, and snapshot restore;
- complete route, filter, socket, process, and descriptor cleanup.

Exit gate:

- every advertised backend prevents direct bypass of an enabled allowlist;
- unsupported transport combinations fail before mutation;
- no policy state crosses execution generations;
- decision records are bounded and contain no credentials.

## Release C: Credential Brokering and Managed Policy

Estimated implementation effort: **10-15 engineer-days**, excluding A3S
Cloud deployment qualification.

### CRED-101 — Host credential broker

- [ ] Keep brokered credentials outside the guest workload.
- [ ] Bind each credential to an exact policy, destination, method/path scope
      where available, execution identity, and expiry.
- [ ] Inject a credential only after the egress request passes policy.
- [ ] Prevent credentials from appearing in environment variables, guest
      files, logs, receipts, metrics, or errors.
- [ ] Revoke and zeroize credentials on execution cleanup.
- [ ] Add negative tests for destination confusion, redirects, proxy reuse,
      stale generation, malformed requests, and log disclosure.

Credential brokering depends on Release B. It must not be exposed over an
unrestricted or bypassable network path.

### CLOUD-101 — Signed managed policy

A3S Cloud owns policy authoring, versioning, signing, assignment, revocation,
and organization-level audit. A3S Box owns signature verification, digest
binding, local enforcement, and execution evidence. A3S OCI Runtime owns only
the platform enforcement and evidence it advertises.

- [ ] Define a versioned signed policy bundle with explicit tenant, policy,
      revision, issue, expiry, and key identity.
- [ ] Add a typed Box policy verifier/provider extension.
- [ ] Keep local SDK operation credential-free when no Cloud provider is
      selected.
- [ ] Reject expired, revoked, malformed, ambiguously assigned, or
      unverifiable bundles before mutation.
- [ ] Bind the verified bundle digest and signing identity into the execution
      receipt.
- [ ] Add offline cached-policy rules with explicit expiry and no silent
      fallback to an unsigned local policy.
- [ ] Update `compat/cloud-stack.acl` with the Box, Cloud, ACL, and protocol
      revisions in one compatibility change.
- [ ] Run the Cloud lock verifier and contract fixtures before promotion.

## SDK and CLI Delivery

Rust remains the implementation source of truth. Every public feature must
reach Rust, synchronous/asynchronous Python, TypeScript, and Go in the same
release.

The current machine bridge is strict protocol v2. A policy-bearing request
must not be sent to an older runtime that could ignore unknown fields, so the
public policy release requires protocol v3.

- [ ] Add typed policy values to Rust `SandboxCreateOptions` and
      `SandboxBuilder`.
- [ ] Add policy fields and receipt results to bridge protocol v3.
- [ ] Update the checked bridge contract and capability response.
- [ ] Add equivalent builders and immutable option values in Python,
      TypeScript, and Go.
- [ ] Make an SDK/runtime protocol mismatch fail before mutation with the
      stable protocol error category.
- [ ] Add one cross-language golden request per policy variant.
- [ ] Add one real MicroVM and one real A3S OCI Sandbox policy lifecycle per
      language.
- [ ] Keep CLI policy input in ACL by default; CLI flags may provide small
      conveniences but must compile into the same typed request.
- [ ] Do not add endpoint, account, or API-key configuration to the local SDK.

Preferred builder shape:

```rust
let sandbox = client
    .sandbox(image)
    .host_mount_policy(HostMountPolicy::agent_safe())
    .egress_policy(EgressPolicy::allow_domains(["api.openai.com"]))
    .receipt_policy(ReceiptPolicy::required())
    .start()
    .await?;
```

## Repository Ownership

| Area | Owner | Responsibility |
| --- | --- | --- |
| `a3s-box-core` | A3S Box | Typed policy, validation, resolution, digest, receipt schema |
| Box runtime/local execution | A3S Box | Persistence, recovery, mount admission, backend dispatch, cleanup |
| `a3s-box-netproxy` | A3S Box | Restricted MicroVM virtio-net egress enforcement and decision evidence |
| A3S OCI bundle compiler | A3S Box | Compile resolved Sandbox intent, reject unsupported combinations |
| A3S OCI Runtime | OCI Runtime | Enforce and attest shared-kernel namespace/filter controls |
| Rust/Python/TypeScript/Go SDKs | A3S Box | Typed request and response parity over bridge v3 |
| A3S Cloud | Cloud | Signed policy authority, assignment, revocation, organization audit |

No layer may introduce a second lifecycle owner or invoke a backend outside
the canonical Box execution manager.

## Test and Promotion Plan

### Pull request gates

- [ ] `cargo fmt --all -- --check` from `src/`.
- [ ] Focused core, runtime, CLI, bridge, and SDK tests.
- [ ] Python package tests for synchronous and asynchronous policy requests.
- [ ] TypeScript build and tests.
- [ ] Go formatting, vet, unit tests, and race tests.
- [ ] No-policy behavior regression fixtures.
- [ ] Unsupported-policy zero-mutation tests.
- [ ] Secret and credential log scanning.
- [ ] Bridge v3 cross-language contract verification.

### Real-runtime matrix

| Lane | Required evidence |
| --- | --- |
| Linux x86_64/KVM MicroVM | Mount policy, TSI egress, receipt, recovery, cleanup |
| Linux arm64/KVM MicroVM | Affected policy controls before arm64 promotion |
| Apple Silicon/HVF MicroVM | Mount policy, TSI egress, receipt, cleanup |
| Windows x86_64/WHPX MicroVM | Supported controls; unsupported combinations reject before mutation |
| Linux A3S OCI Sandbox | Mount, proxy/network, seccomp/cgroup posture, recovery, cleanup |
| Kubernetes RuntimeClass | Only after a policy is exposed through CRI |
| A3S Cloud managed host | Signed bundle, assignment, expiry, revocation, receipt binding |

### Soak profiles

- [x] Add a `SEC-01` optional policy scenario to
      [the soak plan](docs/soak-test-plan.md).
- [x] Extend the host evidence contract for egress bypass, proxy failure, and
      rule cleanup.
- [ ] Extend `STO-01` for sensitive bind admission and path-race cases.
- [ ] Extend `OBS-01` for required receipts, decision logs, rotation, and
      redaction.
- [ ] Run `R0` after runner, schema, backend, or policy changes.
- [x] Run restricted-egress `G2` on the currently qualified Apple
      Silicon/HVF and Linux x86_64/KVM MicroVM backends.
- [ ] Run `G2` on every additional backend before advertising it.
- [ ] Run `R24` for the release candidate.
- [ ] Run `E72` before broad promotion of low-level netproxy, namespace,
      routing/filtering, or durable-state format changes.

No release claim is made from unit tests, simulated TEE evidence, or a hosted
build that did not run the selected backend.

## Delivery Order

```text
SEC-001  Policy contract and threat model
    |
SEC-002  Resolution, persistence, and recovery
    |
    +----------------+----------------+
    |                |                |
MNT-101/102       RCT-101         NET-101/102
    |                |                |
    +----------------+---------- NET-103/104
                     |
              Bridge protocol v3
                     |
          Rust/Python/TypeScript/Go parity
                     |
             Real-host R0/G2/R24
                     |
            CRED-101 and CLOUD-101
```

Release A is implemented with host-independent regression coverage. NET-102
and the Unix MicroVM subset of NET-103 are also implemented; real-host
qualification, Sandbox/Bridge enforcement, bridge protocol v3, cross-language
SDK delivery, credential brokering, and managed Cloud policy remain.

## Completion Criteria

This roadmap is complete only when:

- every checked task in Releases A, B, and C is implemented or explicitly
  removed from the public scope;
- no-policy behavior remains compatible with the pre-policy release;
- enabled controls fail closed on every advertised backend;
- Bridge support is advertised only after real gateway enforcement evidence;
- all four native SDKs expose the same typed policy and receipt contract;
- execution recovery preserves policy identity and never crosses generations;
- required receipts are durable, inspectable, redacted, and tamper-detecting;
- signed Cloud policy passes the compatibility lock and contract fixtures;
- the required real-host soak evidence is retained with the release candidate;
- README, SDK documentation, changelog, threat model, and soak coverage describe
  the verified implementation rather than planned behavior.
