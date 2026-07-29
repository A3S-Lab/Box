import { useState } from 'react';
import { Content, useLang, withBase } from '@rspress/core/runtime';
import { AgentSkillSection } from './AgentSkillSection';
import { CanvasGridEffect } from './CanvasGridEffect';
import { PremiumInteractions } from './PremiumInteractions';

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
    eyebrow: 'OCI 工作负载运行时 · v3',
    heroLineOne: '让 Agent 任务',
    heroLineTwo: '运行于 MicroVM。',
    heroSubtitle:
      'A3S Box 是一个本地 OCI 运行时，将隔离策略作为每次请求的一部分。默认使用独立来宾内核；共享内核执行必须显式启用，并通过能力检查。',
    getStarted: '快速开始',
    installSkill: '安装 Agent Skill',
    exploreCode: '代码漫游',
    copy: '复制',
    copied: '已复制',
    copyInstall: '复制安装命令',
    isolationAria: 'A3S Box 隔离模型',
    visual: {
      resolver: '隔离解析器',
      localRuntime: '本地运行时',
      request: '请求',
      requestTitle: 'OCI 镜像 + 类型化策略',
      resolve: '解析主机能力',
      admission: '准入',
      noFallback: '不进行隐式降级',
      default: '默认',
      optIn: '显式启用',
      hardwareVm: '硬件虚拟机',
      dedicatedKernel: '独立来宾内核',
      sharedKernel: '共享内核',
      sandboxBoundary: '命名空间 + seccomp',
      generation: '代际隔离',
      durable: '持久状态',
      localOnly: '仅限本地',
    },
    signals: [
      ['独立内核', '默认隔离'],
      ['OCI 原生', '镜像与构建'],
      ['Agent Skill', 'A3S Code · Codex · Claude'],
      ['4 种 SDK', 'Rust · Go · Python · TypeScript'],
    ],
    principleKicker: '将隔离作为数据',
    principleTitle: '让执行边界在代码中清晰可见。',
    principleBody:
      'Box 会解析主机真正能够执行的能力，在修改状态前拒绝不兼容的组合，并为每个工作负载记录实际采用的隔离等级。',
    principles: [
      {
        label: '01 / 默认',
        title: 'MicroVM',
        body: '为不受信任的工作负载提供独立的来宾 Linux 内核，并在 KVM、HVF 或 WHPX 主机上形成更强的租户边界。',
        command: 'a3s-box run --rm alpine:3.20 -- uname -a',
      },
      {
        label: '02 / 显式',
        title: 'Sandbox',
        body: '面向可信自动化任务的 Linux 共享内核后端，仅在具备命名空间、seccomp、从属 ID 和 cgroup v2 的认证主机上使用。',
        command: 'a3s-box run --isolation sandbox ...',
      },
      {
        label: '03 / 契约',
        title: '拒绝降级',
        body: '不支持的隔离、网络、TEE、快照或主机组合会直接失败，不会静默削弱请求中的安全要求。',
        command: '请求 → 校验 → 持久化 → 启动',
      },
    ],
    capabilityKicker: '运行时工具箱',
    capabilityTitle: 'Docker 风格工作流，由一个本地状态所有者统一管理。',
    capabilityBody:
      '镜像、执行实例、网络、卷、快照、日志、策略和清理由同一个具备代际隔离能力的运行时管理。',
    capabilities: [
      {
        index: '01',
        eyebrow: 'OCI 镜像',
        title: '拉取、校验、构建、缓存和迁移 OCI 制品',
        body: '支持可续传分层拉取、仓库凭据、内容寻址 rootfs 缓存、多阶段构建、保存与加载，以及显式镜像生命周期管理。',
        tags: ['OCI', '构建', '缓存'],
        className: 'box-capability--wide box-capability--images',
      },
      {
        index: '02',
        eyebrow: '生命周期',
        title: '统一且具备代际隔离的执行模型',
        body: '通过同一个持久化本地状态所有者完成运行、创建、启动、停止、重启、检查、等待、附加和删除。',
        tags: ['状态', '健康检查', '日志'],
        className: 'box-capability--lifecycle',
      },
      {
        index: '03',
        eyebrow: '存储',
        title: '卷与文件系统快照',
        body: '组合使用绑定挂载、命名卷、tmpfs、复制、差异、导出、提交和停止态文件系统快照。',
        tags: ['卷', '快照', '差异'],
        className: 'box-capability--storage',
      },
      {
        index: '04',
        eyebrow: '网络',
        title: 'TSI、桥接网络与端口发布',
        body: '显式选择网络模式、命名桥接网络、DNS 别名、节点发现，以及具备代际隔离的 TCP 端口发布。',
        tags: ['TSI', 'DNS', '端口'],
        className: 'box-capability--network',
      },
      {
        index: '05',
        eyebrow: '自动化',
        title: '用于本地可编程基础设施的原生 SDK',
        body: 'Rust 直接调用运行时；Go、Python 和 TypeScript 通过同一个受检查的机器桥接协议和版本化能力握手访问运行时。',
        tags: ['Rust', 'Go', 'Python', 'TypeScript'],
        className: 'box-capability--wide box-capability--sdk',
      },
    ],
    sdkKicker: '原生 SDK',
    sdkTitle: '无需解析 CLI 输出，也能自动化同一个运行时。',
    sdkBody:
      '使用你熟悉的语言构建镜像、配置资源、启动沙箱、运行脚本、检查结果，并以确定性的方式完成清理。',
    sdkNotes: {
      rust: '直接访问类型化运行时',
      go: '支持 Context，且并发安全',
      python: '同步与异步 API',
      typescript: '面向 Node.js 20+ 的 Promise API',
    },
    platformKicker: '主机后端',
    platformTitle: '统一请求模型，明确平台边界。',
    platformBody:
      '公共契约保持稳定，各主机则报告自身真正能够执行的虚拟化、网络和来宾通道能力。',
    platformHeaders: ['主机', '虚拟机后端', '架构', '当前边界'],
    platformStatuses: [
      'MicroVM + 认证 Sandbox 主机',
      'MicroVM 运行时',
      'MicroVM 运行时（存在已记录限制）',
    ],
    platformLink: '查看完整平台矩阵',
    ctaKicker: '准备在本地运行？',
    ctaTitle: '从一条 OCI 命令开始，始终保持边界明确。',
    openQuickStart: '打开快速开始',
    viewSource: '查看源码',
  },
  en: {
    eyebrow: 'OCI WORKLOAD RUNTIME · v3',
    heroLineOne: 'Run agent workloads',
    heroLineTwo: 'inside MicroVMs.',
    heroSubtitle:
      'A3S Box is a local OCI runtime that makes isolation part of every request. Dedicated guest kernels are the default. Shared-kernel execution is an explicit, capability-checked opt-in.',
    getStarted: 'Get started',
    installSkill: 'Install Agent Skill',
    exploreCode: 'Code walkthrough',
    copy: 'Copy',
    copied: 'Copied',
    copyInstall: 'Copy install command',
    isolationAria: 'A3S Box isolation model',
    visual: {
      resolver: 'isolation resolver',
      localRuntime: 'local runtime',
      request: 'REQUEST',
      requestTitle: 'OCI image + typed policy',
      resolve: 'resolve capabilities',
      admission: 'ADMISSION',
      noFallback: 'No implicit fallback',
      default: 'default',
      optIn: 'explicit opt-in',
      hardwareVm: 'HARDWARE VM',
      dedicatedKernel: 'dedicated guest kernel',
      sharedKernel: 'SHARED KERNEL',
      sandboxBoundary: 'namespaces + seccomp',
      generation: 'generation fenced',
      durable: 'durable state',
      localOnly: 'local only',
    },
    signals: [
      ['Dedicated kernel', 'default isolation'],
      ['OCI-native', 'images and builds'],
      ['Agent Skill', 'A3S Code · Codex · Claude'],
      ['4 SDKs', 'Rust · Go · Python · TypeScript'],
    ],
    principleKicker: 'ISOLATION AS DATA',
    principleTitle: 'Make the execution boundary visible in code.',
    principleBody:
      'Box resolves what the host can actually enforce, rejects incompatible combinations before mutation, and records the effective isolation class with each workload.',
    principles: [
      {
        label: '01 / DEFAULT',
        title: 'MicroVM',
        body: 'A dedicated guest Linux kernel for untrusted workloads and stronger tenant boundaries on KVM, HVF, or WHPX hosts.',
        command: 'a3s-box run --rm alpine:3.20 -- uname -a',
      },
      {
        label: '02 / EXPLICIT',
        title: 'Sandbox',
        body: 'A shared-kernel Linux backend for trusted automation on certified hosts with namespaces, seccomp, subordinate IDs, and cgroup v2.',
        command: 'a3s-box run --isolation sandbox ...',
      },
      {
        label: '03 / CONTRACT',
        title: 'No fallback',
        body: 'Unsupported isolation, network, TEE, snapshot, or host combinations fail closed instead of silently weakening the request.',
        command: 'request → validate → persist → boot',
      },
    ],
    capabilityKicker: 'RUNTIME TOOLBOX',
    capabilityTitle: 'Docker-like workflows with one local state owner.',
    capabilityBody:
      'Images, executions, networks, volumes, snapshots, logs, policy, and cleanup all terminate at the same generation-fenced runtime.',
    capabilities: [
      {
        index: '01',
        eyebrow: 'OCI IMAGES',
        title: 'Pull, verify, build, cache, and move OCI artifacts',
        body: 'Use resumable layer pulls, registry credentials, content-addressed rootfs caching, multi-stage builds, save/load, and explicit image lifecycle controls.',
        tags: ['OCI', 'build', 'cache'],
        className: 'box-capability--wide box-capability--images',
      },
      {
        index: '02',
        eyebrow: 'LIFECYCLE',
        title: 'One generation-fenced execution model',
        body: 'Run, create, start, stop, restart, inspect, wait, attach, and remove through the same durable local state owner.',
        tags: ['state', 'health', 'logs'],
        className: 'box-capability--lifecycle',
      },
      {
        index: '03',
        eyebrow: 'STORAGE',
        title: 'Volumes and filesystem snapshots',
        body: 'Combine bind mounts, named volumes, tmpfs, copy, diff, export, commit, and stopped-filesystem snapshots.',
        tags: ['volumes', 'snapshot', 'diff'],
        className: 'box-capability--storage',
      },
      {
        index: '04',
        eyebrow: 'NETWORK',
        title: 'TSI, bridge networks, and published ports',
        body: 'Choose explicit network modes, named bridge networks, DNS aliases, peer discovery, and generation-fenced TCP publication.',
        tags: ['TSI', 'DNS', 'ports'],
        className: 'box-capability--network',
      },
      {
        index: '05',
        eyebrow: 'AUTOMATION',
        title: 'Native SDKs for local programmable infrastructure',
        body: 'Rust calls the runtime directly. Go, Python, and TypeScript use one checked machine bridge with a versioned capability handshake.',
        tags: ['Rust', 'Go', 'Python', 'TypeScript'],
        className: 'box-capability--wide box-capability--sdk',
      },
    ],
    sdkKicker: 'NATIVE SDKs',
    sdkTitle: 'Automate the same runtime without parsing CLI output.',
    sdkBody:
      'Build images, provision resources, start sandboxes, run scripts, inspect results, and clean up deterministically from your preferred language.',
    sdkNotes: {
      rust: 'Direct typed runtime access',
      go: 'Context-aware and concurrency-safe',
      python: 'Synchronous and asynchronous APIs',
      typescript: 'Promise APIs for Node.js 20+',
    },
    platformKicker: 'HOST BACKENDS',
    platformTitle: 'One request model, explicit platform boundaries.',
    platformBody:
      'The public contract stays stable while each host reports the virtualization, networking, and guest-channel capabilities it can enforce.',
    platformHeaders: ['Host', 'VM backend', 'Architecture', 'Current boundary'],
    platformStatuses: [
      'MicroVM + certified Sandbox hosts',
      'MicroVM runtime',
      'MicroVM runtime with documented limits',
    ],
    platformLink: 'Read the complete platform matrix',
    ctaKicker: 'READY TO RUN LOCALLY?',
    ctaTitle: 'Start with one OCI command. Keep the boundary explicit.',
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
            <a className="box-button box-button--secondary" href="#agent-skill">
              {copy.installSkill}
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
