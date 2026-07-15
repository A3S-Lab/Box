# E2B Protocol Compatibility and SDK Design

Status: **Phase 1 complete; Phase 2 in progress (slices 1 through 4 complete;
slice 5 control-service composition complete, TLS data plane pending)**

Implementation evidence starts in [`compat/e2b/`](../compat/e2b/README.md).
The pinned contract manifest intentionally reports `full_compatibility=false`;
the release-level compatibility claim remains gated by every phase below.

Scope: protocol compatibility, Python and TypeScript SDKs, and the service
boundary required to provide a remote code-execution environment on A3S Box.

Target: the public E2B SDK contract as observed on 2026-07-14. Compatibility is
pinned by upstream commit and generated protocol descriptors, not by an
unversioned claim.

## Current implementation evidence and remaining gates

| Area | Implemented evidence | Remaining gate |
| --- | --- | --- |
| Pinned contract | Vendored control, envd, volume-content, Process, Filesystem, MCP, public-export, and package artifacts with generated digests | Keep the manifest pinned and regenerate it only through reviewed upstream updates |
| Lifecycle protocol | Owner-scoped create, connect, get, list, timeout, and kill routes; unchanged pinned Python sync/async, TypeScript, and Code Interpreter clients pass against the Rust fixture server | Run the same unchanged clients through the production service and a real Sandbox execution |
| Durable control state | SQLite WAL migrations, strict record validation, compare-and-swap transitions, generation-fenced expiry claims, startup reconciliation, and periodic reaping are composed into the production service; an A3S OS smoke preserves a running record across process restart | Exercise host-reboot recovery end to end |
| Runtime lifecycle | The production compatibility process uses the canonical `LocalExecutionManager`; an A3S OS smoke creates through HTTP, starts through certified `crun`, reconnects after service restart, replaces timeout, kills, and verifies box, runtime-state, and socket cleanup | Complete the host-reboot and official-client matrices |
| Credentials and routing | ACL config wires salted PBKDF2-SHA256 account hashes, scope-bound AES-256-GCM sandbox tokens, independent HMAC validation, versioned key rotation, strict direct/shared parsing, and durable-record-projected generation-fenced leases | Add the TLS data-plane gateway and exercise routed traffic through it |
| Commands and SDK surface | Pinned Process/Filesystem descriptors and Python/TypeScript public-export inventories prevent unreviewed drift | Implement envd HTTP, ConnectRPC, PTY, signed URLs, Code Interpreter/MCP streams, the remaining public control surface, and native convenience packages |

The lifecycle control path is now composed and exercised against a real
Sandbox on A3S OS. That smoke uses HTTP requests rather than the pinned
official SDKs, and it does not traverse the wildcard TLS or sandbox data-plane
gateway. The unchanged-client fixture still uses an in-memory repository and
fake execution manager. These are complementary results rather than the full
black-box compatibility matrix, so `full_compatibility=false` remains
mandatory.

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

Product configuration uses A3S Agent Configuration Language (ACL), parsed by
the pinned `a3s-acl` implementation. The service accepts only a `.acl` file.
Token encryption and digest keys must be independent 32-byte hexadecimal values
referenced through `env("VARIABLE")`; literal key material is rejected. A
production control-service configuration resembles:

```acl
e2b_compat {
  api_listen        = "127.0.0.1:3000"
  api_public_url    = "https://api.box.example.com"
  sandbox_domain   = "box.example.com"
  database_path    = "/var/lib/a3s-box/e2b/lifecycle.sqlite3"
  runtime_home     = "/var/lib/a3s"
  runtime_state_path = "/var/lib/a3s-box/e2b/managed-executions.json"

  supervisor {
    interval_seconds          = 5
    batch_size                = 100
    reconciliation_page_size = 100
  }

  account "primary" {
    scheme    = "api_key"
    owner_id  = "production-team"
    client_id = "production-client"
    hash      = "pbkdf2-sha256$210000$<salt-hex>$<digest-hex>"
  }

  token_key "2026-07" {
    version = 1
    active  = true
    encryption_key = env("A3S_BOX_E2B_TOKEN_ENCRYPTION_KEY_V1")
    digest_key     = env("A3S_BOX_E2B_TOKEN_DIGEST_KEY_V1")
  }

  template_policy "code-interpreter-v1" {
    isolation = "sandbox"
    image = "registry.example.com/a3s/code-interpreter:2026-07"
    envd_version = "0.1.3"

    resources {
      vcpus     = 2
      memory_mb = 1024
      disk_mb   = 4096
    }

    route {
      port = 49999
      token_scope = "traffic"
    }
  }
}
```

The envd port and envd token scope are added when omitted. Runtime paths,
credentials, key versions, template execution policy, resources, and routed
ports are validated before the listener opens. Startup runs durable lifecycle
reconciliation; a bounded supervisor then reaps expired records until graceful
shutdown. The control listener remains behind the deployment TLS edge in this
slice. Wildcard TLS termination and the sandbox data-plane proxy are the next
focused slice.

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

## Phase 2 implementation architecture

Phase 2 is a single-host control-plane preview. It proves the complete
create/connect/get/list/timeout/kill path against a real A3S OS runtime before
introducing multi-host scheduling. The public protocol and internal interfaces
must not assume that the single-host limit is part of the upstream contract.

### Dependency direction

The runtime now owns a canonical managed-execution store, a backend-neutral
`LocalExecutionManager`, and a production VM/Sandbox backend. CLI
`create`/`start`/`restart`/`run` and the Rust SDK lifecycle API use that
manager. The compatibility service must use the same manager directly rather
than spawning `a3s-box`, importing CLI modules, or editing `boxes.json`. Phase
2 completes this dependency direction:

```text
a3s-box-core
  typed execution request + caller record policy + resolved execution plan
          ^
          |
a3s-box-runtime
  canonical state store + ExecutionManager + production backend
          ^
          |
  +-------+------------------+
  |                          |
a3s-box CLI / Rust SDK   a3s-box-compat
  local adapters          remote protocol adapter
```

The runtime lifecycle facade owns image resolution, rootfs preparation,
network and volume attachment, backend capability checks, shim launch, state
registration, and cleanup. The CLI and Rust SDK become callers of that facade.
This removes the current duplicate SDK state model instead of adding a third
model inside the compatibility service.

The backend-neutral runtime interface is deliberately smaller than either the
CLI or the public compatibility API:

```text
ExecutionManager
  create(request, operation_id)           -> ExecutionReservation
  start(execution_id, generation)         -> ExecutionLease
  create_and_start(request, operation_id) -> ExecutionLease
  inspect(execution_id)                   -> ExecutionStatus
  pause(execution_id, generation, policy) -> ExecutionLease
  resume(execution_id, generation)        -> ExecutionLease
  kill(execution_id, generation)          -> KillOutcome
  reconcile(operation_id)                 -> ReconcileOutcome
```

`create_and_start` is the default composition of `create` followed by `start`.
The durable `created` state lets CLI/SDK callers create without booting and
lets startup reconciliation distinguish a reservation from an in-flight
backend start. `operation_id` makes create retryable after a service crash.
`generation` prevents a delayed start, kill, or route request from reaching a
replacement execution. Runtime-specific handles, process IDs, socket paths,
OCI bundle paths, and shim command lines never cross this interface.

`CreateExecutionRequest` keeps backend launch requirements in `BoxConfig` and
keeps caller-owned lifecycle and local resource metadata in a typed
`ExecutionRecordPolicy`. The policy includes the user-visible name, automatic
removal and restart behavior, health and log configuration, named-volume
identity, stop behavior, and host-facing inspection fields. It is part of the
durable creation intent rather than an encoded label. Consequently, retrying
one operation ID with policy drift fails as a conflict, while records written
before the policy field was added deserialize to explicit safe defaults. A
single runtime mapper projects the request and policy into `BoxRecord`; CLI,
SDK, and compatibility adapters must not construct a parallel record shape.

### Compatibility service modules

The existing contract generator remains in `src/compat`. Runtime service code
is added by concern rather than mixed into schema parsing:

```text
src/compat/src/
  control/
    credential.rs     # injected credential and token interfaces
    model.rs          # lifecycle records and public/internal state mapping
    repository.rs     # transactional persistence interface
    service.rs        # create/connect/list/timeout/kill use cases
    sqlite/           # WAL repository and versioned migrations
    supervisor.rs     # expiry reaping and startup reconciliation
  http/
    auth.rs           # credential extraction and verification
    error.rs          # exact upstream error mapping
    lifecycle.rs      # lifecycle route handlers and DTO conversion
    router.rs         # route assembly and request limits
  routing/            # production data-plane authorization boundary
    policy.rs         # persisted exact-port and token-scope policy
    lease.rs          # generation-fenced sandbox route projection
    parser.rs         # wildcard host and explicit-header validation
  bin/
    a3s-box-e2b-fixture-server.rs # deterministic protocol fixture
    a3s-box-e2b.rs                # planned production composition root
```

No handler accesses the database or runtime directly. Handlers authenticate,
parse the pinned wire DTO, call the control service, and map its result. The
control service depends on repository, clock, token, and execution interfaces,
so lifecycle semantics can be tested without booting a VM or OCI sandbox.
The production composition root injects `LocalExecutionManager` directly
behind `ExecutionManager`; a compatibility-owned runtime wrapper is not added
unless protocol translation eventually requires one.

### Durable lifecycle transaction

The initial durable repository is SQLite in WAL mode through an asynchronous
driver. Its location is explicit in ACL and it owns versioned migrations. A
database transaction is never held across an image pull, sandbox boot, or shim
call.

Create follows a recoverable sequence:

```text
authenticate and resolve template policy
              |
              v
transaction: insert creating record
  external ID, operation ID, generation, requested policy,
  plan digest, encrypted tokens, expiry, metadata
              |
              v
ExecutionManager.create(operation_id)
  persist a generation-fenced created reservation
              |
              v
ExecutionManager.start(execution_id, generation)
              |
              v
transaction: compare generation and publish running + route lease
```

If the service stops after the runtime call, startup reconciliation finds the
`creating` record and resolves the same `operation_id`; it does not start a
second sandbox. A failed create is moved to a terminal internal state and its
partial runtime resources are cleaned before the external ID can be reused.

Kill first compares and advances the generation to `killing`, revokes route
leases, calls the idempotent runtime kill, and then records `killed`. Timeout
updates replace `expires_at` from the current clock. A reaper claims an expired
record with the same generation-fenced transition used by an API kill, so a
concurrent connect or timeout extension cannot kill the renewed sandbox.

Connect never creates a missing sandbox. For a running sandbox it only extends
the TTL and returns HTTP 200. For a paused sandbox it performs the explicit
resume transition and returns HTTP 201. The Sandbox template policy does not
silently switch to MicroVM when a resume capability is unavailable.

### Identity, credentials, and routing

External sandbox IDs and A3S execution IDs are different identifiers. Only the
control repository owns their mapping. The runtime receives the external ID as
an untrusted label, not as a filesystem path or host process selector.

Account API keys are stored as salted hashes. Envd and traffic tokens must be
returned by create/connect, so their ciphertext and hash are stored separately
with a key version. Authentication compares hashes in constant time and never
logs raw headers. The first server fixture uses an injected verifier; the
production binary refuses to start without a configured credential and token
encryption provider.

The production credential provider stores account credentials as encoded
PBKDF2-SHA256 records with a per-credential random salt and a minimum work
factor. Compatibility API keys retain the pinned `e2b_[0-9a-f]+` lexical form;
Bearer and Supabase credentials use the same hashed-record boundary without
sharing plaintext material. Sandbox envd and traffic tokens are encrypted with
AES-256-GCM and authenticated separately with a scope- and version-bound HMAC.
The active key version issues new tokens while retained older versions remain
decryptable during rotation. Removing an old version makes its records fail
closed, and swapping an envd token into the traffic scope fails both decryption
and constant-time digest validation.

Each published route lease contains the external sandbox ID, internal
execution ID, generation, port scope, expiry, and token scope. The wildcard
host parser is a pure validated component. It accepts neither arbitrary
hostnames nor a sandbox ID recovered by string splitting after routing has
begun.

Route policy is persisted inside the canonical lifecycle record. A lease is an
immutable projection of a currently running record rather than a second mutable
database row, so timeout replacement, pause, kill, or recreation advances the
record generation and immediately fences every prior lease. Resolution also
checks the execution generation, expiry, exact routed port, and the separately
scoped envd or traffic HMAC. Both `<port>-<sandbox-id>.<domain>` and the shared
host plus `E2b-Sandbox-Id`/`E2b-Sandbox-Port` form use the same parser; duplicate
or conflicting headers and domain-suffix confusion fail closed. SQLite restart
coverage proves that the policy and generation remain authoritative after the
service process is recreated.

### Incremental merge gates

Phase 2 is delivered as small, immediately merged changes:

1. **Complete:** add lifecycle domain types, transition tests, repository and
   execution interfaces, and deterministic clock/token fakes. No network
   listener.
2. **Complete:** add the owner-scoped HTTP lifecycle router and run the
   checked-in official Python sync, Python async, TypeScript, and Code
   Interpreter fixtures against the Rust service with a fake execution
   manager.
3. **Complete:** add SQLite WAL migrations, strict compare-and-swap repository
   operations, atomic generation-fenced expiry claims, restart recovery,
   startup reconciliation, and corruption/crash/concurrency tests.
4. **Complete:** extract canonical A3S state and the runtime
   `ExecutionManager`; add the production backend and prove its real Sandbox
   lifecycle; switch CLI create to the same reservation path; switch CLI
   start/restart/run and the Rust SDK to the same implementation with behavior
   parity tests.
5. **In progress:** production account credentials, sandbox token providers,
   generation-fenced route leases, validated wildcard/shared parsing, and the
   ACL-configured service binary are complete. Add the TLS data-plane gateway.
   Pull each merge commit on an A3S OS server and run the unmodified official
   clients against real `--isolation sandbox` executions.

The runtime foundation of slice 4 is complete. The persisted execution record
is the canonical schema shared by the CLI and Rust SDK, preventing either
client from dropping fields it does not model. Runtime-owned strict and
recovery-compatible reads, a cross-process advisory lock, durable atomic
writes, and synchronous read-modify-write transactions protect that state. The
managed-execution store reserves creation operations atomically, returns an
existing record only when the full creation intent matches, persists
transitional lifecycle claims, rejects stale state or generation comparisons,
and advances the generation exactly once when pause or resume completes or a
restart moves from old-runtime teardown to new-runtime startup.
Backend calls remain outside the state lock.

`LocalExecutionManager` implements the backend-neutral lifecycle contract over
that store and an injectable runtime backend. `create` persists a stable
`created` reservation without backend side effects, `start` fences the caller
by generation and persists a `starting` claim before launch, and
`create_and_start` composes those operations for the compatibility service.
It keeps pause policy with the corresponding transitional record, performs
state-file work on Tokio blocking workers, and resolves ambiguous backend
errors from runtime observations before publishing a result. Startup
reconciliation can therefore distinguish an unstarted reservation from a
runtime that became ready before its durable `running` publication.

Explicit restart persists `restart_stopping` before terminating an active old
runtime. Only confirmed terminal backend evidence and resource release permit
the atomic transition to `restart_starting`, which increments the generation
once. The restart operation ID, source generation, and source state survive a
manager crash. A retry can therefore finish a lost kill response, start a
generation that was advanced before the backend call, or replay a completed
lease without starting another runtime. Start failure is recorded at the new
generation and requires a new operation ID for any later restart. Graceful-stop
timeout is part of the restart intent, so a retry cannot silently change it.
Named-volume and network ownership is released and rebound once, while
execution-owned anonymous volumes remain available to the replacement
generation. Retained terminal stops preserve those anonymous volumes for a
later restart; auto-remove terminal kills remove them.

The production VM/Sandbox backend is also complete for this slice. It owns live
runtime handles, reconstructs MicroVM processes with PID identity fencing,
reconstructs Sandbox executions from validated durable `crun` evidence,
rejects unsupported Sandbox pause/resume without falling back to MicroVM, and
owns terminal cleanup. The opt-in A3S OS smoke harness has proven that Sandbox
`create` persists a `created` reservation without allocating a Box directory,
runtime root, or sockets; manager reconstruction reconciles the same unstarted
reservation; and explicit `start` launches through `crun`. It also proves pause
rollback, kill, and terminal cleanup. Deterministic image-pull failure injection
proves that a failed start does not create those runtime resources.

CLI `create` now converts its validated arguments into `BoxConfig` and
`ExecutionRecordPolicy`, then calls `LocalExecutionManager::create`. It no
longer pre-allocates the Box directory, log directory, or socket directory.
Caller parity coverage verifies both the legacy inspection fields and the full
managed request, including config-only values such as DNS and persistent
filesystem policy. Named-volume bookkeeping remains attached only after the
durable reservation succeeds and rolls the reservation back on failure.

CLI `run` now submits the complete caller policy to the same manager, starts
under generation fencing, and reloads the canonical record before foreground
or detached handling. It resolves image health and stop defaults before
reservation, reuses the cache during backend start, and delegates network,
volume, rootfs, stop, and auto-remove ownership to the managed backend. Caller
parity tests cover isolation, DNS, environment, security, limits, TEE/sidecar,
logs, health, stop policy, shared memory, persistence, and resource metadata.

The Rust SDK now injects the same backend-neutral manager used by the CLI and
exposes typed create, start, create-and-start, inspect, pause, resume, restart,
kill, and reconciliation operations. Caller-parity coverage compares the full
serialized `CreateExecutionRequest`, including `BoxConfig` and
`ExecutionRecordPolicy`, at the injected manager boundary. An opt-in A3S OS
smoke test proves staged create, start, create-and-start, inspect, kill, and
runtime cleanup through the real Sandbox backend without invoking the CLI.

Each slice must pass its focused tests and repository CI before merge. The
durable repository, production execution manager, credentials, routing, and
lifecycle HTTP router are now composed in one ACL-configured process. The
Phase 2 gate remains closed until the real create/connect/list/timeout/kill
matrix also traverses the production TLS and sandbox data planes. Passing the
fake manager in slice 2 and passing the direct runtime smoke in slice 4 remain
complementary evidence, not that missing end-to-end proof.

Slice 2 evidence includes exact recorder drift checks plus live requests from
the pinned, unmodified clients to the Rust router. The live gate was also run
on an A3S OS host before merge. It covers authentication, owner isolation,
create, connect, get, list filtering, timeout replacement, kill, not-found
mapping, and Code Interpreter creation. It does not change the manifest's
`full_compatibility=false` value.

The Slice 3 persistence batch uses a bundled SQLite build through a dedicated
asynchronous connection thread. Versioned migrations create a STRICT table in
WAL mode. The serialized lifecycle record is the single source of truth;
indexed owner, operation, generation, state, creation, and expiry fields are
generated by SQLite from that record. Startup refuses unknown migration
histories, and every read revalidates identifier, generation, credential, and
cross-field lifecycle invariants before returning a record.

The maintenance half of Slice 3 atomically advances expired records to
`pausing` or `killing` inside the repository transaction. This makes timeout
replacement and reaping mutually exclusive at the persisted generation. The
supervisor retries generation-fenced pause, resume, and kill work after a
service crash and uses the runtime operation ID to recover a create that became
ready before its `running` publication committed. A second migration indexes
chronological expiry through SQLite's date representation so optional RFC3339
fractional seconds cannot cause a record to be skipped.

## Delivery phases and gates

### Phase 1: contract fixture (complete)

- Vendor the pinned public OpenAPI and Protobuf descriptors.
- Vendor the volume-content and MCP schemas as well.
- Generate a compatibility manifest and a machine-readable endpoint, method,
  field, header, public-export, and error inventory.
- Build black-box fixtures from the official Python and JavaScript clients.

Gate: CI can detect any field, status, header, or method drift before server
implementation begins.

### Phase 2: lifecycle and routing (in progress)

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
