# A3S Box 作为容器运行时的设计方案

## 概述

本文档设计 A3S Box 如何作为 Docker 和 Kubernetes 的容器运行时，使 A3S Box 能够在容器编排环境中运行，并提供智能体沙箱能力。

## 目标

1. **Docker 集成** - A3S Box 可以作为 Docker 容器运行
2. **Kubernetes 集成** - A3S Box 可以在 K8s 集群中部署和编排
3. **CRI 兼容** - 实现 Kubernetes Container Runtime Interface
4. **OCI 兼容** - 符合 OCI 运行时规范

## 架构方案

### 方案 1: A3S Box as Sidecar Container（推荐）

**架构图**:

```
┌─────────────────────────────────────────────────────────────┐
│                    Kubernetes Pod                           │
│                                                             │
│  ┌──────────────────┐         ┌──────────────────────────┐ │
│  │  App Container   │         │  A3S Box Sidecar         │ │
│  │                  │         │                          │ │
│  │  - 业务应用       │◄───────►│  - a3s-box-runtime      │ │
│  │  - Python/TS SDK │  gRPC   │  - microVM (libkrun)    │ │
│  │                  │         │  - Coding Agent         │ │
│  └──────────────────┘         │  - Skills               │ │
│                                └──────────────────────────┘ │
│                                                             │
│  Shared Volumes:                                            │
│  - /workspace (emptyDir)                                    │
│  - /skills (configMap/secret)                               │
└─────────────────────────────────────────────────────────────┘
```

**特点**:
- ✅ 最简单的集成方式
- ✅ 无需修改 K8s 或 Docker
- ✅ 应用容器通过 SDK 与 A3S Box 通信
- ✅ 支持现有的 A3S Box API
- ⚠️ 每个 Pod 需要一个 A3S Box 实例

**实现步骤**:
1. 将 a3s-box-runtime 打包为 Docker 镜像
2. 在 Pod 中作为 sidecar 容器运行
3. 通过 localhost gRPC 通信（而非 vsock）
4. 共享 volume 用于 workspace 和 skills

### 方案 2: A3S Box as DaemonSet

**架构图**:

```
┌─────────────────────────────────────────────────────────────┐
│                    Kubernetes Node                          │
│                                                             │
│  ┌──────────────────────────────────────────────────────┐  │
│  │  A3S Box DaemonSet (每个节点一个实例)                  │  │
│  │  - a3s-box-runtime                                   │  │
│  │  - 管理多个 Box 实例                                  │  │
│  │  - gRPC Server (监听 Unix Socket)                    │  │
│  └──────────────────────────────────────────────────────┘  │
│                          ▲                                  │
│                          │ Unix Socket                      │
│  ┌───────────┐  ┌───────────┐  ┌───────────┐              │
│  │  Pod 1    │  │  Pod 2    │  │  Pod 3    │              │
│  │  App + SDK│  │  App + SDK│  │  App + SDK│              │
│  └───────────┘  └───────────┘  └───────────┘              │
└─────────────────────────────────────────────────────────────┘
```

**特点**:
- ✅ 节点级别的资源共享
- ✅ 减少资源开销（每个节点一个 runtime）
- ✅ 集中管理和监控
- ⚠️ 需要实现多租户隔离
- ⚠️ 需要实现 Box 实例池管理

**实现步骤**:
1. 扩展 a3s-box-runtime 支持多 Box 管理
2. 实现 Box 实例池（pool）
3. 通过 Unix Socket 提供 gRPC 服务
4. 使用 hostPath volume 共享 socket

### 方案 3: A3S Box as CRI Runtime（高级）

**架构图**:

```
┌─────────────────────────────────────────────────────────────┐
│                    Kubernetes Node                          │
│                                                             │
│  ┌──────────────────────────────────────────────────────┐  │
│  │  kubelet                                             │  │
│  └──────────────────────────────────────────────────────┘  │
│                          │                                  │
│                          │ CRI (gRPC)                       │
│                          ▼                                  │
│  ┌──────────────────────────────────────────────────────┐  │
│  │  a3s-box-cri-runtime                                 │  │
│  │  - 实现 CRI RuntimeService                           │  │
│  │  - 实现 CRI ImageService                             │  │
│  │  - 管理 A3S Box 生命周期                             │  │
│  └──────────────────────────────────────────────────────┘  │
│                          │                                  │
│                          ▼                                  │
│  ┌──────────────────────────────────────────────────────┐  │
│  │  A3S Box Instances (microVMs)                        │  │
│  │  ┌─────────┐  ┌─────────┐  ┌─────────┐              │  │
│  │  │ Box 1   │  │ Box 2   │  │ Box 3   │              │  │
│  │  └─────────┘  └─────────┘  └─────────┘              │  │
│  └──────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

**特点**:
- ✅ 完全集成到 K8s 运行时层
- ✅ 无需修改应用代码
- ✅ 支持标准 K8s 工作负载
- ⚠️ 实现复杂度高
- ⚠️ 需要实现完整的 CRI 接口

**实现步骤**:
1. 实现 CRI RuntimeService 接口
2. 实现 CRI ImageService 接口
3. 将 A3S Box 映射为 CRI Pod/Container
4. 实现容器生命周期管理

## 详细设计

### 1. Docker 镜像打包

**Dockerfile 结构**:

```dockerfile
FROM ubuntu:22.04

# 安装依赖
RUN apt-get update && apt-get install -y \
    libkrun \
    libvirt0 \
    qemu-system-x86 \
    && rm -rf /var/lib/apt/lists/*

# 复制 a3s-box-runtime
COPY target/release/a3s-box-runtime /usr/local/bin/
COPY target/release/a3s-box-code /usr/local/bin/

# 复制配置文件
COPY .a3s/ /etc/a3s/

# 暴露 gRPC 端口
EXPOSE 4088

# 启动 runtime
ENTRYPOINT ["/usr/local/bin/a3s-box-runtime"]
CMD ["serve", "--listen", "0.0.0.0:4088"]
```

**挑战**:
- libkrun 需要 KVM 或 Hypervisor.framework
- Docker 容器内运行 microVM 需要特权模式
- 需要 `/dev/kvm` 设备访问

**解决方案**:
```yaml
# Docker Compose
services:
  a3s-box:
    image: a3s-box:latest
    privileged: true
    devices:
      - /dev/kvm
    volumes:
      - ./workspace:/workspace
      - ./skills:/skills
```

### 2. Kubernetes 部署

#### 2.1 Sidecar 模式部署

**Pod 定义**:

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: my-app-with-a3s-box
spec:
  containers:
  # 应用容器
  - name: app
    image: my-app:latest
    env:
    - name: A3S_BOX_ENDPOINT
      value: "localhost:4088"
    volumeMounts:
    - name: workspace
      mountPath: /workspace
    - name: skills
      mountPath: /skills

  # A3S Box Sidecar
  - name: a3s-box
    image: a3s-box:latest
    securityContext:
      privileged: true
    resources:
      limits:
        devices.kubevirt.io/kvm: "1"
    volumeMounts:
    - name: workspace
      mountPath: /workspace
    - name: skills
      mountPath: /skills
    - name: llm-config
      mountPath: /etc/a3s/llm-config.json
      subPath: llm-config.json

  volumes:
  - name: workspace
    emptyDir: {}
  - name: skills
    configMap:
      name: a3s-skills
  - name: llm-config
    secret:
      secretName: a3s-llm-config
```

#### 2.2 DaemonSet 模式部署

**DaemonSet 定义**:

```yaml
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: a3s-box-runtime
  namespace: kube-system
spec:
  selector:
    matchLabels:
      app: a3s-box-runtime
  template:
    metadata:
      labels:
        app: a3s-box-runtime
    spec:
      hostNetwork: true
      hostPID: true
      containers:
      - name: a3s-box-runtime
        image: a3s-box:latest
        securityContext:
          privileged: true
        volumeMounts:
        - name: a3s-socket
          mountPath: /var/run/a3s
        - name: dev-kvm
          mountPath: /dev/kvm
        resources:
          limits:
            memory: 4Gi
            cpu: 2
      volumes:
      - name: a3s-socket
        hostPath:
          path: /var/run/a3s
          type: DirectoryOrCreate
      - name: dev-kvm
        hostPath:
          path: /dev/kvm
```

**应用 Pod 使用**:

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: my-app
spec:
  containers:
  - name: app
    image: my-app:latest
    env:
    - name: A3S_BOX_ENDPOINT
      value: "unix:///var/run/a3s/a3s-box.sock"
    volumeMounts:
    - name: a3s-socket
      mountPath: /var/run/a3s
  volumes:
  - name: a3s-socket
    hostPath:
      path: /var/run/a3s
```

### 3. 网络通信适配

**当前**: vsock (host-guest 通信)
**容器环境**: TCP/Unix Socket

**适配方案**:

```rust
// src/runtime/transport.rs
pub enum Transport {
    Vsock { cid: u32, port: u32 },
    Tcp { host: String, port: u16 },
    UnixSocket { path: PathBuf },
}

impl Transport {
    pub fn from_env() -> Self {
        if let Ok(endpoint) = env::var("A3S_BOX_ENDPOINT") {
            if endpoint.starts_with("unix://") {
                Transport::UnixSocket {
                    path: PathBuf::from(endpoint.strip_prefix("unix://").unwrap())
                }
            } else if endpoint.starts_with("tcp://") {
                // Parse TCP endpoint
                Transport::Tcp { ... }
            } else {
                // Default to vsock
                Transport::Vsock { cid: 3, port: 4088 }
            }
        } else {
            Transport::Vsock { cid: 3, port: 4088 }
        }
    }
}
```

### 4. 资源隔离和限制

**挑战**:
- microVM 需要 KVM 设备
- 需要特权模式运行
- 资源限制需要传递到 microVM

**解决方案**:

```yaml
# Kubernetes ResourceQuota
apiVersion: v1
kind: ResourceQuota
metadata:
  name: a3s-box-quota
spec:
  hard:
    requests.devices.kubevirt.io/kvm: "10"
    limits.devices.kubevirt.io/kvm: "10"
```

```rust
// 将 K8s 资源限制映射到 Box 配置
impl From<K8sResourceLimits> for ResourceConfig {
    fn from(limits: K8sResourceLimits) -> Self {
        ResourceConfig {
            memory: limits.memory.unwrap_or(2 * 1024 * 1024 * 1024),
            cpus: limits.cpu.unwrap_or(2),
            disk: limits.ephemeral_storage.unwrap_or(10 * 1024 * 1024 * 1024),
        }
    }
}
```

### 5. CRI 实现（方案 3）

**CRI 接口**:

```protobuf
// Kubernetes CRI RuntimeService
service RuntimeService {
    rpc Version(VersionRequest) returns (VersionResponse) {}
    rpc RunPodSandbox(RunPodSandboxRequest) returns (RunPodSandboxResponse) {}
    rpc StopPodSandbox(StopPodSandboxRequest) returns (StopPodSandboxResponse) {}
    rpc RemovePodSandbox(RemovePodSandboxRequest) returns (RemovePodSandboxResponse) {}
    rpc CreateContainer(CreateContainerRequest) returns (CreateContainerResponse) {}
    rpc StartContainer(StartContainerRequest) returns (StartContainerResponse) {}
    rpc StopContainer(StopContainerRequest) returns (StopContainerResponse) {}
    rpc RemoveContainer(RemoveContainerRequest) returns (RemoveContainerResponse) {}
    // ... more methods
}
```

**映射关系**:

| CRI 概念 | A3S Box 概念 |
|---------|-------------|
| PodSandbox | Box Instance |
| Container | Session |
| Image | Agent Image (OCI/Binary) |
| Volume | virtio-fs Mount |

**实现示例**:

```rust
// src/cri/runtime_service.rs
#[tonic::async_trait]
impl RuntimeService for A3sBoxCriRuntime {
    async fn run_pod_sandbox(
        &self,
        request: Request<RunPodSandboxRequest>,
    ) -> Result<Response<RunPodSandboxResponse>, Status> {
        let config = request.into_inner().config.unwrap();

        // 创建 Box 实例
        let box_config = BoxConfig {
            box_id: Some(config.metadata.unwrap().uid),
            coding_agent: AgentConfig::default(),
            resources: ResourceConfig::from_cri_resources(&config.linux),
            ..Default::default()
        };

        let box_instance = self.box_manager.create_box(box_config).await?;

        Ok(Response::new(RunPodSandboxResponse {
            pod_sandbox_id: box_instance.id().to_string(),
        }))
    }

    async fn create_container(
        &self,
        request: Request<CreateContainerRequest>,
    ) -> Result<Response<CreateContainerResponse>, Status> {
        let req = request.into_inner();
        let pod_id = req.pod_sandbox_id;

        // 在 Box 中创建 Session
        let box_instance = self.box_manager.get_box(&pod_id).await?;
        let session_id = box_instance.create_session().await?;

        Ok(Response::new(CreateContainerResponse {
            container_id: session_id,
        }))
    }
}
```

## 配置示例

### Helm Chart

```yaml
# values.yaml
a3sBox:
  mode: sidecar  # sidecar | daemonset | cri
  image:
    repository: ghcr.io/a3s-box/a3s-box
    tag: v0.1.0
  resources:
    limits:
      memory: 4Gi
      cpu: 2
      devices.kubevirt.io/kvm: "1"
  llmConfig:
    secretName: a3s-llm-config
  skills:
    configMapName: a3s-skills
```

### Operator

```yaml
apiVersion: a3s.dev/v1alpha1
kind: A3sBox
metadata:
  name: my-a3s-box
spec:
  mode: sidecar
  codingAgent:
    kind: a3s_code
    version: v0.1.0
  llmConfig:
    secretRef:
      name: llm-config
  skills:
    - name: order-agent
      configMapRef:
        name: order-agent-skill
  resources:
    memory: 2Gi
    cpus: 2
```

## 实施路线图

### Phase 1: Docker 支持（1-2 周）
- [ ] 创建 Dockerfile
- [ ] 实现 TCP/Unix Socket 传输
- [ ] 测试 Docker Compose 部署
- [ ] 文档和示例

### Phase 2: Kubernetes Sidecar（2-3 周）
- [ ] 创建 Helm Chart
- [ ] 实现配置注入（ConfigMap/Secret）
- [ ] 测试 Pod 部署
- [ ] 监控和日志集成

### Phase 3: Kubernetes DaemonSet（3-4 周）
- [ ] 实现多 Box 管理
- [ ] 实现 Box 实例池
- [ ] Unix Socket 通信
- [ ] 资源配额和限制

### Phase 4: CRI 实现（8-12 周）
- [ ] 实现 CRI RuntimeService
- [ ] 实现 CRI ImageService
- [ ] 容器生命周期管理
- [ ] 与 kubelet 集成测试

## 技术挑战

### 1. 特权模式要求
**问题**: microVM 需要 KVM 访问，需要特权容器
**解决**:
- 使用 Kubernetes Device Plugin
- 限制特权范围（只需要 /dev/kvm）
- 考虑使用 gVisor/Firecracker 替代

### 2. 嵌套虚拟化
**问题**: 容器内运行 microVM（嵌套虚拟化）
**解决**:
- 云环境需要支持嵌套虚拟化
- AWS: 使用 .metal 实例
- GCP: 启用嵌套虚拟化
- Azure: 使用 Dv3/Ev3 系列

### 3. 网络通信
**问题**: vsock 在容器环境不可用
**解决**:
- 实现 TCP/Unix Socket 传输
- 保持 API 兼容性
- 自动检测运行环境

### 4. 资源管理
**问题**: K8s 资源限制与 microVM 资源配置
**解决**:
- 将 K8s limits 映射到 Box ResourceConfig
- 实现资源监控和上报
- 支持动态资源调整

## 安全考虑

1. **特权容器风险**
   - 限制特权范围
   - 使用 seccomp/AppArmor 配置
   - 定期安全审计

2. **多租户隔离**
   - 每个 Box 独立的 microVM
   - 网络隔离（NetworkPolicy）
   - 资源配额限制

3. **密钥管理**
   - LLM API Keys 存储在 K8s Secret
   - 使用 Vault 等密钥管理系统
   - 定期轮换密钥

## 监控和可观测性

```yaml
# Prometheus ServiceMonitor
apiVersion: monitoring.coreos.com/v1
kind: ServiceMonitor
metadata:
  name: a3s-box
spec:
  selector:
    matchLabels:
      app: a3s-box
  endpoints:
  - port: metrics
    path: /metrics
```

**关键指标**:
- Box 实例数量
- Session 数量
- CPU/内存使用率
- LLM API 调用次数和延迟
- 错误率

## 参考资料

- [Kubernetes CRI](https://kubernetes.io/docs/concepts/architecture/cri/)
- [OCI Runtime Specification](https://github.com/opencontainers/runtime-spec)
- [containerd](https://containerd.io/)
- [CRI-O](https://cri-o.io/)
- [Firecracker](https://firecracker-microvm.github.io/)
- [Kata Containers](https://katacontainers.io/)

## 总结

**推荐方案**: 从 **Sidecar 模式**开始，逐步演进到 **DaemonSet 模式**，最后考虑 **CRI 实现**。

**优先级**:
1. ✅ **Phase 1**: Docker 支持（必须）
2. ✅ **Phase 2**: Kubernetes Sidecar（推荐）
3. ⚠️ **Phase 3**: Kubernetes DaemonSet（可选）
4. ⚠️ **Phase 4**: CRI 实现（长期目标）

---

**状态**: 📝 设计文档
**最后更新**: 2026-02-03
