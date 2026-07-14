# a3s-box-sdk

The Rust SDK for **a3s-box** direct runtime APIs.

By default, the SDK does not spawn the `a3s-box` CLI. `A3sBoxClient` calls
`a3s-box-runtime` stores and socket clients directly, returning typed Rust data
for management apps, automation, and tests.

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

## API Coverage

- Boxes: list, get, pause, unpause, stop on Unix, remove, prune inactive boxes,
  log snapshots, and host-side stats snapshots.
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

The client reads the shared `boxes.json` state format through an SDK-local model
so it does not depend on the CLI crate. Image, volume, network, snapshot, build,
registry, exec, PTY, and attestation operations use `a3s-box-runtime` directly.

The runtime now provides generation-fenced `create`, `start`, and
`create_and_start` operations over the canonical managed-execution store.
Container lifecycle orchestration is not exposed by this SDK yet: the SDK
adapter and the CLI-specific record-policy mapping still need behavior-parity
coverage before becoming default APIs. Pause, unpause, Unix stop, and box
removal remain available through the existing direct management surface. The
default SDK does not shell out for lifecycle commands.

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
