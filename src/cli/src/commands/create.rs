//! `a3s-box create` command — Create without starting.

use std::collections::HashMap;
use std::path::PathBuf;

use a3s_box_core::config::ResourceLimits;
use clap::Args;

use crate::output::parse_memory;
use crate::state::{generate_name, BoxRecord, StateFile};

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

    /// Read environment variables from a file, can be repeated
    #[arg(long)]
    pub env_file: Vec<String>,

    /// Add a custom host-to-IP mapping (host:ip), can be repeated
    #[arg(long)]
    pub add_host: Vec<String>,

    /// Set target platform (e.g., "linux/amd64", "linux/arm64")
    #[arg(long)]
    pub platform: Option<String>,

    /// Run an init process (tini) as PID 1
    #[arg(long)]
    pub init: bool,

    /// Mount the root filesystem as read-only
    #[arg(long)]
    pub read_only: bool,

    /// Add a Linux capability, can be repeated
    #[arg(long)]
    pub cap_add: Vec<String>,

    /// Drop a Linux capability, can be repeated
    #[arg(long)]
    pub cap_drop: Vec<String>,

    /// Security options (e.g., "seccomp=unconfined"), can be repeated
    #[arg(long)]
    pub security_opt: Vec<String>,

    /// Give extended privileges to the box
    #[arg(long)]
    pub privileged: bool,

    /// Add a host device to the box (host_path[:guest_path[:perms]]), can be repeated
    #[arg(long)]
    pub device: Vec<String>,

    /// GPU devices to add (e.g., "all", "0,1")
    #[arg(long)]
    pub gpus: Option<String>,

    /// Size of /dev/shm (e.g., "64m", "1g")
    #[arg(long)]
    pub shm_size: Option<String>,

    /// Override the default signal to stop the box
    #[arg(long)]
    pub stop_signal: Option<String>,

    /// Timeout (in seconds) to stop the box before killing
    #[arg(long)]
    pub stop_timeout: Option<u64>,

    /// Disable any healthcheck defined in the image
    #[arg(long)]
    pub no_healthcheck: bool,

    /// Disable OOM Killer for the box
    #[arg(long)]
    pub oom_kill_disable: bool,

    /// Tune the host OOM score adjustment (-1000 to 1000)
    #[arg(long)]
    pub oom_score_adj: Option<i32>,
}

pub async fn execute(args: CreateArgs) -> Result<(), Box<dyn std::error::Error>> {
    // Validate restart policy
    let (restart_policy, max_restart_count) = crate::state::parse_restart_policy(&args.restart)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let memory_mb = parse_memory(&args.memory).map_err(|e| format!("Invalid --memory: {e}"))?;

    // Build resource limits before any partial moves of args
    let resource_limits = build_resource_limits(&args)?;

    let name = args.name.unwrap_or_else(generate_name);
    let mut env = parse_env_vars(&args.env)?;

    // Load --env-file entries (merged into env, CLI --env takes precedence)
    for env_file in &args.env_file {
        let file_env = parse_env_file(env_file)?;
        for (k, v) in file_env {
            env.entry(k).or_insert(v);
        }
    }

    let labels =
        parse_env_vars(&args.labels).map_err(|e| e.replace("environment variable", "label"))?;

    // Parse health check config (--no-healthcheck disables)
    let health_check = if args.no_healthcheck {
        None
    } else {
        args.health_cmd
            .as_ref()
            .map(|cmd| crate::state::HealthCheck {
                cmd: vec!["sh".to_string(), "-c".to_string(), cmd.clone()],
                interval_secs: args.health_interval,
                timeout_secs: args.health_timeout,
                retries: args.health_retries,
                start_period_secs: args.health_start_period,
            })
    };

    // Parse --shm-size
    let shm_size = match &args.shm_size {
        Some(s) => Some(parse_memory_bytes(s).map_err(|e| format!("Invalid --shm-size: {e}"))?),
        None => None,
    };

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

    let entrypoint = args
        .entrypoint
        .as_ref()
        .map(|ep| ep.split_whitespace().map(String::from).collect::<Vec<_>>());

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
        restart_policy,
        port_map: args.publish,
        labels,
        stopped_by_user: false,
        restart_count: 0,
        max_restart_count,
        exit_code: None,
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
        log_config: a3s_box_core::log::LogConfig::default(),
        add_host: args.add_host,
        platform: args.platform,
        init: args.init,
        read_only: args.read_only,
        cap_add: args.cap_add,
        cap_drop: args.cap_drop,
        security_opt: args.security_opt,
        privileged: args.privileged,
        devices: args.device,
        gpus: args.gpus,
        shm_size,
        stop_signal: args.stop_signal,
        stop_timeout: args.stop_timeout,
        oom_kill_disable: args.oom_kill_disable,
        oom_score_adj: args.oom_score_adj,
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

/// Load environment variables from a file.
///
/// Each line should be KEY=VALUE. Empty lines and lines starting with '#' are skipped.
fn parse_env_file(path: &str) -> Result<HashMap<String, String>, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read env file '{}': {}", path, e))?;
    let mut map = HashMap::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once('=') {
            map.insert(key.trim().to_string(), value.trim().to_string());
        } else {
            map.insert(trimmed.to_string(), String::new());
        }
    }
    Ok(map)
}

/// Build ResourceLimits from CLI args.
fn build_resource_limits(args: &CreateArgs) -> Result<ResourceLimits, Box<dyn std::error::Error>> {
    let memory_reservation = match &args.memory_reservation {
        Some(s) => {
            Some(parse_memory_bytes(s).map_err(|e| format!("Invalid --memory-reservation: {e}"))?)
        }
        None => None,
    };
    let memory_swap = match &args.memory_swap {
        Some(s) if s == "-1" => Some(-1i64),
        Some(s) => {
            Some(parse_memory_bytes(s).map_err(|e| format!("Invalid --memory-swap: {e}"))? as i64)
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_env_vars() {
        let vars = vec!["KEY1=value1".to_string(), "KEY2=value2".to_string()];
        let result = parse_env_vars(&vars).unwrap();
        assert_eq!(result.get("KEY1"), Some(&"value1".to_string()));
        assert_eq!(result.get("KEY2"), Some(&"value2".to_string()));
    }

    #[test]
    fn test_parse_env_vars_empty() {
        let vars: Vec<String> = vec![];
        let result = parse_env_vars(&vars).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_env_vars_invalid() {
        let vars = vec!["INVALID".to_string()];
        let result = parse_env_vars(&vars);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid environment variable"));
    }

    #[test]
    fn test_parse_env_vars_with_equals_in_value() {
        let vars = vec!["KEY=value=with=equals".to_string()];
        let result = parse_env_vars(&vars).unwrap();
        assert_eq!(result.get("KEY"), Some(&"value=with=equals".to_string()));
    }

    #[test]
    fn test_parse_memory_bytes_raw_number() {
        assert_eq!(parse_memory_bytes("1073741824").unwrap(), 1073741824);
        assert_eq!(parse_memory_bytes("512").unwrap(), 512);
    }

    #[test]
    fn test_parse_memory_bytes_kilobytes() {
        assert_eq!(parse_memory_bytes("1k").unwrap(), 1024);
        assert_eq!(parse_memory_bytes("512kb").unwrap(), 512 * 1024);
        assert_eq!(parse_memory_bytes("2K").unwrap(), 2048);
    }

    #[test]
    fn test_parse_memory_bytes_megabytes() {
        assert_eq!(parse_memory_bytes("1m").unwrap(), 1024 * 1024);
        assert_eq!(parse_memory_bytes("512mb").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_memory_bytes("2M").unwrap(), 2 * 1024 * 1024);
    }

    #[test]
    fn test_parse_memory_bytes_gigabytes() {
        assert_eq!(parse_memory_bytes("1g").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_memory_bytes("2gb").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_memory_bytes("4G").unwrap(), 4 * 1024 * 1024 * 1024);
    }

    #[test]
    fn test_parse_memory_bytes_bytes() {
        assert_eq!(parse_memory_bytes("1024b").unwrap(), 1024);
        assert_eq!(parse_memory_bytes("512B").unwrap(), 512);
    }

    #[test]
    fn test_parse_memory_bytes_empty() {
        assert!(parse_memory_bytes("").is_err());
        assert!(parse_memory_bytes("   ").is_err());
    }

    #[test]
    fn test_parse_memory_bytes_invalid_format() {
        assert!(parse_memory_bytes("abc").is_err());
        assert!(parse_memory_bytes("123x").is_err());
        assert!(parse_memory_bytes("m512").is_err());
    }

    #[test]
    fn test_parse_memory_bytes_invalid_number() {
        assert!(parse_memory_bytes("abcm").is_err());
        assert!(parse_memory_bytes("12.5g").is_err());
    }

    #[test]
    fn test_parse_memory_bytes_whitespace() {
        assert_eq!(parse_memory_bytes("  512m  ").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_memory_bytes("\t1g\n").unwrap(), 1024 * 1024 * 1024);
    }

    // --- parse_env_file tests ---

    #[test]
    fn test_parse_env_file_basic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("env");
        std::fs::write(&path, "FOO=bar\nBAZ=qux\n").unwrap();
        let map = parse_env_file(path.to_str().unwrap()).unwrap();
        assert_eq!(map.get("FOO").unwrap(), "bar");
        assert_eq!(map.get("BAZ").unwrap(), "qux");
    }

    #[test]
    fn test_parse_env_file_comments_and_blanks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("env");
        std::fs::write(&path, "# comment\n\nKEY=val\n  \n# another\n").unwrap();
        let map = parse_env_file(path.to_str().unwrap()).unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("KEY").unwrap(), "val");
    }

    #[test]
    fn test_parse_env_file_key_without_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("env");
        std::fs::write(&path, "STANDALONE\n").unwrap();
        let map = parse_env_file(path.to_str().unwrap()).unwrap();
        assert_eq!(map.get("STANDALONE").unwrap(), "");
    }

    #[test]
    fn test_parse_env_file_value_with_equals() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("env");
        std::fs::write(&path, "CONN=postgres://host?opt=1\n").unwrap();
        let map = parse_env_file(path.to_str().unwrap()).unwrap();
        assert_eq!(map.get("CONN").unwrap(), "postgres://host?opt=1");
    }

    #[test]
    fn test_parse_env_file_missing_file() {
        let result = parse_env_file("/nonexistent/path/env");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_env_file_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("env");
        std::fs::write(&path, "").unwrap();
        let map = parse_env_file(path.to_str().unwrap()).unwrap();
        assert!(map.is_empty());
    }
}
