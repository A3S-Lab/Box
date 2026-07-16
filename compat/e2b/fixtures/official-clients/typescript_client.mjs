#!/usr/bin/env node
/** Exercise pinned official TypeScript lifecycle clients against the recorder. */

import assert from 'node:assert/strict'
import { Sandbox, SandboxNotFoundError, Volume } from 'e2b'
import { Sandbox as CodeInterpreter } from '@e2b/code-interpreter'

const apiUrl = process.argv[2]
if (!apiUrl) {
  throw new Error('API URL argument is required')
}

const connection = {
  apiKey: 'e2b_a1b2c3',
  apiUrl,
}
const volumeApi = { apiUrl }
const volume = await Volume.create('fixture-data', connection)
assert.equal(volume.token, 'fixture-volume-token')
const connectedVolume = await Volume.connect(volume.volumeId, connection)
assert.equal(connectedVolume.name, 'fixture-data')
assert.ok((await Volume.list(connection)).some((item) => item.volumeId === volume.volumeId))
const directory = await volume.makeDir('/nested', {
  ...volumeApi,
  force: true,
  mode: 0o755,
})
assert.equal(directory.path, '/nested')
const written = await volume.writeFile('/nested/value.txt', 'volume-value', {
  ...volumeApi,
  mode: 0o644,
})
assert.equal(written.size, 'volume-value'.length)
assert.equal(await volume.exists('/nested/value.txt', volumeApi), true)
const updated = await volume.updateMetadata(
  '/nested/value.txt',
  { mode: 0o600 },
  volumeApi
)
assert.equal(updated.mode, 0o600)
assert.equal((await volume.list('/', { ...volumeApi, depth: 2 })).length, 2)
assert.equal(
  await volume.readFile('/nested/value.txt', volumeApi),
  'volume-value'
)
await volume.remove('/nested', volumeApi)

const sandbox = await Sandbox.create('fixture-template', {
  ...connection,
  allowInternetAccess: false,
  envs: { BETA: 'two', ALPHA: 'one' },
  lifecycle: {
    onTimeout: { action: 'pause', keepMemory: false },
    autoResume: false,
  },
  metadata: { team: 'alpha beta', purpose: 'fixture' },
  secure: true,
  timeoutMs: 321_000,
  volumeMounts: { '/mnt/data': volume },
})
assert.equal(sandbox.sandboxId, 'fixture-sandbox')

assert.equal(await sandbox.pause({ keepMemory: true }), true)
assert.equal(await sandbox.pause({ keepMemory: true }), false)

const connected = await Sandbox.connect('fixture-sandbox', {
  ...connection,
  timeoutMs: 222_000,
})
assert.equal(connected.sandboxId, 'fixture-sandbox')

const paginator = Sandbox.list({
  ...connection,
  limit: 2,
  nextToken: 'cursor-0',
  query: {
    metadata: { team: 'alpha beta' },
    state: ['running', 'paused'],
  },
})
const listed = await paginator.nextItems()
assert.equal(listed.length, 1)
assert.equal(listed[0].volumeMounts[0].name, 'fixture-data')
assert.equal(listed[0].volumeMounts[0].path, '/mnt/data')

const snapshot = await sandbox.createSnapshot({ name: 'fixture-state' })
assert.ok(snapshot.snapshotId)
assert.deepEqual(snapshot.names, [snapshot.snapshotId])
const snapshots = await sandbox.listSnapshots({ limit: 1 }).nextItems()
assert.equal(snapshots.length, 1)
assert.equal(snapshots[0].snapshotId, snapshot.snapshotId)
const restored = await Sandbox.create(snapshot.snapshotId, connection)
assert.equal(restored.sandboxId, 'fixture-restored')
assert.equal(await restored.kill(), true)
assert.equal(await Sandbox.deleteSnapshot(snapshot.snapshotId, connection), true)
assert.equal(await Sandbox.deleteSnapshot(snapshot.snapshotId, connection), false)

await sandbox.setTimeout(123_000)
assert.equal(await sandbox.kill(), true)
assert.equal(await Sandbox.kill('missing-sandbox', connection), false)
await assert.rejects(
  Sandbox.connect('missing-sandbox', connection),
  SandboxNotFoundError
)

const interpreter = await CodeInterpreter.create(connection)
assert.equal(interpreter.sandboxId, 'fixture-interpreter')
assert.equal(await interpreter.kill(), true)
assert.equal(await Volume.destroy(volume.volumeId, connection), true)
