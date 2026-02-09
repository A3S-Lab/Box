//! `a3s-box exec` command — Execute a command in a running box.
//!
//! Connects to the exec server inside the guest VM via the exec Unix socket
//! and runs the specified command, printing stdout/stderr and exiting with
//! the command's exit code.

use a3s_box_core::exec::{ExecRequest, DEFAULT_EXEC_TIMEOUT_NS};
use a3s_box_runtime::ExecClient;
use clap::Args;

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct ExecArgs {
    /// Box name or ID
    pub r#box: String,

    /// Timeout in seconds (default: 5)
    #[arg(long, default_value = "5")]
    pub timeout: u64,

    /// Set environment variables (KEY=VALUE), can be repeated
    #[arg(short, long = "env")]
    pub envs: Vec<String>,

    /// Working directory inside the box
    #[arg(short, long)]
    pub workdir: Option<String>,

    /// Command and arguments to execute
    #[arg(last = true, required = true)]
    pub cmd: Vec<String>,
}

pub async fn execute(args: ExecArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, &args.r#box)?;

    if record.status != "running" {
        return Err(format!("Box {} is not running", record.name).into());
    }

    // Derive exec socket path from record or fallback
    let exec_socket_path = if !record.exec_socket_path.as_os_str().is_empty() {
        record.exec_socket_path.clone()
    } else {
        // Fallback for older records without exec_socket_path
        record.box_dir.join("sockets").join("exec.sock")
    };

    if !exec_socket_path.exists() {
        return Err(format!(
            "Exec socket not found for box {} at {}",
            record.name,
            exec_socket_path.display()
        )
        .into());
    }

    // Connect to exec server
    let client = ExecClient::connect(&exec_socket_path).await?;

    // Build request
    let timeout_ns = if args.timeout == 0 {
        DEFAULT_EXEC_TIMEOUT_NS
    } else {
        args.timeout * 1_000_000_000
    };

    let request = ExecRequest {
        cmd: args.cmd,
        timeout_ns,
        env: args.envs,
        working_dir: args.workdir,
    };

    // Execute command
    let output = client.exec_command(&request).await?;

    // Print stdout
    if !output.stdout.is_empty() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        print!("{}", stdout);
    }

    // Print stderr to stderr
    if !output.stderr.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprint!("{}", stderr);
    }

    // Exit with the command's exit code
    if output.exit_code != 0 {
        std::process::exit(output.exit_code);
    }

    Ok(())
}
