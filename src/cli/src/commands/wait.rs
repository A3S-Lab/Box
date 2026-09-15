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

    /// Maximum seconds to wait for all boxes; does not stop them on timeout
    #[arg(long, value_name = "SECONDS")]
    pub timeout: Option<u64>,
}

pub async fn execute(args: WaitArgs) -> Result<(), Box<dyn std::error::Error>> {
    let timeout = wait_timeout(&args)?;
    let deadline = timeout.and_then(|timeout| tokio::time::Instant::now().checked_add(timeout));
    if timeout.is_some() && deadline.is_none() {
        return Err("--timeout is too large".into());
    }
    let heartbeat_interval = wait_heartbeat_interval(&args);
    for query in &args.boxes {
        match deadline {
            Some(deadline) => {
                tokio::time::timeout_at(deadline, wait_one(query, heartbeat_interval))
                    .await
                    .map_err(|_| {
                        format!(
                            "timed out after {}s while waiting for {query}; the box was not stopped",
                            args.timeout.expect("validated timeout")
                        )
                    })??;
            }
            None => wait_one(query, heartbeat_interval).await?,
        }
    }
    Ok(())
}

async fn wait_one(
    query: &str,
    heartbeat_interval: Option<std::time::Duration>,
) -> Result<(), Box<dyn std::error::Error>> {
    use a3s_box_core::{ExecutionId, ExecutionManager, ExecutionState};

    let mut heartbeat = WaitHeartbeat::new(heartbeat_interval);
    let mut managed_manager = None;
    loop {
        let state = StateFile::load_default()?;
        let record = match resolve::resolve(&state, query) {
            Ok(record) => record,
            Err(error @ resolve::ResolveError::NotFound(_)) => {
                match archived_wait_exit_code(query)? {
                    Some(exit_code) => {
                        println!("{exit_code}");
                        return Ok(());
                    }
                    None => {
                        if crate::log_archive::resolve_archive(query)?.is_some() {
                            return Err(format!(
                                "box {query} was removed without a recorded exit code"
                            )
                            .into());
                        }
                        return Err(error.into());
                    }
                }
            }
            Err(error) => return Err(error.into()),
        };

        if uses_managed_execution(record) {
            if managed_manager.is_none() {
                let home = a3s_box_core::dirs_home();
                managed_manager = Some(super::configured_local_execution_manager(&home).await?);
            }
            let manager = managed_manager
                .as_ref()
                .expect("managed execution manager initialized");
            // Abandoned Removing must resume finish_remove — inspect returns
            // Conflict and would hard-fail wait forever.
            if record.status == "removing" {
                let execution_id = ExecutionId::new(record.id.clone())?;
                let generation = record
                    .managed_execution
                    .as_ref()
                    .map(|metadata| metadata.generation)
                    .ok_or_else(|| {
                        format!("box {} lost managed generation while waiting", record.id)
                    })?;
                let _ = manager.remove(&execution_id, generation).await?;
                // Remove forgets the row; only finish when archive recorded an
                // exit — never invent success (0) for an unknown code.
                match archived_wait_exit_code(query)? {
                    Some(exit_code) => {
                        println!("{exit_code}");
                        return Ok(());
                    }
                    None => {
                        return Err(format!(
                            "box {query} was removed without a recorded exit code"
                        )
                        .into());
                    }
                }
            }
            // Abandoned RestartStopping/RestartStarting: inspect keeps Creating
            // forever — resume via reconcile(create operation), then re-poll.
            if matches!(
                record.status.as_str(),
                "restart_stopping" | "restart_starting"
            ) {
                super::observe_inventory::resume_managed_restart_claims(
                    manager,
                    std::slice::from_ref(record),
                )
                .await?;
                heartbeat.maybe_emit(query);
                tokio::time::sleep(tokio::time::Duration::from_millis(WAIT_POLL_MILLIS)).await;
                continue;
            }
            let status = manager
                .inspect(&ExecutionId::new(record.id.clone())?)
                .await?;
            match status.state {
                ExecutionState::Stopped | ExecutionState::Failed => {
                    // inspect persisted the terminal result (and may retire
                    // abandoned Starting via #385/#386 without inventing exit).
                    // Reuse the legacy poll gate so missing exit keeps waiting.
                    let refreshed = StateFile::load_default()?;
                    let refreshed = resolve::resolve(&refreshed, query)?;
                    if let WaitPollAction::Finish(exit_code) =
                        managed_terminal_wait_action(refreshed)
                    {
                        println!("{exit_code}");
                        return Ok(());
                    }
                }
                ExecutionState::Created | ExecutionState::Creating => {}
                ExecutionState::Running | ExecutionState::Paused => {}
            }
            heartbeat.maybe_emit(query);
            tokio::time::sleep(tokio::time::Duration::from_millis(WAIT_POLL_MILLIS)).await;
            continue;
        }

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

/// Any managed record must wait through manager inspect — not only OCI-routed
/// ones. MicroVM managed Starting otherwise hits the legacy poll path and can
/// invent exit 0 while durable status is still `starting`.
fn uses_managed_execution(record: &BoxRecord) -> bool {
    record.managed_execution.is_some()
}

fn archived_wait_exit_code(query: &str) -> Result<Option<i32>, String> {
    Ok(crate::log_archive::resolve_archive(query)?.and_then(|archive| archive.exit_code))
}

fn wait_heartbeat_interval(args: &WaitArgs) -> Option<std::time::Duration> {
    if args.no_heartbeat || args.heartbeat_interval == 0 {
        None
    } else {
        Some(std::time::Duration::from_secs(args.heartbeat_interval))
    }
}

fn wait_timeout(args: &WaitArgs) -> Result<Option<std::time::Duration>, &'static str> {
    match args.timeout {
        Some(0) => Err("--timeout must be greater than zero"),
        Some(seconds) => Ok(Some(std::time::Duration::from_secs(seconds))),
        None => Ok(None),
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
            // The shim/host process can disappear before the durable exit code is
            // written. Keep polling until an authoritative code is available so
            // `wait` never invents success (0) ahead of inspect/ps.
            _ => match record.exit_code {
                Some(code) => WaitPollAction::Finish(code),
                None => WaitPollAction::Sleep,
            },
        },
        // Transitional / reserved claims: keep waiting. Never invent exit 0 for
        // `starting`/`creating`/restart claims (legacy `_ => Finish(0)` lie).
        "created" | "creating" | "starting" | "killing" | "pausing" | "resuming"
        | "restart_stopping" | "restart_starting" | "removing" | "snapshotting"
        | "updating_resources" => WaitPollAction::Sleep,
        "stopped" | "dead" | "failed" => match record.exit_code {
            Some(code) => WaitPollAction::Finish(code),
            None => WaitPollAction::Sleep,
        },
        // Unknown status: only finish when an exit was recorded; otherwise sleep.
        _ => match record.exit_code {
            Some(code) => WaitPollAction::Finish(code),
            None => WaitPollAction::Sleep,
        },
    }
}

/// Managed inspect Stopped/Failed finish decision — same gate as legacy poll.
fn managed_terminal_wait_action(record: &BoxRecord) -> WaitPollAction {
    wait_poll_action(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_managed_terminal_wait_does_not_invent_exit_zero() {
        let stopped = crate::test_helpers::fixtures::make_record("id", "box", "stopped", None);
        assert_eq!(
            managed_terminal_wait_action(&stopped),
            WaitPollAction::Sleep,
            "managed wait must not Finish(0) when exit_code is absent after inspect retire"
        );

        let failed = crate::test_helpers::fixtures::make_record("id", "box", "failed", None);
        assert_eq!(managed_terminal_wait_action(&failed), WaitPollAction::Sleep);

        let mut with_exit =
            crate::test_helpers::fixtures::make_record("id", "box", "stopped", None);
        with_exit.exit_code = Some(137);
        assert_eq!(
            managed_terminal_wait_action(&with_exit),
            WaitPollAction::Finish(137)
        );
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
    fn test_wait_poll_action_keeps_waiting_when_exit_code_is_missing() {
        let record = crate::test_helpers::fixtures::make_record("id", "box", "paused", None);
        assert_eq!(wait_poll_action(&record), WaitPollAction::Sleep);

        let record = crate::test_helpers::fixtures::make_record("id", "box", "stopped", None);
        assert_eq!(wait_poll_action(&record), WaitPollAction::Sleep);

        let record = crate::test_helpers::fixtures::make_record("id", "box", "running", None);
        assert_eq!(wait_poll_action(&record), WaitPollAction::Sleep);
    }

    #[test]
    fn test_wait_poll_action_finishes_with_recorded_exit_code() {
        let mut record = crate::test_helpers::fixtures::make_record("id", "box", "stopped", None);
        record.exit_code = Some(137);
        assert_eq!(wait_poll_action(&record), WaitPollAction::Finish(137));

        let mut record = crate::test_helpers::fixtures::make_record("id", "box", "running", None);
        record.exit_code = Some(137);
        assert_eq!(wait_poll_action(&record), WaitPollAction::Finish(137));

        let mut failed = crate::test_helpers::fixtures::make_record("id", "box", "failed", None);
        failed.exit_code = Some(1);
        assert_eq!(wait_poll_action(&failed), WaitPollAction::Finish(1));
    }

    #[test]
    fn test_wait_poll_action_does_not_invent_exit_for_transitional_status() {
        for status in [
            "starting",
            "creating",
            "killing",
            "restart_starting",
            "restart_stopping",
            "pausing",
            "resuming",
            "removing",
            "snapshotting",
            "updating_resources",
        ] {
            let record = crate::test_helpers::fixtures::make_record("id", "box", status, None);
            assert_eq!(
                wait_poll_action(&record),
                WaitPollAction::Sleep,
                "status {status} must not invent Finish(0)"
            );
        }

        let failed_no_exit =
            crate::test_helpers::fixtures::make_record("id", "box", "failed", None);
        assert_eq!(wait_poll_action(&failed_no_exit), WaitPollAction::Sleep);
    }

    #[test]
    fn test_uses_managed_execution_when_metadata_present() {
        use std::collections::BTreeMap;

        use a3s_box_core::{BoxConfig, CreateExecutionRequest, ExecutionGeneration, OperationId};
        use a3s_box_runtime::ManagedExecutionMetadata;

        let unmanaged = crate::test_helpers::fixtures::make_record("id", "box", "starting", None);
        assert!(!uses_managed_execution(&unmanaged));

        let mut managed = crate::test_helpers::fixtures::make_record(
            "22222222-2222-4222-8222-222222222222",
            "box",
            "starting",
            None,
        );
        managed.managed_execution = Some(
            ManagedExecutionMetadata::new(
                OperationId::new("op-wait").unwrap(),
                ExecutionGeneration::INITIAL,
                CreateExecutionRequest {
                    external_sandbox_id: "ext".into(),
                    config: BoxConfig::default(),
                    labels: BTreeMap::new(),
                    policy: Default::default(),
                    rootfs_snapshot_id: None,
                },
            )
            .unwrap(),
        );
        assert!(uses_managed_execution(&managed));
    }

    #[test]
    fn test_wait_heartbeat_interval_can_be_disabled() {
        assert!(wait_heartbeat_interval(&WaitArgs {
            boxes: vec!["box".to_string()],
            heartbeat_interval: 60,
            no_heartbeat: true,
            timeout: None,
        })
        .is_none());
        assert!(wait_heartbeat_interval(&WaitArgs {
            boxes: vec!["box".to_string()],
            heartbeat_interval: 0,
            no_heartbeat: false,
            timeout: None,
        })
        .is_none());
    }

    #[test]
    fn test_wait_timeout_rejects_zero_and_accepts_positive_seconds() {
        let mut args = WaitArgs {
            boxes: vec!["box".to_string()],
            heartbeat_interval: 60,
            no_heartbeat: false,
            timeout: Some(0),
        };
        assert_eq!(
            wait_timeout(&args),
            Err("--timeout must be greater than zero")
        );
        args.timeout = Some(7);
        assert_eq!(
            wait_timeout(&args),
            Ok(Some(std::time::Duration::from_secs(7)))
        );
    }
}
