import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'

import SandboxDefault, {
  A3SBoxClient,
  A3SLocalRuntime,
  A3SRemoteConnection,
  DEFAULT_IMAGE,
  RegistryCredentials,
  Sandbox,
  SignaturePolicy,
  SUPPORTED_BRIDGE_OPERATIONS,
} from '../dist/index.js'
import { Sandbox as CodeInterpreter } from '../dist/code-interpreter.js'

class FakeRuntime {
  requests = []

  async request(request) {
    this.requests.push(request)
    switch (request.operation) {
      case 'image_build':
        return {
          reference: request.tag ?? 'local/build:latest',
          digest: 'sha256:build',
          size_bytes: 8192,
          layer_count: 3,
        }
      case 'image_pull':
        return imageResponse(request.reference)
      case 'image_get':
        return { image: imageResponse(request.reference) }
      case 'image_list':
        return { images: [imageResponse('alpine:3.20')] }
      case 'image_inspect':
        return { image: imageInspectResponse(request.reference) }
      case 'image_history':
        return {
          history: [
            {
              created: '2026-07-23T00:00:00Z',
              created_by: 'RUN npm test',
              size_bytes: 2048,
              comment: 'ci',
              empty_layer: false,
            },
          ],
        }
      case 'image_tag':
        return imageResponse(request.target)
      case 'image_push':
        return {
          reference: request.target,
          manifest_digest: 'sha256:manifest',
          config_url: 'https://registry.example/config',
          manifest_url: 'https://registry.example/manifest',
        }
      case 'image_evict':
        return { references: ['local/old:latest'] }
      case 'image_remove':
        return { reference: request.reference, removed: true }
      case 'volume_create':
        return volumeResponse(request.name)
      case 'volume_get':
        return { volume: volumeResponse(request.name) }
      case 'volume_list':
        return { volumes: [volumeResponse('ci-cache')] }
      case 'volume_remove':
        return volumeResponse(request.name)
      case 'volume_prune':
        return { names: ['old-cache'] }
      case 'network_create':
        return networkResponse(request.name, request.subnet)
      case 'network_get':
        return { network: networkResponse(request.name, '10.89.0.0/24') }
      case 'network_list':
        return { networks: [networkResponse('ci-net', '10.89.0.0/24')] }
      case 'network_remove':
        return networkResponse(request.name, '10.89.0.0/24')
      case 'network_prune':
        return { names: ['old-network'] }
      case 'sdk_capabilities':
        return {
          protocol_version: 1,
          operations: [
            'sdk_capabilities',
            'image_get',
            'image_inspect',
            'image_history',
            'image_tag',
            'image_push',
            'image_evict',
            'volume_prune',
            'network_prune',
          ],
        }
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
      case 'sandbox_snapshot_create':
        return {
          snapshot_id: request.snapshot_id,
          size_bytes: 4096,
          state: 'running',
          generation: request.generation,
        }
      case 'filesystem_snapshot_size':
        return {
          snapshot_id: request.snapshot_id,
          size_bytes: 4096,
        }
      case 'filesystem_snapshot_delete':
        return {
          snapshot_id: request.snapshot_id,
          deleted: true,
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

function imageResponse(reference) {
  return {
    reference,
    digest: 'sha256:image',
    size_bytes: 4096,
    pulled_at: '2026-07-23T00:00:00Z',
    last_used: '2026-07-23T00:00:00Z',
    path: '/tmp/image',
  }
}

function imageInspectResponse(reference) {
  return {
    ...imageResponse(reference),
    manifest_digest: 'sha256:manifest',
    layer_count: 2,
    entrypoint: ['/bin/sh'],
    command: ['-c', 'npm test'],
    env: { CI: 'true' },
    working_dir: '/workspace',
    user: '1000:1000',
    exposed_ports: ['8080/tcp'],
    volumes: ['/cache'],
    stop_signal: 'SIGTERM',
    health_check: {
      test: ['CMD', 'true'],
      interval: 1_000_000_000,
      timeout: 500_000_000,
      retries: 3,
      start_period: 0,
    },
    onbuild: [],
    labels: { purpose: 'ci' },
  }
}

function volumeResponse(name) {
  return {
    name,
    driver: 'local',
    mount_point: `/tmp/volumes/${name}`,
    labels: { purpose: 'ci' },
    in_use_by: [],
    in_use: false,
    size_limit: 4096,
    created_at: '2026-07-23T00:00:00Z',
  }
}

function networkResponse(name, subnet) {
  return {
    name,
    driver: 'bridge',
    subnet,
    gateway: '10.89.0.1',
    labels: { purpose: 'ci' },
    endpoints: [],
    endpoint_count: 0,
    isolation: 'none',
    created_at: '2026-07-23T00:00:00Z',
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
assert.equal(create.isolation, 'microvm')
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

const builderRuntime = new FakeRuntime()
const client = new A3SBoxClient(builderRuntime)
const builtImage = await client
  .image('./ci')
  .dockerfile('Dockerfile.ci')
  .tag('local/ci-base:latest')
  .buildArg('NODE_VERSION', '24')
  .platform('linux/arm64')
  .build()
const cacheVolume = await client
  .volume('ci-cache')
  .label('purpose', 'ci')
  .sizeLimit(4096)
  .create()
const ciNetwork = await client
  .network('ci-net')
  .subnet('10.89.55.0/24')
  .label('purpose', 'ci')
  .create()
const builderSandbox = await client
  .sandbox(builtImage.reference)
  .cpus(4)
  .memoryMb(4096)
  .mountNamed(cacheVolume.name, '/cache')
  .network(ciNetwork.name)
  .publishTcp(8080, 80)
  .workdir('/workspace')
  .autoRemove(false)
  .start()
const scriptResult = await builderSandbox
  .script('console.log(6 * 7)\n')
  .interpreter('node', '-')
  .env('CI', 'true')
  .cwd('/workspace')
  .run()
await builderSandbox.kill()
assert.equal(scriptResult.stdout, '42\n')
assert.equal(builderRuntime.requests[0].operation, 'image_build')
assert.equal(builderRuntime.requests[0].dockerfile, 'Dockerfile.ci')
assert.deepEqual(builderRuntime.requests[0].platforms, ['linux/arm64'])
assert.deepEqual(builderRuntime.requests[3].mounts, [
  {
    kind: 'named',
    name: 'ci-cache',
    target: '/cache',
    read_only: false,
  },
])
assert.deepEqual(builderRuntime.requests[3].network, {
  mode: 'bridge',
  name: 'ci-net',
})
assert.deepEqual(builderRuntime.requests[3].ports, [
  { host_port: 8080, guest_port: 80 },
])
assert.equal(builderRuntime.requests[3].auto_remove, false)
assert.deepEqual(builderRuntime.requests[4].argv, ['node', '-'])
assert.equal(
  Buffer.from(builderRuntime.requests[4].stdin_base64, 'base64').toString(),
  'console.log(6 * 7)\n'
)
await client.removeNetwork(ciNetwork.name)
await client.removeVolume(cacheVolume.name)
await client.removeImage(builtImage.reference)

const managementRuntime = new FakeRuntime()
const management = new A3SBoxClient(managementRuntime)
const credentials = new RegistryCredentials('builder', 'secret')
const signaturePolicy = SignaturePolicy.cosignKey('/keys/cosign.pub')
const pulled = await management.pullImage('registry.example/ci/base:latest', {
  credentials,
  signaturePolicy,
})
const cached = await management.getImage(pulled.reference)
const inspected = await management.inspectImage(pulled.reference)
const history = await management.imageHistory(pulled.reference)
const tagged = await management.tagImage(pulled.reference, 'local/ci:tested')
const pushed = await management.pushImage(
  tagged.reference,
  'registry.example/ci/app:tested',
  { credentials, registryProtocol: 'http' }
)
assert.deepEqual(await management.evictImages(), ['local/old:latest'])
assert.deepEqual(await management.pruneVolumes(), ['old-cache'])
assert.deepEqual(await management.pruneNetworks(), ['old-network'])
const capabilities = await management.capabilities()
assert.deepEqual(cached, pulled)
assert.equal(inspected.manifestDigest, 'sha256:manifest')
assert.equal(inspected.healthCheck.retries, 3)
assert.equal(history[0].createdBy, 'RUN npm test')
assert.equal(pushed.manifestDigest, 'sha256:manifest')
assert.ok(capabilities.operations.includes('image_push'))
assert.deepEqual(managementRuntime.requests[0].credentials, {
  username: 'builder',
  password: 'secret',
})
assert.deepEqual(managementRuntime.requests[0].signature_policy, {
  mode: 'cosign_key',
  public_key: '/keys/cosign.pub',
})
assert.equal(managementRuntime.requests[5].registry_protocol, 'http')

const sandboxIsolationRuntime = new FakeRuntime()
const sharedKernelSandbox = await Sandbox.create(undefined, {
  isolation: 'sandbox',
  runtime: sandboxIsolationRuntime,
})
await sharedKernelSandbox.kill()
assert.equal(sandboxIsolationRuntime.requests[0].isolation, 'sandbox')

const snapshotRuntime = new FakeRuntime()
const snapshotSandbox = await Sandbox.create(undefined, {
  isolation: 'sandbox',
  filesystemSnapshotId: 'ci-base-source',
  runtime: snapshotRuntime,
})
const snapshot = await snapshotSandbox.createFilesystemSnapshot('ci-base-captured')
assert.equal(snapshot.snapshotId, 'ci-base-captured')
assert.equal(snapshot.sizeBytes, 4096)
assert.equal(
  await Sandbox.filesystemSnapshotSize(snapshot.snapshotId, {
    runtime: snapshotRuntime,
  }),
  4096
)
assert.equal(
  await Sandbox.deleteFilesystemSnapshot(snapshot.snapshotId, {
    runtime: snapshotRuntime,
  }),
  true
)
await snapshotSandbox.kill()
assert.deepEqual(
  snapshotRuntime.requests.map((request) => request.operation),
  [
    'sandbox_create',
    'sandbox_snapshot_create',
    'filesystem_snapshot_size',
    'filesystem_snapshot_delete',
    'sandbox_kill',
  ]
)
assert.equal(
  snapshotRuntime.requests[0].filesystem_snapshot_id,
  'ci-base-source'
)

const savedEnvironment = {
  E2B_API_KEY: process.env.E2B_API_KEY,
  A3S_BOX_API_KEY: process.env.A3S_BOX_API_KEY,
  A3S_BOX_ENDPOINT: process.env.A3S_BOX_ENDPOINT,
  A3S_BOX_BINARY: process.env.A3S_BOX_BINARY,
}
process.env.E2B_API_KEY = 'must-not-be-read'
process.env.A3S_BOX_API_KEY = 'must-not-be-read'
process.env.A3S_BOX_ENDPOINT = 'https://must-not-be-read.invalid'
delete process.env.A3S_BOX_BINARY
assert.equal(new A3SLocalRuntime().binaryPath, 'a3s-box')
for (const [key, value] of Object.entries(savedEnvironment)) {
  if (value === undefined) delete process.env[key]
  else process.env[key] = value
}

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
const operationInventory = JSON.parse(
  await readFile(
    new URL('../../bridge-operations.json', import.meta.url),
    'utf8'
  )
)
assert.deepEqual(SUPPORTED_BRIDGE_OPERATIONS, operationInventory)
