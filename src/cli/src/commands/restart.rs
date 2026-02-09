//! `a3s-box restart` command — Restart a running box.
//!
//! Equivalent to `a3s-box stop` followed by `a3s-box start`.

use a3s_box_core::config::{AgentType, BoxConfig, ResourceConfig};
use a3s_box_core::event::EventEmitter;
use a3s_box_runtime::VmManager;
use clap::Args;

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct RestartArgs {
    /// Box name or ID
    pub r#box: String,

    /// Seconds to wait for stop before force-killing
    #[arg(short = 't', long, default_value = "10")]
    pub timeout: u64,
}

pub async fn execute(args: RestartArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = StateFile::load_default()?;

    let record = resolve::resolve(&state, &args.r#box)?;

    let box_id = record.id.clone();
    let name = record.name.clone();
    let image = record.image.clone();
    let cpus = record.cpus;
    let memory_mb = record.memory_mb;
    let was_running = record.status == "running";
    let pid = record.pid;

    // Phase 1: Stop the box if it's running
    if was_running {
        if let Some(pid) = pid {
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }

            let start = std::time::Instant::now();
            let timeout_ms = args.timeout * 1000;
            loop {
                if !is_process_alive(pid) {
                    break;
                }
                if start.elapsed().as_millis() > timeout_ms as u128 {
                    unsafe {
                        libc::kill(pid as i32, libc::SIGKILL);
                    }
                    // Wait briefly for SIGKILL to take effect
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    break;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            }
        }

        // Update state to stopped
        let record = resolve::resolve_mut(&mut state, &box_id)?;
        record.status = "stopped".to_string();
        record.pid = None;
        state.save()?;
    } else {
        // Verify the box is in a startable state
        match record.status.as_str() {
            "created" | "stopped" | "dead" => {}
            other => {
                return Err(format!("Cannot restart box in state: {other}").into());
            }
        }
    }

    // Phase 2: Start the box
    let config = BoxConfig {
        agent: AgentType::OciRegistry {
            reference: image,
        },
        resources: ResourceConfig {
            vcpus: cpus,
            memory_mb,
            ..Default::default()
        },
        ..Default::default()
    };

    let emitter = EventEmitter::new(256);
    let mut vm = VmManager::with_box_id(config, emitter, box_id.clone());

    vm.boot().await?;

    // Update record to running
    let record = resolve::resolve_mut(&mut state, &box_id)?;
    record.status = "running".to_string();
    record.pid = vm.pid().await;
    record.started_at = Some(chrono::Utc::now());
    state.save()?;

    println!("{name}");
    Ok(())
}

fn is_process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}
