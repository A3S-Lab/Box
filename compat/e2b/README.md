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
connections, and generation-fenced access to real Sandbox loopback ports. A
host-side broker implements authenticated envd `GET /health`; it returns `204`
only after the runtime manager confirms the leased execution ID and generation
are still running, and returns the official-client terminal `502` for a killed
lifecycle record only after validating its envd token. Runtime-envd templates
proxy the remaining data plane into the Sandbox after fail-closed initialization.
Invalid tokens remain unauthorized, and no live route lease is reopened.

The destructive A3S OS integration harness is
[`scripts/e2b-production-smoke.sh`](../../scripts/e2b-production-smoke.sh). It
requires a dedicated runtime home and explicit acknowledgement, and verifies a
real Sandbox lifecycle, v1 running-list behavior, monotonic refresh with an
optional body, current batch metrics, TLS direct/shared routing, token-scope
denial, service restart recovery, envd health/metrics/environment,
metadata-preserving HTTP file upload and download, a traffic-scoped workload
service on port `49999`, stale-route fencing, and resource cleanup. The default
`localhost.localdomain` wildcard is DNS- and TLS-preflighted before a Sandbox
starts. With
`A3S_BOX_E2B_OFFICIAL_CLIENTS=1`, it additionally runs the checksum-pinned,
unchanged Python sync, Python async, TypeScript, and Code Interpreter packages
through the production lifecycle listener, calls their official running-state
health methods through the TLS gateway before and after kill, and verifies
cleanup of every real `crun` execution. The clients exercise foreground and
background commands, process listing, stdin send/close, wait, PTY
create/resize/input/wait, Filesystem remove/mkdir/write/read/stat/list/rename,
current Sandbox metrics with historical-range filtering, and Python Code
Interpreter execution plus context create/list/run/restart/remove.

With `A3S_BOX_E2B_NATIVE_SDKS=1`, the harness repeats that matrix through the
A3S Python sync/async and TypeScript packages after removing every `E2B_*`
connection variable and configuring only `A3S_BOX_*`. This production subset
passes on A3S OS with certified `crun`, but it does not establish full protocol
compatibility. Templates, snapshots, volumes, volume-content, signed files,
historical metrics, MCP, additional signals, reconnect, cancellation,
backpressure, multi-file and large-file behavior, and other pinned edge cases
remain outside the matrix, so `full_compatibility=false` remains mandatory.
