# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
