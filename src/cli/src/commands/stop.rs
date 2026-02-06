//! `a3s-box stop` command — Graceful stop.

use clap::Args;

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct StopArgs {
    /// Box name or ID
    pub r#box: String,

    /// Seconds to wait before force-killing
    #[arg(short = 't', long, default_value = "10")]
    pub timeout: u64,
}

pub async fn execute(args: StopArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = StateFile::load_default()?;

    let record = resolve::resolve(&state, &args.r#box)?;

    if record.status != "running" {
        return Err(format!("Box {} is not running (status: {})", record.name, record.status).into());
    }

    let box_id = record.id.clone();
    let name = record.name.clone();
    let pid = record.pid;
    let auto_remove = record.auto_remove;
    let box_dir = record.box_dir.clone();

    // Send SIGTERM, then SIGKILL after timeout
    if let Some(pid) = pid {
        unsafe { libc::kill(pid as i32, libc::SIGTERM); }

        // Wait for process to exit with timeout
        let start = std::time::Instant::now();
        let timeout_ms = args.timeout * 1000;
        loop {
            if !is_process_alive(pid) {
                break;
            }
            if start.elapsed().as_millis() > timeout_ms as u128 {
                unsafe { libc::kill(pid as i32, libc::SIGKILL); }
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    }

    // Update state
    let record = resolve::resolve_mut(&mut state, &box_id)?;
    record.status = "stopped".to_string();
    record.pid = None;

    if auto_remove {
        let _ = std::fs::remove_dir_all(&box_dir);
        state.remove(&box_id)?;
        println!("{name} (auto-removed)");
    } else {
        state.save()?;
        println!("{name}");
    }

    Ok(())
}

fn is_process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}
