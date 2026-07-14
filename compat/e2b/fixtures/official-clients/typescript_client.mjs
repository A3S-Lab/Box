#!/usr/bin/env node
/** Exercise pinned official TypeScript lifecycle clients against the recorder. */

import assert from 'node:assert/strict'
import { Sandbox, SandboxNotFoundError } from 'e2b'
import { Sandbox as CodeInterpreter } from '@e2b/code-interpreter'

const apiUrl = process.argv[2]
if (!apiUrl) {
  throw new Error('API URL argument is required')
}

const connection = {
  apiKey: 'e2b_a1b2c3',
  apiUrl,
}
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
})
assert.equal(sandbox.sandboxId, 'fixture-sandbox')

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
assert.equal((await paginator.nextItems()).length, 1)

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
