export type RuntimeTerminalLocale = 'zh' | 'en';

type Localized = Record<RuntimeTerminalLocale, string>;

export type RuntimeTerminalLine = {
  label: Localized;
  value: Localized;
  tone: 'accent' | 'muted' | 'success' | 'warning';
};

export type RuntimeTerminalScenario = {
  id: string;
  label: string;
  command: string;
  summary: Localized;
  output: readonly RuntimeTerminalLine[];
};

export const runtimeTerminalScenarios: readonly RuntimeTerminalScenario[] = [
  {
    id: 'microvm',
    label: 'MICROVM',
    command: 'a3s-box run --rm alpine:3.20 -- uname -a',
    summary: {
      zh: '工作负载在独立来宾内核中完成',
      en: 'The workload completed inside a dedicated guest kernel',
    },
    output: [
      {
        label: { zh: '校验', en: 'validate' },
        value: {
          zh: '宿主能力与运行参数',
          en: 'host capabilities and runtime options',
        },
        tone: 'muted',
      },
      {
        label: { zh: '隔离', en: 'isolation' },
        value: {
          zh: 'MicroVM · 独立来宾内核',
          en: 'MicroVM · dedicated guest kernel',
        },
        tone: 'accent',
      },
      {
        label: { zh: '结果', en: 'result' },
        value: { zh: '任务退出码 0', en: 'workload exited 0' },
        tone: 'success',
      },
    ],
  },
  {
    id: 'sandbox',
    label: 'SANDBOX',
    command: 'a3s-box run --isolation sandbox --rm alpine:3.20 -- id',
    summary: {
      zh: 'Linux 共享内核模式只会在显式请求时使用',
      en: 'Linux shared-kernel mode is used only when explicitly requested',
    },
    output: [
      {
        label: { zh: '模式', en: 'mode' },
        value: {
          zh: '显式启用 · 仅 Linux',
          en: 'explicit opt-in · Linux only',
        },
        tone: 'warning',
      },
      {
        label: { zh: '边界', en: 'boundary' },
        value: {
          zh: 'namespace + seccomp + cgroup v2',
          en: 'namespace + seccomp + cgroup v2',
        },
        tone: 'accent',
      },
      {
        label: { zh: '内核', en: 'kernel' },
        value: {
          zh: '与宿主共享 · 适合可信任务',
          en: 'shared with host · trusted work',
        },
        tone: 'muted',
      },
    ],
  },
  {
    id: 'cow',
    label: 'CoW',
    command: 'a3s-box snapshot restore checkpoint-1 --name test-2',
    summary: {
      zh: '恢复实例共享只读模板，只为修改分配空间',
      en: 'The restored instance shares a read-only template and allocates writes',
    },
    output: [
      {
        label: { zh: '模板', en: 'template' },
        value: {
          zh: 'checkpoint-1 · 只读 lower',
          en: 'checkpoint-1 · read-only lower',
        },
        tone: 'accent',
      },
      {
        label: { zh: '写入', en: 'writes' },
        value: {
          zh: 'test-2 · 私有 upper',
          en: 'test-2 · private upper',
        },
        tone: 'muted',
      },
      {
        label: { zh: '内存', en: 'memory' },
        value: {
          zh: 'Linux/KVM 可选 snapshot-fork',
          en: 'optional snapshot-fork on Linux/KVM',
        },
        tone: 'success',
      },
    ],
  },
  {
    id: 'pool',
    label: 'WARM POOL',
    command: 'a3s-box pool start --image node:24 --size 4 --snapshot-fork',
    summary: {
      zh: '暖池提前准备 MicroVM，任务优先领取已就绪实例',
      en: 'The pool prepares MicroVMs ahead of time and leases ready instances first',
    },
    output: [
      {
        label: { zh: '配置', en: 'shape' },
        value: { zh: 'node:24 · pool size 4', en: 'node:24 · pool size 4' },
        tone: 'accent',
      },
      {
        label: { zh: '填充', en: 'fill' },
        value: {
          zh: 'Linux/KVM snapshot-fork',
          en: 'Linux/KVM snapshot-fork',
        },
        tone: 'muted',
      },
      {
        label: { zh: '状态', en: 'status' },
        value: { zh: '4 个实例已就绪', en: '4 instances ready' },
        tone: 'success',
      },
    ],
  },
  {
    id: 'tee',
    label: 'TEE',
    command: 'a3s-box attest secure-job --ratls --policy policy.json',
    summary: {
      zh: '合格的 SEV-SNP 主机可在证明通过后释放密钥',
      en: 'A qualifying SEV-SNP host can release secrets after attestation',
    },
    output: [
      {
        label: { zh: '范围', en: 'scope' },
        value: {
          zh: '需要合格的 AMD SEV-SNP 主机',
          en: 'qualifying AMD SEV-SNP host required',
        },
        tone: 'warning',
      },
      {
        label: { zh: '证明', en: 'attest' },
        value: {
          zh: '工作负载度量符合策略',
          en: 'workload measurement matches policy',
        },
        tone: 'accent',
      },
      {
        label: { zh: '密钥', en: 'secret' },
        value: {
          zh: '通过 RA-TLS 释放到来宾',
          en: 'released to the guest over RA-TLS',
        },
        tone: 'success',
      },
    ],
  },
];

export const runtimeTerminalInterfaceCopy = {
  zh: {
    region: 'A3S Box 核心能力终端演示',
    pause: '暂停终端演示',
    play: '继续终端演示',
    replay: '重播',
    replayLabel: '重新播放当前能力',
    reduced: '系统已启用减弱动画',
    ready: '完成',
    running: '执行中',
    paused: '已暂停',
    scenario: '选择核心能力演示',
  },
  en: {
    region: 'A3S Box core capability terminal playback',
    pause: 'Pause terminal playback',
    play: 'Resume terminal playback',
    replay: 'Replay',
    replayLabel: 'Replay the current capability',
    reduced: 'Reduced motion is enabled',
    ready: 'DONE',
    running: 'RUNNING',
    paused: 'PAUSED',
    scenario: 'Select a core capability demonstration',
  },
} as const;
