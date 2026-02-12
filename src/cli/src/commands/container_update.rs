//! `a3s-box container-update` command — Update resource limits on a running box.
//!
//! Similar to `docker update`, allows changing CPU and memory limits
//! without restarting the box. Changes are persisted to the state file.

use clap::Args;

use crate::output::parse_memory;
use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct ContainerUpdateArgs {
    /// Box name or ID
    pub name: String,

    /// Number of CPUs
    #[arg(long)]
    pub cpus: Option<u32>,

    /// Memory limit (e.g., "512m", "2g")
    #[arg(long)]
    pub memory: Option<String>,

    /// Memory reservation/soft limit (e.g., "256m", "1g")
    #[arg(long)]
    pub memory_reservation: Option<String>,

    /// Memory+swap limit (e.g., "1g", "-1" for unlimited)
    #[arg(long)]
    pub memory_swap: Option<String>,

    /// Limit PIDs inside the box
    #[arg(long)]
    pub pids_limit: Option<u64>,

    /// CPU shares (relative weight, 2-262144)
    #[arg(long)]
    pub cpu_shares: Option<u64>,

    /// CPU quota in microseconds per cpu-period
    #[arg(long)]
    pub cpu_quota: Option<i64>,

    /// CPU period in microseconds
    #[arg(long)]
    pub cpu_period: Option<u64>,

    /// Pin to specific CPUs (e.g., "0,1,3" or "0-3")
    #[arg(long)]
    pub cpuset_cpus: Option<String>,

    /// Restart policy: no, always, on-failure, unless-stopped
    #[arg(long)]
    pub restart: Option<String>,
}

pub async fn execute(args: ContainerUpdateArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = StateFile::load_default()?;
    let record = resolve::resolve_mut(&mut state, &args.name)?;

    let name = record.name.clone();
    let mut updated = Vec::new();

    if let Some(cpus) = args.cpus {
        record.cpus = cpus;
        updated.push(format!("cpus={cpus}"));
    }

    if let Some(ref mem_str) = args.memory {
        let mb = parse_memory(mem_str).map_err(|e| format!("Invalid --memory: {e}"))?;
        record.memory_mb = mb;
        updated.push(format!("memory={mem_str}"));
    }

    if let Some(ref reservation) = args.memory_reservation {
        let bytes = parse_memory_bytes(reservation)
            .map_err(|e| format!("Invalid --memory-reservation: {e}"))?;
        record.resource_limits.memory_reservation = Some(bytes);
        updated.push(format!("memory-reservation={reservation}"));
    }

    if let Some(ref swap) = args.memory_swap {
        let val = if swap == "-1" {
            -1i64
        } else {
            parse_memory_bytes(swap).map_err(|e| format!("Invalid --memory-swap: {e}"))? as i64
        };
        record.resource_limits.memory_swap = Some(val);
        updated.push(format!("memory-swap={swap}"));
    }

    if let Some(pids) = args.pids_limit {
        record.resource_limits.pids_limit = Some(pids);
        updated.push(format!("pids-limit={pids}"));
    }

    if let Some(shares) = args.cpu_shares {
        record.resource_limits.cpu_shares = Some(shares);
        updated.push(format!("cpu-shares={shares}"));
    }

    if let Some(quota) = args.cpu_quota {
        record.resource_limits.cpu_quota = Some(quota);
        updated.push(format!("cpu-quota={quota}"));
    }

    if let Some(period) = args.cpu_period {
        record.resource_limits.cpu_period = Some(period);
        updated.push(format!("cpu-period={period}"));
    }

    if let Some(ref cpuset) = args.cpuset_cpus {
        record.resource_limits.cpuset_cpus = Some(cpuset.clone());
        updated.push(format!("cpuset-cpus={cpuset}"));
    }

    if let Some(ref restart) = args.restart {
        let (policy, max_count) = crate::state::parse_restart_policy(restart)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        record.restart_policy = policy;
        record.max_restart_count = max_count;
        updated.push(format!("restart={restart}"));
    }

    if updated.is_empty() {
        println!("No updates specified.");
        return Ok(());
    }

    state.save()?;
    println!("{name}");

    Ok(())
}

/// Parse a memory size string into bytes.
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
    fn test_parse_memory_bytes_megabytes() {
        assert_eq!(parse_memory_bytes("512m").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_memory_bytes("512mb").unwrap(), 512 * 1024 * 1024);
    }

    #[test]
    fn test_parse_memory_bytes_gigabytes() {
        assert_eq!(parse_memory_bytes("2g").unwrap(), 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn test_parse_memory_bytes_raw() {
        assert_eq!(parse_memory_bytes("1048576").unwrap(), 1048576);
    }

    #[test]
    fn test_parse_memory_bytes_empty() {
        assert!(parse_memory_bytes("").is_err());
    }

    #[test]
    fn test_parse_memory_bytes_invalid() {
        assert!(parse_memory_bytes("abc").is_err());
    }
}
