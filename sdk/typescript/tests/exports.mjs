import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'

import SandboxDefault, {
  A3SBoxClient,
  A3SBoxError,
  A3SBoxNotInstalledError,
  A3SLocalRuntime,
  BRIDGE_PROTOCOL_VERSION,
  DEFAULT_IMAGE,
  RegistryCredentials,
  Sandbox,
  SignaturePolicy,
  SUPPORTED_BRIDGE_OPERATIONS,
} from '../dist/index.js'
import { Sandbox as CodeInterpreter } from '../dist/code-interpreter.js'

class FakeRuntime {
  requests = []
  isolations = new Map()

  async request(request) {
    if (request.operation !== 'sdk_capabilities') this.requests.push(request)
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
      case 'runtime_diagnostics':
        return {
          core_version: '3.2.0',
          runtime_version: '3.2.0',
          sdk_version: '3.2.0',
          home: '/tmp/a3s',
          virtualization: {
            available: true,
            backend: 'hvf',
            details: 'Apple Hypervisor.framework',
          },
        }
      case 'runtime_disk_usage':
        return {
          home: '/tmp/a3s',
          total_bytes: 28,
          boxes_bytes: 1,
          images_bytes: 2,
          volumes_bytes: 3,
          snapshots_bytes: 4,
          state_bytes: 8,
          other_bytes: 10,
        }
      case 'sdk_capabilities':
        return {
          protocol_version: BRIDGE_PROTOCOL_VERSION,
          operations: [...SUPPORTED_BRIDGE_OPERATIONS],
        }
      case 'sandbox_list':
        return { sandboxes: [sandboxSummaryResponse('sandbox-local-1')] }
      case 'sandbox_get':
        return { sandbox: sandboxSummaryResponse(request.query) }
      case 'sandbox_create': {
        const isolation = request.isolation ?? 'microvm'
        this.isolations.set('sandbox-local-1', isolation)
        return {
          sandbox_id: 'sandbox-local-1',
          generation: 1,
          state: 'running',
          isolation,
        }
      }
      case 'sandbox_inspect': {
        const isolation =
          this.isolations.get(request.sandbox_id) ?? 'sandbox'
        this.isolations.set(request.sandbox_id, isolation)
        return {
          sandbox_id: request.sandbox_id,
          generation: 2,
          state: 'paused',
          isolation,
        }
      }
      case 'sandbox_stop':
        return {
          sandbox_id: request.sandbox_id,
          generation: request.generation,
          state: 'stopped',
          isolation: this.isolations.get(request.sandbox_id) ?? 'microvm',
        }
      case 'sandbox_restart':
        return {
          sandbox_id: request.sandbox_id,
          generation: request.generation + 1,
          state: 'running',
          isolation: this.isolations.get(request.sandbox_id) ?? 'microvm',
        }
      case 'sandbox_remove':
        return {
          sandbox_id: request.sandbox_id,
          generation: request.generation,
          state: 'removed',
          isolation: this.isolations.get(request.sandbox_id) ?? 'microvm',
        }
      case 'sandbox_kill':
        return {
          sandbox_id: request.sandbox_id,
          generation: request.generation,
          state: 'stopped',
          isolation: this.isolations.get(request.sandbox_id) ?? 'microvm',
        }
      case 'sandbox_pause':
        return {
          sandbox_id: request.sandbox_id,
          generation: request.generation,
          state: 'paused',
          isolation: this.isolations.get(request.sandbox_id) ?? 'microvm',
        }
      case 'sandbox_resume':
        return {
          sandbox_id: request.sandbox_id,
          generation: request.generation,
          state: 'running',
          isolation: this.isolations.get(request.sandbox_id) ?? 'microvm',
        }
      case 'sandbox_logs':
        return {
          logs: [
            {
              stream: 'stdout',
              log: 'sdk-log\n',
              time: '2026-07-23T00:00:00Z',
            },
          ],
        }
      case 'sandbox_stats':
        return { stats: sandboxStatsResponse(request.sandbox_id) }
      case 'sandbox_snapshot_create':
        return {
          snapshot_id: request.snapshot_id,
          size_bytes: 4096,
          state: 'running',
          generation: request.generation,
        }
      case 'filesystem_snapshot_list':
        return { snapshots: [filesystemSnapshotResponse('ci-base')] }
      case 'filesystem_snapshot_get':
        return {
          snapshot: filesystemSnapshotResponse(request.snapshot_id),
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
      case 'filesystem_make_dir':
      case 'filesystem_move':
      case 'filesystem_remove':
        return { ok: true }
      default:
        throw new Error(`unexpected operation: ${request.operation}`)
    }
  }
}

class CapabilityRuntime {
  requests = []

  constructor({
    protocolVersion = BRIDGE_PROTOCOL_VERSION,
    operations = SUPPORTED_BRIDGE_OPERATIONS,
  } = {}) {
    this.protocolVersion = protocolVersion
    this.operations = operations
  }

  async request(request) {
    this.requests.push(request)
    if (request.operation === 'sdk_capabilities') {
      await Promise.resolve()
      return {
        protocol_version: this.protocolVersion,
        operations: [...this.operations],
      }
    }
    if (request.operation === 'image_list') return { images: [] }
    if (request.operation === 'volume_list') return { volumes: [] }
    throw new Error(`unexpected operation: ${request.operation}`)
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

function sandboxSummaryResponse(sandboxId) {
  return {
    id: sandboxId,
    short_id: 'sandboxlocal',
    name: 'ci-box',
    image: 'alpine:3.20',
    isolation: 'microvm',
    status: 'running',
    status_summary: 'running',
    active: true,
    pid: 1234,
    cpus: 2,
    memory_mb: 512,
    ports: ['8080:80'],
    command: ['sh'],
    health: 'none',
    labels: { purpose: 'ci' },
    created_at: '2026-07-23T00:00:00Z',
    started_at: '2026-07-23T00:00:01Z',
    network_name: 'ci-net',
    volume_names: ['ci-cache'],
  }
}

function sandboxStatsResponse(sandboxId) {
  return {
    id: sandboxId,
    short_id: 'sandboxlocal',
    name: 'ci-box',
    status: 'running',
    pid: 1234,
    cpus: 2,
    cpu_percent: 1.5,
    cpu_percent_scaled: 3,
    memory_bytes: 1024,
    memory_limit_bytes: 2048,
    memory_percent: 50,
    network_rx_bytes: 10,
    network_tx_bytes: 20,
    block_read_bytes: 30,
    block_write_bytes: 40,
  }
}

function filesystemSnapshotResponse(snapshotId) {
  return {
    id: snapshotId,
    name: 'CI base',
    source_box_id: 'sandbox-local-1',
    image: 'alpine:3.20',
    vcpus: 2,
    memory_mb: 512,
    volumes: ['/cache'],
    command: ['sh'],
    port_map: ['8080:80'],
    labels: { purpose: 'ci' },
    network_mode: 'tsi',
    size_bytes: 4096,
    created_at: '2026-07-23T00:00:00Z',
    description: 'Warm CI base',
  }
}

assert.equal(SandboxDefault, Sandbox)
assert.equal(DEFAULT_IMAGE, 'alpine:3.20')
assert.notEqual(CodeInterpreter, Sandbox)
assert.equal(
  new A3SBoxNotInstalledError('/missing/a3s-box').code,
  'binary_not_found'
)

const missingOperationRuntime = new CapabilityRuntime({
  operations: SUPPORTED_BRIDGE_OPERATIONS.filter(
    (operation) => operation !== 'image_list'
  ),
})
await assert.rejects(
  new A3SBoxClient(missingOperationRuntime).listImages(),
  (error) =>
    error instanceof A3SBoxError &&
    error.code === 'unavailable' &&
    error.message.includes('image_list')
)
assert.deepEqual(
  missingOperationRuntime.requests.map((request) => request.operation),
  ['sdk_capabilities']
)

const mismatchedRuntime = new CapabilityRuntime({
  protocolVersion: BRIDGE_PROTOCOL_VERSION + 1,
})
await assert.rejects(
  new A3SBoxClient(mismatchedRuntime).listImages(),
  (error) =>
    error instanceof A3SBoxError && error.code === 'bridge_protocol_error'
)
assert.deepEqual(
  mismatchedRuntime.requests.map((request) => request.operation),
  ['sdk_capabilities']
)

const concurrentCapabilityRuntime = new CapabilityRuntime()
const concurrentClient = new A3SBoxClient(concurrentCapabilityRuntime)
await Promise.all([
  concurrentClient.listImages(),
  concurrentClient.listVolumes(),
])
assert.equal(
  concurrentCapabilityRuntime.requests.filter(
    (request) => request.operation === 'sdk_capabilities'
  ).length,
  1
)
assert.deepEqual(
  concurrentCapabilityRuntime.requests
    .map((request) => request.operation)
    .filter((operation) => operation !== 'sdk_capabilities')
    .sort(),
  ['image_list', 'volume_list']
)

const coverageRuntime = new FakeRuntime()
const coverageClient = new A3SBoxClient(coverageRuntime)
await coverageClient.runtimeDiagnostics()
await coverageClient.runtimeDiskUsage()
await coverageClient.image('.').tag('local/test:latest').build()
await coverageClient.pullImage('alpine:3.20')
await coverageClient.getImage('alpine:3.20')
await coverageClient.listImages()
await coverageClient.inspectImage('alpine:3.20')
await coverageClient.imageHistory('alpine:3.20')
await coverageClient.tagImage('alpine:3.20', 'local/alpine:latest')
await coverageClient.pushImage(
  'local/alpine:latest',
  'registry/alpine:latest'
)
await coverageClient.removeImage('local/alpine:latest')
await coverageClient.evictImages()
await coverageClient.volume('cache').create()
await coverageClient.getVolume('cache')
await coverageClient.listVolumes()
await coverageClient.removeVolume('cache', { force: true })
await coverageClient.pruneVolumes()
await coverageClient.network('ci-net').create()
await coverageClient.getNetwork('ci-net')
await coverageClient.listNetworks()
await coverageClient.removeNetwork('ci-net')
await coverageClient.pruneNetworks()
await coverageClient.listSandboxes()
await coverageClient.getSandbox('sandbox-local-1')

const coverageSandbox = await coverageClient.sandbox().start()
await coverageSandbox.stop()
await coverageSandbox.restart({ operationId: 'coverage-restart' })
await coverageSandbox.pause()
await coverageSandbox.resume()
await coverageSandbox.logs({ tail: 10 })
await coverageSandbox.stats()
await coverageSandbox.createFilesystemSnapshot('snap-1')
await coverageSandbox.commands.run(['true'])
await coverageSandbox.files.write('/tmp/value', 'value')
await coverageSandbox.files.read('/tmp/value')
await coverageSandbox.files.stat('/tmp/value')
await coverageSandbox.files.list('/tmp', { depth: 1 })
await coverageSandbox.files.makeDir('/tmp/dir')
await coverageSandbox.files.rename('/tmp/dir', '/tmp/moved')
await coverageSandbox.files.remove('/tmp/moved')
await coverageSandbox.kill()

const removableSandbox = await Sandbox.connect('sandbox-remove', {
  runtime: coverageRuntime,
})
await removableSandbox.remove()
await coverageClient.listFilesystemSnapshots()
await coverageClient.getFilesystemSnapshot('snap-1')
await Sandbox.filesystemSnapshotSize('snap-1', { runtime: coverageRuntime })
await Sandbox.deleteFilesystemSnapshot('snap-1', {
  runtime: coverageRuntime,
})
await coverageClient.capabilities()

assert.deepEqual(
  [...new Set(coverageRuntime.requests.map((request) => request.operation))].sort(),
  SUPPORTED_BRIDGE_OPERATIONS.filter(
    (operation) => operation !== 'sdk_capabilities'
  ).sort()
)

class MalformedBase64Runtime extends FakeRuntime {
  async request(request) {
    if (request.operation === 'command_run') {
      return {
        stdout_base64: 'not%base64',
        stderr_base64: '',
        exit_code: 0,
        truncated: false,
      }
    }
    return super.request(request)
  }
}

const malformedSandbox = await Sandbox.create(undefined, {
  runtime: new MalformedBase64Runtime(),
})
await assert.rejects(
  malformedSandbox.commands.run(['true']),
  (error) =>
    error instanceof A3SBoxError && error.code === 'bridge_protocol_error'
)

class MalformedSandboxRuntime extends FakeRuntime {
  constructor(operation, result) {
    super()
    this.operation = operation
    this.result = result
  }

  async request(request) {
    if (request.operation === this.operation) return this.result
    return super.request(request)
  }
}

const invalidSandboxResults = [
  {
    sandbox_id: '',
    generation: 1,
    state: 'running',
    isolation: 'microvm',
  },
  {
    sandbox_id: 'sandbox-local-1',
    generation: 0,
    state: 'running',
    isolation: 'microvm',
  },
  {
    sandbox_id: 'sandbox-local-1',
    generation: 1.5,
    state: 'running',
    isolation: 'microvm',
  },
  {
    sandbox_id: 'sandbox-local-1',
    generation: 1,
    state: 'unknown',
    isolation: 'microvm',
  },
  {
    sandbox_id: 'sandbox-local-1',
    generation: 1,
    state: 'running',
    isolation: 'process',
  },
]
for (const invalidResult of invalidSandboxResults) {
  await assert.rejects(
    Sandbox.create(undefined, {
      runtime: new MalformedSandboxRuntime(
        'sandbox_create',
        invalidResult
      ),
    }),
    (error) =>
      error instanceof A3SBoxError && error.code === 'bridge_protocol_error'
  )
}

await assert.rejects(
  Sandbox.connect('sandbox-expected', {
    runtime: new MalformedSandboxRuntime('sandbox_inspect', {
      sandbox_id: 'sandbox-other',
      generation: 1,
      state: 'running',
      isolation: 'microvm',
    }),
  }),
  (error) =>
    error instanceof A3SBoxError && error.code === 'bridge_protocol_error'
)

const changedIsolationRuntime = new MalformedSandboxRuntime(
  'sandbox_stop',
  {
    sandbox_id: 'sandbox-local-1',
    generation: 1,
    state: 'stopped',
    isolation: 'sandbox',
  }
)
const changedIsolationSandbox = await Sandbox.create(undefined, {
  runtime: changedIsolationRuntime,
})
await assert.rejects(
  changedIsolationSandbox.stop(),
  (error) =>
    error instanceof A3SBoxError && error.code === 'bridge_protocol_error'
)
assert.equal(changedIsolationSandbox.generation, 1)
assert.equal(changedIsolationSandbox.state, 'running')

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

const lifecycleRuntime = new FakeRuntime()
const lifecycleSandbox = await Sandbox.create(undefined, {
  runtime: lifecycleRuntime,
})
await lifecycleSandbox.stop()
assert.equal(lifecycleSandbox.state, 'stopped')
await assert.rejects(
  lifecycleSandbox.restart({ operationId: ' ' }),
  /operationId cannot be empty/
)
await assert.rejects(lifecycleSandbox.logs({ tail: 0 }), /between 1 and 10000/)
await lifecycleSandbox.restart({
  operationId: 'typescript-restart-1',
  stopTimeoutSeconds: 7,
})
assert.equal(lifecycleSandbox.generation, 2)
assert.equal(lifecycleSandbox.state, 'running')
const lifecycleLogs = await lifecycleSandbox.logs({ tail: 1 })
const lifecycleStats = await lifecycleSandbox.stats()
await lifecycleSandbox.stop()
await lifecycleSandbox.remove()
await lifecycleSandbox.kill()
assert.equal(lifecycleSandbox.state, 'removed')
assert.equal(lifecycleLogs[0].message, 'sdk-log\n')
assert.equal(lifecycleStats.memoryPercent, 50)
assert.deepEqual(
  lifecycleRuntime.requests.map((request) => request.operation),
  [
    'sandbox_create',
    'sandbox_stop',
    'sandbox_restart',
    'sandbox_logs',
    'sandbox_stats',
    'sandbox_stop',
    'sandbox_remove',
  ]
)
assert.deepEqual(lifecycleRuntime.requests[2], {
  operation: 'sandbox_restart',
  sandbox_id: 'sandbox-local-1',
  generation: 1,
  operation_id: 'typescript-restart-1',
  stop_timeout_seconds: 7,
})
assert.equal(lifecycleRuntime.requests[3].generation, 2)
assert.equal(lifecycleRuntime.requests[3].tail, 1)
assert.equal(lifecycleRuntime.requests.at(-1).generation, 2)

const builderRuntime = new FakeRuntime()
const client = new A3SBoxClient(builderRuntime)
const builtImage = await client
  .image('./ci')
  .dockerfile('Dockerfile.ci')
  .tag('local/ci-base:latest')
  .buildArg('NODE_VERSION', '24')
  .quiet(false)
  .platform('linux/arm64')
  .target('test')
  .noCache()
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
  .timeout(90_000)
  .env('CI', 'true')
  .metadata('job', 'test')
  .name('typescript-test')
  .cpus(4)
  .memoryMb(4096)
  .isolation('sandbox')
  .filesystemSnapshot('base-snapshot')
  .workspace('/workspace')
  .entrypoint('/usr/bin/env', 'sh')
  .command('-c', 'npm test && sleep 3600')
  .mountNamed(cacheVolume.name, '/cache', { readOnly: true })
  .mountBind('./src', '/workspace/src')
  .tmpfs('/scratch', { sizeBytes: 1024, readOnly: true })
  .network(ciNetwork.name)
  .publishTcp(8080, 80)
  .dnsServer('1.1.1.1')
  .hostAlias('registry.local', '10.89.55.2')
  .workdir('/workspace/src')
  .user('1000:1000')
  .hostname('typescript-ci')
  .readOnly()
  .persistent()
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
assert.equal(builderRuntime.requests[0].quiet, false)
assert.equal(builderRuntime.requests[0].target, 'test')
assert.equal(builderRuntime.requests[0].no_cache, true)
assert.equal(builderRuntime.requests[3].timeout_seconds, 90)
assert.deepEqual(builderRuntime.requests[3].env, { CI: 'true' })
assert.deepEqual(builderRuntime.requests[3].labels, { job: 'test' })
assert.equal(builderRuntime.requests[3].name, 'typescript-test')
assert.equal(builderRuntime.requests[3].cpus, 4)
assert.equal(builderRuntime.requests[3].memory_mb, 4096)
assert.equal(builderRuntime.requests[3].isolation, 'sandbox')
assert.equal(builderRuntime.requests[3].filesystem_snapshot_id, 'base-snapshot')
assert.equal(builderRuntime.requests[3].workspace, '/workspace')
assert.deepEqual(builderRuntime.requests[3].entrypoint, ['/usr/bin/env', 'sh'])
assert.deepEqual(builderRuntime.requests[3].command, [
  '-c',
  'npm test && sleep 3600',
])
assert.equal(builderRuntime.requests[3].workdir, '/workspace/src')
assert.equal(builderRuntime.requests[3].user, '1000:1000')
assert.equal(builderRuntime.requests[3].hostname, 'typescript-ci')
assert.deepEqual(builderRuntime.requests[3].mounts, [
  {
    kind: 'named',
    name: 'ci-cache',
    target: '/cache',
    read_only: true,
  },
  {
    kind: 'bind',
    source: './src',
    target: '/workspace/src',
    read_only: false,
  },
])
assert.deepEqual(builderRuntime.requests[3].tmpfs, [
  { target: '/scratch', size_bytes: 1024, read_only: true },
])
assert.deepEqual(builderRuntime.requests[3].network, {
  mode: 'bridge',
  name: 'ci-net',
})
assert.deepEqual(builderRuntime.requests[3].ports, [
  { host_port: 8080, guest_port: 80 },
])
assert.deepEqual(builderRuntime.requests[3].dns, ['1.1.1.1'])
assert.deepEqual(builderRuntime.requests[3].host_aliases, {
  'registry.local': '10.89.55.2',
})
assert.equal(builderRuntime.requests[3].read_only, true)
assert.equal(builderRuntime.requests[3].persistent, true)
assert.equal(builderRuntime.requests[3].auto_remove, false)
assert.deepEqual(builderRuntime.requests[4].argv, ['node', '-'])
assert.equal(
  Buffer.from(builderRuntime.requests[4].stdin_base64, 'base64').toString(),
  'console.log(6 * 7)\n'
)
await client.removeNetwork(ciNetwork.name)
await client.removeVolume(cacheVolume.name)
await client.removeImage(builtImage.reference)

const invalidInitialProcessRuntime = new FakeRuntime()
await assert.rejects(
  Sandbox.create('alpine:3.20', {
    command: [],
    runtime: invalidInitialProcessRuntime,
  }),
  /initial command cannot be empty/
)
await assert.rejects(
  Sandbox.create('alpine:3.20', {
    entrypoint: [' '],
    runtime: invalidInitialProcessRuntime,
  }),
  /initial entrypoint.*blank/
)
await assert.rejects(
  Sandbox.create('alpine:3.20', {
    command: 'echo split-into-characters',
    runtime: invalidInitialProcessRuntime,
  }),
  /initial command must be an array of strings/
)
assert.deepEqual(invalidInitialProcessRuntime.requests, [])

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
const managedSandboxes = await management.listSandboxes({ all: false })
const managedSandbox = await management.getSandbox('sandbox-local-1')
const diagnostics = await management.runtimeDiagnostics()
const diskUsage = await management.runtimeDiskUsage()
const filesystemSnapshots = await management.listFilesystemSnapshots()
const filesystemSnapshot = await management.getFilesystemSnapshot('ci-base')
assert.equal(managedSandboxes[0].name, 'ci-box')
assert.equal(managedSandbox.id, 'sandbox-local-1')
assert.equal(diagnostics.virtualization.backend, 'hvf')
assert.equal(diskUsage.totalBytes, 28)
assert.equal(filesystemSnapshots[0].sourceSandboxId, 'sandbox-local-1')
assert.equal(filesystemSnapshot.description, 'Warm CI base')
assert.deepEqual(managementRuntime.requests.at(-6), {
  operation: 'sandbox_list',
  all: false,
})
assert.deepEqual(
  managementRuntime.requests.slice(-6).map((request) => request.operation),
  [
    'sandbox_list',
    'sandbox_get',
    'runtime_diagnostics',
    'runtime_disk_usage',
    'filesystem_snapshot_list',
    'filesystem_snapshot_get',
  ]
)

const sandboxIsolationRuntime = new FakeRuntime()
const sharedKernelSandbox = await Sandbox.create(undefined, {
  isolation: 'sandbox',
  runtime: sandboxIsolationRuntime,
})
await sharedKernelSandbox.kill()
assert.equal(sharedKernelSandbox.isolation, 'sandbox')
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
  A3S_BOX_BINARY: process.env.A3S_BOX_BINARY,
}
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
const bridgeProtocol = JSON.parse(
  await readFile(
    new URL('../../bridge-protocol.json', import.meta.url),
    'utf8'
  )
)
assert.equal(BRIDGE_PROTOCOL_VERSION, bridgeProtocol.version)
