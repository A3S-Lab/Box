type Locale = 'zh' | 'en';

type KernelSceneLabels = {
  comparison: string;
  tradeoff: string;
  sandbox: string;
  sharedMode: string;
  microvm: string;
  dedicatedMode: string;
  startup: string;
  lowerCost: string;
  higherCost: string;
  ready: string;
  agent: string;
  processBoundary: string;
  hostKernel: string;
  hostData: string;
  guestKernel: string;
  vmBoundary: string;
  sharedAttempt: string;
  hostRisk: string;
  microvmAttempt: string;
  blocked: string;
  poolHint: string;
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

type TeeSceneLabels = {
  setupCost: string;
  verifier: string;
  policy: string;
  report: string;
  verified: string;
  secret: string;
  protectedVm: string;
  workload: string;
  protectedMemory: string;
  hostView: string;
  ciphertext: string;
  boot: string;
  attest: string;
  release: string;
  run: string;
};

const benchmarkHref =
  'https://github.com/A3S-Lab/Box/blob/main/docs/ANNOUNCEMENT-v2.1.0.md';

const content = {
  zh: {
    kicker: '核心实现',
    title: '独立内核要多付启动成本；暖池让任务不用等待。',
    body: '共享内核进程启动更轻，但保留宿主内核攻击面。A3S Box 默认使用独立来宾内核；CoW 和暖池减少重复启动成本，符合条件的 SEV-SNP 主机还可在证明通过后注入密钥。',
    kernel: {
      label: '01 / 内核边界',
      title: '共享内核更轻；独立内核多一道硬件边界',
      body: 'Sandbox 用 namespace、seccomp、从属 ID 和 cgroup v2 隔离进程，因此不必启动来宾内核；但 Agent 与宿主机仍共用 Linux 内核。如果有效的宿主内核漏洞被利用，进程隔离可能失效。默认 MicroVM 为任务启动独立来宾内核，即使 Agent 取得来宾内核，仍需再跨过硬件 VM 边界。',
      boundary:
        '创建 MicroVM 还要启动 VMM、来宾内核和 guest init，耗时和内存开销高于共享内核进程。暖池和 snapshot-fork 可提前支付这部分成本；Hypervisor、硬件、挂载、网络和侧信道仍需审查。',
      command: 'a3s-box run --rm alpine:3.20 -- uname -a',
      scene: {
        comparison: '同一个 Agent，两种隔离路径',
        tradeoff: '启动开销与攻击路径',
        sandbox: 'SANDBOX',
        sharedMode: '共享内核',
        microvm: 'MICROVM',
        dedicatedMode: '独立内核',
        startup: '创建路径',
        lowerCost: '进程创建 · 较低开销',
        higherCost: 'VMM → 内核 → INIT · 较高开销',
        ready: 'READY',
        agent: 'AGENT',
        processBoundary: 'NAMESPACE + SECCOMP',
        hostKernel: '共享宿主 Linux 内核',
        hostData: '宿主机数据',
        guestKernel: '来宾 Linux 内核',
        vmBoundary: '硬件 VM 边界',
        sharedAttempt: '宿主内核漏洞利用成功时',
        hostRisk: '可能影响宿主机',
        microvmAttempt: '取得来宾内核后仍需 VM 逃逸',
        blocked: '在 VM 边界停下',
        poolHint: '暖池可预先完成启动',
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
    tee: {
      label: '04 / 机密计算',
      title: '先验证来宾，再把密钥送进去',
      body: '在符合条件的 AMD SEV-SNP 主机上，来宾先生成硬件证明。客户端或密钥服务按策略验证工作负载度量后，才通过 RA-TLS 把密钥送入来宾的受保护内存；该流程用于降低宿主侧直接读取来宾明文内存的风险。',
      boundary:
        'TEE 会增加受保护 VM 初始化、证明验证和密钥释放开销。当前仓库的单元与模拟测试只覆盖流程；没有真实 SEV-SNP 硬件证明证据就不作硬件安全声明。它也不防应用泄漏、拒绝服务或全部侧信道。',
      command: 'a3s-box attest secure-job --ratls --policy policy.json',
      scene: {
        setupCost: '额外路径 · 受保护启动 + 证明',
        verifier: '密钥服务',
        policy: '度量策略',
        report: '证明报告',
        verified: '验证通过',
        secret: '密钥',
        protectedVm: 'SEV-SNP MICROVM',
        workload: 'AGENT WORKLOAD',
        protectedMemory: '受保护的来宾内存',
        hostView: '宿主侧读取',
        ciphertext: '加密内存页',
        boot: '受保护启动',
        attest: '验证证明',
        release: '释放密钥',
        run: '开始执行',
      },
    },
    platformLink: '查看平台支持范围',
  },
  en: {
    kicker: 'CORE MECHANISMS',
    title:
      'A dedicated kernel costs more to start; a warm pool keeps it off the request path.',
    body: 'A shared-kernel process starts lighter but keeps the host-kernel attack surface. A3S Box defaults to a dedicated guest kernel; CoW and warm pools reduce repeated startup work, while qualifying SEV-SNP hosts can inject secrets after attestation.',
    kernel: {
      label: '01 / KERNEL BOUNDARY',
      title: 'Shared kernels start lighter; MicroVMs add a hardware boundary',
      body: 'A Sandbox uses namespace, seccomp, subordinate IDs, and cgroup v2 process isolation, so it does not boot a guest kernel. The agent still shares the host Linux kernel: if a working host-kernel exploit succeeds, process isolation may fail with it. The default MicroVM boots a dedicated guest kernel, so taking over that guest kernel still leaves a hardware VM boundary to cross.',
      boundary:
        'Creating a MicroVM has higher startup and memory cost because it starts a VMM, guest kernel, and guest init. Warm pools and snapshot-fork can pay that cost before a task arrives. The hypervisor, hardware, mounts, network exposure, and side channels still require review.',
      command: 'a3s-box run --rm alpine:3.20 -- uname -a',
      scene: {
        comparison: 'ONE AGENT · TWO ISOLATION PATHS',
        tradeoff: 'startup cost and attack path',
        sandbox: 'SANDBOX',
        sharedMode: 'shared kernel',
        microvm: 'MICROVM',
        dedicatedMode: 'dedicated kernel',
        startup: 'create path',
        lowerCost: 'process create · lower cost',
        higherCost: 'VMM → kernel → init · higher cost',
        ready: 'READY',
        agent: 'AGENT',
        processBoundary: 'namespace + seccomp',
        hostKernel: 'shared host kernel',
        hostData: 'host data',
        guestKernel: 'guest Linux kernel',
        vmBoundary: 'hardware VM boundary',
        sharedAttempt: 'if a host-kernel exploit succeeds',
        hostRisk: 'host may be affected',
        microvmAttempt: 'guest-kernel control still needs a VM escape',
        blocked: 'stopped at VM boundary',
        poolHint: 'a warm pool can boot this ahead of time',
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
    tee: {
      label: '04 / CONFIDENTIAL COMPUTING',
      title: 'Verify the guest before releasing a secret',
      body: 'On a qualifying AMD SEV-SNP host, the guest first produces hardware attestation evidence. A client or key service checks the workload measurement against policy, then sends secrets into protected guest memory over RA-TLS. This reduces the risk of the host directly reading guest plaintext memory.',
      boundary:
        'TEE adds protected-VM initialization, attestation, and key-release work. No hardware security claim is made without evidence from a qualifying SEV-SNP host; current unit and simulation tests cover the flow only. TEE does not stop application leaks, denial of service, or every side channel.',
      command: 'a3s-box attest secure-job --ratls --policy policy.json',
      scene: {
        setupCost: 'extra path · protected boot + attestation',
        verifier: 'KEY SERVICE',
        policy: 'measurement policy',
        report: 'attestation report',
        verified: 'verified',
        secret: 'secret',
        protectedVm: 'SEV-SNP MICROVM',
        workload: 'AGENT WORKLOAD',
        protectedMemory: 'protected guest memory',
        hostView: 'host memory view',
        ciphertext: 'encrypted pages',
        boot: 'protected boot',
        attest: 'verify evidence',
        release: 'release secret',
        run: 'run workload',
      },
    },
    platformLink: 'Check platform support',
  },
} as const;

function KernelBoundaryScene({ labels }: { labels: KernelSceneLabels }) {
  return (
    <div className="box-kernel-scene" aria-hidden="true">
      <header>
        <span>{labels.comparison}</span>
        <small>{labels.tradeoff}</small>
      </header>
      <div className="box-kernel-compare">
        <section className="box-kernel-lane box-kernel-lane--shared">
          <header>
            <span>{labels.sandbox}</span>
            <strong>{labels.sharedMode}</strong>
          </header>
          <div className="box-kernel-startup">
            <header>
              <span>{labels.startup}</span>
              <strong>{labels.ready}</strong>
            </header>
            <div>
              <i />
            </div>
            <small>{labels.lowerCost}</small>
          </div>
          <div className="box-kernel-stack">
            <div className="box-kernel-agent">
              <i />
              <strong>{labels.agent}</strong>
            </div>
            <div className="box-process-boundary">{labels.processBoundary}</div>
            <div className="box-shared-kernel">{labels.hostKernel}</div>
            <div className="box-kernel-host">{labels.hostData}</div>
            <div className="box-shared-risk-path">
              <i />
              <strong>{labels.hostRisk}</strong>
            </div>
          </div>
          <footer>{labels.sharedAttempt}</footer>
        </section>

        <section className="box-kernel-lane box-kernel-lane--microvm">
          <header>
            <span>{labels.microvm}</span>
            <strong>{labels.dedicatedMode}</strong>
          </header>
          <div className="box-kernel-startup">
            <header>
              <span>{labels.startup}</span>
              <strong>{labels.ready}</strong>
            </header>
            <div>
              <i />
            </div>
            <small>{labels.higherCost}</small>
          </div>
          <div className="box-kernel-stack">
            <div className="box-kernel-agent">
              <i />
              <strong>{labels.agent}</strong>
            </div>
            <div className="box-guest-kernel">{labels.guestKernel}</div>
            <div className="box-vm-boundary">
              <i />
              <span>{labels.vmBoundary}</span>
            </div>
            <div className="box-kernel-host">HOST · {labels.hostKernel}</div>
            <div className="box-microvm-risk-path">
              <i />
              <strong>{labels.blocked}</strong>
            </div>
          </div>
          <footer>
            {labels.microvmAttempt}
            <small>{labels.poolHint}</small>
          </footer>
        </section>
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

function TeeAttestationScene({ labels }: { labels: TeeSceneLabels }) {
  return (
    <div className="box-tee-scene" aria-hidden="true">
      <header>
        <span>SEV-SNP · RA-TLS</span>
        <small>{labels.setupCost}</small>
      </header>
      <div className="box-tee-flow">
        <section className="box-tee-verifier">
          <header>{labels.verifier}</header>
          <div className="box-tee-policy">
            <i />
            <span>{labels.policy}</span>
          </div>
          <div className="box-tee-verdict">
            <i />
            <strong>{labels.verified}</strong>
          </div>
          <div className="box-tee-key">
            <i />
            <span>{labels.secret}</span>
          </div>
        </section>

        <div className="box-tee-channel">
          <span className="box-tee-report-label">{labels.report}</span>
          <i className="box-tee-report-line" />
          <i className="box-tee-report-packet" />
          <span className="box-tee-secret-label">{labels.secret}</span>
          <i className="box-tee-secret-line" />
          <i className="box-tee-secret-packet" />
        </div>

        <section className="box-tee-guest">
          <header>
            <span>{labels.protectedVm}</span>
            <small>HARDWARE TEE</small>
          </header>
          <div className="box-tee-workload">
            <i />
            <strong>{labels.workload}</strong>
          </div>
          <div className="box-tee-memory">
            <span>{labels.protectedMemory}</span>
            <div>
              {Array.from({ length: 12 }, (_, index) => (
                <i key={index} />
              ))}
            </div>
          </div>
          <div className="box-tee-host-view">
            <span>{labels.hostView}</span>
            <strong>{labels.ciphertext}</strong>
            <i />
          </div>
        </section>
      </div>
      <footer>
        {[labels.boot, labels.attest, labels.release, labels.run].map(
          (step, index) => (
            <span key={step}>
              <i>{String(index + 1).padStart(2, '0')}</i>
              {step}
            </span>
          ),
        )}
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
        <article
          className="box-feature-row box-feature-row--kernel"
          id="kernel-boundary"
        >
          <div className="box-feature-copy">
            <span>{copy.kernel.label}</span>
            <h3>{copy.kernel.title}</h3>
            <p>{copy.kernel.body}</p>
            <small>{copy.kernel.boundary}</small>
            <code>{copy.kernel.command}</code>
          </div>
          <KernelBoundaryScene labels={copy.kernel.scene} />
        </article>

        <article
          className="box-feature-row box-feature-row--cow"
          id="copy-on-write"
        >
          <div className="box-feature-copy">
            <span>{copy.cow.label}</span>
            <h3>{copy.cow.title}</h3>
            <p>{copy.cow.body}</p>
            <small>{copy.cow.boundary}</small>
            <code>{copy.cow.command}</code>
          </div>
          <CowForkScene labels={copy.cow.scene} />
        </article>

        <article
          className="box-feature-row box-feature-row--pool"
          id="warm-pool"
        >
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

        <article
          className="box-feature-row box-feature-row--tee"
          id="confidential-computing"
        >
          <div className="box-feature-copy">
            <span>{copy.tee.label}</span>
            <h3>{copy.tee.title}</h3>
            <p>{copy.tee.body}</p>
            <small>{copy.tee.boundary}</small>
            <div className="box-feature-links">
              <a href={platformHref}>{copy.platformLink}</a>
            </div>
            <code>{copy.tee.command}</code>
          </div>
          <TeeAttestationScene labels={copy.tee.scene} />
        </article>
      </div>
    </section>
  );
}
