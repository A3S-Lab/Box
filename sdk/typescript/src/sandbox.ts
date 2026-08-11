import { createHash, randomUUID } from 'node:crypto'
import { open, rm, type FileHandle } from 'node:fs/promises'

import { A3SBoxError } from './errors.js'
import {
  asRecord,
  decodeBase64,
  entryInfo,
  executionEventBatch,
  executionProcessInventory,
  executionStats,
  filesystemSnapshotInfo,
  requiredBoolean,
  requiredGeneration,
  requiredNumber,
  requiredRecord,
  requiredSandboxState,
  requiredString,
  sandboxLogEntry,
  sandboxStats,
  unknownRecordArray,
} from './bridge-values.js'
import type { SandboxLogEntry, SandboxStats } from './client.js'
import {
  A3SLocalRuntime,
  compatibleRuntime,
  type BridgeResult,
  type LocalRuntime,
} from './runtime.js'

export const DEFAULT_IMAGE = 'alpine:3.20'
export const MAX_ARTIFACT_BYTES = 8 * 1024 * 1024
export const MAX_EXECUTION_EVENT_BATCH_ITEMS = 4_096
export const DEFAULT_EVENT_STREAM_BATCH_ITEMS = 256
export const DEFAULT_EVENT_STREAM_WAIT_TIMEOUT_MS = 1_000

export type Isolation = 'microvm' | 'sandbox'

export type SandboxNetwork =
  | { readonly mode: 'tsi' }
  | { readonly mode: 'none' }
  | { readonly mode: 'bridge'; readonly name: string }

export type VolumeMount =
  | {
      readonly kind: 'bind'
      readonly source: string
      readonly target: string
      readonly readOnly?: boolean
    }
  | {
      readonly kind: 'named'
      readonly name: string
      readonly target: string
      readonly readOnly?: boolean
    }

export interface TmpfsMount {
  target: string
  sizeBytes?: number
  readOnly?: boolean
}

export interface PortMapping {
  hostPort: number
  guestPort: number
}

export interface Script {
  source: string | Uint8Array
  interpreter?: readonly string[]
}

export interface SandboxCreateOptions {
  command?: readonly string[]
  entrypoint?: readonly string[]
  timeoutMs?: number
  envs?: Readonly<Record<string, string>>
  metadata?: Readonly<Record<string, string>>
  name?: string
  cpus?: number
  memoryMb?: number
  isolation?: Isolation
  filesystemSnapshotId?: string
  workspace?: string
  workdir?: string
  user?: string
  hostname?: string
  mounts?: readonly VolumeMount[]
  tmpfs?: readonly TmpfsMount[]
  network?: SandboxNetwork
  ports?: readonly PortMapping[]
  dns?: readonly string[]
  hostAliases?: Readonly<Record<string, string>>
  readOnly?: boolean
  persistent?: boolean
  autoRemove?: boolean
  runtime?: LocalRuntime
}

export interface SandboxConnectOptions {
  runtime?: LocalRuntime
}

export interface CommandRunOptions {
  timeoutMs?: number
  envs?: Readonly<Record<string, string>>
  cwd?: string
  user?: string
  stdin?: string | Uint8Array
}

export interface CommandResult {
  stdout: string
  stderr: string
  exitCode: number
  truncated: boolean
}

export interface WriteInfo {
  path: string
  size: number
}

export interface Artifact {
  path: string
  data: Uint8Array
  size: number
  sha256: string
  hostPath?: string
}

export interface ArtifactExportOptions {
  maxBytes?: number
  destination?: string
  user?: string
}

export interface FilesystemSnapshotInfo {
  snapshotId: string
  sizeBytes: number
  state: string
  generation: number
}

export interface EntryInfo {
  name: string
  type: 'file' | 'directory' | 'unspecified'
  path: string
  size: number
  mode: number
  permissions: string
  owner: string
  group: string
  modifiedSeconds: number
  modifiedNanos: number
  symlinkTarget?: string
}

export interface FilesystemReadOptions {
  format?: 'text' | 'bytes'
  user?: string
}

export interface ExecutionProcessInfo {
  processId: string
  pid?: number
  terminal: boolean
}

export interface ExecutionProcessInventory {
  executionId: string
  generation: number
  processes: ExecutionProcessInfo[]
}

export interface ExecutionCpuStats {
  usageNs: number
  userNs: number
  systemNs: number
  throttledNs: number
}

export interface ExecutionMemoryStats {
  usageBytes: number
  limitBytes?: number
  peakBytes?: number
}

export interface ExecutionStats {
  executionId: string
  generation: number
  timestampUnixNs: number
  cpu: ExecutionCpuStats
  memory: ExecutionMemoryStats
  processCount: number
  metrics: Readonly<Record<string, number>>
}

export type ExecutionEventKind =
  | 'container-creating'
  | 'container-created'
  | 'container-started'
  | 'container-stopped'
  | 'container-deleted'
  | 'container-paused'
  | 'container-resumed'
  | 'resources-updated'
  | 'process-created'
  | 'process-started'
  | 'process-exited'
  | 'output-dropped'
  | 'runtime-warning'

export interface ExecutionRuntimeEvent {
  sequence: number
  timestampUnixNs: number
  processId?: string
  kind: ExecutionEventKind
  attributes: Readonly<Record<string, string>>
}

export interface ExecutionEventBatch {
  executionId: string
  generation: number
  events: ExecutionRuntimeEvent[]
  nextSequence: number
}

export interface ExecutionEventsOptions {
  afterSequence?: number
  limit?: number
  waitTimeoutMs?: number
}

export interface ExecutionEventStreamOptions {
  afterSequence?: number
  batchSize?: number
  waitTimeoutMs?: number
  signal?: AbortSignal
}

export interface ExecutionResourceUpdate {
  memoryReservation?: number
  memorySwap?: number
  pidsLimit?: number
  cpuShares?: number
  cpuQuota?: number
  cpuPeriod?: number
  cpusetCpus?: string
}

export interface UpdateResourcesOptions {
  operationId?: string
}

export class Sandbox {
  readonly sandboxId: string
  readonly id: string
  generation: number
  state: string
  readonly isolation: Isolation
  readonly commands: Commands
  readonly files: Filesystem
  private readonly runtime: LocalRuntime

  protected constructor(
    sandboxId: string,
    generation: number,
    state: string,
    isolation: Isolation,
    runtime: LocalRuntime
  ) {
    this.sandboxId = sandboxId
    this.id = sandboxId
    this.generation = generation
    this.state = state
    this.isolation = isolation
    this.runtime = runtime
    this.commands = new Commands(this)
    this.files = new Filesystem(this)
  }

  static async create(
    template = DEFAULT_IMAGE,
    options: SandboxCreateOptions = {}
  ): Promise<Sandbox> {
    const runtime = compatibleRuntime(
      options.runtime ?? new A3SLocalRuntime()
    )
    const timeoutMs = options.timeoutMs ?? 3_600_000
    if (timeoutMs <= 0) throw new Error('timeoutMs must be greater than zero')
    const command = initialArgv('initial command', options.command)
    const entrypoint = initialArgv('initial entrypoint', options.entrypoint)
    const result = await runtime.request({
      operation: 'sandbox_create',
      image: template,
      ...(command === undefined ? {} : { command }),
      ...(entrypoint === undefined ? {} : { entrypoint }),
      timeout_seconds: Math.ceil(timeoutMs / 1000),
      env: { ...(options.envs ?? {}) },
      labels: { ...(options.metadata ?? {}) },
      isolation: options.isolation ?? 'microvm',
      ...(options.name === undefined ? {} : { name: options.name }),
      ...(options.cpus === undefined ? {} : { cpus: options.cpus }),
      ...(options.memoryMb === undefined
        ? {}
        : { memory_mb: options.memoryMb }),
      ...(options.filesystemSnapshotId === undefined
        ? {}
        : { filesystem_snapshot_id: options.filesystemSnapshotId }),
      ...(options.workspace === undefined
        ? {}
        : { workspace: options.workspace }),
      ...(options.workdir === undefined ? {} : { workdir: options.workdir }),
      ...(options.user === undefined ? {} : { user: options.user }),
      ...(options.hostname === undefined ? {} : { hostname: options.hostname }),
      mounts: (options.mounts ?? []).map(bridgeVolumeMount),
      tmpfs: (options.tmpfs ?? []).map((mount) => ({
        target: mount.target,
        ...(mount.sizeBytes === undefined
          ? {}
          : { size_bytes: mount.sizeBytes }),
        read_only: mount.readOnly ?? false,
      })),
      network: options.network ?? { mode: 'tsi' },
      ports: (options.ports ?? []).map((port) => ({
        host_port: port.hostPort,
        guest_port: port.guestPort,
      })),
      dns: [...(options.dns ?? [])],
      host_aliases: { ...(options.hostAliases ?? {}) },
      read_only: options.readOnly ?? false,
      persistent: options.persistent ?? false,
      auto_remove: options.autoRemove ?? true,
    })
    return Sandbox.fromResult(
      result,
      runtime,
      undefined,
      options.isolation ?? 'microvm'
    )
  }

  static async connect(
    sandboxId: string,
    options: SandboxConnectOptions = {}
  ): Promise<Sandbox> {
    const runtime = compatibleRuntime(
      options.runtime ?? new A3SLocalRuntime()
    )
    const result = await runtime.request({
      operation: 'sandbox_inspect',
      sandbox_id: sandboxId,
    })
    return Sandbox.fromResult(result, runtime, sandboxId)
  }

  private static fromResult(
    result: BridgeResult,
    runtime: LocalRuntime,
    expectedId?: string,
    expectedIsolation?: Isolation
  ): Sandbox {
    const info = sandboxLifecycleInfo(
      result,
      expectedId,
      expectedIsolation
    )
    return new Sandbox(
      info.sandboxId,
      info.generation,
      info.state,
      info.isolation,
      runtime
    )
  }

  async kill(): Promise<void> {
    if (this.state === 'killed' || this.state === 'removed') return
    const result = await this.runtime.request(
      this.lifecycleRequest('sandbox_kill')
    )
    this.updateLifecycle(result)
    this.state = 'killed'
  }

  async stop(): Promise<void> {
    if (this.state === 'killed' || this.state === 'removed') return
    const result = await this.runtime.request(
      this.lifecycleRequest('sandbox_stop')
    )
    this.updateLifecycle(result)
  }

  async restart(
    options: {
      operationId?: string
      stopTimeoutSeconds?: number
    } = {}
  ): Promise<void> {
    if (this.state === 'killed' || this.state === 'removed') {
      throw new Error(`sandbox ${this.sandboxId} has been removed`)
    }
    if (
      options.operationId !== undefined &&
      options.operationId.trim().length === 0
    ) {
      throw new Error('operationId cannot be empty')
    }
    if (
      options.stopTimeoutSeconds !== undefined &&
      options.stopTimeoutSeconds < 0
    ) {
      throw new Error('stopTimeoutSeconds cannot be negative')
    }
    const result = await this.runtime.request({
      ...this.lifecycleRequest('sandbox_restart'),
      operation_id: options.operationId ?? `sdk-restart-${randomUUID()}`,
      ...(options.stopTimeoutSeconds === undefined
        ? {}
        : { stop_timeout_seconds: options.stopTimeoutSeconds }),
    })
    this.updateLifecycle(result)
  }

  async remove(): Promise<void> {
    if (this.state === 'killed' || this.state === 'removed') return
    const result = await this.runtime.request(
      this.lifecycleRequest('sandbox_remove')
    )
    this.updateLifecycle(result)
    this.state = 'removed'
  }

  async pause(options: { keepMemory?: boolean } = {}): Promise<void> {
    const result = await this.runtime.request({
      ...this.lifecycleRequest('sandbox_pause'),
      keep_memory: options.keepMemory ?? true,
    })
    this.updateLifecycle(result)
  }

  async resume(): Promise<void> {
    const result = await this.runtime.request(
      this.lifecycleRequest('sandbox_resume')
    )
    this.updateLifecycle(result)
  }

  async isRunning(): Promise<boolean> {
    try {
      const result = await this.runtime.request({
        operation: 'sandbox_inspect',
        sandbox_id: this.sandboxId,
      })
      this.updateLifecycle(result)
      return this.state === 'running'
    } catch (error) {
      if (error instanceof A3SBoxError && error.code === 'not_found') {
        return false
      }
      throw error
    }
  }

  async logs(options: { tail?: number } = {}): Promise<SandboxLogEntry[]> {
    const tail = options.tail ?? 100
    if (!Number.isInteger(tail) || tail < 1 || tail > 10_000) {
      throw new Error('tail must be an integer between 1 and 10000')
    }
    if (this.state === 'killed' || this.state === 'removed') {
      throw new Error(`sandbox ${this.sandboxId} has been removed`)
    }
    const result = await this.runtime.request({
      ...this.lifecycleRequest('sandbox_logs'),
      tail,
    })
    return unknownRecordArray(result.logs).map(sandboxLogEntry)
  }

  async stats(): Promise<SandboxStats | undefined> {
    if (this.state === 'killed' || this.state === 'removed') return undefined
    const result = await this.runtime.request(
      this.lifecycleRequest('sandbox_stats')
    )
    return result.stats === null || result.stats === undefined
      ? undefined
      : sandboxStats(asRecord(result.stats))
  }

  async processes(): Promise<ExecutionProcessInventory> {
    this.requireObservable()
    const inventory = executionProcessInventory(
      await this.runtime.request(
        this.lifecycleRequest('sandbox_processes')
      )
    )
    this.validateExecutionIdentity(
      inventory.executionId,
      inventory.generation
    )
    validateProcessInventory(inventory)
    return inventory
  }

  async runtimeStats(): Promise<ExecutionStats> {
    this.requireObservable()
    const stats = executionStats(
      await this.runtime.request(
        this.lifecycleRequest('sandbox_runtime_stats')
      )
    )
    this.validateExecutionIdentity(stats.executionId, stats.generation)
    validateExecutionStats(stats)
    return stats
  }

  async events(
    options: ExecutionEventsOptions = {}
  ): Promise<ExecutionEventBatch> {
    this.requireObservable()
    const afterSequence = options.afterSequence ?? 0
    const limit = options.limit ?? 256
    validateInteger('afterSequence', afterSequence, 0)
    validateInteger(
      'limit',
      limit,
      1,
      MAX_EXECUTION_EVENT_BATCH_ITEMS
    )
    if (options.waitTimeoutMs !== undefined) {
      validateInteger('waitTimeoutMs', options.waitTimeoutMs, 0)
    }
    const batch = executionEventBatch(
      await this.runtime.request({
        ...this.lifecycleRequest('sandbox_events'),
        after_sequence: afterSequence,
        limit,
        ...(options.waitTimeoutMs === undefined
          ? {}
          : { wait_timeout_ms: options.waitTimeoutMs }),
      })
    )
    this.validateExecutionIdentity(batch.executionId, batch.generation)
    validateExecutionEventBatch(batch, afterSequence)
    return batch
  }

  streamEvents(
    options: ExecutionEventStreamOptions = {}
  ): AsyncIterable<ExecutionRuntimeEvent> {
    this.requireObservable()
    const afterSequence = options.afterSequence ?? 0
    const batchSize = options.batchSize ?? DEFAULT_EVENT_STREAM_BATCH_ITEMS
    const waitTimeoutMs =
      options.waitTimeoutMs ?? DEFAULT_EVENT_STREAM_WAIT_TIMEOUT_MS
    validateInteger('afterSequence', afterSequence, 0)
    validateInteger(
      'batchSize',
      batchSize,
      1,
      MAX_EXECUTION_EVENT_BATCH_ITEMS
    )
    validateInteger('waitTimeoutMs', waitTimeoutMs, 1)

    const sandbox = this
    const generation = this.generation
    const signal = options.signal
    return {
      async *[Symbol.asyncIterator](): AsyncGenerator<ExecutionRuntimeEvent> {
        let cursor = afterSequence
        while (true) {
          if (signal?.aborted) throw canceledEventStreamError()
          sandbox.requireStreamGeneration(generation)
          const batch = executionEventBatch(
            await sandbox.runtime.request(
              {
                operation: 'sandbox_events',
                sandbox_id: sandbox.sandboxId,
                generation,
                after_sequence: cursor,
                limit: batchSize,
                wait_timeout_ms: waitTimeoutMs,
              },
              { signal }
            )
          )
          sandbox.validateExecutionIdentity(
            batch.executionId,
            batch.generation,
            generation
          )
          validateExecutionEventBatch(batch, cursor)
          cursor = batch.nextSequence
          if (signal?.aborted) throw canceledEventStreamError()
          for (const event of batch.events) yield event
        }
      },
    }
  }

  async updateResources(
    update: ExecutionResourceUpdate,
    options: UpdateResourcesOptions = {}
  ): Promise<void> {
    this.requireRunning()
    if (
      options.operationId !== undefined &&
      options.operationId.trim().length === 0
    ) {
      throw new Error('operationId cannot be empty')
    }
    const result = await this.runtime.request({
      ...this.lifecycleRequest('sandbox_update_resources'),
      operation_id:
        options.operationId ?? `sdk-resource-update-${randomUUID()}`,
      resources: executionResourceUpdateValue(update),
    })
    this.updateLifecycle(result, 'running', this.generation)
  }

  async createFilesystemSnapshot(
    snapshotId: string
  ): Promise<FilesystemSnapshotInfo> {
    const result = await this.runtime.request({
      ...this.lifecycleRequest('sandbox_snapshot_create'),
      snapshot_id: snapshotId,
    })
    const snapshot = filesystemSnapshotInfo(result)
    this.generation = snapshot.generation
    this.state = snapshot.state
    return snapshot
  }

  script(source: string | Uint8Array | Script): ScriptBuilder {
    return this.commands.script(source)
  }

  static async filesystemSnapshotSize(
    snapshotId: string,
    options: SandboxConnectOptions = {}
  ): Promise<number | undefined> {
    const runtime = compatibleRuntime(
      options.runtime ?? new A3SLocalRuntime()
    )
    const result = await runtime.request({
      operation: 'filesystem_snapshot_size',
      snapshot_id: snapshotId,
    })
    const size = result.size_bytes
    if (size === null || size === undefined) return undefined
    if (typeof size !== 'number') {
      throw new A3SBoxError(
        'Bridge result has an invalid size_bytes',
        'bridge_protocol_error'
      )
    }
    return size
  }

  static async deleteFilesystemSnapshot(
    snapshotId: string,
    options: SandboxConnectOptions = {}
  ): Promise<boolean> {
    const runtime = compatibleRuntime(
      options.runtime ?? new A3SLocalRuntime()
    )
    const result = await runtime.request({
      operation: 'filesystem_snapshot_delete',
      snapshot_id: snapshotId,
    })
    if (typeof result.deleted !== 'boolean') {
      throw new A3SBoxError(
        'Bridge result is missing deleted',
        'bridge_protocol_error'
      )
    }
    return result.deleted
  }

  bridgeRequest(request: Readonly<Record<string, unknown>>): Promise<BridgeResult> {
    return this.runtime.request(request)
  }

  private lifecycleRequest(
    operation: string
  ): Readonly<Record<string, unknown>> {
    return {
      operation,
      sandbox_id: this.sandboxId,
      generation: this.generation,
    }
  }

  private updateLifecycle(
    result: BridgeResult,
    expectedState?: string,
    expectedGeneration?: number
  ): void {
    const info = sandboxLifecycleInfo(
      result,
      this.sandboxId,
      this.isolation
    )
    if (expectedState !== undefined && info.state !== expectedState) {
      throw new A3SBoxError(
        `Bridge returned sandbox state ${info.state}; expected ${expectedState}`,
        'bridge_protocol_error'
      )
    }
    if (
      expectedGeneration !== undefined &&
      info.generation !== expectedGeneration
    ) {
      throw new A3SBoxError(
        'Bridge returned a different execution generation',
        'bridge_protocol_error'
      )
    }
    this.generation = info.generation
    this.state = info.state
  }

  private requireRunning(): void {
    if (this.state !== 'running') {
      throw new Error(`sandbox ${this.sandboxId} is not running`)
    }
  }

  private requireObservable(): void {
    if (this.state !== 'running' && this.state !== 'paused') {
      throw new Error(
        `sandbox ${this.sandboxId} is neither running nor paused`
      )
    }
  }

  private requireStreamGeneration(expectedGeneration: number): void {
    if (
      this.generation !== expectedGeneration ||
      (this.state !== 'running' && this.state !== 'paused')
    ) {
      throw new A3SBoxError(
        `Sandbox ${this.sandboxId} changed while streaming events`,
        'conflict'
      )
    }
  }

  private validateExecutionIdentity(
    executionId: string,
    generation: number,
    expectedGeneration = this.generation
  ): void {
    if (
      executionId !== this.sandboxId ||
      generation !== expectedGeneration
    ) {
      throw new A3SBoxError(
        'Bridge returned a different execution generation',
        'bridge_protocol_error'
      )
    }
  }
}

function canceledEventStreamError(): A3SBoxError {
  return new A3SBoxError('Sandbox event stream was canceled', 'canceled')
}

function validateProcessInventory(
  inventory: ExecutionProcessInventory
): void {
  const identifiers = new Set<string>()
  for (const process of inventory.processes) {
    if (
      process.processId.trim().length === 0 ||
      (process.pid !== undefined &&
        (!Number.isSafeInteger(process.pid) || process.pid <= 0)) ||
      identifiers.has(process.processId)
    ) {
      throw new A3SBoxError(
        'Bridge returned an invalid runtime process inventory',
        'bridge_protocol_error'
      )
    }
    identifiers.add(process.processId)
  }
}

function validateExecutionStats(stats: ExecutionStats): void {
  if (
    stats.timestampUnixNs <= 0 ||
    stats.cpu.userNs + stats.cpu.systemNs > stats.cpu.usageNs ||
    (stats.memory.peakBytes !== undefined &&
      stats.memory.peakBytes < stats.memory.usageBytes) ||
    Object.keys(stats.metrics).some(
      (name) =>
        name.length === 0 ||
        name.length > 256 ||
        /[\u0000-\u0020\u007f]/.test(name)
    )
  ) {
    throw new A3SBoxError(
      'Bridge returned invalid runtime statistics',
      'bridge_protocol_error'
    )
  }
}

function validateExecutionEventBatch(
  batch: ExecutionEventBatch,
  afterSequence: number
): void {
  let previous = afterSequence
  for (const event of batch.events) {
    if (event.sequence <= previous || event.timestampUnixNs <= 0) {
      throw new A3SBoxError(
        'Bridge returned an invalid runtime event order',
        'bridge_protocol_error'
      )
    }
    previous = event.sequence
  }
  if (batch.nextSequence < previous) {
    throw new A3SBoxError(
      'Bridge returned a regressed runtime event cursor',
      'bridge_protocol_error'
    )
  }
}

function executionResourceUpdateValue(
  update: ExecutionResourceUpdate
): Readonly<Record<string, number | string>> {
  if (typeof update !== 'object' || update === null) {
    throw new Error('update must be an ExecutionResourceUpdate')
  }
  const result: Record<string, number | string> = {}
  const numericFields: readonly [
    keyof ExecutionResourceUpdate,
    string,
    number,
    number?
  ][] = [
    ['memoryReservation', 'memory_reservation', 0],
    ['memorySwap', 'memory_swap', -1],
    ['pidsLimit', 'pids_limit', 1],
    ['cpuShares', 'cpu_shares', 2, 262_144],
    ['cpuQuota', 'cpu_quota', 1],
    ['cpuPeriod', 'cpu_period', 1],
  ]
  for (const [property, field, minimum, maximum] of numericFields) {
    const value = update[property]
    if (value === undefined) continue
    if (typeof value !== 'number') {
      throw new Error(`${property} must be an integer`)
    }
    validateInteger(property, value, minimum, maximum)
    result[field] = value
  }
  if (update.cpusetCpus !== undefined) {
    if (!validCpuset(update.cpusetCpus)) {
      throw new Error(
        'cpusetCpus must be a comma-separated list of indices or ascending ranges'
      )
    }
    result.cpuset_cpus = update.cpusetCpus
  }
  if (Object.keys(result).length === 0) {
    throw new Error(
      'resource update must change at least one supported field'
    )
  }
  return result
}

function validateInteger(
  name: string,
  value: number,
  minimum: number,
  maximum = Number.MAX_SAFE_INTEGER
): void {
  if (
    !Number.isSafeInteger(value) ||
    value < minimum ||
    value > maximum
  ) {
    throw new Error(
      `${name} must be an integer between ${minimum} and ${maximum}`
    )
  }
}

function validCpuset(value: unknown): boolean {
  if (typeof value !== 'string' || value.trim().length === 0) return false
  return value.split(',').every((rawItem) => {
    const item = rawItem.trim()
    if (/^[0-9]+$/.test(item)) return validCPUIndex(item)
    const range = /^([0-9]+)-([0-9]+)$/.exec(item)
    return (
      range !== null &&
      validCPUIndex(range[1]) &&
      validCPUIndex(range[2]) &&
      Number(range[1]) <= Number(range[2])
    )
  })
}

function validCPUIndex(value: string): boolean {
  const index = Number(value)
  return Number.isSafeInteger(index) && index <= 0xffff_ffff
}

export class Commands {
  constructor(private readonly sandbox: Sandbox) {}

  async run(
    command: string | readonly string[],
    options: CommandRunOptions = {}
  ): Promise<CommandResult> {
    const argv =
      typeof command === 'string'
        ? ['/bin/sh', '-lc', command]
        : [...command]
    if (argv.length === 0) throw new Error('command cannot be empty')
    if (options.timeoutMs !== undefined && options.timeoutMs <= 0) {
      throw new Error('timeoutMs must be greater than zero')
    }
    const stdin =
      options.stdin === undefined
        ? undefined
        : Buffer.from(options.stdin).toString('base64')
    const result = await this.sandbox.bridgeRequest({
      operation: 'command_run',
      sandbox_id: this.sandbox.sandboxId,
      generation: this.sandbox.generation,
      argv,
      env: { ...(options.envs ?? {}) },
      ...(options.timeoutMs === undefined
        ? {}
        : { timeout_ms: options.timeoutMs }),
      ...(options.cwd === undefined ? {} : { cwd: options.cwd }),
      ...(options.user === undefined ? {} : { user: options.user }),
      ...(stdin === undefined ? {} : { stdin_base64: stdin }),
    })
    return {
      stdout: decodeBase64(result, 'stdout_base64').toString('utf8'),
      stderr: decodeBase64(result, 'stderr_base64').toString('utf8'),
      exitCode: requiredNumber(result, 'exit_code'),
      truncated: requiredBoolean(result, 'truncated'),
    }
  }

  script(source: string | Uint8Array | Script): ScriptBuilder {
    return new ScriptBuilder(this, source)
  }

  async runScript(
    source: string | Uint8Array | Script,
    options: CommandRunOptions = {}
  ): Promise<CommandResult> {
    let builder = this.script(source)
    if (options.timeoutMs !== undefined) {
      builder = builder.timeout(options.timeoutMs)
    }
    for (const [key, value] of Object.entries(options.envs ?? {})) {
      builder = builder.env(key, value)
    }
    if (options.cwd !== undefined) builder = builder.cwd(options.cwd)
    if (options.user !== undefined) builder = builder.user(options.user)
    return builder.run()
  }
}

/** Fluent script builder that sends source through stdin to an interpreter. */
export class ScriptBuilder {
  private readonly source: string | Uint8Array
  private interpreterArgv: string[]
  private options: CommandRunOptions = {}

  constructor(
    private readonly commands: Commands,
    script: string | Uint8Array | Script
  ) {
    if (isScript(script)) {
      this.source = script.source
      this.interpreterArgv = [...(script.interpreter ?? ['/bin/sh', '-se'])]
    } else {
      this.source = script
      this.interpreterArgv = ['/bin/sh', '-se']
    }
  }

  interpreter(executable: string, ...args: string[]): ScriptBuilder {
    this.interpreterArgv = [executable, ...args]
    return this
  }

  timeout(timeoutMs: number): ScriptBuilder {
    this.options = { ...this.options, timeoutMs }
    return this
  }

  env(key: string, value: string): ScriptBuilder {
    this.options = {
      ...this.options,
      envs: { ...(this.options.envs ?? {}), [key]: value },
    }
    return this
  }

  cwd(path: string): ScriptBuilder {
    this.options = { ...this.options, cwd: path }
    return this
  }

  user(user: string): ScriptBuilder {
    this.options = { ...this.options, user }
    return this
  }

  async run(): Promise<CommandResult> {
    if (this.source.length === 0) throw new Error('script source cannot be empty')
    if (this.interpreterArgv.length === 0) {
      throw new Error('script interpreter cannot be empty')
    }
    return this.commands.run(this.interpreterArgv, {
      ...this.options,
      stdin: this.source,
    })
  }
}

export class Filesystem {
  constructor(private readonly sandbox: Sandbox) {}

  async write(
    path: string,
    data: string | Uint8Array,
    options: { user?: string } = {}
  ): Promise<WriteInfo> {
    const result = await this.sandbox.bridgeRequest({
      ...this.request('file_write', path, options.user),
      data_base64: Buffer.from(data).toString('base64'),
    })
    return {
      path: requiredString(result, 'path'),
      size: requiredNumber(result, 'size'),
    }
  }

  async read(path: string, options?: FilesystemReadOptions): Promise<string>
  async read(
    path: string,
    options: FilesystemReadOptions & { format: 'bytes' }
  ): Promise<Uint8Array>
  async read(
    path: string,
    options: FilesystemReadOptions = {}
  ): Promise<string | Uint8Array> {
    const data = await this.readBytes(path, options.user)
    return options.format === 'bytes' ? data : data.toString('utf8')
  }

  async export(
    path: string,
    options: ArtifactExportOptions = {}
  ): Promise<Artifact> {
    if (typeof path !== 'string' || path.trim().length === 0) {
      throw new A3SBoxError(
        'artifact source path cannot be empty',
        'invalid_request'
      )
    }
    const maxBytes = artifactLimit(options.maxBytes ?? MAX_ARTIFACT_BYTES)
    const destination = artifactDestination(options.destination)
    const entry = await this.stat(path, { user: options.user })
    if (entry.type !== 'file') {
      throw new A3SBoxError(
        `artifact source ${JSON.stringify(path)} must be a file`,
        'invalid_request'
      )
    }
    if (!Number.isSafeInteger(entry.size) || entry.size < 0) {
      throw new A3SBoxError(
        'Bridge returned an invalid artifact size',
        'bridge_protocol_error'
      )
    }
    if (entry.size > maxBytes) {
      throw new A3SBoxError(
        `artifact source is ${entry.size} bytes; maxBytes is ${maxBytes}`,
        'invalid_request'
      )
    }
    const data = await this.readBytes(path, options.user, maxBytes)
    if (data.length > maxBytes) {
      throw new A3SBoxError(
        `artifact source grew beyond maxBytes (${maxBytes}) while reading`,
        'bridge_protocol_error'
      )
    }
    if (data.length !== entry.size) {
      throw new A3SBoxError(
        'artifact source changed size while it was being exported',
        'bridge_protocol_error'
      )
    }
    if (destination !== undefined) {
      await writeNewHostFile(destination, data)
    }
    return {
      path,
      data,
      size: data.length,
      sha256: createHash('sha256').update(data).digest('hex'),
      ...(destination === undefined ? {} : { hostPath: destination }),
    }
  }

  async stat(path: string, options: { user?: string } = {}): Promise<EntryInfo> {
    const result = await this.sandbox.bridgeRequest(
      this.request('filesystem_stat', path, options.user)
    )
    return entryInfo(requiredRecord(result, 'entry'))
  }

  async exists(
    path: string,
    options: { user?: string } = {}
  ): Promise<boolean> {
    try {
      await this.stat(path, options)
      return true
    } catch (error) {
      if (error instanceof A3SBoxError && error.code === 'not_found') {
        return false
      }
      throw error
    }
  }

  async list(
    path: string,
    options: { depth?: number; user?: string } = {}
  ): Promise<EntryInfo[]> {
    const result = await this.sandbox.bridgeRequest({
      ...this.request('filesystem_list', path, options.user),
      depth: options.depth ?? 1,
    })
    if (!Array.isArray(result.entries)) {
      throw new A3SBoxError('Bridge result is missing entries', 'bridge_protocol_error')
    }
    return result.entries.map((entry) => entryInfo(asRecord(entry)))
  }

  async makeDir(
    path: string,
    options: { user?: string } = {}
  ): Promise<EntryInfo | undefined> {
    const result = await this.sandbox.bridgeRequest(
      this.request('filesystem_make_dir', path, options.user)
    )
    return result.entry === undefined ? undefined : entryInfo(asRecord(result.entry))
  }

  async rename(
    oldPath: string,
    newPath: string,
    options: { user?: string } = {}
  ): Promise<EntryInfo | undefined> {
    const result = await this.sandbox.bridgeRequest({
      ...this.request('filesystem_move', oldPath, options.user),
      destination: newPath,
    })
    return result.entry === undefined ? undefined : entryInfo(asRecord(result.entry))
  }

  async remove(path: string, options: { user?: string } = {}): Promise<void> {
    await this.sandbox.bridgeRequest(
      this.request('filesystem_remove', path, options.user)
    )
  }

  private async readBytes(
    path: string,
    user: string | undefined,
    maxBytes?: number
  ): Promise<Buffer> {
    const request = this.request('file_read', path, user)
    const result = await this.sandbox.bridgeRequest(
      maxBytes === undefined ? request : { ...request, max_bytes: maxBytes }
    )
    const responsePath = requiredString(result, 'path')
    if (responsePath !== path) {
      throw new A3SBoxError(
        'Bridge returned file data for a different path',
        'bridge_protocol_error'
      )
    }
    const data = decodeBase64(result, 'data_base64')
    const declaredSize = requiredNumber(result, 'size')
    if (
      !Number.isSafeInteger(declaredSize) ||
      declaredSize < 0 ||
      declaredSize !== data.length
    ) {
      throw new A3SBoxError(
        'Bridge returned inconsistent file size metadata',
        'bridge_protocol_error'
      )
    }
    return data
  }

  private request(
    operation: string,
    path: string,
    user: string | undefined
  ): Readonly<Record<string, unknown>> {
    return {
      operation,
      sandbox_id: this.sandbox.sandboxId,
      generation: this.sandbox.generation,
      path,
      ...(user === undefined ? {} : { user }),
    }
  }
}

function artifactLimit(maxBytes: number): number {
  if (
    !Number.isSafeInteger(maxBytes) ||
    maxBytes <= 0 ||
    maxBytes > MAX_ARTIFACT_BYTES
  ) {
    throw new A3SBoxError(
      `maxBytes must be between 1 and ${MAX_ARTIFACT_BYTES}`,
      'invalid_request'
    )
  }
  return maxBytes
}

function artifactDestination(destination: string | undefined): string | undefined {
  if (destination === undefined) return undefined
  if (typeof destination !== 'string' || destination.trim().length === 0) {
    throw new A3SBoxError(
      'destination must be a non-empty host filesystem path',
      'invalid_request'
    )
  }
  return destination
}

async function writeNewHostFile(
  destination: string,
  data: Uint8Array
): Promise<void> {
  let file: FileHandle
  try {
    file = await open(destination, 'wx', 0o600)
  } catch (error) {
    throw new A3SBoxError(
      `Could not create artifact destination ${JSON.stringify(destination)}: ${errorMessage(error)}`,
      'runtime_error'
    )
  }

  try {
    await file.writeFile(data)
    await file.sync()
    await file.close()
  } catch (error) {
    try {
      await file.close()
    } catch {
      // The original write or close failure remains authoritative.
    }
    let cleanupFailure: unknown
    try {
      await rm(destination)
    } catch (cleanupError) {
      cleanupFailure = cleanupError
    }
    const cleanup =
      cleanupFailure === undefined
        ? ''
        : `; partial-file cleanup failed: ${errorMessage(cleanupFailure)}`
    throw new A3SBoxError(
      `Could not write artifact destination ${JSON.stringify(destination)}: ${errorMessage(error)}${cleanup}`,
      'runtime_error'
    )
  }
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}

function bridgeVolumeMount(
  mount: VolumeMount
): Readonly<Record<string, unknown>> {
  return mount.kind === 'bind'
    ? {
        kind: 'bind',
        source: mount.source,
        target: mount.target,
        read_only: mount.readOnly ?? false,
      }
    : {
        kind: 'named',
        name: mount.name,
        target: mount.target,
        read_only: mount.readOnly ?? false,
      }
}

function initialArgv(
  name: string,
  value: readonly string[] | undefined
): readonly string[] | undefined {
  if (value === undefined) return undefined
  if (!Array.isArray(value) || value.some((part) => typeof part !== 'string')) {
    throw new Error(`${name} must be an array of strings`)
  }
  if (value.length === 0) throw new Error(`${name} cannot be empty`)
  if (value[0].trim().length === 0) {
    throw new Error(`${name} first element cannot be blank`)
  }
  return [...value]
}

function isScript(value: string | Uint8Array | Script): value is Script {
  return (
    typeof value === 'object' &&
    !(value instanceof Uint8Array) &&
    'source' in value
  )
}

function requiredIsolation(result: BridgeResult): Isolation {
  const isolation = requiredString(result, 'isolation')
  if (isolation !== 'microvm' && isolation !== 'sandbox') {
    throw new A3SBoxError(
      'Bridge result has an invalid isolation',
      'bridge_protocol_error'
    )
  }
  return isolation
}

interface SandboxLifecycleInfo {
  sandboxId: string
  generation: number
  state: string
  isolation: Isolation
}

function sandboxLifecycleInfo(
  result: BridgeResult,
  expectedId?: string,
  expectedIsolation?: Isolation
): SandboxLifecycleInfo {
  const sandboxId = requiredString(result, 'sandbox_id')
  if (sandboxId.trim().length === 0) {
    throw new A3SBoxError(
      'Bridge result has an invalid sandbox_id',
      'bridge_protocol_error'
    )
  }
  if (expectedId !== undefined && sandboxId !== expectedId) {
    throw new A3SBoxError(
      'Bridge result returned a different sandbox_id',
      'bridge_protocol_error'
    )
  }
  const isolation = requiredIsolation(result)
  if (expectedIsolation !== undefined && isolation !== expectedIsolation) {
    throw new A3SBoxError(
      'Bridge result changed sandbox isolation',
      'bridge_protocol_error'
    )
  }
  return {
    sandboxId,
    generation: requiredGeneration(result, 'generation'),
    state: requiredSandboxState(result, 'state'),
    isolation,
  }
}
