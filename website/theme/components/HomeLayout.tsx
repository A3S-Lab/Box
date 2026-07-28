import { useState } from 'react';
import { withBase } from '@rspress/core/runtime';

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

const capabilityCards = [
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
];

const sdkCards = [
  {
    language: 'Rust',
    packageName: 'a3s-box-sdk',
    command: 'cargo add a3s-box-sdk',
    href: '/sdk/rust.html',
    note: 'Direct typed runtime access',
  },
  {
    language: 'Go',
    packageName: 'sdk/go/v3',
    command: 'go get github.com/A3S-Lab/Box/sdk/go/v3',
    href: '/sdk/go.html',
    note: 'Context-aware and concurrency-safe',
  },
  {
    language: 'Python',
    packageName: 'a3s-box',
    command: 'python -m pip install a3s-box',
    href: '/sdk/python.html',
    note: 'Synchronous and asynchronous APIs',
  },
  {
    language: 'TypeScript',
    packageName: '@a3s-lab/box',
    command: 'npm install @a3s-lab/box',
    href: '/sdk/typescript.html',
    note: 'Promise APIs for Node.js 20+',
  },
];

const platformRows = [
  {
    platform: 'Linux',
    backend: 'KVM',
    architecture: 'x86_64 / arm64',
    status: 'MicroVM + certified Sandbox hosts',
  },
  {
    platform: 'macOS',
    backend: 'HVF',
    architecture: 'Apple Silicon',
    status: 'MicroVM runtime',
  },
  {
    platform: 'Windows',
    backend: 'WHPX',
    architecture: 'x86_64',
    status: 'MicroVM runtime with documented limits',
  },
];

function ArrowIcon() {
  return (
    <svg viewBox="0 0 24 24" aria-hidden="true">
      <path d="M5 12h14M13 6l6 6-6 6" />
    </svg>
  );
}

function GithubIcon() {
  return (
    <svg viewBox="0 0 24 24" aria-hidden="true">
      <path
        fill="currentColor"
        stroke="none"
        d="M12 .8a11.4 11.4 0 0 0-3.6 22.2c.6.1.8-.2.8-.5v-2.2c-3.3.7-4-1.4-4-1.4-.5-1.4-1.3-1.8-1.3-1.8-1.1-.7.1-.7.1-.7 1.2.1 1.8 1.2 1.8 1.2 1.1 1.8 2.8 1.3 3.5 1 .1-.8.4-1.3.8-1.6-2.7-.3-5.5-1.3-5.5-5.9 0-1.3.5-2.4 1.2-3.2-.1-.3-.5-1.6.1-3.2 0 0 1-.3 3.3 1.2a11.5 11.5 0 0 1 6 0c2.3-1.5 3.3-1.2 3.3-1.2.6 1.6.2 2.9.1 3.2.8.8 1.2 1.9 1.2 3.2 0 4.6-2.8 5.6-5.5 5.9.4.4.8 1.1.8 2.2v3.3c0 .3.2.6.8.5A11.4 11.4 0 0 0 12 .8Z"
      />
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
      <section className="box-hero" aria-labelledby="box-hero-title">
        <div className="box-hero-copy">
          <div className="box-eyebrow">
            <span />
            OCI WORKLOAD RUNTIME · v3
          </div>
          <h1 id="box-hero-title">
            Run agent workloads
            <span>inside MicroVMs.</span>
          </h1>
          <p className="box-hero-subtitle">
            A3S Box is a local OCI runtime that makes isolation part of every
            request. Dedicated guest kernels are the default. Shared-kernel
            execution is an explicit, capability-checked opt-in.
          </p>
          <div className="box-hero-actions">
            <a
              className="box-button box-button--primary"
              href={withBase('/guide/quick-start.html')}
            >
              Get started
              <ArrowIcon />
            </a>
            <a
              className="box-button box-button--secondary"
              href="https://github.com/A3S-Lab/Box"
              target="_blank"
              rel="noreferrer"
            >
              <GithubIcon />
              GitHub
            </a>
          </div>

          <div className="box-install">
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
                aria-label="Copy install command"
              >
                {copied ? <CheckIcon /> : <CopyIcon />}
                {copied ? 'Copied' : 'Copy'}
              </button>
            </div>
          </div>
        </div>

        <div className="box-hero-visual" aria-label="A3S Box isolation model">
          <div className="box-runtime-window">
            <header>
              <span className="box-runtime-status" />
              isolation resolver
              <small>local runtime</small>
            </header>
            <div className="box-request">
              <span>REQUEST</span>
              <strong>OCI image + typed policy</strong>
              <code>alpine:3.20 · 1 CPU · 512 MiB</code>
            </div>
            <div className="box-runtime-connector">
              <span>resolve capabilities</span>
            </div>
            <div className="box-policy">
              <span>ADMISSION</span>
              <strong>No implicit fallback</strong>
              <div>
                <code>host</code>
                <code>isolation</code>
                <code>network</code>
                <code>storage</code>
              </div>
            </div>
            <div className="box-runtime-fork">
              <span>default</span>
              <span>explicit opt-in</span>
            </div>
            <div className="box-isolation-grid">
              <article className="box-isolation-card box-isolation-card--microvm">
                <span>HARDWARE VM</span>
                <strong>MicroVM</strong>
                <small>libkrun</small>
                <div>dedicated guest kernel</div>
              </article>
              <article className="box-isolation-card box-isolation-card--sandbox">
                <span>SHARED KERNEL</span>
                <strong>Sandbox</strong>
                <small>A3S OCI Runtime</small>
                <div>namespaces + seccomp</div>
              </article>
            </div>
            <footer>
              <span>generation fenced</span>
              <span>durable state</span>
              <span>local only</span>
            </footer>
          </div>
        </div>
      </section>

      <section className="box-signal-strip" aria-label="Runtime summary">
        <div>
          <strong>Dedicated kernel</strong>
          <span>default isolation</span>
        </div>
        <div>
          <strong>OCI-native</strong>
          <span>images and builds</span>
        </div>
        <div>
          <strong>4 SDKs</strong>
          <span>one runtime contract</span>
        </div>
        <div>
          <strong>3 hosts</strong>
          <span>KVM · HVF · WHPX</span>
        </div>
      </section>

      <section className="box-section box-principles">
        <div className="box-section-heading">
          <span>ISOLATION AS DATA</span>
          <h2>Make the execution boundary visible in code.</h2>
          <p>
            Box resolves what the host can actually enforce, rejects
            incompatible combinations before mutation, and records the effective
            isolation class with each workload.
          </p>
        </div>
        <div className="box-principle-grid">
          <article>
            <span>01 / DEFAULT</span>
            <h3>MicroVM</h3>
            <p>
              A dedicated guest Linux kernel for untrusted workloads and
              stronger tenant boundaries on KVM, HVF, or WHPX hosts.
            </p>
            <code>a3s-box run --rm alpine:3.20 -- uname -a</code>
          </article>
          <article>
            <span>02 / EXPLICIT</span>
            <h3>Sandbox</h3>
            <p>
              A shared-kernel Linux backend for trusted automation on certified
              hosts with namespaces, seccomp, subordinate IDs, and cgroup v2.
            </p>
            <code>a3s-box run --isolation sandbox ...</code>
          </article>
          <article>
            <span>03 / CONTRACT</span>
            <h3>No fallback</h3>
            <p>
              Unsupported isolation, network, TEE, snapshot, or host
              combinations fail closed instead of silently weakening the
              request.
            </p>
            <code>request → validate → persist → boot</code>
          </article>
        </div>
      </section>

      <section className="box-section box-capabilities">
        <div className="box-section-heading box-section-heading--split">
          <div>
            <span>RUNTIME TOOLBOX</span>
            <h2>Docker-like workflows with one local state owner.</h2>
          </div>
          <p>
            Images, executions, networks, volumes, snapshots, logs, policy, and
            cleanup all terminate at the same generation-fenced runtime.
          </p>
        </div>
        <div className="box-capability-grid">
          {capabilityCards.map((card) => (
            <article key={card.index} className={card.className}>
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
          <span>NATIVE SDKs</span>
          <h2>Automate the same runtime without parsing CLI output.</h2>
          <p>
            Build images, provision resources, start sandboxes, run scripts,
            inspect results, and clean up deterministically from your preferred
            language.
          </p>
        </div>
        <div className="box-sdk-grid">
          {sdkCards.map((sdk) => (
            <a key={sdk.language} href={withBase(sdk.href)}>
              <header>
                <span>{sdk.language}</span>
                <ArrowIcon />
              </header>
              <strong>{sdk.packageName}</strong>
              <p>{sdk.note}</p>
              <code>{sdk.command}</code>
            </a>
          ))}
        </div>
      </section>

      <section className="box-section box-platforms">
        <div className="box-section-heading box-section-heading--split">
          <div>
            <span>HOST BACKENDS</span>
            <h2>One request model, explicit platform boundaries.</h2>
          </div>
          <p>
            The public contract stays stable while each host reports the
            virtualization, networking, and guest-channel capabilities it can
            enforce.
          </p>
        </div>
        <div className="box-platform-table">
          <div className="box-platform-row box-platform-row--header">
            <span>Host</span>
            <span>VM backend</span>
            <span>Architecture</span>
            <span>Current boundary</span>
          </div>
          {platformRows.map((row) => (
            <div className="box-platform-row" key={row.platform}>
              <strong>{row.platform}</strong>
              <code>{row.backend}</code>
              <span>{row.architecture}</span>
              <span>{row.status}</span>
            </div>
          ))}
        </div>
        <a
          className="box-inline-link"
          href={withBase('/reference/platforms.html')}
        >
          Read the complete platform matrix
          <ArrowIcon />
        </a>
      </section>

      <section className="box-cta">
        <div>
          <span>READY TO RUN LOCALLY?</span>
          <h2>Start with one OCI command. Keep the boundary explicit.</h2>
        </div>
        <div>
          <a
            className="box-button box-button--primary"
            href={withBase('/guide/quick-start.html')}
          >
            Open the quick start
            <ArrowIcon />
          </a>
          <a
            className="box-button box-button--secondary"
            href="https://github.com/A3S-Lab/Box"
            target="_blank"
            rel="noreferrer"
          >
            View source
          </a>
        </div>
      </section>
    </main>
  );
}
