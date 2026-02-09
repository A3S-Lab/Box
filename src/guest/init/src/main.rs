//! Guest init process for a3s-box VM.
//!
//! This process runs as PID 1 inside the MicroVM and is responsible for:
//! - Setting up the guest environment
//! - Creating isolated namespaces for agent and business code
//! - Launching the agent process
//! - Managing process lifecycle
//! - Handling SIGTERM for graceful shutdown

use a3s_box_guest_init::{exec_server, namespace};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{error, info, warn};

/// Agent configuration parsed from environment variables.
struct AgentConfig {
    /// Agent executable path
    executable: String,
    /// Agent arguments
    args: Vec<String>,
    /// Agent environment variables
    env: Vec<(String, String)>,
}

impl AgentConfig {
    /// Parse agent configuration from environment variables.
    ///
    /// Expected environment variables:
    /// - A3S_AGENT_EXEC: agent executable path
    /// - A3S_AGENT_ARGS: agent arguments (space-separated)
    /// - A3S_AGENT_ENV_*: agent environment variables
    fn from_env() -> Self {
        let executable =
            std::env::var("A3S_AGENT_EXEC").unwrap_or_else(|_| "/agent/bin/agent".to_string());

        let args: Vec<String> = std::env::var("A3S_AGENT_ARGS")
            .map(|s| s.split_whitespace().map(String::from).collect())
            .unwrap_or_else(|_| vec!["--listen".to_string(), "vsock://4088".to_string()]);

        // Collect A3S_AGENT_ENV_* variables
        let env: Vec<(String, String)> = std::env::vars()
            .filter_map(|(key, value)| {
                if let Some(stripped) = key.strip_prefix("A3S_AGENT_ENV_") {
                    Some((stripped.to_string(), value))
                } else {
                    None
                }
            })
            .collect();

        Self {
            executable,
            args,
            env,
        }
    }
}

/// Global flag set by the SIGTERM handler to request graceful shutdown.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Register a SIGTERM handler that sets the shutdown flag.
///
/// As PID 1 inside the VM, we must explicitly handle SIGTERM — the kernel
/// does not deliver unhandled signals to init. When the host kills the shim
/// process, libkrun triggers a guest shutdown and the kernel sends SIGTERM
/// to PID 1.
#[cfg(target_os = "linux")]
fn register_sigterm_handler() -> Result<(), Box<dyn std::error::Error>> {
    use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};

    let handler = SigHandler::Handler(sigterm_handler);
    let action = SigAction::new(handler, SaFlags::empty(), SigSet::empty());
    unsafe { sigaction(Signal::SIGTERM, &action)? };
    info!("Registered SIGTERM handler");
    Ok(())
}

#[cfg(target_os = "linux")]
extern "C" fn sigterm_handler(_: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

#[cfg(not(target_os = "linux"))]
fn register_sigterm_handler() -> Result<(), Box<dyn std::error::Error>> {
    info!("Skipping SIGTERM handler on non-Linux platform (development mode)");
    Ok(())
}

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

    // Step 3: Register SIGTERM handler before spawning any children
    register_sigterm_handler()?;

    // Step 4: Parse agent configuration from environment
    let agent_config = AgentConfig::from_env();
    info!(
        executable = %agent_config.executable,
        args = ?agent_config.args,
        env_count = agent_config.env.len(),
        "Agent configuration loaded"
    );

    // Step 5: Create namespace for agent
    info!("Creating isolated namespace for agent");
    let namespace_config = namespace::NamespaceConfig::default();

    // Step 6: Launch agent in isolated namespace
    info!("Launching agent process");

    // Convert args to &str for spawn_isolated
    let args_refs: Vec<&str> = agent_config.args.iter().map(|s| s.as_str()).collect();
    let env_refs: Vec<(&str, &str)> = agent_config
        .env
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let agent_pid = namespace::spawn_isolated(
        &namespace_config,
        &agent_config.executable,
        &args_refs,
        &env_refs,
        "/agent",
    )?;

    info!("Agent started with PID {}", agent_pid);

    // Step 7: Start exec server in background thread
    std::thread::spawn(|| {
        if let Err(e) = exec_server::run_exec_server() {
            error!("Exec server failed: {}", e);
        }
    });

    // Step 8: Wait for agent process (reap zombies, handle SIGTERM)
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

/// Mount virtio-fs shares for workspace, skills, and user volumes.
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

        // Mount user-defined volumes from environment variables
        // Format: A3S_VOL_<index>=<tag>:<guest_path>[:ro]
        mount_user_volumes()?;
    }

    #[cfg(not(target_os = "linux"))]
    {
        info!("Skipping virtio-fs mount on non-Linux platform (development mode)");
    }

    Ok(())
}

/// Mount user-defined volumes passed via A3S_VOL_* environment variables.
///
/// Each variable has the format: `<tag>:<guest_path>[:ro]`
#[cfg(target_os = "linux")]
fn mount_user_volumes() -> Result<(), Box<dyn std::error::Error>> {
    use nix::mount::{mount, MsFlags};

    let mut index = 0;
    loop {
        let env_key = format!("A3S_VOL_{}", index);
        match std::env::var(&env_key) {
            Ok(value) => {
                let parts: Vec<&str> = value.split(':').collect();
                if parts.len() < 2 {
                    error!("Invalid volume spec in {}: {}", env_key, value);
                    index += 1;
                    continue;
                }

                let tag = parts[0];
                let guest_path = parts[1];
                let read_only = parts.get(2).map(|&m| m == "ro").unwrap_or(false);

                info!(
                    tag = tag,
                    guest_path = guest_path,
                    read_only = read_only,
                    "Mounting user volume"
                );

                // Ensure mount point exists
                std::fs::create_dir_all(guest_path)?;

                let flags = if read_only {
                    MsFlags::MS_RDONLY
                } else {
                    MsFlags::empty()
                };
                mount(
                    Some(tag),
                    guest_path,
                    Some("virtiofs"),
                    flags,
                    None::<&str>,
                )?;

                index += 1;
            }
            Err(_) => break,
        }
    }

    if index > 0 {
        info!("Mounted {} user volume(s)", index);
    }

    Ok(())
}

/// Wait for all child processes and reap zombies.
///
/// When SIGTERM is received (via the global `SHUTDOWN_REQUESTED` flag):
/// 1. Forward SIGTERM to all child processes
/// 2. Wait up to 5 seconds for children to exit
/// 3. Send SIGKILL to any remaining children
/// 4. Call sync() to flush filesystem buffers
fn wait_for_children() -> Result<(), Box<dyn std::error::Error>> {
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
    use nix::unistd::Pid;

    /// Maximum time to wait for children after forwarding SIGTERM (5 seconds).
    const CHILD_SHUTDOWN_TIMEOUT_MS: u64 = 5000;

    info!("Waiting for child processes");

    loop {
        // Check if shutdown was requested via SIGTERM
        if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
            info!("SIGTERM received, initiating graceful shutdown");
            graceful_shutdown(CHILD_SHUTDOWN_TIMEOUT_MS);
            return Ok(());
        }

        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(pid, status)) => {
                info!("Child process {} exited with status {}", pid, status);
                if status != 0 {
                    error!("Child process failed");
                    return Err(
                        format!("Child process {} failed with status {}", pid, status).into(),
                    );
                }
            }
            Ok(WaitStatus::Signaled(pid, signal, _)) => {
                // If we're shutting down, a child killed by signal is expected
                if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
                    info!("Child process {} terminated by signal {:?} during shutdown", pid, signal);
                } else {
                    error!("Child process {} killed by signal {:?}", pid, signal);
                    return Err(format!("Child process {} killed by signal {:?}", pid, signal).into());
                }
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

/// Perform graceful shutdown: forward SIGTERM to children, wait, then force-kill.
fn graceful_shutdown(timeout_ms: u64) {
    // Step 1: Send SIGTERM to all processes (except ourselves, PID 1)
    #[cfg(target_os = "linux")]
    {
        info!("Forwarding SIGTERM to all child processes");
        // kill(-1, SIGTERM) sends to all processes except PID 1
        unsafe {
            libc::kill(-1, libc::SIGTERM);
        }
    }

    // Step 2: Wait for children to exit with timeout
    let start = std::time::Instant::now();
    loop {
        use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
        use nix::unistd::Pid;

        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(pid, status)) => {
                info!("Child {} exited with status {} during shutdown", pid, status);
            }
            Ok(WaitStatus::Signaled(pid, signal, _)) => {
                info!("Child {} terminated by {:?} during shutdown", pid, signal);
            }
            Ok(WaitStatus::StillAlive) => {
                if start.elapsed().as_millis() > timeout_ms as u128 {
                    warn!("Shutdown timeout reached, sending SIGKILL to remaining children");
                    #[cfg(target_os = "linux")]
                    unsafe {
                        libc::kill(-1, libc::SIGKILL);
                    }
                    // Reap any remaining
                    loop {
                        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                            Ok(WaitStatus::StillAlive) | Err(nix::errno::Errno::ECHILD) => break,
                            _ => continue,
                        }
                    }
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Ok(_) => {
                // Other status, continue
            }
            Err(nix::errno::Errno::ECHILD) => {
                info!("All children exited during shutdown");
                break;
            }
            Err(e) => {
                warn!("waitpid error during shutdown: {}", e);
                break;
            }
        }
    }

    // Step 3: Sync filesystem buffers
    info!("Syncing filesystem buffers");
    #[cfg(target_os = "linux")]
    unsafe {
        libc::sync();
    }

    info!("Graceful shutdown complete");
}
