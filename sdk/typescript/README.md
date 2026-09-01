# A3S Box TypeScript SDK

`@a3s-lab/box` is a local-first TypeScript SDK with native `Sandbox`,
`commands`, and `files` APIs. It controls the A3S Box runtime installed on the
same machine.

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

Export one bounded build or test artifact, optionally writing it to an exact
host destination:

```typescript
const artifactSandbox = await Sandbox.create('alpine:3.20')
try {
  const artifact = await artifactSandbox.files.export('/workspace/report.json', {
    maxBytes: 8 * 1024 * 1024,
    destination: 'artifacts/report.json',
  })
  console.log(artifact.size, artifact.sha256)
} finally {
  await artifactSandbox.kill()
}
```

Artifact export has a hard 8 MiB ceiling. It checks the file type and size
before reading, rejects declared-size mismatches or stat/read size changes, and returns the bytes
with a lowercase SHA-256 digest. A destination is created exclusively; an
existing host file is never overwritten.

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

By default, creation replaces the image command with a long-running keepalive
process. Set `command` and `entrypoint` to configure the initial OCI process,
including on Windows/WHPX where post-boot command execution is unavailable:

```typescript
const sandbox = await Sandbox.create('alpine:3.20', {
  entrypoint: ['/bin/sh', '-c'],
  command: ['echo ready; exec httpd -f -p 8080'],
})
```

## Lifecycle and inspection

Local Sandbox lifecycle calls are generation-fenced. `stop()` preserves the
durable Sandbox, `restart()` advances its generation under a caller-supplied
idempotency identity, `remove()` deletes a terminal Sandbox, and `kill()`
performs stop plus removal. Reuse the same `operationId` when retrying a
restart whose outcome is not yet known.

```typescript
import { A3SBoxClient, Sandbox } from '@a3s-lab/box'

const client = new A3SBoxClient()
const sandbox = await Sandbox.create('alpine:3.20')

try {
  const logs = await sandbox.logs({ tail: 100 })
  const stats = await sandbox.stats()
  console.log(logs.length, stats?.memoryPercent)

  const processes = await sandbox.processes()
  const runtimeStats = await sandbox.runtimeStats()
  const events = await sandbox.events({
    afterSequence: 0,
    limit: 256,
    waitTimeoutMs: 1_000,
  })
  const eventStream = sandbox
    .streamEvents({ afterSequence: events.nextSequence })
    [Symbol.asyncIterator]()
  console.log(processes.processes.length, runtimeStats.memory.usageBytes)
  console.log(events.nextSequence)
  await sandbox.updateResources(
    { cpuShares: 512 },
    { operationId: 'ci-resources-1' }
  )
  console.log((await eventStream.next()).value?.kind)
  await eventStream.return()

  await sandbox.stop()
  await sandbox.restart({
    operationId: 'ci-restart-1',
    stopTimeoutSeconds: 10,
  })
  console.log(await client.getSandbox(sandbox.id))
} finally {
  await sandbox.kill()
}
```

Log snapshots contain structured stream, message, and timestamp values, and
accept tails from 1 through 10,000 entries. The runtime client also exposes
`listSandboxes()`, `getSandbox()`, `runtimeDiagnostics()`,
`runtimeDiskUsage()`, `listFilesystemSnapshots()`, and
`getFilesystemSnapshot()`.

`processes()`, `runtimeStats()`, `events()`, and `streamEvents()` accept a
running or paused Sandbox and preserve its exact generation;
`updateResources()` requires a running Sandbox. Event polls and stream batches
default to 256 items and accept at most 4,096. `streamEvents()` is an
`AsyncIterable`; pass an `AbortSignal` to cancel an active local bridge process.
Streams terminate on generation drift instead of following a restart. Reuse an
explicit resource-update `operationId` when retrying an outcome that is not yet
known. Runtime and event timestamps are Unix epoch nanoseconds. They remain
JavaScript `number` values for API compatibility, so current-epoch timestamps
can round sub-microsecond digits; use event sequence values for exact ordering.

## Builder-style programmable CI/CD

The direct `Sandbox` API remains available for execution. For build and CI
tooling, `A3SBoxClient` adds fluent builders over the same local runtime and
bridge:

```typescript
import { A3SBoxClient } from '@a3s-lab/box'

const client = new A3SBoxClient()

const image = await client
  .image('./ci')
  .dockerfile('Dockerfile')
  .tag('local/ci-base:latest')
  .buildArg('NODE_VERSION', '24')
  .build()
const cache = await client
  .volume('npm-cache')
  .label('purpose', 'ci-cache')
  .sizeLimit(10 * 1024 * 1024 * 1024)
  .create()
const network = await client
  .network('ci-net')
  .subnet('10.89.40.0/24')
  .create()

const box = await client
  .sandbox(image.reference)
  .cpus(4)
  .memoryMb(4096)
  .entrypoint('/usr/bin/env', 'sh')
  .command('-c', 'npm test && sleep 3600')
  .mountNamed(cache.name, '/root/.npm')
  .network(network.name)
  .publishTcp(8080, 8080)
  .workdir('/workspace')
  .start()

try {
  const result = await box
    .script('npm ci\nnpm test\n')
    .interpreter('/bin/sh', '-se')
    .env('CI', 'true')
    .run()
  if (result.exitCode !== 0) throw new Error(result.stderr)
} finally {
  await box.kill()
}
```

Named volumes and networks must be created explicitly before they are mounted
or selected. Builder scripts are sent through standard input to the selected
interpreter, so their contents are not interpolated into a host shell command.
Initial command and entrypoint argument vectors are validated before the
runtime is invoked.

Named bridge networks and published ports are currently MicroVM-only. A
shared-kernel Sandbox request that selects either fails before runtime
mutation; use `.disableNetwork()` or the default TSI-compatible configuration
for supported Sandbox workloads.

The package invokes the versioned machine bridge built into the installed
`a3s-box` executable. It does not parse human CLI output. Protocol v3 performs
one shared, complete capability handshake before the first normal operation,
including when callers start concurrently. Typed values, standard Base64, and
Sandbox identity, generation, state, and isolation are validated fail-closed;
malformed responses use `bridge_protocol_error`, a missing binary uses
`binary_not_found`, and a local bridge deadline uses `bridge_timeout`. Set
`A3S_BOX_BINARY` only when the executable is not on `PATH`, or inject a typed
`A3SLocalRuntime` object in application tests.

Host resources use the same typed client:

```typescript
import {
  A3SBoxClient,
  RegistryCredentials,
  SignaturePolicy,
} from '@a3s-lab/box'

const client = new A3SBoxClient()
const password = process.env.REGISTRY_PASSWORD
if (!password) throw new Error('REGISTRY_PASSWORD is required')
const credentials = new RegistryCredentials('builder', password)
const image = await client.pullImage('registry.example/ci/base:latest', {
  credentials,
  signaturePolicy: SignaturePolicy.cosignKey('/keys/cosign.pub'),
})
const metadata = await client.inspectImage(image.reference)
const history = await client.imageHistory(image.reference)
const tagged = await client.tagImage(image.reference, 'local/ci-base:tested')
await client.pushImage(
  tagged.reference,
  'registry.example/ci/base:tested',
  { credentials }
)
await client.pruneVolumes()
await client.pruneNetworks()
```

`client.capabilities()` returns bridge protocol version 3 and the exact 52
supported operation names. Registry passwords are passed only to the local
runtime process.
