# A3S Box TypeScript SDK

`@a3s-lab/box` is a local-first TypeScript SDK with familiar E2B-style
`Sandbox`, `commands`, and `files` APIs. It controls the A3S Box runtime
installed on the same machine. It does not depend on, wrap, import, or contact
the official E2B SDK.

## Local use

Install the A3S Box runtime and the TypeScript package:

```bash
brew install a3s-lab/tap/a3s-box
npm install @a3s-lab/box
```

No endpoint or API key is required:

```typescript
import { Sandbox } from '@a3s-lab/box'

const sandbox = await Sandbox.create('python:3.12-alpine')

try {
  const result = await sandbox.commands.run(
    'python -c "print(6 * 7)"'
  )
  console.log(result.stdout)

  await sandbox.files.write('/workspace/note.txt', 'hello')
  console.log(await sandbox.files.read('/workspace/note.txt'))
} finally {
  await sandbox.kill()
}
```

`Sandbox.create()` defaults to `alpine:3.20` and MicroVM isolation. The first
argument is an OCI image reference in local mode. Select the shared-kernel
Sandbox backend explicitly on a certified Linux host:

```typescript
const sandbox = await Sandbox.create('python:3.12-alpine', {
  isolation: 'sandbox',
  cpus: 2,
  memoryMb: 1024,
})
```

The package invokes the versioned machine bridge built into the installed
`a3s-box` executable. It does not parse human CLI output. Set `A3S_BOX_BINARY`
only when the executable is not on `PATH`, or inject a typed
`A3SLocalRuntime` object in application tests.

## Remote and self-hosted deployments

`A3S_BOX_ENDPOINT`, `A3S_BOX_API_KEY`, `A3S_BOX_DOMAIN`, and
`A3S_BOX_SANDBOX_URL` are remote-only settings. Local `Sandbox.create()` never
reads them.

The native package exposes `A3SRemoteConnection` as a typed configuration
helper for applications that deliberately install an unchanged official E2B
client and point it at a remote A3S Box compatibility service:

```typescript
import { A3SRemoteConnection } from '@a3s-lab/box'
import { Sandbox as RemoteSandbox } from 'e2b'

const connection = A3SRemoteConnection.fromEnvironment(process.env)
const remote = await RemoteSandbox.create('code-interpreter-v1', {
  ...connection.officialSdkOptions(),
})
await remote.kill()
```

This explicit migration path is separate from the native local SDK. The
official client is not a dependency of `@a3s-lab/box`.

See the repository README for complete self-hosted endpoint, wildcard DNS,
TLS, and API-key setup.
