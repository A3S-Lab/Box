import { useState } from 'react';
import { Content, useLang, withBase } from '@rspress/core/runtime';
import { AgentSkillSection } from './AgentSkillSection';
import { CanvasGridEffect } from './CanvasGridEffect';
import { PremiumInteractions } from './PremiumInteractions';
import { RuntimeFeatureShowcase } from './RuntimeFeatureShowcase';

const installCommands = [
  {
    id: 'unix',
    label: 'Linux / macOS',
    command:
      "curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/A3S-Lab/Box/main/install.sh | sh",
  },
  {
    id: 'windows',
    label: 'Windows',
    command:
      'irm https://raw.githubusercontent.com/A3S-Lab/Box/main/install.ps1 | iex',
  },
  {
    id: 'brew',
    label: 'Homebrew',
    command: 'brew install a3s-lab/tap/a3s-box',
  },
  {
    id: 'rust',
    label: 'Rust SDK',
    command: 'cargo add a3s-box-sdk',
  },
  {
    id: 'go',
    label: 'Go SDK',
    command: 'go get github.com/A3S-Lab/Box/sdk/go/v3',
  },
  {
    id: 'python',
    label: 'Python',
    command: 'python -m pip install a3s-box',
  },
  {
    id: 'typescript',
    label: 'TypeScript',
    command: 'npm install @a3s-lab/box',
  },
] as const;

const content = {
  zh: {
    eyebrow: '本地 OCI 运行时 · v3',
    heroLineOne: '给每个任务',
    heroLineTwo: '一个独立内核。',
    heroSubtitle:
      'A3S Box 默认在独立内核 MicroVM 中运行 OCI 工作负载。Linux/KVM 可用 CoW snapshot-fork 填充暖池，让重复任务直接从已就绪的 VM 开始。',
    getStarted: '快速开始',
    exploreFeatures: '查看核心特性',
    exploreCode: '代码漫游',
    copy: '复制',
    copied: '已复制',
    copyInstall: '复制安装命令',
    isolationAria: 'A3S Box 隔离模型',
    visual: {
      resolver: '主机能力检查',
      localRuntime: '本地运行时',
      request: '请求',
      requestTitle: 'OCI 镜像 + 运行参数',
      resolve: '检查主机能力',
      admission: '准入',
      noFallback: '不自动降级',
      default: '默认',
      optIn: '显式启用',
      hardwareVm: '硬件虚拟机',
      dedicatedKernel: '独立来宾内核',
      sharedKernel: '共享内核',
      sandboxBoundary: '命名空间 + seccomp',
      generation: '代际检查',
      durable: '本地状态',
      localOnly: '本机运行',
    },
    signals: [
      ['独立内核', '默认隔离'],
      ['写时复制', 'ROOTFS · RAM'],
      ['暖池', '预启动 MicroVM'],
      ['4 种 SDK', 'Rust · Go · Python · TypeScript'],
    ],
    principleKicker: '隔离模式',
    principleTitle: '默认使用 MicroVM，不支持时直接报错。',
    principleBody:
      '每次创建都会检查主机、隔离、网络和存储配置。Box 在启动前拒绝不支持的组合，并在本地状态中记录实际隔离方式。',
    principles: [
      {
        label: '01 / 默认',
        title: 'MicroVM',
        body: '每个工作负载使用独立的来宾 Linux 内核。Linux、macOS 和 Windows 分别使用 KVM、HVF 和 WHPX。',
        command: 'a3s-box run --rm alpine:3.20 -- uname -a',
      },
      {
        label: '02 / 显式',
        title: 'Sandbox',
        body: 'Linux 共享内核模式，适合可信任务。主机必须支持 namespace、seccomp、从属 ID 和 cgroup v2。',
        command: 'a3s-box run --isolation sandbox ...',
      },
      {
        label: '03 / 错误处理',
        title: '不自动降级',
        body: '隔离、网络、TEE、快照或主机配置不受支持时，创建请求会失败，不会切换到更弱的模式。',
        command: '请求 → 校验 → 持久化 → 启动',
      },
    ],
    capabilityKicker: '运行时功能',
    capabilityTitle: '管理镜像、实例、存储和网络。',
    capabilityBody:
      'CLI 和四种 SDK 共用同一个本地运行时与状态目录。实例操作使用代际编号，避免旧请求修改新实例。',
    capabilities: [
      {
        index: '01',
        eyebrow: 'OCI 镜像',
        title: '管理 OCI 镜像',
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
        eyebrow: '自动化',
        title: '四种本地 SDK',
        body: 'Rust、Go、Python 和 TypeScript SDK 操作同一份本地镜像、实例、卷、网络和快照。',
        tags: ['Rust', 'Go', 'Python', 'TypeScript'],
        className: 'box-capability--wide box-capability--sdk',
      },
    ],
    sdkKicker: '原生 SDK',
    sdkTitle: '用四种 SDK 调用本机 A3S Box。',
    sdkBody:
      'SDK 覆盖镜像构建、资源配置、实例启动、文件、命令、脚本和清理。本地使用不需要 endpoint 或 API key。',
    sdkNotes: {
      rust: '类型化 Rust API',
      go: 'Context 与并发安全 API',
      python: '同步和异步 API',
      typescript: 'Node.js 20+ Promise API',
    },
    platformKicker: '主机后端',
    platformTitle: 'Linux、macOS 和 Windows 使用各自的虚拟化后端。',
    platformBody:
      'Linux 使用 KVM，Apple Silicon 使用 HVF，Windows x86_64 使用 WHPX。部分网络、快照和 TEE 功能受平台限制。',
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
    viewSource: '查看源码',
  },
  en: {
    eyebrow: 'LOCAL OCI RUNTIME · v3',
    heroLineOne: 'Give every task',
    heroLineTwo: 'a dedicated kernel.',
    heroSubtitle:
      'A3S Box runs OCI workloads in dedicated-kernel MicroVMs by default. On Linux/KVM, CoW snapshot-fork can fill warm pools so repeated tasks start from ready VMs.',
    getStarted: 'Get started',
    exploreFeatures: 'Explore core features',
    exploreCode: 'Code walkthrough',
    copy: 'Copy',
    copied: 'Copied',
    copyInstall: 'Copy install command',
    isolationAria: 'A3S Box isolation model',
    visual: {
      resolver: 'host capability check',
      localRuntime: 'local runtime',
      request: 'REQUEST',
      requestTitle: 'OCI image + runtime options',
      resolve: 'check host capabilities',
      admission: 'ADMISSION',
      noFallback: 'No implicit fallback',
      default: 'default',
      optIn: 'explicit opt-in',
      hardwareVm: 'HARDWARE VM',
      dedicatedKernel: 'dedicated guest kernel',
      sharedKernel: 'SHARED KERNEL',
      sandboxBoundary: 'namespaces + seccomp',
      generation: 'generation checked',
      durable: 'local state',
      localOnly: 'runs locally',
    },
    signals: [
      ['Dedicated kernel', 'default isolation'],
      ['Copy on write', 'ROOTFS · RAM'],
      ['Warm pool', 'pre-booted MicroVMs'],
      ['4 SDKs', 'Rust · Go · Python · TypeScript'],
    ],
    principleKicker: 'ISOLATION MODES',
    principleTitle:
      'MicroVM by default. An error when the host cannot provide it.',
    principleBody:
      'Each create request checks the host, isolation, network, and storage settings. Box rejects unsupported combinations before startup and records the selected isolation mode in local state.',
    principles: [
      {
        label: '01 / DEFAULT',
        title: 'MicroVM',
        body: 'Each workload gets a dedicated guest Linux kernel. Linux, macOS, and Windows use KVM, HVF, and WHPX respectively.',
        command: 'a3s-box run --rm alpine:3.20 -- uname -a',
      },
      {
        label: '02 / EXPLICIT',
        title: 'Sandbox',
        body: 'A shared-kernel Linux mode for trusted work. The host must provide namespaces, seccomp, subordinate IDs, and cgroup v2.',
        command: 'a3s-box run --isolation sandbox ...',
      },
      {
        label: '03 / ERRORS',
        title: 'No automatic fallback',
        body: 'Unsupported isolation, network, TEE, snapshot, or host settings fail instead of switching to a weaker mode.',
        command: 'request → validate → persist → boot',
      },
    ],
    capabilityKicker: 'RUNTIME FEATURES',
    capabilityTitle: 'Manage images, instances, storage, and networking.',
    capabilityBody:
      'The CLI and all four SDKs share one local runtime and state directory. Instance operations use generations so stale requests cannot modify a newer instance.',
    capabilities: [
      {
        index: '01',
        eyebrow: 'OCI IMAGES',
        title: 'Manage OCI images',
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
        eyebrow: 'AUTOMATION',
        title: 'Four local SDKs',
        body: 'Rust, Go, Python, and TypeScript operate the same local images, instances, volumes, networks, and snapshots.',
        tags: ['Rust', 'Go', 'Python', 'TypeScript'],
        className: 'box-capability--wide box-capability--sdk',
      },
    ],
    sdkKicker: 'NATIVE SDKs',
    sdkTitle: 'Call the local A3S Box runtime from four SDKs.',
    sdkBody:
      'The SDKs cover image builds, resources, instance startup, files, commands, scripts, and cleanup. Local use needs no endpoint or API key.',
    sdkNotes: {
      rust: 'Typed Rust API',
      go: 'Context and concurrency-safe API',
      python: 'Synchronous and asynchronous API',
      typescript: 'Node.js 20+ Promise API',
    },
    platformKicker: 'HOST BACKENDS',
    platformTitle: 'Linux, macOS, and Windows use different VM backends.',
    platformBody:
      'Linux uses KVM, Apple Silicon uses HVF, and Windows x86_64 uses WHPX. Some network, snapshot, and TEE features are platform-specific.',
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
    viewSource: 'View source',
  },
} as const;

const sdkCards = [
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

const platformRows = [
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

function ArrowIcon() {
  return (
    <svg viewBox="0 0 24 24" aria-hidden="true">
      <path d="M5 12h14M13 6l6 6-6 6" />
    </svg>
  );
}

function CopyIcon() {
  return (
    <svg viewBox="0 0 24 24" aria-hidden="true">
      <rect x="8" y="8" width="11" height="11" rx="2" />
      <path d="M16 8V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h2" />
    </svg>
  );
}

function CheckIcon() {
  return (
    <svg viewBox="0 0 24 24" aria-hidden="true">
      <path d="m5 12 4 4L19 6" />
    </svg>
  );
}

export function HomeLayout() {
  const lang = useLang();
  const isChinese = lang.startsWith('zh');
  const copy = isChinese ? content.zh : content.en;
  const languagePrefix = isChinese ? '' : '/en';
  const docLink = (href: string) => withBase(`${languagePrefix}${href}`);
  const [selectedInstall, setSelectedInstall] =
    useState<(typeof installCommands)[number]['id']>('unix');
  const [copied, setCopied] = useState(false);
  const activeInstall =
    installCommands.find((option) => option.id === selectedInstall) ??
    installCommands[0];

  async function copyInstallCommand() {
    try {
      await navigator.clipboard.writeText(activeInstall.command);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1400);
    } catch {
      setCopied(false);
    }
  }

  return (
    <main className="box-home">
      <PremiumInteractions />
      <div className="box-global-grid" aria-hidden="true">
        <CanvasGridEffect
          cellSize={54}
          className="box-global-grid-canvas"
          intensity={0.62}
          interactionScope="page"
        />
      </div>
      <section className="box-hero" aria-labelledby="box-hero-title">
        <div className="box-hero-copy">
          <div className="box-eyebrow">
            <span />
            {copy.eyebrow}
          </div>
          <h1 id="box-hero-title">
            {copy.heroLineOne}
            <span>{copy.heroLineTwo}</span>
          </h1>
          <p className="box-hero-subtitle">{copy.heroSubtitle}</p>
          <div className="box-hero-actions">
            <a
              className="box-button box-button--primary"
              href={docLink('/guide/quick-start.html')}
            >
              {copy.getStarted}
              <ArrowIcon />
            </a>
            <a
              className="box-button box-button--secondary"
              href="#runtime-features"
            >
              {copy.exploreFeatures}
              <ArrowIcon />
            </a>
            <a
              className="box-button box-button--secondary"
              href="#homepage-code-hike"
            >
              {copy.exploreCode}
              <ArrowIcon />
            </a>
          </div>

          <div className="box-install box-premium-surface">
            <div className="box-install-tabs" role="tablist">
              {installCommands.map((option) => (
                <button
                  key={option.id}
                  type="button"
                  role="tab"
                  aria-selected={selectedInstall === option.id}
                  className={
                    selectedInstall === option.id ? 'is-active' : undefined
                  }
                  onClick={() => {
                    setSelectedInstall(option.id);
                    setCopied(false);
                  }}
                >
                  {option.label}
                </button>
              ))}
            </div>
            <div className="box-command">
              <pre>{activeInstall.command}</pre>
              <button
                type="button"
                className="box-copy-button"
                onClick={copyInstallCommand}
                aria-label={copy.copyInstall}
              >
                {copied ? <CheckIcon /> : <CopyIcon />}
                {copied ? copy.copied : copy.copy}
              </button>
            </div>
          </div>
        </div>

        <div className="box-hero-visual" aria-label={copy.isolationAria}>
          <div className="box-runtime-window box-premium-surface">
            <header>
              <span className="box-runtime-status" />
              {copy.visual.resolver}
              <small>{copy.visual.localRuntime}</small>
            </header>
            <div className="box-request">
              <span>{copy.visual.request}</span>
              <strong>{copy.visual.requestTitle}</strong>
              <code>alpine:3.20 · 1 CPU · 512 MiB</code>
            </div>
            <div className="box-runtime-connector">
              <span>{copy.visual.resolve}</span>
            </div>
            <div className="box-policy">
              <span>{copy.visual.admission}</span>
              <strong>{copy.visual.noFallback}</strong>
              <div>
                <code>host</code>
                <code>isolation</code>
                <code>network</code>
                <code>storage</code>
              </div>
            </div>
            <div className="box-runtime-fork">
              <span>{copy.visual.default}</span>
              <span>{copy.visual.optIn}</span>
            </div>
            <div className="box-isolation-grid">
              <article className="box-isolation-card box-isolation-card--microvm">
                <span>{copy.visual.hardwareVm}</span>
                <strong>MicroVM</strong>
                <small>libkrun</small>
                <div>{copy.visual.dedicatedKernel}</div>
              </article>
              <article className="box-isolation-card box-isolation-card--sandbox">
                <span>{copy.visual.sharedKernel}</span>
                <strong>Sandbox</strong>
                <small>A3S OCI Runtime</small>
                <div>{copy.visual.sandboxBoundary}</div>
              </article>
            </div>
            <footer>
              <span>{copy.visual.generation}</span>
              <span>{copy.visual.durable}</span>
              <span>{copy.visual.localOnly}</span>
            </footer>
          </div>
        </div>
      </section>

      <section className="box-signal-strip" aria-label="Runtime summary">
        {copy.signals.map(([title, detail]) => (
          <div key={title}>
            <strong>{title}</strong>
            <span>{detail}</span>
          </div>
        ))}
      </section>

      <RuntimeFeatureShowcase
        locale={isChinese ? 'zh' : 'en'}
        platformHref={docLink('/reference/platforms.html')}
      />

      <AgentSkillSection
        guideHref={docLink('/guide/agent-skill.html')}
        locale={isChinese ? 'zh' : 'en'}
      />

      <section id="sdk-code-tour" className="box-section box-home-code-tour">
        <Content />
      </section>

      <section className="box-section box-principles">
        <div className="box-section-heading">
          <span>{copy.principleKicker}</span>
          <h2>{copy.principleTitle}</h2>
          <p>{copy.principleBody}</p>
        </div>
        <div className="box-principle-grid">
          {copy.principles.map((principle) => (
            <article className="box-premium-surface" key={principle.label}>
              <span>{principle.label}</span>
              <h3>{principle.title}</h3>
              <p>{principle.body}</p>
              <code>{principle.command}</code>
            </article>
          ))}
        </div>
      </section>

      <section className="box-section box-capabilities">
        <div className="box-section-heading box-section-heading--split">
          <div>
            <span>{copy.capabilityKicker}</span>
            <h2>{copy.capabilityTitle}</h2>
          </div>
          <p>{copy.capabilityBody}</p>
        </div>
        <div className="box-capability-grid">
          {copy.capabilities.map((card) => (
            <article
              key={card.index}
              className={`${card.className} box-premium-surface`}
            >
              <div className="box-card-index">{card.index}</div>
              <span>{card.eyebrow}</span>
              <h3>{card.title}</h3>
              <p>{card.body}</p>
              <div className="box-tags">
                {card.tags.map((tag) => (
                  <code key={tag}>{tag}</code>
                ))}
              </div>
            </article>
          ))}
        </div>
      </section>

      <section className="box-section box-sdks">
        <div className="box-section-heading">
          <span>{copy.sdkKicker}</span>
          <h2>{copy.sdkTitle}</h2>
          <p>{copy.sdkBody}</p>
        </div>
        <div className="box-sdk-grid">
          {sdkCards.map((sdk) => (
            <a
              className="box-premium-surface"
              key={sdk.language}
              href={docLink(sdk.href)}
            >
              <header>
                <span>{sdk.language}</span>
                <ArrowIcon />
              </header>
              <strong>{sdk.packageName}</strong>
              <p>{copy.sdkNotes[sdk.id]}</p>
              <code>{sdk.command}</code>
            </a>
          ))}
        </div>
      </section>

      <section className="box-section box-platforms">
        <div className="box-section-heading box-section-heading--split">
          <div>
            <span>{copy.platformKicker}</span>
            <h2>{copy.platformTitle}</h2>
          </div>
          <p>{copy.platformBody}</p>
        </div>
        <div className="box-platform-table">
          <div className="box-platform-row box-platform-row--header">
            {copy.platformHeaders.map((header) => (
              <span key={header}>{header}</span>
            ))}
          </div>
          {platformRows.map((row, index) => (
            <div className="box-platform-row" key={row.platform}>
              <strong>{row.platform}</strong>
              <code>{row.backend}</code>
              <span>{row.architecture}</span>
              <span>{copy.platformStatuses[index]}</span>
            </div>
          ))}
        </div>
        <a
          className="box-inline-link"
          href={docLink('/reference/platforms.html')}
        >
          {copy.platformLink}
          <ArrowIcon />
        </a>
      </section>

      <section className="box-cta">
        <div>
          <span>{copy.ctaKicker}</span>
          <h2>{copy.ctaTitle}</h2>
        </div>
        <div>
          <a
            className="box-button box-button--primary"
            href={docLink('/guide/quick-start.html')}
          >
            {copy.openQuickStart}
            <ArrowIcon />
          </a>
          <a
            className="box-button box-button--secondary"
            href="https://github.com/A3S-Lab/Box"
            target="_blank"
            rel="noreferrer"
          >
            {copy.viewSource}
          </a>
        </div>
      </section>
    </main>
  );
}
