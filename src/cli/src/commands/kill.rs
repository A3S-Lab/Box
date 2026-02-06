//! `a3s-box kill` command — Force-kill a running box.

use clap::Args;

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct KillArgs {
    /// Box name or ID
    pub r#box: String,
}

pub async fn execute(args: KillArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = StateFile::load_default()?;

    let record = resolve::resolve(&state, &args.r#box)?;

    if record.status != "running" {
        return Err(format!("Box {} is not running", record.name).into());
    }

    let box_id = record.id.clone();
    let name = record.name.clone();

    if let Some(pid) = record.pid {
        unsafe { libc::kill(pid as i32, libc::SIGKILL); }
    }

    let record = resolve::resolve_mut(&mut state, &box_id)?;
    record.status = "stopped".to_string();
    record.pid = None;
    state.save()?;

    println!("{name}");
    Ok(())
}
