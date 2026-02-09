# A3S Box Kubernetes Deployment Guide

Deploy A3S Box as a CRI-compatible container runtime on Kubernetes, enabling pods to run inside hardware-isolated MicroVMs.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│  Kubernetes Node                                     │
│                                                      │
│  ┌──────────┐     Unix Socket      ┌──────────────┐ │
│  │  kubelet  │ ──────────────────→  │ a3s-box-cri  │ │
│  │           │  /var/run/a3s-box/   │  (DaemonSet) │ │
│  └──────────┘  a3s-box.sock        └──────┬───────┘ │
│       │                                    │         │
│       │ RuntimeClass: a3s-box              │ libkrun │
│       │                                    ▼         │
│  ┌────┴─────┐                      ┌──────────────┐ │
│  │   Pod    │                      │   MicroVM    │ │
│  │ (spec)   │                      │  /dev/kvm    │ │
│  └──────────┘                      └──────────────┘ │
└─────────────────────────────────────────────────────┘
```

## Prerequisites

- Kubernetes 1.26+ cluster
- Nodes with KVM support (`/dev/kvm` accessible)
- `kubectl` configured to access the cluster
- `crictl` (optional, for testing)

### Verify KVM Support

```bash
# On each target node:
ls -la /dev/kvm
# Should show: crw-rw---- 1 root kvm 10, 232 ... /dev/kvm
```

## Quick Start

### 1. Label Nodes

Label nodes where a3s-box should run:

```bash
# Label specific nodes
kubectl label node <node-name> a3s-box.io/runtime=true

# Or label all nodes
kubectl label nodes --all a3s-box.io/runtime=true
```

### 2. Deploy with Kustomize

```bash
# Deploy all resources
kubectl apply -k deploy/kubernetes/

# Verify deployment
kubectl -n a3s-box-system get pods -w
```

Expected output:
```
NAME                  READY   STATUS    RESTARTS   AGE
a3s-box-cri-xxxxx    1/1     Running   0          30s
```

### 3. Configure kubelet

Add a3s-box as an alternative runtime in the kubelet configuration. The exact method depends on your Kubernetes distribution.

#### containerd (most common)

Edit `/etc/containerd/config.toml` on each node:

```toml
[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.a3s-box]
  runtime_type = "io.containerd.a3s-box.v1"

[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.a3s-box.options]
  BinaryName = "/var/run/a3s-box/a3s-box.sock"
  RuntimeRoot = "/var/lib/a3s-box"
```

Then restart containerd:
```bash
sudo systemctl restart containerd
```

#### CRI-O

Edit `/etc/crio/crio.conf.d/10-a3s-box.conf`:

```toml
[crio.runtime.runtimes.a3s-box]
runtime_path = ""
runtime_type = "vm"
runtime_root = "/var/lib/a3s-box"
runtime_config_path = ""
```

### 4. Run a Pod with a3s-box

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: hello-a3s-box
spec:
  runtimeClassName: a3s-box
  containers:
    - name: alpine
      image: alpine:latest
      command: ["sleep", "3600"]
      resources:
        requests:
          cpu: "500m"
          memory: "256Mi"
        limits:
          cpu: "1"
          memory: "512Mi"
```

```bash
kubectl apply -f deploy/examples/alpine-pod.yaml
kubectl get pod hello-a3s-box -w
```

## Verification

### Check CRI Socket

```bash
# On the node where a3s-box-cri is running:
ls -la /var/run/a3s-box/a3s-box.sock
```

### Test with crictl

```bash
# Set the CRI endpoint
export CONTAINER_RUNTIME_ENDPOINT=unix:///var/run/a3s-box/a3s-box.sock

# Check version
crictl version

# Pull an image
crictl pull alpine:latest

# List images
crictl images

# Run the full smoke test
bash deploy/scripts/crictl-smoke-test.sh
```

### Check DaemonSet Logs

```bash
kubectl -n a3s-box-system logs -l app.kubernetes.io/component=cri-runtime -f
```

## Configuration

### ConfigMap Options

| Key | Description | Default |
|-----|-------------|---------|
| `socket-path` | CRI Unix socket path | `/var/run/a3s-box/a3s-box.sock` |
| `image-cache-size` | Max image cache in bytes | `10737418240` (10 GB) |
| `log-level` | Log verbosity | `info` |

### RuntimeClass Overhead

The RuntimeClass defines per-pod overhead for MicroVM resources:

| Resource | Overhead | Description |
|----------|----------|-------------|
| Memory | 30Mi | VM kernel + guest init |
| CPU | 50m | VM management overhead |

Adjust in `runtime-class.yaml` based on your workload profile.

### Custom Image Tag

```bash
# Deploy with a specific version
cd deploy/kubernetes
kustomize edit set image ghcr.io/a3s-lab/a3s-box-cri:v0.1.0
kubectl apply -k .
```

## Uninstall

```bash
# Remove all a3s-box resources
kubectl delete -k deploy/kubernetes/

# Remove node labels
kubectl label nodes --all a3s-box.io/runtime-
```

## Troubleshooting

### Pod stuck in ContainerCreating

```bash
# Check events
kubectl describe pod <pod-name>

# Check CRI runtime logs
kubectl -n a3s-box-system logs -l app.kubernetes.io/component=cri-runtime --tail=50
```

### /dev/kvm not found

Ensure the node has KVM support:
```bash
# Check KVM module
lsmod | grep kvm

# Load KVM module
sudo modprobe kvm
sudo modprobe kvm_intel  # or kvm_amd
```

### Socket permission denied

The DaemonSet runs as privileged. If you see permission errors:
```bash
# Check socket permissions
ls -la /var/run/a3s-box/

# Verify the DaemonSet security context
kubectl -n a3s-box-system get ds a3s-box-cri -o yaml | grep -A5 securityContext
```
