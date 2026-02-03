# A3S Box CRI Runtime 实现计划（方案 B：混合架构）

> **决策**: 采用混合架构 - 对外兼容 OCI 镜像格式，对内使用 libkrun microVM

## 架构概览

```
┌─────────────────────────────────────────────────────────────────┐
│  Kubernetes Cluster                                             │
│                                                                 │
│  ┌───────────────────────────────────────────────────────────┐ │
│  │  kubelet                                                  │ │
│  └───────────────────────────────────────────────────────────┘ │
│                          │ CRI (gRPC)                           │
│                          ▼                                       │
│  ┌───────────────────────────────────────────────────────────┐ │
│  │  a3s-box-cri-runtime                                      │ │
│  │  ┌─────────────────────────────────────────────────────┐ │ │
│  │  │  CRI Service Layer                                  │ │ │
│  │  │  - RuntimeService (Pod/Container 生命周期)          │ │ │
│  │  │  - ImageService (OCI 镜像管理)                      │ │ │
│  │  └─────────────────────────────────────────────────────┘ │ │
│  │                          │                                │ │
│  │                          ▼                                │ │
│  │  ┌─────────────────────────────────────────────────────┐ │ │
│  │  │  OCI Adapter Layer                                  │ │ │
│  │  │  - OCI 镜像解析                                      │ │ │
│  │  │  - rootfs 提取                                       │ │ │
│  │  │  - 配置转换                                          │ │ │
│  │  └─────────────────────────────────────────────────────┘ │ │
│  │                          │                                │ │
│  │                          ▼                                │ │
│  │  ┌─────────────────────────────────────────────────────┐ │ │
│  │  │  a3s-box-runtime (Core)                             │ │ │
│  │  │  - libkrun (microVM)                                │ │ │
│  │  │  - Box 生命周期管理                                  │ │ │
│  │  │  - Session 管理                                      │ │ │
│  │  └─────────────────────────────────────────────────────┘ │ │
│  └───────────────────────────────────────────────────────────┘ │
│                          │                                       │
│                          ▼                                       │
│  ┌───────────────────────────────────────────────────────────┐ │
│  │  microVM Instances                                        │ │
│  │  ┌─────────┐  ┌─────────┐  ┌─────────┐                  │ │
│  │  │ Box 1   │  │ Box 2   │  │ Box 3   │                  │ │
│  │  │ (VM)    │  │ (VM)    │  │ (VM)    │                  │ │
│  │  └─────────┘  └─────────┘  └─────────┘                  │ │
│  └───────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

## 核心设计原则

1. **对外 OCI 兼容** - 使用标准 OCI 镜像格式，兼容 K8s 生态
2. **对内 microVM 隔离** - 保持 libkrun microVM 的硬件级隔离
3. **渐进式实现** - 分阶段实现，每个阶段可独立交付
4. **保持核心价值** - 不牺牲 A3S Box 的安全性和隔离性

## 实施阶段

### Phase 1: OCI 镜像支持（2-3 周）

**目标**: 让 A3S Box 支持从 OCI 镜像启动

#### 1.1 OCI 镜像格式定义

```dockerfile
# Dockerfile for a3s-box-code
FROM scratch

# 添加最小化的 rootfs
ADD rootfs.tar.gz /

# 添加 a3s-box-code 二进制
COPY a3s-box-code /usr/local/bin/
COPY a3s-box-agent /usr/local/bin/

# A3S Box 特定的标签
LABEL a3s.agent.kind="a3s_code"
LABEL a3s.agent.version="0.1.0"
LABEL a3s.agent.entrypoint="/usr/local/bin/a3s-box-code"
LABEL a3s.agent.listen="vsock://3:4088"

# 标准 OCI 标签
LABEL org.opencontainers.image.title="A3S Code Agent"
LABEL org.opencontainers.image.description="A3S Box Coding Agent"
LABEL org.opencontainers.image.version="0.1.0"

# 入口点（在 microVM 中执行）
ENTRYPOINT ["/usr/local/bin/a3s-box-code"]
CMD ["--listen", "vsock://3:4088"]
```

#### 1.2 OCI 镜像解析器

```rust
// src/runtime/oci/mod.rs
pub mod image;
pub mod manifest;
pub mod config;

// src/runtime/oci/image.rs
use oci_spec::image::{ImageManifest, ImageConfiguration};

pub struct OciImage {
    manifest: ImageManifest,
    config: ImageConfiguration,
    layers: Vec<PathBuf>,
}

impl OciImage {
    /// 从镜像引用拉取 OCI 镜像
    pub async fn pull(image_ref: &str) -> Result<Self> {
        // 使用 containerd 或 skopeo 拉取镜像
        let manifest = Self::fetch_manifest(image_ref).await?;
        let config = Self::fetch_config(&manifest).await?;
        let layers = Self::fetch_layers(&manifest).await?;

        Ok(Self { manifest, config, layers })
    }

    /// 提取 rootfs
    pub fn extract_rootfs(&self, target_dir: &Path) -> Result<()> {
        for layer in &self.layers {
            // 解压每一层到 target_dir
            Self::extract_layer(layer, target_dir)?;
        }
        Ok(())
    }

    /// 获取 A3S Agent 配置
    pub fn get_agent_config(&self) -> Result<AgentConfig> {
        let labels = &self.config.config().labels();

        Ok(AgentConfig {
            kind: labels.get("a3s.agent.kind")
                .ok_or(BoxError::InvalidImage("missing a3s.agent.kind"))?,
            version: labels.get("a3s.agent.version").cloned(),
            entrypoint: labels.get("a3s.agent.entrypoint").cloned(),
            ..Default::default()
        })
    }
}
```

#### 1.3 集成到 Box Runtime

```rust
// src/runtime/box_manager.rs
impl BoxManager {
    /// 从 OCI 镜像创建 Box
    pub async fn create_box_from_oci_image(
        &self,
        image_ref: &str,
        config: BoxConfig,
    ) -> Result<Box> {
        // 1. 拉取 OCI 镜像
        let oci_image = OciImage::pull(image_ref).await?;

        // 2. 提取 rootfs
        let rootfs_dir = self.prepare_rootfs_dir(&config.box_id)?;
        oci_image.extract_rootfs(&rootfs_dir)?;

        // 3. 获取 Agent 配置
        let agent_config = oci_image.get_agent_config()?;

        // 4. 创建 Box（使用现有的 libkrun 逻辑）
        let box_config = BoxConfig {
            coding_agent: agent_config,
            ..config
        };

        self.create_box_from_rootfs(box_config, rootfs_dir).await
    }
}
```

### Phase 2: CRI RuntimeService 实现（3-4 周）

**目标**: 实现 CRI RuntimeService 接口

#### 2.1 CRI 服务结构

```rust
// src/cri/mod.rs
pub mod runtime_service;
pub mod image_service;
pub mod server;

// src/cri/runtime_service.rs
use k8s_cri::v1::runtime_service_server::{RuntimeService, RuntimeServiceServer};
use k8s_cri::v1::*;

pub struct A3sBoxRuntimeService {
    box_manager: Arc<BoxManager>,
    pod_sandbox_map: Arc<RwLock<HashMap<String, PodSandbox>>>,
    container_map: Arc<RwLock<HashMap<String, Container>>>,
}

#[tonic::async_trait]
impl RuntimeService for A3sBoxRuntimeService {
    async fn version(
        &self,
        _request: Request<VersionRequest>,
    ) -> Result<Response<VersionResponse>, Status> {
        Ok(Response::new(VersionResponse {
            version: "0.1.0".to_string(),
            runtime_name: "a3s-box".to_string(),
            runtime_version: "0.1.0".to_string(),
            runtime_api_version: "v1".to_string(),
        }))
    }

    async fn run_pod_sandbox(
        &self,
        request: Request<RunPodSandboxRequest>,
    ) -> Result<Response<RunPodSandboxResponse>, Status> {
        let req = request.into_inner();
        let config = req.config.ok_or_else(|| {
            Status::invalid_argument("missing pod sandbox config")
        })?;

        // 从 PodSandboxConfig 创建 BoxConfig
        let box_config = self.pod_config_to_box_config(&config)?;

        // 创建 Box 实例（作为 Pod Sandbox）
        let box_instance = self.box_manager
            .create_box(box_config)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let pod_id = box_instance.id().to_string();

        // 保存 Pod Sandbox 信息
        let pod_sandbox = PodSandbox {
            id: pod_id.clone(),
            metadata: config.metadata,
            state: PodSandboxState::Ready,
            created_at: SystemTime::now(),
            box_instance,
        };

        self.pod_sandbox_map.write().await.insert(pod_id.clone(), pod_sandbox);

        Ok(Response::new(RunPodSandboxResponse {
            pod_sandbox_id: pod_id,
        }))
    }

    async fn create_container(
        &self,
        request: Request<CreateContainerRequest>,
    ) -> Result<Response<CreateContainerResponse>, Status> {
        let req = request.into_inner();
        let pod_id = req.pod_sandbox_id;
        let config = req.config.ok_or_else(|| {
            Status::invalid_argument("missing container config")
        })?;

        // 获取 Pod Sandbox (Box Instance)
        let pod_sandbox = self.pod_sandbox_map.read().await
            .get(&pod_id)
            .ok_or_else(|| Status::not_found("pod sandbox not found"))?
            .clone();

        // 在 Box 中创建 Session（作为 Container）
        let session_id = pod_sandbox.box_instance
            .create_session()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        // 保存 Container 信息
        let container = Container {
            id: session_id.clone(),
            pod_sandbox_id: pod_id,
            metadata: config.metadata,
            image: config.image,
            state: ContainerState::Created,
            created_at: SystemTime::now(),
        };

        self.container_map.write().await.insert(session_id.clone(), container);

        Ok(Response::new(CreateContainerResponse {
            container_id: session_id,
        }))
    }

    async fn start_container(
        &self,
        request: Request<StartContainerRequest>,
    ) -> Result<Response<StartContainerResponse>, Status> {
        let container_id = request.into_inner().container_id;

        // 获取 Container
        let mut containers = self.container_map.write().await;
        let container = containers.get_mut(&container_id)
            .ok_or_else(|| Status::not_found("container not found"))?;

        // 启动 Session
        let pod_sandbox = self.pod_sandbox_map.read().await
            .get(&container.pod_sandbox_id)
            .ok_or_else(|| Status::not_found("pod sandbox not found"))?
            .clone();

        pod_sandbox.box_instance
            .start_session(&container_id)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        container.state = ContainerState::Running;

        Ok(Response::new(StartContainerResponse {}))
    }

    // 实现其他 CRI 方法...
}
```

#### 2.2 配置映射

```rust
// src/cri/config_mapper.rs
impl A3sBoxRuntimeService {
    fn pod_config_to_box_config(
        &self,
        pod_config: &PodSandboxConfig,
    ) -> Result<BoxConfig> {
        let metadata = pod_config.metadata.as_ref()
            .ok_or_else(|| BoxError::InvalidConfig("missing metadata"))?;

        // 从 Pod annotations 读取 A3S Box 配置
        let annotations = &pod_config.annotations;
        let agent_kind = annotations.get("a3s.box/agent-kind")
            .unwrap_or(&"a3s_code".to_string())
            .clone();
        let agent_image = annotations.get("a3s.box/agent-image");

        // 从 Linux 配置读取资源限制
        let resources = if let Some(linux) = &pod_config.linux {
            ResourceConfig {
                memory: linux.resources.as_ref()
                    .and_then(|r| r.memory_limit_in_bytes)
                    .unwrap_or(2 * 1024 * 1024 * 1024),
                cpus: linux.resources.as_ref()
                    .and_then(|r| r.cpu_quota)
                    .map(|q| (q / 100000) as u32)
                    .unwrap_or(2),
                ..Default::default()
            }
        } else {
            ResourceConfig::default()
        };

        Ok(BoxConfig {
            box_id: Some(metadata.uid.clone()),
            coding_agent: AgentConfig {
                kind: agent_kind,
                image: agent_image.cloned(),
                ..Default::default()
            },
            resources,
            ..Default::default()
        })
    }
}
```

### Phase 3: CRI ImageService 实现（2-3 周）

**目标**: 实现 CRI ImageService 接口

```rust
// src/cri/image_service.rs
use k8s_cri::v1::image_service_server::{ImageService, ImageServiceServer};

pub struct A3sBoxImageService {
    image_store: Arc<RwLock<HashMap<String, OciImage>>>,
    cache_dir: PathBuf,
}

#[tonic::async_trait]
impl ImageService for A3sBoxImageService {
    async fn list_images(
        &self,
        request: Request<ListImagesRequest>,
    ) -> Result<Response<ListImagesResponse>, Status> {
        let images = self.image_store.read().await;
        let image_list = images.values()
            .map(|img| Image {
                id: img.id().to_string(),
                repo_tags: img.repo_tags().to_vec(),
                size: img.size(),
                ..Default::default()
            })
            .collect();

        Ok(Response::new(ListImagesResponse {
            images: image_list,
        }))
    }

    async fn pull_image(
        &self,
        request: Request<PullImageRequest>,
    ) -> Result<Response<PullImageResponse>, Status> {
        let req = request.into_inner();
        let image_ref = req.image.ok_or_else(|| {
            Status::invalid_argument("missing image spec")
        })?.image;

        // 拉取 OCI 镜像
        let oci_image = OciImage::pull(&image_ref)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let image_id = oci_image.id().to_string();

        // 保存到镜像存储
        self.image_store.write().await.insert(image_id.clone(), oci_image);

        Ok(Response::new(PullImageResponse {
            image_ref: image_id,
        }))
    }

    async fn remove_image(
        &self,
        request: Request<RemoveImageRequest>,
    ) -> Result<Response<RemoveImageResponse>, Status> {
        let image_ref = request.into_inner().image.ok_or_else(|| {
            Status::invalid_argument("missing image spec")
        })?.image;

        self.image_store.write().await.remove(&image_ref);

        Ok(Response::new(RemoveImageResponse {}))
    }
}
```

### Phase 4: 部署和测试（2-3 周）

#### 4.1 RuntimeClass 配置

```yaml
# runtime-class.yaml
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: a3s-box
handler: a3s-box
scheduling:
  nodeSelector:
    a3s.box/enabled: "true"
  tolerations:
  - key: a3s.box/runtime
    operator: Exists
    effect: NoSchedule
```

#### 4.2 DaemonSet 部署

```yaml
# a3s-box-cri-daemonset.yaml
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: a3s-box-cri-runtime
  namespace: kube-system
spec:
  selector:
    matchLabels:
      app: a3s-box-cri-runtime
  template:
    metadata:
      labels:
        app: a3s-box-cri-runtime
    spec:
      hostNetwork: true
      hostPID: true
      nodeSelector:
        a3s.box/enabled: "true"
      containers:
      - name: a3s-box-cri-runtime
        image: ghcr.io/a3s-box/cri-runtime:v0.1.0
        securityContext:
          privileged: true
        volumeMounts:
        - name: cri-socket
          mountPath: /var/run/a3s-box
        - name: dev-kvm
          mountPath: /dev/kvm
        - name: image-cache
          mountPath: /var/lib/a3s-box/images
        env:
        - name: CRI_SOCKET_PATH
          value: /var/run/a3s-box/a3s-box.sock
        resources:
          limits:
            memory: 4Gi
            cpu: 2
      volumes:
      - name: cri-socket
        hostPath:
          path: /var/run/a3s-box
          type: DirectoryOrCreate
      - name: dev-kvm
        hostPath:
          path: /dev/kvm
      - name: image-cache
        hostPath:
          path: /var/lib/a3s-box/images
          type: DirectoryOrCreate
```

#### 4.3 kubelet 配置

```yaml
# /var/lib/kubelet/config.yaml
apiVersion: kubelet.config.k8s.io/v1beta1
kind: KubeletConfiguration
containerRuntimeEndpoint: unix:///var/run/a3s-box/a3s-box.sock
imageServiceEndpoint: unix:///var/run/a3s-box/a3s-box.sock
```

#### 4.4 测试 Pod

```yaml
# test-pod.yaml
apiVersion: v1
kind: Pod
metadata:
  name: test-a3s-box
spec:
  runtimeClassName: a3s-box
  containers:
  - name: app
    image: ghcr.io/a3s-box/a3s-code:v0.1.0
    command: ["/usr/local/bin/a3s-box-code"]
    args: ["--listen", "vsock://3:4088"]
```

## 技术细节

### OCI 镜像层次结构

```
ghcr.io/a3s-box/a3s-code:v0.1.0
├── manifest.json
├── config.json
└── layers/
    ├── layer-1.tar.gz  (base rootfs)
    ├── layer-2.tar.gz  (a3s-box-code binary)
    └── layer-3.tar.gz  (configuration files)
```

### 数据流

```
1. kubectl apply -f pod.yaml
   ↓
2. API Server → Scheduler → kubelet
   ↓
3. kubelet → CRI (RunPodSandbox)
   ↓
4. a3s-box-cri-runtime → ImageService.PullImage
   ↓
5. OCI Image → extract rootfs
   ↓
6. a3s-box-runtime → libkrun.create_vm(rootfs)
   ↓
7. microVM started with a3s-box-code
   ↓
8. kubelet → CRI (CreateContainer)
   ↓
9. a3s-box-runtime → box.create_session()
   ↓
10. Session created in microVM
```

## 依赖和工具

### Rust Crates

```toml
[dependencies]
# CRI
tonic = "0.10"
prost = "0.12"
k8s-cri = "0.7"

# OCI
oci-spec = "0.6"
oci-distribution = "0.10"
containerd-client = "0.4"

# 现有依赖
a3s-box-core = { path = "../core" }
a3s-box-runtime = { path = "../runtime" }
```

### 外部工具

- **containerd**: 用于 OCI 镜像拉取和管理
- **skopeo**: 备选的镜像工具
- **crictl**: CRI 测试工具

## 测试策略

### 单元测试

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_oci_image_pull() {
        let image = OciImage::pull("ghcr.io/a3s-box/a3s-code:v0.1.0")
            .await
            .unwrap();
        assert_eq!(image.get_agent_config().unwrap().kind, "a3s_code");
    }

    #[tokio::test]
    async fn test_cri_run_pod_sandbox() {
        let service = A3sBoxRuntimeService::new();
        let request = RunPodSandboxRequest {
            config: Some(PodSandboxConfig {
                metadata: Some(PodSandboxMetadata {
                    name: "test-pod".to_string(),
                    uid: "test-uid".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };

        let response = service.run_pod_sandbox(Request::new(request))
            .await
            .unwrap();
        assert!(!response.into_inner().pod_sandbox_id.is_empty());
    }
}
```

### 集成测试

```bash
# 使用 crictl 测试
crictl --runtime-endpoint unix:///var/run/a3s-box/a3s-box.sock version
crictl --runtime-endpoint unix:///var/run/a3s-box/a3s-box.sock pull ghcr.io/a3s-box/a3s-code:v0.1.0
crictl --runtime-endpoint unix:///var/run/a3s-box/a3s-box.sock runp pod-config.json
crictl --runtime-endpoint unix:///var/run/a3s-box/a3s-box.sock create <pod-id> container-config.json pod-config.json
```

## 性能优化

### 镜像缓存

```rust
// src/cri/image_cache.rs
pub struct ImageCache {
    cache_dir: PathBuf,
    lru: LruCache<String, OciImage>,
}

impl ImageCache {
    pub async fn get_or_pull(&mut self, image_ref: &str) -> Result<OciImage> {
        // 1. 检查内存缓存
        if let Some(image) = self.lru.get(image_ref) {
            return Ok(image.clone());
        }

        // 2. 检查磁盘缓存
        let cache_path = self.cache_dir.join(Self::image_hash(image_ref));
        if cache_path.exists() {
            let image = OciImage::load_from_cache(&cache_path)?;
            self.lru.put(image_ref.to_string(), image.clone());
            return Ok(image);
        }

        // 3. 拉取镜像
        let image = OciImage::pull(image_ref).await?;

        // 4. 保存到缓存
        image.save_to_cache(&cache_path)?;
        self.lru.put(image_ref.to_string(), image.clone());

        Ok(image)
    }
}
```

### Box 实例池

```rust
// src/runtime/box_pool.rs
pub struct BoxPool {
    pool: Vec<Box>,
    max_size: usize,
}

impl BoxPool {
    pub async fn get_or_create(&mut self, config: BoxConfig) -> Result<Box> {
        // 尝试从池中获取
        if let Some(box_instance) = self.pool.pop() {
            box_instance.reconfigure(config).await?;
            return Ok(box_instance);
        }

        // 创建新实例
        BoxManager::create_box(config).await
    }

    pub async fn return_box(&mut self, box_instance: Box) {
        if self.pool.len() < self.max_size {
            box_instance.reset().await.ok();
            self.pool.push(box_instance);
        }
    }
}
```

## 监控和可观测性

### Metrics

```rust
// src/cri/metrics.rs
use prometheus::{Counter, Gauge, Histogram};

lazy_static! {
    static ref POD_SANDBOX_CREATED: Counter = register_counter!(
        "a3s_box_pod_sandbox_created_total",
        "Total number of pod sandboxes created"
    ).unwrap();

    static ref CONTAINER_CREATED: Counter = register_counter!(
        "a3s_box_container_created_total",
        "Total number of containers created"
    ).unwrap();

    static ref IMAGE_PULL_DURATION: Histogram = register_histogram!(
        "a3s_box_image_pull_duration_seconds",
        "Time spent pulling images"
    ).unwrap();

    static ref ACTIVE_BOXES: Gauge = register_gauge!(
        "a3s_box_active_boxes",
        "Number of active Box instances"
    ).unwrap();
}
```

## 文档和示例

### 用户文档

- [ ] CRI Runtime 安装指南
- [ ] RuntimeClass 配置说明
- [ ] OCI 镜像构建指南
- [ ] 故障排查手册

### 开发者文档

- [ ] CRI 接口实现细节
- [ ] OCI 适配层设计
- [ ] 测试指南
- [ ] 贡献指南

## 时间线

| 阶段 | 时间 | 交付物 |
|------|------|--------|
| Phase 1 | 2-3 周 | OCI 镜像支持 |
| Phase 2 | 3-4 周 | CRI RuntimeService |
| Phase 3 | 2-3 周 | CRI ImageService |
| Phase 4 | 2-3 周 | 部署和测试 |
| **总计** | **9-13 周** | **完整的 CRI Runtime** |

## 风险和缓解

| 风险 | 影响 | 缓解措施 |
|------|------|----------|
| CRI 接口复杂 | 高 | 参考 containerd/CRI-O 实现 |
| OCI 镜像兼容性 | 中 | 使用标准库，充分测试 |
| 性能问题 | 中 | 实现缓存和池化 |
| 嵌套虚拟化限制 | 高 | 文档说明，提供云环境配置 |

## 下一步行动

1. [ ] 创建 `src/cri/` 目录结构
2. [ ] 实现 OCI 镜像解析器
3. [ ] 编写单元测试
4. [ ] 构建第一个 OCI 镜像
5. [ ] 测试从 OCI 镜像启动 Box

---

**状态**: 📋 实施计划
**决策**: ✅ 方案 B（混合架构）
**最后更新**: 2026-02-03
