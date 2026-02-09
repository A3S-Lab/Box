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

use a3s_box_core::error::{BoxError, Result};
use a3s_box_runtime::krun::KrunContext;
use a3s_box_runtime::vmm::InstanceSpec;
use a3s_box_runtime::AGENT_VSOCK_PORT;
use a3s_box_runtime::EXEC_VSOCK_PORT;
use clap::Parser;
use tracing_subscriber::EnvFilter;

/// A3S Box Shim - MicroVM subprocess
#[derive(Parser, Debug)]
#[command(name = "a3s-box-shim")]
#[command(about = "MicroVM shim process for A3S Box")]
struct Args {
    /// JSON-encoded InstanceSpec configuration
    #[arg(long)]
    config: String,
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

fn run() -> Result<()> {
    let args = Args::parse();

    // Parse configuration
    let spec: InstanceSpec =
        serde_json::from_str(&args.config).map_err(|e| BoxError::BoxBootError {
            message: format!("Failed to parse config: {}", e),
            hint: None,
        })?;

    tracing::info!(
        box_id = %spec.box_id,
        vcpus = spec.vcpus,
        memory_mib = spec.memory_mib,
        rootfs = %spec.rootfs_path.display(),
        "Starting VM"
    );

    // Validate rootfs exists
    if !spec.rootfs_path.exists() {
        return Err(BoxError::BoxBootError {
            message: format!("Rootfs not found: {}", spec.rootfs_path.display()),
            hint: Some("Ensure the guest rootfs is properly set up".to_string()),
        });
    }

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

/// Configure libkrun context and start the VM.
///
/// # Safety
/// This function calls unsafe libkrun FFI functions.
/// It performs process takeover on success - the function never returns.
unsafe fn configure_and_start_vm(spec: &InstanceSpec) -> Result<()> {
    // Initialize libkrun logging
    tracing::debug!("Initializing libkrun logging");
    if let Err(e) = KrunContext::init_logging() {
        tracing::warn!(error = %e, "Failed to initialize libkrun logging");
    }

    // Create libkrun context
    tracing::debug!("Creating libkrun context");
    let ctx = KrunContext::create()?;

    // Configure VM resources
    tracing::debug!(
        vcpus = spec.vcpus,
        memory_mib = spec.memory_mib,
        "Setting VM config"
    );
    ctx.set_vm_config(spec.vcpus, spec.memory_mib)?;

    // Raise RLIMIT_NOFILE to maximum - CRITICAL for virtio-fs
    #[cfg(unix)]
    {
        use libc::{getrlimit, rlimit, setrlimit, RLIMIT_NOFILE};
        let mut rlim = rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if getrlimit(RLIMIT_NOFILE, &mut rlim) == 0 {
            rlim.rlim_cur = rlim.rlim_max;
            if setrlimit(RLIMIT_NOFILE, &rlim) != 0 {
                tracing::warn!("Failed to raise RLIMIT_NOFILE");
            } else {
                tracing::debug!(limit = rlim.rlim_cur, "RLIMIT_NOFILE raised");
            }
        }
    }

    // Configure guest rlimits
    let rlimits = vec![
        "6=4096:8192".to_string(),       // RLIMIT_NPROC = 6
        "7=1048576:1048576".to_string(), // RLIMIT_NOFILE = 7
    ];
    tracing::debug!(rlimits = ?rlimits, "Configuring guest rlimits");
    ctx.set_rlimits(&rlimits)?;

    // Add filesystem mounts via virtiofs
    tracing::info!("Adding filesystem mounts via virtiofs:");
    for mount in &spec.fs_mounts {
        let path_str = mount
            .host_path
            .to_str()
            .ok_or_else(|| BoxError::BoxBootError {
                message: format!("Invalid path: {}", mount.host_path.display()),
                hint: None,
            })?;

        tracing::info!(
            "  {} → {} ({})",
            mount.tag,
            mount.host_path.display(),
            if mount.read_only { "ro" } else { "rw" }
        );
        ctx.add_virtiofs(&mount.tag, path_str)?;
    }

    // Set root filesystem
    let rootfs_str = spec
        .rootfs_path
        .to_str()
        .ok_or_else(|| BoxError::BoxBootError {
            message: format!("Invalid rootfs path: {}", spec.rootfs_path.display()),
            hint: None,
        })?;
    tracing::debug!(rootfs = rootfs_str, "Setting root filesystem");
    ctx.set_root(rootfs_str)?;

    // Set working directory
    tracing::debug!(workdir = %spec.workdir, "Setting working directory");
    ctx.set_workdir(&spec.workdir)?;

    // Set entrypoint
    tracing::debug!(
        executable = %spec.entrypoint.executable,
        args = ?spec.entrypoint.args,
        "Setting entrypoint"
    );
    ctx.set_exec(
        &spec.entrypoint.executable,
        &spec.entrypoint.args,
        &spec.entrypoint.env,
    )?;

    // Configure gRPC communication channel (Unix socket bridged to vsock)
    // listen=true: libkrun creates socket, host connects, guest accepts via vsock
    let grpc_socket_str = spec
        .grpc_socket_path
        .to_str()
        .ok_or_else(|| BoxError::BoxBootError {
            message: format!(
                "Invalid gRPC socket path: {}",
                spec.grpc_socket_path.display()
            ),
            hint: None,
        })?;
    tracing::debug!(
        socket_path = grpc_socket_str,
        guest_port = AGENT_VSOCK_PORT,
        "Configuring vsock bridge for gRPC"
    );
    ctx.add_vsock_port(AGENT_VSOCK_PORT, grpc_socket_str, true)?;

    // Configure exec communication channel (Unix socket bridged to vsock port 4089)
    let exec_socket_str = spec
        .exec_socket_path
        .to_str()
        .ok_or_else(|| BoxError::BoxBootError {
            message: format!(
                "Invalid exec socket path: {}",
                spec.exec_socket_path.display()
            ),
            hint: None,
        })?;
    tracing::debug!(
        socket_path = exec_socket_str,
        guest_port = EXEC_VSOCK_PORT,
        "Configuring vsock bridge for exec"
    );
    ctx.add_vsock_port(EXEC_VSOCK_PORT, exec_socket_str, true)?;

    // Configure console output if specified
    if let Some(console_path) = &spec.console_output {
        let console_str = console_path
            .to_str()
            .ok_or_else(|| BoxError::BoxBootError {
                message: format!("Invalid console output path: {}", console_path.display()),
                hint: None,
            })?;
        tracing::debug!(console_path = console_str, "Redirecting console output");
        ctx.set_console_output(console_str)?;
    }

    // Configure TEE if specified (only available on Linux with SEV support)
    #[cfg(target_os = "linux")]
    if let Some(ref tee_config) = spec.tee_config {
        tracing::info!(
            tee_type = %tee_config.tee_type,
            config_path = %tee_config.config_path.display(),
            "Configuring TEE"
        );

        // Enable split IRQ chip (required for TEE)
        ctx.enable_split_irqchip()?;

        // Set TEE configuration file
        let tee_config_str = tee_config
            .config_path
            .to_str()
            .ok_or_else(|| BoxError::TeeConfig(format!(
                "Invalid TEE config path: {}",
                tee_config.config_path.display()
            )))?;
        ctx.set_tee_config(tee_config_str)?;

        tracing::info!("TEE configured successfully");
    }

    #[cfg(not(target_os = "linux"))]
    if spec.tee_config.is_some() {
        tracing::warn!("TEE configuration is only supported on Linux; ignoring");
    }

    // Start VM (process takeover - never returns on success)
    tracing::info!(box_id = %spec.box_id, "Starting VM (process takeover)");
    let status = ctx.start_enter();

    // If we reach here, either:
    // 1. VM failed to start (negative status)
    // 2. VM started and guest exited (non-negative status)
    if status < 0 {
        if status == -22 {
            return Err(BoxError::BoxBootError {
                message: "libkrun returned EINVAL - invalid configuration".to_string(),
                hint: Some("Check VM configuration (rootfs, entrypoint, etc.)".to_string()),
            });
        }
        Err(BoxError::BoxBootError {
            message: format!("VM failed to start with status {}", status),
            hint: None,
        })
    } else {
        // VM started and guest exited - this is success
        tracing::info!(exit_status = status, "VM exited");
        Ok(())
    }
}
