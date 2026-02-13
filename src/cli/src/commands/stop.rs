//! `a3s-box stop` command — Graceful stop of one or more boxes.

use clap::Args;

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct StopArgs {
    /// Box name(s) or ID(s)
    #[arg(required = true)]
    pub boxes: Vec<String>,

    /// Seconds to wait before force-killing
    #[arg(short = 't', long, default_value = "10")]
    pub timeout: u64,
}

pub async fn execute(args: StopArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = StateFile::load_default()?;
    let mut errors: Vec<String> = Vec::new();

    for query in &args.boxes {
        if let Err(e) = stop_one(&mut state, query, args.timeout).await {
            errors.push(format!("{query}: {e}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n").into())
    }
}

async fn stop_one(
    state: &mut StateFile,
    query: &str,
    timeout: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let record = resolve::resolve(state, query)?;

    if record.status != "running" {
        return Err(format!(
            "Box {} is not running (status: {})",
            record.name, record.status
        )
        .into());
    }

    let box_id = record.id.clone();
    let name = record.name.clone();
    let pid = record.pid;
    let auto_remove = record.auto_remove;
    let box_dir = record.box_dir.clone();
    let network_name = record.network_name.clone();
    let volume_names = record.volume_names.clone();

    // Send SIGTERM, then SIGKILL after timeout
    if let Some(pid) = pid {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }

        let start = std::time::Instant::now();
        let timeout_ms = timeout * 1000;
        loop {
            if !is_process_alive(pid) {
                break;
            }
            if start.elapsed().as_millis() > timeout_ms as u128 {
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    }

    // Detach named volumes
    super::volume::detach_volumes(&volume_names, &box_id);

    // Disconnect from network if connected
    if let Some(ref net_name) = network_name {
        if let Ok(net_store) = a3s_box_runtime::NetworkStore::default_path() {
            if let Ok(Some(mut net_config)) = net_store.get(net_name) {
                net_config.disconnect(&box_id).ok();
                net_store.update(&net_config).ok();
            }
        }
    }

    // Update state
    let record = resolve::resolve_mut(state, &box_id)?;
    record.status = "stopped".to_string();
    record.pid = None;
    record.stopped_by_user = true;

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
