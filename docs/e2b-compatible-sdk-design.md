# E2B Protocol Compatibility and SDK Design

Status: **Proposed**

Scope: protocol compatibility, Python and TypeScript SDKs, and the service
boundary required to provide a remote code-execution environment on A3S Box.

Target: the public E2B SDK contract as observed on 2026-07-14. Compatibility is
pinned by upstream commit and generated protocol descriptors, not by an
unversioned claim.

## Executive decision

A3S Box should provide an E2B-compatible control plane and sandbox endpoint so
the official E2B Python and JavaScript SDKs can connect to A3S by changing only
connection configuration such as `E2B_API_URL`, `E2B_DOMAIN`, and credentials.

A3S should also publish native Python and TypeScript packages with the same
public object model and behavior. Those packages are convenience clients, not
the proof of compatibility. The compatibility gate is an unmodified upstream
SDK running its contract suite against an A3S deployment.

Delivery is protocol-first. A3S must implement the server contracts before it
implements native convenience SDKs. Forking an upstream SDK, replacing its
transport, adding an A3S-only constructor argument, or requiring application
source changes does not satisfy compatibility.

`E2B-compatible` is a release-level claim, not a description of an endpoint.
A release may expose an explicitly named preview subset while it is being
built, but it must not claim full compatibility until every public operation
used by the pinned official clients passes the semantic conformance suite. A
response saying that an operation is unsupported does not count when the same
request succeeds on the pinned upstream contract.

The compatibility service must submit backend-neutral A3S execution requests.
It must never invoke `crun`, libkrun, or a shim directly. Isolation selection,
feature rejection, capability probing, audit records, and cleanup remain owned
by A3S Box.

## What “fully compatible” means

The target is the complete public SDK protocol, including:

1. Control-plane REST/OpenAPI behavior for sandbox lifecycle, listing,
   pagination, metadata, metrics, timeout, network policy, snapshots,
   templates, and volumes.
2. Per-sandbox envd HTTP behavior for health, metrics, environment, file upload,
   and file download.
3. ConnectRPC/Protobuf behavior for commands, process attachment, stdin,
   signals, PTY, filesystem metadata, directory operations, and watches.
4. Code Interpreter behavior for code contexts, streamed execution, rich MIME
   results, stdout/stderr, errors, and execution counts.
5. Python sync and async APIs, TypeScript APIs, error classes, timeout units,
   stream ordering, cancellation, and pagination semantics.
6. Routing, authentication headers, status codes, and response bodies expected
   by an unmodified official client.
7. Volume-content upload, download, directory, and path operations from the
   separate volume-content OpenAPI contract.
8. Public sandbox ports, traffic tokens, signed file URLs, and the MCP gateway
   behavior reached through the generic sandbox routing contract.

Compatibility does not include private vendor administration endpoints or
undocumented infrastructure internals unless a public SDK calls them. Public
template, snapshot, volume, and access-token operations are in scope.

Every supported upstream version is recorded in a compatibility manifest:

```json
{
  "e2b_git_commit": "423a1b73025ce871d9b9bfe338396c6b316be845",
  "code_interpreter_git_commit": "5aeca43fe3fae2df260b1fb17c71fed5b5dac852",
  "python_e2b_version": "2.32.0",
  "typescript_e2b_version": "2.33.0",
  "python_code_interpreter_version": "2.8.1",
  "typescript_code_interpreter_version": "2.6.1",
  "control_openapi_digest": "sha256:...",
  "envd_openapi_digest": "sha256:...",
  "volume_content_openapi_digest": "sha256:...",
  "process_descriptor_digest": "sha256:...",
  "filesystem_descriptor_digest": "sha256:...",
  "mcp_schema_digest": "sha256:...",
  "a3s_compat_version": "..."
}
```

The first manifest is generated during implementation from vendored source;
digests must not be copied manually from this design document.

The manifest identifies a tested version tuple rather than promising
compatibility with all past or future package versions. Adding a version means
running the complete official-client matrix and publishing the result; a
semver range inferred from similar schemas is not sufficient.

### Release gate

A release may use the unqualified `E2B-compatible` label only when all of the
following are true for a published compatibility manifest:

1. The pinned official Python sync, Python async, and TypeScript packages run
   unchanged. Only their documented API URL, sandbox domain, and credential
   configuration may differ.
2. Every public operation in the manifest has a black-box conformance result
   covering its request, response, error, timeout, cancellation, and streaming
   semantics.
3. Wildcard sandbox routing, signed file URLs, public ports, access tokens, and
   reconnect behavior work through the production TLS edge rather than a
   single-sandbox test shortcut.
4. Code Interpreter and MCP fixtures pass through the same generic sandbox
   routing and process/filesystem services used in production.
5. No response in the upstream namespace requires an A3S-only field or exposes
   an A3S-only error for behavior that succeeds against the pinned contract.

The conformance report is published per template and isolation profile. A
shared-kernel template cannot inherit a passing MicroVM result. Until pause,
resume, snapshots, volumes, and the rest of the pinned lifecycle surface have
matching observable behavior on the Sandbox backend, that backend is reported
as a preview subset rather than fully compatible.

## Compatibility architecture

Compatibility is implemented as one versioned product surface with separate
components and ownership boundaries:

```text
Official or A3S Python/TypeScript SDK
                  |
          TLS edge and route parser
          /                     \
 Control-plane API       Sandbox data-plane gateway
          |              /       |        |       \
 lifecycle store     envd     user port   MCP   interpreter
          |              \       |        |       /
          +----------- ProcessSession + FilesystemSession
                                  |
                         A3S ExecutionManager
                         /                  \
                   MicroVM              OCI sandbox
```

The edge owns public DNS, TLS, CORS, request limits, authentication, and route
normalization. The control plane owns public IDs, lifecycle state, templates,
volumes, tokens, leases, and reconciliation. The data-plane gateway owns the
wire protocols and translates them into backend-neutral process and filesystem
sessions. A backend never parses a public compatibility request.

The durable lifecycle state machine is generation-fenced:

```text
creating -> running -> pausing -> paused -> resuming
               ^                             |
               +-----------------------------+

creating | running | pausing | paused | resuming -> killing -> killed
```

Each transition is idempotent under the exact rules of the pinned control
contract. Route leases and process handles include the sandbox generation so a
late request cannot target a recreated workload with the same external ID.

## Protocol sources of truth

| Contract | Pinned source | A3S owner |
| --- | --- | --- |
| Lifecycle, templates, snapshots, auth, and volumes | `spec/openapi.yml`, restricted to public SDK tags | Control plane |
| Volume content | `spec/openapi-volumecontent.yml` | Control plane and volume service |
| envd health, metrics, env, init, and file transfer | `spec/envd/envd.yaml` | Data-plane gateway |
| Process, command, stdin, signal, and PTY | `spec/envd/process/process.proto` | Process session service |
| Filesystem metadata, mutation, and watches | `spec/envd/filesystem/filesystem.proto` | Filesystem session service |
| MCP configuration | `spec/mcp-server.json` | MCP template component |
| Code Interpreter | Pinned official client requests, parsers, server routes, and models | Interpreter template component |

The pinned Code Interpreter repository does not provide a standalone OpenAPI
document for its streaming service. Its contract fixture must therefore be
generated from both official clients and the pinned template server, including
raw NDJSON chunks and error responses; inventing an OpenAPI approximation is
not a compatibility source of truth.

## Protocol layers

### Control plane

The compatibility control plane implements the public operations used by the
SDKs:

| Area | Required operations |
| --- | --- |
| Sandbox lifecycle | create, connect/resume, get, list, kill, pause, timeout |
| Observability | health, logs, metrics, pagination |
| Network | get/update egress policy and routed ports |
| Persistence | create/list/delete snapshots |
| Templates | create/build/status/logs/list/get/delete, aliases and tags |
| Volumes | create/list/get/delete plus the separate volume-content API |
| Credentials | API keys and access tokens exposed by the public SDK |

The service accepts `X-API-Key` and Bearer access tokens using the same
precedence and error behavior as the pinned protocol. A3S credentials are
stored as salted hashes. Raw API keys, access tokens, environment secrets, and
command input must never appear in logs or audit detail fields.

Account API keys are one-way hashed. Sandbox-scoped envd and traffic tokens
must be returned again by create/connect flows, so they are stored encrypted at
rest under a versioned service key and separately hashed for constant-time
validation. Their ciphertext, plaintext, and hashes are excluded from normal
API objects, diagnostics, and audit payloads except where the pinned response
contract explicitly returns the plaintext token.

### Sandbox data plane

The client routes a request to a host derived from sandbox ID and port, such as:

```text
<port>-<sandbox-id>.<sandbox-domain>
```

The A3S edge proxy terminates TLS, validates the sandbox route and traffic
token, and forwards the request to the sandbox broker. It recognizes the
compatibility headers used by current clients:

```text
E2b-Sandbox-Id
E2b-Sandbox-Port
X-Access-Token
E2B-Traffic-Access-Token
```

`X-Access-Token` protects envd and signed file operations.
`E2B-Traffic-Access-Token` protects user-exposed services such as the code
interpreter. They have different scopes and lifetimes and must never be
collapsed into one credential.

The proxy must support HTTP/1.1, HTTP/2, WebSocket, Connect JSON, Connect
binary, streaming responses and trailers, half-closed stdin streams,
backpressure, partial frames, and browser CORS preflight. A wildcard DNS record
and wildcard certificate are required for production. Development can use the
explicit API and sandbox URL overrides without changing SDK code.

Both routing forms present in the pinned protocol are implemented:

```text
https://sandbox.<domain>             # shared endpoint plus route headers
https://<port>-<sandbox-id>.<domain> # direct endpoint and arbitrary ports
```

The pinned clients automatically select the shared endpoint only for an
upstream allowlist of domains. With a custom A3S domain they select the direct
form, which is also required by `getHost()`, Code Interpreter, MCP, signed file
URLs, and user services. Route parsing must validate the port and sandbox ID
before DNS-derived input reaches the internal router.

`E2B_SANDBOX_URL` is a fixed URL, not a hostname template. It must not point to
a multi-sandbox shared endpoint in the production compatibility profile:
upload and download URLs produced by the pinned SDK do not carry the route
headers and would lose their sandbox identity. It remains useful for local
single-sandbox fixtures. Shared-endpoint behavior is tested directly with the
route headers, while the official-client production gate uses wildcard direct
routing.

An official-client smoke test uses configuration only, for example:

```text
E2B_API_URL=https://api.box.example.com
E2B_DOMAIN=box.example.com
E2B_API_KEY=<compatibility-api-key>
```

Compatibility API keys must use the lexical form accepted by the pinned
clients' default validation: `e2b_` followed by one or more lowercase
hexadecimal characters. Requiring source patches or a hidden validation
override fails the zero-code-change gate. Native A3S credentials may retain a
separate format outside this compatibility surface.

### envd-compatible broker

The envd-compatible service is a host-side broker backed by the existing A3S
control protocols. Keeping it outside the workload prevents the workload from
replacing the compatibility endpoint or stealing its access token.

It translates:

```text
E2B control/data protocol
        |
        v
a3s-box-compat service
        |
        v
A3S ExecutionManager / control transport
        |
        +-- MicroVM backend
        +-- OCI sandbox backend
```

The broker owns process IDs exposed to the client and maps them to
backend-specific execution handles. Numeric IDs are generation-fenced so a
restarted workload cannot accidentally receive input or a signal intended for
an earlier process.

### Public ports and template services

The router can expose any valid sandbox port through the direct hostname form,
not just envd's port. Access is authorized against the sandbox route and its
traffic policy before forwarding. HTTP and WebSocket upgrades must preserve
streaming and cancellation behavior.

MCP support is delivered by a versioned template component on the port expected
by the pinned SDK. The generic SDK starts and configures that component through
normal commands and retrieves its token through the normal filesystem API, so
the compatibility layer must preserve those operations and the MCP schema. The
MCP process has no host runtime credentials.

The first manifest pins envd on port `49983`, Code Interpreter on port `49999`,
and MCP on port `50005`, matching the selected clients. These are compatibility
constants for that manifest, not configurable per deployment.

## A3S execution mapping

An external sandbox record contains at least:

```text
external_sandbox_id
box_id
template_id_and_version
sandbox_domain
requested_isolation
resolved_backend
isolation_class
execution_plan_digest
status_generation
created_at
expires_at
metadata
envd_protocol_version
envd_access_token_ciphertext_and_key_version
envd_access_token_hash
traffic_access_token_ciphertext_and_key_version
traffic_access_token_hash
traffic_policy
routing_state
```

The compatibility API resolves templates to an explicit A3S execution policy.
For example, a code-interpreter template can select shared-kernel sandbox
execution, while a confidential template selects MicroVM execution. This is a
deterministic template policy, not automatic backend fallback.

The E2B request schema does not require an A3S-specific isolation field. This
keeps official clients wire-compatible. A3S-native clients may expose an
optional typed template policy helper, but the resolved choice is persisted and
returned through A3S diagnostics rather than injected into upstream response
objects.

If a requested protocol operation is unavailable for the selected isolation
class, the server returns the matching protocol error and does not switch
backends. That configuration is then listed as partial rather than fully
compatible unless the pinned upstream contract rejects the same request. For
example, shared-kernel execution cannot be certified for the full lifecycle
surface until memory-preserving pause and resume have matching observable
semantics.

## Command and PTY compatibility

The compatibility implementation covers the current process service contract:

- list processes;
- start and stream a command;
- connect to an existing process;
- write stdin and close stdin;
- deliver signals;
- allocate and resize PTYs;
- return start, stdout, stderr, keepalive, and exit events in order;
- enforce request timeout independently from process timeout;
- preserve detached process handles after the initiating client disconnects.

The current A3S one-shot exec protocol is insufficient by itself. It needs a
durable process-session layer with independent stdin, stdout, stderr, signal,
wait, and PTY channels. Both MicroVM and host-sandbox transports implement the
same internal trait; the compatibility broker only sees that trait.

## Filesystem compatibility

The filesystem service covers:

- read and write one or multiple files;
- octet-stream and multipart upload modes;
- file metadata and content type behavior;
- stat, exists, list, make directory, move/rename, and recursive remove;
- directory watch streams and polling watcher handles;
- user-relative paths and ownership;
- signed upload/download URLs and expiration;
- stable errors for invalid path, invalid user, not found, and insufficient
  space.

All paths are resolved beneath the workload rootfs with descriptor-relative
operations. String-prefix checks are not sufficient. Symlink traversal and
rename races must be covered by negative tests. The broker must not expose the
host bundle, state directory, runtime sockets, or rootfs lower layers.

## Code Interpreter compatibility

The code-interpreter template contains a versioned kernel service reached
through the standard sandbox port router. It implements:

```text
GET    /health
POST   /execute
POST   /contexts
GET    /contexts
DELETE /contexts/{id}
POST   /contexts/{id}/restart
```

`/execute` returns the pinned newline-delimited streaming format. The adapter
preserves:

- Python, JavaScript, R, Java, Bash, and explicitly advertised languages;
- persistent named contexts;
- rich MIME results including text, HTML, Markdown, SVG, PNG, JPEG, PDF,
  LaTeX, JSON, JavaScript, tabular data, and chart data;
- main-result flags and execution count;
- stdout and stderr ordering;
- structured execution errors with name, value, and traceback;
- streaming callbacks and cancellation;
- context list, restart, and removal behavior.

The kernel service is an image/template component. It does not receive host
runtime authority and cannot call the control-plane API with service
credentials.

## Python SDK

The Python distribution provides both synchronous and asynchronous surfaces
matching the pinned SDK:

```python
from a3s_box import AsyncSandbox, Sandbox

sandbox = await AsyncSandbox.create("code-interpreter")
result = await sandbox.commands.run("python -V")
await sandbox.files.write("/tmp/input.txt", "hello")
await sandbox.kill()
```

Required modules include sandbox lifecycle, commands, PTY, filesystem, git,
network, templates, snapshots, volumes, pagination, and code interpreter.
Public types use complete annotations and ship `py.typed`. Async operations use
`async`/`await`, streaming uses async iterators, and resource-owning helpers
support `async with` cleanup without changing the behavior of explicit
`kill()`.

The first compatibility proof uses the published `e2b` and
`e2b-code-interpreter` wheels unchanged. The A3S package then exposes the same
call shapes with A3S endpoint defaults and typed A3S-only diagnostics in a
separate namespace. It must not add required parameters or inject A3S fields
into upstream-compatible response types.

## TypeScript SDK

The TypeScript distribution exports the pinned class and type surface and works
in supported Node.js, edge, and browser environments where the upstream SDK is
expected to work:

```typescript
import { Sandbox } from '@a3s-lab/box'

const sandbox = await Sandbox.create('code-interpreter')
const result = await sandbox.commands.run('node --version')
await sandbox.files.write('/tmp/input.txt', 'hello')
await sandbox.kill()
```

It provides `Sandbox`, `Commands`, command handles, `Pty`, `Filesystem`, watch
handles, templates, snapshots, volumes, paginators, and code-interpreter result
types. Cancellation uses `AbortSignal`; timeouts remain milliseconds; binary
stdin and file content remain `Uint8Array`-compatible.

The first compatibility proof uses the published `e2b` and
`@e2b/code-interpreter` packages unchanged. The A3S package uses the same wire
clients and exposes A3S-only diagnostics through an additive namespace. Public
export snapshots and TypeScript compile fixtures prevent accidental source API
drift.

Generated wire clients come from the pinned OpenAPI and Protobuf sources.
Hand-written ergonomic classes are kept small and covered by cross-language
golden tests so Python and TypeScript do not drift.

## Error compatibility

The gateway maintains a table from A3S errors to the pinned HTTP, Connect, and
SDK-visible error contracts. It preserves:

- HTTP status and response content type;
- structured error code and message fields;
- not-found versus already-stopped behavior;
- request timeout versus sandbox lifetime timeout;
- command non-zero exit as a command result, not a transport failure;
- cancellation and stream termination semantics;
- retryable versus terminal failures.

Unknown internal errors are assigned a request ID and sanitized before leaving
the service. Internal paths, command environment, runtime stderr, OCI bundle
content, and credentials are never returned as generic error detail.

## Versioning and source generation

Compatibility sources are vendored at a reviewed upstream commit with license
and attribution. CI performs all of the following:

1. Generate Rust server bindings, Python models, and TypeScript clients from the
   pinned control, envd, volume-content, Protobuf, and MCP schemas.
2. Compare generated descriptors and public SDK snapshots to committed golden
   files.
3. Detect upstream changes and produce a machine-readable compatibility diff.
4. Require an explicit compatibility-manifest update before claiming support
   for a newer upstream version.
5. Keep old supported protocol versions until the documented deprecation
   window ends.
6. Lock and checksum the exact official Python wheels and npm package tarballs
   used by black-box tests.

The service advertises its compatibility manifest through a diagnostic
endpoint outside the upstream namespace. Upstream response objects are not
modified with required A3S fields.

## Configuration

Product configuration uses HCL. A production service configuration resembles:

```hcl
e2b_compat {
  api_listen          = "0.0.0.0:443"
  api_public_url      = "https://api.box.example.com"
  sandbox_domain     = "box.example.com"
  shared_sandbox_url = "https://sandbox.box.example.com"
  default_template   = "code-interpreter-v1"

  protocol_manifest = "/etc/a3s-box/e2b-compat-manifest.json"

  routing {
    wildcard_tls_certificate = "/etc/a3s-box/tls/fullchain.pem"
    wildcard_tls_key         = "/etc/a3s-box/tls/privkey.pem"
  }

  template_policy "code-interpreter-v1" {
    isolation = "sandbox"
  }
}
```

Runtime binaries, credential-store paths, and template-to-isolation mappings
are validated during startup. No production default accepts an arbitrary host
runtime binary or disables authentication.

## Repository boundaries

The implementation should keep generated compatibility artifacts out of core
runtime modules:

```text
compat/e2b/
  manifests/        # version tuples and generated digests
  spec/             # vendored public schemas and attribution
  fixtures/         # wire and public-export golden files
src/compat/         # Rust control/data-plane service crate
sdk/python/         # A3S Python package and official-client fixtures
sdk/typescript/     # A3S TypeScript package and official-client fixtures
templates/
  code-interpreter/ # versioned interpreter image component
  mcp-gateway/      # versioned MCP image component
```

`src/compat` may depend on the backend-neutral runtime interfaces. Core and
runtime crates must not depend on generated public API server code. SDK
packages must not invoke `crun`, libkrun, local state files, or private runtime
sockets.

## Delivery phases and gates

### Phase 1: contract fixture

- Vendor the pinned public OpenAPI and Protobuf descriptors.
- Vendor the volume-content and MCP schemas as well.
- Generate a compatibility manifest and a machine-readable endpoint, method,
  field, header, public-export, and error inventory.
- Build black-box fixtures from the official Python and JavaScript clients.

Gate: CI can detect any field, status, header, or method drift before server
implementation begins.

### Phase 2: lifecycle and routing

- Implement authentication, create/connect/get/list/kill/timeout, pagination,
  durable mappings, wildcard routing, and traffic tokens.
- Route every create through A3S execution resolution and persist its plan.

Gate: unmodified official SDKs create, reconnect to, list, time out, and kill an
A3S sandbox.

### Phase 3: commands, files, and PTY

- Implement ConnectRPC process and filesystem services plus HTTP file transfer.
- Add durable process handles, PTY resize, streaming, watch, and signed URLs.

Gate: upstream command/filesystem contract suites pass in Python sync, Python
async, and TypeScript clients on both A3S backends where supported.

### Phase 4: code interpreter

- Publish the versioned interpreter template and kernel service.
- Implement contexts, NDJSON execution streaming, rich results, and callbacks.
- Publish the versioned MCP template and verify its standard port, token, and
  streaming behavior through the generic SDK.

Gate: upstream code-interpreter and MCP client suites pass without source
patches.

### Phase 5: complete public surface

- Implement templates/builds, snapshots, pause/resume, volumes, metrics,
  network policy, routed ports, and remaining public helpers.
- Run compatibility across every supported SDK/version tuple.

Gate: the complete public SDK inventory has an observed passing test or an
observed upstream-equivalent rejection for the same request. No method is
marked compatible based only on matching its name, JSON shape, or an
A3S-specific unsupported response.

## Validation matrix

Compatibility evidence must include:

- official SDK packages configured only through supported endpoint/domain and
  credential options;
- default client-side API-key validation with an issued compatibility key;
- Python sync, Python async, TypeScript Node.js, and supported edge transports;
- direct wildcard-host routing through official clients and shared-endpoint
  routing through explicit header-level fixtures;
- exact request methods, paths, headers, query serialization, and body fields;
- success and error status codes and response bodies;
- HTTP/1.1, HTTP/2, Connect content types and trailers, WebSocket upgrades, and
  browser CORS behavior;
- stream event order, partial UTF-8, binary stdin, backpressure, cancellation,
  disconnect, and reconnect;
- timeout boundary tests using the protocol's documented units;
- process exit, signal, PTY resize, and detached handle behavior;
- file path traversal, symlink race, metadata, large file, and watcher behavior;
- code-context persistence, rich MIME output, traceback, and concurrent cells;
- arbitrary public ports, traffic-token denial, signed URL expiry, and MCP
  streaming;
- sandbox crash, service restart, host reboot, and stale-route reconciliation;
- both MicroVM and shared-kernel isolation with resolved backend evidence.

Large compatibility, image, networking, and performance suites run on A3S OS
servers after pulling the tested Git revision. Developer laptops run schema
generation, formatters, and pure contract tests only.

## Upstream references

- [E2B repository](https://github.com/e2b-dev/e2b)
- [E2B control-plane OpenAPI](https://github.com/e2b-dev/e2b/blob/main/spec/openapi.yml)
- [envd HTTP OpenAPI](https://github.com/e2b-dev/e2b/blob/main/spec/envd/envd.yaml)
- [envd process protocol](https://github.com/e2b-dev/e2b/blob/main/spec/envd/process/process.proto)
- [envd filesystem protocol](https://github.com/e2b-dev/e2b/blob/main/spec/envd/filesystem/filesystem.proto)
- [volume-content OpenAPI](https://github.com/e2b-dev/e2b/blob/main/spec/openapi-volumecontent.yml)
- [MCP configuration schema](https://github.com/e2b-dev/e2b/blob/main/spec/mcp-server.json)
- [E2B code interpreter](https://github.com/e2b-dev/code-interpreter)
