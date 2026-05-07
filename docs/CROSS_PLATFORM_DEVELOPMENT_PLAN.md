# a3s-box Cross-Platform Development Plan

Date: 2026-05-04

## Executive Summary

This plan outlines the development roadmap for making a3s-box a practical Docker replacement with full cross-platform support across macOS, Linux, and Windows. The plan prioritizes platform-agnostic abstractions, platform-specific implementations, and comprehensive testing infrastructure.

## Platform Support Matrix

### Current Status

| Platform | Hypervisor | Status | Limitations |
|----------|-----------|--------|-------------|
| macOS ARM64 | Apple HVF | ✅ Production | - |
| macOS x86_64 | Apple HVF | ⚠️ Limited | Intel Mac support |
| Linux x86_64 | KVM | ✅ Production | - |
| Linux ARM64 | KVM | ✅ Production | - |
| Windows x86_64 | WHPX | ⚠️ Experimental | Port forwarding, networking |

### Target Status (6 months)

| Platform | Hypervisor | Target | Key Work |
|----------|-----------|--------|----------|
| macOS ARM64 | Apple HVF | ✅ Production | Stability, performance |
| macOS x86_64 | Apple HVF | ✅ Production | Testing, CI |
| Linux x86_64 | KVM | ✅ Production | Full Docker parity |
| Linux ARM64 | KVM | ✅ Production | ARM-specific testing |
| Windows x86_64 | WHPX | ✅ Beta | Networking, vsock, virtiofs |

## Architecture Principles

### 1. Platform Abstraction Layers

```
┌─────────────────────────────────────────────────────────┐
│                   CLI / CRI / API                        │
└─────────────────────────────────────────────────────────┘
                          │
┌─────────────────────────────────────────────────────────┐
│              Platform-Agnostic Runtime                   │
│  (Container lifecycle, OCI, networking, volumes)        │
└─────────────────────────────────────────────────────────┘
                          │
┌─────────────────────────────────────────────────────────┐
│              Platform Abstraction Layer                  │
│  Traits: VmmBackend, NetworkBackend, FsBackend         │
└─────────────────────────────────────────────────────────┘
                          │
        ┌─────────────────┼─────────────────┐
        │                 │                 │
┌───────▼──────┐  ┌──────▼──────┐  ┌──────▼──────┐
│ macOS (HVF)  │  │ Linux (KVM) │  │ Windows     │
│              │  │             │  │ (WHPX)      │
└──────────────┘  └─────────────┘  └─────────────┘
```

### 2. Cross-Platform Design Rules

1. **Trait-based backends**: All platform-specific code behind traits
2. **Compile-time selection**: Use `#[cfg(target_os)]` for platform selection
3. **Graceful degradation**: Features unavailable on a platform return clear errors
4. **Unified testing**: Same test suite runs on all platforms
5. **Platform parity tracking**: Document feature availability per platform

## Phase 1: Foundation (Weeks 1-4)

### 1.1 Platform Abstraction Layer

**Goal**: Extract platform-specific code into trait-based backends

#### Tasks

**T1.1.1: Define Core Traits** (Week 1)
- [ ] `VmmBackend` trait for hypervisor operations
  - `boot()`, `shutdown()`, `pause()`, `resume()`
  - `configure_cpu()`, `configure_memory()`
  - Platform-specific context management
- [ ] `NetworkBackend` trait for networking
  - `create_bridge()`, `attach_interface()`
  - `configure_nat()`, `configure_port_forward()`
  - DNS resolution, IP allocation
- [ ] `FsBackend` trait for filesystem operations
  - `mount_virtiofs()`, `mount_bind()`
  - `create_volume()`, `snapshot_volume()`
- [ ] `ExecBackend` trait for command execution
  - `exec()`, `attach_pty()`
  - Signal delivery, exit code capture

**T1.1.2: Implement Platform Backends** (Weeks 2-3)
- [ ] `HvfBackend` for macOS (Apple Hypervisor Framework)
  - Leverage existing libkrun HVF support
  - Handle ARM64 and x86_64 differences
- [ ] `KvmBackend` for Linux (KVM)
  - Leverage existing libkrun KVM support
  - Handle x86_64 and ARM64 differences
- [ ] `WhpxBackend` for Windows (Windows Hypervisor Platform)
  - Implement missing vsock support
  - Implement virtiofs alternative (9p or SMB)
  - Port forwarding via TCP proxy

**T1.1.3: Runtime Integration** (Week 4)
- [ ] Refactor `VmManager` to use trait backends
- [ ] Add platform detection and backend selection
- [ ] Implement feature flags for platform-specific capabilities
- [ ] Add runtime capability reporting (`a3s-box info`)

#### Platform-Specific Challenges

| Challenge | macOS | Linux | Windows |
|-----------|-------|-------|---------|
| Vsock | ✅ Native | ✅ Native | ⚠️ TCP proxy needed |
| Virtiofs | ✅ Native | ✅ Native | ⚠️ 9p/SMB alternative |
| Port forwarding | ✅ Native | ✅ Native | ⚠️ TCP proxy |
| Networking | ✅ vmnet | ✅ bridge/tap | ⚠️ NAT/HNS |
| Signal handling | ✅ Unix signals | ✅ Unix signals | ⚠️ Windows events |

### 1.2 Guest Agent Enhancement

**Goal**: Make guest-init platform-aware and robust

#### Tasks

**T1.2.1: Guest-Init Refactoring** (Week 2)
- [ ] Abstract vsock communication layer
  - Support TCP fallback for Windows
  - Implement reconnection logic
- [ ] Enhance exec server
  - Support Windows-style paths in guest
  - Handle signal translation (Unix ↔ Windows)
- [ ] Improve PTY server
  - Handle terminal size changes
  - Support Windows console API

**T1.2.2: Mount Execution** (Week 3)
- [ ] Implement actual mount operations in guest
  - Parse mount specs from host
  - Execute mount commands in guest namespace
  - Handle read-only, tmpfs, bind mounts
- [ ] Volume management
  - Create volume directories
  - Apply permissions and ownership
  - Cleanup on container removal

**T1.2.3: Logging Infrastructure** (Week 4)
- [ ] Implement continuous log streaming
  - Capture stdout/stderr separately
  - Add timestamps (RFC3339)
  - Support log rotation
- [ ] CRI log format compliance
  - `<timestamp> <stream> <flags> <message>`
  - Handle partial lines and buffering

## Phase 2: Core Docker Workflows (Weeks 5-10)

### 2.1 Long-Running Container Model

**Goal**: Detached containers remain observable and controllable

#### Tasks

**T2.1.1: Container State Management** (Week 5)
- [ ] Implement persistent state tracking
  - Store container metadata in `~/.a3s/boxes.json`
  - Track PID, exit code, start time, status
- [ ] Background process management
  - Detach from terminal for `-d` flag
  - Monitor container process lifecycle
  - Capture exit codes and timestamps
- [ ] Restart policy implementation
  - `always`, `on-failure[:max]`, `unless-stopped`
  - Implement restart backoff logic
  - Persist restart policy in metadata

**T2.1.2: Container Lifecycle Commands** (Week 6)
- [ ] `start` - Start stopped containers
  - Validate container exists and is stopped
  - Restore network, volumes, mounts
  - Apply restart policy
- [ ] `stop` - Graceful shutdown
  - Send SIGTERM, wait for timeout
  - Send SIGKILL if timeout exceeded
  - Cleanup resources (network, mounts)
- [ ] `restart` - Stop + Start
- [ ] `pause` / `unpause` - SIGSTOP/SIGCONT
- [ ] `kill` - Force termination with signal
- [ ] `wait` - Block until container stops

**T2.1.3: Process Monitoring** (Week 6)
- [ ] Implement container monitor daemon
  - Watch for container exits
  - Apply restart policies
  - Emit events on state changes
- [ ] Health check execution
  - Parse HEALTHCHECK from image config
  - Execute checks in container
  - Update container status

### 2.2 Logs and Attach

**Goal**: Docker-compatible log streaming and terminal attachment

#### Tasks

**T2.2.1: Log Storage** (Week 7)
- [ ] Implement log file management
  - Store logs in `~/.a3s/logs/<container-id>.log`
  - Support log rotation (size-based)
  - Implement log drivers (json-file, journald)
- [ ] Log format standardization
  - JSON lines: `{"stream":"stdout","log":"message\n","time":"..."}`
  - Support Docker log format parsing

**T2.2.2: Log Commands** (Week 7)
- [ ] `logs` command implementation
  - `-f` / `--follow` - Stream logs in real-time
  - `--tail N` - Show last N lines
  - `--since` / `--until` - Time-based filtering
  - `--timestamps` - Show timestamps
  - Separate stdout/stderr streams
- [ ] Log streaming infrastructure
  - Tail file with inotify/kqueue/ReadDirectoryChangesW
  - Handle log rotation during streaming
  - Support multiple concurrent readers

**T2.2.3: Attach Command** (Week 8)
- [ ] `attach` command implementation
  - Connect to container's PTY
  - Support detach sequence (Ctrl-P Ctrl-Q)
  - Handle terminal resize events
  - Reconnection on disconnect

### 2.3 Exec Implementation

**Goal**: Execute commands in running containers

#### Tasks

**T2.3.1: Exec Server Enhancement** (Week 8)
- [ ] Implement exec request handling
  - Parse command, args, env, workdir, user
  - Create new process in container namespace
  - Support interactive (`-it`) and detached modes
- [ ] PTY allocation for interactive exec
  - Allocate PTY in guest
  - Forward terminal I/O over vsock
  - Handle terminal size changes
- [ ] Exit code capture
  - Wait for exec process completion
  - Return exit code to CLI

**T2.3.2: Exec CLI** (Week 8)
- [ ] `exec` command implementation
  - `-it` - Interactive with PTY
  - `-u USER` - Run as specific user
  - `-e KEY=VAL` - Environment variables
  - `-w DIR` - Working directory
  - `--privileged` - Run with elevated privileges

**T2.3.3: Cross-Platform Exec** (Week 9)
- [ ] macOS exec implementation
  - Use existing vsock-based exec server
- [ ] Linux exec implementation
  - Use existing vsock-based exec server
- [ ] Windows exec implementation
  - Implement TCP-based exec proxy
  - Handle Windows path translation
  - Map Unix signals to Windows events

### 2.4 Networking

**Goal**: Docker-compatible networking with bridge, DNS, port publishing

#### Tasks

**T2.4.1: Network Drivers** (Week 9)
- [ ] Bridge network driver
  - Create virtual bridge on host
  - Assign IP addresses (DHCP or static)
  - Configure NAT for internet access
- [ ] Host network driver
  - Share host network namespace
- [ ] None network driver
  - No networking

**T2.4.2: DNS Resolution** (Week 9)
- [ ] Implement embedded DNS server
  - Resolve container names to IPs
  - Forward external queries to host DNS
  - Support custom DNS servers (`--dns`)
- [ ] DNS configuration in guest
  - Generate `/etc/resolv.conf`
  - Support search domains

**T2.4.3: Port Publishing** (Week 10)
- [ ] Port mapping implementation
  - `-p HOST:CONTAINER` syntax
  - Support TCP and UDP protocols
  - Implement port allocation (random ports)
- [ ] Platform-specific port forwarding
  - macOS: Use libkrun port mapping
  - Linux: Use iptables/nftables
  - Windows: Use netsh or HNS

**T2.4.4: Container-to-Container Networking** (Week 10)
- [ ] Network isolation
  - Containers in same network can communicate
  - Containers in different networks are isolated
- [ ] Service discovery
  - Resolve container names via DNS
  - Support network aliases

### 2.5 Volumes and Mounts

**Goal**: Persistent data storage and bind mounts

#### Tasks

**T2.5.1: Volume Management** (Week 10)
- [ ] Named volume implementation
  - Create volumes in `~/.a3s/volumes/<name>`
  - Track volume metadata (driver, labels, options)
  - Reference counting for cleanup
- [ ] Volume commands
  - `volume create` - Create named volume
  - `volume ls` - List volumes
  - `volume rm` - Remove volume
  - `volume inspect` - Show volume details
  - `volume prune` - Remove unused volumes

**T2.5.2: Mount Execution** (Week 10)
- [ ] Bind mount implementation
  - Mount host directory into guest
  - Support read-only mounts (`ro`)
  - Handle permission mapping (user namespaces)
- [ ] Volume mount implementation
  - Mount named volume into guest
  - Support volume drivers (local, nfs, etc.)
- [ ] Tmpfs mount implementation
  - Create in-memory filesystem
  - Support size limits

## Phase 3: Platform-Specific Implementations (Weeks 11-16)

### 3.1 Windows-Specific Work

**Goal**: Achieve feature parity on Windows

#### Tasks

**T3.1.1: Vsock Alternative** (Week 11)
- [ ] Implement TCP-based communication
  - Replace vsock with TCP sockets
  - Use localhost with random ports
  - Implement authentication/encryption
- [ ] Port forwarding proxy
  - Implement TCP proxy for port mapping
  - Use Windows Firewall API for rules

**T3.1.2: Filesystem Integration** (Week 12)
- [ ] Virtiofs alternative
  - Evaluate 9p protocol support
  - Evaluate SMB/CIFS mounting
  - Implement chosen solution
- [ ] Windows path handling
  - Convert Windows paths to Unix paths
  - Handle drive letters (C:\ → /mnt/c)

**T3.1.3: Networking** (Week 13)
- [ ] Windows networking stack
  - Use Host Network Service (HNS)
  - Create virtual switches
  - Configure NAT
- [ ] DNS integration
  - Integrate with Windows DNS client
  - Support Windows DNS suffixes

**T3.1.4: Process Management** (Week 14)
- [ ] Windows process lifecycle
  - Use Windows Job Objects for process groups
  - Handle Windows events (not Unix signals)
  - Implement graceful shutdown

### 3.2 macOS-Specific Work

**Goal**: Optimize macOS experience

#### Tasks

**T3.2.1: Apple Silicon Optimization** (Week 11)
- [ ] ARM64-specific tuning
  - Optimize memory allocation
  - Tune CPU pinning
- [ ] Rosetta 2 integration (optional)
  - Support x86_64 images on ARM64
  - Transparent emulation

**T3.2.2: macOS Networking** (Week 12)
- [ ] vmnet framework integration
  - Use vmnet for better performance
  - Support shared and bridged modes
- [ ] macOS firewall integration
  - Configure pf rules for port forwarding
  - Handle macOS security prompts

**T3.2.3: Keychain Integration** (Week 13)
- [ ] Docker credential helper
  - Use macOS Keychain for registry credentials
  - Support `docker-credential-osxkeychain`

### 3.3 Linux-Specific Work

**Goal**: Full Docker parity on Linux

#### Tasks

**T3.3.1: Networking Optimization** (Week 14)
- [ ] CNI plugin support
  - Implement CNI plugin interface
  - Support bridge, macvlan, ipvlan
- [ ] iptables/nftables integration
  - Use iptables for port forwarding
  - Support custom firewall rules

**T3.3.2: Security Features** (Week 15)
- [ ] Seccomp profiles
  - Load seccomp BPF filters
  - Support custom profiles
- [ ] AppArmor/SELinux
  - Apply security profiles
  - Support custom policies
- [ ] User namespaces
  - Map host users to container users
  - Support rootless containers

**T3.3.3: Systemd Integration** (Week 16)
- [ ] Systemd unit files
  - Install a3s-box as systemd service
  - Support socket activation
- [ ] Journald logging
  - Integrate with journald
  - Support `journalctl` queries

## Phase 4: Docker Ecosystem Compatibility (Weeks 17-22)

### 4.1 Docker Engine API

**Goal**: Provide `/var/run/docker.sock` compatibility

#### Tasks

**T4.1.1: API Server** (Week 17)
- [ ] Implement Docker Engine API v1.41+
  - Container endpoints: `/containers/*`
  - Image endpoints: `/images/*`
  - Network endpoints: `/networks/*`
  - Volume endpoints: `/volumes/*`
  - System endpoints: `/info`, `/version`, `/events`
- [ ] Unix socket server
  - Listen on `/var/run/a3s-box.sock`
  - Support symlink to `/var/run/docker.sock`

**T4.1.2: API Compatibility** (Week 18)
- [ ] Request/response format compatibility
  - Match Docker JSON schemas
  - Support query parameters
  - Handle HTTP headers (X-Registry-Auth)
- [ ] Error response compatibility
  - Match Docker error codes
  - Return compatible error messages

**T4.1.3: Streaming Endpoints** (Week 19)
- [ ] `/containers/{id}/logs` - Log streaming
- [ ] `/containers/{id}/attach` - Terminal attachment
- [ ] `/containers/{id}/exec` - Exec creation and start
- [ ] `/events` - Event streaming
- [ ] `/containers/{id}/stats` - Stats streaming

### 4.2 Docker Compose

**Goal**: Support common Compose workflows

#### Tasks

**T4.2.1: Compose File Parsing** (Week 19)
- [ ] Parse Compose v3 format
  - Services, networks, volumes
  - Environment variables, secrets
  - Depends_on, healthchecks
- [ ] Variable substitution
  - Environment variables
  - `.env` file support

**T4.2.2: Compose Commands** (Week 20)
- [ ] `compose up` - Start services
  - Create networks and volumes
  - Start containers in dependency order
  - Handle healthcheck dependencies
- [ ] `compose down` - Stop services
  - Stop containers in reverse order
  - Remove networks and volumes (if requested)
- [ ] `compose ps` - List services
- [ ] `compose logs` - Aggregate logs
- [ ] `compose exec` - Execute in service

**T4.2.3: Compose Features** (Week 21)
- [ ] Service scaling
  - `--scale service=N`
  - Load balancing across replicas
- [ ] Build integration
  - Build images from Dockerfile
  - Cache build contexts
- [ ] Profiles
  - Enable/disable services by profile

### 4.3 Build Support

**Goal**: Dockerfile build capability

#### Tasks

**T4.3.1: BuildKit Integration** (Week 21)
- [ ] Embed BuildKit or use external BuildKit
  - Evaluate embedding vs. external daemon
  - Implement chosen approach
- [ ] Build command
  - `build -t TAG PATH`
  - Support `-f` for custom Dockerfile
  - Support `--platform` for multi-arch

**T4.3.2: Build Features** (Week 22)
- [ ] Multi-stage builds
  - Parse FROM stages
  - Copy artifacts between stages
- [ ] Build cache
  - Layer caching
  - Cache invalidation
- [ ] Build secrets
  - `--secret` flag
  - Mount secrets during build

### 4.4 Credential Helpers

**Goal**: Support Docker credential helpers

#### Tasks

**T4.4.1: Credential Helper Discovery** (Week 22)
- [ ] Discover installed helpers
  - Check `~/.docker/config.json`
  - Search PATH for `docker-credential-*`
- [ ] Platform-specific helpers
  - macOS: `docker-credential-osxkeychain`
  - Linux: `docker-credential-secretservice`
  - Windows: `docker-credential-wincred`

**T4.4.2: Credential Operations** (Week 22)
- [ ] Store credentials
  - Call helper `store` command
  - Fallback to config.json
- [ ] Retrieve credentials
  - Call helper `get` command
  - Fallback to config.json
- [ ] Erase credentials
  - Call helper `erase` command

## Phase 5: Testing and Validation (Weeks 23-26)

### 5.1 Cross-Platform Test Suite

**Goal**: Unified test suite for all platforms

#### Tasks

**T5.1.1: Unit Tests** (Week 23)
- [ ] Platform abstraction tests
  - Mock backends for testing
  - Test trait implementations
- [ ] Core logic tests
  - Container lifecycle
  - Image operations
  - Network management

**T5.1.2: Integration Tests** (Week 24)
- [ ] CLI integration tests
  - Test all CLI commands
  - Verify output format
  - Test error handling
- [ ] API integration tests
  - Test Docker Engine API endpoints
  - Verify request/response format
  - Test streaming endpoints

**T5.1.3: Platform-Specific Tests** (Week 25)
- [ ] macOS test suite
  - Run on ARM64 and x86_64
  - Test HVF-specific features
- [ ] Linux test suite
  - Run on x86_64 and ARM64
  - Test KVM-specific features
- [ ] Windows test suite
  - Run on x86_64
  - Test WHPX-specific features

**T5.1.4: Compatibility Tests** (Week 26)
- [ ] Docker SDK tests
  - Python docker-py
  - Go docker/client
  - Node.js dockerode
- [ ] Testcontainers tests
  - Java Testcontainers
  - Python testcontainers
- [ ] Compose tests
  - Multi-service applications
  - WordPress + MySQL
  - NGINX + PHP-FPM

### 5.2 CI/CD Pipeline

**Goal**: Automated testing on all platforms

#### Tasks

**T5.2.1: GitHub Actions Workflows** (Week 23)
- [ ] macOS CI
  - Build on macOS ARM64 and x86_64
  - Run unit and integration tests
  - Build release artifacts
- [ ] Linux CI
  - Build on Linux x86_64 and ARM64
  - Run unit and integration tests
  - Build release artifacts
- [ ] Windows CI
  - Build on Windows x86_64
  - Run unit and integration tests
  - Build release artifacts

**T5.2.2: Release Automation** (Week 24)
- [ ] Automated releases
  - Tag-based releases
  - Build multi-platform binaries
  - Publish to GitHub Releases
- [ ] Package distribution
  - Homebrew formula (macOS/Linux)
  - Winget manifest (Windows)
  - APT/YUM repositories (Linux)

### 5.3 Documentation

**Goal**: Comprehensive cross-platform documentation

#### Tasks

**T5.3.1: Platform-Specific Guides** (Week 25)
- [ ] macOS installation and setup
  - Homebrew installation
  - HVF requirements
  - Troubleshooting
- [ ] Linux installation and setup
  - Package manager installation
  - KVM requirements
  - Troubleshooting
- [ ] Windows installation and setup
  - Winget installation
  - WHPX requirements
  - Troubleshooting

**T5.3.2: Migration Guides** (Week 26)
- [ ] Docker to a3s-box migration
  - Command mapping
  - Feature differences
  - Known limitations
- [ ] Platform-specific considerations
  - Performance characteristics
  - Security implications
  - Networking differences

## Phase 6: Performance and Optimization (Weeks 27-30)

### 6.1 Performance Benchmarking

**Goal**: Measure and optimize performance

#### Tasks

**T6.1.1: Benchmark Suite** (Week 27)
- [ ] Container lifecycle benchmarks
  - Cold start time
  - Warm start time
  - Stop time
- [ ] I/O benchmarks
  - Disk I/O (virtiofs)
  - Network I/O (vsock, TCP)
  - Log streaming throughput
- [ ] Resource usage benchmarks
  - Memory overhead
  - CPU overhead
  - Disk space usage

**T6.1.2: Performance Optimization** (Week 28)
- [ ] Boot time optimization
  - Optimize kernel boot
  - Reduce init time
  - Implement warm pool
- [ ] I/O optimization
  - Tune virtiofs parameters
  - Optimize vsock buffer sizes
  - Implement zero-copy where possible
- [ ] Memory optimization
  - Reduce runtime memory footprint
  - Implement memory ballooning
  - Optimize image cache

### 6.2 Platform-Specific Optimization

**Goal**: Optimize for each platform's characteristics

#### Tasks

**T6.2.1: macOS Optimization** (Week 29)
- [ ] HVF tuning
  - Optimize CPU configuration
  - Tune memory allocation
- [ ] vmnet optimization
  - Reduce network latency
  - Increase throughput

**T6.2.2: Linux Optimization** (Week 29)
- [ ] KVM tuning
  - Enable KVM features (nested, EPT)
  - Optimize interrupt handling
- [ ] Networking optimization
  - Use vhost-net for better performance
  - Tune TCP parameters

**T6.2.3: Windows Optimization** (Week 30)
- [ ] WHPX tuning
  - Optimize CPU configuration
  - Tune memory allocation
- [ ] Networking optimization
  - Reduce TCP proxy overhead
  - Optimize HNS configuration

## Acceptance Criteria

### Docker Replacement Acceptance

a3s-box can be described as a Docker replacement when:

- [ ] All P0 Docker workflows pass on macOS, Linux, and Windows
- [ ] Docker SDK smoke tests pass (Python, Go, Node.js)
- [ ] Testcontainers smoke tests pass (Java, Python)
- [ ] Docker Compose multi-service app works
- [ ] Performance is within 2x of Docker on each platform
- [ ] Documentation covers all platforms

### Platform Parity Acceptance

Each platform achieves parity when:

- [ ] All core features work (lifecycle, logs, exec, networking, volumes)
- [ ] Platform-specific tests pass
- [ ] Performance benchmarks meet targets
- [ ] Installation and setup are documented
- [ ] Known limitations are documented

## Risk Mitigation

### Technical Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| Windows vsock unavailable | High | Implement TCP proxy fallback |
| Windows virtiofs unavailable | High | Implement 9p or SMB alternative |
| Performance gap vs Docker | Medium | Optimize hot paths, implement warm pool |
| BuildKit integration complexity | Medium | Start with external BuildKit, evaluate embedding later |
| Platform-specific bugs | Medium | Comprehensive testing, CI on all platforms |

### Resource Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| Windows development expertise | High | Allocate dedicated Windows developer |
| CI infrastructure costs | Medium | Use GitHub Actions free tier, optimize test duration |
| Testing hardware availability | Medium | Use cloud VMs for testing (Azure, AWS, GCP) |

## Success Metrics

### Functional Metrics

- [ ] 100% of P0 Docker workflows implemented
- [ ] 90%+ of P1 Docker workflows implemented
- [ ] 95%+ test coverage on core modules
- [ ] All platforms pass acceptance tests

### Performance Metrics

- [ ] Cold start < 300ms (all platforms)
- [ ] Warm start < 50ms (all platforms)
- [ ] Memory overhead < 100MB per container
- [ ] I/O throughput > 80% of native

### Adoption Metrics

- [ ] 1000+ GitHub stars
- [ ] 100+ production users
- [ ] 10+ community contributors
- [ ] 50+ issues resolved

## Timeline Summary

| Phase | Duration | Key Deliverables |
|-------|----------|------------------|
| Phase 1: Foundation | 4 weeks | Platform abstraction layer, guest agent |
| Phase 2: Core Docker | 6 weeks | Lifecycle, logs, exec, networking, volumes |
| Phase 3: Platform-Specific | 6 weeks | Windows, macOS, Linux optimizations |
| Phase 4: Ecosystem | 6 weeks | Engine API, Compose, Build, Credentials |
| Phase 5: Testing | 4 weeks | Test suite, CI/CD, documentation |
| Phase 6: Performance | 4 weeks | Benchmarking, optimization |
| **Total** | **30 weeks** | **Docker replacement ready** |

## Next Steps

1. **Week 1**: Start Phase 1 - Define core traits
2. **Week 2**: Implement platform backends
3. **Week 3**: Refactor runtime to use backends
4. **Week 4**: Complete foundation, start Phase 2

## Appendix A: Platform Feature Matrix

| Feature | macOS | Linux | Windows | Notes |
|---------|-------|-------|---------|-------|
| **Hypervisor** |
| HVF | ✅ | ❌ | ❌ | macOS only |
| KVM | ❌ | ✅ | ❌ | Linux only |
| WHPX | ❌ | ❌ | ✅ | Windows only |
| **Communication** |
| Vsock | ✅ | ✅ | ⚠️ | TCP fallback on Windows |
| Unix sockets | ✅ | ✅ | ⚠️ | Named pipes on Windows |
| **Filesystem** |
| Virtiofs | ✅ | ✅ | ⚠️ | 9p/SMB on Windows |
| Bind mounts | ✅ | ✅ | ✅ | |
| Named volumes | ✅ | ✅ | ✅ | |
| **Networking** |
| Bridge | ✅ | ✅ | ✅ | HNS on Windows |
| NAT | ✅ | ✅ | ✅ | |
| Port forwarding | ✅ | ✅ | ⚠️ | TCP proxy on Windows |
| DNS | ✅ | ✅ | ✅ | |
| **Security** |
| Namespaces | ✅ | ✅ | ⚠️ | Limited on Windows |
| Capabilities | ✅ | ✅ | ❌ | Unix only |
| Seccomp | ✅ | ✅ | ❌ | Unix only |
| AppArmor/SELinux | ❌ | ✅ | ❌ | Linux only |
| **Process** |
| Signals | ✅ | ✅ | ⚠️ | Windows events |
| PTY | ✅ | ✅ | ⚠️ | ConPTY on Windows |
| Exit codes | ✅ | ✅ | ✅ | |

Legend:
- ✅ Fully supported
- ⚠️ Partial support or alternative implementation
- ❌ Not supported

## Appendix B: Code Structure

```
crates/box/src/
├── core/                    # Platform-agnostic types
│   ├── config.rs
│   ├── error.rs
│   └── event.rs
├── runtime/                 # Runtime implementation
│   ├── backends/            # Platform abstraction
│   │   ├── mod.rs          # Trait definitions
│   │   ├── hvf.rs          # macOS HVF backend
│   │   ├── kvm.rs          # Linux KVM backend
│   │   └── whpx.rs         # Windows WHPX backend
│   ├── container.rs         # Container lifecycle
│   ├── image.rs             # OCI image operations
│   ├── network.rs           # Networking
│   ├── volume.rs            # Volume management
│   └── vm.rs                # VM management
├── cli/                     # CLI commands
│   ├── run.rs
│   ├── exec.rs
│   ├── logs.rs
│   └── ...
├── api/                     # Docker Engine API
│   ├── server.rs
│   ├── containers.rs
│   ├── images.rs
│   └── ...
├── guest/                   # Guest agent
│   └── init/
│       ├── exec_server.rs
│       ├── pty_server.rs
│       └── mount.rs
└── shim/                    # libkrun shim
    ├── hvf.rs
    ├── kvm.rs
    └── whpx.rs
```

## Appendix C: Testing Strategy

### Unit Tests
- Mock platform backends
- Test core logic in isolation
- Run on all platforms in CI

### Integration Tests
- Test CLI commands end-to-end
- Test API endpoints
- Test cross-component interactions

### Platform Tests
- Platform-specific test suites
- Run on native hardware/VMs
- Test platform-specific features

### Compatibility Tests
- Docker SDK compatibility
- Testcontainers compatibility
- Docker Compose compatibility

### Performance Tests
- Benchmark suite
- Regression detection
- Cross-platform comparison

### Manual Tests
- Real-world applications
- Multi-container setups
- Edge cases and error handling
