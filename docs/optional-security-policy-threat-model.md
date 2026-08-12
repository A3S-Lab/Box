# Optional Security Policy Threat Model

Status: **In development**

Scope: request-scoped, opt-in host mount, outbound network, and execution
receipt policies for A3S Box MicroVM and shared-kernel Sandbox executions.

## Security boundary

A3S Box provides two explicit isolation classes:

| Isolation | Boundary | Intended use |
| --- | --- | --- |
| MicroVM | Dedicated guest kernel through libkrun | Untrusted and multi-tenant workloads |
| Sandbox | Linux namespaces and controls through A3S OCI Runtime | Trusted or constrained workloads that accept a shared host kernel |

Optional security policies add defense in depth to either class. They do not
make a shared-kernel Sandbox equivalent to a MicroVM, and policy resolution
must never silently change the requested isolation class.

The trusted computing base includes A3S Box, the selected backend, the host
kernel and administrator, pinned runtime artifacts, the policy compiler, and
the durable execution-state store. A MicroVM also trusts libkrun and its
virtual device boundary. A Sandbox additionally trusts the host kernel to
enforce its namespaces, seccomp policy, capabilities, and cgroups correctly.

## Compatibility and failure contract

The complete security policy is optional. Omitting it preserves the behavior
and serialized meaning of executions created before the policy feature.
Individual controls are also optional.

Selecting a control changes the contract:

- the request is normalized and bound to a deterministic digest before any
  runtime mutation;
- the selected backend must advertise and prove every required capability;
- incomplete or unsupported enforcement rejects the request before launch;
- persisted policy identity is validated during restart and recovery;
- warm-pool reuse, snapshot restore, and generation changes cannot inherit
  another execution's policy state;
- a failure cannot trigger a fallback to another isolation class.

Audit-only mount policy reports findings but deliberately does not block the
mount. This weaker behavior is represented by an explicit typed mode and must
not be reported as enforcement.

## Assets protected

The policies are designed to protect:

- host files, credential directories, runtime sockets, devices, and special
  files from unintended bind-mount exposure;
- services and networks from outbound connections that the execution was not
  authorized to make;
- execution identity and security decisions from undetected durable-state
  drift;
- logs and receipts from accidental credential or secret disclosure;
- concurrent and later execution generations from stale mount, proxy,
  credential, or receipt state.

## Host mount policy

`HostMountPolicy` applies only to host bind mounts. It classifies broad root
and home-directory mounts, common credential paths, container control sockets,
devices, sockets, FIFOs, and other special sources. Exact-path exceptions and
host-control-socket authorization are separate typed values.

The enforcing mode addresses accidental or malicious exposure caused by:

- binding the host root or a whole user home into a workload;
- binding a parent directory that contains SSH, cloud, package-manager, or
  environment credentials;
- exposing Docker, containerd, Podman, or compatible control sockets;
- changing a validated source through a symlink, rename, replacement, or file
  type race before launch.

The policy does not inspect or classify image filesystems, named volumes,
snapshots, or tmpfs as host bind mounts. It also cannot protect data that an
authorized caller deliberately exposes through an exact exception. The first
enforcing version rejects a broad parent containing protected descendants; it
does not claim a partial path carveout that the backend cannot prove.

Mount identity and classification are checked during request planning, again
while recovering durable state, when backend mounts are compiled, and once
more immediately before the runtime launch call. These checks detect changes
that occur before the final validation. They do not provide an atomic,
file-descriptor-pinned mount transaction, so a privileged host process that
can replace a source after the final check remains inside the trusted host
boundary.

## Lifecycle reuse semantics

A filesystem snapshot carries rootfs contents and resolved image metadata. It
does not carry authority to select the policy of a later execution. Every
restore is a new creation request: that request is normalized independently,
receives its own policy digest and mount evidence, and may omit policy without
inheriting one from the snapshot source.

Restart and control-plane recovery retain the original creation request,
normalized policy, digest, mount evidence, and execution generation. Recovery
recomputes the plan from the persisted request and current host evidence; a
different policy, changed mount source, or inconsistent digest fails closed.

Warm-pool templates currently reject every optional security policy before
mutation. Policy-bearing leases remain unsupported until lease checkout can
atomically reset and revalidate policy proxies, mount exceptions, credentials,
receipts, and all generation-scoped backend state. Rejection is the current
anti-inheritance guarantee; the runtime does not claim policy-aware pool reuse.

## Egress policy

`EgressPolicy` is separate from network transport. `NetworkMode` chooses TSI,
Bridge, or no transport; the egress policy authorizes destinations and ports.

`DenyAll` and `Allowlist` are intended to address direct outbound access,
unapproved IP ranges and ports, hostname confusion, proxy bypass, stale
generation rules, and cross-execution policy reuse. Hostname rules require a
mandatory host proxy for supported HTTP and HTTPS traffic. Raw TCP and UDP are
authorized by IP or CIDR and port, not by an unverified hostname claim.

The normalized first-release rule shapes are deliberately separate:

- an HTTP rule binds `http` or `https`, one canonical hostname pattern, and
  one port;
- an IP rule binds one canonical IPv4/IPv6 address or CIDR, `tcp` or `udp`,
  and one port;
- a hostname may be exact or start with one `*.`; that wildcard matches
  exactly one label, never the suffix itself and never multiple labels;
- UTS #46/WHATWG host parsing converts internationalized and case-insensitive
  names to lowercase ASCII, while percent-encoded, malformed, ambiguous
  wildcard, and hostname-shaped IP inputs are rejected;
- an HTTP IP literal is evaluated as TCP against the IP rules. A hostname rule
  can never authorize an IP literal or raw transport.

An allowlist denies every unmatched destination. Its normalized digest also
binds limits for active and pending connections, DNS cache entries, answers,
query rate, positive and negative TTLs, DNS/connect/idle timeouts, and decision
record count and bytes. Exhausting one of these budgets does not select an
unrestricted fallback: new connection setup is denied and a reserved,
redacted terminal decision records the exhausted budget. The default and hard
limit values are defined by `EgressPolicyLimits` and are enforced during
policy resolution before runtime mutation.

The implemented enforcement subset is deliberately narrower than the semantic
model. On a Unix MicroVM request using `NetworkMode::Tsi`, an enabled
restriction creates a fixed IPv4 virtio-net boundary instead of permitting
direct libkrun TSI internet-socket handling:

- the guest uses `10.90.0.2/24` with gateway `10.90.0.1`;
- each raw IPv4 TCP connect is authorized through an owner-only Unix policy
  socket before the host connect is created;
- HTTP and HTTPS hostname traffic is routed through an authenticated,
  generation-scoped host-loopback proxy exposed at guest gateway port `3128`;
- guest DNS forwarding is absent, so hostname resolution occurs only in the
  mandatory host proxy;
- caller-supplied proxy variables are replaced, `ALL_PROXY` is removed, and
  `NO_PROXY` is constrained without re-enabling direct TSI;
- proxy-task failure makes the VM unhealthy, and boot failure or destroy
  closes listeners and removes the generation socket.

The resolver rejects combinations outside that boundary before runtime
mutation: A3S OCI Sandbox, Bridge and disabled transports, raw UDP, raw IPv6,
published ports, sidecars, warm pools, snapshot-fork, and VM restore. The
semantic model retains TCP/UDP and IPv4/IPv6 rule types so future backends can
implement them without changing policy meaning; their presence does not imply
current enforcement support.

The current implementation does not provide TLS interception, content
inspection, or arbitrary-protocol FQDN enforcement. Host-independent tests
cover policy decisions, IP-literal handling, DNS answer validation, mandatory
gateway routing, adversarial proxy environment replacement, authentication,
task failure, and cleanup. Real KVM/HVF tests must still exercise DNS
rebinding, custom DNS, DNS over HTTPS, QUIC, cleared proxy variables,
`NO_PROXY=*`, raw sockets, and direct IPv4/IPv6 bypass attempts before the
Unix MicroVM subset is advertised as qualified. Bridge transport remains
unsupported until host-gateway enforcement is implemented and qualified.

An egress allowlist limits network destinations; it does not make an allowed
service trustworthy and does not prevent sensitive data from being sent to an
explicitly allowed destination.

## Required execution receipt

`ReceiptPolicy::Required` makes versioned execution evidence a launch
precondition. The receipt binds the request and policy digests to the selected
backend, isolation class, image, artifacts, generation, resolved mounts,
network policy, identity mappings, runtime controls, and capability evidence.

This addresses missing, stale, truncated, or inconsistent evidence during
inspection and recovery. Atomic creation and identity validation make
accidental corruption and unsophisticated tampering detectable. The receipt
must exclude environment values, credentials, secret contents, and sensitive
proxy headers.

A local digest is not a remote attestation and does not protect against a
host administrator who can replace both state and verification code. Stronger
organization provenance depends on a signed A3S Cloud policy bundle and, where
supported, hardware-backed attestation.

The existing operational event log remains best-effort observability. It must
not be presented as a required security receipt.

## Explicit non-goals

These optional policies do not protect against:

- a working host-kernel exploit from a shared-kernel Sandbox;
- a malicious host administrator, compromised Box daemon, or compromised
  policy compiler;
- hardware side channels or denial of service outside configured resource
  controls;
- secrets deliberately placed in a guest, image, mounted volume, command
  argument, or allowed network response;
- malicious behavior by an explicitly authorized destination;
- integrity of unsigned policy received from an untrusted control plane;
- unrestricted host control granted through a separately authorized runtime
  socket.

MicroVM remains the required boundary when the shared host kernel is outside
the workload's trust model. TEE and signed managed policy are separate features
and must be described only where their platform evidence is real.

## Required validation

Unit tests cover typed deserialization, invalid and ambiguous policy rejection,
canonical normalization, and stable digests. Integration tests must prove
zero mutation on unsupported policy/backend combinations, two-phase mount
identity validation, persisted-plan drift rejection, receipt durability, and
generation-scoped cleanup.

Public support additionally requires real-host bypass and lifecycle evidence
for every advertised operating system, architecture, backend, and isolation
class. Simulated TEE output, unit-only evidence, or a build that did not launch
the selected backend is insufficient.
