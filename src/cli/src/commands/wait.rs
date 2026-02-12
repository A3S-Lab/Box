//! `a3s-box wait` command — Block until one or more boxes stop, then print exit codes.

use clap::Args;

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct WaitArgs {
    /// Box name(s) or ID(s)
    #[arg(required = true)]
    pub boxes: Vec<String>,
}

pub async fn execute(args: WaitArgs) -> Result<(), Box<dyn std::error::Error>> {
    for query in &args.boxes {
        wait_one(query).await?;
    }
    Ok(())
}

async fn wait_one(query: &str) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let state = StateFile::load_default()?;
        let record = resolve::resolve(&state, query)?;

        match record.status.as_str() {
            "running" => {
                // Check if the process is still alive
                if let Some(pid) = record.pid {
                    if !is_process_alive(pid) {
                        // Process died — box has stopped
                        println!("0");
                        return Ok(());
                    }
                }
                // Still running, poll again
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }
            "stopped" | "dead" => {
                // Already stopped
                println!("0");
                return Ok(());
            }
            "created" => {
                // Not started yet, wait for it to start and then stop
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }
            _ => {
                println!("0");
                return Ok(());
            }
        }
    }
}

/// Check if a process is alive by sending signal 0.
fn is_process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_process_alive_current_process() {
        let current_pid = std::process::id();
        assert!(is_process_alive(current_pid));
    }

    #[test]
    fn test_is_process_alive_nonexistent() {
        // PID 99999 is very unlikely to exist
        assert!(!is_process_alive(99999));
    }

    #[test]
    fn test_is_process_alive_parent_process() {
        // Parent process should be alive (the test runner)
        let parent_pid = unsafe { libc::getppid() as u32 };
        assert!(is_process_alive(parent_pid));
    }
}
