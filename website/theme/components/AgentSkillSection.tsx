import { useState } from 'react';

type Locale = 'zh' | 'en';

type AgentSkillSectionProps = {
  guideHref: string;
  locale: Locale;
};

const installerUrl =
  'https://raw.githubusercontent.com/A3S-Lab/Box/main/integrations/skills/install.sh';

const installTargets = [
  {
    id: 'a3s-code',
    label: 'A3S Code',
    root: '~/.a3s/skills/a3s-box',
    command: `curl --proto '=https' --tlsv1.2 -fsSL ${installerUrl} | sh -s -- --home a3s-code`,
  },
  {
    id: 'codex',
    label: 'Codex',
    root: '~/.codex/skills/a3s-box',
    command: `curl --proto '=https' --tlsv1.2 -fsSL ${installerUrl} | sh -s -- --home codex`,
  },
  {
    id: 'claude',
    label: 'Claude Code',
    root: '~/.claude/skills/a3s-box',
    command: `curl --proto '=https' --tlsv1.2 -fsSL ${installerUrl} | sh -s -- --home claude`,
  },
  {
    id: 'all',
    label: 'All agents',
    root: '~/{.agents,.claude,.codex,.a3s}/skills/a3s-box',
    command: `curl --proto '=https' --tlsv1.2 -fsSL ${installerUrl} | sh -s -- --home all`,
  },
] as const;

const copy = {
  zh: {
    kicker: 'AGENT SKILL',
    title: '让 Agent 把不受信任的执行交给 Box。',
    body: '安装一次，即可让 A3S Code、Codex、Claude Code 等支持 Skill 的 Agent 理解 Box 的隔离模型、生命周期、网络、快照与故障恢复。',
    guide: '阅读完整 Skill 指南',
    consoleTitle: 'a3s-box · skill installer',
    consoleStatus: '远程副本',
    targetLabel: '安装目标',
    copyCommand: '复制 Skill 安装命令',
    copy: '复制',
    copied: '已复制',
    flowLabel: '三步接入',
    steps: [
      {
        label: '01 / INSTALL',
        title: '选择 Agent',
        body: '命令会将同一份 SKILL.md 写入所选用户级技能目录。',
      },
      {
        label: '02 / RELOAD',
        title: '重新加载 Agent',
        body: '重启会话或刷新 Skill 列表，让 Agent 发现 /a3s-box。',
      },
      {
        label: '03 / RUN',
        title: '描述隔离任务',
        body: 'Agent 会先检查主机能力，再使用明确的 Box 生命周期完成任务和清理。',
      },
    ],
    promptLabel: '示例请求',
    prompt:
      '在隔离的 MicroVM 中构建这个仓库并运行测试，保留失败日志，完成后清理临时 Box。',
    boundaryLabel: '工具边界',
    boundary:
      '在 Agent 执行 allowed-tools 的环境中，Skill 仅开放 a3s-box 与 curl 的 Shell 调用；文件读取仍由 Agent 自身权限控制。',
  },
  en: {
    kicker: 'AGENT SKILL',
    title: 'Give agents a safe place to execute untrusted work.',
    body: 'Install once so A3S Code, Codex, Claude Code, and other skill-aware agents understand Box isolation, lifecycle, networking, snapshots, and recovery.',
    guide: 'Read the complete Skill guide',
    consoleTitle: 'a3s-box · skill installer',
    consoleStatus: 'remote copy',
    targetLabel: 'INSTALL TARGET',
    copyCommand: 'Copy the Skill install command',
    copy: 'Copy',
    copied: 'Copied',
    flowLabel: 'THREE-STEP SETUP',
    steps: [
      {
        label: '01 / INSTALL',
        title: 'Choose an agent',
        body: 'The command writes the same SKILL.md into the selected user-level skill root.',
      },
      {
        label: '02 / RELOAD',
        title: 'Reload the agent',
        body: 'Restart the session or refresh skills so the agent discovers /a3s-box.',
      },
      {
        label: '03 / RUN',
        title: 'Describe isolated work',
        body: 'The agent checks host capabilities, then uses an explicit Box lifecycle and cleanup path.',
      },
    ],
    promptLabel: 'EXAMPLE REQUEST',
    prompt:
      'Build this repository and run its tests in an isolated MicroVM. Preserve failing logs and clean up the temporary Box.',
    boundaryLabel: 'TOOL BOUNDARY',
    boundary:
      'Where an agent enforces allowed-tools, the Skill limits shell calls to a3s-box and curl. File reads remain governed by the agent policy.',
  },
} as const;

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

export function AgentSkillSection({
  guideHref,
  locale,
}: AgentSkillSectionProps) {
  const labels = copy[locale];
  const [selectedTarget, setSelectedTarget] =
    useState<(typeof installTargets)[number]['id']>('a3s-code');
  const [copied, setCopied] = useState(false);
  const activeTarget =
    installTargets.find((target) => target.id === selectedTarget) ??
    installTargets[0];

  async function copyInstallCommand() {
    try {
      await navigator.clipboard.writeText(activeTarget.command);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1_400);
    } catch {
      setCopied(false);
    }
  }

  return (
    <section className="box-section box-agent-skill" id="agent-skill">
      <div className="box-section-heading box-section-heading--split">
        <div>
          <span>{labels.kicker}</span>
          <h2>{labels.title}</h2>
        </div>
        <div className="box-agent-skill-intro">
          <p>{labels.body}</p>
          <a className="box-inline-link" href={guideHref}>
            {labels.guide}
            <ArrowIcon />
          </a>
        </div>
      </div>

      <div className="box-agent-skill-stage">
        <div className="box-agent-skill-console box-premium-surface">
          <header>
            <div aria-hidden="true">
              <span />
              <span />
              <span />
            </div>
            <strong>{labels.consoleTitle}</strong>
            <small>{labels.consoleStatus}</small>
          </header>

          <div className="box-agent-skill-tabs" role="tablist">
            {installTargets.map((target) => (
              <button
                aria-controls="box-agent-skill-command"
                aria-selected={selectedTarget === target.id}
                className={
                  selectedTarget === target.id ? 'is-active' : undefined
                }
                id={`box-agent-skill-tab-${target.id}`}
                key={target.id}
                onClick={() => {
                  setSelectedTarget(target.id);
                  setCopied(false);
                }}
                role="tab"
                type="button"
              >
                {target.label}
              </button>
            ))}
          </div>

          <div
            aria-labelledby={`box-agent-skill-tab-${activeTarget.id}`}
            className="box-agent-skill-command"
            id="box-agent-skill-command"
            role="tabpanel"
          >
            <pre>
              <code>{activeTarget.command}</code>
            </pre>
            <button
              aria-label={labels.copyCommand}
              onClick={copyInstallCommand}
              type="button"
            >
              {copied ? <CheckIcon /> : <CopyIcon />}
              {copied ? labels.copied : labels.copy}
            </button>
          </div>

          <footer>
            <span>{labels.targetLabel}</span>
            <code>{activeTarget.root}</code>
          </footer>
        </div>

        <div className="box-agent-skill-flow">
          <span>{labels.flowLabel}</span>
          <ol>
            {labels.steps.map((step) => (
              <li className="box-premium-surface" key={step.label}>
                <span>{step.label}</span>
                <h3>{step.title}</h3>
                <p>{step.body}</p>
              </li>
            ))}
          </ol>
        </div>
      </div>

      <div className="box-agent-skill-example">
        <div>
          <span>{labels.promptLabel}</span>
          <code>/a3s-box</code>
          <p>{labels.prompt}</p>
        </div>
        <div>
          <span>{labels.boundaryLabel}</span>
          <p>{labels.boundary}</p>
        </div>
      </div>
    </section>
  );
}
