//! `a3s-box start` command — Start one or more created/stopped boxes.

use a3s_box_core::config::{AgentType, BoxConfig, ResourceConfig};
use a3s_box_core::event::EventEmitter;
use a3s_box_runtime::VmManager;
use clap::Args;

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct StartArgs {
    /// Box name(s) or ID(s)
    #[arg(required = true)]
    pub boxes: Vec<String>,
}

pub async fn execute(args: StartArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = StateFile::load_default()?;
    let mut errors: Vec<String> = Vec::new();

    for query in &args.boxes {
        if let Err(e) = start_one(&mut state, query).await {
            errors.push(format!("{query}: {e}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n").into())
    }
}

async fn start_one(
    state: &mut StateFile,
    query: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let record = resolve::resolve(state, query)?;

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
    let record = resolve::resolve_mut(state, &box_id)?;
    record.status = "running".to_string();
    record.started_at = Some(chrono::Utc::now());
    record.stopped_by_user = false;
    record.restart_count = 0;
    state.save()?;

    println!("{name}");
    Ok(())
}
