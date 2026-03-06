# a3s-box v0.8.0 发布说明

## 🎉 重大更新：Windows WHPX 后端支持

我们很高兴地宣布 a3s-box v0.8.0 正式发布！这是一个里程碑版本，带来了完整的 Windows Hypervisor Platform (WHPX) 后端支持。

## ✨ 新功能

### Windows WHPX 后端
- ✅ **完整的 Windows 虚拟化支持** - 基于 Windows Hypervisor Platform API
- ✅ **virtiofs 直通文件系统** - 在 Windows 上实现高性能文件共享
- ✅ **virtio-net TCP 后端** - 网络设备支持
- ✅ **virtio-blk 块设备** - 磁盘镜像支持
- ✅ **virtio-console** - 串口控制台，支持 Windows stdin/stdout
- ✅ **TSI (透明套接字模拟)** - vsock 支持，使用 Named Pipes

### a3s-libkrun-sys 0.1.2
- 新增 Windows FFI 绑定
- 预编译的 krun.dll (2.3 MB)
- 完整的 Windows API 覆盖
- 16 个 API 测试全部通过

### CI/CD 改进
- GitHub Actions Windows 构建流水线
- 自动化 Windows 二进制发布
- crates.io 发布支持 Windows

## 📦 平台支持

| 平台 | 后端 | 状态 |
|------|------|------|
| Linux x86_64 | KVM | ✅ 支持 |
| Linux aarch64 | KVM | ✅ 支持 |
| macOS arm64 | Hypervisor.framework | ✅ 支持 |
| **Windows x86_64** | **WHPX** | ✅ **新增！** |

## 🚀 快速开始 (Windows)

### 1. 启用 Windows Hypervisor Platform

```powershell
# 以管理员身份运行
Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform

# 重启计算机
Restart-Computer
```

### 2. 验证启用状态

```powershell
Get-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform
```

### 3. 安装 a3s-box

从 [Releases](https://github.com/A3S-Lab/Box/releases/tag/v0.8.0) 页面下载 Windows 版本：
- `a3s-box-v0.8.0-windows-x86_64.zip`

解压后将 `lib/krun.dll` 添加到 PATH 或放在可执行文件同目录。

### 4. 使用 Rust 依赖

```toml
[dependencies]
a3s-libkrun-sys = "0.1.2"
a3s-box-runtime = "0.8.0"
```

## 📝 示例代码

### 创建 Windows VM

```rust
use a3s_libkrun_sys::*;
use std::ffi::CString;

unsafe {
    // 创建 VM 上下文
    let ctx = krun_create_ctx();
    let ctx_id = ctx as u32;

    // 配置 VM (2 vCPU, 512 MiB RAM)
    krun_set_vm_config(ctx_id, 2, 512);

    // 设置内核
    let kernel = CString::new("C:\\\\path\\\\to\\\\vmlinux").unwrap();
    let cmdline = CString::new("console=ttyS0 root=/dev/vda rw").unwrap();
    krun_set_kernel(
        ctx_id,
        kernel.as_ptr(),
        KRUN_KERNEL_FORMAT_ELF,
        std::ptr::null(),
        cmdline.as_ptr(),
    );

    // 设置根文件系统 (virtiofs)
    let root = CString::new("C:\\\\path\\\\to\\\\rootfs").unwrap();
    krun_set_root(ctx_id, root.as_ptr());

    // 配置网络 (TCP 后端)
    let iface = CString::new("eth0").unwrap();
    let mac: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    krun_add_net_tcp(ctx_id, iface.as_ptr(), mac.as_ptr(), std::ptr::null());

    // 设置工作负载
    let exec = CString::new("/bin/sh").unwrap();
    let arg0 = CString::new("sh").unwrap();
    let argv = [arg0.as_ptr(), std::ptr::null()];
    krun_set_exec(ctx_id, exec.as_ptr(), argv.as_ptr(), std::ptr::null());

    // 启动 VM
    krun_start_enter(ctx_id);
}
```

## 🧪 测试验证

### 已验证功能
- ✅ VM 上下文创建和配置
- ✅ 内核加载 (Firecracker vmlinux-5.10.225)
- ✅ virtiofs 文件系统挂载
- ✅ virtio-net 设备创建
- ✅ virtio-blk 块设备
- ✅ 串口控制台输入输出
- ✅ VM 启动和执行

### 测试示例
```powershell
# 运行 Windows API 测试
cd src\deps\libkrun-sys
cargo test --target x86_64-pc-windows-msvc --lib -- --test-threads=1

# 运行 nginx 容器测试
cargo run --example nginx_test --target x86_64-pc-windows-msvc
```

## 📚 文档

- [Windows 集成测试指南](WINDOWS_INTEGRATION_TEST.md)
- [a3s-libkrun-sys README](src/deps/libkrun-sys/README.md)
- [API 文档](https://docs.rs/a3s-libkrun-sys)

## 🔧 技术细节

### 架构组件
- **WHPX VM/vCPU 管理** - `src/vmm/src/windows/vstate.rs`, `whpx_vcpu.rs`
- **virtio 设备**:
  - `virtio-fs` - `src/devices/src/virtio/fs/windows/`
  - `virtio-net` - `src/devices/src/virtio/net_windows.rs`
  - `virtio-blk` - `src/devices/src/virtio/block_windows.rs`
  - `virtio-console` - `src/devices/src/virtio/console_windows.rs`
  - `virtio-vsock` - `src/devices/src/virtio/vsock/tsi/windows/`
- **EventFd** - `src/utils/src/windows/eventfd.rs`

### 代码统计
- **新增代码**: ~2,100 行 Windows 特定实现
- **测试覆盖**: 16 个 API 测试
- **示例程序**: 4 个 Windows 示例

## 🐛 已知限制

1. **需要 Linux 内核** - Windows 上的 libkrun 需要提供 Linux 内核文件 (ELF 格式)
2. **TCP 网络后端** - 需要预先建立 TCP 连接或使用断开连接模式
3. **测试线程限制** - WHPX 测试必须使用 `--test-threads=1`

## 🔄 破坏性变更

无 - 此版本完全向后兼容现有的 Linux/macOS 代码。

## 📦 发布资产

- `a3s-box-v0.8.0-linux-x86_64.tar.gz` - Linux x86_64
- `a3s-box-v0.8.0-linux-arm64.tar.gz` - Linux ARM64
- `a3s-box-v0.8.0-macos-arm64.tar.gz` - macOS ARM64
- `a3s-box-v0.8.0-windows-x86_64.zip` - **Windows x86_64 (新增)**

## 🙏 致谢

- A3S Lab 团队
- libkrun 项目 (Red Hat)
- Claude Sonnet 4.6 (AI 结对编程)

## 📅 发布时间

2026-03-06

---

**完整更新日志**: https://github.com/A3S-Lab/Box/compare/v0.7.0...v0.8.0
