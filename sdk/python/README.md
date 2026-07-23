# A3S Box Python SDK

`a3s-box` is a local-first Python SDK with familiar E2B-style `Sandbox`,
`commands`, and `files` APIs. It controls the A3S Box runtime installed on the
same machine. It does not depend on, wrap, import, or contact the official E2B
SDK.

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

## Builder-style programmable CI/CD

The E2B-style API remains available for direct execution. For build and CI
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
shell command.

Named bridge networks and published ports are currently MicroVM-only. A
shared-kernel Sandbox request that selects either fails before runtime
mutation; use `.disable_network()` or the default TSI-compatible configuration
for supported Sandbox workloads.

The package invokes the versioned machine bridge built into the installed
`a3s-box` executable. It does not parse human CLI output. Set `A3S_BOX_BINARY`
only when the executable is not on `PATH`.

## Remote and self-hosted deployments

`A3S_BOX_ENDPOINT`, `A3S_BOX_API_KEY`, `A3S_BOX_DOMAIN`, and
`A3S_BOX_SANDBOX_URL` are remote-only settings. Local `Sandbox.create()` never
reads them.

The native package exposes `A3SRemoteConnection` as a typed configuration
helper for applications that deliberately install an unchanged official E2B
client and point it at a remote A3S Box compatibility service:

```python
from a3s_box import A3SRemoteConnection
from e2b import Sandbox as RemoteSandbox

connection = A3SRemoteConnection.from_environment()
remote = RemoteSandbox.create(
    "code-interpreter-v1",
    **connection.official_python_options(),
)
remote.kill()
```

This explicit migration path is separate from the native local SDK. The
official client is not a dependency of `a3s-box`.

See the repository README for complete self-hosted endpoint, wildcard DNS,
TLS, and API-key setup.
