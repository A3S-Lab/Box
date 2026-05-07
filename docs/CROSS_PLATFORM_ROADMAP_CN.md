# a3s-box 跨平台开发路线图

日期: 2026-05-04

## 概述

本路线图规划了 a3s-box 成为 Docker 替代品的跨平台开发计划,预计 **30 周**完成。

## 平台支持目标

| 平台 | 虚拟化技术 | 当前状态 | 6个月目标 |
|------|-----------|---------|----------|
| macOS ARM64 | Apple HVF | ✅ 生产可用 | ✅ 生产可用 |
| macOS x86_64 | Apple HVF | ⚠️ 有限支持 | ✅ 生产可用 |
| Linux x86_64 | KVM | ✅ 生产可用 | ✅ 生产可用 |
| Linux ARM64 | KVM | ✅ 生产可用 | ✅ 生产可用 |
| Windows x86_64 | WHPX | ⚠️ 实验性 | ✅ Beta |

## 六阶段开发计划

### 阶段 1: 基础架构 (第 1-4 周)

**目标**: 建立平台抽象层

#### 核心任务
- **定义平台抽象 Trait**
  - `VmmBackend` - 虚拟化管理
  - `NetworkBackend` - 网络管理
  - `FsBackend` - 文件系统管理
  - `ExecBackend` - 命令执行

- **实现平台后端**
  - `HvfBackend` (macOS)
  - `KvmBackend` (Linux)
  - `WhpxBackend` (Windows)

- **增强 Guest Agent**
  - 支持 TCP 回退(Windows)
  - 实际执行挂载操作
  - 持续日志流

### 阶段 2: 核心 Docker 工作流 (第 5-10 周)

**目标**: 实现 Docker 核心功能

#### 2.1 长期运行容器模型 (第 5-6 周)
- 持久化状态跟踪
- 后台进程管理
- 重启策略 (`always`, `on-failure`, `unless-stopped`)
- 完整生命周期命令 (`start`, `stop`, `restart`, `pause`, `kill`, `wait`)

#### 2.2 日志和附加 (第 7-8 周)
- 日志文件管理和轮转
- `logs -f` 实时流式传输
- `--tail`, `--since`, `--until` 过滤
- `attach` 命令和 PTY 重连

#### 2.3 Exec 实现 (第 8-9 周)
- 交互式和分离式 exec
- 环境变量、工作目录、用户覆盖
- 退出码捕获
- 跨平台 exec (vsock/TCP)

#### 2.4 网络 (第 9-10 周)
- Bridge 网络驱动
- 内嵌 DNS 服务器
- 端口映射 (`-p HOST:CONTAINER`)
- 容器间通信和隔离

#### 2.5 卷和挂载 (第 10 周)
- 命名卷管理
- Bind mount 执行
- Tmpfs 支持
- 卷命令 (`create`, `ls`, `rm`, `inspect`, `prune`)

### 阶段 3: 平台特定实现 (第 11-16 周)

**目标**: 实现平台特性平等

#### 3.1 Windows 特定工作 (第 11-14 周)
- **Vsock 替代方案** - TCP 通信
- **文件系统集成** - 9p 或 SMB
- **网络** - Host Network Service (HNS)
- **进程管理** - Windows Job Objects

#### 3.2 macOS 特定工作 (第 11-13 周)
- **Apple Silicon 优化** - ARM64 调优
- **macOS 网络** - vmnet 框架集成
- **Keychain 集成** - 凭证助手

#### 3.3 Linux 特定工作 (第 14-16 周)
- **网络优化** - CNI 插件支持
- **安全特性** - Seccomp, AppArmor, SELinux
- **Systemd 集成** - 服务单元和 socket 激活

### 阶段 4: Docker 生态兼容 (第 17-22 周)

**目标**: 支持 Docker 生态工具

#### 4.1 Docker Engine API (第 17-19 周)
- 实现 Docker Engine API v1.41+
- Unix socket 服务器 (`/var/run/a3s-box.sock`)
- 流式端点 (logs, attach, exec, events, stats)

#### 4.2 Docker Compose (第 19-21 周)
- 解析 Compose v3 格式
- `compose up/down/ps/logs/exec`
- 服务扩展和健康检查

#### 4.3 构建支持 (第 21-22 周)
- BuildKit 集成
- 多阶段构建
- 构建缓存和 secrets

#### 4.4 凭证助手 (第 22 周)
- 发现已安装的助手
- 平台特定助手 (osxkeychain, secretservice, wincred)

### 阶段 5: 测试和验证 (第 23-26 周)

**目标**: 全面测试和文档

#### 5.1 跨平台测试套件 (第 23-26 周)
- 单元测试 (平台抽象、核心逻辑)
- 集成测试 (CLI、API)
- 平台特定测试 (macOS, Linux, Windows)
- 兼容性测试 (Docker SDK, Testcontainers, Compose)

#### 5.2 CI/CD 流水线 (第 23-24 周)
- GitHub Actions 工作流 (所有平台)
- 自动化发布
- 包分发 (Homebrew, Winget, APT/YUM)

#### 5.3 文档 (第 25-26 周)
- 平台特定安装指南
- Docker 迁移指南
- 故障排除文档

### 阶段 6: 性能和优化 (第 27-30 周)

**目标**: 优化性能

#### 6.1 性能基准测试 (第 27-28 周)
- 容器生命周期基准
- I/O 基准 (磁盘、网络、日志)
- 资源使用基准
- 性能优化 (启动时间、I/O、内存)

#### 6.2 平台特定优化 (第 29-30 周)
- macOS: HVF 调优、vmnet 优化
- Linux: KVM 调优、vhost-net
- Windows: WHPX 调优、HNS 优化

## 关键架构决策

### 平台抽象层设计

```
┌─────────────────────────────────────────┐
│         CLI / CRI / API                  │
└─────────────────────────────────────────┘
              │
┌─────────────────────────────────────────┐
│    平台无关运行时                         │
│  (容器生命周期、OCI、网络、卷)             │
└─────────────────────────────────────────┘
              │
┌─────────────────────────────────────────┐
│    平台抽象层 (Traits)                    │
│  VmmBackend, NetworkBackend, FsBackend  │
└─────────────────────────────────────────┘
              │
    ┌─────────┼─────────┐
    │         │         │
┌───▼───┐ ┌──▼───┐ ┌──▼────┐
│ macOS │ │Linux │ │Windows│
│ (HVF) │ │(KVM) │ │(WHPX) │
└───────┘ └──────┘ └───────┘
```

### 跨平台设计原则

1. **基于 Trait 的后端** - 所有平台特定代码在 trait 后面
2. **编译时选择** - 使用 `#[cfg(target_os)]` 选择平台
3. **优雅降级** - 不可用功能返回清晰错误
4. **统一测试** - 相同测试套件在所有平台运行
5. **平台平等跟踪** - 记录每个平台的功能可用性

## 平台特定挑战

| 挑战 | macOS | Linux | Windows | 解决方案 |
|------|-------|-------|---------|---------|
| Vsock | ✅ 原生 | ✅ 原生 | ❌ 不支持 | TCP 代理 |
| Virtiofs | ✅ 原生 | ✅ 原生 | ❌ 不支持 | 9p/SMB |
| 端口转发 | ✅ 原生 | ✅ 原生 | ⚠️ 有限 | TCP 代理 |
| 网络 | ✅ vmnet | ✅ bridge/tap | ⚠️ HNS | 平台抽象 |
| 信号处理 | ✅ Unix | ✅ Unix | ❌ 不同 | Windows 事件 |

## 验收标准

### Docker 替代验收

- [ ] 所有 P0 Docker 工作流在 macOS、Linux、Windows 上通过
- [ ] Docker SDK 冒烟测试通过 (Python, Go, Node.js)
- [ ] Testcontainers 冒烟测试通过 (Java, Python)
- [ ] Docker Compose 多服务应用工作
- [ ] 性能在每个平台上在 Docker 的 2 倍以内
- [ ] 文档覆盖所有平台

### 平台平等验收

每个平台达到平等时:

- [ ] 所有核心功能工作 (生命周期、日志、exec、网络、卷)
- [ ] 平台特定测试通过
- [ ] 性能基准达到目标
- [ ] 安装和设置有文档
- [ ] 已知限制有文档

## 成功指标

### 功能指标
- 100% P0 Docker 工作流实现
- 90%+ P1 Docker 工作流实现
- 95%+ 核心模块测试覆盖率
- 所有平台通过验收测试

### 性能指标
- 冷启动 < 300ms (所有平台)
- 热启动 < 50ms (所有平台)
- 内存开销 < 100MB/容器
- I/O 吞吐量 > 原生的 80%

### 采用指标
- 1000+ GitHub stars
- 100+ 生产用户
- 10+ 社区贡献者
- 50+ 已解决问题

## 风险缓解

### 技术风险

| 风险 | 影响 | 缓解措施 |
|------|------|---------|
| Windows vsock 不可用 | 高 | 实现 TCP 代理回退 |
| Windows virtiofs 不可用 | 高 | 实现 9p 或 SMB 替代 |
| 与 Docker 性能差距 | 中 | 优化热路径,实现预热池 |
| BuildKit 集成复杂性 | 中 | 先使用外部 BuildKit |
| 平台特定 bug | 中 | 全面测试,所有平台 CI |

### 资源风险

| 风险 | 影响 | 缓解措施 |
|------|------|---------|
| Windows 开发专业知识 | 高 | 分配专门的 Windows 开发者 |
| CI 基础设施成本 | 中 | 使用 GitHub Actions 免费层 |
| 测试硬件可用性 | 中 | 使用云 VM (Azure, AWS, GCP) |

## 时间线总结

| 阶段 | 周数 | 关键交付物 |
|------|------|-----------|
| 阶段 1: 基础 | 4 周 | 平台抽象层、guest agent |
| 阶段 2: 核心 Docker | 6 周 | 生命周期、日志、exec、网络、卷 |
| 阶段 3: 平台特定 | 6 周 | Windows、macOS、Linux 优化 |
| 阶段 4: 生态系统 | 6 周 | Engine API、Compose、Build、凭证 |
| 阶段 5: 测试 | 4 周 | 测试套件、CI/CD、文档 |
| 阶段 6: 性能 | 4 周 | 基准测试、优化 |
| **总计** | **30 周** | **Docker 替代品就绪** |

## 下一步行动

1. **第 1 周**: 开始阶段 1 - 定义核心 trait
2. **第 2 周**: 实现平台后端
3. **第 3 周**: 重构运行时使用后端
4. **第 4 周**: 完成基础,开始阶段 2

## 平台功能矩阵

| 功能 | macOS | Linux | Windows | 备注 |
|------|-------|-------|---------|------|
| **虚拟化** |
| HVF | ✅ | ❌ | ❌ | 仅 macOS |
| KVM | ❌ | ✅ | ❌ | 仅 Linux |
| WHPX | ❌ | ❌ | ✅ | 仅 Windows |
| **通信** |
| Vsock | ✅ | ✅ | ⚠️ | Windows 用 TCP |
| Unix sockets | ✅ | ✅ | ⚠️ | Windows 用命名管道 |
| **文件系统** |
| Virtiofs | ✅ | ✅ | ⚠️ | Windows 用 9p/SMB |
| Bind mounts | ✅ | ✅ | ✅ | |
| Named volumes | ✅ | ✅ | ✅ | |
| **网络** |
| Bridge | ✅ | ✅ | ✅ | Windows 用 HNS |
| NAT | ✅ | ✅ | ✅ | |
| 端口转发 | ✅ | ✅ | ⚠️ | Windows 用 TCP 代理 |
| DNS | ✅ | ✅ | ✅ | |
| **安全** |
| Namespaces | ✅ | ✅ | ⚠️ | Windows 有限 |
| Capabilities | ✅ | ✅ | ❌ | 仅 Unix |
| Seccomp | ✅ | ✅ | ❌ | 仅 Unix |
| AppArmor/SELinux | ❌ | ✅ | ❌ | 仅 Linux |
| **进程** |
| Signals | ✅ | ✅ | ⚠️ | Windows 事件 |
| PTY | ✅ | ✅ | ⚠️ | Windows 用 ConPTY |
| Exit codes | ✅ | ✅ | ✅ | |

图例:
- ✅ 完全支持
- ⚠️ 部分支持或替代实现
- ❌ 不支持
