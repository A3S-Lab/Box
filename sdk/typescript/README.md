# A3S Box TypeScript SDK

`@a3s-lab/box` re-exports the official `e2b` 2.33.0 TypeScript API and the
pinned `@e2b/code-interpreter` 2.6.1 package. It does not fork or translate the
public protocol.

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
The A3S service owns template and isolation selection; this package never
starts a local container or runtime.
