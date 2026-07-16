#!/usr/bin/env node
/** Exercise the unchanged official TypeScript clients against production. */

import assert from 'node:assert/strict'

const nativeSdk = process.env.A3S_BOX_NATIVE_SDK === '1'
const baseSdk = await import(nativeSdk ? '@a3s-lab/box' : 'e2b')
const codeInterpreterSdk = await import(
  nativeSdk ? '@a3s-lab/box/code-interpreter' : '@e2b/code-interpreter'
)
const { Sandbox, SandboxNotFoundError, Volume, VolumeError } = baseSdk
const { Sandbox: CodeInterpreter } = codeInterpreterSdk

const [apiUrl, domain, template] = process.argv.slice(2)
const apiKey = nativeSdk
  ? process.env.A3S_BOX_API_KEY
  : process.env.E2B_API_KEY
if (!apiUrl || !domain || !template || !apiKey) {
  throw new Error('API URL, domain, template, and API key are required')
}

const connection = nativeSdk
  ? baseSdk.A3SConnectionConfig.fromEnvironment(
      process.env
    ).typescriptOptions()
  : { apiKey, apiUrl, domain }
const volumeConnection = nativeSdk
  ? baseSdk.A3SConnectionConfig.fromEnvironment(process.env).volumeOptions()
  : { apiUrl }
const metadata = { client: 'typescript', suite: 'production-official' }
const clientLabel = `${nativeSdk ? 'a3s' : 'official'}-typescript`
const volumeName = `${clientLabel}-volume`
const trace = (stage) => console.log(`${clientLabel}:${stage}`)
let sandbox
let interpreter
let volume

async function exerciseDataPlane(sandbox, label) {
  const root = `a3s-runtime-${label}`
  const original = `${root}/nested/original.txt`
  const renamed = `${root}/nested/renamed.txt`
  const content = `${label}-filesystem`

  trace('filesystem.remove-initial')
  await sandbox.files.remove(root)
  trace('filesystem.make-dir')
  assert.equal(await sandbox.files.makeDir(`${root}/nested`), true)
  trace('filesystem.write')
  const written = await sandbox.files.write(original, content)
  assert.equal(written.path, `/home/user/${original}`)
  trace('filesystem.read')
  assert.equal(await sandbox.files.read(original), content)
  trace('filesystem.get-info')
  const info = await sandbox.files.getInfo(original)
  assert.equal(info.name, 'original.txt')
  assert.equal(info.path, `/home/user/${original}`)
  trace('filesystem.list')
  const entries = await sandbox.files.list(root, { depth: 2 })
  assert.ok(entries.some((entry) => entry.path === `/home/user/${original}`))
  trace('filesystem.rename')
  const moved = await sandbox.files.rename(original, renamed)
  assert.equal(moved.path, `/home/user/${renamed}`)
  trace('filesystem.exists-renamed')
  assert.equal(await sandbox.files.exists(original), false)
  assert.equal(await sandbox.files.exists(renamed), true)
  trace('filesystem.remove-final')
  await sandbox.files.remove(root)
  trace('filesystem.exists-final')
  assert.equal(await sandbox.files.exists(root), false)

  const payload = `${label}-stdin`
  trace('process.start-background')
  const command = await sandbox.commands.run('cat', {
    background: true,
    stdin: true,
    timeoutMs: 20_000,
  })
  trace('process.list')
  const processes = await sandbox.commands.list()
  assert.ok(processes.some((process) => process.pid === command.pid))
  trace('process.send-stdin')
  await command.sendStdin(payload)
  trace('process.close-stdin')
  await command.closeStdin()
  trace('process.wait')
  const result = await command.wait()
  assert.equal(result.exitCode, 0)
  assert.equal(result.stdout, payload)
  assert.equal(result.stderr, '')

  let terminalOutput = ''
  const decoder = new TextDecoder()
  trace('pty.create')
  const terminal = await sandbox.pty.create({
    cols: 80,
    rows: 24,
    onData: (data) => {
      terminalOutput += decoder.decode(data)
    },
    timeoutMs: 20_000,
  })
  trace('pty.resize')
  await sandbox.pty.resize(terminal.pid, { cols: 100, rows: 30 })
  trace('pty.send-input')
  await sandbox.pty.sendInput(
    terminal.pid,
    new TextEncoder().encode(`printf '${label}-pty:'; stty size; exit\n`)
  )
  trace('pty.wait')
  await terminal.wait()
  assert.equal(terminal.exitCode, 0)
  assert.ok(terminalOutput.includes(`${label}-pty:`))
  assert.ok(terminalOutput.includes('30 100'))
  trace('data-plane.complete')
}

async function exerciseInterpreter(interpreter, label) {
  trace('interpreter.run')
  const execution = await interpreter.runCode(`print('${label}-code')\n6 * 7`)
  assert.equal(execution.text, '42')
  assert.ok(execution.logs.stdout.some((line) => line.includes(`${label}-code`)))

  trace('interpreter.context-create')
  const context = await interpreter.createCodeContext({ language: 'python' })
  trace('interpreter.context-list')
  let contexts = await interpreter.listCodeContexts()
  assert.ok(contexts.some((item) => item.id === context.id))
  trace('interpreter.context-run')
  const contextual = await interpreter.runCode('value = 41\nvalue + 1', {
    context,
  })
  assert.equal(contextual.text, '42')
  trace('interpreter.context-restart')
  await interpreter.restartCodeContext(context.id)
  trace('interpreter.context-run-restarted')
  const restarted = await interpreter.runCode('value', { context })
  assert.equal(restarted.error?.name, 'NameError')
  trace('interpreter.context-remove')
  await interpreter.removeCodeContext(context.id)
  trace('interpreter.context-list-removed')
  contexts = await interpreter.listCodeContexts()
  assert.equal(contexts.some((item) => item.id === context.id), false)
  trace('interpreter.complete')
}

try {
  trace('volume.create')
  volume = await Volume.create(volumeName, connection)
  assert.equal(volume.name, volumeName)
  assert.ok(volume.volumeId)
  assert.ok(volume.token)
  trace('volume.connect')
  const connectedVolume = await Volume.connect(volume.volumeId, connection)
  assert.equal(connectedVolume.name, volumeName)
  trace('volume.list')
  assert.ok(
    (await Volume.list(connection)).some(
      (item) => item.volumeId === volume.volumeId
    )
  )
  trace('volume.make-dir')
  await volume.makeDir('/shared', {
    ...volumeConnection,
    uid: 1000,
    gid: 1000,
    mode: 0o777,
    force: true,
  })
  const apiVolumeContent = 'typescript-api-to-sandbox'
  trace('volume.api-write')
  await volume.writeFile('/shared/from-api.txt', apiVolumeContent, {
    ...volumeConnection,
    uid: 1000,
    gid: 1000,
    mode: 0o644,
  })

  trace('sandbox.create')
  sandbox = await Sandbox.create(template, {
    ...connection,
    timeoutMs: 60_000,
    metadata,
    envs: { OFFICIAL_CLIENT: 'typescript' },
    secure: true,
    allowInternetAccess: false,
    volumeMounts: { '/mnt/data': volume },
  })
  trace('sandbox.connect')
  const connected = await Sandbox.connect(sandbox.sandboxId, {
    ...connection,
    timeoutMs: 45_000,
  })
  assert.equal(connected.sandboxId, sandbox.sandboxId)
  trace('sandbox.health')
  assert.equal(await sandbox.isRunning(), true)
  trace('sandbox.metrics')
  const metrics = await sandbox.getMetrics()
  assert.ok(metrics.length > 0)
  for (const field of [
    'timestamp',
    'cpuCount',
    'cpuUsedPct',
    'memUsed',
    'memTotal',
    'diskUsed',
    'diskTotal',
  ]) {
    assert.notEqual(metrics[0][field], undefined)
  }
  trace('sandbox.metrics-past-range')
  assert.deepEqual(
    await sandbox.getMetrics({
      start: new Date('1970-01-01T00:00:00Z'),
      end: new Date('1970-01-02T00:00:00Z'),
    }),
    []
  )
  trace('process.foreground')
  const command = await sandbox.commands.run(
    'printf \'typescript:%s\' "$OFFICIAL_CLIENT"'
  )
  assert.equal(command.stdout, 'typescript:typescript')
  assert.equal(command.stderr, '')
  trace('process.foreground.complete')
  trace('volume.sandbox-read')
  const mounted = await sandbox.commands.run(
    'cat /mnt/data/shared/from-api.txt'
  )
  assert.equal(mounted.stdout, apiVolumeContent)
  assert.equal(mounted.stderr, '')
  trace('volume.sandbox-stat')
  const ownership = await sandbox.commands.run(
    "stat -c '%u:%g' /mnt/data/shared/from-api.txt"
  )
  assert.equal(ownership.stdout.trim(), '1000:1000')
  const identity = await sandbox.commands.run(
    'printf \'%s:%s\' "$(id -u)" "$(id -g)"'
  )
  const [sandboxUid, sandboxGid] = identity.stdout
    .split(':')
    .map((value) => Number.parseInt(value, 10))
  const sandboxVolumeContent = 'typescript-sandbox-to-api'
  trace('volume.sandbox-write')
  await sandbox.commands.run(
    `printf '%s' '${sandboxVolumeContent}' > /mnt/data/shared/from-sandbox.txt`
  )
  trace('volume.api-read')
  assert.equal(
    await volume.readFile('/shared/from-sandbox.txt', volumeConnection),
    sandboxVolumeContent
  )
  const sandboxEntry = await volume.getInfo(
    '/shared/from-sandbox.txt',
    volumeConnection
  )
  assert.equal(sandboxEntry.uid, sandboxUid)
  assert.equal(sandboxEntry.gid, sandboxGid)
  trace('volume.destroy-in-use')
  await assert.rejects(
    Volume.destroy(volume.volumeId, connection),
    (error) =>
      error instanceof VolumeError && /in use/i.test(error.message)
  )
  await exerciseDataPlane(sandbox, 'typescript')

  trace('sandbox.pause-process-start')
  const survivor = await sandbox.commands.run('cat', {
    background: true,
    stdin: true,
    timeoutMs: 20_000,
  })
  trace('sandbox.pause')
  assert.equal(await sandbox.pause({ keepMemory: true }), true)
  trace('sandbox.pause-idempotent')
  assert.equal(await sandbox.pause({ keepMemory: true }), false)
  trace('sandbox.list-paused')
  const pausedPaginator = Sandbox.list({
    ...connection,
    query: { metadata, state: ['paused'] },
    limit: 20,
  })
  const paused = await pausedPaginator.nextItems()
  assert.ok(paused.some((item) => item.sandboxId === sandbox.sandboxId))
  trace('sandbox.resume-connect')
  const resumed = await sandbox.connect({ timeoutMs: 45_000 })
  assert.equal(resumed.sandboxId, sandbox.sandboxId)
  trace('sandbox.pause-process-survived')
  await survivor.sendStdin('typescript-pause')
  await survivor.closeStdin()
  const survivorResult = await survivor.wait()
  assert.equal(survivorResult.exitCode, 0)
  assert.equal(survivorResult.stdout, 'typescript-pause')

  trace('sandbox.list')
  const paginator = Sandbox.list({
    ...connection,
    query: { metadata, state: ['running'] },
    limit: 20,
  })
  const listed = await paginator.nextItems()
  const listedSandbox = listed.find(
    (item) => item.sandboxId === sandbox.sandboxId
  )
  assert.ok(listedSandbox)
  assert.ok(
    listedSandbox.volumeMounts.some(
      (mount) => mount.name === volumeName && mount.path === '/mnt/data'
    )
  )

  trace('sandbox.set-timeout')
  await sandbox.setTimeout(30_000)
  trace('sandbox.kill')
  assert.equal(await sandbox.kill(), true)
  trace('sandbox.health-killed')
  assert.equal(await sandbox.isRunning(), false)
  trace('volume.destroy')
  assert.equal(await Volume.destroy(volume.volumeId, connection), true)
  volume = undefined

  const missingId = 'missing-production-typescript'
  trace('sandbox.kill-missing')
  assert.equal(await Sandbox.kill(missingId, connection), false)
  trace('sandbox.connect-missing')
  await assert.rejects(
    Sandbox.connect(missingId, connection),
    SandboxNotFoundError
  )

  trace('interpreter.create')
  interpreter = await CodeInterpreter.create({
    ...connection,
    timeoutMs: 60_000,
    metadata: { client: 'typescript-code-interpreter' },
  })
  trace('interpreter.health')
  assert.equal(await interpreter.isRunning(), true)
  await exerciseInterpreter(interpreter, 'typescript')
  trace('interpreter.kill')
  assert.equal(await interpreter.kill(), true)
  trace('interpreter.health-killed')
  assert.equal(await interpreter.isRunning(), false)
  trace('complete')
} finally {
  try {
    if (interpreter) {
      await Sandbox.kill(interpreter.sandboxId, connection)
    }
    if (sandbox) {
      await Sandbox.kill(sandbox.sandboxId, connection)
    }
  } finally {
    if (volume) {
      await Volume.destroy(volume.volumeId, connection)
    }
  }
}
