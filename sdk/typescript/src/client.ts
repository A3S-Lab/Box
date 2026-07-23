import { A3SBoxError } from './errors.js'
import { A3SLocalRuntime, type BridgeResult, type LocalRuntime } from './runtime.js'
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

function buildImageInfo(result: BridgeResult): BuildImageInfo {
  return {
    reference: requiredString(result, 'reference'),
    digest: requiredString(result, 'digest'),
    sizeBytes: requiredNumber(result, 'size_bytes'),
    layerCount: requiredNumber(result, 'layer_count'),
  }
}

function imageInfo(result: BridgeResult): ImageInfo {
  return {
    reference: requiredString(result, 'reference'),
    digest: requiredString(result, 'digest'),
    sizeBytes: requiredNumber(result, 'size_bytes'),
    pulledAt: requiredString(result, 'pulled_at'),
    lastUsed: requiredString(result, 'last_used'),
    path: requiredString(result, 'path'),
  }
}

function imageInspectInfo(result: BridgeResult): ImageInspectInfo {
  const healthCheck =
    result.health_check === null || result.health_check === undefined
      ? undefined
      : imageHealthCheckInfo(asRecord(result.health_check))
  return {
    ...imageInfo(result),
    manifestDigest: requiredString(result, 'manifest_digest'),
    layerCount: requiredNumber(result, 'layer_count'),
    entrypoint: optionalStringArray(result.entrypoint),
    command: optionalStringArray(result.command),
    env: stringRecord(result.env),
    workingDir: optionalString(result, 'working_dir'),
    user: optionalString(result, 'user'),
    exposedPorts: stringArray(result.exposed_ports),
    volumes: stringArray(result.volumes),
    stopSignal: optionalString(result, 'stop_signal'),
    healthCheck,
    onbuild: stringArray(result.onbuild),
    labels: stringRecord(result.labels),
  }
}

function imageHealthCheckInfo(result: BridgeResult): ImageHealthCheckInfo {
  return {
    test: stringArray(result.test),
    interval: optionalNumber(result, 'interval'),
    timeout: optionalNumber(result, 'timeout'),
    retries: optionalNumber(result, 'retries'),
    startPeriod: optionalNumber(result, 'start_period'),
  }
}

function imageHistoryInfo(result: BridgeResult): ImageHistoryInfo {
  return {
    created: optionalString(result, 'created'),
    createdBy: requiredString(result, 'created_by'),
    sizeBytes: requiredNumber(result, 'size_bytes'),
    comment: requiredString(result, 'comment'),
    emptyLayer: requiredBoolean(result, 'empty_layer'),
  }
}

function pushImageInfo(result: BridgeResult): PushImageInfo {
  return {
    reference: requiredString(result, 'reference'),
    manifestDigest: requiredString(result, 'manifest_digest'),
    configUrl: requiredString(result, 'config_url'),
    manifestUrl: requiredString(result, 'manifest_url'),
  }
}

function sdkCapabilities(result: BridgeResult): SdkCapabilities {
  return {
    protocolVersion: requiredNumber(result, 'protocol_version'),
    operations: stringArray(result.operations),
  }
}

function volumeInfo(result: BridgeResult): VolumeInfo {
  return {
    name: requiredString(result, 'name'),
    driver: requiredString(result, 'driver'),
    mountPoint: requiredString(result, 'mount_point'),
    labels: stringRecord(result.labels),
    inUseBy: stringArray(result.in_use_by),
    inUse: requiredBoolean(result, 'in_use'),
    sizeLimit: requiredNumber(result, 'size_limit'),
    createdAt: requiredString(result, 'created_at'),
  }
}

function networkInfo(result: BridgeResult): NetworkInfo {
  return {
    name: requiredString(result, 'name'),
    driver: requiredString(result, 'driver'),
    subnet: requiredString(result, 'subnet'),
    gateway: requiredString(result, 'gateway'),
    labels: stringRecord(result.labels),
    endpoints: recordArray(result, 'endpoints').map(networkEndpointInfo),
    endpointCount: requiredNumber(result, 'endpoint_count'),
    isolation: requiredString(result, 'isolation'),
    createdAt: requiredString(result, 'created_at'),
  }
}

function networkEndpointInfo(result: BridgeResult): NetworkEndpointInfo {
  return {
    boxId: requiredString(result, 'box_id'),
    boxName: requiredString(result, 'box_name'),
    aliases: stringArray(result.aliases),
    ipAddress: requiredString(result, 'ip_address'),
    macAddress: requiredString(result, 'mac_address'),
  }
}

function requiredString(result: BridgeResult, key: string): string {
  const value = result[key]
  if (typeof value !== 'string') bridgeTypeError(key)
  return value
}

function requiredNumber(result: BridgeResult, key: string): number {
  const value = result[key]
  if (typeof value !== 'number') bridgeTypeError(key)
  return value
}

function requiredBoolean(result: BridgeResult, key: string): boolean {
  const value = result[key]
  if (typeof value !== 'boolean') bridgeTypeError(key)
  return value
}

function optionalString(
  result: BridgeResult,
  key: string
): string | undefined {
  const value = result[key]
  if (value === null || value === undefined) return undefined
  if (typeof value !== 'string') bridgeTypeError(key)
  return value
}

function optionalNumber(
  result: BridgeResult,
  key: string
): number | undefined {
  const value = result[key]
  if (value === null || value === undefined) return undefined
  if (typeof value !== 'number') bridgeTypeError(key)
  return value
}

function recordArray(result: BridgeResult, key: string): BridgeResult[] {
  const value = result[key]
  if (!Array.isArray(value)) bridgeTypeError(key)
  return value.map(asRecord)
}

function unknownRecordArray(value: unknown): BridgeResult[] {
  if (!Array.isArray(value)) bridgeTypeError('array')
  return value.map(asRecord)
}

function stringArray(value: unknown): string[] {
  if (!Array.isArray(value) || !value.every((item) => typeof item === 'string')) {
    bridgeTypeError('array')
  }
  return value
}

function optionalStringArray(value: unknown): string[] | undefined {
  return value === null || value === undefined ? undefined : stringArray(value)
}

function stringRecord(value: unknown): Readonly<Record<string, string>> {
  const record = asRecord(value)
  if (!Object.values(record).every((item) => typeof item === 'string')) {
    bridgeTypeError('record')
  }
  return record as Record<string, string>
}

function asRecord(value: unknown): BridgeResult {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    bridgeTypeError('object')
  }
  return value as BridgeResult
}

function bridgeTypeError(key: string): never {
  throw new A3SBoxError(
    `Bridge result has an invalid ${key}`,
    'bridge_protocol_error'
  )
}
