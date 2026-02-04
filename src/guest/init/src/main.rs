//! Guest init process for a3s-box VM.
//!
//! This process runs as PID 1 inside the MicroVM and is responsible for:
//! - Setting up the guest environment
//! - Creating isolated namespaces for agent and business code
//! - Launching the agent process
//! - Managing process lifecycle

use a3s_box_guest_init::namespace;
use std::process;
use tracing::{error, info};

fn main() {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    info!("a3s-box guest init starting (PID {})", process::id());

    // Run init process
    if let Err(e) = run_init() {
        error!("Init process failed: {}", e);
        process::exit(1);
    }

    info!("Init process completed successfully");
}

fn run_init() -> Result<(), Box<dyn std::error::Error>> {
    // Step 1: Mount essential filesystems
    mount_essential_filesystems()?;

    // Step 2: Mount virtio-fs shares
    mount_virtio_fs_shares()?;

    // Step 3: Create namespace for agent
    info!("Creating isolated namespace for agent");
    let agent_config = namespace::NamespaceConfig::default();

    // Step 4: Launch agent in isolated namespace
    info!("Launching agent process");
    let agent_pid = namespace::spawn_isolated(
        &agent_config,
        "/agent/bin/agent",
        &["--listen", "vsock://4088"],
        &[],
        "/agent",
    )?;

    info!("Agent started with PID {}", agent_pid);

    // Step 5: Wait for agent process (reap zombies)
    wait_for_children()?;

    Ok(())
}

/// Mount essential filesystems (/proc, /sys, /dev).
fn mount_essential_filesystems() -> Result<(), Box<dyn std::error::Error>> {
    info!("Mounting essential filesystems");

    // Note: mount() signature differs between Linux and macOS in nix crate
    // On Linux: mount(source, target, fstype, flags, data)
    // On macOS: mount(source, target, flags, data)
    // This code is meant to run on Linux inside the VM

    #[cfg(target_os = "linux")]
    {
        use nix::mount::{mount, MsFlags};

        // Mount /proc
        mount(
            Some("proc"),
            "/proc",
            Some("proc"),
            MsFlags::empty(),
            None::<&str>,
        )?;

        // Mount /sys
        mount(
            Some("sysfs"),
            "/sys",
            Some("sysfs"),
            MsFlags::empty(),
            None::<&str>,
        )?;

        // Mount /dev (devtmpfs)
        mount(
            Some("devtmpfs"),
            "/dev",
            Some("devtmpfs"),
            MsFlags::empty(),
            None::<&str>,
        )?;
    }

    #[cfg(not(target_os = "linux"))]
    {
        // On non-Linux platforms (e.g., macOS for development),
        // skip mounting as this code won't actually run
        info!("Skipping mount on non-Linux platform (development mode)");
    }

    Ok(())
}

/// Mount virtio-fs shares for workspace and skills.
fn mount_virtio_fs_shares() -> Result<(), Box<dyn std::error::Error>> {
    info!("Mounting virtio-fs shares");

    #[cfg(target_os = "linux")]
    {
        use nix::mount::{mount, MsFlags};

        // Mount workspace share
        mount(
            Some("workspace"),
            "/workspace",
            Some("virtiofs"),
            MsFlags::empty(),
            None::<&str>,
        )?;

        // Mount skills share
        mount(
            Some("skills"),
            "/skills",
            Some("virtiofs"),
            MsFlags::MS_RDONLY,
            None::<&str>,
        )?;
    }

    #[cfg(not(target_os = "linux"))]
    {
        info!("Skipping virtio-fs mount on non-Linux platform (development mode)");
    }

    Ok(())
}

/// Wait for all child processes and reap zombies.
fn wait_for_children() -> Result<(), Box<dyn std::error::Error>> {
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
    use nix::unistd::Pid;

    info!("Waiting for child processes");

    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(pid, status)) => {
                info!("Child process {} exited with status {}", pid, status);
                if status != 0 {
                    error!("Child process failed");
                    return Err(format!("Child process {} failed with status {}", pid, status).into());
                }
            }
            Ok(WaitStatus::Signaled(pid, signal, _)) => {
                error!("Child process {} killed by signal {:?}", pid, signal);
                return Err(format!("Child process {} killed by signal {:?}", pid, signal).into());
            }
            Ok(WaitStatus::StillAlive) => {
                // No children to reap, sleep briefly
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Ok(_) => {
                // Other status, continue waiting
            }
            Err(nix::errno::Errno::ECHILD) => {
                // No more children
                info!("No more child processes");
                break;
            }
            Err(e) => {
                return Err(format!("waitpid failed: {}", e).into());
            }
        }
    }

    Ok(())
}
