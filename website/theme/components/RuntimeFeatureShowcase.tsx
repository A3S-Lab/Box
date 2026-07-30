type Locale = 'zh' | 'en';

type KernelSceneLabels = {
  host: string;
  hostKernel: string;
  hostData: string;
  vmBoundary: string;
  guestKernel: string;
  agent: string;
  attempt: string;
  blocked: string;
};

type CowSceneLabels = {
  template: string;
  readonlyPages: string;
  fork: string;
  shared: string;
  privateWrite: string;
};

type PoolSceneLabels = {
  requests: string;
  ready: string;
  running: string;
  replenish: string;
  idle: string;
  active: string;
  leased: string;
};

const benchmarkHref =
  'https://github.com/A3S-Lab/Box/blob/main/docs/ANNOUNCEMENT-v2.1.0.md';

const content = {
  zh: {
    kicker: '核心实现',
    title: '每个任务一个内核；重复任务不必每次冷启动。',
    body: '默认 MicroVM、写时复制和暖池是三套可以组合使用的机制。隔离负责挡住共享内核攻击面，CoW 减少重复数据，暖池把启动移出任务热路径。',
    kernel: {
      label: '01 / 独立内核',
      title: 'Agent 与宿主机不共享内核',
      body: '每个 Box 默认运行在自己的来宾 Linux 内核中。Agent 即使利用了来宾内核漏洞，仍需跨过硬件虚拟机边界才能接触宿主机；没有显式挂载的宿主目录也不会出现在来宾中。',
      boundary:
        'MicroVM 降低共享内核风险，但不能替代 Hypervisor、硬件和挂载策略本身的安全审查。',
      command: 'a3s-box run --rm alpine:3.20 -- uname -a',
      scene: {
        host: 'HOST',
        hostKernel: '宿主机内核',
        hostData: '宿主机数据',
        vmBoundary: '硬件 VM 边界',
        guestKernel: '来宾 Linux 内核',
        agent: 'AGENT',
        attempt: '内核越界尝试',
        blocked: '被 VM 边界阻断',
      },
    },
    cow: {
      label: '02 / 写时复制',
      title: '只为修改过的数据分配新页',
      body: 'Linux 文件系统恢复把同一份快照作为只读 lower，每个 Box 只保存自己的 upper。启用 Linux/KVM snapshot-fork 后，来宾 RAM 也从同一模板按 MAP_PRIVATE 恢复；未修改页继续共享，脏页才进入分叉副本。',
      boundary:
        '文件系统 CoW 依赖 Linux overlay；RAM CoW 需要支持原生快照的 Linux/KVM 主机，其他情况会使用完整复制或冷启动。',
      command: 'a3s-box snapshot restore checkpoint-1 --name test-2',
      scene: {
        template: '模板快照',
        readonlyPages: '只读 RAM + ROOTFS',
        fork: 'FORK',
        shared: '共享模板页',
        privateWrite: '自己的修改',
      },
    },
    pool: {
      label: '03 / 暖池',
      title: '任务到来时直接领取已就绪的 MicroVM',
      body: 'pool daemon 按镜像、CPU、内存和卷配置维护空闲实例。pool run 优先领取已启动的 VM；没有空闲实例时按需启动，后台再补到 min_idle。Linux/KVM 还可用 snapshot-fork 加快池填充。',
      boundary:
        '一次特定 /dev/kvm 主机实测中，预热池约 73 ms，冷启动约 1688 ms；这是历史实测，不是跨机器性能承诺。',
      command: 'a3s-box pool start --image node:24 --size 4 --snapshot-fork',
      benchmark: '查看实测条件',
      scene: {
        requests: '任务队列',
        ready: '已就绪',
        running: '执行中',
        replenish: '后台补池',
        idle: '空闲 3',
        active: '活跃 1',
        leased: '租约 0',
      },
    },
    platformLink: '查看平台支持范围',
    moreLabel: '同时提供',
    more: [
      ['OCI 构建', '多阶段 · 缓存 · save/load'],
      ['持久存储', '命名卷 · tmpfs · 快照'],
      ['网络', 'TSI · Bridge · TCP 端口'],
      ['可编程 CI/CD', '镜像 · 脚本 · 并行分叉'],
      ['Kubernetes', 'CRI · containerd shim'],
      ['机密计算', 'SEV-SNP · 证明 · 密钥注入'],
    ],
  },
  en: {
    kicker: 'CORE MECHANISMS',
    title: 'One kernel per task, without a cold boot for every repeat run.',
    body: 'Default MicroVMs, copy-on-write forks, and warm pools solve different parts of the runtime path. Isolation removes the shared-kernel attack surface, CoW avoids duplicate data, and the pool moves boot work out of the request path.',
    kernel: {
      label: '01 / DEDICATED KERNEL',
      title: 'The agent does not share the host kernel',
      body: 'Every Box runs with its own guest Linux kernel by default. Exploiting that guest kernel still leaves the hardware VM boundary between the agent and the host. Host directories are absent unless they are mounted explicitly.',
      boundary:
        'A MicroVM reduces shared-kernel risk; it does not replace review of the hypervisor, hardware, or mount policy.',
      command: 'a3s-box run --rm alpine:3.20 -- uname -a',
      scene: {
        host: 'HOST',
        hostKernel: 'host kernel',
        hostData: 'host data',
        vmBoundary: 'hardware VM boundary',
        guestKernel: 'guest Linux kernel',
        agent: 'AGENT',
        attempt: 'kernel escape attempt',
        blocked: 'stopped at VM boundary',
      },
    },
    cow: {
      label: '02 / COPY ON WRITE',
      title: 'Allocate new pages only for changed data',
      body: 'On Linux, filesystem restores share one read-only snapshot lower while each Box keeps its own upper. With Linux/KVM snapshot-fork enabled, guest RAM also restores from one MAP_PRIVATE template: untouched pages remain shared and dirty pages belong to the fork.',
      boundary:
        'Filesystem CoW requires Linux overlay. RAM CoW requires a snapshot-capable Linux/KVM host; other paths use a full copy or a cold boot.',
      command: 'a3s-box snapshot restore checkpoint-1 --name test-2',
      scene: {
        template: 'template snapshot',
        readonlyPages: 'read-only RAM + ROOTFS',
        fork: 'FORK',
        shared: 'shared template pages',
        privateWrite: 'private writes',
      },
    },
    pool: {
      label: '03 / WARM POOL',
      title: 'Lease a ready MicroVM when work arrives',
      body: 'The pool daemon keeps idle instances by image, CPU, memory, and volume shape. pool run takes a ready VM first, boots on demand after a miss, and replenishes min_idle in the background. Linux/KVM can also use snapshot-fork to fill the pool.',
      boundary:
        'One historical /dev/kvm host measured about 73 ms from a warm pool versus about 1688 ms cold. This is host-specific evidence, not a universal performance guarantee.',
      command: 'a3s-box pool start --image node:24 --size 4 --snapshot-fork',
      benchmark: 'Read the benchmark conditions',
      scene: {
        requests: 'request queue',
        ready: 'ready',
        running: 'running',
        replenish: 'replenish',
        idle: 'idle 3',
        active: 'active 1',
        leased: 'leased 0',
      },
    },
    platformLink: 'Check platform support',
    moreLabel: 'ALSO INCLUDED',
    more: [
      ['OCI builds', 'multi-stage · cache · save/load'],
      ['Persistent storage', 'volumes · tmpfs · snapshots'],
      ['Networking', 'TSI · bridge · TCP ports'],
      ['Programmable CI/CD', 'images · scripts · parallel forks'],
      ['Kubernetes', 'CRI · containerd shim'],
      ['Confidential workloads', 'SEV-SNP · attestation · secrets'],
    ],
  },
} as const;

function KernelBoundaryScene({ labels }: { labels: KernelSceneLabels }) {
  return (
    <div className="box-kernel-scene" aria-hidden="true">
      <header>
        <span>{labels.host}</span>
        <small>KVM · HVF · WHPX</small>
      </header>
      <div className="box-host-target">
        <span>{labels.hostKernel}</span>
        <strong>{labels.hostData}</strong>
        <i />
      </div>
      <div className="box-microvm-boundary">
        <span>{labels.vmBoundary}</span>
        <div className="box-guest-kernel">{labels.guestKernel}</div>
        <div className="box-agent-probe">
          <i />
          <strong>{labels.agent}</strong>
        </div>
        <div className="box-escape-trace">
          <span>{labels.attempt}</span>
          <i />
        </div>
        <div className="box-vm-barrier">
          <i />
          <strong>{labels.blocked}</strong>
        </div>
      </div>
    </div>
  );
}

function CowPageGrid({ dirty = [] }: { dirty?: number[] }) {
  return (
    <div className="box-cow-pages">
      {Array.from({ length: 12 }, (_, index) => (
        <i className={dirty.includes(index) ? 'is-dirty' : ''} key={index} />
      ))}
    </div>
  );
}

function CowForkScene({ labels }: { labels: CowSceneLabels }) {
  return (
    <div className="box-cow-scene" aria-hidden="true">
      <div className="box-cow-template">
        <header>
          <span>{labels.template}</span>
          <small>{labels.readonlyPages}</small>
        </header>
        <CowPageGrid />
      </div>
      <div className="box-cow-branches">
        <span>{labels.fork}</span>
        <i />
        <i />
        <i />
      </div>
      <div className="box-cow-forks">
        {[[1, 8], [4], [2, 6, 10]].map((dirty, index) => (
          <div key={index}>
            <header>
              <strong>BOX {String(index + 1).padStart(2, '0')}</strong>
              <small>{labels.shared}</small>
            </header>
            <CowPageGrid dirty={dirty} />
            <span>{labels.privateWrite}</span>
          </div>
        ))}
      </div>
    </div>
  );
}

function WarmPoolScene({ labels }: { labels: PoolSceneLabels }) {
  return (
    <div className="box-pool-scene" aria-hidden="true">
      <header>
        <span>pool.sock</span>
        <small>min_idle=3 · max=8</small>
      </header>
      <div className="box-pool-flow">
        <div className="box-pool-queue">
          <span>{labels.requests}</span>
          <i />
          <i />
          <i />
        </div>
        <div className="box-pool-request-packet">JOB</div>
        <div className="box-pool-slots">
          <span>{labels.ready}</span>
          {[0, 1, 2, 3].map((index) => (
            <div className={index === 1 ? 'is-active' : ''} key={index}>
              <i />
              <strong>VM {index + 1}</strong>
              <small>{index === 1 ? labels.running : labels.ready}</small>
            </div>
          ))}
          <div className="box-pool-replenish">{labels.replenish}</div>
        </div>
        <div className="box-pool-result">EXIT 0</div>
      </div>
      <footer>
        <span>{labels.idle}</span>
        <span>{labels.active}</span>
        <span>{labels.leased}</span>
      </footer>
    </div>
  );
}

export function RuntimeFeatureShowcase({
  locale,
  platformHref,
}: {
  locale: Locale;
  platformHref: string;
}) {
  const copy = content[locale];

  return (
    <section
      className="box-section box-runtime-features"
      id="runtime-features"
      aria-labelledby="runtime-features-title"
    >
      <div className="box-section-heading box-feature-heading">
        <span>{copy.kicker}</span>
        <h2 id="runtime-features-title">{copy.title}</h2>
        <p>{copy.body}</p>
      </div>

      <div className="box-feature-list">
        <article className="box-feature-row box-feature-row--kernel">
          <div className="box-feature-copy">
            <span>{copy.kernel.label}</span>
            <h3>{copy.kernel.title}</h3>
            <p>{copy.kernel.body}</p>
            <small>{copy.kernel.boundary}</small>
            <code>{copy.kernel.command}</code>
          </div>
          <KernelBoundaryScene labels={copy.kernel.scene} />
        </article>

        <article className="box-feature-row box-feature-row--cow">
          <div className="box-feature-copy">
            <span>{copy.cow.label}</span>
            <h3>{copy.cow.title}</h3>
            <p>{copy.cow.body}</p>
            <small>{copy.cow.boundary}</small>
            <code>{copy.cow.command}</code>
          </div>
          <CowForkScene labels={copy.cow.scene} />
        </article>

        <article className="box-feature-row box-feature-row--pool">
          <div className="box-feature-copy">
            <span>{copy.pool.label}</span>
            <h3>{copy.pool.title}</h3>
            <p>{copy.pool.body}</p>
            <small>{copy.pool.boundary}</small>
            <div className="box-feature-links">
              <a href={benchmarkHref} target="_blank" rel="noreferrer">
                {copy.pool.benchmark}
              </a>
              <a href={platformHref}>{copy.platformLink}</a>
            </div>
            <code>{copy.pool.command}</code>
          </div>
          <WarmPoolScene labels={copy.pool.scene} />
        </article>
      </div>

      <div className="box-feature-more">
        <header>{copy.moreLabel}</header>
        <div>
          {copy.more.map(([title, detail]) => (
            <article key={title}>
              <strong>{title}</strong>
              <span>{detail}</span>
            </article>
          ))}
        </div>
      </div>
    </section>
  );
}
