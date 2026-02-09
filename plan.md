# Phase 4 Completion: Kubernetes Deployment

## Context

The CRI RuntimeService and ImageService are fully implemented. The `a3s-box-cri` binary
serves both services over a Unix domain socket. What's missing is the Kubernetes deployment
infrastructure: RuntimeClass, DaemonSet manifests, and a crictl smoke test script.

## Deliverables

### 1. Kubernetes Manifests (`deploy/kubernetes/`)
- namespace.yaml, runtime-class.yaml, daemonset.yaml
- configmap.yaml, rbac.yaml, kustomization.yaml

### 2. Deployment Guide (`deploy/kubernetes/README.md`)

### 3. crictl Smoke Test (`deploy/scripts/crictl-smoke-test.sh`)

### 4. Example Pod Specs (`deploy/examples/`)
