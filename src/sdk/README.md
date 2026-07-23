# a3s-box-sdk

The Rust SDK for **a3s-box** direct runtime APIs.

By default, the SDK does not spawn the `a3s-box` CLI. `A3sBoxClient` calls
`a3s-box-runtime` stores and socket clients directly, returning typed Rust data
for management apps, automation, and tests.

## E2B-Style Local Sandbox

The high-level `Sandbox` API is local and zero-configuration. It does not read
`E2B_API_KEY`, `A3S_BOX_API_KEY`, an endpoint, or a domain:

```rust
use a3s_box_sdk::Sandbox;

# async fn example() -> Result<(), a3s_box_sdk::ClientError> {
let sandbox = Sandbox::create("python:3.12-alpine").await?;
let result = sandbox
    .commands
    .run("python -c 'print(6 * 7)'")
    .await?;
println!("{}", result.stdout);

sandbox.files.write("/workspace/note.txt", "hello").await?;
assert_eq!(
    sandbox.files.read_text("/workspace/note.txt").await?,
    "hello"
);
sandbox.kill().await?;
# Ok(()) }
```

MicroVM isolation is the default. Shared-kernel Sandbox isolation is an
explicit opt-in and requires a certified Linux host:

```rust
use a3s_box_sdk::{ExecutionIsolation, Sandbox, SandboxCreateOptions};

# async fn example() -> Result<(), a3s_box_sdk::ClientError> {
let sandbox = Sandbox::create_with_options(
    SandboxCreateOptions::new("python:3.12-alpine")
        .isolation(ExecutionIsolation::Sandbox)
        .cpus(2)
        .memory_mb(1024),
)
.await?;
sandbox.kill().await?;
# Ok(()) }
```

The facade also provides `connect`, `pause`, `resume`, `is_running`, command
environment/working-directory/stdin options, and file metadata and mutation
operations. `A3sBoxClient` remains available for lower-level management APIs.

## Builder-Style Programmable CI/CD

The E2B-style facade and the fluent builders are two entry styles over the
same typed client and lifecycle implementation:

```rust
use a3s_box_sdk::{A3sBoxClient, SandboxNetwork};

# async fn example() -> Result<(), a3s_box_sdk::ClientError> {
let client = A3sBoxClient::new();
let image = client
    .image("./ci")
    .dockerfile("Dockerfile")
    .tag("local/ci-base:latest")
    .build_arg("NODE_VERSION", "24")
    .build()
    .await?;
let cache = client
    .volume("npm-cache")
    .label("purpose", "ci-cache")
    .size_limit(10 * 1024 * 1024 * 1024)
    .create()?;
let network = client
    .network("ci-net")
    .subnet("10.89.40.0/24")
    .create()?;

let sandbox = client
    .sandbox(image.reference)
    .cpus(4)
    .memory_mb(4096)
    .mount_named(cache.name, "/root/.npm")
    .network(SandboxNetwork::bridge(network.name))
    .publish_tcp(8080, 8080)
    .workdir("/workspace")
    .start()
    .await?;

let result = sandbox
    .script("npm ci\nnpm test\n")
    .interpreter(["/bin/sh", "-se"])
    .env("CI", "true")
    .run()
    .await?;
sandbox.kill().await?;
if result.exit_code != 0 {
    return Err(a3s_box_sdk::ClientError::Guest(result.stderr));
}
# Ok(()) }
```

Named volumes and networks must be created explicitly before selection.
Scripts are sent through standard input to the selected interpreter and are
not interpolated into a host shell command. Typed bind mounts, named-volume
mounts, tmpfs, TSI/disabled/bridge networking, TCP publications, DNS, host
aliases, workdir/user/hostname, read-only root filesystems, persistence,
automatic cleanup, and filesystem-snapshot restore are available on
`SandboxBuilder` and `SandboxCreateOptions`.

Named bridge networks and published ports are currently MicroVM-only. The
shared-kernel Sandbox resolver rejects either before runtime mutation.

The same native Python and TypeScript packages now expose the Rust client's
local image lifecycle: get, inspect, history, pull, tag, push, remove, and
cache eviction. Pull accepts typed registry credentials and cosign verification
policies; push accepts typed credentials and an explicit HTTPS or trusted
plain-HTTP registry protocol. Named volumes and networks also expose prune.
`sdk_capabilities` reports the exact operation inventory supported by the
installed runtime so applications can fail closed before using a newer API.

## Runtime-Backed Client

```rust
use a3s_box_sdk::{
    A3sBoxClient, BuildImage, CreateNetwork, CreateSnapshot, CreateVolume, ListBoxesOptions,
    PullImage, ReadBoxLogsOptions, RemoveBox, RestoreSnapshot, StopBox,
};

# async fn example() -> Result<(), a3s_box_sdk::ClientError> {
let client = A3sBoxClient::new();

let boxes = client.list_boxes(ListBoxesOptions::all())?;
let disk = client.runtime_disk_usage()?;
let stats = client.list_box_stats()?;
let logs = client.read_box_logs("web", ReadBoxLogsOptions::tail(20))?;
let stopped = client.stop_box("web", StopBox::new()).await?;
let snapshot = client.create_snapshot("web", CreateSnapshot::new().name("web-snapshot"))?;
let restored = client.restore_snapshot(&snapshot.id, RestoreSnapshot::new())?;
let removed = client.remove_box("web", RemoveBox::new())?;

let pulled = client.pull_image(PullImage::new("alpine:latest")).await?;
let inspect = client.inspect_image("alpine:latest").await?;
let history = client.image_history("alpine:latest").await?;
let built = client
    .build_image(BuildImage::new(".").tag("local/app:dev").quiet(true))
    .await?;
let tagged = client
    .tag_image(a3s_box_sdk::TagImage::new("local/app:dev", "local/app:latest"))
    .await?;

let volume = client.create_volume(CreateVolume::new("cache").label("role", "build"))?;
let network = client.create_network(CreateNetwork::new("dev").subnet("10.89.44.0/24"))?;

println!(
    "{} boxes, {} disk bytes, {} stats, {} logs, stopped {}, snapshot {}, restored {}, removed {}, pulled {}, inspected {}, history {}, built {}, tagged {}, volume {}, network {}",
    boxes.len(),
    disk.total_bytes,
    stats.len(),
    logs.len(),
    stopped.name,
    snapshot.name,
    restored.name,
    removed.name,
    pulled.reference,
    inspect.is_some(),
    history.as_ref().map_or(0, Vec::len),
    built.reference,
    tagged.reference,
    volume.name,
    network.name
);
# Ok(()) }
```

Use `A3sBoxClient::from_home(path)` for tests or tools that should operate on a
non-default a3s-box state directory.

## Managed Lifecycle

The SDK submits lifecycle requests directly to the same generation-fenced
`ExecutionManager` used by the CLI and compatibility service. It does not spawn
the CLI or construct a parallel box record.

```rust
use std::collections::BTreeMap;

use a3s_box_sdk::{
    A3sBoxClient, BoxConfig, CreateExecutionRequest, ExecutionIsolation,
    ExecutionRecordPolicy, OperationId,
};

# async fn lifecycle() -> Result<(), a3s_box_sdk::ClientError> {
let client = A3sBoxClient::new();
let operation = OperationId::new("example-create")?;
let request = CreateExecutionRequest {
    external_sandbox_id: "example-sandbox".to_string(),
    config: BoxConfig {
        image: "alpine:latest".to_string(),
        isolation: ExecutionIsolation::Sandbox,
        cmd: vec!["sleep".to_string(), "60".to_string()],
        ..BoxConfig::default()
    },
    labels: BTreeMap::new(),
    policy: ExecutionRecordPolicy {
        name: Some("sdk-example".to_string()),
        ..ExecutionRecordPolicy::default()
    },
};

let reservation = client.create_box(request, &operation).await?;
let lease = client
    .start_box(&reservation.execution_id, reservation.generation)
    .await?;
let status = client.inspect_execution(&lease.execution_id).await?;
client
    .kill_execution(&status.execution_id, status.generation)
    .await?;
# Ok(()) }
```

`run_box` provides the idempotent create-and-start composition. Typed methods
also expose inspect, pause, resume, restart, kill, and operation reconciliation.
`A3sBoxClient::with_execution_manager` accepts an explicit typed manager for
embedding or tests without changing request semantics.

## API Coverage

- Boxes: generation-fenced create, start, run, inspect, pause, resume, restart,
  kill, and reconciliation; plus list, get, legacy pause/unpause, Unix stop,
  remove, prune inactive boxes, log snapshots, and host-side stats snapshots.
- Images: list, get, inspect local OCI metadata, read OCI history, pull, build,
  tag, push, remove, and evict.
- Volumes: list, get, create, remove, and prune.
- Networks: list, get, create, remove, connect inactive boxes, disconnect inactive
  boxes, and prune.
- Snapshots: list, get, create from a box rootfs, restore into a new created box
  record, remove, and prune.
- Diagnostics: a3s-box/core/runtime/SDK versions, home path, host
  virtualization availability, and runtime disk usage grouped by boxes, images,
  volumes, snapshots, state files, and other local data.
- Running boxes on Unix: exec, file transfer, heartbeat, main-process signal,
  deferred-main spawn, PTY client, and attestation report through runtime sockets.
- E2B-style local use: zero-configuration `Sandbox`, `commands`, `files`,
  lifecycle, explicit MicroVM/shared-kernel isolation, and typed client
  injection for embedding and tests.
- Language bridge: versioned capability inventory plus image
  get/inspect/history/pull/build/tag/push/remove/evict and
  volume/network create/get/list/remove/prune parity in Python sync/async and
  TypeScript.

The client reads the shared `boxes.json` state format through an SDK-local model
so it does not depend on the CLI crate. Image, volume, network, snapshot, build,
registry, exec, PTY, and attestation operations use `a3s-box-runtime` directly.

Managed lifecycle methods preserve the complete typed `BoxConfig` and
`ExecutionRecordPolicy` request and call the canonical runtime facade. Pause,
unpause, Unix stop, and box removal remain available through the existing
query-based management surface for backwards compatibility. The default SDK
does not shell out for lifecycle commands.

## Maintenance Calls

Destructive APIs include `remove_box`, `prune_boxes`, `remove_image`, `evict_images`,
`remove_volume`, `prune_volumes`, `remove_network`, `prune_networks`,
`remove_snapshot`, and `prune_snapshots`. Product UIs should pair these with
selection state and confirmation prompts.

`restore_snapshot` is not destructive, but it creates a new box record and box
directory, so product UIs should still pair it with explicit source selection
and confirmation.

## Optional Pipeline Runner

The historical programmable CI runner is still available behind an explicit
feature:

```bash
cargo test -p a3s-box-sdk --features pipeline-cli
A3S_BOX=/path/to/a3s-box cargo run -p a3s-box-sdk --features pipeline-cli --bin a3s-box-ci
```

This optional runner drives lifecycle-heavy commands through the installed
`a3s-box` binary because those flows are not yet exposed as stable runtime client
APIs. It is not part of the default SDK surface.
