# E2B Compatibility Contract Fixture

This directory pins the public E2B contracts that A3S Box implements. It is
the source of truth for protocol generation, official-client conformance, and
the Python and TypeScript public API alignment checks.

The fixture does not claim full compatibility. The generated manifest keeps
`full_compatibility` set to `false` until the complete black-box release gate
in `docs/e2b-compatible-sdk-design.md` passes.

## Pinned sources

`upstream.lock.json` records immutable repository commits, package versions,
source paths, SHA-256 digests, and the control-plane tags selected by the pinned
official SDK codegen. The vendored schemas and public export entry points retain
their upstream licenses below `spec/`.

The same lock records the published Python wheels and npm tarballs used by
black-box fixtures. Fixture runners download those exact artifacts and verify
both SHA-256 and, for npm, the published integrity value before installing.

The first tuple pins:

- Python `e2b` 2.32.0 and TypeScript `e2b` 2.33.0;
- Python `e2b-code-interpreter` 2.8.1;
- TypeScript `@e2b/code-interpreter` 2.6.1.

## Generated evidence

The `a3s-box-e2b-contract` tool produces:

- `inventory/contracts.json`: OpenAPI operations, parameters, response errors,
  schema fields, authentication headers, Protobuf services/descriptors, and
  MCP schema fields;
- `inventory/public-exports.json`: the pinned Python and TypeScript top-level
  public exports for the base and Code Interpreter packages;
- `manifests/v1.json`: the tested version tuple and contract/inventory digests.

Generate and verify from the Box repository:

```bash
cd src
cargo run -p a3s-box-compat --bin a3s-box-e2b-contract -- generate
cargo run -p a3s-box-compat --bin a3s-box-e2b-contract -- verify
```

`protoc` must be available because the Protobuf inventory is generated from a
real descriptor set rather than a hand-written parser. CI verifies that the
vendored sources, inventories, and manifest remain byte-for-byte consistent.

## Updating the pin

1. Select explicit upstream commits and package versions.
2. Review licenses and the upstream protocol diff.
3. Replace the vendored files and update every source digest in
   `upstream.lock.json`.
4. Regenerate the inventories and manifest.
5. Review the machine-readable diff and update server/SDK conformance fixtures.
6. Run the unchanged official Python sync, Python async, and TypeScript clients
   before advertising the new tuple.

Never edit generated inventories by hand or infer compatibility from matching
method names alone.

## Production control service

`a3s-box-e2b` composes the lifecycle router with the SQLite repository, the
canonical A3S runtime manager, production credential providers, startup
reconciliation, and periodic expiry maintenance. It requires a `.acl` file
parsed by `a3s-acl`; literal sandbox token keys are rejected in favor of
`env("VARIABLE")` references.

Sandbox expiry is measured from the later of runtime start and observed envd
readiness, so cold startup does not consume the caller's requested usable
timeout. Startup reconciliation applies the same lifetime rule when it recovers
a creating record whose execution was committed before the service restarted.

Run it from the Rust workspace:

```bash
cargo run --locked -p a3s-box-compat --bin a3s-box-e2b -- \
  --config /etc/a3s-box/e2b.acl
```

The validated schema and an operator example are documented in
[`docs/e2b-compatible-sdk-design.md`](../../docs/e2b-compatible-sdk-design.md#configuration).
This process exposes the lifecycle control subset plus an authenticated
wildcard TLS data-plane edge. The edge supports direct and shared route forms,
HTTP/1.1 and HTTP/2 streaming proxying, CORS preflight, upgrades, bounded
connections, and generation-fenced access to real Sandbox loopback ports.
Templates select host-broker or in-Sandbox runtime envd placement explicitly.
Runtime placement proxies health, Process, Filesystem, and file HTTP routes to
port `49983` only after a fenced readiness connection succeeds. Authenticated
terminal health remains host-resolved: it returns the official-client `502`
for a killed lifecycle record without reopening a live route lease. Invalid
tokens remain unauthorized. The remaining pinned envd HTTP and ConnectRPC
protocols are still required before the manifest can set
`full_compatibility=true`.

The destructive A3S OS integration harness is
[`scripts/e2b-production-smoke.sh`](../../scripts/e2b-production-smoke.sh). It
requires a dedicated runtime home and explicit acknowledgement, and verifies a
real Sandbox lifecycle, TLS direct/shared routing, token-scope denial, service
restart recovery, host envd health, a traffic-scoped workload service on port
`49999`, stale-route fencing, authenticated terminal health, and resource
cleanup. With
`A3S_BOX_E2B_OFFICIAL_CLIENTS=1`, it additionally runs the checksum-pinned,
unchanged Python sync, Python async, TypeScript, and Code Interpreter packages
through the production lifecycle listener, calls their official running-state
health methods through the TLS gateway before and after kill, and verifies
cleanup of every real `crun` execution. With the runtime image selected, the
three base clients also exercise Filesystem create/read/stat/list/rename/remove,
foreground and background commands, process listing, stdin close, and PTY
resize. The Code Interpreter clients execute code and cover context
create/list/restart/remove. This does not establish complete Process,
Filesystem, PTY, rich-result, or multi-language compatibility.

The default smoke uses the small Alpine broker fixture. Set
`A3S_BOX_E2B_RUNTIME_IMAGE` to an immutable
`ghcr.io/a3s-lab/box-e2b-runtime` tag or digest to generate runtime-mode
template policies and exercise envd plus Code Interpreter health inside that
image. This mode is destructive and retains the same dedicated-home,
credential, restart, stale-route, and cleanup requirements.

Set `A3S_BOX_E2B_NATIVE_SDKS=1` together with
`A3S_BOX_E2B_OFFICIAL_CLIENTS=1` to repeat that same matrix through the
repository's Python and TypeScript packages. The runner uses the pinned
official dependencies already installed for the unchanged-client pass, builds
and packs the native TypeScript package, and keeps A3S endpoint configuration
local to each client invocation.
