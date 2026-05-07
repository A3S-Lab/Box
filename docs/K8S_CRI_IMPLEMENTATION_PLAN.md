# Kubernetes CRI 实现计划

**日期:** 2026-05-04  
**目标:** 使 a3s-box 成为生产级 Kubernetes CRI 运行时  
**当前状态:** 基础 CRI 实现已完成 (~7,094 行代码)，需要完善和验证

---

## 📊 当前 CRI 实现状态评估

### ✅ 已实现的核心组件

| 组件 | 代码行数 | 完成度 | 状态 |
|------|---------|--------|------|
| RuntimeService | 3,463 | 80% | 核心 API 已实现 |
| ImageService | 1,049 | 90% | 镜像管理完整 |
| Streaming | 583 | 70% | exec/attach 基础支持 |
| PersistentStore | 393 | 85% | 状态持久化 |
| Container | 433 | 75% | 容器生命周期 |
| ConfigMapper | 298 | 80% | CRI 配置映射 |
| Sandbox | 242 | 75% | Pod Sandbox 管理 |
| State | 265 | 85% | 状态管理 |
| Server | 128 | 90% | gRPC 服务器 |
| Main | 127 | 95% | CLI 入口 |

**总计:** 7,094 行代码，平均完成度 **82%**

### 🎯 架构设计

```
CRI 架构映射:
┌─────────────────────────────────────────────────┐
│ Kubernetes kubelet                               │
└─────────────────┬───────────────────────────────┘
                  │ gRPC (Unix Socket)
┌─────────────────▼───────────────────────────────┐
│ a3s-box-cri Server                               │
│ ├─ RuntimeService (Pod/Container lifecycle)     │
│ └─ ImageService (Image pull/list/remove)        │
└─────────────────┬───────────────────────────────┘
                  │
┌─────────────────▼───────────────────────────────┐
│ Mapping Layer                                    │
│ ├─ Pod Sandbox → Box Instance (1 MicroVM/pod)   │
│ └─ Container → Session within Box               │
└─────────────────┬───────────────────────────────┘
                  │
┌─────────────────▼───────────────────────────────┐
│ a3s-box Runtime (VmManager, ImageStore, etc.)   │
└──────────────────────────────────────────────────┘
```

---

## 🔴 P0: crictl Runtime MVP (必须完成)

### 目标
使 `crictl` 命令能够成功操作 a3s-box CRI 运行时

### 任务清单

#### 1. Stable Sandbox Image (沙箱镜像)
**优先级:** 🔴 P0  
**工作量:** 2-3 天  
**状态:** ⚠️ 需要定义和构建

**任务:**
- [ ] 定义 pause/sandbox 镜像规范
- [ ] 构建最小化 sandbox 镜像 (基于 Alpine)
- [ ] 发布到公共镜像仓库
- [ ] 配置默认 sandbox 镜像路径
- [ ] 添加 sandbox 镜像健康检查

**验收标准:**
```bash
# 镜像可以被拉取
crictl pull registry.a3s.box/pause:latest

# 镜像可以启动 sandbox
crictl runp sandbox-config.json
```

#### 2. crictl Harness (测试套件)
**优先级:** 🔴 P0  
**工作量:** 3-4 天  
**状态:** ⚠️ 部分实现

**任务:**
- [ ] 创建 crictl 烟雾测试脚本
- [ ] 实现 `crictl info` 测试
- [ ] 实现 `crictl pull` 测试
- [ ] 实现 `crictl runp` (RunPodSandbox) 测试
- [ ] 实现 `crictl create` 测试
- [ ] 实现 `crictl start` 测试
- [ ] 实现 `crictl logs` 测试
- [ ] 实现 `crictl stats` 测试
- [ ] 实现 `crictl stop` 测试
- [ ] 实现 `crictl rm` 测试
- [ ] 添加 CI/CD 集成

**验收标准:**
```bash
# 所有 crictl 命令成功执行
./deploy/scripts/crictl-smoke-test.sh
# 输出: All tests passed ✅
```

#### 3. RunPodSandbox Boot Path (Pod 启动路径)
**优先级:** 🔴 P0  
**工作量:** 4-5 天  
**状态:** ⚠️ 需要完善

**任务:**
- [ ] 确保 sandbox 达到 Ready 状态
- [ ] 分配稳定的网络 IP
- [ ] 配置 DNS 解析
- [ ] 设置网络命名空间
- [ ] 实现网络就绪检查
- [ ] 添加超时和重试机制
- [ ] 记录详细的启动日志

**验收标准:**
```bash
# Sandbox 成功启动并达到 Ready 状态
crictl runp sandbox.json
# 输出: sandbox_id

crictl inspectp <sandbox_id> | jq '.status.state'
# 输出: "SANDBOX_READY"

crictl inspectp <sandbox_id> | jq '.status.network.ip'
# 输出: "10.244.0.2"
```

#### 4. Long-running Container Lifecycle (长期运行容器)
**优先级:** 🔴 P0  
**工作量:** 3-4 天  
**状态:** ⚠️ 需要验证

**任务:**
- [ ] 确保容器持续运行
- [ ] 实现容器状态查询
- [ ] 实现容器停止功能
- [ ] 实现容器删除功能
- [ ] 处理容器崩溃场景
- [ ] 添加容器重启策略
- [ ] 实现优雅关闭

**验收标准:**
```bash
# 容器持续运行
crictl create <sandbox_id> container.json sandbox.json
crictl start <container_id>
sleep 60
crictl ps | grep <container_id>
# 输出: CONTAINER_RUNNING

# 容器可以被停止和删除
crictl stop <container_id>
crictl rm <container_id>
```

#### 5. Continuous CRI Logs (持续日志)
**优先级:** 🔴 P0  
**工作量:** 3-4 天  
**状态:** ⚠️ 需要实现

**任务:**
- [ ] 实现 CRI 日志格式 (JSON)
- [ ] 添加时间戳处理
- [ ] 分离 stdout/stderr 流
- [ ] 实现日志轮转
- [ ] 支持日志跟踪 (tail -f)
- [ ] 处理日志文件权限
- [ ] 添加日志大小限制

**验收标准:**
```bash
# 日志文件格式正确
crictl logs <container_id>
# 输出: 2026-05-04T10:00:00.000000000Z stdout F Hello World

# 日志持续更新
crictl logs -f <container_id>
# 输出: 实时日志流
```

#### 6. Mount Execution (挂载执行)
**优先级:** 🔴 P0  
**工作量:** 2-3 天  
**状态:** ⚠️ 需要验证

**任务:**
- [ ] 应用 CRI 挂载到 guest 内部
- [ ] 支持只读挂载
- [ ] 实现挂载传播
- [ ] 处理挂载失败
- [ ] 验证挂载权限
- [ ] 添加挂载测试

**验收标准:**
```bash
# 挂载成功应用
crictl exec <container_id> ls /mnt/volume
# 输出: 挂载的文件列表
```

#### 7. Stop and Remove Idempotency (幂等性)
**优先级:** 🔴 P0  
**工作量:** 2 天  
**状态:** ⚠️ 需要实现

**任务:**
- [ ] 实现幂等的 Stop 操作
- [ ] 实现幂等的 Remove 操作
- [ ] 处理重复调用
- [ ] 返回兼容的错误码
- [ ] 添加幂等性测试

**验收标准:**
```bash
# 多次调用不报错
crictl stop <container_id>
crictl stop <container_id>  # 第二次调用成功
crictl rm <container_id>
crictl rm <container_id>    # 第二次调用成功
```

#### 8. Private Image Pull (私有镜像拉取)
**优先级:** 🔴 P0  
**工作量:** 2-3 天  
**状态:** ✅ 已实现 (需要测试)

**任务:**
- [ ] 支持 ImageService auth config
- [ ] 配置私有镜像仓库
- [ ] 实现认证凭据传递
- [ ] 添加私有镜像拉取测试
- [ ] 处理认证失败场景

**验收标准:**
```bash
# 私有镜像拉取成功
crictl pull --auth <auth_config> registry.private.com/app:v1.0
# 输出: Image pulled successfully
```

---

## 🟡 P1: k3s Single-Node Runtime (单节点集成)

### 目标
在 k3s 单节点集群中运行 a3s-box 作为容器运行时

### 任务清单

#### 1. Kubelet Endpoint Integration
**优先级:** 🟡 P1  
**工作量:** 2-3 天

**任务:**
- [ ] 配置 Unix socket 启动
- [ ] 提供 kubelet 配置示例
- [ ] 实现 `--container-runtime-endpoint`
- [ ] 实现 `--image-service-endpoint`
- [ ] 添加 socket 权限管理
- [ ] 编写集成文档

**配置示例:**
```bash
# 启动 a3s-box-cri
a3s-box-cri --socket /var/run/a3s-box/a3s-box.sock

# 配置 kubelet
kubelet \
  --container-runtime-endpoint=unix:///var/run/a3s-box/a3s-box.sock \
  --image-service-endpoint=unix:///var/run/a3s-box/a3s-box.sock
```

#### 2. CNI and Pod Networking
**优先级:** 🟡 P1  
**工作量:** 5-7 天

**任务:**
- [ ] 实现 CNI 插件集成
- [ ] 分配 Pod IP 地址
- [ ] 配置 DNS 解析
- [ ] 实现 Service 可达性
- [ ] 实现网络清理
- [ ] 添加网络测试

#### 3. RuntimeConfig
**优先级:** 🟡 P1  
**工作量:** 2 天

**任务:**
- [ ] 实现 Pod CIDR 配置
- [ ] 实现网络运行时配置
- [ ] 处理 kubelet 配置更新

#### 4. Real Stats
**优先级:** 🟡 P1  
**工作量:** 4-5 天

**任务:**
- [ ] 实现真实的 CPU 统计
- [ ] 实现真实的内存统计
- [ ] 实现文件系统统计
- [ ] 实现网络统计
- [ ] 实现可写层统计
- [ ] 实现镜像文件系统统计

#### 5. SecurityContext Execution
**优先级:** 🟡 P1  
**工作量:** 5-7 天

**任务:**
- [ ] 实现 runAsUser
- [ ] 实现 runAsGroup
- [ ] 实现 supplementalGroups
- [ ] 实现 readonlyRootfs
- [ ] 实现 capabilities
- [ ] 实现 privileged mode
- [ ] 实现 devices
- [ ] 实现 seccomp/AppArmor

#### 6. Exec, Attach, and Port-forward
**优先级:** 🟡 P1  
**工作量:** 4-5 天

**任务:**
- [ ] 实现 kubelet streaming workflows
- [ ] 实现 exec 功能
- [ ] 实现 attach 功能
- [ ] 实现 port-forward 功能
- [ ] 兼容 Kubernetes 客户端

#### 7. ConfigMap, Secret, and Token Mounts
**优先级:** 🟡 P1  
**工作量:** 3-4 天

**任务:**
- [ ] 实现 ConfigMap 挂载
- [ ] 实现 Secret 挂载
- [ ] 实现 projected 挂载
- [ ] 实现 service account token 挂载

#### 8. Image and Container Garbage Collection
**优先级:** 🟡 P1  
**工作量:** 3-4 天

**任务:**
- [ ] 实现镜像垃圾回收
- [ ] 实现容器垃圾回收
- [ ] 处理 kubelet 压力场景
- [ ] 添加安全检查

#### 9. Runtime Errors
**优先级:** 🟡 P1  
**工作量:** 2 天

**任务:**
- [ ] 映射到 CRI 兼容的 gRPC 状态码
- [ ] 提供清晰的错误消息
- [ ] 添加错误处理测试

---

## 🟢 P2: Broader Kubernetes Readiness (更广泛的就绪度)

### 任务清单

#### 1. RuntimeClass Integration
**优先级:** 🟢 P2  
**工作量:** 3-4 天

#### 2. Node Pressure and Eviction
**优先级:** 🟢 P2  
**工作量:** 4-5 天

#### 3. Checkpoint/Restore Support
**优先级:** 🟢 P2  
**工作量:** 7-10 天

#### 4. Conformance Coverage
**优先级:** 🟢 P2  
**工作量:** 5-7 天

#### 5. Multi-architecture Validation
**优先级:** 🟢 P2  
**工作量:** 3-5 天

---

## 🎯 CRI 验收标准

### Phase 1: crictl 验收 (P0)

```bash
# 1. Info 命令
crictl info
# ✅ 返回运行时信息

# 2. 镜像拉取
crictl pull docker.io/library/busybox:latest
# ✅ 镜像拉取成功

# 3. Pod Sandbox 创建
crictl runp sandbox-config.json
# ✅ 返回 sandbox_id

# 4. 容器创建和启动
crictl create <sandbox_id> container-config.json sandbox-config.json
crictl start <container_id>
# ✅ 容器运行

# 5. 日志查看
crictl logs <container_id>
# ✅ 显示容器日志

# 6. 统计信息
crictl stats <container_id>
# ✅ 显示资源统计

# 7. 停止和删除
crictl stop <container_id>
crictl rm <container_id>
crictl stopp <sandbox_id>
crictl rmp <sandbox_id>
# ✅ 清理成功
```

### Phase 2: k3s 验收 (P1)

```bash
# 1. k3s 节点启动
k3s server \
  --container-runtime-endpoint=unix:///var/run/a3s-box/a3s-box.sock \
  --image-service-endpoint=unix:///var/run/a3s-box/a3s-box.sock
# ✅ 节点达到 Ready 状态

# 2. BusyBox Pod
kubectl run busybox --image=busybox --command -- sleep 3600
kubectl get pods
# ✅ Pod 运行

# 3. Nginx Pod
kubectl run nginx --image=nginx
kubectl get pods
# ✅ Pod 运行

# 4. 多容器 Pod
kubectl apply -f multi-container-pod.yaml
# ✅ Pod 运行

# 5. ConfigMap 和 Secret
kubectl create configmap test-config --from-literal=key=value
kubectl create secret generic test-secret --from-literal=password=secret
kubectl apply -f pod-with-config.yaml
# ✅ Pod 运行并可以访问配置

# 6. 网络测试
kubectl run test-1 --image=busybox --command -- sleep 3600
kubectl run test-2 --image=busybox --command -- sleep 3600
kubectl exec test-1 -- ping <test-2-ip>
# ✅ Pod 间网络通信

# 7. kubectl 命令
kubectl logs <pod>
kubectl exec <pod> -- ls
kubectl attach <pod>
kubectl port-forward <pod> 8080:80
# ✅ 所有命令工作正常

# 8. kubelet 重启
systemctl restart kubelet
kubectl get pods
# ✅ 没有孤儿资源
```

---

## 📅 实施时间线

### 第 1 周: P0 基础设施 (5 天)
- [ ] Day 1-2: Stable Sandbox Image
- [ ] Day 3-4: crictl Harness
- [ ] Day 5: RunPodSandbox Boot Path (开始)

### 第 2 周: P0 核心功能 (5 天)
- [ ] Day 1-2: RunPodSandbox Boot Path (完成)
- [ ] Day 3-4: Long-running Container Lifecycle
- [ ] Day 5: Continuous CRI Logs (开始)

### 第 3 周: P0 完善 (5 天)
- [ ] Day 1-2: Continuous CRI Logs (完成)
- [ ] Day 2-3: Mount Execution
- [ ] Day 4: Stop and Remove Idempotency
- [ ] Day 5: Private Image Pull 测试

### 第 4 周: P0 验收 (5 天)
- [ ] Day 1-3: crictl 完整测试套件
- [ ] Day 4-5: 修复发现的问题

**P0 里程碑:** crictl 所有命令通过 ✅

### 第 5-6 周: P1 Kubelet 集成 (10 天)
- [ ] Kubelet Endpoint Integration
- [ ] CNI and Pod Networking (开始)

### 第 7-8 周: P1 网络和安全 (10 天)
- [ ] CNI and Pod Networking (完成)
- [ ] RuntimeConfig
- [ ] SecurityContext Execution (开始)

### 第 9-10 周: P1 高级功能 (10 天)
- [ ] SecurityContext Execution (完成)
- [ ] Real Stats
- [ ] Exec, Attach, Port-forward

### 第 11-12 周: P1 完善和验收 (10 天)
- [ ] ConfigMap, Secret, Token Mounts
- [ ] Image and Container GC
- [ ] Runtime Errors
- [ ] k3s 完整测试

**P1 里程碑:** k3s 单节点集群运行 ✅

### 第 13-16 周: P2 生产就绪 (20 天)
- [ ] RuntimeClass Integration
- [ ] Node Pressure and Eviction
- [ ] Conformance Coverage
- [ ] Multi-architecture Validation

**P2 里程碑:** 生产级 Kubernetes CRI 运行时 ✅

---

## 🧪 测试策略

### 单元测试
- [ ] RuntimeService 单元测试覆盖率 > 80%
- [ ] ImageService 单元测试覆盖率 > 80%
- [ ] Streaming 单元测试覆盖率 > 70%

### 集成测试
- [ ] crictl 烟雾测试套件
- [ ] k3s 集成测试
- [ ] 网络连通性测试
- [ ] 资源限制测试

### 端到端测试
- [ ] 真实 Kubernetes 工作负载
- [ ] 多 Pod 场景
- [ ] 故障恢复测试
- [ ] 性能基准测试

---

## 📊 成功指标

### P0 成功指标
- ✅ crictl 所有命令通过
- ✅ Pod Sandbox 启动成功率 > 95%
- ✅ 容器运行稳定性 > 99%
- ✅ 日志完整性 100%

### P1 成功指标
- ✅ k3s 节点达到 Ready 状态
- ✅ 标准 Kubernetes 工作负载运行
- ✅ Pod 网络连通性 100%
- ✅ kubectl 命令兼容性 > 95%

### P2 成功指标
- ✅ Kubernetes 一致性测试通过
- ✅ 多架构支持
- ✅ 生产环境验证

---

## 🚀 快速开始

### 构建 CRI 运行时
```bash
cd /Users/roylin/Desktop/code/a3s/crates/box/src
cargo build --release -p a3s-box-cri
```

### 启动 CRI 服务
```bash
./target/release/a3s-box-cri \
  --socket /var/run/a3s-box/a3s-box.sock \
  --sandbox-image registry.a3s.box/pause:latest \
  --sandbox-network k8s-pods
```

### 测试 crictl
```bash
export CONTAINER_RUNTIME_ENDPOINT=unix:///var/run/a3s-box/a3s-box.sock
crictl info
crictl pull docker.io/library/busybox:latest
```

---

## 📚 参考资源

- [CRI v1 API Specification](https://github.com/kubernetes/cri-api)
- [crictl User Guide](https://github.com/kubernetes-sigs/cri-tools/blob/master/docs/crictl.md)
- [k3s Documentation](https://docs.k3s.io/)
- [Kubernetes CRI Documentation](https://kubernetes.io/docs/concepts/architecture/cri/)

---

## 🎯 下一步行动

### 立即开始 (本周)
1. ✅ 创建实施计划文档 (本文档)
2. 🔴 构建 Stable Sandbox Image
3. 🔴 创建 crictl 烟雾测试脚本
4. 🔴 验证 RunPodSandbox 启动路径

### 本月目标
- 完成 P0 所有任务
- crictl 所有命令通过
- 开始 P1 Kubelet 集成

### 季度目标
- 完成 P1 所有任务
- k3s 单节点集群运行
- 开始 P2 生产就绪工作

---

**总预计时间:** 16 周 (4 个月)  
**关键里程碑:** P0 (4 周), P1 (8 周), P2 (4 周)  
**成功标准:** 成为生产级 Kubernetes CRI 运行时 ✅
