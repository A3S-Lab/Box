//! `a3s-box attach` command — attach to a running box's console output.

use clap::Args;

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct AttachArgs {
    /// Box name or ID
    pub r#box: String,
}

pub async fn execute(args: AttachArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, &args.r#box)?;

    if record.status != "running" {
        return Err(format!("Box {} is not running", record.name).into());
    }

    let console_log = record.console_log.clone();
    if !console_log.exists() {
        return Err(format!(
            "Console log not found for box {} at {}",
            record.name,
            console_log.display()
        )
        .into());
    }

    println!("Attached to box {}. Press Ctrl-C to detach.", record.name);

    // Tail console log in background
    let log_handle = tokio::spawn(async move {
        super::tail_file(&console_log).await;
    });

    // Wait for Ctrl-C
    let _ = tokio::signal::ctrl_c().await;
    println!("\nDetached from box {}.", record.name);

    log_handle.abort();

    Ok(())
}
