# A3S Box Runtime

Local MicroVM, Sandbox, and shared A3S Runtime provider implementation for A3S
Box.

## Overview

This package provides the actual runtime implementation for A3S Box, including:

- **VM Management**: MicroVM lifecycle management with libkrun
- **OCI Image Support**: Pull, store, and extract OCI container images
- **Rootfs Builder**: Construct guest root filesystems from OCI layers
- **gRPC Communication**: Guest agent health checking over Unix socket
- **Filesystem Operations**: virtio-fs mount management
- **Metrics Collection**: Runtime metrics and monitoring
- **A3S Runtime Provider**: generation-fenced Tasks and Services over Linux
  MicroVM and explicit Sandbox backends
- **Transient Secrets**: caller-authorized environment and file Secret
  materialization without durable plaintext

## Architecture

The runtime package builds on top of `a3s-box-core` which provides foundational types:

```
┌─────────────────────────────────────┐
│         a3s-box-runtime             │
│  (VM, OCI, Rootfs, gRPC, etc.)     │
└─────────────────────────────────────┘
                 │
                 ▼
┌─────────────────────────────────────┐
│          a3s-box-core               │
│  (Config, Error, Event)            │
└─────────────────────────────────────┘
```

## Shared A3S Runtime provider

`BoxRuntimeDriver` implements the shared `a3s-runtime` contract. It maps
digest-pinned OCI Tasks and Services onto the canonical local execution manager
instead of creating a second Box lifecycle path. The driver owns provider
identity, generation fencing, recovery, Service endpoints, health observations,
logs, exec, resource controls, and cleanup.

The provider pins `a3s-runtime` 0.5.0 at
`4c5fbd56bedd84d1007a7d9cd046a9f7083bbdcd` and advertises its atomic
`ServiceLifecycle` feature. `RuntimeUnitSpec::health` remains readiness for
traffic admission. `RuntimeServiceLifecycle::liveness` independently drives
recovery according to the declared restart policy, while
`shutdown_grace_seconds` is persisted as an exact `SIGTERM` grace interval
before Box forces termination. Apply, inspect, and exec use independent
threshold trackers for the two probes, refresh the canonical execution after
every in-flight probe, and apply at most one liveness restart per public
operation. The durable Box execution generation remains the sole restart
counter and lifecycle authority.

The lifecycle-aware provider timeout reserves the complete declared grace plus
the normal control-plane budget. Source-level coverage proves threshold reset,
one-restart generation advancement, exact policy mapping, and timeout
reservation. Advertising the feature also activates the real-provider
readiness/liveness separation, liveness transition, graceful stop, and forced
deadline cases; those ignored-by-default cases require the dedicated Linux
Sandbox or KVM certification environment.

Both Linux isolation paths implement the same advertised Runtime profile set.
`NetworkMode::None` and `NetworkMode::Service` remain loopback-only and do not
grant workload egress. A MicroVM retains explicit vsock IPC without libkrun TSI
socket interception; Runtime Service listeners relay to the declared guest TCP
ports through the generation-fenced execution connector.

Callers that need Runtime Secrets compose the driver with exactly one
`BoxSecretMaterializer`:

```rust
use std::sync::Arc;

use a3s_box_core::ExecutionIsolation;
use a3s_box_runtime::{BoxRuntimeDriver, BoxRuntimeDriverConfig};

let config = BoxRuntimeDriverConfig {
    secret_root: "/run/a3s-box/runtime-secrets".into(),
    ..Default::default()
};
let driver = BoxRuntimeDriver::new_with_isolation(config, ExecutionIsolation::Sandbox)?
    .with_secret_materializer(Arc::new(node_secret_materializer));
```

The configured `secret_root` must already be a canonical provider-owned `0700`
Linux tmpfs mount. Box never creates it and never falls back to disk. The caller
owns reference authorization and transport; Box owns only node-local
materialization, deterministic isolation-specific read-only mounts, recovery
validation, log redaction, and lifecycle cleanup.

`BoxTransientSecretStore` exposes that same validated tmpfs owner to Box's
local Compose adapter. Compose creates a private scope only after the parent
tmpfs passes the provider checks; it supplies zeroizing values and receives
only mount paths plus the guest manifest. For MicroVM single-file mounts, the
virtio-fs directory staging copy stays inside that tmpfs scope. The ordinary
disk-backed `.filemounts` fallback is never used for managed Secret sources.

Environment, file, and registry credential targets are supported. Environment
values use a non-sensitive `A3S_BOX_SECRET_ENV_V1` binding manifest that Guest
Init validates and consumes immediately before process creation. File material
uses the exact requested mode. A registry credential is resolved only when the
image is absent from the local cache, handed through a per-execution in-memory
broker, consumed at the image-pull boundary, and zeroized before rootfs
preparation continues. Cache hits do not call the resolver. With no registry
target the Runtime driver supplies explicit anonymous auth, preventing fallback
to persistent Box-local or process-environment credentials.

Secret bytes never enter durable Box records, creation intent, OCI
configuration, logs, cursors, or the image credential store. Stopping retains
environment and file material for an explicit restart; removal and stale-
generation retirement remove it. A reconstructed driver validates the existing
live generation without rematerializing it. Every log read reauthorizes only
workload-visible references, redacts exact values longest-first, and hashes only
redacted content into its cursor. Registry credentials are never injected into
the workload and therefore never become log-redaction inputs.

## Cloud image-build boundary

`BoxBuildPlan` is the closed A3S ACL admission contract for Cloud-owned image
build intent. It validates one `build "oci"` block, produces canonical ACL
bytes and a stable SHA-256 digest through `a3s-acl`, confines both the context
and build file to one canonical source root, and compiles into the existing
`BuildConfig`. The recorded-build API adds supervision by extending the same
native engine, receipt journal, and `ImageStore`; it does not introduce another
build backend, cache, queue, scheduler, lifecycle store, workspace registry, or
image authority. Content-changing build arguments are deliberately absent from
version 1; adding them requires a new closed schema so they cannot alter output
behind an unchanged plan digest.

```acl
build "oci" {
  cache = "content-addressed"
  context = "."
  file = "Dockerfile"
  network = "none"
  platform = "linux/amd64"
  schema = "a3s.box.build-plan.v1"
}
```

The admitted cache values are `content-addressed` and `disabled`. Network is
closed to `none` or `outbound`. `none` rejects remote URL `ADD`, binds the
policy into every `RUN` cache identity, and adds a private Linux network
namespace to every native `RUN`. Base-image and external-stage resolution stay
host-side OCI operations; they are not executed inside the build rootfs.
Network-none plans reject the warm RUN pool until that provider can prove an
equivalent isolated network boundary.

```rust
use std::path::Path;
use std::sync::Arc;

use a3s_box_core::OperationId;
use a3s_box_runtime::{
    execute_recorded_build_plan, BoxBuildPlan, BuildOperationIdentity,
};

let plan = BoxBuildPlan::parse_acl(plan_acl)?;
let identity = BuildOperationIdentity::new(
    OperationId::new("cloud-build-018f")?,
    source_artifact_digest,
)?;
let result = execute_recorded_build_plan(
    &identity,
    &plan,
    Path::new("/srv/a3s/source"),
    true,
    Arc::clone(&image_store),
)
.await?;
```

`start_recorded_build_plan` starts without blocking;
`inspect_recorded_build_status` returns running, cancelling, cancelled, failed,
or a revalidated successful output; and `cancel_recorded_build_plan` durably
requests cancellation through that same journal. A crash-released execution
lease is liveness evidence, not a second state store. Linux `RUN` uses one
asynchronous subprocess boundary with kill-on-drop, a parent-death signal, a
private PID namespace, and a journaled PID/start-time identity. Recovery fences
that process tree before removing the hash-derived operation workspace.
The existing journal lock serializes cancellation with ImageStore publication;
content-addressed plans use that same permit to publish one operation-owned
Box-native OCI cache artifact before the ImageStore commit. One trace records
hits and stores from the existing `BuildCache`; there is no export cache,
publisher, or cache lifecycle store alongside it. Export layers are copied
into an immutable snapshot so receipt validation and cleanup cannot mutate the
live cache authority through shared inodes.
Native OCI config and history records use the canonical epoch because a build
plan carries no creation clock, so identical inputs preserve the exact manifest
descriptor across rebuilds and parent-cache hydration.

The cache key length-binds the source digest, canonical plan digest, platform,
and cache schema profile. The v2 supervised operation record persists the
admitted cache policy: a new `content-addressed` operation must commit cache
evidence, while a `disabled` operation must not. Legacy v1 operation and output
records remain readable without cache evidence. Replay rejects extra,
missing, symlinked, or changed cache entries and revalidates the manifest,
config, layers, descriptor sizes, exact blob inventory, and byte count before
returning the cache receipt.

`hydrate_recorded_build_cache` accepts that existing `RecordedBuildCache`
type, repeats the same complete artifact validation, and publishes entries
through the sole `BuildCache` lock and blob/key write boundary. A valid local
key with different content fails the entire preflight before an imported key
is written; repeated imports are idempotent. Imported layers are copied, the
complete imported set is retained while unrelated old blobs are pruned, and a
successful return carries the exact revalidated receipt.

`assemble_recorded_build_outputs` accepts two to eight
`BuildOutputAssemblyInput` values after their single-platform native builds
complete. `BuildOutputAssembly` requires one source digest, identical context,
build file, target, network, and cache intent, and one unique plan-bound receipt
per platform. It sorts the inputs before constructing the OCI image index,
copies shared content-addressed blobs once, revalidates every input and the
complete assembled graph, then enters the same ImageStore commit boundary as a
direct build. It has no execution backend, queue, scheduler, receipt journal,
manifest store, or publisher.

```rust
use std::sync::Arc;

use a3s_box_runtime::{
    assemble_recorded_build_outputs, BuildOutputAssembly, BuildOutputAssemblyInput,
};

let assembly = BuildOutputAssembly::new(
    "registry.example/a3s/service:build-018f",
    source_artifact_digest,
    vec![
        BuildOutputAssemblyInput::new(amd64_plan, amd64_result.receipt),
        BuildOutputAssemblyInput::new(arm64_plan, arm64_result.receipt),
    ],
)?;
let image_index =
    assemble_recorded_build_outputs(&assembly, Arc::clone(&image_store)).await?;
```

This completes the Box-owned `BX0.4` supervision, portable cache receipt,
typed cache hydration, and multi-platform OCI assembly slices. Cloud Node Agent
consumption through its existing Fleet queue, Artifact publication, and
end-to-end SPDX/SLSA signing evidence remain separate integration gates.

## Components

### VM Manager

Manages the microVM lifecycle:

```rust
use a3s_box_runtime::VmManager;
use a3s_box_core::{BoxConfig, EventEmitter};

let config = BoxConfig::default();
let emitter = EventEmitter::new();
let vm = VmManager::new(config, emitter);

// Boot the VM (lazy initialization)
vm.boot().await?;

// Check health
let healthy = vm.health_check().await?;

// Destroy when done
vm.destroy().await?;
```

## VM States

The runtime manages the following VM states:

- **Created**: Config captured, no VM started
- **Ready**: VM booted, agent initialized, health check passing
- **Busy**: A session is actively processing
- **Compacting**: A session is compressing its context
- **Stopped**: VM terminated, resources freed

## gRPC Communication

The runtime communicates with the guest agent over Unix socket (bridged to vsock port 4088):

- Health checks

Agent-level operations (sessions, generation, skills) are handled by the
a3s-code crate, not the Box runtime.

## License

MIT
