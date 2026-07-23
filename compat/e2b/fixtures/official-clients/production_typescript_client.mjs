#!/usr/bin/env node
/** Exercise the unchanged official TypeScript clients against production. */

import assert from 'node:assert/strict'

const baseSdk = await import('e2b')
const codeInterpreterSdk = await import('@e2b/code-interpreter')
const { Sandbox, SandboxError, SandboxNotFoundError, Volume, VolumeError } =
  baseSdk
const { Sandbox: CodeInterpreter } = codeInterpreterSdk

const [apiUrl, domain, template] = process.argv.slice(2)
const apiKey = process.env.E2B_API_KEY
if (!apiUrl || !domain || !template || !apiKey) {
  throw new Error('API URL, domain, template, and API key are required')
}

const connection = { apiKey, apiUrl, domain }
const volumeConnection = { apiUrl }
const metadata = { client: 'typescript', suite: 'production-official' }
const clientLabel = 'official-typescript'
const volumeName = `${clientLabel}-volume`
const trace = (stage) => console.log(`${clientLabel}:${stage}`)
let sandbox
let restored
let interpreter
let volume
let snapshotId

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

  const snapshotContent = `${clientLabel}-snapshot`
  trace('snapshot.write-state')
  await sandbox.files.write('a3s-snapshot-state.txt', snapshotContent)
  const snapshotMetadata = (
    await sandbox.commands.run(
      "stat -c '%u:%g:%a' /home/user/a3s-snapshot-state.txt"
    )
  ).stdout.trim()
  trace('snapshot.create')
  const snapshot = await sandbox.createSnapshot({
    name: `${clientLabel}-state`,
  })
  snapshotId = snapshot.snapshotId
  assert.ok(snapshotId)
  assert.deepEqual(snapshot.names, [snapshotId])
  trace('snapshot.list')
  const snapshots = await sandbox.listSnapshots({ limit: 20 }).nextItems()
  assert.ok(snapshots.some((item) => item.snapshotId === snapshotId))
  trace('snapshot.source-running')
  assert.equal(await sandbox.isRunning(), true)

  const coldPauseContent = `${clientLabel}-cold-pause`
  trace('sandbox.cold-pause-write-state')
  await sandbox.files.write('a3s-cold-pause-state.txt', coldPauseContent)
  trace('sandbox.cold-pause-process-start')
  const coldProcess = await sandbox.commands.run('sleep 300', {
    background: true,
    timeoutMs: 310_000,
  })
  trace('sandbox.cold-pause')
  assert.equal(await sandbox.pause({ keepMemory: false }), true)
  trace('sandbox.cold-pause-connect')
  const coldResumed = await sandbox.connect({ timeoutMs: 60_000 })
  assert.equal(coldResumed.sandboxId, sandbox.sandboxId)
  trace('sandbox.cold-pause-read-state')
  assert.equal(
    await sandbox.files.read('a3s-cold-pause-state.txt'),
    coldPauseContent
  )
  trace('sandbox.cold-pause-process-gone')
  assert.equal(
    (await sandbox.commands.list()).some(
      (process) => process.pid === coldProcess.pid
    ),
    false
  )
  trace('sandbox.cold-pause-environment')
  const coldEnvironment = await sandbox.commands.run(
    'printf \'%s\' "$OFFICIAL_CLIENT"'
  )
  assert.equal(coldEnvironment.stdout, 'typescript')
  assert.equal(coldEnvironment.stderr, '')
  trace('sandbox.cold-pause-volume')
  const coldMounted = await sandbox.commands.run(
    'cat /mnt/data/shared/from-api.txt'
  )
  assert.equal(coldMounted.stdout, apiContent)
  assert.equal(coldMounted.stderr, '')

  trace('sandbox.set-timeout')
  await sandbox.setTimeout(30_000)
  trace('sandbox.kill')
  assert.equal(await sandbox.kill(), true)
  trace('sandbox.health-killed')
  assert.equal(await sandbox.isRunning(), false)

  trace('snapshot.restore-after-source-kill')
  restored = await Sandbox.create(snapshotId, {
    ...connection,
    timeoutMs: 60_000,
  })
  trace('snapshot.read-restored-state')
  assert.equal(
    await restored.files.read('a3s-snapshot-state.txt'),
    snapshotContent
  )
  const restoredMetadata = (
    await restored.commands.run(
      "stat -c '%u:%g:%a' /home/user/a3s-snapshot-state.txt"
    )
  ).stdout.trim()
  assert.equal(restoredMetadata, snapshotMetadata)
  await restored.commands.run(
    "printf '%s' '-writable' >> /home/user/a3s-snapshot-state.txt"
  )
  trace('snapshot.delete-in-use')
  await assert.rejects(
    Sandbox.deleteSnapshot(snapshotId, connection),
    (error) => error instanceof SandboxError && /^409:/.test(error.message)
  )
  trace('snapshot.restored-kill')
  assert.equal(await restored.kill(), true)
  restored = undefined
  trace('snapshot.delete')
  assert.equal(await Sandbox.deleteSnapshot(snapshotId, connection), true)
  trace('snapshot.delete-missing')
  assert.equal(await Sandbox.deleteSnapshot(snapshotId, connection), false)
  snapshotId = undefined

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
    if (restored) {
      await Sandbox.kill(restored.sandboxId, connection)
    }
    if (sandbox) {
      await Sandbox.kill(sandbox.sandboxId, connection)
    }
    if (snapshotId) {
      await Sandbox.deleteSnapshot(snapshotId, connection)
    }
  } finally {
    if (volume) {
      await Volume.destroy(volume.volumeId, connection)
    }
  }
}
