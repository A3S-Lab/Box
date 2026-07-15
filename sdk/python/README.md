# A3S Box Python SDK

`a3s-box` is a typed convenience package around the checksum-pinned official
E2B Python clients used by A3S Box compatibility tests. It re-exports the
official `e2b` 2.32.0 API instead of maintaining a fork, so existing E2B code
can keep the same classes and method signatures.

```python
from a3s_box import A3SConnectionConfig, Sandbox

connection = A3SConnectionConfig.from_environment()

with Sandbox.create(
    "code-interpreter-v1",
    **connection.python_options(),
) as sandbox:
    result = sandbox.commands.run("python -c 'print(6 * 7)'")
    print(result.stdout)
```

The synchronous and asynchronous Code Interpreter exports are available from
`a3s_box.code_interpreter`.

`A3SConnectionConfig` reads `E2B_API_URL`, `E2B_DOMAIN`, and `E2B_API_KEY`
without changing process-global environment variables. The A3S endpoint still
decides the execution template and isolation policy; the SDK never invokes a
local runtime.
