//! `a3s-box rm` command — Remove a box.

use clap::Args;

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct RmArgs {
    /// Box name or ID
    pub r#box: String,

    /// Force removal of a running box (stops it first)
    #[arg(short, long)]
    pub force: bool,
}

pub async fn execute(args: RmArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = StateFile::load_default()?;

    let record = resolve::resolve(&state, &args.r#box)?;

    if record.status == "running" {
        if !args.force {
            return Err(format!(
                "Box {} is running. Use --force to remove a running box.",
                record.name
            ).into());
        }

        // Force-kill the running box
        if let Some(pid) = record.pid {
            unsafe { libc::kill(pid as i32, libc::SIGKILL); }
        }
    }

    let box_id = record.id.clone();
    let name = record.name.clone();
    let box_dir = record.box_dir.clone();

    // Remove box directory
    if box_dir.exists() {
        let _ = std::fs::remove_dir_all(&box_dir);
    }

    // Remove from state
    state.remove(&box_id)?;
    println!("{name}");

    Ok(())
}
