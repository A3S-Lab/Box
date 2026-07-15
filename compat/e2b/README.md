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
connections, and generation-fenced access to real Sandbox loopback ports. The
pinned envd/ConnectRPC protocols and remaining data-plane behavior are still
required before the manifest can set `full_compatibility=true`.

The destructive A3S OS integration harness is
[`scripts/e2b-production-smoke.sh`](../../scripts/e2b-production-smoke.sh). It
requires a dedicated runtime home and explicit acknowledgement, and verifies a
real Sandbox lifecycle, TLS direct/shared routing, token-scope denial, service
restart recovery, stale-route fencing, and resource cleanup. With
`A3S_BOX_E2B_OFFICIAL_CLIENTS=1`, it additionally runs the checksum-pinned,
unchanged Python sync, Python async, TypeScript, and Code Interpreter packages
through the production lifecycle listener and verifies cleanup of every real
`crun` execution. That optional matrix does not exercise envd commands, files,
PTY, or interpreter execution.
