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
    title: '为编码 Agent 安装 A3S Box Skill。',
    body: 'Skill 向 A3S Code、Codex 和 Claude Code 提供 A3S Box 的命令、隔离模式、生命周期和清理规则。',
    guide: '阅读完整 Skill 指南',
    consoleTitle: 'a3s-box · skill installer',
    consoleStatus: '远程副本',
    targetLabel: '安装目标',
    copyCommand: '复制 Skill 安装命令',
    copy: '复制',
    copied: '已复制',
    flowLabel: '安装步骤',
    steps: [
      {
        label: '01 / INSTALL',
        title: '选择 Agent',
        body: '安装脚本会把 SKILL.md 写入对应的用户级 Skill 目录。',
      },
      {
        label: '02 / RELOAD',
        title: '重新加载 Skill',
        body: '重启会话或刷新 Skill 列表，然后确认 /a3s-box 可用。',
      },
      {
        label: '03 / RUN',
        title: '提交任务',
        body: '说明镜像、命令、文件和清理要求。Agent 会先检查本机能力。',
      },
    ],
    promptLabel: '示例请求',
    prompt:
      '在隔离的 MicroVM 中构建这个仓库并运行测试，保留失败日志，完成后清理临时 Box。',
    boundaryLabel: '工具边界',
    boundary:
      'Skill 只声明 a3s-box 和 curl 的 Shell 调用。最终可用工具仍由 Agent 的 allowed-tools 和文件权限决定。',
  },
  en: {
    kicker: 'AGENT SKILL',
    title: 'Install the A3S Box Skill for coding agents.',
    body: 'The Skill gives A3S Code, Codex, and Claude Code the A3S Box commands, isolation modes, lifecycle, and cleanup rules.',
    guide: 'Read the complete Skill guide',
    consoleTitle: 'a3s-box · skill installer',
    consoleStatus: 'remote copy',
    targetLabel: 'INSTALL TARGET',
    copyCommand: 'Copy the Skill install command',
    copy: 'Copy',
    copied: 'Copied',
    flowLabel: 'INSTALLATION',
    steps: [
      {
        label: '01 / INSTALL',
        title: 'Choose an agent',
        body: 'The installer writes SKILL.md to the selected user-level Skill directory.',
      },
      {
        label: '02 / RELOAD',
        title: 'Reload Skills',
        body: 'Restart the session or refresh the Skill list, then confirm that /a3s-box is available.',
      },
      {
        label: '03 / RUN',
        title: 'Submit a task',
        body: 'State the image, command, files, and cleanup requirements. The agent checks the local host first.',
      },
    ],
    promptLabel: 'EXAMPLE REQUEST',
    prompt:
      'Build this repository and run its tests in an isolated MicroVM. Preserve failing logs and clean up the temporary Box.',
    boundaryLabel: 'TOOL BOUNDARY',
    boundary:
      'The Skill declares only a3s-box and curl shell calls. The agent allowed-tools and file policy still decide what is available.',
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
