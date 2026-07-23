import { A3SBoxError } from './errors.js'
import type { BridgeResult } from './runtime.js'
import type {
  BuildImageInfo,
  FilesystemSnapshotSummary,
  ImageHealthCheckInfo,
  ImageHistoryInfo,
  ImageInfo,
  ImageInspectInfo,
  NetworkEndpointInfo,
  NetworkInfo,
  PushImageInfo,
  RuntimeDiagnostics,
  RuntimeDiskUsage,
  SandboxLogEntry,
  SandboxStats,
  SandboxSummary,
  SdkCapabilities,
  VolumeInfo,
} from './client.js'
import type {
  EntryInfo,
  FilesystemSnapshotInfo,
} from './sandbox.js'

export function buildImageInfo(result: BridgeResult): BuildImageInfo {
  return {
    reference: requiredString(result, 'reference'),
    digest: requiredString(result, 'digest'),
    sizeBytes: requiredNumber(result, 'size_bytes'),
    layerCount: requiredNumber(result, 'layer_count'),
  }
}

export function imageInfo(result: BridgeResult): ImageInfo {
  return {
    reference: requiredString(result, 'reference'),
    digest: requiredString(result, 'digest'),
    sizeBytes: requiredNumber(result, 'size_bytes'),
    pulledAt: requiredString(result, 'pulled_at'),
    lastUsed: requiredString(result, 'last_used'),
    path: requiredString(result, 'path'),
  }
}

export function imageInspectInfo(result: BridgeResult): ImageInspectInfo {
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

export function imageHealthCheckInfo(
  result: BridgeResult
): ImageHealthCheckInfo {
  return {
    test: stringArray(result.test),
    interval: optionalNumber(result, 'interval'),
    timeout: optionalNumber(result, 'timeout'),
    retries: optionalNumber(result, 'retries'),
    startPeriod: optionalNumber(result, 'start_period'),
  }
}

export function imageHistoryInfo(result: BridgeResult): ImageHistoryInfo {
  return {
    created: optionalString(result, 'created'),
    createdBy: requiredString(result, 'created_by'),
    sizeBytes: requiredNumber(result, 'size_bytes'),
    comment: requiredString(result, 'comment'),
    emptyLayer: requiredBoolean(result, 'empty_layer'),
  }
}

export function pushImageInfo(result: BridgeResult): PushImageInfo {
  return {
    reference: requiredString(result, 'reference'),
    manifestDigest: requiredString(result, 'manifest_digest'),
    configUrl: requiredString(result, 'config_url'),
    manifestUrl: requiredString(result, 'manifest_url'),
  }
}

export function sdkCapabilities(result: BridgeResult): SdkCapabilities {
  return {
    protocolVersion: requiredNumber(result, 'protocol_version'),
    operations: stringArray(result.operations),
  }
}

export function volumeInfo(result: BridgeResult): VolumeInfo {
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

export function networkInfo(result: BridgeResult): NetworkInfo {
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

export function networkEndpointInfo(
  result: BridgeResult
): NetworkEndpointInfo {
  return {
    boxId: requiredString(result, 'box_id'),
    boxName: requiredString(result, 'box_name'),
    aliases: stringArray(result.aliases),
    ipAddress: requiredString(result, 'ip_address'),
    macAddress: requiredString(result, 'mac_address'),
  }
}

export function sandboxSummary(result: BridgeResult): SandboxSummary {
  return {
    id: requiredString(result, 'id'),
    shortId: requiredString(result, 'short_id'),
    name: requiredString(result, 'name'),
    image: requiredString(result, 'image'),
    isolation: requiredString(result, 'isolation'),
    status: requiredString(result, 'status'),
    statusSummary: requiredString(result, 'status_summary'),
    active: requiredBoolean(result, 'active'),
    pid: optionalNumber(result, 'pid'),
    cpus: requiredNumber(result, 'cpus'),
    memoryMb: requiredNumber(result, 'memory_mb'),
    ports: stringArray(result.ports),
    command: stringArray(result.command),
    health: requiredString(result, 'health'),
    labels: stringRecord(result.labels),
    createdAt: requiredString(result, 'created_at'),
    startedAt: optionalString(result, 'started_at'),
    networkName: optionalString(result, 'network_name'),
    volumeNames: stringArray(result.volume_names),
  }
}

export function sandboxLogEntry(result: BridgeResult): SandboxLogEntry {
  return {
    stream: requiredString(result, 'stream'),
    message: requiredString(result, 'log'),
    timestamp: optionalString(result, 'time'),
  }
}

export function sandboxStats(result: BridgeResult): SandboxStats {
  return {
    id: requiredString(result, 'id'),
    shortId: requiredString(result, 'short_id'),
    name: requiredString(result, 'name'),
    status: requiredString(result, 'status'),
    pid: requiredNumber(result, 'pid'),
    cpus: requiredNumber(result, 'cpus'),
    cpuPercent: requiredNumber(result, 'cpu_percent'),
    cpuPercentScaled: requiredNumber(result, 'cpu_percent_scaled'),
    memoryBytes: requiredNumber(result, 'memory_bytes'),
    memoryLimitBytes: requiredNumber(result, 'memory_limit_bytes'),
    memoryPercent: requiredNumber(result, 'memory_percent'),
    networkRxBytes: requiredNumber(result, 'network_rx_bytes'),
    networkTxBytes: requiredNumber(result, 'network_tx_bytes'),
    blockReadBytes: requiredNumber(result, 'block_read_bytes'),
    blockWriteBytes: requiredNumber(result, 'block_write_bytes'),
  }
}

export function runtimeDiagnostics(result: BridgeResult): RuntimeDiagnostics {
  const virtualization = asRecord(result.virtualization)
  return {
    coreVersion: requiredString(result, 'core_version'),
    runtimeVersion: requiredString(result, 'runtime_version'),
    sdkVersion: requiredString(result, 'sdk_version'),
    home: requiredString(result, 'home'),
    virtualization: {
      available: requiredBoolean(virtualization, 'available'),
      backend: optionalString(virtualization, 'backend'),
      details: requiredString(virtualization, 'details'),
    },
  }
}

export function runtimeDiskUsage(result: BridgeResult): RuntimeDiskUsage {
  return {
    home: requiredString(result, 'home'),
    totalBytes: requiredNumber(result, 'total_bytes'),
    boxesBytes: requiredNumber(result, 'boxes_bytes'),
    imagesBytes: requiredNumber(result, 'images_bytes'),
    volumesBytes: requiredNumber(result, 'volumes_bytes'),
    snapshotsBytes: requiredNumber(result, 'snapshots_bytes'),
    stateBytes: requiredNumber(result, 'state_bytes'),
    otherBytes: requiredNumber(result, 'other_bytes'),
  }
}

export function filesystemSnapshotSummary(
  result: BridgeResult
): FilesystemSnapshotSummary {
  return {
    id: requiredString(result, 'id'),
    name: requiredString(result, 'name'),
    sourceSandboxId: requiredString(result, 'source_box_id'),
    image: requiredString(result, 'image'),
    vcpus: requiredNumber(result, 'vcpus'),
    memoryMb: requiredNumber(result, 'memory_mb'),
    volumes: stringArray(result.volumes),
    command: stringArray(result.command),
    ports: stringArray(result.port_map),
    labels: stringRecord(result.labels),
    networkMode: optionalString(result, 'network_mode'),
    sizeBytes: requiredNumber(result, 'size_bytes'),
    createdAt: requiredString(result, 'created_at'),
    description: requiredString(result, 'description'),
  }
}

export function entryInfo(entry: BridgeResult): EntryInfo {
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

export function filesystemSnapshotInfo(
  result: BridgeResult
): FilesystemSnapshotInfo {
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
  throw new A3SBoxError(
    'Bridge returned an invalid entry type',
    'bridge_protocol_error'
  )
}

export function requiredString(
  result: BridgeResult,
  key: string
): string {
  const value = result[key]
  if (typeof value !== 'string') bridgeTypeError(key)
  return value
}

export function requiredNumber(
  result: BridgeResult,
  key: string
): number {
  const value = result[key]
  if (typeof value !== 'number') bridgeTypeError(key)
  return value
}

export function requiredBoolean(
  result: BridgeResult,
  key: string
): boolean {
  const value = result[key]
  if (typeof value !== 'boolean') bridgeTypeError(key)
  return value
}

export function optionalString(
  result: BridgeResult,
  key: string
): string | undefined {
  const value = result[key]
  if (value === null || value === undefined) return undefined
  if (typeof value !== 'string') bridgeTypeError(key)
  return value
}

export function optionalNumber(
  result: BridgeResult,
  key: string
): number | undefined {
  const value = result[key]
  if (value === null || value === undefined) return undefined
  if (typeof value !== 'number') bridgeTypeError(key)
  return value
}

export function recordArray(
  result: BridgeResult,
  key: string
): BridgeResult[] {
  const value = result[key]
  if (!Array.isArray(value)) bridgeTypeError(key)
  return value.map(asRecord)
}

export function unknownRecordArray(value: unknown): BridgeResult[] {
  if (!Array.isArray(value)) bridgeTypeError('array')
  return value.map(asRecord)
}

export function stringArray(value: unknown): string[] {
  if (!Array.isArray(value) || !value.every((item) => typeof item === 'string')) {
    bridgeTypeError('array')
  }
  return value
}

export function optionalStringArray(
  value: unknown
): string[] | undefined {
  return value === null || value === undefined ? undefined : stringArray(value)
}

export function stringRecord(
  value: unknown
): Readonly<Record<string, string>> {
  const record = asRecord(value)
  if (!Object.values(record).every((item) => typeof item === 'string')) {
    bridgeTypeError('record')
  }
  return record as Record<string, string>
}

export function requiredRecord(
  result: BridgeResult,
  key: string
): BridgeResult {
  return asRecord(result[key])
}

export function asRecord(value: unknown): BridgeResult {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    bridgeTypeError('object')
  }
  return value as BridgeResult
}

export function decodeBase64(result: BridgeResult, key: string): Buffer {
  return Buffer.from(requiredString(result, key), 'base64')
}

function bridgeTypeError(key: string): never {
  throw new A3SBoxError(
    `Bridge result has an invalid ${key}`,
    'bridge_protocol_error'
  )
}
