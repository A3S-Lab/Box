# A3S Box TypeScript SDK

`@a3s-lab/box` re-exports the official `e2b` 2.33.0 TypeScript API and the
pinned `@e2b/code-interpreter` 2.6.1 package. It does not fork or translate the
public protocol. A3S Box provides the runtime; this native package does not
read `E2B_API_URL` or contact E2B Cloud.

```bash
export A3S_BOX_ENDPOINT=https://api.box.example.com
export A3S_BOX_API_KEY=a3s_your_key
```

```typescript
import { A3SConnectionConfig, Sandbox } from '@a3s-lab/box'

const connection = A3SConnectionConfig.fromEnvironment(process.env)
const sandbox = await Sandbox.create('code-interpreter-v1', {
  ...connection.typescriptOptions(),
  timeoutMs: 60_000,
})

try {
  const result = await sandbox.commands.run('node -e "console.log(6 * 7)"')
  console.log(result.stdout)
} finally {
  await sandbox.kill()
}
```

Code Interpreter exports are available from `@a3s-lab/box/code-interpreter`.
The production-tested Sandbox backend supports memory-preserving pause through
the unchanged SDK methods: `await sandbox.pause({ keepMemory: true })` followed
by `await sandbox.connect({ timeoutMs: 60_000 })`. The A3S OS matrix proves
that a process started before pause continues after resume. `keepMemory: false`
performs a durable filesystem-only pause: it retains the rootfs and replaces
the runtime generation on connect. Those cold-pause assertions are in the
production gate and await the next certified A3S OS run.

`A3SConnectionConfig` derives the Sandbox domain from conventional
`https://api.<domain>` endpoints. Set `A3S_BOX_DOMAIN` only when that convention
does not apply. The service returns the public direct Sandbox authority,
including a configured non-standard TLS port. `A3S_BOX_SANDBOX_URL` is retained
only for single-Sandbox fixtures. The A3S service owns template and isolation
selection; this package never starts a local container or runtime.
`E2B_API_URL` is not read by this package; that name is used only when an
unchanged official SDK is connected directly to the same A3S Box endpoint.

Volume control requests use `connection.typescriptOptions()`. Volume content
requests use `connection.volumeOptions()` so they reach that same A3S Box
endpoint without `E2B_VOLUME_API_URL`:

```typescript
import { A3SConnectionConfig, Volume } from '@a3s-lab/box'

const connection = A3SConnectionConfig.fromEnvironment(process.env)
const volume = await Volume.create('data', connection.typescriptOptions())
await volume.writeFile('/input.txt', 'hello', connection.volumeOptions())
```

Filesystem Snapshots use the same Sandbox connection. They capture rootfs
state, preserve the source Sandbox state, and restore into a writable private
copy-on-write layer:

```typescript
const snapshot = await sandbox.createSnapshot({ name: 'checkpoint' })
const restored = await Sandbox.create(snapshot.snapshotId, {
  ...connection.typescriptOptions(),
})
```
