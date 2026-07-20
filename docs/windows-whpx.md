# Windows WHPX

A3S Box runs Linux OCI workloads directly on x86_64 Windows through the
Windows Hypervisor Platform (WHPX) backend in libkrun. WSL is not part of the
runtime path.

## Requirements

- x86_64 Windows 10 or Windows 11;
- hardware virtualization enabled in firmware;
- Windows Hypervisor Platform enabled;
- the A3S Box Windows binaries and their matching runtime DLLs.

Enable WHPX from an elevated PowerShell prompt, then restart Windows if the
feature manager requests it:

```powershell
Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform
```

Run `a3s-box info` before starting a workload to verify that virtualization is
available.

## Runtime package

A runnable Windows package contains:

```text
a3s-box.exe
a3s-box-shim.exe
a3s-box-guest-init
lib/
├── krun.dll
└── libkrunfw.dll
```

`a3s-box-guest-init` is a static Linux executable used as PID 1 inside the
MicroVM. `krun.dll` provides the native WHPX VMM, while `libkrunfw.dll` carries
the Linux guest kernel. Zig is used only to cross-link guest-init; it does not
compile the Linux kernel.

For a source build:

```powershell
cd src
cargo install cargo-zigbuild
cargo zigbuild --release -p a3s-box-guest-init --target x86_64-unknown-linux-musl
cargo build --release -p a3s-box-cli -p a3s-box-shim
```

The build copies `krun.dll` and `libkrunfw.dll` next to the native binaries in
the Cargo target directory.

## Current support boundary

| Capability | Windows status |
| --- | --- |
| Pull and run Linux OCI images | Implemented; Alpine 3.20 validated on real WHPX |
| Foreground and detached `run` | Validated |
| `ps`, `logs`, and `inspect` | Validated, including separated stdout/stderr and structured JSON logs |
| Workload exit codes | Validated for foreground and detached workloads |
| vCPUs | Exactly one; omitting `--cpus` selects the Windows default of 1 |
| Published TCP ports | Validated through the Windows named-pipe bridge, including sequential connections |
| Bind mounts | Validated for drive-letter directory and single-file sources, including read-only enforcement |
| Named volumes | Validated across `stop` and `start`, including explicit removal |
| `diff`, `export`, `commit`, and filesystem snapshots | Validated through snapshot restore, restart, and re-export |
| `stats --no-stream` | Validated |
| Bridge networks and Compose service networking | Not currently supported on Windows |
| Interactive PTY, `attach`, and post-boot `exec` | Not currently supported on Windows |
| Memory snapshot-fork, TEE, and CRI | Not supported on Windows |

Requests such as `--cpus 2` fail before image pull with an explicit WHPX
diagnostic.

## Smoke test

The following paths were validated on July 20–21, 2026, on Windows 11 Pro build
26200 with an AMD Ryzen 7 9800X3D and `HypervisorPlatform` enabled:

```powershell
# Success, stdout, and stderr
a3s-box run --name whpx-ok alpine:3.20 -- /bin/sh -c `
  'echo WHPX_OK; echo WHPX_STDERR_OK >&2; uname -srm'

# Real non-zero exit propagation
a3s-box run --name whpx-exit alpine:3.20 -- /bin/sh -c 'exit 7'
$LASTEXITCODE  # 7

# Detached result reconciliation
a3s-box run -d --name whpx-detached alpine:3.20 -- /bin/sh -c `
  'sleep 1; echo DETACHED_OK; exit 3'
a3s-box ps -a
a3s-box logs whpx-detached
a3s-box inspect whpx-detached
```

The validation booted the kernel bundled in `libkrunfw.dll` as Linux 6.12.91.
The `/sbin/init` installed into the test rootfs matched the locally
Zig-cross-linked guest-init byte for byte. Foreground exit 7 and detached exit
3 both reached the host unchanged. The validation also ran all 113 Linux
guest-init unit tests inside a real WHPX guest, sent multiple requests through
the same published TCP port, exercised Windows directory and single-file bind
mounts, restarted a named volume, and restored and restarted a filesystem
snapshot.

Standard Compose services create a bridge network by default. Because native
WHPX bridge networking is not implemented, Compose workload startup remains
outside the current Windows support boundary even when a Compose file contains
only one service.

## Diagnostics and kernel override

The default WHPX boot path automatically selects the current reliable
single-vCPU, legacy-PIC kernel configuration. Do not set a custom kernel for
normal operation.

For kernel debugging only, `A3S_BOX_KERNEL` can point to an x86_64 ELF
`vmlinux` or a supported PE/COFF kernel image:

```powershell
$env:A3S_BOX_KERNEL = 'C:\path\to\vmlinux'
```

`LIBKRUN_WINDOWS_KERNEL_CMDLINE_APPEND` is also an expert override for the
additional Windows kernel command line. When it is absent, A3S Box supplies
`noapic`; an explicitly set value, including an empty value, replaces that
default.

If a boot fails, inspect the per-box files below
`$env:A3S_HOME\boxes\<id>\logs\` and `rootfs\init-rust.log`. A package missing
either `krun.dll` or `libkrunfw.dll` is incomplete.
