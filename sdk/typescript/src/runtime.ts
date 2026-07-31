import { spawn } from 'node:child_process'

import { A3SBoxError, A3SBoxNotInstalledError } from './errors.js'

export const BRIDGE_PROTOCOL_VERSION = 3
export const SUPPORTED_BRIDGE_OPERATIONS = [
  'sdk_capabilities',
  'runtime_diagnostics',
  'runtime_disk_usage',
  'image_build',
  'image_pull',
  'image_get',
  'image_list',
  'image_inspect',
  'image_history',
  'image_tag',
  'image_push',
  'image_remove',
  'image_evict',
  'volume_create',
  'volume_get',
  'volume_list',
  'volume_remove',
  'volume_prune',
  'network_create',
  'network_get',
  'network_list',
  'network_remove',
  'network_prune',
  'sandbox_list',
  'sandbox_get',
  'sandbox_create',
  'sandbox_inspect',
  'sandbox_stop',
  'sandbox_restart',
  'sandbox_remove',
  'sandbox_kill',
  'sandbox_pause',
  'sandbox_resume',
  'sandbox_logs',
  'sandbox_stats',
  'sandbox_snapshot_create',
  'filesystem_snapshot_list',
  'filesystem_snapshot_get',
  'filesystem_snapshot_size',
  'filesystem_snapshot_delete',
  'command_run',
  'file_write',
  'file_read',
  'filesystem_stat',
  'filesystem_list',
  'filesystem_make_dir',
  'filesystem_move',
  'filesystem_remove',
] as const

export type BridgeRequest = Readonly<Record<string, unknown>>
export type BridgeResult = Record<string, unknown>

export interface LocalRuntime {
  request(request: BridgeRequest): Promise<BridgeResult>
}

class CompatibleLocalRuntime implements LocalRuntime {
  private verification?: Promise<BridgeResult>

  constructor(private readonly runtime: LocalRuntime) {}

  async request(request: BridgeRequest): Promise<BridgeResult> {
    if (request.operation === 'sdk_capabilities') {
      return { ...(await this.verify()) }
    }
    await this.verify()
    return this.runtime.request(request)
  }

  private verify(): Promise<BridgeResult> {
    this.verification ??= this.runtime
      .request({ operation: 'sdk_capabilities' })
      .then(validateCapabilities)
    return this.verification
  }
}

export function compatibleRuntime(runtime: LocalRuntime): LocalRuntime {
  return runtime instanceof CompatibleLocalRuntime
    ? runtime
    : new CompatibleLocalRuntime(runtime)
}

function validateCapabilities(result: BridgeResult): BridgeResult {
  if (result.protocol_version !== BRIDGE_PROTOCOL_VERSION) {
    throw new A3SBoxError(
      `Unsupported local A3S Box capability protocol version ${String(
        result.protocol_version
      )}`,
      'bridge_protocol_error'
    )
  }
  if (
    !Array.isArray(result.operations) ||
    result.operations.some((operation) => typeof operation !== 'string')
  ) {
    throw new A3SBoxError(
      'Invalid local A3S Box capability inventory',
      'bridge_protocol_error'
    )
  }
  const operations = result.operations as string[]
  if (new Set(operations).size !== operations.length) {
    throw new A3SBoxError(
      'Invalid local A3S Box capability inventory: duplicate operations',
      'bridge_protocol_error'
    )
  }
  const available = new Set(operations)
  const missing = SUPPORTED_BRIDGE_OPERATIONS.filter(
    (operation) => !available.has(operation)
  ).sort()
  if (missing.length !== 0) {
    throw new A3SBoxError(
      `Installed A3S Box runtime is missing required operations: ${missing.join(
        ', '
      )}`,
      'unavailable'
    )
  }
  return {
    protocol_version: result.protocol_version,
    operations: [...operations],
  }
}

export interface A3SLocalRuntimeOptions {
  binaryPath?: string
  bridgeTimeoutMs?: number
}

/** Structured local transport to the bridge built into `a3s-box`. */
export class A3SLocalRuntime implements LocalRuntime {
  readonly binaryPath: string
  readonly bridgeTimeoutMs: number

  constructor(options: A3SLocalRuntimeOptions = {}) {
    this.binaryPath =
      options.binaryPath ?? process.env.A3S_BOX_BINARY ?? 'a3s-box'
    this.bridgeTimeoutMs = options.bridgeTimeoutMs ?? 600_000
    if (this.bridgeTimeoutMs <= 0) {
      throw new Error('bridgeTimeoutMs must be greater than zero')
    }
  }

  async request(request: BridgeRequest): Promise<BridgeResult> {
    const response = await invokeBridge(
      this.binaryPath,
      request,
      this.bridgeTimeoutMs
    )
    return decodeResponse(response.stdout, response.stderr, response.exitCode)
  }
}

interface ProcessResponse {
  stdout: string
  stderr: string
  exitCode: number
}

function invokeBridge(
  binary: string,
  request: BridgeRequest,
  timeoutMs: number
): Promise<ProcessResponse> {
  return new Promise((resolve, reject) => {
    const child = spawn(binary, ['sdk-bridge'], {
      stdio: ['pipe', 'pipe', 'pipe'],
    })
    const stdout: Buffer[] = []
    const stderr: Buffer[] = []
    let settled = false

    const timer = setTimeout(() => {
      if (settled) return
      settled = true
      child.kill()
      reject(
        new A3SBoxError(
          `Local A3S Box bridge timed out after ${timeoutMs} ms`,
          'bridge_timeout'
        )
      )
    }, timeoutMs)

    child.stdout.on('data', (chunk: Buffer) => stdout.push(chunk))
    child.stderr.on('data', (chunk: Buffer) => stderr.push(chunk))
    child.on('error', (error: NodeJS.ErrnoException) => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      if (error.code === 'ENOENT') {
        reject(new A3SBoxNotInstalledError(binary))
      } else {
        reject(new A3SBoxError(error.message, 'bridge_start_error'))
      }
    })
    child.on('close', (code) => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      resolve({
        stdout: Buffer.concat(stdout).toString('utf8'),
        stderr: Buffer.concat(stderr).toString('utf8'),
        exitCode: code ?? 1,
      })
    })

    child.stdin.end(JSON.stringify(request))
  })
}

function decodeResponse(
  stdout: string,
  stderr: string,
  exitCode: number
): BridgeResult {
  let envelope: unknown
  try {
    envelope = JSON.parse(stdout)
  } catch {
    const detail = stderr.trim() || stdout.trim() || `exit status ${exitCode}`
    throw new A3SBoxError(
      `Invalid response from the local A3S Box bridge: ${detail}`,
      'bridge_protocol_error'
    )
  }
  if (!isRecord(envelope)) {
    throw new A3SBoxError(
      'Invalid response from the local A3S Box bridge: expected an object',
      'bridge_protocol_error'
    )
  }
  if (envelope.protocol_version !== BRIDGE_PROTOCOL_VERSION) {
    throw new A3SBoxError(
      'Unsupported local A3S Box bridge protocol version',
      'bridge_protocol_error'
    )
  }
  if (envelope.ok !== true) {
    const error = envelope.error
    const code =
      isRecord(error) && typeof error.code === 'string'
        ? error.code
        : 'runtime_error'
    const message =
      isRecord(error) && typeof error.message === 'string'
        ? error.message
        : 'Local A3S Box request failed'
    throw new A3SBoxError(message, code)
  }
  if (!isRecord(envelope.result)) {
    throw new A3SBoxError(
      'Invalid response from the local A3S Box bridge: missing result',
      'bridge_protocol_error'
    )
  }
  return envelope.result
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}
