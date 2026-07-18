import assert from 'node:assert/strict'

import { Sandbox as OfficialSandbox } from 'e2b'
import { Sandbox as OfficialCodeInterpreter } from '@e2b/code-interpreter'
import { A3SConnectionConfig, Sandbox } from '../dist/index.js'
import { Sandbox as CodeInterpreter } from '../dist/code-interpreter.js'

assert.equal(Sandbox, OfficialSandbox)
assert.equal(CodeInterpreter, OfficialCodeInterpreter)

const connection = A3SConnectionConfig.fromEnvironment({
  A3S_BOX_ENDPOINT: 'https://api.box.example.com',
  A3S_BOX_API_KEY: 'a3s_a1b2c3',
  A3S_BOX_SANDBOX_URL: 'https://sandbox.box.example.com',
})
assert.deepEqual(connection.typescriptOptions(), {
  apiUrl: 'https://api.box.example.com',
  domain: 'box.example.com',
  validateApiKey: false,
  apiKey: 'a3s_a1b2c3',
  sandboxUrl: 'https://sandbox.box.example.com',
})

const selfHosted = A3SConnectionConfig.fromEnvironment({
  A3S_BOX_ENDPOINT: 'https://gateway.internal.example',
  A3S_BOX_DOMAIN: 'sandboxes.internal.example',
})
assert.equal(selfHosted.domain, 'sandboxes.internal.example')

assert.throws(
  () =>
    A3SConnectionConfig.fromEnvironment({
      E2B_API_URL: 'https://api.box.example.com',
      E2B_DOMAIN: 'box.example.com',
  }),
  /A3S_BOX_ENDPOINT is required/
)

assert.throws(
  () => new A3SConnectionConfig({ apiUrl: 'unix:///run/a3s-box.sock' }),
  /apiUrl must be an absolute HTTP or HTTPS URL/
)
