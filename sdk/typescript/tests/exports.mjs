import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'

import SandboxDefault, {
  A3SRemoteConnection,
  DEFAULT_IMAGE,
  Sandbox,
} from '../dist/index.js'
import { Sandbox as CodeInterpreter } from '../dist/code-interpreter.js'

class FakeRuntime {
  requests = []

  async request(request) {
    this.requests.push(request)
    switch (request.operation) {
      case 'sandbox_create':
        return {
          sandbox_id: 'sandbox-local-1',
          generation: 1,
          state: 'running',
        }
      case 'sandbox_inspect':
        return {
          sandbox_id: request.sandbox_id,
          generation: 2,
          state: 'paused',
        }
      case 'command_run':
        return {
          stdout_base64: Buffer.from('42\n').toString('base64'),
          stderr_base64: '',
          exit_code: 0,
          truncated: false,
        }
      case 'file_write':
        return { path: request.path, size: 5 }
      case 'file_read':
        return {
          path: request.path,
          data_base64: Buffer.from('hello').toString('base64'),
          size: 5,
        }
      case 'filesystem_stat':
        return {
          entry: {
            name: 'notes.txt',
            type: 'file',
            path: request.path,
            size: 5,
            mode: 420,
            permissions: '-rw-r--r--',
            owner: 'root',
            group: 'root',
            modified_seconds: 1,
            modified_nanos: 0,
            symlink_target: null,
          },
        }
      case 'filesystem_list':
        return { entries: [] }
      case 'sandbox_kill':
      case 'sandbox_pause':
      case 'sandbox_resume':
      case 'filesystem_make_dir':
      case 'filesystem_move':
      case 'filesystem_remove':
        return { ok: true }
      default:
        throw new Error(`unexpected operation: ${request.operation}`)
    }
  }
}

assert.equal(SandboxDefault, Sandbox)
assert.equal(DEFAULT_IMAGE, 'alpine:3.20')
assert.notEqual(CodeInterpreter, Sandbox)

const runtime = new FakeRuntime()
const sandbox = await Sandbox.create('python:3.12-alpine', {
  timeoutMs: 120_000,
  envs: { MODE: 'test' },
  metadata: { suite: 'sdk' },
  runtime,
})
assert.equal(sandbox.sandboxId, 'sandbox-local-1')

const result = await sandbox.commands.run("python -c 'print(6 * 7)'", {
  timeoutMs: 10_000,
  cwd: '/workspace',
  envs: { REQUEST: 'one' },
})
assert.equal(result.stdout, '42\n')
assert.equal(result.stderr, '')
assert.equal(result.exitCode, 0)

const write = await sandbox.files.write('/workspace/notes.txt', 'hello')
assert.equal(write.size, 5)
assert.equal(await sandbox.files.read('/workspace/notes.txt'), 'hello')
assert.equal(await sandbox.files.exists('/workspace/notes.txt'), true)
await sandbox.kill()

const [create, command, writeRequest, read, stat, kill] = runtime.requests
assert.equal(create.operation, 'sandbox_create')
assert.equal(create.image, 'python:3.12-alpine')
assert.equal(create.timeout_seconds, 120)
assert.deepEqual(create.env, { MODE: 'test' })
assert.deepEqual(create.labels, { suite: 'sdk' })
assert.deepEqual(command.argv, [
  '/bin/sh',
  '-lc',
  "python -c 'print(6 * 7)'",
])
assert.equal(command.generation, 1)
assert.equal(writeRequest.data_base64, Buffer.from('hello').toString('base64'))
assert.equal(read.path, '/workspace/notes.txt')
assert.equal(stat.operation, 'filesystem_stat')
assert.equal(kill.operation, 'sandbox_kill')

const connected = await Sandbox.connect('existing-local', { runtime })
assert.equal(connected.sandboxId, 'existing-local')
assert.equal(connected.generation, 2)
assert.equal(connected.state, 'paused')

const remote = A3SRemoteConnection.fromEnvironment({
  A3S_BOX_ENDPOINT: 'https://api.box.example.com',
  A3S_BOX_API_KEY: 'e2b_a1b2c3',
})
assert.equal(remote.domain, 'box.example.com')
assert.deepEqual(remote.officialSdkOptions(), {
  apiUrl: 'https://api.box.example.com',
  domain: 'box.example.com',
  apiKey: 'e2b_a1b2c3',
})
assert.throws(
  () => A3SRemoteConnection.fromEnvironment({}),
  /A3S_BOX_ENDPOINT is required/
)

const interpreterRuntime = new FakeRuntime()
const interpreter = await CodeInterpreter.create(undefined, {
  runtime: interpreterRuntime,
})
const interpreterResult = await interpreter.runCode('print(6 * 7)')
await interpreter.kill()
assert.equal(interpreterResult.stdout, '42\n')
assert.equal(interpreterRuntime.requests[0].image, 'python:3.12-alpine')
assert.deepEqual(interpreterRuntime.requests[1].argv, [
  'python',
  '-c',
  'print(6 * 7)',
])

const packageJson = JSON.parse(
  await readFile(new URL('../package.json', import.meta.url), 'utf8')
)
assert.deepEqual(packageJson.dependencies ?? {}, {})
