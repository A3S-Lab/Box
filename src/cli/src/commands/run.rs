//! `a3s-box run` command — Pull + Create + Start.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::PathBuf;

use a3s_box_core::config::{AgentType, BoxConfig, ResourceConfig, ResourceLimits};
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

    /// Keep STDIN open (interactive mode)
    #[arg(short = 'i', long = "interactive")]
    pub interactive: bool,

    /// Allocate a pseudo-TTY
    #[arg(short = 't', long = "tty")]
    pub tty: bool,

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

    /// Automatically remove the box when it stops
    #[arg(long)]
    pub rm: bool,

    /// Mount a tmpfs (e.g., "/tmp" or "/tmp:size=100m"), can be repeated
    #[arg(long)]
    pub tmpfs: Vec<String>,

    /// Connect to a network (e.g., "mynet")
    #[arg(long)]
    pub network: Option<String>,

    /// Set metadata labels (KEY=VALUE), can be repeated
    #[arg(short = 'l', long = "label")]
    pub labels: Vec<String>,

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

    /// Command to run (override entrypoint)
    #[arg(last = true)]
    pub cmd: Vec<String>,

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

    /// Logging driver (json-file, none) [default: json-file]
    #[arg(long, default_value = "json-file")]
    pub log_driver: String,

    /// Log driver options (KEY=VALUE), can be repeated
    #[arg(long = "log-opt")]
    pub log_opts: Vec<String>,
}

pub async fn execute(args: RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    let memory_mb = parse_memory(&args.memory)
        .map_err(|e| format!("Invalid --memory: {e}"))?;

    // Build resource limits before any partial moves of args
    let resource_limits = build_resource_limits(&args)?;

    // Parse logging config
    let log_driver: a3s_box_core::log::LogDriver = args.log_driver.parse()
        .map_err(|e: String| format!("Invalid --log-driver: {e}"))?;
    let log_opts = parse_env_vars(&args.log_opts)
        .map_err(|e| e.replace("environment variable", "log option"))?;
    let log_config = a3s_box_core::log::LogConfig {
        driver: log_driver,
        options: log_opts,
    };

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
    let health_status = if health_check.is_some() {
        "starting".to_string()
    } else {
        "none".to_string()
    };

    // Parse entrypoint override: split string into argv
    let entrypoint_override = args.entrypoint.as_ref().map(|ep| {
        ep.split_whitespace().map(String::from).collect::<Vec<_>>()
    });

    // Resolve named volumes (e.g., "mydata:/app/data" → "/home/user/.a3s/volumes/mydata:/app/data")
    let mut resolved_volumes = Vec::new();
    let mut volume_names = Vec::new();
    for vol_spec in &args.volumes {
        let (resolved, vol_name) = super::volume::resolve_named_volume(vol_spec)?;
        if let Some(name) = vol_name {
            volume_names.push(name);
        }
        resolved_volumes.push(resolved);
    }

    // Determine network mode
    let network_mode = match &args.network {
        Some(name) => a3s_box_core::NetworkMode::Bridge {
            network: name.clone(),
        },
        None => a3s_box_core::NetworkMode::Tsi,
    };

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
        volumes: resolved_volumes.clone(),
        extra_env: env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        port_map: args.publish.clone(),
        dns: args.dns.clone(),
        network: network_mode.clone(),
        tmpfs: args.tmpfs.clone(),
        resource_limits: resource_limits.clone(),
        ..Default::default()
    };

    // Create VmManager and boot
    let emitter = EventEmitter::new(256);
    let mut vm = VmManager::new(config, emitter);
    let box_id = vm.box_id().to_string();

    println!("Creating box {} ({})...", name, &BoxRecord::make_short_id(&box_id));

    // Register endpoint in network store BEFORE boot so the VM can find its IP
    if let Some(ref net_name) = args.network {
        let net_store = a3s_box_runtime::NetworkStore::default_path()?;
        let mut net_config = net_store
            .get(net_name)?
            .ok_or_else(|| format!("network '{}' not found", net_name))?;
        let endpoint = net_config
            .connect(&box_id, &name)
            .map_err(|e| format!("Failed to connect to network: {e}"))?;
        net_store.update(&net_config)?;
        println!("Connected to network {} (IP: {})", net_name, endpoint.ip_address);
    }

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
        volumes: resolved_volumes.clone(),
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
        hostname: args.hostname.clone(),
        user: args.user.clone(),
        workdir: args.workdir.clone(),
        restart_policy: args.restart.clone(),
        port_map: args.publish.clone(),
        labels,
        stopped_by_user: false,
        restart_count: 0,
        health_check,
        health_status,
        health_retries: 0,
        health_last_check: None,
        network_mode: network_mode.clone(),
        network_name: args.network.clone(),
        volume_names: volume_names.clone(),
        tmpfs: args.tmpfs.clone(),
        anonymous_volumes: vm.anonymous_volumes().to_vec(),
        resource_limits,
        log_config: log_config.clone(),
        max_restart_count: 0,
        exit_code: None,
    };

    let mut state = StateFile::load_default()?;
    state.add(record)?;

    // Spawn structured log processor (json-file driver writes container.json)
    let log_dir = box_dir.join("logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let _log_handle = a3s_box_runtime::log::spawn_log_processor(
        box_dir.join("logs").join("console.log"),
        log_dir,
        log_config,
    );

    // Attach named volumes to this box
    super::volume::attach_volumes(&volume_names, &box_id)?;

    if args.detach && args.tty {
        return Err("Cannot use -t (tty) with -d (detach)".into());
    }

    if args.tty && !std::io::stdin().is_terminal() {
        return Err("The -t flag requires a terminal (stdin is not a TTY)".into());
    }

    if args.detach {
        println!("{box_id}");
        return Ok(());
    }

    // Interactive PTY mode: connect to the guest PTY server
    if args.tty {
        use a3s_box_core::pty::PtyRequest;
        use a3s_box_runtime::PtyClient;
        use crossterm::terminal;

        let pty_socket_path = box_dir.join("sockets").join("pty.sock");

        // Wait for PTY socket to appear (guest init may still be starting)
        for _ in 0..50 {
            if pty_socket_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        if !pty_socket_path.exists() {
            return Err(format!(
                "PTY socket not found at {} (guest may not support interactive mode)",
                pty_socket_path.display()
            ).into());
        }

        // Build the command for the PTY session: use cmd override, entrypoint, or /bin/sh
        let pty_cmd = if !args.cmd.is_empty() {
            args.cmd.clone()
        } else if let Some(ref ep) = entrypoint_override {
            ep.clone()
        } else {
            vec!["/bin/sh".to_string()]
        };

        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let mut client = PtyClient::connect(&pty_socket_path).await?;
        client.send_request(&PtyRequest {
            cmd: pty_cmd,
            env: args.env.clone(),
            working_dir: args.workdir.clone(),
            user: args.user.clone(),
            cols,
            rows,
        }).await?;

        terminal::enable_raw_mode()?;
        let (read_half, write_half) = client.into_split();
        let exit_code = super::exec::run_pty_session(read_half, write_half).await;
        terminal::disable_raw_mode()?;

        // Clean up: destroy VM
        vm.destroy().await?;
        super::volume::detach_volumes(&volume_names, &box_id);
        if let Some(ref net_name) = args.network {
            let net_store = a3s_box_runtime::NetworkStore::default_path()?;
            if let Some(mut net_config) = net_store.get(net_name)? {
                net_config.disconnect(&box_id).ok();
                net_store.update(&net_config)?;
            }
        }

        let mut state = StateFile::load_default()?;
        if let Some(rec) = state.find_by_id_mut(&box_id) {
            rec.status = "stopped".to_string();
            rec.pid = None;
        }
        if args.rm {
            state.remove(&box_id)?;
            let _ = std::fs::remove_dir_all(&box_dir);
        } else {
            state.save()?;
        }

        if exit_code != 0 {
            std::process::exit(exit_code);
        }
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

    // Detach named volumes
    super::volume::detach_volumes(&volume_names, &box_id);

    // Disconnect from network if connected
    if let Some(ref net_name) = args.network {
        let net_store = a3s_box_runtime::NetworkStore::default_path()?;
        if let Some(mut net_config) = net_store.get(net_name)? {
            net_config.disconnect(&box_id).ok();
            net_store.update(&net_config)?;
        }
    }

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

/// Build ResourceLimits from CLI args.
fn build_resource_limits(args: &RunArgs) -> Result<ResourceLimits, Box<dyn std::error::Error>> {
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
