import assert from 'node:assert/strict'

import { Sandbox as OfficialSandbox } from 'e2b'
import { Sandbox as OfficialCodeInterpreter } from '@e2b/code-interpreter'
import { A3SConnectionConfig, Sandbox } from '../dist/index.js'
import { Sandbox as CodeInterpreter } from '../dist/code-interpreter.js'

assert.equal(Sandbox, OfficialSandbox)
assert.equal(CodeInterpreter, OfficialCodeInterpreter)

const connection = A3SConnectionConfig.fromEnvironment({
  E2B_API_URL: 'https://api.box.example.com',
  E2B_DOMAIN: 'box.example.com',
  E2B_API_KEY: 'e2b_a1b2c3',
})
assert.deepEqual(connection.typescriptOptions(), {
  apiUrl: 'https://api.box.example.com',
  domain: 'box.example.com',
  apiKey: 'e2b_a1b2c3',
})
