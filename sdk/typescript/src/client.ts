import {
  asRecord,
  buildImageInfo,
  filesystemSnapshotSummary,
  imageHistoryInfo,
  imageInfo,
  imageInspectInfo,
  networkInfo,
  pushImageInfo,
  recordArray,
  runtimeDiagnostics,
  runtimeDiskUsage,
  sandboxSummary,
  sdkCapabilities,
  stringArray,
  unknownRecordArray,
  volumeInfo,
} from './bridge-values.js'
import { A3SLocalRuntime, type LocalRuntime } from './runtime.js'
import {
  DEFAULT_IMAGE,
  Sandbox,
  type Isolation,
  type PortMapping,
  type SandboxCreateOptions,
  type SandboxNetwork,
  type TmpfsMount,
  type VolumeMount,
} from './sandbox.js'

export interface BuildImageInfo {
  reference: string
  digest: string
  sizeBytes: number
  layerCount: number
}

export interface ImageInfo {
  reference: string
  digest: string
  sizeBytes: number
  pulledAt: string
  lastUsed: string
  path: string
}

export type RegistryProtocol = 'https' | 'http'

export class RegistryCredentials {
  constructor(
    readonly username: string,
    readonly password: string
  ) {}

  bridgeValue(): Readonly<Record<string, string>> {
    return {
      username: this.username,
      password: this.password,
    }
  }
}

type SignaturePolicyValue =
  | { mode: 'skip' }
  | { mode: 'cosign_key'; public_key: string }
  | { mode: 'cosign_keyless'; issuer: string; identity: string }

export class SignaturePolicy {
  private constructor(private readonly value: SignaturePolicyValue) {}

  static skip(): SignaturePolicy {
    return new SignaturePolicy({ mode: 'skip' })
  }

  static cosignKey(publicKey: string): SignaturePolicy {
    return new SignaturePolicy({
      mode: 'cosign_key',
      public_key: publicKey,
    })
  }

  static cosignKeyless(issuer: string, identity: string): SignaturePolicy {
    return new SignaturePolicy({
      mode: 'cosign_keyless',
      issuer,
      identity,
    })
  }

  bridgeValue(): SignaturePolicyValue {
    return this.value
  }
}

export interface ImageHealthCheckInfo {
  test: readonly string[]
  interval?: number
  timeout?: number
  retries?: number
  startPeriod?: number
}

export interface ImageInspectInfo extends ImageInfo {
  manifestDigest: string
  layerCount: number
  entrypoint?: readonly string[]
  command?: readonly string[]
  env: Readonly<Record<string, string>>
  workingDir?: string
  user?: string
  exposedPorts: readonly string[]
  volumes: readonly string[]
  stopSignal?: string
  healthCheck?: ImageHealthCheckInfo
  onbuild: readonly string[]
  labels: Readonly<Record<string, string>>
}

export interface ImageHistoryInfo {
  created?: string
  createdBy: string
  sizeBytes: number
  comment: string
  emptyLayer: boolean
}

export interface PushImageInfo {
  reference: string
  manifestDigest: string
  configUrl: string
  manifestUrl: string
}

export interface SdkCapabilities {
  protocolVersion: number
  operations: readonly string[]
}

export interface SandboxSummary {
  id: string
  shortId: string
  name: string
  image: string
  isolation: string
  status: string
  statusSummary: string
  active: boolean
  pid?: number
  cpus: number
  memoryMb: number
  ports: readonly string[]
  command: readonly string[]
  health: string
  labels: Readonly<Record<string, string>>
  createdAt: string
  startedAt?: string
  networkName?: string
  volumeNames: readonly string[]
}

export interface SandboxLogEntry {
  stream: string
  message: string
  timestamp?: string
}

export interface SandboxStats {
  id: string
  shortId: string
  name: string
  status: string
  pid: number
  cpus: number
  cpuPercent: number
  cpuPercentScaled: number
  memoryBytes: number
  memoryLimitBytes: number
  memoryPercent: number
  networkRxBytes: number
  networkTxBytes: number
  blockReadBytes: number
  blockWriteBytes: number
}

export interface RuntimeVirtualization {
  available: boolean
  backend?: string
  details: string
}

export interface RuntimeDiagnostics {
  coreVersion: string
  runtimeVersion: string
  sdkVersion: string
  home: string
  virtualization: RuntimeVirtualization
}

export interface RuntimeDiskUsage {
  home: string
  totalBytes: number
  boxesBytes: number
  imagesBytes: number
  volumesBytes: number
  snapshotsBytes: number
  stateBytes: number
  otherBytes: number
}

export interface FilesystemSnapshotSummary {
  id: string
  name: string
  sourceSandboxId: string
  image: string
  vcpus: number
  memoryMb: number
  volumes: readonly string[]
  command: readonly string[]
  ports: readonly string[]
  labels: Readonly<Record<string, string>>
  networkMode?: string
  sizeBytes: number
  createdAt: string
  description: string
}

export interface VolumeInfo {
  name: string
  driver: string
  mountPoint: string
  labels: Readonly<Record<string, string>>
  inUseBy: readonly string[]
  inUse: boolean
  sizeLimit: number
  createdAt: string
}

export interface NetworkEndpointInfo {
  boxId: string
  boxName: string
  aliases: readonly string[]
  ipAddress: string
  macAddress: string
}

export interface NetworkInfo {
  name: string
  driver: string
  subnet: string
  gateway: string
  labels: Readonly<Record<string, string>>
  endpoints: readonly NetworkEndpointInfo[]
  endpointCount: number
  isolation: string
  createdAt: string
}

/** Host-level resource client and entry point for fluent builders. */
export class A3SBoxClient {
  constructor(readonly runtime: LocalRuntime = new A3SLocalRuntime()) {}

  image(contextDir: string): ImageBuilder {
    return new ImageBuilder(this.runtime, contextDir)
  }

  volume(name: string): VolumeBuilder {
    return new VolumeBuilder(this.runtime, name)
  }

  network(name: string): NetworkBuilder {
    return new NetworkBuilder(this.runtime, name)
  }

  sandbox(image = DEFAULT_IMAGE): SandboxBuilder {
    return new SandboxBuilder(this.runtime, image)
  }

  async pullImage(
    reference: string,
    options: {
      force?: boolean
      platform?: string
      credentials?: RegistryCredentials
      signaturePolicy?: SignaturePolicy
    } = {}
  ): Promise<ImageInfo> {
    return imageInfo(
      await this.runtime.request({
        operation: 'image_pull',
        reference,
        force: options.force ?? false,
        ...(options.platform === undefined
          ? {}
          : { platform: options.platform }),
        ...(options.credentials === undefined
          ? {}
          : { credentials: options.credentials.bridgeValue() }),
        ...(options.signaturePolicy === undefined
          ? {}
          : { signature_policy: options.signaturePolicy.bridgeValue() }),
      })
    )
  }

  async getImage(reference: string): Promise<ImageInfo | undefined> {
    const result = await this.runtime.request({
      operation: 'image_get',
      reference,
    })
    return result.image === null || result.image === undefined
      ? undefined
      : imageInfo(asRecord(result.image))
  }

  async listImages(): Promise<ImageInfo[]> {
    const result = await this.runtime.request({ operation: 'image_list' })
    return recordArray(result, 'images').map(imageInfo)
  }

  async inspectImage(reference: string): Promise<ImageInspectInfo | undefined> {
    const result = await this.runtime.request({
      operation: 'image_inspect',
      reference,
    })
    return result.image === null || result.image === undefined
      ? undefined
      : imageInspectInfo(asRecord(result.image))
  }

  async imageHistory(
    reference: string
  ): Promise<ImageHistoryInfo[] | undefined> {
    const result = await this.runtime.request({
      operation: 'image_history',
      reference,
    })
    return result.history === null || result.history === undefined
      ? undefined
      : unknownRecordArray(result.history).map(imageHistoryInfo)
  }

  async tagImage(source: string, target: string): Promise<ImageInfo> {
    return imageInfo(
      await this.runtime.request({
        operation: 'image_tag',
        source,
        target,
      })
    )
  }

  async pushImage(
    source: string,
    target: string,
    options: {
      credentials?: RegistryCredentials
      registryProtocol?: RegistryProtocol
    } = {}
  ): Promise<PushImageInfo> {
    return pushImageInfo(
      await this.runtime.request({
        operation: 'image_push',
        source,
        target,
        ...(options.credentials === undefined
          ? {}
          : { credentials: options.credentials.bridgeValue() }),
        ...(options.registryProtocol === undefined
          ? {}
          : { registry_protocol: options.registryProtocol }),
      })
    )
  }

  async removeImage(reference: string): Promise<void> {
    await this.runtime.request({ operation: 'image_remove', reference })
  }

  async evictImages(): Promise<string[]> {
    const result = await this.runtime.request({ operation: 'image_evict' })
    return stringArray(result.references)
  }

  async getVolume(name: string): Promise<VolumeInfo | undefined> {
    const result = await this.runtime.request({ operation: 'volume_get', name })
    return result.volume === null || result.volume === undefined
      ? undefined
      : volumeInfo(asRecord(result.volume))
  }

  async listVolumes(): Promise<VolumeInfo[]> {
    const result = await this.runtime.request({ operation: 'volume_list' })
    return recordArray(result, 'volumes').map(volumeInfo)
  }

  async removeVolume(
    name: string,
    options: { force?: boolean } = {}
  ): Promise<VolumeInfo> {
    return volumeInfo(
      await this.runtime.request({
        operation: 'volume_remove',
        name,
        force: options.force ?? false,
      })
    )
  }

  async pruneVolumes(): Promise<string[]> {
    const result = await this.runtime.request({ operation: 'volume_prune' })
    return stringArray(result.names)
  }

  async getNetwork(name: string): Promise<NetworkInfo | undefined> {
    const result = await this.runtime.request({ operation: 'network_get', name })
    return result.network === null || result.network === undefined
      ? undefined
      : networkInfo(asRecord(result.network))
  }

  async listNetworks(): Promise<NetworkInfo[]> {
    const result = await this.runtime.request({ operation: 'network_list' })
    return recordArray(result, 'networks').map(networkInfo)
  }

  async removeNetwork(name: string): Promise<NetworkInfo> {
    return networkInfo(
      await this.runtime.request({ operation: 'network_remove', name })
    )
  }

  async pruneNetworks(): Promise<string[]> {
    const result = await this.runtime.request({ operation: 'network_prune' })
    return stringArray(result.names)
  }

  async capabilities(): Promise<SdkCapabilities> {
    return sdkCapabilities(
      await this.runtime.request({ operation: 'sdk_capabilities' })
    )
  }

  async listSandboxes(options: { all?: boolean } = {}): Promise<SandboxSummary[]> {
    const result = await this.runtime.request({
      operation: 'sandbox_list',
      all: options.all ?? true,
    })
    return recordArray(result, 'sandboxes').map(sandboxSummary)
  }

  async getSandbox(query: string): Promise<SandboxSummary | undefined> {
    const result = await this.runtime.request({
      operation: 'sandbox_get',
      query,
    })
    return result.sandbox === null || result.sandbox === undefined
      ? undefined
      : sandboxSummary(asRecord(result.sandbox))
  }

  async runtimeDiagnostics(): Promise<RuntimeDiagnostics> {
    return runtimeDiagnostics(
      await this.runtime.request({ operation: 'runtime_diagnostics' })
    )
  }

  async runtimeDiskUsage(): Promise<RuntimeDiskUsage> {
    return runtimeDiskUsage(
      await this.runtime.request({ operation: 'runtime_disk_usage' })
    )
  }

  async listFilesystemSnapshots(): Promise<FilesystemSnapshotSummary[]> {
    const result = await this.runtime.request({
      operation: 'filesystem_snapshot_list',
    })
    return recordArray(result, 'snapshots').map(filesystemSnapshotSummary)
  }

  async getFilesystemSnapshot(
    snapshotId: string
  ): Promise<FilesystemSnapshotSummary | undefined> {
    const result = await this.runtime.request({
      operation: 'filesystem_snapshot_get',
      snapshot_id: snapshotId,
    })
    return result.snapshot === null || result.snapshot === undefined
      ? undefined
      : filesystemSnapshotSummary(asRecord(result.snapshot))
  }
}

export class ImageBuilder {
  private dockerfilePath?: string
  private imageTag?: string
  private readonly buildArgs: Record<string, string> = {}
  private quietBuild = true
  private readonly platforms: string[] = []
  private buildTarget?: string
  private disableCache = false

  constructor(
    private readonly runtime: LocalRuntime,
    private readonly contextDir: string
  ) {}

  dockerfile(path: string): this {
    this.dockerfilePath = path
    return this
  }

  tag(tag: string): this {
    this.imageTag = tag
    return this
  }

  buildArg(key: string, value: string): this {
    this.buildArgs[key] = value
    return this
  }

  quiet(enabled = true): this {
    this.quietBuild = enabled
    return this
  }

  platform(platform: string): this {
    this.platforms.push(platform)
    return this
  }

  target(target: string): this {
    this.buildTarget = target
    return this
  }

  noCache(enabled = true): this {
    this.disableCache = enabled
    return this
  }

  async build(): Promise<BuildImageInfo> {
    const result = await this.runtime.request({
      operation: 'image_build',
      context_dir: this.contextDir,
      build_args: { ...this.buildArgs },
      quiet: this.quietBuild,
      platforms: [...this.platforms],
      no_cache: this.disableCache,
      ...(this.dockerfilePath === undefined
        ? {}
        : { dockerfile: this.dockerfilePath }),
      ...(this.imageTag === undefined ? {} : { tag: this.imageTag }),
      ...(this.buildTarget === undefined ? {} : { target: this.buildTarget }),
    })
    return buildImageInfo(result)
  }
}

export class VolumeBuilder {
  private readonly labels: Record<string, string> = {}
  private sizeLimitBytes = 0

  constructor(
    private readonly runtime: LocalRuntime,
    private readonly name: string
  ) {}

  label(key: string, value: string): this {
    this.labels[key] = value
    return this
  }

  sizeLimit(sizeBytes: number): this {
    this.sizeLimitBytes = sizeBytes
    return this
  }

  async create(): Promise<VolumeInfo> {
    return volumeInfo(
      await this.runtime.request({
        operation: 'volume_create',
        name: this.name,
        labels: { ...this.labels },
        size_limit: this.sizeLimitBytes,
      })
    )
  }
}

export class NetworkBuilder {
  private networkSubnet = '10.89.0.0/24'
  private readonly labels: Record<string, string> = {}

  constructor(
    private readonly runtime: LocalRuntime,
    private readonly name: string
  ) {}

  subnet(subnet: string): this {
    this.networkSubnet = subnet
    return this
  }

  label(key: string, value: string): this {
    this.labels[key] = value
    return this
  }

  async create(): Promise<NetworkInfo> {
    return networkInfo(
      await this.runtime.request({
        operation: 'network_create',
        name: this.name,
        subnet: this.networkSubnet,
        labels: { ...this.labels },
      })
    )
  }
}

export class SandboxBuilder {
  private readonly options: SandboxCreateOptions

  constructor(
    runtime: LocalRuntime,
    private readonly image: string
  ) {
    this.options = { runtime }
  }

  timeout(timeoutMs: number): this {
    this.options.timeoutMs = timeoutMs
    return this
  }

  env(key: string, value: string): this {
    this.options.envs = { ...(this.options.envs ?? {}), [key]: value }
    return this
  }

  metadata(key: string, value: string): this {
    this.options.metadata = {
      ...(this.options.metadata ?? {}),
      [key]: value,
    }
    return this
  }

  name(name: string): this {
    this.options.name = name
    return this
  }

  cpus(cpus: number): this {
    this.options.cpus = cpus
    return this
  }

  memoryMb(memoryMb: number): this {
    this.options.memoryMb = memoryMb
    return this
  }

  isolation(isolation: Isolation): this {
    this.options.isolation = isolation
    return this
  }

  filesystemSnapshot(snapshotId: string): this {
    this.options.filesystemSnapshotId = snapshotId
    return this
  }

  workspace(path: string): this {
    this.options.workspace = path
    return this
  }

  workdir(path: string): this {
    this.options.workdir = path
    return this
  }

  user(user: string): this {
    this.options.user = user
    return this
  }

  hostname(hostname: string): this {
    this.options.hostname = hostname
    return this
  }

  mount(mount: VolumeMount): this {
    this.options.mounts = [...(this.options.mounts ?? []), mount]
    return this
  }

  mountBind(
    source: string,
    target: string,
    options: { readOnly?: boolean } = {}
  ): this {
    return this.mount({
      kind: 'bind',
      source,
      target,
      readOnly: options.readOnly,
    })
  }

  mountNamed(
    name: string,
    target: string,
    options: { readOnly?: boolean } = {}
  ): this {
    return this.mount({
      kind: 'named',
      name,
      target,
      readOnly: options.readOnly,
    })
  }

  tmpfs(
    target: string,
    options: { sizeBytes?: number; readOnly?: boolean } = {}
  ): this {
    const mount: TmpfsMount = {
      target,
      sizeBytes: options.sizeBytes,
      readOnly: options.readOnly,
    }
    this.options.tmpfs = [...(this.options.tmpfs ?? []), mount]
    return this
  }

  network(network: string | SandboxNetwork): this {
    this.options.network =
      typeof network === 'string'
        ? { mode: 'bridge', name: network }
        : network
    return this
  }

  disableNetwork(): this {
    this.options.network = { mode: 'none' }
    return this
  }

  publishTcp(hostPort: number, guestPort: number): this {
    const port: PortMapping = { hostPort, guestPort }
    this.options.ports = [...(this.options.ports ?? []), port]
    return this
  }

  dnsServer(address: string): this {
    this.options.dns = [...(this.options.dns ?? []), address]
    return this
  }

  hostAlias(host: string, address: string): this {
    this.options.hostAliases = {
      ...(this.options.hostAliases ?? {}),
      [host]: address,
    }
    return this
  }

  readOnly(enabled = true): this {
    this.options.readOnly = enabled
    return this
  }

  persistent(enabled = true): this {
    this.options.persistent = enabled
    return this
  }

  autoRemove(enabled = true): this {
    this.options.autoRemove = enabled
    return this
  }

  start(): Promise<Sandbox> {
    return Sandbox.create(this.image, this.options)
  }
}
