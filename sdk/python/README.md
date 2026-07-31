# A3S Box Python SDK

`a3s-box` is a local-first Python SDK with native `Sandbox`, `commands`, and
`files` APIs. It controls the A3S Box runtime installed on the same machine.

## Local use

Install the A3S Box runtime and the Python package:

```bash
brew install a3s-lab/tap/a3s-box
python -m pip install a3s-box
```

No endpoint or API key is required:

```python
from a3s_box import Sandbox

with Sandbox.create("python:3.12-alpine") as sandbox:
    result = sandbox.commands.run("python -c 'print(6 * 7)'")
    print(result.stdout)

    sandbox.files.write("/workspace/note.txt", "hello")
    print(sandbox.files.read("/workspace/note.txt"))
```

`Sandbox.create()` defaults to `alpine:3.20` and MicroVM isolation. The first
argument is an OCI image reference in local mode. Select the shared-kernel
Sandbox backend explicitly on a certified Linux host:

```python
sandbox = Sandbox.create(
    "python:3.12-alpine",
    isolation="sandbox",
    cpus=2,
    memory_mb=1024,
)
```

By default, creation replaces the image command with a long-running keepalive
process. Set `command` and `entrypoint` to configure the initial OCI process,
including on Windows/WHPX where post-boot command execution is unavailable:

```python
sandbox = Sandbox.create(
    "alpine:3.20",
    entrypoint=["/bin/sh", "-c"],
    command=["echo ready; exec httpd -f -p 8080"],
)
```

Async applications use the same local runtime:

```python
import asyncio

from a3s_box import AsyncSandbox


async def main() -> None:
    async with await AsyncSandbox.create("python:3.12-alpine") as sandbox:
        result = await sandbox.commands.run(["python", "-c", "print(6 * 7)"])
        print(result.stdout)


asyncio.run(main())
```

## Lifecycle and inspection

Local Sandbox lifecycle calls are generation-fenced. `stop()` preserves the
durable Sandbox, `restart()` advances its generation under a caller-supplied
idempotency identity, `remove()` deletes a terminal Sandbox, and `kill()`
performs stop plus removal. Reuse the same `operation_id` when retrying a
restart whose outcome is not yet known.

```python
from a3s_box import A3SBoxClient, Sandbox

client = A3SBoxClient()
sandbox = Sandbox.create("alpine:3.20")

try:
    logs = sandbox.logs(tail=100)
    stats = sandbox.stats()
    print(len(logs), stats.memory_percent if stats else None)

    sandbox.stop()
    sandbox.restart(operation_id="ci-restart-1", stop_timeout=10)
    print(client.get_sandbox(sandbox.id))
finally:
    sandbox.kill()
```

Log snapshots contain structured stream, message, and timestamp values, and
accept tails from 1 through 10,000 entries. The runtime client also exposes
`list_sandboxes()`, `get_sandbox()`, `runtime_diagnostics()`,
`runtime_disk_usage()`, `list_filesystem_snapshots()`, and
`get_filesystem_snapshot()`. `A3SAsyncBoxClient` and `AsyncSandbox` provide
the same operations with `async` methods.

## Builder-style programmable CI/CD

The direct `Sandbox` API remains available for execution. For build and CI
tooling, `A3SBoxClient` adds fluent builders over the same local runtime and
bridge:

```python
from a3s_box import A3SBoxClient

client = A3SBoxClient()

image = (
    client.image("./ci")
    .dockerfile("Dockerfile")
    .tag("local/ci-base:latest")
    .build_arg("NODE_VERSION", "24")
    .build()
)
cache = (
    client.volume("npm-cache")
    .label("purpose", "ci-cache")
    .size_limit(10 * 1024 * 1024 * 1024)
    .create()
)
network = client.network("ci-net").subnet("10.89.40.0/24").create()

with (
    client.sandbox(image.reference)
    .cpus(4)
    .memory_mb(4096)
    .entrypoint("/usr/bin/env", "sh")
    .command("-c", "npm test && sleep 3600")
    .mount_named(cache.name, "/root/.npm")
    .network(network.name)
    .publish_tcp(8080, 8080)
    .workdir("/workspace")
    .start()
) as box:
    result = (
        box.script("npm ci\nnpm test\n")
        .interpreter("/bin/sh", "-se")
        .env("CI", "true")
        .run()
    )
    if result.exit_code != 0:
        raise RuntimeError(result.stderr)
```

`A3SAsyncBoxClient` provides the same builders with asynchronous terminal
operations. Named volumes and networks must be created explicitly before they
are mounted or selected. Builder scripts are sent through standard input to
the selected interpreter, so their contents are not interpolated into a host
shell command. Initial command and entrypoint argument vectors are validated
before the runtime is invoked.

Named bridge networks and published ports are currently MicroVM-only. A
shared-kernel Sandbox request that selects either fails before runtime
mutation; use `.disable_network()` or the default TSI-compatible configuration
for supported Sandbox workloads.

The package invokes the versioned machine bridge built into the installed
`a3s-box` executable. It does not parse human CLI output. Protocol v3 performs
one shared, complete capability handshake before the first normal operation,
including when callers start concurrently. Typed response fields and standard
Base64 are validated fail-closed; malformed responses use
`bridge_protocol_error`, a missing binary uses `binary_not_found`, and a local
bridge deadline uses `bridge_timeout`. Set `A3S_BOX_BINARY` only when the
executable is not on `PATH`.

Host resources use the same typed client:

```python
import os

from a3s_box import A3SBoxClient, RegistryCredentials, SignaturePolicy

client = A3SBoxClient()
credentials = RegistryCredentials("builder", os.environ["REGISTRY_PASSWORD"])
image = client.pull_image(
    "registry.example/ci/base:latest",
    credentials=credentials,
    signature_policy=SignaturePolicy.cosign_key("/keys/cosign.pub"),
)
metadata = client.inspect_image(image.reference)
history = client.image_history(image.reference)
tagged = client.tag_image(image.reference, "local/ci-base:tested")
client.push_image(
    tagged.reference,
    "registry.example/ci/base:tested",
    credentials=credentials,
)
client.prune_volumes()
client.prune_networks()
```

`client.capabilities()` returns bridge protocol version 3 and the exact 48
supported operation names. Registry passwords are passed only to the local
runtime process.
