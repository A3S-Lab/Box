# Windows WHPX

A3S Box runs Linux OCI workloads directly on x86_64 Windows through the
Windows Hypervisor Platform (WHPX) backend in libkrun. WSL is not part of the
runtime path.

## Requirements

- x86_64 Windows 10 or Windows 11;
- hardware virtualization enabled in firmware;
- Windows Hypervisor Platform enabled;
- Windows Developer Mode enabled, or the A3S Box service identity granted
  `SeCreateSymbolicLinkPrivilege` (A3S Box temporarily enables an assigned but
  disabled privilege only while extracting an OCI layer);
- the A3S Box Windows binaries and their matching runtime DLLs.

Enable WHPX from an elevated PowerShell prompt, then restart Windows if the
feature manager requests it:

```powershell
Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform
```

Run `a3s-box info` before starting a workload. It should report both
`Virtualization: WHPX` and `OCI symlink support: available`. The probe uses the
same scoped privilege implementation as layer extraction, so an assigned but
initially disabled service-token privilege is reported accurately.

Linux OCI images commonly contain symbolic links for paths such as `/bin`,
dynamic loaders, and shared libraries. The Windows rootfs is backed by NTFS and
served to the guest through virtio-fs, so A3S Box must preserve those entries as
real symbolic links. Enable Developer Mode in **Settings > System > Advanced >
For developers** (called **Settings > System > For developers** before Windows
11 25H2). Once enabled, A3S Box can run as a normal, non-elevated process. See
Microsoft's [Developer Mode instructions](https://learn.microsoft.com/windows/advanced-settings/developer-mode)
and [`CreateSymbolicLink` documentation](https://learn.microsoft.com/windows/win32/api/winbase/nf-winbase-createsymboliclinkw).

If Developer Mode is disabled and the process token lacks the privilege, or the
target directory denies link creation, image extraction fails with an explicit
`ERROR_ACCESS_DENIED (5)` / `ERROR_PRIVILEGE_NOT_HELD (1314)` diagnostic. This
is intentional: replacing a symbolic link with a copied file, hard link, or
directory junction would change OCI layer, whiteout, and guest path-resolution
semantics.

## Runtime package

A runnable Windows package contains:

```text
a3s-box.exe
a3s-box-shim.exe
a3s-box-guest-init
krun.dll
libkrunfw.dll
```

`a3s-box-guest-init` is a static Linux executable used as PID 1 inside the
MicroVM. `krun.dll` provides the native WHPX VMM, while `libkrunfw.dll` carries
the Linux guest kernel. Both DLLs must remain next to the Windows executables so
the Windows loader can resolve them. Zig cross-links the Linux guest binaries;
it does not compile the Linux kernel.

For a source build:

```powershell
winget install --id zig.zig --exact --version 0.16.0
cd src
powershell.exe -NoProfile -ExecutionPolicy Bypass -File deps/libkrun-sys/vendor/libkrun/scripts/build-windows-init.ps1
cargo install cargo-zigbuild
cargo zigbuild --release -p a3s-box-guest-init --target x86_64-unknown-linux-musl
cargo build --release -p a3s-box-cli -p a3s-box-shim
```

The nested script is required before rebuilding `krun.dll` from a fresh
submodule checkout. It produces the ignored, stripped `init/init` payload used
by libkrun's Windows wrapper; release workflows never substitute an empty file.

The build copies `krun.dll` and `libkrunfw.dll` next to the native binaries in
the Cargo target directory.

## Current support boundary

| Capability | Windows status |
| --- | --- |
| Pull and run Linux OCI images | Implemented; Alpine 3.20 validated on real WHPX |
| Foreground and detached `run` | Validated |
| `ps`, `logs`, `inspect`, and read-only `attach` | Validated, including separated stdout/stderr and structured JSON logs |
| Non-interactive post-boot `exec` | Validated through the shim-owned local named pipe and guest control tunnel, including environment propagation and repeated commands |
| `cp`, `top`, and `stats --no-stream` | Validated for bidirectional single-file copy, guest process listing, and guest PID accounting |
| Workload exit codes | Validated for foreground and detached workloads |
| Long workload arguments | Validated with a 4,096-byte argument staged outside the bounded guest kernel command line |
| Graceful stop and cleanup | Validated through the guest control channel with configured signal delivery, bounded force termination, and no residual shim or forwarding worker |
| vCPUs | Exactly one; omitting `--cpus` selects the Windows default of 1 |
| Published TCP ports | Validated through the Windows named-pipe bridge, including sequential connections |
| Bind mounts | Validated for drive-letter directory and single-file sources, including read-only enforcement |
| Named volumes | Validated across `stop` and `restart`, including explicit removal |
| Initialization scripts | Validated for a read-only host-provided script/config plus a named state volume, including exact success, exit 42 failure, and persisted evidence from both attempts |
| POSIX ownership and modes | Validated for `chmod`, `chown`, and umask-created files and directories across clean stop, restart, and commit |
| `diff`, `export`, stopped-box `commit`, and stopped-box filesystem snapshots | Validated through clean-stop metadata capture, committed-image re-run, snapshot restore, restart, and re-export. Running-box host-path capture remains unavailable because WHPX has no post-boot guest archive channel. |
| Container health checks | Not currently supported; `--health-*` requests and persisted health checks fail before workload start |
| Bridge networks and Compose service networking | Not currently supported on Windows |
| Interactive PTY (`attach -it` and `exec -it`) | Not currently supported on Windows; non-interactive `exec` is supported |
| Memory snapshot-fork, TEE, and CRI | Not supported on Windows |

Requests such as `--cpus 2` or `--health-cmd ...` fail before image pull with
an explicit WHPX diagnostic. `--no-healthcheck` remains available to disable an
image-defined health check.

## Smoke test

The following paths were validated on July 20–22 and again on August 1, 2026,
on Windows build 26200 with an AMD Ryzen 7 9800X3D and
`HypervisorPlatform` enabled:

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
mounts, verified graceful and forced-stop cleanup without orphan processes,
restarted a named volume, and restored and restarted a filesystem snapshot.

The August 1 qualification additionally exercised repeated post-boot exec,
bidirectional single-file `cp`, `top`, and guest PID-aware
`stats --no-stream`. All 12 selected tests passed in 563.541 seconds against
the OCI archive with SHA-256
`45a567b46d75167f02a0a0042781fed3dfa5835b2b4c7c85fa1b8259f67aa27d`.
Peak observed runtime usage was 170,004,480 bytes and 960 handles, and both
the starting and final A3S process inventories were empty.

Standard Compose services create a bridge network by default. Because native
WHPX bridge networking is not implemented, Compose workload startup remains
outside the current Windows support boundary even when a Compose file contains
only one service. `compose up` rejects this platform combination before image
resolution, network creation, box-directory creation, or VM startup.

## WHPX soak validation

Run the Windows-specific soak harness from the Box repository root on an
otherwise idle WHPX host. It builds the current guest-init and Windows binaries,
then precompiles the real smoke executable and repeatedly exercises the
supported lifecycle, logs, exit-code, long-argv, post-boot exec, bidirectional
single-file copy, `top`, guest PID-aware stats, published-port, bind-mount,
named-volume, commit, snapshot, and virtio-fs paths. The initialization profile
additionally combines a read-only script mount with a named state volume and
requires both a successful run and an exact nonzero failure.

This runner supplies the `WIN-01` lane in the
[Cross-Capability Soak Test Plan](soak-test-plan.md). Its evidence proves only
the documented Windows subset; unsupported Windows features require fail-closed
functional tests rather than being inferred from another host.

```powershell
.\scripts\windows-whpx-soak.ps1 `
  -ImageTar C:\images\alpine-3.20.tar `
  -Iterations 1

# Two-hour gate. The current iteration is allowed to finish after the deadline.
.\scripts\windows-whpx-soak.ps1 `
  -ImageTar C:\images\alpine-3.20.tar `
  -Iterations 0 `
  -DurationSeconds 7200
```

Evidence is written under `src/target/a3s-box-whpx-soak/` by default. Each test
has its own log. `summary.json`, `operations.tsv`, `resource-samples.tsv`,
start/final inventories, `host.json`, and `verify.out` record the commit, image
digest, timings, peak runtime working set, handles, process counts, failures,
and cleanup. The runner requires no active A3S Box or A3S OCI Runtime process
at startup, verifies the same invariant after every test, and fails when
requested/completed counts or resource guardrails drift.

The twelve-test default matrix includes a 4,096-byte workload argument,
volume-backed init success/failure, and POSIX ownership and mode replay through
restart and commit. Use `-ListTests` to inspect the exact selection.

The virtio-fs case intentionally scans 2,048 files five times with cache mode
`none`. Real WHPX validation took 373 seconds on the host described above, so
that test has an independent 900-second default budget. `-SkipVirtiofsStress`
is suitable for a short functional rehearsal, not for release soak evidence.

## Box-to-OCI Runtime product qualification

The unified-runtime vertical slice has a separate, build-free hardware gate.
Download the Box `windows-whpx` artifact and the pinned OCI Runtime
`windows-whpx-qualification` and `guest-agents-musl` artifacts without renaming
or removing their `artifact-manifest.json` files. Use the fixed Alpine 3.22.5
x86_64 minirootfs archive, whose SHA-256 is
`4b4daa9fe2fc696c4919c4412a4c3d3e770d8fb70292a004a2c72f5096175282`.

```powershell
.\scripts\windows-whpx-oci-qualification.ps1 `
  -BoxArtifactDirectory C:\artifacts\box-windows `
  -OciWindowsArtifactDirectory C:\artifacts\oci-windows `
  -OciGuestArtifactDirectory C:\artifacts\oci-agents `
  -RootfsArchive C:\images\alpine-minirootfs-3.22.5-x86_64.tar.gz
```

The runner verifies the checkout and pinned OCI source commits, requires the
two OCI artifacts to share one workflow commit and run ID, and rehashes every
input before staging it. It starts the protected named-pipe service, imports the
rootfs into a dedicated Box home without registry access, and runs the public
Box manager through idempotent create, manager reopen and reconcile, WHPX
start/wait, exact exit code 23, and generation-fenced delete replay. Success
also requires no Box execution directory, OCI generation share, bundle handoff,
or A3S host process to remain. `report.json` and `summary.json` use the
versioned `a3s.box.windows-whpx-oci-qualification.v1` and
`a3s.box.windows-whpx-oci-qualification-run.v1` schemas respectively.
Preparation and start have a 30-minute bound. On Windows, handoff validation
accepts the ordinary and `\\?\` namespace spellings only when they name the
same exact container/create-operation path; a different operation suffix or a
filesystem alias remains invalid.

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
