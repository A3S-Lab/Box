import type {
  FilesystemSnapshotSummary,
  RuntimeDiagnostics,
  RuntimeDiskUsage,
  RuntimeVirtualization,
  SandboxLogEntry,
  SandboxStats,
  SandboxSummary,
} from '../src/index.js'

type PublicInspectionTypes = [
  FilesystemSnapshotSummary,
  RuntimeDiagnostics,
  RuntimeDiskUsage,
  RuntimeVirtualization,
  SandboxLogEntry,
  SandboxStats,
  SandboxSummary,
]

export type { PublicInspectionTypes }
