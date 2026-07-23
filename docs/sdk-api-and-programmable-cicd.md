# SDK API and Programmable CI/CD Plan

This document is the authoritative completion plan for the native local A3S
Box SDKs. It covers Rust, Python, and TypeScript. It does not redefine the
native SDKs as wrappers around the official E2B clients: local calls remain
credential-free and execute through the installed A3S Box runtime.

Remote E2B protocol compatibility is a separate product surface. Its pinned
contract and official-client evidence remain tracked under
[`compat/e2b`](../compat/e2b/README.md).

## Product Contract

"Programmable CI/CD" means a code-first execution toolbox, similar in spirit
to BoxLite. A user must be able to build an OCI base image, create reusable
storage and networks, configure an isolated box, run scripts, capture results,
and reuse a filesystem snapshot directly from normal Rust, Python, or
TypeScript code. A separate YAML service or workflow engine is not required.

Rust is the implementation source of truth. Python and TypeScript use the
versioned machine bridge and never parse human CLI output. Language differences
are limited to normal conventions such as Python sync/async variants and
JavaScript promises.

Each language exposes two complementary entry styles over that single
implementation:

- the familiar E2B-style `Sandbox.create`, `commands`, and `files` surface for
  low-friction migration and direct execution;
- a fluent builder surface for image, storage, network, box, and script
  configuration.

The E2B-style surface remains supported as the builder API grows. Neither entry
style may maintain a separate lifecycle or transport implementation.
`SandboxBuilder.start()` returns the same public `Sandbox` type used by
`Sandbox.create()`, with the same `commands`, `files`, snapshot, and lifecycle
namespaces.

Every public operation must have:

1. a typed request and response;
2. validation before runtime mutation;
3. a stable machine error code;
4. unit or protocol coverage in every language that exposes it;
5. a real-runtime test for every supported isolation backend;
6. documentation that distinguishes implemented, tested, and published state.

An API is not complete merely because a method exists. Completion requires the
matching cross-language and real-runtime evidence.

## Programmable Runtime Toolbox

The primary SDK contract consists of the following layers.

### Runtime and resource management

The runtime client owns host-level resources:

- build an OCI image from a context and Dockerfile;
- pull, inspect, list, tag, push, and remove images;
- create, inspect, list, remove, and prune named volumes;
- create, inspect, list, remove, and prune bridge networks;
- create, reconnect to, inspect, list, stop, restart, and remove boxes;
- inspect runtime diagnostics, resource usage, logs, and metrics.

Resource creation is explicit. Passing a named volume or network to a box does
not silently create it with guessed settings.

### Typed box configuration

A box creation request must support:

- OCI image reference or a previously built image reference;
- explicit MicroVM or shared-kernel Sandbox isolation;
- CPU, memory, lifetime, and persistence settings;
- environment, metadata, hostname, user, and working directory;
- typed host bind mounts and named-volume mounts with read-only/read-write
  access;
- typed tmpfs mounts;
- TSI, disabled, or named bridge networking;
- validated TCP port publication, DNS servers, and host aliases;
- read-only root filesystems and explicit automatic cleanup;
- restoration from a runtime-managed immutable filesystem snapshot.

The public API uses typed objects for mounts, networking, and published ports.
Raw runtime strings such as `host:guest:ro` or `bridge:name` remain internal
serialization details.

### Commands and scripts

The box handle must support:

- argv execution without a shell;
- shell commands when explicitly selected;
- script execution by sending source through standard input to an explicit
  interpreter, avoiding shell-escaping and temporary-host-file problems;
- environment, working directory, user, standard input, and timeout controls;
- separated bounded stdout/stderr, exit code, and truncation state;
- background process handles, output streaming, wait, signal, and PTY controls.

Script steps are composed in the user's programming language. A user can use
ordinary loops, functions, exceptions, and concurrency rather than translating
their program into a second workflow language.

### Snapshots, caches, and artifacts

- capture a running or paused box filesystem under a validated snapshot ID;
- restore independent copy-on-write boxes from that snapshot;
- query snapshot size and delete snapshots with live-use fencing;
- mount named volumes as explicit dependency caches;
- collect artifacts through typed file APIs with path confinement, size limits,
  hashes, and optional host export;
- clean up boxes and ephemeral resources on success, failure, cancellation,
  and context-manager exit.

This is sufficient to implement warm CI bases, test matrices, build caches, and
artifact collection in application code without a dedicated pipeline engine.

## Cross-Language API Shape

The naming follows each language while preserving the same concepts:

| Concept | Rust | Python | TypeScript |
| --- | --- | --- | --- |
| Runtime client | `A3sBoxClient` | `A3SBoxClient` | `A3SBoxClient` |
| Box handle | `Sandbox` | `Sandbox` / `AsyncSandbox` | `Sandbox` |
| Build request | `BuildImage` | `BuildImageOptions` | `BuildImageOptions` |
| Named volume | `CreateVolume` | `CreateVolumeOptions` | `CreateVolumeOptions` |
| Named network | `CreateNetwork` | `CreateNetworkOptions` | `CreateNetworkOptions` |
| Mount | `VolumeMount` | `VolumeMount` | `VolumeMount` |
| Network selection | `SandboxNetwork` | `SandboxNetwork` | `SandboxNetwork` |
| Published port | `PortMapping` | `PortMapping` | `PortMapping` |
| Script | `Script` | `Script` | `Script` |

The existing E2B-style `Sandbox` remains the convenient local execution
facade. It is not overloaded with host-wide image, volume, and network
management; those operations belong to the runtime client.

The preferred fluent flow is:

1. `client.image(context)...build()`;
2. `client.volume(name)...create()` and
   `client.network(name)...create()` where required;
3. `client.sandbox(image)...mount(...).network(...).start()`;
4. `box.script(source)...run()` or the E2B-style
   `box.commands.run(...)`;
5. deterministic cleanup through `kill`, `close`, or a language context
   manager.

Typed option values remain public for serialization, testing, and applications
that construct configuration dynamically. Builders are the ergonomic default,
not a second contract.

## Optional Composition Layer

Parallel fan-out, matrices, dependency graphs, retries, and reports may be
provided later as a small library above the runtime toolbox. They are not the
definition of programmable CI/CD and must not introduce a second lifecycle
implementation.

If retained, the composition layer must:

- call the typed runtime client instead of assembling CLI strings;
- represent jobs and results as stable serializable values;
- use runtime-managed snapshots for warm-base fan-out;
- use named volumes for declared caches and file APIs for artifacts;
- preserve exit codes and distinguish command, timeout, cancellation, and
  infrastructure failures;
- fence cleanup ownership so crash recovery never removes a live peer's
  resources.

## Required Native SDK Surface

| Area | Required operations | Current state |
| --- | --- | --- |
| Images and builds | list, inspect, history, pull, build, tag, push, remove, cache eviction, platform selection, credentials, and progress | Fluent build plus pull/list have Rust/Python/TypeScript parity. Inspect, history, tag, push, remove, eviction, credentials, and progress remain bridge gaps. |
| Typed box configuration | image, isolation, CPU/memory/lifetime, environment, workdir/user, mounts, tmpfs, network, ports, DNS/hosts, read-only root, persistence, cleanup, and snapshot restore | Implemented in Rust/Python/TypeScript options and fluent builders. The real macOS/HVF MicroVM builder gate passes; the Ubuntu/crun gate is wired into blocking CI, while Linux/KVM remains host-gated. |
| Volumes | list, get, create, typed mount, content operations, remove, and prune | Create/get/list/remove and typed bind/named mounts have three-language parity. Prune and direct content helpers remain pending. |
| Networking | list/create/get/remove/prune, typed attachment, published ports, and resolved endpoint inspection | Create/get/list/remove, typed TSI/disabled/bridge selection, endpoint responses, and TCP publication have three-language parity. Prune remains pending; live hot-plug is intentionally unsupported. |
| Commands and scripts | foreground argv/shell/script execution, environment, cwd, user, stdin, timeout, binary-safe output, background processes, signals, wait, and streaming | Foreground argv/shell and stdin-backed fluent scripts have three-language parity and pass the real macOS/HVF builder-to-E2B smoke. Process handles, signals, wait, and streaming remain pending. |
| Files and artifacts | binary/text read/write, stat, exists, list, mkdir, move, remove, streaming, confined export, size limits, and hashes | Core mutations implemented; artifact/export layer and large-file streaming pending. |
| Filesystem snapshots | capture, size, restore, delete, in-use fencing, and cleanup | Rust and bridge foundations implemented; real-backend release gate pending. |
| Lifecycle | create, connect, inspect, list, pause, resume, restart, timeout replacement, stop, kill, remove, and deterministic cleanup | Partial high-level parity. |
| Observability | structured logs, stats, events, health, audit data, and runtime diagnostics | Partial Rust direct-client coverage; language parity pending. |
| PTY | create, resize, input, output streaming, wait, and cancellation | Rust lower-level primitives only. |
| Security | typed isolation, resource limits, read-only policy, capabilities, devices, secret injection, and attestation | Partial; unsupported policies must be rejected rather than represented as enforced. |

## Delivery Sequence

### Phase 1: Typed build and runtime primitives

- Keep this plan and the capability matrix current.
- Expose image build and resource-management operations through the bridge and
  native Python/TypeScript clients.
- Add typed volume mounts, network selection, published ports, workdir, and
  persistence controls to box creation.
- Add explicit script execution and executable examples in all three languages.
- Retain and validate runtime-managed filesystem snapshot operations.

### Phase 2: Results, caches, and artifacts

- Complete lifecycle, structured logs, stats, and snapshot inspection parity.
- Add confined artifact export with hashes and limits.
- Document named-volume cache patterns and warm-base snapshot patterns.
- Add a checked API inventory so exported operations cannot drift silently.

### Phase 3: Processes and interactive execution

- Expose background process handles, output streaming, wait, signals, and PTY
  operations through the same bridge.
- Add cancellation and cleanup behavior for interrupted language clients.

### Phase 4: Optional composition and release gates

- Refactor any retained matrix or DAG helper onto the typed toolbox.
- Run every supported operation through Rust, Python, and TypeScript against
  certified Linux Sandbox execution.
- Run the supported matrix against Linux/KVM and macOS/HVF MicroVM execution.
- Build and install clean package artifacts before testing them.
- Publish Rust, Python, TypeScript, runtime, and documentation from one version
  and verify the public registries after release.

## Completion Gates

The SDK objective is complete only when all of the following are true:

- the required native SDK surface table has no `Partial` or pending row;
- the checked Rust/Python/TypeScript inventory reports parity;
- all bridge operations have success, validation, runtime-error, and malformed
  response tests;
- the real Sandbox three-language matrix covers every supported operation;
- the MicroVM matrix covers every operation supported on Linux/KVM and
  macOS/HVF;
- build, volume, network, port, script, cache, snapshot, artifact, cancellation,
  and cleanup scenarios have end-to-end evidence;
- release artifacts contain the tested implementation and public registry
  versions match the runtime release;
- root and package READMEs contain only commands that work with the currently
  published versions.
