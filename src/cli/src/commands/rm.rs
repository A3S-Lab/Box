//! `a3s-box rm` command — Remove one or more boxes.

use clap::Args;

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct RmArgs {
    /// Box name(s) or ID(s)
    #[arg(required = true)]
    pub boxes: Vec<String>,

    /// Force removal of running boxes (stops them first)
    #[arg(short, long)]
    pub force: bool,
}

pub async fn execute(args: RmArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = StateFile::load_default()?;
    let mut errors: Vec<String> = Vec::new();

    for query in &args.boxes {
        if let Err(e) = rm_one(&mut state, query, args.force) {
            errors.push(format!("{query}: {e}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n").into())
    }
}

fn rm_one(
    state: &mut StateFile,
    query: &str,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let record = resolve::resolve(state, query)?;

    if record.status == "running" {
        if !force {
            return Err(format!(
                "Box {} is running. Use --force to remove a running box.",
                record.name
            )
            .into());
        }

        // Force-kill the running box
        if let Some(pid) = record.pid {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
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
