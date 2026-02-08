//! `a3s-box exec` command — Execute a command in a running box.
//!
//! This command requires the a3s-code agent to be running inside the box.
//! A proper Exec RPC will be added when the agent protocol is finalized.

use clap::Args;

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct ExecArgs {
    /// Box name or ID
    pub r#box: String,

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

    let socket_path = &record.socket_path;
    if !socket_path.exists() {
        return Err(format!(
            "gRPC socket not found for box {} at {}",
            record.name,
            socket_path.display()
        ).into());
    }

    // Verify the agent is reachable
    let client = a3s_box_runtime::AgentClient::connect(socket_path).await?;
    let healthy = client.health_check().await?;
    if !healthy {
        return Err(format!("Agent in box {} is not healthy", record.name).into());
    }

    Err("exec command requires the a3s-code agent protocol (not yet integrated into Box CLI)".into())
}
