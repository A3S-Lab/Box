export const homeContent = {
  zh: {
    eyebrow: '本地 OCI 运行时 · v3',
    heroLineOne: '给每个任务',
    heroLineTwo: '一个独立内核。',
    heroSubtitle:
      'A3S Box 默认在独立内核 MicroVM 中运行 OCI 工作负载。Linux/KVM 可用 CoW snapshot-fork 填充暖池，让重复任务直接从已就绪的 VM 开始。',
    getStarted: '快速开始',
    viewSource: '查看源码',
    install: {
      copy: '复制',
      copied: '已复制',
      tabs: '选择 A3S Box 安装方式或 SDK',
    },
    signals: [
      {
        title: '独立内核',
        detail: '默认隔离',
        href: '#kernel-boundary',
      },
      {
        title: '写时复制',
        detail: 'ROOTFS · RAM',
        href: '#copy-on-write',
      },
      {
        title: '暖池',
        detail: '预启动 MicroVM',
        href: '#warm-pool',
      },
      {
        title: '4 种 SDK',
        detail: 'Rust · Go · Python · TypeScript',
        href: '#native-sdks',
      },
    ],
    capabilityKicker: '运行时工作流',
    capabilityTitle: '明确隔离边界之后，完成整个 OCI 工作流。',
    capabilityBody:
      '从镜像构建到实例生命周期、存储、网络和可编程流水线，CLI 与 SDK 共用同一个本地运行时与状态目录。代际编号会阻止旧请求修改新实例。',
    capabilities: [
      {
        index: '01',
        eyebrow: 'OCI 镜像',
        title: '构建与管理 OCI 镜像',
        body: '支持分层拉取、仓库凭据、rootfs 缓存、多阶段构建、保存、加载和删除。',
        tags: ['OCI', '构建', '缓存'],
        className: 'box-capability--wide box-capability--images',
      },
      {
        index: '02',
        eyebrow: '生命周期',
        title: '管理实例生命周期',
        body: '支持运行、创建、启动、停止、重启、检查、等待、附加和删除。',
        tags: ['状态', '健康检查', '日志'],
        className: 'box-capability--lifecycle',
      },
      {
        index: '03',
        eyebrow: '存储',
        title: '卷与文件系统快照',
        body: '支持绑定挂载、命名卷、tmpfs、文件复制、差异、导出、提交和停止态文件系统快照。',
        tags: ['卷', '快照', '差异'],
        className: 'box-capability--storage',
      },
      {
        index: '04',
        eyebrow: '网络',
        title: 'TSI、桥接网络与端口发布',
        body: '支持 TSI、无网络模式、命名桥接网络、DNS 别名、节点发现和 TCP 端口发布。',
        tags: ['TSI', 'DNS', '端口'],
        className: 'box-capability--network',
      },
      {
        index: '05',
        eyebrow: '可编程 CI/CD',
        title: '用构建者 API 组合执行环境',
        body: '在代码中组合镜像、CPU、内存、环境变量、初始化脚本、卷、网络和命令步骤，并为重复任务复用快照与暖池。',
        tags: ['Builder', '脚本', '并行任务'],
        className: 'box-capability--wide box-capability--cicd',
      },
      {
        index: '06',
        eyebrow: '集成与运维',
        title: '接入 Kubernetes 与可观测系统',
        body: '提供 CRI、containerd shim、结构化日志、审计记录和 Prometheus 指标。',
        tags: ['CRI', '审计', 'Prometheus'],
        className: 'box-capability--operations',
      },
    ],
    sdkKicker: '原生 SDK',
    sdkTitle: '在 CLI 之外，用四种 SDK 编排同一个本地运行时。',
    sdkBody:
      'Rust、Go、Python 和 TypeScript SDK 覆盖镜像构建、资源配置、实例启动、文件、命令、脚本和清理。本地使用不需要 endpoint 或 API key。',
    sdkNotes: {
      rust: '类型化 Rust API',
      go: 'Context 与并发安全 API',
      python: '同步和异步 API',
      typescript: 'Node.js 20+ Promise API',
    },
    platformKicker: '平台边界',
    platformTitle: '核心机制依赖宿主后端，先确认平台支持范围。',
    platformBody:
      'Linux 使用 KVM，Apple Silicon 使用 HVF，Windows x86_64 使用 WHPX。网络、内存快照、Sandbox 和 TEE 的支持范围并不相同。',
    platformHeaders: ['主机', '虚拟机后端', '架构', '当前边界'],
    platformStatuses: [
      'MicroVM + 认证 Sandbox 主机',
      'MicroVM 运行时',
      'MicroVM 运行时（存在已记录限制）',
    ],
    platformLink: '查看完整平台矩阵',
    ctaKicker: '开始使用',
    ctaTitle: '安装 A3S Box，运行第一个 OCI 工作负载。',
    openQuickStart: '打开快速开始',
  },
  en: {
    eyebrow: 'LOCAL OCI RUNTIME · v3',
    heroLineOne: 'Give every task',
    heroLineTwo: 'a dedicated kernel.',
    heroSubtitle:
      'A3S Box runs OCI workloads in dedicated-kernel MicroVMs by default. On Linux/KVM, CoW snapshot-fork can fill warm pools so repeated tasks start from ready VMs.',
    getStarted: 'Get started',
    viewSource: 'View source',
    install: {
      copy: 'Copy',
      copied: 'Copied',
      tabs: 'Choose an A3S Box installer or SDK',
    },
    signals: [
      {
        title: 'Dedicated kernel',
        detail: 'default isolation',
        href: '#kernel-boundary',
      },
      {
        title: 'Copy on write',
        detail: 'ROOTFS · RAM',
        href: '#copy-on-write',
      },
      {
        title: 'Warm pool',
        detail: 'pre-booted MicroVMs',
        href: '#warm-pool',
      },
      {
        title: '4 SDKs',
        detail: 'Rust · Go · Python · TypeScript',
        href: '#native-sdks',
      },
    ],
    capabilityKicker: 'RUNTIME WORKFLOW',
    capabilityTitle:
      'With the boundary understood, run the whole OCI workflow.',
    capabilityBody:
      'From image builds through lifecycle, storage, networking, and programmable pipelines, the CLI and SDKs share one local runtime and state directory. Generations stop stale requests from modifying newer instances.',
    capabilities: [
      {
        index: '01',
        eyebrow: 'OCI IMAGES',
        title: 'Build and manage OCI images',
        body: 'Pull layers, use registry credentials, cache root filesystems, run multi-stage builds, save, load, and remove images.',
        tags: ['OCI', 'build', 'cache'],
        className: 'box-capability--wide box-capability--images',
      },
      {
        index: '02',
        eyebrow: 'LIFECYCLE',
        title: 'Manage the instance lifecycle',
        body: 'Run, create, start, stop, restart, inspect, wait, attach, and remove instances.',
        tags: ['state', 'health', 'logs'],
        className: 'box-capability--lifecycle',
      },
      {
        index: '03',
        eyebrow: 'STORAGE',
        title: 'Volumes and filesystem snapshots',
        body: 'Use bind mounts, named volumes, tmpfs, file copy, diff, export, commit, and stopped-filesystem snapshots.',
        tags: ['volumes', 'snapshot', 'diff'],
        className: 'box-capability--storage',
      },
      {
        index: '04',
        eyebrow: 'NETWORK',
        title: 'TSI, bridge networks, and published ports',
        body: 'Use TSI, no-network mode, named bridge networks, DNS aliases, peer discovery, and TCP port publishing.',
        tags: ['TSI', 'DNS', 'ports'],
        className: 'box-capability--network',
      },
      {
        index: '05',
        eyebrow: 'PROGRAMMABLE CI/CD',
        title: 'Compose execution environments with builder APIs',
        body: 'Combine images, CPU, memory, environment variables, init scripts, volumes, networks, and command steps in code, then reuse snapshots and warm pools for repeated work.',
        tags: ['Builder', 'scripts', 'parallel jobs'],
        className: 'box-capability--wide box-capability--cicd',
      },
      {
        index: '06',
        eyebrow: 'INTEGRATION & OPS',
        title: 'Connect Kubernetes and observability systems',
        body: 'Use the CRI, containerd shim, structured logs, audit records, and Prometheus metrics.',
        tags: ['CRI', 'audit', 'Prometheus'],
        className: 'box-capability--operations',
      },
    ],
    sdkKicker: 'NATIVE SDKs',
    sdkTitle:
      'Beyond the CLI, orchestrate the same local runtime from four SDKs.',
    sdkBody:
      'Rust, Go, Python, and TypeScript cover image builds, resources, instance startup, files, commands, scripts, and cleanup. Local use needs no endpoint or API key.',
    sdkNotes: {
      rust: 'Typed Rust API',
      go: 'Context and concurrency-safe API',
      python: 'Synchronous and asynchronous API',
      typescript: 'Node.js 20+ Promise API',
    },
    platformKicker: 'PLATFORM BOUNDARIES',
    platformTitle:
      'Core mechanisms depend on the host backend. Check support first.',
    platformBody:
      'Linux uses KVM, Apple Silicon uses HVF, and Windows x86_64 uses WHPX. Networking, memory snapshots, Sandbox, and TEE have different support boundaries.',
    platformHeaders: ['Host', 'VM backend', 'Architecture', 'Current boundary'],
    platformStatuses: [
      'MicroVM + certified Sandbox hosts',
      'MicroVM runtime',
      'MicroVM runtime with documented limits',
    ],
    platformLink: 'Read the complete platform matrix',
    ctaKicker: 'GET STARTED',
    ctaTitle: 'Install A3S Box and run your first OCI workload.',
    openQuickStart: 'Open the quick start',
  },
} as const;

export const sdkCards = [
  {
    id: 'rust',
    language: 'Rust',
    packageName: 'a3s-box-sdk',
    command: 'cargo add a3s-box-sdk',
    href: '/sdk/rust.html',
  },
  {
    id: 'go',
    language: 'Go',
    packageName: 'sdk/go/v3',
    command: 'go get github.com/A3S-Lab/Box/sdk/go/v3',
    href: '/sdk/go.html',
  },
  {
    id: 'python',
    language: 'Python',
    packageName: 'a3s-box',
    command: 'python -m pip install a3s-box',
    href: '/sdk/python.html',
  },
  {
    id: 'typescript',
    language: 'TypeScript',
    packageName: '@a3s-lab/box',
    command: 'npm install @a3s-lab/box',
    href: '/sdk/typescript.html',
  },
] as const;

export const platformRows = [
  {
    platform: 'Linux',
    backend: 'KVM',
    architecture: 'x86_64 / arm64',
  },
  {
    platform: 'macOS',
    backend: 'HVF',
    architecture: 'Apple Silicon',
  },
  {
    platform: 'Windows',
    backend: 'WHPX',
    architecture: 'x86_64',
  },
] as const;
