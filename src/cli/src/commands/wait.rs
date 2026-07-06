//! `a3s-box wait` command — Block until one or more boxes stop, then print exit codes.

use clap::Args;

use crate::process;
use crate::resolve;
use crate::state::{BoxRecord, StateFile};

const WAIT_POLL_MILLIS: u64 = 500;
const DEFAULT_HEARTBEAT_SECS: u64 = 60;

#[derive(Args)]
pub struct WaitArgs {
    /// Box name(s) or ID(s)
    #[arg(required = true)]
    pub boxes: Vec<String>,

    /// Seconds between stderr keepalive messages while waiting (0 disables)
    #[arg(long, default_value_t = DEFAULT_HEARTBEAT_SECS)]
    pub heartbeat_interval: u64,

    /// Disable stderr keepalive messages while waiting
    #[arg(long)]
    pub no_heartbeat: bool,
}

pub async fn execute(args: WaitArgs) -> Result<(), Box<dyn std::error::Error>> {
    let heartbeat_interval = wait_heartbeat_interval(&args);
    for query in &args.boxes {
        wait_one(query, heartbeat_interval).await?;
    }
    Ok(())
}

async fn wait_one(
    query: &str,
    heartbeat_interval: Option<std::time::Duration>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut heartbeat = WaitHeartbeat::new(heartbeat_interval);
    loop {
        let state = StateFile::load_default()?;
        let record = resolve::resolve(&state, query)?;

        match wait_poll_action(record) {
            WaitPollAction::Finish(exit_code) => {
                println!("{exit_code}");
                return Ok(());
            }
            WaitPollAction::Sleep => {
                heartbeat.maybe_emit(query);
                tokio::time::sleep(tokio::time::Duration::from_millis(WAIT_POLL_MILLIS)).await;
            }
        }
    }
}

fn wait_heartbeat_interval(args: &WaitArgs) -> Option<std::time::Duration> {
    if args.no_heartbeat || args.heartbeat_interval == 0 {
        None
    } else {
        Some(std::time::Duration::from_secs(args.heartbeat_interval))
    }
}

struct WaitHeartbeat {
    interval: Option<std::time::Duration>,
    started: std::time::Instant,
    next: std::time::Instant,
}

impl WaitHeartbeat {
    fn new(interval: Option<std::time::Duration>) -> Self {
        let now = std::time::Instant::now();
        let next = interval.map(|interval| now + interval).unwrap_or(now);
        Self {
            interval,
            started: now,
            next,
        }
    }

    fn maybe_emit(&mut self, query: &str) {
        let Some(interval) = self.interval else {
            return;
        };

        let now = std::time::Instant::now();
        if now < self.next {
            return;
        }

        eprintln!(
            "a3s-box wait: still waiting for {query} ({}s)",
            now.duration_since(self.started).as_secs()
        );
        self.next = now + interval;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitPollAction {
    Finish(i32),
    Sleep,
}

fn wait_poll_action(record: &BoxRecord) -> WaitPollAction {
    match record.status.as_str() {
        "running" | "paused" => match record.pid {
            Some(pid) if process::is_process_alive_with_identity(pid, record.pid_start_time) => {
                WaitPollAction::Sleep
            }
            _ => WaitPollAction::Finish(wait_exit_code(record)),
        },
        "created" => WaitPollAction::Sleep,
        "stopped" | "dead" => WaitPollAction::Finish(wait_exit_code(record)),
        _ => WaitPollAction::Finish(0),
    }
}

fn wait_exit_code(record: &BoxRecord) -> i32 {
    record.exit_code.unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wait_exit_code_defaults_to_success() {
        let record = crate::test_helpers::fixtures::make_record("id", "box", "stopped", None);
        assert_eq!(wait_exit_code(&record), 0);
    }

    #[test]
    fn test_wait_exit_code_uses_recorded_code() {
        let mut record = crate::test_helpers::fixtures::make_record("id", "box", "stopped", None);
        record.exit_code = Some(42);
        assert_eq!(wait_exit_code(&record), 42);
    }

    #[test]
    fn test_wait_poll_action_keeps_waiting_for_paused_live_process() {
        let record = crate::test_helpers::fixtures::make_record(
            "id",
            "box",
            "paused",
            Some(std::process::id()),
        );

        assert_eq!(wait_poll_action(&record), WaitPollAction::Sleep);
    }

    #[test]
    fn test_wait_poll_action_finishes_for_paused_without_pid() {
        let record = crate::test_helpers::fixtures::make_record("id", "box", "paused", None);

        assert_eq!(wait_poll_action(&record), WaitPollAction::Finish(0));
    }

    #[test]
    fn test_wait_heartbeat_interval_can_be_disabled() {
        assert!(wait_heartbeat_interval(&WaitArgs {
            boxes: vec!["box".to_string()],
            heartbeat_interval: 60,
            no_heartbeat: true,
        })
        .is_none());
        assert!(wait_heartbeat_interval(&WaitArgs {
            boxes: vec!["box".to_string()],
            heartbeat_interval: 0,
            no_heartbeat: false,
        })
        .is_none());
    }
}
