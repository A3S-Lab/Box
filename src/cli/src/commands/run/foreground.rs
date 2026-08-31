//! Foreground run lifecycle, signal handling, and terminal log draining.

use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

// ============================================================================
// Phase 2b: Foreground mode (tail logs, wait for exit or Ctrl-C)
// ============================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ForegroundStopReason {
    ProcessExited,
    UserInterrupted(i32),
    VmUnhealthy,
    TimedOut,
}

#[cfg(unix)]
type ForegroundTerminateSignal = Option<tokio::signal::unix::Signal>;
#[cfg(not(unix))]
struct ForegroundTerminateSignal;

#[cfg(unix)]
pub(super) const FOREGROUND_SIGINT: i32 = libc::SIGINT;
#[cfg(not(unix))]
pub(super) const FOREGROUND_SIGINT: i32 = 2;
#[cfg(unix)]
pub(super) const FOREGROUND_SIGTERM: i32 = libc::SIGTERM;
#[cfg(not(unix))]
pub(super) const FOREGROUND_SIGTERM: i32 = 15;

pub(super) const FOREGROUND_LOG_DRAIN_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(2);
pub(super) const FOREGROUND_EXIT_POLL: std::time::Duration = std::time::Duration::from_millis(20);
pub(super) const FOREGROUND_OCI_EXIT_POLL: std::time::Duration =
    std::time::Duration::from_millis(100);
pub(super) const FOREGROUND_HEALTH_POLL: std::time::Duration =
    std::time::Duration::from_millis(500);
pub(super) const FOREGROUND_LOG_DRAIN_QUIET: std::time::Duration =
    std::time::Duration::from_millis(50);
pub(super) const FOREGROUND_LOG_DRAIN_POLL: std::time::Duration =
    std::time::Duration::from_millis(10);

impl ForegroundStopReason {
    pub(super) fn stopped_by_user(self) -> bool {
        matches!(self, Self::UserInterrupted(_))
    }
}

#[cfg(unix)]
fn foreground_terminate_signal() -> ForegroundTerminateSignal {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok()
}

#[cfg(not(unix))]
fn foreground_terminate_signal() -> ForegroundTerminateSignal {
    ForegroundTerminateSignal
}

#[cfg(unix)]
async fn recv_foreground_terminate(signal: &mut ForegroundTerminateSignal) {
    if let Some(signal) = signal {
        let _ = signal.recv().await;
    } else {
        std::future::pending::<()>().await;
    }
}

#[cfg(not(unix))]
async fn recv_foreground_terminate(_signal: &mut ForegroundTerminateSignal) {
    std::future::pending::<()>().await;
}

pub(super) async fn run_foreground(
    mut ctx: RunContext,
    args: &RunArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let foreground_start = std::time::Instant::now();
    println!(
        "Box {} ({}) started. Press Ctrl-C to stop.",
        ctx.name,
        BoxRecord::make_short_id(&ctx.box_id)
    );

    #[cfg(target_os = "windows")]
    let (console_log, console_err) = {
        // WHPX persists workload output in the shared rootfs. The shim tails
        // these files into container.json, while the conventional raw console
        // files only receive a completed-stream fallback after exit.
        let rootfs = ctx.box_dir.join("rootfs");
        (
            rootfs.join("guest-init.stdout.log"),
            rootfs.join("guest-init.stderr.log"),
        )
    };
    #[cfg(not(target_os = "windows"))]
    let (console_log, console_err) = (
        ctx.box_dir.join("logs").join("console.log"),
        ctx.box_dir.join("logs").join("console.err.log"),
    );
    let stdout_pos = Arc::new(AtomicU64::new(0));
    let stderr_pos = Arc::new(AtomicU64::new(0));
    let tail_stdout_pos = Arc::clone(&stdout_pos);
    let tail_stderr_pos = Arc::clone(&stderr_pos);
    let tail_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stdout_tail_stop = Arc::clone(&tail_stop);
    let stderr_tail_stop = Arc::clone(&tail_stop);
    let tail_console_log = console_log.clone();
    let tail_console_err = console_err.clone();
    let runtime_filter = cfg!(target_os = "windows")
        .then(|| Arc::new(a3s_box_core::log::RuntimeConsoleFilter::new()));
    let stdout_runtime_filter = runtime_filter.clone();
    let stderr_runtime_filter = runtime_filter;
    let mut log_handle = tokio::spawn(async move {
        // Stream the selected raw stdout/stderr sources to the terminal.
        tokio::join!(
            super::super::tail_file_stream_positioned(
                &tail_console_log,
                false,
                Some(tail_stdout_pos),
                Some(stdout_tail_stop),
                stdout_runtime_filter,
            ),
            super::super::tail_file_stream_positioned(
                &tail_console_err,
                true,
                Some(tail_stderr_pos),
                Some(stderr_tail_stop),
                stderr_runtime_filter,
            ),
        );
    });

    let name = ctx.name.clone();
    let mut terminate_signal = foreground_terminate_signal();
    let timeout_at = args
        .timeout
        .map(|secs| tokio::time::Instant::now() + std::time::Duration::from_secs(secs));
    // Process exit is latency-sensitive for short foreground commands, while a
    // VM health check is comparatively expensive and only needs the existing
    // 500 ms cadence. Keeping independent timers avoids adding a fixed half
    // second to every no-op without polling health more aggressively.
    let exit_poll_period = if run_context_uses_oci(&ctx) {
        // OCI inspection crosses the SDK/service boundary; a 100 ms cadence
        // remains responsive without issuing 50 control requests per second.
        FOREGROUND_OCI_EXIT_POLL
    } else {
        FOREGROUND_EXIT_POLL
    };
    let mut exit_poll = tokio::time::interval(exit_poll_period);
    exit_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut health_poll = tokio::time::interval_at(
        tokio::time::Instant::now() + FOREGROUND_HEALTH_POLL,
        FOREGROUND_HEALTH_POLL,
    );
    health_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let stop_reason = loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("\nStopping box {}...", name);
                break ForegroundStopReason::UserInterrupted(FOREGROUND_SIGINT);
            }
            _ = recv_foreground_terminate(&mut terminate_signal) => {
                println!("\nStopping box {} after SIGTERM...", name);
                break ForegroundStopReason::UserInterrupted(FOREGROUND_SIGTERM);
            }
            _ = recv_foreground_timeout(timeout_at) => {
                println!("\nStopping box {} after --timeout expired...", name);
                break ForegroundStopReason::TimedOut;
            }
            _ = exit_poll.tick() => {
                if !managed_process_alive(&mut ctx).await {
                    break ForegroundStopReason::ProcessExited;
                }
            }
            _ = health_poll.tick() => {
                if !managed_runtime_healthy(&ctx).await {
                    break foreground_health_stop_reason(foreground_workload_exit_code(
                        &ctx.box_dir,
                        ctx.record.exit_code,
                    ));
                }
            }
        }
    };
    a3s_box_core::lifecycle_profile::record_lifecycle_phase(
        "foreground.command_execution",
        foreground_start.elapsed(),
    );

    let natural_exit = stop_reason == ForegroundStopReason::ProcessExited;
    let sandbox_natural_exit = natural_exit && ctx.record.isolation.is_sandbox();
    if sandbox_natural_exit {
        // The generation-owned worker exits only after the A3S OCI owner has closed both
        // raw console streams and projected their final records. Once it is
        // gone, the terminal tailers can catch up to immutable file lengths
        // without an additional writer-quiet grace period.
        let structured_log_drain_start = std::time::Instant::now();
        wait_for_sandbox_structured_log_drain(&ctx).await?;
        a3s_box_core::lifecycle_profile::record_lifecycle_phase(
            "foreground.structured_log_drain",
            structured_log_drain_start.elapsed(),
        );
    }

    let raw_log_drain_start = std::time::Instant::now();
    // A natural Sandbox completion is published only after its generation-owned
    // log worker exits. A natural MicroVM completion is published only after the
    // shim closes the raw streams and joins its log processor. In both cases the
    // lengths are immutable, so wait only for the terminal tailers to catch up.
    wait_for_foreground_log_drain(
        &[(&console_log, &stdout_pos), (&console_err, &stderr_pos)],
        natural_exit,
    )
    .await;
    a3s_box_core::lifecycle_profile::record_lifecycle_phase(
        "foreground.raw_log_drain",
        raw_log_drain_start.elapsed(),
    );
    tail_stop.store(true, Ordering::Release);
    if tokio::time::timeout(std::time::Duration::from_secs(1), &mut log_handle)
        .await
        .is_err()
    {
        log_handle.abort();
    }

    if stop_reason == ForegroundStopReason::ProcessExited && !sandbox_natural_exit {
        let structured_log_drain_start = std::time::Instant::now();
        wait_for_sandbox_structured_log_drain(&ctx).await?;
        a3s_box_core::lifecycle_profile::record_lifecycle_phase(
            "foreground.structured_log_drain",
            structured_log_drain_start.elapsed(),
        );
    }

    let persisted_exit_code = foreground_workload_exit_code(&ctx.box_dir, ctx.record.exit_code);
    let exit_code = foreground_exit_code(stop_reason, persisted_exit_code);
    let archive_start = std::time::Instant::now();
    archive_auto_removed_logs(&ctx, args.rm, exit_code, stop_reason.stopped_by_user());
    a3s_box_core::lifecycle_profile::record_lifecycle_phase(
        "foreground.archive",
        archive_start.elapsed(),
    );
    cleanup_managed_execution(
        &mut ctx,
        args.rm,
        exit_code,
        stop_reason.stopped_by_user(),
        stop_reason == ForegroundStopReason::ProcessExited,
    )
    .await?;
    println!(
        "{}",
        foreground_completion_message(stop_reason, args.rm, &ctx.name)
    );

    if let Some(code) = exit_code {
        if code != 0 {
            std::process::exit(code);
        }
    }

    Ok(())
}

async fn wait_for_sandbox_structured_log_drain(
    ctx: &RunContext,
) -> Result<(), Box<dyn std::error::Error>> {
    if !ctx.record.isolation.is_sandbox() || run_context_uses_oci(ctx) {
        return Ok(());
    }
    let box_dir = ctx.box_dir.clone();
    let box_id = ctx.box_id.clone();
    let drained = tokio::task::spawn_blocking(move || {
        a3s_box_runtime::vm::reap::wait_for_recorded_sandbox_log_drain(
            &box_dir,
            &box_id,
            std::time::Duration::from_secs(3),
        )
    })
    .await
    .map_err(|error| format!("Sandbox log drain task failed for {}: {error}", ctx.box_id))??;
    if !drained {
        return Err(format!(
            "Sandbox logs did not finish draining for {}; state was preserved for recovery",
            ctx.box_id
        )
        .into());
    }
    Ok(())
}

async fn recv_foreground_timeout(deadline: Option<tokio::time::Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline).await;
    } else {
        std::future::pending::<()>().await;
    }
}

pub(super) async fn wait_for_foreground_log_drain(
    paths: &[(&std::path::Path, &AtomicU64)],
    writers_finished: bool,
) {
    let start = std::time::Instant::now();
    let mut last_lens = foreground_log_lengths(paths);
    let mut quiet_since = None;

    loop {
        let lens = foreground_log_lengths(paths);
        let lengths_stable = lens == last_lens;
        let tails_caught_up = paths
            .iter()
            .zip(lens.iter())
            .all(|((_, pos), len)| pos.load(Ordering::Relaxed) >= *len);

        if writers_finished && tails_caught_up {
            break;
        }

        if lengths_stable && tails_caught_up {
            let now = std::time::Instant::now();
            match quiet_since {
                Some(since) if now.duration_since(since) >= FOREGROUND_LOG_DRAIN_QUIET => break,
                Some(_) => {}
                None => quiet_since = Some(now),
            }
        } else {
            last_lens = lens;
            quiet_since = None;
        }

        if start.elapsed() >= FOREGROUND_LOG_DRAIN_TIMEOUT {
            break;
        }

        tokio::time::sleep(FOREGROUND_LOG_DRAIN_POLL).await;
    }
}

fn foreground_log_lengths(paths: &[(&std::path::Path, &AtomicU64)]) -> Vec<u64> {
    paths
        .iter()
        .map(|(path, _)| std::fs::metadata(path).map(|m| m.len()).unwrap_or(0))
        .collect()
}

pub(super) fn foreground_exit_code(
    reason: ForegroundStopReason,
    vm_exit_code: Option<i32>,
) -> Option<i32> {
    match reason {
        // A dead runtime without a persisted guest result is not evidence of a
        // successful command. This happens when the VM/shim fails before
        // guest-init can write `.a3s_exit_code`; returning `None` here used to
        // make foreground `run --rm` fall through to CLI exit status 0.
        ForegroundStopReason::ProcessExited => vm_exit_code.or(Some(1)),
        ForegroundStopReason::UserInterrupted(signal) => vm_exit_code.or(Some(128 + signal)),
        ForegroundStopReason::VmUnhealthy => vm_exit_code.or(Some(1)),
        ForegroundStopReason::TimedOut => Some(124),
    }
}

pub(super) fn foreground_health_stop_reason(exit_code: Option<i32>) -> ForegroundStopReason {
    if exit_code.is_some() {
        ForegroundStopReason::ProcessExited
    } else {
        ForegroundStopReason::VmUnhealthy
    }
}

pub(super) fn foreground_workload_exit_code(
    box_dir: &std::path::Path,
    recorded_exit_code: Option<i32>,
) -> Option<i32> {
    a3s_box_runtime::rootfs::resolve_workload_exit_code(box_dir, recorded_exit_code)
}

async fn managed_process_alive(ctx: &mut RunContext) -> bool {
    if run_context_uses_oci(ctx) {
        match ctx.manager.inspect(&ctx.execution_id).await {
            Ok(status)
                if matches!(
                    status.state,
                    ExecutionState::Running | ExecutionState::Paused
                ) =>
            {
                true
            }
            Ok(_) => {
                if let Ok(state) = StateFile::load_readonly() {
                    if let Some(record) = state.find_by_id(&ctx.box_id) {
                        ctx.record = record.clone();
                    }
                }
                false
            }
            Err(_) => false,
        }
    } else {
        ctx.record.pid.is_some_and(|pid| {
            a3s_box_runtime::is_process_alive_with_identity(pid, ctx.record.pid_start_time)
        })
    }
}

#[cfg(unix)]
async fn managed_runtime_healthy(ctx: &RunContext) -> bool {
    if run_context_uses_oci(ctx) {
        return true;
    }
    if !ctx.record.pid.is_some_and(|pid| {
        a3s_box_runtime::is_process_alive_with_identity(pid, ctx.record.pid_start_time)
    }) {
        return false;
    }
    let probe = async {
        let client = a3s_box_runtime::ExecClient::connect(&ctx.exec_socket_path)
            .await
            .ok()?;
        client.heartbeat().await.ok().filter(|ready| *ready)
    };
    tokio::time::timeout(std::time::Duration::from_millis(500), probe)
        .await
        .ok()
        .flatten()
        .is_some()
}

#[cfg(not(unix))]
async fn managed_runtime_healthy(ctx: &RunContext) -> bool {
    if run_context_uses_oci(ctx) {
        true
    } else {
        ctx.record.pid.is_some_and(|pid| {
            a3s_box_runtime::is_process_alive_with_identity(pid, ctx.record.pid_start_time)
        })
    }
}

pub(super) fn run_context_uses_oci(ctx: &RunContext) -> bool {
    ctx.record
        .managed_execution
        .as_ref()
        .is_some_and(a3s_box_runtime::ManagedExecutionMetadata::is_oci_routed)
}

pub(super) fn foreground_completion_message(
    reason: ForegroundStopReason,
    auto_remove: bool,
    name: &str,
) -> String {
    match (reason, auto_remove) {
        (ForegroundStopReason::ProcessExited, true) => {
            format!("Box {name} exited and was removed.")
        }
        (ForegroundStopReason::ProcessExited, false) => format!("Box {name} exited."),
        (ForegroundStopReason::UserInterrupted(_), true) => format!("Box {name} removed."),
        (ForegroundStopReason::UserInterrupted(_), false) => format!("Box {name} stopped."),
        (ForegroundStopReason::VmUnhealthy, true) => {
            format!("Box {name} stopped after VM health check failed and was removed.")
        }
        (ForegroundStopReason::VmUnhealthy, false) => {
            format!("Box {name} stopped after VM health check failed.")
        }
        (ForegroundStopReason::TimedOut, true) => {
            format!("Box {name} stopped after --timeout expired and was removed.")
        }
        (ForegroundStopReason::TimedOut, false) => {
            format!("Box {name} stopped after --timeout expired.")
        }
    }
}
