# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Package deterministic, checksum-pinned libkrun source for Linux/macOS and
  the exact tested `krun.dll`, `krun.lib`, and `libkrunfw.dll` bundle for
  Windows without exceeding crates.io's 10 MiB archive limit.
- Publish the native runtime release, license notices, provenance record, and
  matching libkrunfw/Linux corresponding source before publishing the crate.

### Fixed

- Preserve guest-visible POSIX mode, UID, and GID values in the Windows
  virtiofs inode table so callers can capture and replay them across VM
  generations.
- Verify cached firmware downloads on every use and replace partial or corrupt
  downloads instead of extracting them as trusted inputs.
- Keep the Windows runtime trio version-locked instead of combining a current
  `krun.dll` with an older downloaded firmware DLL.

## [0.1.5] - 2026-03-12

### Fixed
- Fixed TSI EventSet::OUT error log pollution on macOS
  - Changed spurious "EventSet::OUT while not connecting" error logs to debug level
  - This was caused by residual kqueue EVFILT_WRITE events after state transition
  - Fixes compatibility with OpenClaw and other network services in a3s-box

### Changed
- Updated to libkrun with TSI bug fix (commit e8b1854)

## [0.1.4] - 2024-03-10

### Added
- Initial release with Windows WHPX backend support
- Cross-platform support: Linux (KVM), macOS (Hypervisor.framework), Windows (WHPX)
- virtiofs, virtio-net, virtio-blk, virtio-console support
- TSI (Transparent Socket Impersonation) for vsock
