# A3S Box Python SDK

`a3s-box` is a typed convenience package around the checksum-pinned official
E2B Python clients used by A3S Box compatibility tests. It re-exports the
official `e2b` 2.32.0 API instead of maintaining a fork, so existing E2B code
can keep the same classes and method signatures. A3S Box provides the runtime;
this native package does not read `E2B_API_URL` or contact E2B Cloud.

```bash
export A3S_BOX_ENDPOINT=https://api.box.example.com
export A3S_BOX_API_KEY=a3s_your_key
```

```python
import asyncio

from a3s_box import A3SConnectionConfig, AsyncSandbox


async def main() -> None:
    connection = A3SConnectionConfig.from_environment()
    sandbox = await AsyncSandbox.create(
        "code-interpreter-v1",
        **connection.python_options(),
    )
    async with sandbox:
        result = await sandbox.commands.run("python -c 'print(6 * 7)'")
        print(result.stdout)


asyncio.run(main())
```

The synchronous and asynchronous Code Interpreter exports are available from
`a3s_box.code_interpreter`.

The production-tested Sandbox backend supports memory-preserving pause through
the unchanged SDK methods: `await sandbox.pause(keep_memory=True)` followed by
`await sandbox.connect(timeout=60)`. The A3S OS matrix proves that a process
started before pause continues after resume. `keep_memory=False` performs a
durable filesystem-only pause: it retains the rootfs and replaces the runtime
generation on connect. Those cold-pause assertions are in the production gate
and await the next certified A3S OS run.

`A3SConnectionConfig` reads `A3S_BOX_ENDPOINT` and `A3S_BOX_API_KEY` without
changing process-global environment variables. It derives the Sandbox domain
from conventional `https://api.<domain>` endpoints. Set `A3S_BOX_DOMAIN` only
when that convention does not apply. The A3S service returns the public direct
Sandbox authority, including a non-standard TLS port when configured.
`A3S_BOX_SANDBOX_URL` is retained only for single-Sandbox fixtures. The A3S
endpoint decides the execution template and isolation policy; the SDK never
invokes a local runtime. `E2B_API_URL` is not read by this package. It is used
only when the unchanged official SDK is intentionally connected to the same
A3S Box endpoint.

Volume control requests use `connection.python_options()`. Volume content
requests use `connection.volume_options()` so they reach that same A3S Box
endpoint without `E2B_VOLUME_API_URL`:

```python
from a3s_box import A3SConnectionConfig, Volume

connection = A3SConnectionConfig.from_environment()
volume = Volume.create("data", **connection.python_options())
volume.write_file("/input.txt", "hello", **connection.volume_options())
```

Filesystem Snapshots use the same Sandbox connection. They capture rootfs
state, preserve the source Sandbox state, and restore into a writable private
copy-on-write layer:

```python
snapshot = await sandbox.create_snapshot(name="checkpoint")
restored = await AsyncSandbox.create(
    snapshot.snapshot_id,
    **connection.python_options(),
)
```
