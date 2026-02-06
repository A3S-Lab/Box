//! `a3s-box start` command — Start a created/stopped box.

use a3s_box_core::config::{AgentType, BoxConfig, ResourceConfig};
use a3s_box_core::event::EventEmitter;
use a3s_box_runtime::VmManager;
use clap::Args;

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct StartArgs {
    /// Box name or ID
    pub r#box: String,
}

pub async fn execute(args: StartArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = StateFile::load_default()?;

    let record = resolve::resolve(&state, &args.r#box)?;

    match record.status.as_str() {
        "created" | "stopped" | "dead" => {}
        "running" => return Err(format!("Box {} is already running", record.name).into()),
        other => return Err(format!("Cannot start box in state: {other}").into()),
    }

    let box_id = record.id.clone();
    let name = record.name.clone();

    // Reconstruct BoxConfig from record
    let config = BoxConfig {
        agent: AgentType::OciRegistry {
            reference: record.image.clone(),
        },
        resources: ResourceConfig {
            vcpus: record.cpus,
            memory_mb: record.memory_mb,
            ..Default::default()
        },
        ..Default::default()
    };

    let emitter = EventEmitter::new(256);
    let mut vm = VmManager::with_box_id(config, emitter, box_id.clone());

    println!("Starting box {name}...");
    vm.boot().await?;

    // Update record
    let record = resolve::resolve_mut(&mut state, &box_id)?;
    record.status = "running".to_string();
    record.started_at = Some(chrono::Utc::now());
    state.save()?;

    println!("{name}");
    Ok(())
}
