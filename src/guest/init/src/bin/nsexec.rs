//! Command-line tool for executing commands in isolated namespaces.
//!
//! This binary can be invoked by the agent to run business code in isolated
//! namespaces, providing separation between agent and business code execution.

use a3s_box_guest_init::namespace::{spawn_isolated, NamespaceConfig};
use clap::Parser;
use std::process;

#[derive(Parser, Debug)]
#[command(name = "a3s-box-nsexec")]
#[command(about = "Execute commands in isolated Linux namespaces")]
struct Args {
    /// Command to execute
    #[arg(short, long)]
    command: String,

    /// Arguments to pass to the command
    #[arg(short, long)]
    args: Vec<String>,

    /// Working directory
    #[arg(short, long, default_value = "/workspace")]
    workdir: String,

    /// Environment variables (KEY=VALUE format)
    #[arg(short, long)]
    env: Vec<String>,

    /// Enable mount namespace isolation
    #[arg(long, default_value = "true")]
    mount: bool,

    /// Enable PID namespace isolation
    #[arg(long, default_value = "true")]
    pid: bool,

    /// Enable IPC namespace isolation
    #[arg(long, default_value = "true")]
    ipc: bool,

    /// Enable UTS namespace isolation
    #[arg(long, default_value = "true")]
    uts: bool,

    /// Enable network namespace isolation
    #[arg(long, default_value = "false")]
    net: bool,
}

fn main() {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    // Parse environment variables
    let env: Vec<(&str, &str)> = args
        .env
        .iter()
        .filter_map(|e| {
            let parts: Vec<&str> = e.splitn(2, '=').collect();
            if parts.len() == 2 {
                Some((parts[0], parts[1]))
            } else {
                eprintln!("Warning: Invalid environment variable format: {}", e);
                None
            }
        })
        .collect();

    // Build namespace configuration
    let config = NamespaceConfig {
        mount: args.mount,
        pid: args.pid,
        ipc: args.ipc,
        uts: args.uts,
        net: args.net,
    };

    // Convert args to &str
    let args_refs: Vec<&str> = args.args.iter().map(|s| s.as_str()).collect();

    // Spawn command in isolated namespace
    match spawn_isolated(&config, &args.command, &args_refs, &env, &args.workdir) {
        Ok(pid) => {
            tracing::info!("Command spawned with PID {}", pid);

            // Wait for the child process
            match wait_for_child(pid) {
                Ok(exit_code) => {
                    process::exit(exit_code);
                }
                Err(e) => {
                    eprintln!("Error waiting for child: {}", e);
                    process::exit(1);
                }
            }
        }
        Err(e) => {
            eprintln!("Failed to spawn command: {}", e);
            process::exit(1);
        }
    }
}

/// Wait for a child process and return its exit code.
fn wait_for_child(pid: u32) -> Result<i32, Box<dyn std::error::Error>> {
    use nix::sys::wait::{waitpid, WaitStatus};
    use nix::unistd::Pid;

    let child_pid = Pid::from_raw(pid as i32);

    loop {
        match waitpid(child_pid, None) {
            Ok(WaitStatus::Exited(_, status)) => {
                return Ok(status);
            }
            Ok(WaitStatus::Signaled(_, signal, _)) => {
                // Child was killed by signal, return 128 + signal number
                return Ok(128 + signal as i32);
            }
            Ok(WaitStatus::Stopped(_, _)) => {
                // Child was stopped, continue waiting
                continue;
            }
            Ok(WaitStatus::Continued(_)) => {
                // Child was continued, continue waiting
                continue;
            }
            Ok(WaitStatus::StillAlive) => {
                // Should not happen with blocking wait
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
            #[cfg(any(target_os = "linux", target_os = "android"))]
            Ok(WaitStatus::PtraceEvent(_, _, _)) | Ok(WaitStatus::PtraceSyscall(_)) => {
                // Ptrace events, continue waiting
                continue;
            }
            Err(e) => {
                return Err(format!("waitpid failed: {}", e).into());
            }
        }
    }
}
