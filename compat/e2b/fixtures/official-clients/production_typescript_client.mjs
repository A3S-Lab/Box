#!/usr/bin/env node
/** Exercise the unchanged official TypeScript clients against production. */

import assert from 'node:assert/strict'

const nativeSdk = process.env.A3S_BOX_NATIVE_SDK === '1'
const baseSdk = await import(nativeSdk ? '@a3s-lab/box' : 'e2b')
const codeInterpreterSdk = await import(
  nativeSdk ? '@a3s-lab/box/code-interpreter' : '@e2b/code-interpreter'
)
const { Sandbox, SandboxNotFoundError } = baseSdk
const { Sandbox: CodeInterpreter } = codeInterpreterSdk

const [apiUrl, domain, template] = process.argv.slice(2)
const apiKey = process.env.E2B_API_KEY
if (!apiUrl || !domain || !template || !apiKey) {
  throw new Error('API URL, domain, template, and E2B_API_KEY are required')
}

const connection = nativeSdk
  ? new baseSdk.A3SConnectionConfig({ apiKey, apiUrl, domain }).typescriptOptions()
  : { apiKey, apiUrl, domain }
const metadata = { client: 'typescript', suite: 'production-official' }
let sandbox
let interpreter

async function exerciseDataPlane(sandbox, label) {
  const root = `a3s-runtime-${label}`
  const original = `${root}/nested/original.txt`
  const renamed = `${root}/nested/renamed.txt`
  const content = `${label}-filesystem`

  await sandbox.files.remove(root)
  assert.equal(await sandbox.files.makeDir(`${root}/nested`), true)
  const written = await sandbox.files.write(original, content)
  assert.equal(written.path, `/home/user/${original}`)
  assert.equal(await sandbox.files.read(original), content)
  const info = await sandbox.files.getInfo(original)
  assert.equal(info.name, 'original.txt')
  assert.equal(info.path, `/home/user/${original}`)
  const entries = await sandbox.files.list(root, { depth: 2 })
  assert.ok(entries.some((entry) => entry.path === `/home/user/${original}`))
  const moved = await sandbox.files.rename(original, renamed)
  assert.equal(moved.path, `/home/user/${renamed}`)
  assert.equal(await sandbox.files.exists(original), false)
  assert.equal(await sandbox.files.exists(renamed), true)
  await sandbox.files.remove(root)
  assert.equal(await sandbox.files.exists(root), false)

  const payload = `${label}-stdin`
  const command = await sandbox.commands.run('cat', {
    background: true,
    stdin: true,
    timeoutMs: 20_000,
  })
  const processes = await sandbox.commands.list()
  assert.ok(processes.some((process) => process.pid === command.pid))
  await command.sendStdin(payload)
  await command.closeStdin()
  const result = await command.wait()
  assert.equal(result.exitCode, 0)
  assert.equal(result.stdout, payload)
  assert.equal(result.stderr, '')

  let terminalOutput = ''
  const decoder = new TextDecoder()
  const terminal = await sandbox.pty.create({
    cols: 80,
    rows: 24,
    onData: (data) => {
      terminalOutput += decoder.decode(data)
    },
    timeoutMs: 20_000,
  })
  await sandbox.pty.resize(terminal.pid, { cols: 100, rows: 30 })
  await sandbox.pty.sendInput(
    terminal.pid,
    new TextEncoder().encode(`printf '${label}-pty:'; stty size; exit\n`)
  )
  await terminal.wait()
  assert.equal(terminal.exitCode, 0)
  assert.ok(terminalOutput.includes(`${label}-pty:`))
  assert.ok(terminalOutput.includes('30 100'))
}

async function exerciseInterpreter(interpreter, label) {
  const execution = await interpreter.runCode(`print('${label}-code')\n6 * 7`)
  assert.equal(execution.text, '42')
  assert.ok(execution.logs.stdout.some((line) => line.includes(`${label}-code`)))

  const context = await interpreter.createCodeContext({ language: 'python' })
  let contexts = await interpreter.listCodeContexts()
  assert.ok(contexts.some((item) => item.id === context.id))
  const contextual = await interpreter.runCode('value = 41\nvalue + 1', {
    context,
  })
  assert.equal(contextual.text, '42')
  await interpreter.restartCodeContext(context.id)
  const restarted = await interpreter.runCode('value', { context })
  assert.equal(restarted.error?.name, 'NameError')
  await interpreter.removeCodeContext(context.id)
  contexts = await interpreter.listCodeContexts()
  assert.equal(contexts.some((item) => item.id === context.id), false)
}

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
  const command = await sandbox.commands.run(
    'printf \'typescript:%s\' "$OFFICIAL_CLIENT"'
  )
  assert.equal(command.stdout, 'typescript:typescript')
  assert.equal(command.stderr, '')
  await exerciseDataPlane(sandbox, 'typescript')

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
  await exerciseInterpreter(interpreter, 'typescript')
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
