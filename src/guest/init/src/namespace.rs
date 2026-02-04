//! Linux namespace isolation for agent and business code.
//!
//! Provides utilities to spawn processes in isolated namespaces.

#[cfg(target_os = "linux")]
use nix::sched::{unshare, CloneFlags};

use nix::unistd::{fork, ForkResult};
use std::os::unix::process::CommandExt;
use std::process::Command;
use thiserror::Error;

/// Namespace isolation errors.
#[derive(Debug, Error)]
pub enum NamespaceError {
    #[error("Fork failed: {0}")]
    ForkFailed(#[from] nix::Error),

    #[error("Unshare failed: {0}")]
    UnshareFailed(nix::Error),

    #[error("Exec failed: {0}")]
    ExecFailed(std::io::Error),

    #[error("Invalid command: {0}")]
    InvalidCommand(String),
}

/// Namespace configuration for process isolation.
#[derive(Debug, Clone)]
pub struct NamespaceConfig {
    /// Separate filesystem view (mount namespace)
    pub mount: bool,

    /// Separate process tree (PID namespace)
    pub pid: bool,

    /// Separate IPC (IPC namespace)
    pub ipc: bool,

    /// Separate hostname (UTS namespace)
    pub uts: bool,

    /// Separate network (network namespace)
    /// Usually false to allow agent-business communication
    pub net: bool,
}

impl Default for NamespaceConfig {
    fn default() -> Self {
        Self {
            mount: true,
            pid: true,
            ipc: true,
            uts: true,
            net: false, // Share network for communication
        }
    }
}

impl NamespaceConfig {
    /// Create a namespace config with all isolation enabled.
    pub fn full_isolation() -> Self {
        Self {
            mount: true,
            pid: true,
            ipc: true,
            uts: true,
            net: true,
        }
    }

    /// Create a namespace config with minimal isolation (mount + PID only).
    pub fn minimal() -> Self {
        Self {
            mount: true,
            pid: true,
            ipc: false,
            uts: false,
            net: false,
        }
    }

    /// Convert to CloneFlags for unshare().
    #[cfg(target_os = "linux")]
    fn to_clone_flags(&self) -> CloneFlags {
        let mut flags = CloneFlags::empty();

        if self.mount {
            flags |= CloneFlags::CLONE_NEWNS;
        }
        if self.pid {
            flags |= CloneFlags::CLONE_NEWPID;
        }
        if self.ipc {
            flags |= CloneFlags::CLONE_NEWIPC;
        }
        if self.uts {
            flags |= CloneFlags::CLONE_NEWUTS;
        }
        if self.net {
            flags |= CloneFlags::CLONE_NEWNET;
        }

        flags
    }

    /// Stub for non-Linux platforms (development only).
    #[cfg(not(target_os = "linux"))]
    fn to_clone_flags(&self) -> u32 {
        0 // Placeholder for non-Linux
    }
}

/// Spawn a process in isolated namespaces.
///
/// # Arguments
///
/// * `config` - Namespace isolation configuration
/// * `command` - Path to executable
/// * `args` - Command arguments
/// * `env` - Environment variables (key-value pairs)
/// * `workdir` - Working directory
///
/// # Returns
///
/// PID of the spawned process in the parent namespace.
///
/// # Errors
///
/// Returns error if fork, unshare, or exec fails.
pub fn spawn_isolated(
    config: &NamespaceConfig,
    command: &str,
    args: &[&str],
    env: &[(&str, &str)],
    workdir: &str,
) -> Result<u32, NamespaceError> {
    tracing::info!(
        command = %command,
        args = ?args,
        workdir = %workdir,
        "Spawning process in isolated namespace"
    );

    // Fork to create child process
    match unsafe { fork() }.map_err(NamespaceError::ForkFailed)? {
        ForkResult::Child => {
            // Child process: create namespaces and exec
            if let Err(e) = child_process(config, command, args, env, workdir) {
                tracing::error!("Child process failed: {}", e);
                std::process::exit(1);
            }
            unreachable!("exec should not return");
        }
        ForkResult::Parent { child } => {
            // Parent process: return child PID
            let pid = child.as_raw() as u32;
            tracing::info!(pid = pid, "Child process spawned");
            Ok(pid)
        }
    }
}

/// Child process logic: create namespaces and exec command.
#[cfg(target_os = "linux")]
fn child_process(
    config: &NamespaceConfig,
    command: &str,
    args: &[&str],
    env: &[(&str, &str)],
    workdir: &str,
) -> Result<(), NamespaceError> {
    // Create new namespaces
    let flags = config.to_clone_flags();
    unshare(flags).map_err(NamespaceError::UnshareFailed)?;

    tracing::debug!("Namespaces created: {:?}", config);

    // If PID namespace was created, we need to fork again
    // so the child becomes PID 1 in the new namespace
    if config.pid {
        match unsafe { fork() }.map_err(NamespaceError::ForkFailed)? {
            ForkResult::Child => {
                // This is PID 1 in the new namespace
                tracing::debug!("Now PID 1 in new namespace");
            }
            ForkResult::Parent { child } => {
                // Wait for the child (PID 1 in new namespace)
                use nix::sys::wait::{waitpid, WaitStatus};

                match waitpid(child, None) {
                    Ok(WaitStatus::Exited(_, status)) => {
                        std::process::exit(status);
                    }
                    Ok(WaitStatus::Signaled(_, signal, _)) => {
                        tracing::error!("Child killed by signal {:?}", signal);
                        std::process::exit(128 + signal as i32);
                    }
                    Ok(_) => {
                        std::process::exit(1);
                    }
                    Err(e) => {
                        tracing::error!("waitpid failed: {}", e);
                        std::process::exit(1);
                    }
                }
            }
        }
    }

    // Execute the command
    let mut cmd = Command::new(command);
    cmd.args(args).current_dir(workdir);

    // Set environment variables
    for (key, value) in env {
        cmd.env(key, value);
    }

    tracing::debug!("Executing command: {} {:?}", command, args);

    // Replace current process with the command
    let err = cmd.exec();

    // If exec returns, it failed
    Err(NamespaceError::ExecFailed(err))
}

/// Child process logic for non-Linux platforms (development stub).
#[cfg(not(target_os = "linux"))]
fn child_process(
    _config: &NamespaceConfig,
    command: &str,
    args: &[&str],
    env: &[(&str, &str)],
    workdir: &str,
) -> Result<(), NamespaceError> {
    // On non-Linux, just exec without namespace isolation
    tracing::warn!("Namespace isolation not available on this platform");

    let mut cmd = Command::new(command);
    cmd.args(args).current_dir(workdir);

    for (key, value) in env {
        cmd.env(key, value);
    }

    let err = cmd.exec();
    Err(NamespaceError::ExecFailed(err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_namespace_config_default() {
        let config = NamespaceConfig::default();
        assert!(config.mount);
        assert!(config.pid);
        assert!(config.ipc);
        assert!(config.uts);
        assert!(!config.net);
    }

    #[test]
    fn test_namespace_config_full_isolation() {
        let config = NamespaceConfig::full_isolation();
        assert!(config.mount);
        assert!(config.pid);
        assert!(config.ipc);
        assert!(config.uts);
        assert!(config.net);
    }

    #[test]
    fn test_namespace_config_minimal() {
        let config = NamespaceConfig::minimal();
        assert!(config.mount);
        assert!(config.pid);
        assert!(!config.ipc);
        assert!(!config.uts);
        assert!(!config.net);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_namespace_config_to_clone_flags() {
        let config = NamespaceConfig {
            mount: true,
            pid: true,
            ipc: false,
            uts: false,
            net: false,
        };

        let flags = config.to_clone_flags();
        assert!(flags.contains(CloneFlags::CLONE_NEWNS));
        assert!(flags.contains(CloneFlags::CLONE_NEWPID));
        assert!(!flags.contains(CloneFlags::CLONE_NEWIPC));
        assert!(!flags.contains(CloneFlags::CLONE_NEWUTS));
        assert!(!flags.contains(CloneFlags::CLONE_NEWNET));
    }
}
