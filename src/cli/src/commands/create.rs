//! `a3s-box create` command — Create without starting.

use std::collections::HashMap;
use std::path::PathBuf;

use a3s_box_core::config::ResourceLimits;
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

    /// Set metadata labels (KEY=VALUE), can be repeated
    #[arg(short = 'l', long = "label")]
    pub labels: Vec<String>,

    /// Mount a tmpfs (e.g., "/tmp" or "/tmp:size=100m"), can be repeated
    #[arg(long)]
    pub tmpfs: Vec<String>,

    /// Connect to a network (e.g., "mynet")
    #[arg(long)]
    pub network: Option<String>,

    /// Health check command (e.g., "curl -f http://localhost/health")
    #[arg(long)]
    pub health_cmd: Option<String>,

    /// Health check interval in seconds (default: 30)
    #[arg(long, default_value = "30")]
    pub health_interval: u64,

    /// Health check timeout in seconds (default: 5)
    #[arg(long, default_value = "5")]
    pub health_timeout: u64,

    /// Health check retries before unhealthy (default: 3)
    #[arg(long, default_value = "3")]
    pub health_retries: u32,

    /// Health check start period in seconds (default: 0)
    #[arg(long, default_value = "0")]
    pub health_start_period: u64,

    /// Limit PIDs inside the box (--pids-limit)
    #[arg(long)]
    pub pids_limit: Option<u64>,

    /// Pin to specific CPUs (e.g., "0,1,3" or "0-3")
    #[arg(long)]
    pub cpuset_cpus: Option<String>,

    /// Set ulimit (e.g., "nofile=1024:4096"), can be repeated
    #[arg(long = "ulimit")]
    pub ulimits: Vec<String>,

    /// CPU shares (relative weight, 2-262144)
    #[arg(long)]
    pub cpu_shares: Option<u64>,

    /// CPU quota in microseconds per cpu-period
    #[arg(long)]
    pub cpu_quota: Option<i64>,

    /// CPU period in microseconds (default: 100000)
    #[arg(long)]
    pub cpu_period: Option<u64>,

    /// Memory reservation/soft limit (e.g., "256m", "1g")
    #[arg(long)]
    pub memory_reservation: Option<String>,

    /// Memory+swap limit (e.g., "1g", "-1" for unlimited)
    #[arg(long)]
    pub memory_swap: Option<String>,
}

pub async fn execute(args: CreateArgs) -> Result<(), Box<dyn std::error::Error>> {
    let memory_mb = parse_memory(&args.memory)
        .map_err(|e| format!("Invalid --memory: {e}"))?;

    // Build resource limits before any partial moves of args
    let resource_limits = build_resource_limits(&args)?;

    let name = args.name.unwrap_or_else(generate_name);
    let env = parse_env_vars(&args.env)?;
    let labels = parse_env_vars(&args.labels)
        .map_err(|e| e.replace("environment variable", "label"))?;

    // Parse health check config
    let health_check = args.health_cmd.as_ref().map(|cmd| {
        crate::state::HealthCheck {
            cmd: vec!["sh".to_string(), "-c".to_string(), cmd.clone()],
            interval_secs: args.health_interval,
            timeout_secs: args.health_timeout,
            retries: args.health_retries,
            start_period_secs: args.health_start_period,
        }
    });

    let box_id = uuid::Uuid::new_v4().to_string();
    let short_id = BoxRecord::make_short_id(&box_id);

    let home = dirs::home_dir()
        .map(|h| h.join(".a3s"))
        .unwrap_or_else(|| PathBuf::from(".a3s"));
    let box_dir = home.join("boxes").join(&box_id);

    // Create box directory structure
    std::fs::create_dir_all(box_dir.join("sockets"))?;
    std::fs::create_dir_all(box_dir.join("logs"))?;

    // Resolve named volumes
    let mut resolved_volumes = Vec::new();
    let mut volume_names = Vec::new();
    for vol_spec in &args.volumes {
        let (resolved, vol_name) = super::volume::resolve_named_volume(vol_spec)?;
        if let Some(name) = vol_name {
            volume_names.push(name);
        }
        resolved_volumes.push(resolved);
    }

    let entrypoint = args.entrypoint.as_ref().map(|ep| {
        ep.split_whitespace().map(String::from).collect::<Vec<_>>()
    });

    // Determine network mode
    let network_mode = match &args.network {
        Some(name) => a3s_box_core::NetworkMode::Bridge {
            network: name.clone(),
        },
        None => a3s_box_core::NetworkMode::Tsi,
    };

    let record = BoxRecord {
        id: box_id.clone(),
        short_id: short_id.clone(),
        name: name.clone(),
        image: args.image.clone(),
        status: "created".to_string(),
        pid: None,
        cpus: args.cpus,
        memory_mb,
        volumes: resolved_volumes,
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
        labels,
        stopped_by_user: false,
        restart_count: 0,
        health_check,
        health_status: "none".to_string(),
        health_retries: 0,
        health_last_check: None,
        network_mode,
        network_name: args.network,
        volume_names: volume_names.clone(),
        tmpfs: args.tmpfs,
        anonymous_volumes: vec![],
        resource_limits,
    };

    let mut state = StateFile::load_default()?;
    state.add(record)?;

    // Attach named volumes to this box
    super::volume::attach_volumes(&volume_names, &box_id)?;

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

/// Build ResourceLimits from CLI args.
fn build_resource_limits(args: &CreateArgs) -> Result<ResourceLimits, Box<dyn std::error::Error>> {
    let memory_reservation = match &args.memory_reservation {
        Some(s) => Some(parse_memory_bytes(s).map_err(|e| format!("Invalid --memory-reservation: {e}"))?),
        None => None,
    };
    let memory_swap = match &args.memory_swap {
        Some(s) if s == "-1" => Some(-1i64),
        Some(s) => Some(parse_memory_bytes(s).map_err(|e| format!("Invalid --memory-swap: {e}"))? as i64),
        None => None,
    };

    Ok(ResourceLimits {
        pids_limit: args.pids_limit,
        cpuset_cpus: args.cpuset_cpus.clone(),
        ulimits: args.ulimits.clone(),
        cpu_shares: args.cpu_shares,
        cpu_quota: args.cpu_quota,
        cpu_period: args.cpu_period,
        memory_reservation,
        memory_swap,
    })
}

/// Parse a memory size string (e.g., "256m", "1g", "1073741824") into bytes.
fn parse_memory_bytes(s: &str) -> Result<u64, String> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return Err("empty value".to_string());
    }

    if let Ok(bytes) = s.parse::<u64>() {
        return Ok(bytes);
    }

    let (num_str, multiplier) = if s.ends_with("gb") || s.ends_with("g") {
        let num = s.trim_end_matches("gb").trim_end_matches('g');
        (num, 1024u64 * 1024 * 1024)
    } else if s.ends_with("mb") || s.ends_with("m") {
        let num = s.trim_end_matches("mb").trim_end_matches('m');
        (num, 1024u64 * 1024)
    } else if s.ends_with("kb") || s.ends_with("k") {
        let num = s.trim_end_matches("kb").trim_end_matches('k');
        (num, 1024u64)
    } else if s.ends_with('b') {
        let num = s.trim_end_matches('b');
        (num, 1u64)
    } else {
        return Err(format!("unrecognized memory format: {s}"));
    };

    let num: u64 = num_str
        .parse()
        .map_err(|_| format!("invalid number: {num_str}"))?;
    Ok(num * multiplier)
}
