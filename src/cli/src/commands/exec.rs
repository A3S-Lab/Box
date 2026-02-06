//! `a3s-box exec` command — Execute a command in a running box.
//!
//! Interim implementation: uses the existing Generate gRPC RPC to send
//! commands. A proper Exec RPC will be added in Layer 2.

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

    // Connect to the agent via gRPC
    let client = a3s_box_runtime::AgentClient::connect(socket_path).await?;

    // Use Generate RPC as interim exec mechanism
    let cmd_str = args.cmd.join(" ");
    let request = a3s_box_runtime::grpc::GenerateRequest {
        session_id: String::new(),
        prompt: cmd_str,
    };
    let result = client.generate(request).await?;

    println!("{}", result.text);
    Ok(())
}
