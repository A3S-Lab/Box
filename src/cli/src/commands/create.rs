//! `a3s-box create` command — Create without starting.

use std::collections::HashMap;
use std::path::PathBuf;

use clap::Args;

use crate::output::parse_memory;
use crate::state::{BoxRecord, StateFile, generate_name};

#[derive(Args)]
pub struct CreateArgs {
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

    /// Override the image entrypoint
    #[arg(long)]
    pub entrypoint: Option<String>,

    /// Set the box hostname
    #[arg(long)]
    pub hostname: Option<String>,

    /// Run as a specific user (e.g., "root", "1000:1000")
    #[arg(short = 'u', long)]
    pub user: Option<String>,

    /// Working directory inside the box
    #[arg(short = 'w', long)]
    pub workdir: Option<String>,

    /// Restart policy: no, always, on-failure, unless-stopped
    #[arg(long, default_value = "no")]
    pub restart: String,
}

pub async fn execute(args: CreateArgs) -> Result<(), Box<dyn std::error::Error>> {
    let memory_mb = parse_memory(&args.memory)
        .map_err(|e| format!("Invalid --memory: {e}"))?;

    let name = args.name.unwrap_or_else(generate_name);
    let env = parse_env_vars(&args.env)?;

    let box_id = uuid::Uuid::new_v4().to_string();
    let short_id = BoxRecord::make_short_id(&box_id);

    let home = dirs::home_dir()
        .map(|h| h.join(".a3s"))
        .unwrap_or_else(|| PathBuf::from(".a3s"));
    let box_dir = home.join("boxes").join(&box_id);

    // Create box directory structure
    std::fs::create_dir_all(box_dir.join("sockets"))?;
    std::fs::create_dir_all(box_dir.join("logs"))?;

    let entrypoint = args.entrypoint.as_ref().map(|ep| {
        ep.split_whitespace().map(String::from).collect::<Vec<_>>()
    });

    let record = BoxRecord {
        id: box_id.clone(),
        short_id: short_id.clone(),
        name: name.clone(),
        image: args.image.clone(),
        status: "created".to_string(),
        pid: None,
        cpus: args.cpus,
        memory_mb,
        volumes: args.volumes,
        env,
        cmd: vec![],
        entrypoint,
        box_dir: box_dir.clone(),
        socket_path: box_dir.join("sockets").join("grpc.sock"),
        exec_socket_path: box_dir.join("sockets").join("exec.sock"),
        console_log: box_dir.join("logs").join("console.log"),
        created_at: chrono::Utc::now(),
        started_at: None,
        auto_remove: false,
        hostname: args.hostname,
        user: args.user,
        workdir: args.workdir,
        restart_policy: args.restart,
        port_map: args.publish,
    };

    let mut state = StateFile::load_default()?;
    state.add(record)?;

    println!("{box_id}");
    Ok(())
}

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
