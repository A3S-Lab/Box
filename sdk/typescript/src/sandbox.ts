import { A3SBoxError } from './errors.js'
import {
  A3SLocalRuntime,
  type BridgeResult,
  type LocalRuntime,
} from './runtime.js'

export const DEFAULT_IMAGE = 'alpine:3.20'

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

export class Sandbox {
  readonly sandboxId: string
  readonly id: string
  generation: number
  state: string
  readonly commands: Commands
  readonly files: Filesystem
  private readonly runtime: LocalRuntime

  protected constructor(
    sandboxId: string,
    generation: number,
    state: string,
    runtime: LocalRuntime
  ) {
    this.sandboxId = sandboxId
    this.id = sandboxId
    this.generation = generation
    this.state = state
    this.runtime = runtime
    this.commands = new Commands(this)
    this.files = new Filesystem(this)
  }

  static async create(
    template = DEFAULT_IMAGE,
    options: SandboxCreateOptions = {}
  ): Promise<Sandbox> {
    const runtime = options.runtime ?? new A3SLocalRuntime()
    const timeoutMs = options.timeoutMs ?? 3_600_000
    if (timeoutMs <= 0) throw new Error('timeoutMs must be greater than zero')
    const result = await runtime.request({
      operation: 'sandbox_create',
      image: template,
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
    return Sandbox.fromResult(result, runtime)
  }

  static async connect(
    sandboxId: string,
    options: SandboxConnectOptions = {}
  ): Promise<Sandbox> {
    const runtime = options.runtime ?? new A3SLocalRuntime()
    const result = await runtime.request({
      operation: 'sandbox_inspect',
      sandbox_id: sandboxId,
    })
    return Sandbox.fromResult(result, runtime)
  }

  private static fromResult(
    result: BridgeResult,
    runtime: LocalRuntime
  ): Sandbox {
    return new Sandbox(
      requiredString(result, 'sandbox_id'),
      requiredNumber(result, 'generation'),
      requiredString(result, 'state'),
      runtime
    )
  }

  async kill(): Promise<void> {
    if (this.state === 'killed') return
    await this.runtime.request(this.lifecycleRequest('sandbox_kill'))
    this.state = 'killed'
  }

  async pause(options: { keepMemory?: boolean } = {}): Promise<void> {
    const result = await this.runtime.request({
      ...this.lifecycleRequest('sandbox_pause'),
      keep_memory: options.keepMemory ?? true,
    })
    this.updateLifecycle(result, 'paused')
  }

  async resume(): Promise<void> {
    const result = await this.runtime.request(
      this.lifecycleRequest('sandbox_resume')
    )
    this.updateLifecycle(result, 'running')
  }

  async isRunning(): Promise<boolean> {
    try {
      const result = await this.runtime.request({
        operation: 'sandbox_inspect',
        sandbox_id: this.sandboxId,
      })
      this.updateLifecycle(result, this.state)
      return this.state === 'running'
    } catch (error) {
      if (error instanceof A3SBoxError && error.code === 'not_found') {
        return false
      }
      throw error
    }
  }

  async createFilesystemSnapshot(
    snapshotId: string
  ): Promise<FilesystemSnapshotInfo> {
    const result = await this.runtime.request({
      ...this.lifecycleRequest('sandbox_snapshot_create'),
      snapshot_id: snapshotId,
    })
    this.updateLifecycle(result, this.state)
    return filesystemSnapshotInfo(result)
  }

  script(source: string | Uint8Array | Script): ScriptBuilder {
    return this.commands.script(source)
  }

  static async filesystemSnapshotSize(
    snapshotId: string,
    options: SandboxConnectOptions = {}
  ): Promise<number | undefined> {
    const runtime = options.runtime ?? new A3SLocalRuntime()
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
    const runtime = options.runtime ?? new A3SLocalRuntime()
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

  private updateLifecycle(result: BridgeResult, fallbackState: string): void {
    if (typeof result.generation === 'number') {
      this.generation = result.generation
    }
    this.state =
      typeof result.state === 'string' ? result.state : fallbackState
  }
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
      truncated: result.truncated === true,
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
    const result = await this.sandbox.bridgeRequest(
      this.request('file_read', path, options.user)
    )
    const data = decodeBase64(result, 'data_base64')
    return options.format === 'bytes' ? data : data.toString('utf8')
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

function isScript(value: string | Uint8Array | Script): value is Script {
  return (
    typeof value === 'object' &&
    !(value instanceof Uint8Array) &&
    'source' in value
  )
}

function entryInfo(entry: Record<string, unknown>): EntryInfo {
  return {
    name: requiredString(entry, 'name'),
    type: entryType(entry.type),
    path: requiredString(entry, 'path'),
    size: requiredNumber(entry, 'size'),
    mode: requiredNumber(entry, 'mode'),
    permissions: requiredString(entry, 'permissions'),
    owner: requiredString(entry, 'owner'),
    group: requiredString(entry, 'group'),
    modifiedSeconds: requiredNumber(entry, 'modified_seconds'),
    modifiedNanos: requiredNumber(entry, 'modified_nanos'),
    ...(typeof entry.symlink_target === 'string'
      ? { symlinkTarget: entry.symlink_target }
      : {}),
  }
}

function filesystemSnapshotInfo(result: BridgeResult): FilesystemSnapshotInfo {
  return {
    snapshotId: requiredString(result, 'snapshot_id'),
    sizeBytes: requiredNumber(result, 'size_bytes'),
    state: requiredString(result, 'state'),
    generation: requiredNumber(result, 'generation'),
  }
}

function entryType(value: unknown): EntryInfo['type'] {
  if (value === 'file' || value === 'directory' || value === 'unspecified') {
    return value
  }
  throw new A3SBoxError('Bridge returned an invalid entry type', 'bridge_protocol_error')
}

function requiredString(result: BridgeResult, key: string): string {
  const value = result[key]
  if (typeof value !== 'string') {
    throw new A3SBoxError(`Bridge result is missing ${key}`, 'bridge_protocol_error')
  }
  return value
}

function requiredNumber(result: BridgeResult, key: string): number {
  const value = result[key]
  if (typeof value !== 'number') {
    throw new A3SBoxError(`Bridge result is missing ${key}`, 'bridge_protocol_error')
  }
  return value
}

function requiredRecord(
  result: BridgeResult,
  key: string
): Record<string, unknown> {
  return asRecord(result[key])
}

function asRecord(value: unknown): Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    throw new A3SBoxError('Bridge result is not an object', 'bridge_protocol_error')
  }
  return value as Record<string, unknown>
}

function decodeBase64(result: BridgeResult, key: string): Buffer {
  return Buffer.from(requiredString(result, key), 'base64')
}
