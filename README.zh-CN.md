<p align="center">
  <img src="assets/readme/hero.svg" width="100%" alt="A3S Box 将本地 OCI 工作负载解析到其请求的 MicroVM 或 Sandbox 隔离边界">
</p>

<p align="center">
  <strong>Language / 语言:</strong>
  <a href="README.md">English</a> ·
  <a href="README.zh-CN.md">中文</a>
</p>

<p align="center">
  <strong>面向 Linux OCI 工作负载的本地产品平面：Docker 式工作流、类型化 SDK，以及不会在背后悄悄改变的隔离。</strong>
</p>

<p align="center">
  <a href="https://github.com/A3S-Lab/Box/actions/workflows/ci.yml"><img alt="CI 状态" src="https://img.shields.io/github/actions/workflow/status/A3S-Lab/Box/ci.yml?branch=main&amp;style=flat-square&amp;label=CI"></a>
  <a href="https://github.com/A3S-Lab/Box/releases/latest"><img alt="最新 A3S Box 发布" src="https://img.shields.io/github/v/release/A3S-Lab/Box?display_name=tag&amp;sort=semver&amp;style=flat-square&amp;color=62d78b"></a>
  <a href="https://pypi.org/project/a3s-box/"><img alt="A3S Box Python 包" src="https://img.shields.io/pypi/v/a3s-box?style=flat-square&amp;color=3775a9"></a>
  <a href="https://www.npmjs.com/package/@a3s-lab/box"><img alt="A3S Box TypeScript 包" src="https://img.shields.io/npm/v/@a3s-lab/box?style=flat-square&amp;color=cb3837"></a>
  <a href="https://pkg.go.dev/github.com/A3S-Lab/Box/sdk/go/v3"><img alt="A3S Box Go 包" src="https://pkg.go.dev/badge/github.com/A3S-Lab/Box/sdk/go/v3.svg"></a>
  <a href="LICENSE"><img alt="MIT 许可证" src="https://img.shields.io/badge/license-MIT-f5b95f?style=flat-square"></a>
</p>

<p align="center">
  <a href="#从一个工作负载开始">开始</a> ·
  <a href="#有意选择边界">隔离</a> ·
  <a href="#box-拥有什么">能力</a> ·
  <a href="#一套状态模型四种原生-sdk">SDKs</a> ·
  <a href="#平台状态">平台</a> ·
  <a href="#架构当前与目标">架构</a> ·
  <a href="#开发">开发</a>
</p>

---

**A3S Box** 将镜像、命令与产品策略转化为本机上生命周期托管的工作负载。它拥有开发者体验与产品资源：镜像、构建、网络、卷、快照、健康、重启策略、日志与清理。

当前 3.2 执行模型有两条显式路径：

- 省略 `--isolation` 选择由 Box 通过 libkrun 管理的专用内核 MicroVM；
- `--isolation sandbox` 在合格的 Linux 主机上选择共享主机内核路径，生命周期执行委托给固定版本的
  [A3S OCI Runtime](https://github.com/A3S-Lab/OCI-Runtime) SDK。

两者之间没有静默回退。请求、已解析后端与策略会被持久化，因此重启恢复不能重新解释工作负载。

runtime crate 现在还暴露显式的 `OciMigrationPolicy` 与 `LocalExecutionBackendRouter`，用于分阶段切换。新记录在能力预检前被盖上 `box_vm` 或 `oci_sdk` 戳记，并在启动副作用前与预留一起持久化该选择。之后的策略变更不能重路由其生命周期、恢复或清理，且已选的 OCI 失败绝不会在 Box 后端上重试。在 Linux 上，CLI、machine bridge 与异步 Rust SDK 构造器默认将新的 Sandbox 记录纳入生产 bundle provider 与长期固定的 runtime 所有者（`SandboxViaOci`），无需设置 `A3S_BOX_OCI_MIGRATION`。显式 `off` 保留仅 VM 后端；显式 `sandbox`/`on` 在所有者未就绪时硬失败。默认无法启动所有者时，MicroVM 继续走遗留后端，Sandbox 预检失败即关闭。Windows x86_64 也有面向外部启动的 OCI Runtime WHPX 服务的显式资格验证-only `microvm`/`all` 组合；默认未启用，也尚非生产声明。

> **在找轻量 Agent sandbox？** 参见 [`a3s-sandbox`](https://github.com/A3S-Lab/Sandbox)。
> 该项目聚焦轻量跨平台命令沙箱；
> **A3S Box** 聚焦本地 OCI 工作负载、Docker 式生命周期管理，以及显式的 MicroVM 或共享内核隔离边界。

## 当前发布线

`3.2.5` 发布线在保持公共 SDK 契约稳定的同时，打包来自 `main` 的最新运行时与集成修复：

| 领域 | 最新行为 |
| --- | --- |
| Warm pools | SIGTERM/`SIGINT`/`pool stop` 以有界并发排空空闲 VM 与租约；销毁失败尽力回收孤儿（空闲、租约释放/过期、一次性 `pool run`、补充中途与模板拆除）；即使构建为 Failing/Unavailable，也会移除快照模板目录（`~/.a3s/pool/tpl-*`）。 |
| Linux Sandbox | Setuid OCI launcher 发现覆盖环境变量、打包路径与 `/usr/local/libexec/...`；在有委托用户 cgroup 时，前台 `run --rm --isolation sandbox` 在合格主机上干净完成。 |
| MicroVM lifecycle | 工作负载已退出时跳过 guest stop；Unix 冷启动在无 exec 心跳时失败即关闭；崩溃检测宽限为 80ms。 |
| CRI | PodSandbox 创建将 agent 工作负载推迟到 `StartContainer`；取消与销毁路径在 VM 拆除失败时尽力回收孤儿。 |
| Runtime builds | 仅 OCI 的构建在无 hypervisor 依赖的情况下保留持久清理与 socket 处理。 |
| Evidence | Soak 运行记录每能力结果与版本化主机资源采样；历史性能矩阵保留数字，同时记录哪些 Linux 清理阻塞被后续 tip 关闭。 |

安装程序、原生二进制以及 Rust、Python、TypeScript 与 Go SDK 产物从同一版本化发布标签发布。完整补丁历史见
[Changelog](CHANGELOG.md)。

> [!NOTE]
> 当前仅 SDK 的 Sandbox 适配器现覆盖五条精确代际轨道：
>
> - 版本化的 rootfs、挂载、网络、进程 I/O、密钥与扩展附件，带持久化清单摘要；
> - 保留内存的 pause/resume，加上捕获与流式 exec、stdin、游标检查输出、signal/wait、PTY resize、确切退出状态与有界超时清理；
> - 有界文件上传/下载，以及通过描述符受限运行时会话的文件系统 stat、递归 mkdir、移动、有界列举与递归删除；
> - 实时进程清单、规范化统计、有界有序事件，以及编译进完整 OCI 契约的可重放安全资源更新；
> - 将分离的 init stdout/stderr 投影到 Box 日志，以及只读与 PTY CLI attach，且无遗留 runtime-socket 回退。
>
> 调用经能力检查并绑定到确切运行时目标。文件与文件系统变更对显式可重试的丢失响应复用同一操作身份，并验证仅生效一次；读响应有大小边界，若目标或形状漂移则拒绝。资源意图在变更前持久化，并以同一操作身份恢复。快照 freezer 声明也持久化其运行时变更是否已应用，因此崩溃恢复从不重放已完成的 thaw，同时原始创建身份保持不可变。保留的本地 SDK 客户端现在暴露首个断开流结果，然后在后续显式对账时重连并重新协商。通用 Box/OCI 契约还在两个不同的 runtime-owner 测试进程间恢复一个 manager，恰好一次 create、start 与 exec；原始实时进程流与输入句柄通过替换所有者继续清单、stdin、输出、signal、wait 与清理。原始运行时输出与结构化 Box 日志保持分离。固定运行时资格验证现在在 Box SDK 套件运行前，对其真实原生与 utility-VM 驱动执行二进制文件传输与描述符受限的 mkdir/stat/list/move/remove。固定运行时现在提供长期多容器 Native Linux 主机所有者，Box 现在提供其生产直连进程 bundle 编译器、受保护的身份围栏所有者启动，以及显式 CLI/SDK 组合。在 bundle 构造前，资源守卫校验托管 home、持久附加产品卷与网络，并安装经验证的快照 lower，失败即关闭回滚。直连 SDK argv 命令在 OCI 调度前使用有效容器 `PATH`，对照已准备的 rootfs 解析 `argv[0]`，在不削弱运行时规范化绝对路径契约的前提下保留无 shell 的 `Argv("printf", ...)` 行为。阻塞式 Native Linux x86_64 与 aarch64 真实主机通道现在通过 Rust、Python、TypeScript 与 Go SDK 的生命周期、exec、文件系统、路由感知统计、pause/resume、快照恢复、重启与清理表面传递此生产所有者组合。两条通道在运行中的 Sandbox 下杀死确切已认证的 OCI 所有者，证明其 launcher 与 init 身份终止，并用新的 Box SDK-bridge 进程重新绑定所有者端点，将对账代际视为已停止且不捏造退出状态，删除其确切运行时墓碑，并重启下一 Box 与 OCI 代际。代际围栏的 Box worker 在 OCI init 启动前就绪，消费运行时的有序输出游标，写入常规拆分控制台文件，喂给配置的保留/脱敏驱动，在 runtime-service 所有者替换后重连，并在删除运行时代际前发布排空证据。
> **Sandbox（Native Linux）宣称面：** Linux 默认将新的 Sandbox 记录路由到生产 OCI 所有者（`SandboxViaOci`），无需 `A3S_BOX_OCI_MIGRATION`。托管 CI（x86_64/aarch64 SDK Local Sandbox）在未设置该变量时证明：生命周期、exec、文件系统、pause/resume、快照、重启、清理，以及 Native Live v4 在所有者 SIGKILL 后保留流句柄与文件系统连续性。显式 `off` 保留仅 VM 后端；显式 `sandbox` 在所有者未就绪时硬失败。主机 harness 报告仍保持 `b2_process_session_recovery_closed=false`（报告永不自证关闭 B2）。fixture `process_restart` 不是 driver Live 证据。
> **仍开放（不在 Sandbox GA 范围）：** 默认 MicroVM → OCI cutover、WHPX/KVM MicroVM *生产* 组合（资格验证-only 仍有效）、HostRuntimeService 作为默认 create 路径，以及更广的 Cloud `BX0.3` 硬件 TEE 宣称。省略 `--isolation` → MicroVM 的默认拆分仍具权威性，直至单独的 MicroVM cutover。
> 遵循[迁移路线图](ROADMAP.md)中已检查的门。

## 从一个工作负载开始

在 Linux 或 macOS 上安装稳定发布：

```bash
curl --proto '=https' --tlsv1.2 -fsSL \
  https://raw.githubusercontent.com/A3S-Lab/Box/main/install.sh | sh
```

在 Windows x86_64 上使用 PowerShell：

```powershell
irm https://raw.githubusercontent.com/A3S-Lab/Box/main/install.ps1 | iex
```

如有需要打开新终端，然后在启动前检查确切的主机能力：

```text
a3s-box --version
a3s-box info
```

运行一次性 Alpine 工作负载。省略隔离标志是有意的：

```bash
a3s-box run --rm alpine:3.20 -- sh -lc 'echo "inside $(uname -s)"; uname -r'
```

然后练习熟悉的长期运行生命周期：

```bash
a3s-box run -d --name web --memory 1g -p 8080:80 nginx:alpine
a3s-box ps
a3s-box logs -f web
a3s-box exec web -- nginx -v
a3s-box stop web
a3s-box rm web
```

安装程序在解压前验证发布 SHA-256，并拒绝不支持的架构或不安全的替换目标。固定版本、离线包、Homebrew、PATH 行为与卸载步骤见
[Installation](docs/installation.md)。

## 有意选择边界

| 契约 | 默认 MicroVM | 显式 Sandbox |
| --- | --- | --- |
| 请求 | 省略 `--isolation` | `--isolation sandbox` |
| 有效隔离 | `hardware-vm` | `shared-kernel` |
| 当前执行所有者 | Box → libkrun | Box → `a3s-oci-sdk` → 原生 Linux 服务 |
| 内核边界 | 专用 guest Linux 内核 | 共享主机 Linux 内核 |
| 合格主机 | Linux/KVM、Apple Silicon/HVF、下方平台门内的 Windows x86_64/WHPX | 经认证的 Linux x86_64/aarch64 主机 |
| 最适合 | 不信任工作负载与更强租户边界 | 信任或半信任工具、基准与自动化 |
| 仅 VM 功能 | TEE、warm pool、在合格处的 snapshot-fork | 拒绝 |
| 回退 | 从不 | 从不 |

显式 `--isolation microvm` 写法被拒绝。省略是选择默认的唯一公开方式，这可防止脚本将后端名称当作可互换的兼容模式。

在经认证的 Linux 主机上，显式请求共享内核 **Sandbox**。`--isolation sandbox`
默认走生产 OCI 所有者路由（见下节）。这不是预览 API：

```bash
a3s-box run --rm \
  --isolation sandbox \
  --cpus 2 \
  --memory 512m \
  alpine:3.20 -- sh -lc 'id; cat /proc/self/status'
```

### 在 Linux 上使用长期 OCI 所有者（Sandbox 生产路径）

Sandbox 生产激活是 Linux 上 `--isolation sandbox` 的默认（不是省略 `--isolation` / MicroVM
路径）。安装固定的 `a3s-oci` 与 `a3s-oci-agent` 对。打包二进制在发现路径上时，产物覆盖可选：

```bash
# 可选覆盖；省略 A3S_BOX_OCI_MIGRATION 即使用 Sandbox GA 默认。
export A3S_BOX_OCI_RUNTIME_PATH=/absolute/path/to/a3s-oci
export A3S_BOX_OCI_AGENT_PATH=/absolute/path/to/a3s-oci-agent
# Optional; the default is a short, per-UID/per-A3S-home directory under /tmp.
export A3S_BOX_OCI_HOST_ROOT=/absolute/private/runtime-root
# 无 OCI 主机准备的仅 MicroVM 主机逃生舱：
# export A3S_BOX_OCI_MIGRATION=off

a3s-box run --rm --isolation sandbox alpine:3.20 -- sleep 5
```

覆盖产物发现时，两个产物变量必须一起提供；每个可执行文件在所有者启动前以及 bundle 变更前都会做能力探测与 SHA-256 围栏。所有者根以模式 `0700` 创建；已有根必须是绝对规范化、真实、同 UID 且具有该确切模式的目录。仅当 PID 启动身份、端点、路径与摘要匹配时才会复用实时所有者。未知 socket 与漂移产物失败即关闭。

若此所有者被不干净地终止，其父绑定的 Native Linux 进程树一并终止。下一次*普通*
Box 操作启动不同的身份围栏所有者，将已认证的旧代际视为已停止，拒绝合成不可用的退出状态，仅移除该确切代际，并允许显式重启创建下一 Box 与 OCI 代际。此 **仅停止**
的崩溃恢复已在真实 x86_64 与 aarch64 Linux 主机上资格验证。

另外，Native Live v4 观察门（SDK Local Sandbox CI）在保留 Box manager 时证明
Host 所有者 SIGKILL 后的 **保留** 流式 exec 句柄与文件系统连续性 — 见英文 README
Native Live 小节。勿将仅停止对账与 Live 重附着混淆；二者皆真，且都不翻转
`b2_process_session_recovery_closed`。

Rust 应用通过 `A3sBoxClient::with_configured_paths(...).await` 选择同一路径，或显式构造 `NativeLinuxOciMigrationConfig`。同步的 `new`、`from_home` 与 `with_paths` 构造器为 API 兼容保留遗留行为。

### 在 Windows 上练习资格验证-only 的 WHPX 交接

用其 shim、受保护运行时根、utility-VM rootfs、状态根、命名管道与就绪文件启动固定的 OCI Runtime `box-whpx-qualification-service`。然后配置拥有测试记录的每个 Box 进程：

```powershell
$env:A3S_BOX_OCI_MIGRATION = 'microvm'
$env:A3S_BOX_OCI_HOST_ROOT = 'C:\absolute\a3s-oci-runtime-root'
$env:A3S_BOX_OCI_WHPX_ENDPOINT = '\\.\pipe\a3s-oci-box-qualification'

a3s-box run --rm --cpus 1 --memory 512m --network none alpine:3.20 -- /bin/true
```

对于确切产品门，下载 Box `windows-whpx` 产物以及固定的 OCI Runtime `windows-whpx-qualification` 与 `guest-agents-musl` 产物，保留每个产物的 `artifact-manifest.json`，然后运行：

```powershell
.\scripts\windows-whpx-oci-qualification.ps1 `
  -BoxArtifactDirectory C:\artifacts\box-windows `
  -OciWindowsArtifactDirectory C:\artifacts\oci-windows `
  -OciGuestArtifactDirectory C:\artifacts\oci-agents `
  -RootfsArchive C:\images\alpine-minirootfs-3.22.5-x86_64.tar.gz
```

运行器仅接受源提交匹配此 Box checkout 及其确切 OCI pin 的产物，要求两个 OCI bundle 来自同一次工作流运行，并重新检查每个大小与 SHA-256 摘要。它首先要求暂存的 `a3s-box.exe` 报告 `OCI symlink support: available`，并对缺失特权与 ACL 或端点保护拒绝分别记日志。然后通过命名管道服务在真实 WHPX 上练习可重放安全的 create、Box-manager reopen、start、wait、确切退出状态与 delete。`summary.json` 使用模式 `a3s.box.windows-whpx-oci-qualification-run.v1`，并与两个产物清单一起记录清理与进程清单。

首次产物绑定运行于 2026 年 8 月 4 日在真实 x86_64 Windows/WHPX 上通过，使用来自 CI 运行
[`30889251291`](https://github.com/A3S-Lab/Box/actions/runs/30889251291)
的 Box `52a2cfe4ee6693c9cc3a88df1b922bc1825b2deb`
以及来自主运行
[`30881404238`](https://github.com/A3S-Lab/OCI-Runtime/actions/runs/30881404238)
的固定 OCI Runtime `08c145d8ce5d06d5f28587226be822a2ab43b299`。
它观察到 `libkrun-whpx`/`dedicated-vm`、退出码 23、可重放安全的恢复与删除、完整生命周期目录清理，以及零残留 A3S 进程。此资格验证-only 组合仍为显式选择加入，默认未启用。

合并后主产物 `aaf9e615ee8bb5e22a5214ca09d7e426701f2d58`
（来自主 CI 运行
[`30898682738`](https://github.com/A3S-Lab/Box/actions/runs/30898682738)）
随后对照固定的 OCI Runtime 主产物通过了同一完整门。其清单绑定的 `a3s-box.exe` SHA-256 为
`31e98e73b325825bf1c49798cc9f51d744bf2502785b6651978947e312b5fa6b`。

此配置仅接受新鲜可写的 Linux amd64 rootfs、一个 vCPU、512 MiB、`network=none`，且无 TEE、主机挂载、卷、设备、sidecar、Snapshot、自定义安全控制或持久化。Box 将准备好的 rootfs 复制到确切操作范围的 SDK 交接，将其镜像元数据转换为 `a3s.oci.rootfs-metadata.v1`，发出相对 `rootfs` 且无用户命名空间的 OCI 规范，并原子发布 bundle。OCI Runtime 随后将该 bundle 移入确切的 WHPX 代际共享。缺失扩展支持或任何不合格选项在镜像准备前失败。此外，`run` 与 `create` 要求迁移路由器在命名卷创建或镜像缓存访问前观察到可启动的 `DedicatedVm` 驱动。端点必须显式提供，以便此实验服务绝不会意外激活。

当前 Linux 选择加入限制是有意的：仅新的 Sandbox 预留被路由到那里，且 `all`/MicroVM 迁移在 Linux 上被拒绝。镜像声明的匿名卷计划在能力预检后从规范化镜像元数据生成，持久化在初始 Box 预留中，并由确切执行在 bundle 准备期间原子认领。因此恢复与移除使用持久所有权，而非扫描主机目录。CLI `attach` 现在使用持久化的 OCI 路由：只读 attach 跟随代际围栏的 Box 控制台投影，而 `attach -t` 打开确切托管的 PTY 会话；任一路径都不回退到 Box 拥有的 runtime socket。Init stdout/stderr 由在 OCI init 前启动的分离 Box worker 投影，保留流顺序与分离，应用配置的日志策略，并在运行时删除前排空。CLI `top` 与 `stats` 使用该持久化 OCI 路由，包括对运行中或已暂停工作负载的确切代际进程调度与规范化 CPU/内存/PID/块 I/O 快照。CLI `cp` 对文件系统分类、有界单文件传输、目录归档执行与 Unix 权限恢复使用同一持久路由；OCI 路由失败绝不会对 Box 拥有的 socket 重试。实时 CLI `container-update` 现在通过确切持久化代际调度部分 cgroup 意图，复用中断/已完成的操作身份，并让托管生命周期在写入任何剩余 CLI 策略字段前原子应用并持久化资源意图。
类型化 SDK 的生命周期、exec/PTY、文件/文件系统、进程清单、统计、事件、资源更新、pause/resume、wait、重启与清理契约确实路由到确切 OCI 代际。

> [!IMPORTANT]
> 共享内核 Sandbox 不能防御可工作的主机内核漏洞利用、敌对主机管理员、硬件侧信道，或通过 bind mount 故意暴露的数据。当这些风险重要时，使用默认 MicroVM 边界。

完整准入规则与威胁模型见
[Host Sandbox Backend Design](docs/host-sandbox-backend-design.md)。

## Box 拥有什么

Box 类似 Docker，但不与 Docker 完全相同。不支持的控制在运行时变更前失败，而不是被存储并静默削弱。

| 产品领域 | 当前表面 |
| --- | --- |
| 工作负载 | create、start、stop、restart、kill、pause、wait、inspect、exec、attach、PTY、实时进程清单、健康与重启策略 |
| 镜像与构建 | pull、push、tag、save/load、经验证层、选定的 Dockerfile/Containerfile 构建、内容寻址缓存与签名镜像策略 |
| 存储 | bind mount、命名卷、tmpfs、copy、diff、export、commit、文件系统快照与写时复制恢复 |
| 网络与 Compose | TSI、命名桥、对等发现、TCP 发布、Sandbox 与 MicroVM 上代际围栏的 Runtime Service 转发，以及有界 ACL/YAML Compose 子集与有界并发镜像预取 |
| 运维 | 结构化日志、规范化运行时统计、有序事件、审计证据、指标、监控、可重放安全资源更新与清理 |
| 加速与安全 | rootfs/层缓存、warm pool、可选 Linux/KVM snapshot-fork，以及主机门控的 SEV-SNP 导向工作流 |

在 macOS 上，MicroVM 默认使用 guest 原生 ext4 rootfs。Box 将经验证的 OCI 层直接组装到固定、已验证的 ext4 基座，并为每个 box 发布私有 raw 磁盘；启用不可变产物缓存时使用写时复制克隆。新代际不创建 guest 命名的主机目录，也不附加 `A3SRootfs` DiskImage。
原始字节逻辑组装器在调用经审计、可单独发布的 `a3s-box-mkext4` 写入器之前，保留 Linux 文件名、符号链接目标、硬链接、whiteout、xattr、所有权、模式与时间戳。macOS 暂存编解码器仅用于目录传输与遗留 APFS 迁移；当 guest 名称无法无损表示时，目录传输失败即关闭。
启动配置与确切工作负载退出状态使用私有 guest 控制交接，而非主机访问活动根磁盘。第一个原始 `diff` 基线同样在工作负载启动前从 guest 可见的 Linux 元数据捕获，并由主机原子发布；之后的启动不再重扫 rootfs。持久 box 在 PID 1 刷新、只读重新挂载并确认交接后复用确切由 guest 写入的 raw 磁盘。保留的 raw 代际在每次重启时仍具权威性，且不能静默降级为目录传输。主机或不干净的 shim 退出后，运行时校验固定的 ext4 恢复信封，并让 guest 内核重放其日志；它不会在 macOS 上挂载或解析未重放的 guest 元数据。干净停止的 box 通过一次性、无网络的维护 MicroVM 提供 `diff`、`export` 与 `commit`。其当前受信任的 guest-init 从临时目录根启动，只读附加用户磁盘，以 `ro,noload` 挂载，仅暴露归档、心跳与关机控制，并在释放生命周期锁前拆除。日志脏磁盘被拒绝，直到正常可写启动与干净停止完成恢复。遗留转换记录为持久的 `building → artifact_ready → clean_stop_verified` 事务。验证后旧稀疏镜像保持分离作为回滚证据；Box 不会静默删除它。已停止的文件系统快照现在将干净的 raw ext4 代际克隆到版本化、完整性检查的 bundle，并恢复私有可写克隆；创建、恢复与之后的快照删除无需 macOS 挂载，也从不让恢复后的 box 依赖快照存储。
Libkrun 内存 snapshot-fork 仍是可选的 Linux x86_64/KVM 能力。不支持的主机在镜像、RAM、box 或 rootfs 副作用前拒绝其显式状态输入，而 warm pool 冷启动且不尝试快照。
`A3S_BOX_MACOS_LEGACY_APFS_ROOTFS=1` 是创建或在发布期间保留新 APFS 代际的窄范围兼容覆盖；它从不覆盖已有的 raw 代际。参见 [Guest-Native Rootfs Design](docs/guest-native-rootfs-design.md)。

一些端到端工作流：

```bash
# Build and run
a3s-box pull alpine:3.20
a3s-box build -t local/app:dev .
a3s-box run -d --name app local/app:dev

# Durable data and a stopped-filesystem snapshot
a3s-box volume create data
a3s-box run -d --name data-app -v data:/data alpine:3.20 -- sleep 3600
a3s-box stop data-app
a3s-box snapshot create data-app --name checkpoint-1

# Named networking and deterministic Compose normalization
a3s-box network create backend --subnet 10.89.0.0/24
a3s-box compose -f compose.acl config
a3s-box compose -f compose.acl up -d
```

Compose 从 Compose 文件所在目录解析相对 bind mount，因此可从另一工作目录调用 `-f /path/to/compose.yaml`。
分离的 CLI 健康 worker 使用代际围栏、独立的 Unix 会话；其探测在启动终端或作业进程组清理后仍存活。

Compose 可投影调用方拥有的进程环境值，而不将其字节放入 ACL、`.env`、`BoxConfig`、标签或状态记录：

```acl
service "api" {
  image = "ghcr.io/example/api:v1"

  secret_environment = {
    DATABASE_URL = "A3S_CLOUD_POSTGRES_URL"
  }
}
```

`secret_environment` 将 guest 变量映射到真实进程环境变量的名称。在 Linux 上，Box 校验已有的私有 `<A3S_HOME>/runtime-secrets` tmpfs，在那里物化值，只读挂载，并随 box 移除。Box 从不创建或将支撑 tmpfs 降级到磁盘；当挂载或源变量不可用时，密钥支撑的 Compose 启动在资源变更前失败。`.env` 与 `env_file` 仍是字面配置输入，从不是 Secret 源。其他主机解析并规范化引用，但拒绝其执行。

### 运行独立的 Gateway 扩缩权威

`scale-api` 将 Gateway 副本决策转化为持久的 Box 执行。服务目录由 Box 拥有的 ACL 定义，且除非提供目录，命令失败即关闭：

```acl
service "api" {
  image       = "ghcr.io/example/api:v1"
  command     = ["serve", "--port", "8080"]
  ports       = ["0:8080"]
  cpus        = 2
  mem_limit   = "768m"
  environment = { MODE = "production" }
}
```

```bash
a3s-box scale-api \
  --address 127.0.0.1:9090 \
  --state "$HOME/.a3s/scale-authority.json" \
  --services ./scale-services.acl \
  --endpoint-drain-timeout-secs 3
```

该权威在收敛确定性副本槽之前，通过 CLI 与 SDK 使用的同一本地执行管理器，记录 compare-and-set 修订与操作收据。重启恢复采纳已有副本而不是创建重复。单个 `0:<guest-port>` 映射声明运行时发现的 HTTP 端点：Box 探测确切执行代际，租用主机 TCP 中继，并仅在该副本就绪时通过 `GET /v1/scale/{service}` 发布实时 URL。固定或多端口映射、`depends_on`、卷与显式 Compose 网络被拒绝；无端口的模板对提供自身稳定流量端点的部署仍有效。

缩容期间，Gateway 在发送变更前从原子后端快照中移除退役副本槽。Box 随后关闭每个退役监听器，让已建立的中继连接完成，并仅在中继集合为空或有界排空截止到期后移除执行。`--endpoint-drain-timeout-secs` 默认为 3 秒，接受 1–300 秒；截止时仍打开的连接被强制关闭。

端点监听器默认为 loopback。当 Gateway 在另一受信任主机上运行时，用 `--endpoint-bind-address` 绑定私有接口，并通过 `--endpoint-advertise-host` 提供可达的 DNS 名或 IP；未指定绑定地址且无显式通告主机被拒绝。扩缩 API 与中继端口没有公开认证边界，必须留在受信任节点/私有网络上。本地执行端口中继当前需要 Linux；含端点的模板在其他主机上显式失败。`--desired-state-only` 是显式诊断/迁移模式，不启动工作负载。

<details>
<summary><strong>CLI 命令图</strong></summary>

| 领域 | 命令 |
| --- | --- |
| Lifecycle | `run`, `create`, `start`, `stop`, `restart`, `rm`, `kill`, `pause`, `unpause`, `wait`, `rename`, `prune` |
| Execution | `exec`, `shell`, `attach`, `top` |
| Images and builds | `pull`, `push`, `build`, `images`, `rmi`, `tag`, `image-inspect`, `history`, `image-prune`, `save`, `load`, `import` |
| Filesystems | `cp`, `diff`, `export`, `commit`, `volume`, `snapshot` |
| Networking | `network`, `port`, `port-forward`, `compose` |
| Security and TEE | `attest`, `seal`, `unseal`, `inject-secret` |
| Observability | `ps`, `logs`, `inspect`, `stats`, `events`, `df`, `audit`, `monitor` |
| System | `scale-api`, `container-update`, `system-prune`, `pool`, `login`, `logout`, `version`, `info` |

</details>

## 一套状态模型，四种原生 SDK

Rust、Python、TypeScript 与 Go 操作与 CLI 相同的本地资源与持久状态。它们不暴露远程端点、域名或 API 密钥设置。

| 语言 | 安装 | 运行时访问 | 指南 |
| --- | --- | --- | --- |
| Rust | `cargo add a3s-box-sdk` | 直接类型化调用进入运行时与代际围栏执行管理器 | [Rust SDK](src/sdk/README.md) |
| Python | `python -m pip install a3s-box` | 通过已安装 machine bridge 的同步与异步 API | [Python SDK](sdk/python/README.md) |
| TypeScript | `npm install @a3s-lab/box` | 通过已安装 machine bridge 的 Promise API；Node.js 20+ | [TypeScript SDK](sdk/typescript/README.md) |
| Go | `go get github.com/A3S-Lab/Box/sdk/go/v3` | 通过已安装 machine bridge 的 Context 感知 API；Go 1.25+ | [Go SDK](sdk/go/README.md) |

Python、TypeScript 与 Go 与 `a3s-box sdk-bridge` 交换结构化 protocol-v3 消息；它们从不解析人类 CLI 输出。确切的 52 操作握手在缺失、重复、畸形或不兼容能力时失败即关闭。参见
[跨语言 SDK 契约](docs/sdk-api-and-programmable-cicd.md)。

全部四个 SDK 还暴露相同的有界单文件产物导出：调用方选择的上限直至传输安全的 8 MiB 单帧上限、后端有界读取、stat/read 大小校验、小写 SHA-256 摘要，以及从不覆盖已有路径的可选独占主机文件创建。MicroVM guest 在读取前强制执行所选限制；共享内核执行保留 OCI Runtime 传输上限，并拒绝超出所选限制的响应。

全部四个 SDK 暴露确切代际进程清单、规范化运行时统计、有界有序事件轮询，以及可重放安全的实时资源更新。语言原生名称为 `processes`、`runtime_stats`/`runtimeStats`/`RuntimeStats`、`events`/`Events` 与 `update_resources`/`updateResources`/`UpdateResources`。未宣告匹配运行时操作的后端在调度前返回类型化可用性错误。

## 平台状态

| 路径 | 当前证据 | 仍可见的边界 |
| --- | --- | --- |
| Linux MicroVM | 经 KVM/libkrun 的主要本地路径；Runtime 0.5 就绪/存活与有界优雅停止用例已接线到已宣告的提供方配置文件，与自托管生命周期、SDK、CRI、竞态、泄漏、snapshot-fork 与 soak 门并列 | 当前修订仍要求一次已登记的 KVM 运行覆盖所有能力触发的生命周期用例，加上更长的 `G2`/`R24` 配置文件 |
| macOS MicroVM | Apple Silicon/HVF 构建与打包路径，加上物理持久/崩溃恢复、无挂载文件系统快照、遗留迁移、维护与已发布端口回归门 | [`integration-hvf` 门](docs/ci-hvf-runner.md) 需要已登记的物理 Apple Silicon runner；Intel macOS 不受支持 |
| Windows MicroVM | 覆盖生命周期、exec、copy、stats、端口、bind/命名卷、commit、快照与清理的真实 x86_64 WHPX soak | 一个 vCPU；无交互 PTY、桥接网络、TEE、snapshot-fork 或 CRI |
| Linux Sandbox | 已安装、自包含的 x86_64/aarch64 产品包运行每个 A3S OCI Runtime 配置文件，以及在 `/dev/kvm` 缺失与不可访问时的 Rust、Python、TypeScript 与 Go SDK 生命周期；Runtime 0.5 生命周期与 Native Live v4 在未设置 `A3S_BOX_OCI_MIGRATION` 时使用生产所有者路由（Sandbox GA 默认） | **生产** 共享内核路径（`--isolation sandbox`；非默认省略 isolation）。仅 VM 控制被拒绝。主机报告保持 `b2_process_session_recovery_closed=false`。不是 MicroVM/TEE/`BX0.3` 宣称。 |
| Kubernetes | CRI v1 服务器与 containerd runtime-v2 shim 预览 | 不声明完整 CRI 符合性 |
| TEE | 运行时绑定的 RA-TLS 产物、确切身份附件绑定、机密 Tasks 与 Services 的执行前证明，以及可选的模拟 KVM 符合性配置文件；单独武装的 SEV-SNP 硬件门固定启动测量 | 身份附件仅由显式配置的机密提供方宣告；模拟与未执行的硬件作业不是硬件安全证据 |

真实主机证据刻意与单元、仅构建、夹具或模拟结果分离。在推广部署前审查 [Host Integration](docs/host-integration.md)、
[Cross-Capability Soak Tests](docs/soak-test-plan.md) 与
[CRI Conformance](docs/cri-conformance.md)。

Windows 主机还必须遵循 [WHPX 设置指南](docs/windows-whpx.md)。

## 架构：当前与目标

每个已交付入口到达一个后端中立的本地 `ExecutionManager`：

```text
CLI · Rust · Python · TypeScript · Go · Compose · CRI · containerd shim
                                  │
                         ExecutionManager
                  desired state · generations · policy
                    ┌─────────────┴─────────────┐
                    │                           │
         images · builds · storage      isolation resolver
        networks · logs · health        ┌───────┴────────┐
                                       │                │
                           current MicroVM      current Sandbox
                           Box + libkrun        a3s-oci-sdk
                           dedicated kernel    shared host kernel
```

目标依赖方向移除直接执行拆分：

```text
A3S Box product plane
        │  prepared OCI bundle + desired isolation
        ▼
a3s-oci-sdk over bounded local IPC
        ▼
A3S OCI Runtime host service
        ├── native Linux driver
        └── KVM / HVF / WHPX utility-VM drivers
```

Box 仍是产品、镜像、存储、网络、健康与策略所有者。OCI Runtime 成为实际进程/VM 状态、原始进程 I/O、描述符受限的工作负载文件系统访问、操作重放、确切终端状态、驱动选择与运行时清理的权威。适配器仅保留检测恢复漂移所需的确切运行时身份、不可变配置与附件摘要、端点、驱动与隔离证据。实时读取在 SDK 响应后重新检查该绑定；资源变更在调度前进入持久 `updating_resources` 状态，并仅在运行时确认后发布新的重启意图。保留的 SDK 客户端在不重放未知请求的情况下报告断开的本地流，然后重连到持久端点，并在下一次显式重试或对账时重新协商。进程边界契约在两个子所有者交换磁盘支撑的运行时状态时保持同一后端存活，证明确切的 Box 对账与对一个实时 exec 流的继续使用且无重复启动。迁移路由器在后端预检前盖上其选择戳，与成功预留一起持久化，从绑定或空的 Box 端点证据路由旧 OCI 记录，且从不对显式已路由记录再次查阅当前发布策略。
稳固的当前行为与分阶段切换门在 [ROADMAP.md](ROADMAP.md) 中分开保存；未完成的迁移工作从不以平台能力呈现。

此仓库是本地运行时，不是托管 Sandbox 控制平面。需要远程编排的团队应在原生 SDK 前放置经认证的服务，而不是将 Box 当作网络 API。

## 仓库地图

```text
src/core/          policy, protocol types, lifecycle state, logs, and errors
src/runtime/       execution manager, backends, images, storage, networks, pools
src/cli/           a3s-box command-line interface
src/sdk/           native Rust SDK and machine bridge
src/cri/           CRI v1 adapter
src/shim/          current host/guest MicroVM control
src/guest/init/    current guest init and execution service
src/third_party/mkext4/  release-owned byte-preserving ext4 writer
sdk/               Python, TypeScript, and Go packages
containerd-shim/   RuntimeClass integration
```

## 文档

- [产品与 OCI Runtime 迁移路线图](ROADMAP.md)
- [安装与打包](docs/installation.md)
- [主机集成与真实运行时验证](docs/host-integration.md)
- [跨能力 soak 计划](docs/soak-test-plan.md)
- [共享内核 Sandbox 威胁模型](docs/host-sandbox-backend-design.md)
- [Windows WHPX 支持](docs/windows-whpx.md)
- [SDK API 与可编程 CI/CD](docs/sdk-api-and-programmable-cicd.md)
- [Compose 规范化](docs/compose-normalization.md)
- [写时复制 snapshot-fork](docs/cow-snapshot-fork-design.md)
- [Kubernetes CRI 符合性](docs/cri-conformance.md)
- [变更日志](CHANGELOG.md)

## 开发

仓库根仅用于编排。从 `src/` 运行 Rust 检查：

```bash
cd src
cargo fmt --all -- --check
cargo test -p a3s-box-core
cargo test -p a3s-box-runtime --lib
cargo test -p a3s-box-cli --test command_coverage
cargo test -p a3s-box-sdk
```

语言包保持独立测试套件：

```bash
cd sdk/python
python -m pip install -e .
python -m unittest discover -s tests

cd ../typescript
npm ci
npm run build
npm test

cd ../go
go vet ./...
go test -race ./...
```

基于主机的 MicroVM、Sandbox、网络、构建、CRI 与耐力测试需要显式准备的机器与隔离的运行时状态。使用
[`scripts/host-integration-smoke.sh`](scripts/host-integration-smoke.sh) 与
[`scripts/local-sdk-smoke.sh`](scripts/local-sdk-smoke.sh)，并为发布门保留主机、后端、镜像摘要、运行时修订与证据包。

## 许可证

A3S Box 以 [MIT License](LICENSE) 提供。供应商源、生成的夹具、SDK 包与发布归档保留随其目录或产物附带的许可证元数据。
