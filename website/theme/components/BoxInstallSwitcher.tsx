import { useState } from 'react';
import {
  siApple,
  siGo,
  siHomebrew,
  siLinux,
  siPython,
  siRust,
  siTypescript,
} from 'simple-icons';

type InstallSwitcherLabels = {
  copied: string;
  copy: string;
  tabs: string;
};

type BrandIcon = {
  color: string;
  path: string;
  title: string;
};

const windowsIcon: BrandIcon = {
  color: '#4ea4f6',
  path: 'M2 2h9v9H2V2Zm11 0h9v9h-9V2ZM2 13h9v9H2v-9Zm11 0h9v9h-9v-9Z',
  title: 'Windows',
};

const installTargets = [
  {
    id: 'unix',
    label: 'macOS / Linux',
    category: 'CLI',
    packageName: 'a3s-box',
    prompt: '$',
    icons: [
      { color: '#f2f4f7', path: siApple.path, title: siApple.title },
      { color: '#f2c94c', path: siLinux.path, title: siLinux.title },
    ],
    commands: [
      "curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/A3S-Lab/Box/main/install.sh | sh",
      'a3s-box version',
    ],
  },
  {
    id: 'windows',
    label: 'Windows',
    category: 'CLI',
    packageName: 'a3s-box',
    prompt: 'PS›',
    icons: [windowsIcon],
    commands: [
      'irm https://raw.githubusercontent.com/A3S-Lab/Box/main/install.ps1 | iex',
      'a3s-box version',
    ],
  },
  {
    id: 'homebrew',
    label: 'Homebrew',
    category: 'CLI',
    packageName: 'a3s-lab/tap/a3s-box',
    prompt: '$',
    icons: [
      { color: '#f7f0dd', path: siHomebrew.path, title: siHomebrew.title },
    ],
    commands: ['brew install a3s-lab/tap/a3s-box'],
  },
  {
    id: 'rust',
    label: 'Rust',
    category: 'SDK',
    packageName: 'a3s-box-sdk',
    prompt: '$',
    icons: [{ color: '#d7dde6', path: siRust.path, title: siRust.title }],
    commands: ['cargo add a3s-box-sdk'],
  },
  {
    id: 'go',
    label: 'Go',
    category: 'SDK',
    packageName: 'sdk/go/v3',
    prompt: '$',
    icons: [{ color: '#56c4dc', path: siGo.path, title: siGo.title }],
    commands: ['go get github.com/A3S-Lab/Box/sdk/go/v3'],
  },
  {
    id: 'python',
    label: 'Python',
    category: 'SDK',
    packageName: 'a3s-box',
    prompt: '$',
    icons: [{ color: '#4b8bbe', path: siPython.path, title: siPython.title }],
    commands: ['python -m pip install a3s-box'],
  },
  {
    id: 'typescript',
    label: 'TypeScript',
    category: 'SDK',
    packageName: '@a3s-lab/box',
    prompt: '$',
    icons: [
      {
        color: '#3178c6',
        path: siTypescript.path,
        title: siTypescript.title,
      },
    ],
    commands: ['npm install @a3s-lab/box'],
  },
] as const;

function InstallTargetIcon({ icons }: { icons: readonly BrandIcon[] }) {
  return (
    <span className="box-install-target-icons" aria-hidden="true">
      {icons.map((icon) => (
        <svg key={icon.title} viewBox="0 0 24 24">
          <path d={icon.path} fill={icon.color} />
        </svg>
      ))}
    </span>
  );
}

export function BoxInstallSwitcher({
  labels,
}: {
  labels: InstallSwitcherLabels;
}) {
  const [activeId, setActiveId] =
    useState<(typeof installTargets)[number]['id']>('unix');
  const [copied, setCopied] = useState(false);
  const active =
    installTargets.find((target) => target.id === activeId) ??
    installTargets[0];

  async function copyActiveCommand() {
    try {
      await navigator.clipboard.writeText(active.commands.join('\n'));
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1600);
    } catch {
      setCopied(false);
    }
  }

  function selectTarget(id: (typeof installTargets)[number]['id']) {
    setActiveId(id);
    setCopied(false);
  }

  return (
    <div className="box-install-switcher">
      <div className="box-install-console box-premium-surface">
        <div
          className="box-install-platforms"
          role="tablist"
          aria-label={labels.tabs}
        >
          {installTargets.map((target, index) => {
            const isActive = active.id === target.id;
            return (
              <button
                aria-controls="box-install-panel"
                aria-selected={isActive}
                className={isActive ? 'is-active' : undefined}
                id={`box-install-tab-${target.id}`}
                key={target.id}
                onClick={() => selectTarget(target.id)}
                onKeyDown={(event) => {
                  let nextIndex = index;
                  if (event.key === 'ArrowRight') nextIndex = index + 1;
                  if (event.key === 'ArrowLeft') nextIndex = index - 1;
                  if (event.key === 'Home') nextIndex = 0;
                  if (event.key === 'End') {
                    nextIndex = installTargets.length - 1;
                  }
                  if (nextIndex === index) return;

                  event.preventDefault();
                  const normalizedIndex =
                    (nextIndex + installTargets.length) % installTargets.length;
                  const nextTarget = installTargets[normalizedIndex];
                  selectTarget(nextTarget.id);
                  window.requestAnimationFrame(() => {
                    document
                      .getElementById(`box-install-tab-${nextTarget.id}`)
                      ?.focus();
                  });
                }}
                role="tab"
                tabIndex={isActive ? 0 : -1}
                type="button"
              >
                <InstallTargetIcon icons={target.icons} />
                <strong>{target.label}</strong>
              </button>
            );
          })}
        </div>

        <div
          aria-labelledby={`box-install-tab-${active.id}`}
          className="box-install-panel"
          id="box-install-panel"
          role="tabpanel"
        >
          <div className="box-install-panel-meta">
            <span>
              <strong>{active.packageName}</strong>
              <small>{active.category}</small>
            </span>
            <button
              aria-live="polite"
              className={
                copied ? 'box-install-copy is-copied' : 'box-install-copy'
              }
              onClick={copyActiveCommand}
              type="button"
            >
              <span aria-hidden="true">{copied ? '✓' : '⧉'}</span>
              {copied ? labels.copied : labels.copy}
            </button>
          </div>
          <div className="box-install-code" tabIndex={0}>
            {active.commands.map((command, index) => (
              <div key={`${active.id}-${index}`}>
                <span aria-hidden="true">{active.prompt}</span>
                <code>{command}</code>
              </div>
            ))}
          </div>
        </div>
      </div>
    </div>
  );
}
