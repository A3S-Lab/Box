//! `a3s-box stats` command — Display live resource usage statistics.
//!
//! Shows CPU and memory usage for running boxes, similar to `docker stats`.
//! By default streams updates every second; use `--no-stream` for a single snapshot.

use clap::Args;
use sysinfo::{Pid, System};

use crate::output;
use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct StatsArgs {
    /// Box name or ID (shows all running boxes if omitted)
    pub r#box: Option<String>,

    /// Disable streaming and print a single snapshot
    #[arg(long)]
    pub no_stream: bool,
}

/// Collected stats for a single box.
struct BoxStats {
    name: String,
    short_id: String,
    pid: u32,
    cpu_percent: f32,
    memory_bytes: u64,
    memory_limit_bytes: u64,
}

/// Collect stats for a process by PID.
///
/// Requires two `refresh_process` calls with a delay between them
/// for accurate CPU measurement (sysinfo computes CPU as a delta).
fn collect_stats(sys: &mut System, pid: u32, memory_limit_mb: u32) -> Option<(f32, u64)> {
    let spid = Pid::from_u32(pid);

    // First refresh to establish baseline
    sys.refresh_process(spid);
    std::thread::sleep(std::time::Duration::from_millis(200));
    // Second refresh to compute CPU delta
    sys.refresh_process(spid);

    sys.process(spid).map(|proc_info| {
        let cpu = proc_info.cpu_usage();
        let mem = proc_info.memory();
        let _ = memory_limit_mb; // used by caller
        (cpu, mem)
    })
}

/// Print a stats table for the given boxes.
fn print_stats(stats: &[BoxStats]) {
    let mut table = output::new_table(&[
        "BOX ID",
        "NAME",
        "CPU %",
        "MEM USAGE / LIMIT",
        "MEM %",
        "PID",
    ]);

    for s in stats {
        let mem_pct = if s.memory_limit_bytes > 0 {
            (s.memory_bytes as f64 / s.memory_limit_bytes as f64) * 100.0
        } else {
            0.0
        };

        table.add_row(&[
            &s.short_id,
            &s.name,
            &format!("{:.2}%", s.cpu_percent),
            &format!(
                "{} / {}",
                output::format_bytes(s.memory_bytes),
                output::format_bytes(s.memory_limit_bytes)
            ),
            &format!("{:.1}%", mem_pct),
            &s.pid.to_string(),
        ]);
    }

    println!("{table}");
}

pub async fn execute(args: StatsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut sys = System::new();

    loop {
        let state = StateFile::load_default()?;

        // Determine which boxes to show
        let targets: Vec<_> = if let Some(ref name) = args.r#box {
            let record = resolve::resolve(&state, name)?;
            if record.status != "running" {
                return Err(format!("Box {} is not running", record.name).into());
            }
            vec![record.clone()]
        } else {
            state.list(false).into_iter().cloned().collect()
        };

        if targets.is_empty() {
            println!("No running boxes");
            return Ok(());
        }

        // Collect stats for each running box
        let mut stats = Vec::new();
        for record in &targets {
            if let Some(pid) = record.pid {
                let memory_limit_bytes = (record.memory_mb as u64) * 1024 * 1024;
                if let Some((cpu, mem)) = collect_stats(&mut sys, pid, record.memory_mb) {
                    stats.push(BoxStats {
                        name: record.name.clone(),
                        short_id: record.short_id.clone(),
                        pid,
                        cpu_percent: cpu,
                        memory_bytes: mem,
                        memory_limit_bytes,
                    });
                }
            }
        }

        // Clear screen for streaming mode (except first iteration)
        if !args.no_stream {
            // Use ANSI escape to move cursor to top and clear
            print!("\x1B[2J\x1B[H");
        }

        print_stats(&stats);

        if args.no_stream {
            break;
        }

        // Wait before next refresh
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }

    Ok(())
}
