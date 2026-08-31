//! A3S Box Shim - MicroVM subprocess for process isolation.
//!
//! This binary is spawned by VmController to isolate the VM from the host application.
//! libkrun's `krun_start_enter()` performs process takeover, so we need a separate
//! process to prevent the host application from being taken over.
//!
//! # Usage
//! ```bash
//! a3s-box-shim --config '{"box_id": "...", ...}'
//! ```

// Allow large error types - this is a binary, not a library
#![allow(clippy::result_large_err)]

mod krun;
mod managed_oci_log_worker;
mod vm_launch;

#[cfg(target_os = "windows")]
use a3s_box_core::config::validate_vcpu_count;
use a3s_box_core::error::{BoxError, Result};
#[cfg(target_os = "windows")]
use a3s_box_core::exec::WINDOWS_STOP_REQUEST_FILE;
use a3s_box_core::vmm::{InstanceSpec, RawBlockDevice, RootfsSource};
#[cfg(not(target_os = "windows"))]
use a3s_box_core::EXEC_VSOCK_PORT;
#[cfg(target_os = "windows")]
use a3s_box_core::PORT_FWD_VSOCK_PORT;
#[cfg(not(target_os = "windows"))]
use a3s_box_core::{ATTEST_VSOCK_PORT, PORT_FWD_VSOCK_PORT, PTY_VSOCK_PORT};
#[cfg(target_os = "linux")]
use a3s_box_netproxy::spawn_inherited_passt_bridge;
#[cfg(target_os = "macos")]
use a3s_box_netproxy::{spawn_inherited_netproxy, InheritedNetProxyConfig};
use clap::Parser;
use krun::KrunContext;
#[cfg(target_os = "windows")]
use libkrun_sys::{KRUN_KERNEL_FORMAT_ELF, KRUN_KERNEL_FORMAT_IMAGE_GZ};
#[cfg(all(target_os = "windows", test))]
use std::fs;
#[cfg(target_os = "windows")]
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};
use tracing_subscriber::EnvFilter;

#[cfg(target_os = "windows")]
mod windows_port_forward;

use vm_launch::configure_and_start_vm;
#[cfg(target_os = "windows")]
use vm_launch::*;

/// A3S Box Shim - MicroVM subprocess
#[derive(Parser, Debug)]
#[command(name = "a3s-box-shim")]
#[command(about = "MicroVM shim process for A3S Box")]
struct Args {
    /// JSON-encoded InstanceSpec configuration
    #[arg(long)]
    config: Option<String>,

    /// Internal: project one Sandbox generation's split console into its
    /// configured log driver until the exact A3S OCI owner exits.
    #[cfg(target_os = "linux")]
    #[arg(long, hide = true)]
    sandbox_log_worker_config: Option<String>,

    /// Internal: project one exact managed OCI init process into Box logs.
    #[arg(long, hide = true)]
    managed_oci_log_worker_config: Option<String>,

    #[cfg(target_os = "windows")]
    #[arg(long, hide = true)]
    port_fwd_worker: bool,

    #[cfg(target_os = "windows")]
    #[arg(long, hide = true)]
    box_id: Option<String>,

    #[cfg(target_os = "windows")]
    #[arg(long, hide = true)]
    parent_pid: Option<u32>,

    #[cfg(target_os = "windows")]
    #[arg(long, hide = true)]
    ready_file: Option<PathBuf>,

    #[cfg(target_os = "windows")]
    #[arg(long, hide = true)]
    stop_request: Option<PathBuf>,

    #[cfg(target_os = "windows")]
    #[arg(long, hide = true)]
    port_map: Vec<String>,
}

fn main() {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    if let Err(e) = run() {
        tracing::error!(error = %e, "Shim failed");
        std::process::exit(1);
    }
}

/// Opt-in (env `A3S_BOX_KSM=1`): mark this shim's anonymous memory — including
/// libkrun's guest RAM, which `start_enter` allocates as anonymous `mmap` after
/// this runs — as KSM-mergeable via `prctl(PR_SET_MEMORY_MERGE)` (Linux 6.4+).
/// With KSM enabled on the host (`/sys/kernel/mm/ksm/run = 1`), identical pages
/// across same-image microVMs (kernel text, common runtime/libs) are deduplicated
/// by ksmd, so N warm VMs of one image cost far less host RAM than N× their size.
/// Best-effort: a no-op when the env is unset or on pre-6.4 kernels (EINVAL).
#[cfg(target_os = "linux")]
fn maybe_enable_ksm_merge() {
    // PR_SET_MEMORY_MERGE (since Linux 6.4) — not in all libc versions, so use
    // the numeric value directly.
    const PR_SET_MEMORY_MERGE: libc::c_int = 67;

    let enabled = std::env::var("A3S_BOX_KSM")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);
    if !enabled {
        return;
    }

    // SAFETY: PR_SET_MEMORY_MERGE takes a single scalar (enable=1); no pointers
    // or out-params. A non-zero return (e.g. pre-6.4 kernel → EINVAL) is non-fatal.
    let rc = unsafe { libc::prctl(PR_SET_MEMORY_MERGE, 1, 0, 0, 0) };
    if rc == 0 {
        tracing::info!("KSM page-merging enabled for guest memory (PR_SET_MEMORY_MERGE)");
    } else {
        tracing::warn!(
            error = %std::io::Error::last_os_error(),
            "A3S_BOX_KSM set but PR_SET_MEMORY_MERGE failed (needs Linux 6.4+); continuing without KSM"
        );
    }
}

#[cfg(not(target_os = "linux"))]
fn maybe_enable_ksm_merge() {}

fn run() -> Result<()> {
    let args = Args::parse();

    if let Some(config) = args.managed_oci_log_worker_config.as_deref() {
        return managed_oci_log_worker::run(config);
    }

    #[cfg(target_os = "linux")]
    if let Some(config) = args.sandbox_log_worker_config.as_deref() {
        return run_sandbox_log_worker(config);
    }

    #[cfg(target_os = "windows")]
    if args.port_fwd_worker {
        let box_id = args.box_id.ok_or_else(|| BoxError::BoxBootError {
            message: "Missing --box-id for Windows port-forward worker".to_string(),
            hint: None,
        })?;
        let parent_pid = args.parent_pid.ok_or_else(|| BoxError::BoxBootError {
            message: "Missing --parent-pid for Windows port-forward worker".to_string(),
            hint: None,
        })?;
        let ready_file = args.ready_file.ok_or_else(|| BoxError::BoxBootError {
            message: "Missing --ready-file for Windows port-forward worker".to_string(),
            hint: None,
        })?;
        let stop_request = args.stop_request.ok_or_else(|| BoxError::BoxBootError {
            message: "Missing --stop-request for Windows port-forward worker".to_string(),
            hint: None,
        })?;
        return windows_port_forward::run_port_forward_worker(
            &box_id,
            &args.port_map,
            parent_pid,
            &ready_file,
            &stop_request,
        );
    }

    // Parse configuration
    let config = args.config.ok_or_else(|| BoxError::BoxBootError {
        message: "Missing --config".to_string(),
        hint: None,
    })?;
    let spec: InstanceSpec = serde_json::from_str(&config).map_err(|e| BoxError::BoxBootError {
        message: format!("Failed to parse config: {}", e),
        hint: None,
    })?;

    #[cfg(unix)]
    tracing::info!(
        box_id = %spec.box_id,
        vcpus = spec.vcpus,
        memory_mib = spec.memory_mib,
        rootfs = %spec.rootfs,
        net_socket_fd = spec.network.as_ref().and_then(|net| net.net_socket_fd),
        net_proxy_fd = spec.network.as_ref().and_then(|net| net.net_proxy_fd),
        "Starting VM"
    );
    #[cfg(not(unix))]
    tracing::info!(
        box_id = %spec.box_id,
        vcpus = spec.vcpus,
        memory_mib = spec.memory_mib,
        rootfs = %spec.rootfs,
        "Starting VM"
    );

    // Opt-in KSM: mark guest memory mergeable before libkrun allocates it.
    maybe_enable_ksm_merge();

    validate_rootfs_source(&spec.rootfs)?;
    validate_raw_block_devices(&spec)?;
    let _raw_disk_ownership = lock_raw_disk_ownership(&spec)?;

    #[cfg(target_os = "windows")]
    prepare_windows_guest(&spec)?;

    // Validate filesystem mounts exist
    for mount in &spec.fs_mounts {
        if !mount.host_path.exists() {
            return Err(BoxError::BoxBootError {
                message: format!(
                    "Filesystem mount '{}' not found: {}",
                    mount.tag,
                    mount.host_path.display()
                ),
                hint: None,
            });
        }
        tracing::debug!(
            tag = %mount.tag,
            path = %mount.host_path.display(),
            read_only = mount.read_only,
            "Validated filesystem mount"
        );
    }

    // Configure and start VM
    unsafe {
        configure_and_start_vm(&spec)?;
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn run_sandbox_log_worker(config: &str) -> Result<()> {
    use a3s_box_core::log::{
        run_log_processor_with_ready_and_eof_policy, ConsoleEofPolicy, SandboxLogWorkerSpec,
    };
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let spec: SandboxLogWorkerSpec =
        serde_json::from_str(config).map_err(|error| BoxError::BoxBootError {
            message: format!("Failed to parse Sandbox log worker config: {error}"),
            hint: None,
        })?;
    validate_sandbox_log_worker_spec(&spec)?;

    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicUsize::new(0));
    let console_log = spec.console_log.clone();
    let log_dir = console_log
        .parent()
        .ok_or_else(|| BoxError::BoxBootError {
            message: format!(
                "Sandbox console path has no parent: {}",
                console_log.display()
            ),
            hint: None,
        })?
        .to_path_buf();
    let log_config = spec.log_config.clone();
    let processor_stop = Arc::clone(&stop);
    let processor_ready = Arc::clone(&ready);
    let processor = std::thread::spawn(move || {
        run_log_processor_with_ready_and_eof_policy(
            &console_log,
            &log_dir,
            &log_config,
            &processor_stop,
            Some(&processor_ready),
            ConsoleEofPolicy::WriterClosed,
        );
    });

    let deadline = Instant::now() + Duration::from_secs(3);
    while ready.load(Ordering::Acquire) < 2 {
        if processor.is_finished() {
            let _ = processor.join();
            return Err(BoxError::BoxBootError {
                message: "Sandbox log processor exited before opening both console streams"
                    .to_string(),
                hint: None,
            });
        }
        if Instant::now() >= deadline {
            stop.store(true, Ordering::SeqCst);
            let _ = processor.join();
            return Err(BoxError::BoxBootError {
                message: "Sandbox log processor did not become ready before timeout".to_string(),
                hint: None,
            });
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    if let Some(parent) = spec.ready_file.parent() {
        std::fs::create_dir_all(parent).map_err(BoxError::IoError)?;
    }
    let mut ready_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&spec.ready_file)
        .map_err(BoxError::IoError)?;
    ready_file
        .write_all(format!("{}\n", std::process::id()).as_bytes())
        .map_err(BoxError::IoError)?;
    ready_file.sync_all().map_err(BoxError::IoError)?;

    while sandbox_watched_process_is_current(spec.watched_pid, spec.watched_pid_start_time) {
        std::thread::sleep(Duration::from_millis(10));
    }

    // The exact wrapper identity is gone (or a zombie), so its stdout/stderr
    // descriptors are closed. WriterClosed makes the next EOF final and still
    // flushes a trailing partial line.
    stop.store(true, Ordering::SeqCst);
    processor.join().map_err(|_| BoxError::BoxBootError {
        message: format!("Sandbox log processor panicked for {}", spec.box_id),
        hint: None,
    })?;
    tracing::debug!(box_id = %spec.box_id, "Sandbox logs fully drained");
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_sandbox_log_worker_spec(spec: &a3s_box_core::log::SandboxLogWorkerSpec) -> Result<()> {
    if spec.schema != a3s_box_core::log::SANDBOX_LOG_WORKER_SCHEMA
        || spec.box_id.is_empty()
        || spec.watched_pid == 0
        || spec.watched_pid_start_time == 0
        || !spec.console_log.is_absolute()
        || !spec.ready_file.is_absolute()
    {
        return Err(BoxError::BoxBootError {
            message: "Invalid Sandbox log worker identity or path configuration".to_string(),
            hint: None,
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn sandbox_watched_process_is_current(pid: u32, expected_start_time: u64) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    linux_process_identity_from_stat(&stat)
        .is_some_and(|(state, start_time)| state != 'Z' && start_time == expected_start_time)
}

#[cfg(target_os = "linux")]
fn linux_process_identity_from_stat(stat: &str) -> Option<(char, u64)> {
    // `comm` may contain spaces and parentheses. Field 3 (state) begins after
    // the final `)`, and field 22 (starttime) is token 19 from that point.
    let fields: Vec<&str> = stat
        .get(stat.rfind(')')? + 1..)?
        .split_whitespace()
        .collect();
    let state = fields.first()?.chars().next()?;
    let start_time = fields.get(19)?.parse().ok()?;
    Some((state, start_time))
}

/// Parse a Docker-style ulimit string into a krun rlimit string.
///
/// Input format: "RESOURCE=SOFT:HARD" (e.g., "nofile=1024:4096")
/// Output format: "RESOURCE_NUM=SOFT:HARD" (e.g., "7=1024:4096")
///
/// Returns None if the resource name is unrecognized.
fn parse_ulimit(ulimit: &str) -> Option<String> {
    let (name, limits) = ulimit.split_once('=')?;
    let resource_num = match name.to_lowercase().as_str() {
        "core" => 4,        // RLIMIT_CORE
        "cpu" => 0,         // RLIMIT_CPU
        "data" => 2,        // RLIMIT_DATA
        "fsize" => 1,       // RLIMIT_FSIZE
        "locks" => 10,      // RLIMIT_LOCKS
        "memlock" => 8,     // RLIMIT_MEMLOCK
        "msgqueue" => 12,   // RLIMIT_MSGQUEUE
        "nice" => 13,       // RLIMIT_NICE
        "nofile" => 7,      // RLIMIT_NOFILE
        "nproc" => 6,       // RLIMIT_NPROC
        "rss" => 5,         // RLIMIT_RSS
        "rtprio" => 14,     // RLIMIT_RTPRIO
        "rttime" => 15,     // RLIMIT_RTTIME
        "sigpending" => 11, // RLIMIT_SIGPENDING
        "stack" => 3,       // RLIMIT_STACK
        _ => return None,
    };
    Some(format!("{}={}", resource_num, limits))
}

/// Apply CPU pinning via sched_setaffinity (Linux only).
#[cfg(target_os = "linux")]
fn apply_cpuset(cpuset: &str) -> std::result::Result<(), String> {
    use std::mem;

    // Parse comma-separated CPU IDs (e.g., "0,1,3" or "0-3")
    let cpus = parse_cpuset_spec(cpuset)?;
    if cpus.is_empty() {
        return Err("empty cpuset specification".to_string());
    }

    unsafe {
        let mut set: libc::cpu_set_t = mem::zeroed();
        libc::CPU_ZERO(&mut set);
        for cpu in &cpus {
            libc::CPU_SET(*cpu, &mut set);
        }

        let ret = libc::sched_setaffinity(0, mem::size_of::<libc::cpu_set_t>(), &set);
        if ret != 0 {
            return Err(format!(
                "sched_setaffinity failed: {}",
                std::io::Error::last_os_error()
            ));
        }
    }

    tracing::info!(cpus = ?cpus, "Applied CPU pinning");
    Ok(())
}

/// Parse a cpuset specification like "0,1,3" or "0-3" or "0,2-4,7".
#[cfg(target_os = "linux")]
fn parse_cpuset_spec(spec: &str) -> std::result::Result<Vec<usize>, String> {
    let mut cpus = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.contains('-') {
            let range: Vec<&str> = part.split('-').collect();
            if range.len() != 2 {
                return Err(format!("invalid CPU range: {}", part));
            }
            let start: usize = range[0]
                .parse()
                .map_err(|_| format!("invalid CPU number: {}", range[0]))?;
            let end: usize = range[1]
                .parse()
                .map_err(|_| format!("invalid CPU number: {}", range[1]))?;
            if start > end {
                return Err(format!("invalid CPU range: {}-{}", start, end));
            }
            for cpu in start..=end {
                cpus.push(cpu);
            }
        } else {
            let cpu: usize = part
                .parse()
                .map_err(|_| format!("invalid CPU number: {}", part))?;
            cpus.push(cpu);
        }
    }
    Ok(cpus)
}

#[cfg(not(target_os = "windows"))]
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn tsi_port_map_for_spec(spec: &InstanceSpec) -> Vec<String> {
    if native_bridge_port_forwarding_handles_spec(spec) {
        return Vec::new();
    }

    spec.port_map
        .iter()
        .filter(|mapping| !is_auto_assigned_host_port(mapping))
        .cloned()
        .collect()
}

// On both macOS (netproxy) and Linux (passt), bridge-mode published ports are
// forwarded by the native network backend, not TSI. libkrun discards the TSI
// host_port_map once a virtio-net device is attached anyway, so feeding it the
// port map is dead work; let the backend own forwarding instead.
#[cfg(not(target_os = "windows"))]
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn native_bridge_port_forwarding_handles_spec(spec: &InstanceSpec) -> bool {
    spec.network.is_some()
}

#[cfg(not(target_os = "windows"))]
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn is_auto_assigned_host_port(mapping: &str) -> bool {
    mapping
        .split_once(':')
        .and_then(|(host, _)| host.parse::<u16>().ok())
        == Some(0)
}

#[cfg(target_os = "windows")]
const WINDOWS_GUEST_EXIT_CODE: &str = ".a3s_exit_code";
#[cfg(target_os = "windows")]
const WINDOWS_GUEST_STDOUT: &str = "guest-init.stdout.log";
#[cfg(target_os = "windows")]
const WINDOWS_GUEST_STDERR: &str = "guest-init.stderr.log";
#[cfg(target_os = "windows")]
const WINDOWS_GUEST_RESULT_MARKER: &str = ".a3s_host_result_collected";
#[cfg(target_os = "windows")]
const WINDOWS_LIVE_LOGS_DRAINED_MARKER: &str = ".a3s_host_live_logs_drained";
#[cfg(target_os = "windows")]
const WINDOWS_RETURN_ON_EXIT_ENV: &str = "LIBKRUN_WINDOWS_RETURN_ON_EXIT";

fn validate_rootfs_source(rootfs: &RootfsSource) -> Result<()> {
    let path = rootfs.path();
    let metadata = std::fs::symlink_metadata(path).map_err(|error| BoxError::BoxBootError {
        message: format!("Rootfs not found at {}: {error}", path.display()),
        hint: Some("Ensure the guest rootfs is properly prepared before boot".to_string()),
    })?;

    match rootfs {
        RootfsSource::Directory { .. } if metadata.is_dir() => Ok(()),
        RootfsSource::Directory { .. } => Err(BoxError::BoxBootError {
            message: format!("Directory rootfs is not a directory: {}", path.display()),
            hint: None,
        }),
        RootfsSource::Ext4Disk { .. } if !metadata.is_file() => Err(BoxError::BoxBootError {
            message: format!("Ext4 root disk is not a regular file: {}", path.display()),
            hint: None,
        }),
        RootfsSource::Ext4Disk { .. } if metadata.len() == 0 => Err(BoxError::BoxBootError {
            message: format!("Ext4 root disk is empty: {}", path.display()),
            hint: Some("Rebuild the guest root disk before boot".to_string()),
        }),
        RootfsSource::Ext4Disk { .. } => {
            #[cfg(target_os = "windows")]
            return Err(BoxError::BoxBootError {
                message: format!(
                    "Guest-native ext4 root disks are not supported on Windows: {}",
                    path.display()
                ),
                hint: Some("Use a directory rootfs on Windows".to_string()),
            });

            #[cfg(not(target_os = "windows"))]
            Ok(())
        }
    }
}

fn validate_raw_block_devices(spec: &InstanceSpec) -> Result<()> {
    const MAX_RAW_BLOCK_DEVICES: usize = 16;
    if spec.block_devices.len() > MAX_RAW_BLOCK_DEVICES {
        return Err(BoxError::BoxBootError {
            message: format!(
                "Too many raw block devices: {} exceeds {}",
                spec.block_devices.len(),
                MAX_RAW_BLOCK_DEVICES
            ),
            hint: None,
        });
    }

    let mut ids = std::collections::HashSet::new();
    let mut paths = std::collections::HashSet::new();
    for disk in &spec.block_devices {
        validate_raw_block_device(disk)?;
        if disk.id == "rootfs" || !ids.insert(disk.id.as_str()) {
            return Err(BoxError::BoxBootError {
                message: format!("Duplicate or reserved raw block device id: {}", disk.id),
                hint: None,
            });
        }
        if !paths.insert(disk.path.as_path()) || spec.rootfs.path() == disk.path {
            return Err(BoxError::BoxBootError {
                message: format!(
                    "Raw block device path is attached more than once: {}",
                    disk.path.display()
                ),
                hint: None,
            });
        }
    }
    Ok(())
}

fn validate_raw_block_device(disk: &RawBlockDevice) -> Result<()> {
    if disk.id.is_empty()
        || disk.id.len() > 64
        || !disk
            .id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(BoxError::BoxBootError {
            message: format!("Invalid raw block device id: {:?}", disk.id),
            hint: None,
        });
    }
    if !disk.path.is_absolute() {
        return Err(BoxError::BoxBootError {
            message: format!(
                "Raw block device path must be absolute: {}",
                disk.path.display()
            ),
            hint: None,
        });
    }
    let metadata =
        std::fs::symlink_metadata(&disk.path).map_err(|error| BoxError::BoxBootError {
            message: format!(
                "Raw block device not found at {}: {error}",
                disk.path.display()
            ),
            hint: None,
        })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() == 0 {
        return Err(BoxError::BoxBootError {
            message: format!(
                "Raw block device is not a non-empty plain file: {}",
                disk.path.display()
            ),
            hint: None,
        });
    }

    #[cfg(target_os = "windows")]
    return Err(BoxError::BoxBootError {
        message: format!(
            "Raw auxiliary block devices are not supported on Windows: {}",
            disk.path.display()
        ),
        hint: None,
    });

    #[cfg(not(target_os = "windows"))]
    Ok(())
}

/// Hold an advisory exclusive lock for every raw disk throughout the libkrun
/// process lifetime. All A3S shims participate, so an orphaned read-only
/// maintenance VM and a normal writable boot can never own one generation at
/// the same time.
#[cfg(unix)]
fn lock_raw_disk_ownership(spec: &InstanceSpec) -> Result<Vec<std::fs::File>> {
    use std::os::fd::AsRawFd;

    let mut disks = Vec::new();
    if let RootfsSource::Ext4Disk { path, read_only } = &spec.rootfs {
        disks.push((path.as_path(), *read_only));
    }
    disks.extend(
        spec.block_devices
            .iter()
            .map(|disk| (disk.path.as_path(), disk.read_only)),
    );

    let mut guards = Vec::with_capacity(disks.len());
    for (path, read_only) in disks {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(!read_only)
            .open(path)
            .map_err(|error| BoxError::BoxBootError {
                message: format!("Failed to open raw disk {}: {error}", path.display()),
                hint: None,
            })?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(BoxError::StateError(format!(
                "Raw disk {} is already owned by another A3S VM",
                path.display()
            )));
        }
        guards.push(file);
    }
    Ok(guards)
}

#[cfg(not(unix))]
fn lock_raw_disk_ownership(_spec: &InstanceSpec) -> Result<Vec<std::fs::File>> {
    Ok(Vec::new())
}

#[cfg(target_os = "windows")]
fn windows_rootfs_path(spec: &InstanceSpec) -> Result<&Path> {
    spec.rootfs
        .directory_path()
        .ok_or_else(|| BoxError::BoxBootError {
            message: "Windows guest preparation requires a directory rootfs".to_string(),
            hint: Some("Use a directory rootfs on Windows".to_string()),
        })
}

#[cfg(unix)]
fn log_inherited_net_fd(fd: i32) {
    let fd_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    let file_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };

    let mut sock_type: libc::c_int = 0;
    let mut opt_len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    let sock_type_ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            &mut sock_type as *mut _ as *mut libc::c_void,
            &mut opt_len,
        )
    };

    tracing::info!(
        fd,
        fd_flags,
        file_flags,
        sock_type_ret,
        sock_type,
        last_os_error = %std::io::Error::last_os_error(),
        "Validated inherited network socket fd"
    );
}

/// Apply OCI USER directive to the krun context.
///
/// Supports formats:
/// - "uid" (e.g., "1000")
/// - "uid:gid" (e.g., "1000:1000")
/// - Non-numeric names are logged and skipped (would require /etc/passwd lookup)
unsafe fn apply_user_config(ctx: &KrunContext, user: &str) -> Result<()> {
    if user.is_empty() {
        return Ok(());
    }

    let parts: Vec<&str> = user.split(':').collect();
    let uid_str = parts[0];
    let gid_str = parts.get(1).copied();

    // Parse UID
    match uid_str.parse::<u32>() {
        Ok(uid) => {
            tracing::info!(uid, "Setting VM user from OCI USER directive");
            ctx.set_uid(uid)?;
        }
        Err(_) => {
            // Non-numeric user name — would need /etc/passwd lookup inside rootfs
            tracing::warn!(
                user = uid_str,
                "Non-numeric USER directive; skipping (name lookup not yet supported)"
            );
            return Ok(());
        }
    }

    // Parse GID if present
    if let Some(gid_str) = gid_str {
        match gid_str.parse::<u32>() {
            Ok(gid) => {
                tracing::info!(gid, "Setting VM group from OCI USER directive");
                ctx.set_gid(gid)?;
            }
            Err(_) => {
                tracing::warn!(
                    group = gid_str,
                    "Non-numeric group in USER directive; skipping"
                );
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests;
