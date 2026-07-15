#!/usr/bin/env node
/** Exercise the unchanged official TypeScript clients against production. */

import assert from 'node:assert/strict'
import { Sandbox, SandboxNotFoundError } from 'e2b'
import { Sandbox as CodeInterpreter } from '@e2b/code-interpreter'

const [apiUrl, domain, template] = process.argv.slice(2)
const apiKey = process.env.E2B_API_KEY
if (!apiUrl || !domain || !template || !apiKey) {
  throw new Error('API URL, domain, template, and E2B_API_KEY are required')
}

const connection = { apiKey, apiUrl, domain }
const metadata = { client: 'typescript', suite: 'production-official' }
let sandbox
let interpreter

try {
  sandbox = await Sandbox.create(template, {
    ...connection,
    timeoutMs: 60_000,
    metadata,
    envs: { OFFICIAL_CLIENT: 'typescript' },
    secure: true,
    allowInternetAccess: false,
  })
  const connected = await Sandbox.connect(sandbox.sandboxId, {
    ...connection,
    timeoutMs: 45_000,
  })
  assert.equal(connected.sandboxId, sandbox.sandboxId)
  assert.equal(await sandbox.isRunning(), true)

  const paginator = Sandbox.list({
    ...connection,
    query: { metadata, state: ['running'] },
    limit: 20,
  })
  const listed = await paginator.nextItems()
  assert.ok(listed.some((item) => item.sandboxId === sandbox.sandboxId))

  await sandbox.setTimeout(30_000)
  assert.equal(await sandbox.kill(), true)
  assert.equal(await sandbox.isRunning(), false)

  const missingId = 'missing-production-typescript'
  assert.equal(await Sandbox.kill(missingId, connection), false)
  await assert.rejects(
    Sandbox.connect(missingId, connection),
    SandboxNotFoundError
  )

  interpreter = await CodeInterpreter.create({
    ...connection,
    timeoutMs: 60_000,
    metadata: { client: 'typescript-code-interpreter' },
  })
  assert.equal(await interpreter.isRunning(), true)
  assert.equal(await interpreter.kill(), true)
  assert.equal(await interpreter.isRunning(), false)
} finally {
  if (interpreter) {
    await Sandbox.kill(interpreter.sandboxId, connection)
  }
  if (sandbox) {
    await Sandbox.kill(sandbox.sandboxId, connection)
  }
}
