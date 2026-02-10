//! `a3s-box run` command — Pull + Create + Start.

use std::collections::HashMap;
use std::path::PathBuf;

use a3s_box_core::config::{AgentType, BoxConfig, ResourceConfig};
use a3s_box_core::event::EventEmitter;
use a3s_box_runtime::VmManager;
use clap::Args;

use crate::output::parse_memory;
use crate::state::{BoxRecord, StateFile, generate_name};

#[derive(Args)]
pub struct RunArgs {
    /// OCI image reference
    pub image: String,

    /// Assign a name to the box
    #[arg(long)]
    pub name: Option<String>,

    /// Number of CPUs
    #[arg(long, default_value = "2")]
    pub cpus: u32,

    /// Memory (e.g., "512m", "2g")
    #[arg(long, default_value = "512m")]
    pub memory: String,

    /// Volume mount (host:guest), can be repeated
    #[arg(short = 'v', long = "volume")]
    pub volumes: Vec<String>,

    /// Environment variable (KEY=VALUE), can be repeated
    #[arg(short = 'e', long = "env")]
    pub env: Vec<String>,

    /// Publish a port (host_port:guest_port), can be repeated
    #[arg(short = 'p', long = "publish")]
    pub publish: Vec<String>,

    /// Set custom DNS servers, can be repeated
    #[arg(long)]
    pub dns: Vec<String>,

    /// Run in detached mode (background)
    #[arg(short = 'd', long)]
    pub detach: bool,

    /// Override the image entrypoint
    #[arg(long)]
    pub entrypoint: Option<String>,

    /// Automatically remove the box when it stops
    #[arg(long)]
    pub rm: bool,

    /// Command to run (override entrypoint)
    #[arg(last = true)]
    pub cmd: Vec<String>,
}

pub async fn execute(args: RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    let memory_mb = parse_memory(&args.memory)
        .map_err(|e| format!("Invalid --memory: {e}"))?;

    let name = args.name.unwrap_or_else(generate_name);
    let env = parse_env_vars(&args.env)?;

    // Parse entrypoint override: split string into argv
    let entrypoint_override = args.entrypoint.as_ref().map(|ep| {
        ep.split_whitespace().map(String::from).collect::<Vec<_>>()
    });

    // Build BoxConfig
    let config = BoxConfig {
        agent: AgentType::OciRegistry {
            reference: args.image.clone(),
        },
        resources: ResourceConfig {
            vcpus: args.cpus,
            memory_mb,
            ..Default::default()
        },
        cmd: args.cmd.clone(),
        entrypoint_override: entrypoint_override.clone(),
        volumes: args.volumes.clone(),
        extra_env: env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        port_map: args.publish.clone(),
        dns: args.dns.clone(),
        ..Default::default()
    };

    // Create VmManager and boot
    let emitter = EventEmitter::new(256);
    let mut vm = VmManager::new(config, emitter);
    let box_id = vm.box_id().to_string();

    println!("Creating box {} ({})...", name, &BoxRecord::make_short_id(&box_id));

    vm.boot().await?;

    // Get PID from the running VM
    let pid = vm.pid().await;

    // Determine PID from handler metrics (handler holds PID internally)
    // We use the box directory structure to find PID
    let home = dirs::home_dir()
        .map(|h| h.join(".a3s"))
        .unwrap_or_else(|| PathBuf::from(".a3s"));
    let box_dir = home.join("boxes").join(&box_id);

    // Save box record
    let record = BoxRecord {
        id: box_id.clone(),
        short_id: BoxRecord::make_short_id(&box_id),
        name: name.clone(),
        image: args.image.clone(),
        status: "running".to_string(),
        pid,
        cpus: args.cpus,
        memory_mb,
        volumes: args.volumes.clone(),
        env,
        cmd: args.cmd.clone(),
        entrypoint: entrypoint_override.clone(),
        box_dir: box_dir.clone(),
        socket_path: box_dir.join("sockets").join("grpc.sock"),
        exec_socket_path: box_dir.join("sockets").join("exec.sock"),
        console_log: box_dir.join("logs").join("console.log"),
        created_at: chrono::Utc::now(),
        started_at: Some(chrono::Utc::now()),
        auto_remove: args.rm,
    };

    let mut state = StateFile::load_default()?;
    state.add(record)?;

    if args.detach {
        println!("{box_id}");
        return Ok(());
    }

    // Foreground mode: tail console log and wait for Ctrl-C
    println!("Box {} ({}) started. Press Ctrl-C to stop.", name, BoxRecord::make_short_id(&box_id));

    let console_log = box_dir.join("logs").join("console.log");
    let shutdown = tokio::signal::ctrl_c();

    // Tail console log in background
    let log_handle = tokio::spawn(async move {
        super::tail_file(&console_log).await;
    });

    // Wait for Ctrl-C
    let _ = shutdown.await;
    println!("\nStopping box {}...", name);

    log_handle.abort();

    // Destroy VM
    vm.destroy().await?;

    // Update state
    let mut state = StateFile::load_default()?;
    if let Some(rec) = state.find_by_id_mut(&box_id) {
        rec.status = "stopped".to_string();
        rec.pid = None;
    }

    if args.rm {
        state.remove(&box_id)?;
        let _ = std::fs::remove_dir_all(&box_dir);
        println!("Box {} removed.", name);
    } else {
        state.save()?;
        println!("Box {} stopped.", name);
    }

    Ok(())
}

/// Parse KEY=VALUE pairs into a HashMap.
fn parse_env_vars(vars: &[String]) -> Result<HashMap<String, String>, String> {
    let mut map = HashMap::new();
    for var in vars {
        let (key, value) = var
            .split_once('=')
            .ok_or_else(|| format!("Invalid environment variable (expected KEY=VALUE): {var}"))?;
        map.insert(key.to_string(), value.to_string());
    }
    Ok(map)
}
