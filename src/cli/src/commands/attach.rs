//! `a3s-box attach` command — attach to a running box.
//!
//! Without `-it`, tails the console log (read-only, original behavior).
//! With `-it`, opens an interactive PTY session to a shell inside the box.

use clap::Args;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use crate::resolve;
use crate::state::{BoxRecord, StateFile};

const ATTACH_EXIT_POLL: std::time::Duration = std::time::Duration::from_millis(200);
const ATTACH_LOG_DRAIN_POLL: std::time::Duration = std::time::Duration::from_millis(20);
const ATTACH_LOG_DRAIN_QUIET: std::time::Duration = std::time::Duration::from_millis(100);
const ATTACH_LOG_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const ATTACH_TAIL_STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Args)]
pub struct AttachArgs {
    /// Box name or ID
    pub r#box: String,

    /// Keep STDIN open
    #[arg(short = 'i', long = "interactive")]
    pub interactive: bool,

    /// Allocate a pseudo-TTY
    #[arg(short = 't', long = "tty")]
    pub tty: bool,
}

pub async fn execute(args: AttachArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, &args.r#box)?.clone();
    crate::socket_paths::require_running(&record, "attach to")
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    // Interactive PTY mode
    if args.tty {
        #[cfg(not(windows))]
        return execute_pty_attach(&record).await;
        #[cfg(windows)]
        return Err(crate::platform::unsupported_command(
            "attach -it",
            "interactive PTY support",
        ));
    }

    let streams = attach_stream_sources(&record.box_dir, &record.console_log);
    if !streams.stdout.exists() {
        return Err(missing_console_log_message(&record.name, &streams.stdout).into());
    }

    println!("Attached to box {}. Press Ctrl-C to detach.", record.name);

    let runtime_filter = streams
        .filter_runtime_noise
        .then(|| std::sync::Arc::new(a3s_box_core::log::RuntimeConsoleFilter::new()));
    let stdout_runtime_filter = runtime_filter.clone();
    let stderr_runtime_filter = runtime_filter;
    let stdout_path = streams.stdout.clone();
    let stderr_path = streams.stderr.clone();
    let tail_stdout_path = stdout_path.clone();
    let tail_stderr_path = stderr_path.clone();
    let stdout_position = Arc::new(AtomicU64::new(0));
    let stderr_position = Arc::new(AtomicU64::new(0));
    let tail_stdout_position = Arc::clone(&stdout_position);
    let tail_stderr_position = Arc::clone(&stderr_position);
    let tail_stop = Arc::new(AtomicBool::new(false));
    let stdout_tail_stop = Arc::clone(&tail_stop);
    let stderr_tail_stop = Arc::clone(&tail_stop);
    let mut log_handle = tokio::spawn(async move {
        tokio::join!(
            super::tail_file_stream_positioned(
                &tail_stdout_path,
                false,
                Some(tail_stdout_position),
                Some(stdout_tail_stop),
                stdout_runtime_filter,
            ),
            super::tail_file_stream_positioned(
                &tail_stderr_path,
                true,
                Some(tail_stderr_position),
                Some(stderr_tail_stop),
                stderr_runtime_filter,
            ),
        );
    });

    let end_reason = tokio::select! {
        _ = tokio::signal::ctrl_c() => AttachEndReason::UserDetached,
        _ = wait_for_attached_box_exit(&record) => AttachEndReason::BoxExited,
    };

    if end_reason == AttachEndReason::BoxExited {
        let drained = wait_for_attach_log_drain(
            &[
                (&stdout_path, stdout_position.as_ref()),
                (&stderr_path, stderr_position.as_ref()),
            ],
            ATTACH_LOG_DRAIN_QUIET,
            ATTACH_LOG_DRAIN_POLL,
            ATTACH_LOG_DRAIN_TIMEOUT,
        )
        .await;
        if !drained {
            tracing::warn!(
                box_id = %record.id,
                "Timed out while draining final attach output"
            );
        }
    }

    tail_stop.store(true, Ordering::Release);
    if tokio::time::timeout(ATTACH_TAIL_STOP_TIMEOUT, &mut log_handle)
        .await
        .is_err()
    {
        log_handle.abort();
    }

    match end_reason {
        AttachEndReason::UserDetached => println!("\nDetached from box {}.", record.name),
        AttachEndReason::BoxExited => println!("\nBox {} exited.", record.name),
    }

    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttachEndReason {
    UserDetached,
    BoxExited,
}

async fn wait_for_attached_box_exit(record: &BoxRecord) {
    wait_for_attached_box_exit_with(ATTACH_EXIT_POLL, || attached_box_is_live(record)).await;
}

async fn wait_for_attached_box_exit_with(
    poll_interval: std::time::Duration,
    mut is_live: impl FnMut() -> bool,
) {
    let mut poll = tokio::time::interval(poll_interval);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        poll.tick().await;
        if !is_live() {
            return;
        }
    }
}

fn attached_box_is_live(original: &BoxRecord) -> bool {
    match StateFile::load_readonly() {
        Ok(state) => state
            .find_by_id(&original.id)
            .is_some_and(record_keeps_attach_open),
        // A transient state read failure must not detach from a live box. PID
        // identity on the original record still lets a dead runtime terminate
        // the attach without waiting for a persisted reconciliation pass.
        Err(_) => record_keeps_attach_open(original),
    }
}

fn record_keeps_attach_open(record: &BoxRecord) -> bool {
    matches!(record.status.as_str(), "running" | "paused")
        && crate::state::policy::is_record_pid_live(record)
}

async fn wait_for_attach_log_drain(
    paths: &[(&std::path::Path, &AtomicU64)],
    quiet_period: std::time::Duration,
    poll_interval: std::time::Duration,
    timeout: std::time::Duration,
) -> bool {
    let started = std::time::Instant::now();
    let mut last_lengths = attach_log_lengths(paths);
    let mut quiet_since = None;

    loop {
        let lengths = attach_log_lengths(paths);
        let tails_caught_up = paths
            .iter()
            .zip(lengths.iter())
            .all(|((_, position), length)| position.load(Ordering::Relaxed) >= *length);

        if tails_caught_up && lengths == last_lengths {
            let now = std::time::Instant::now();
            match quiet_since {
                Some(since) if now.duration_since(since) >= quiet_period => return true,
                Some(_) => {}
                None => quiet_since = Some(now),
            }
        } else {
            last_lengths = lengths;
            quiet_since = None;
        }

        if started.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(poll_interval).await;
    }
}

fn attach_log_lengths(paths: &[(&std::path::Path, &AtomicU64)]) -> Vec<u64> {
    paths
        .iter()
        .map(|(path, _)| {
            std::fs::metadata(path)
                .map(|metadata| metadata.len())
                .unwrap_or(0)
        })
        .collect()
}

struct AttachStreamSources {
    stdout: std::path::PathBuf,
    stderr: std::path::PathBuf,
    filter_runtime_noise: bool,
}

fn attach_stream_sources(
    box_dir: &std::path::Path,
    console_log: &std::path::Path,
) -> AttachStreamSources {
    if cfg!(target_os = "windows") {
        // WHPX exposes the supervised workload streams through the shared
        // rootfs while the VM is running. The conventional console files only
        // receive a fallback copy after exit, so tailing them makes a live
        // read-only attach silently miss workload output.
        let rootfs = box_dir.join("rootfs");
        AttachStreamSources {
            stdout: rootfs.join("guest-init.stdout.log"),
            stderr: rootfs.join("guest-init.stderr.log"),
            filter_runtime_noise: true,
        }
    } else {
        AttachStreamSources {
            stdout: console_log.to_path_buf(),
            stderr: console_log.with_file_name("console.err.log"),
            filter_runtime_noise: false,
        }
    }
}

fn missing_console_log_message(name: &str, console_log: &std::path::Path) -> String {
    format!(
        "Console log is missing for running box {} at {}. The box may still be starting or the state may be stale; try `a3s-box logs -f {}` or `a3s-box ps`.",
        name,
        console_log.display(),
        name
    )
}

/// Attach to a running box with an interactive PTY session.
#[cfg(not(windows))]
async fn execute_pty_attach(
    record: &crate::state::BoxRecord,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::terminal;
    use a3s_box_core::pty::PtyRequest;

    let pty_socket_path = crate::socket_paths::require_runtime_socket(
        record,
        crate::socket_paths::RuntimeSocket::Pty,
    )
    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let (cols, rows) = terminal::size().unwrap_or((80, 24));

    let mut client =
        super::exec::connect_pty_with_retry(&pty_socket_path, std::time::Duration::from_secs(10))
            .await?;

    // Attach opens a shell
    let request = PtyRequest {
        cmd: vec!["/bin/sh".to_string()],
        env: vec![],
        working_dir: None,
        rootfs: None,
        user: None,
        cols,
        rows,
    };
    client.send_request(&request).await?;

    let (read_half, write_half) = client.into_split();
    let exit_code = {
        let _raw_mode = terminal::raw_mode()?;
        super::exec::run_pty_session(read_half, write_half).await
    };

    if exit_code != 0 {
        std::process::exit(exit_code);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn missing_console_log_message_mentions_recovery_commands() {
        let message = missing_console_log_message("web", Path::new("/tmp/a3s/web/console.log"));

        assert!(message.contains("running box web"));
        assert!(message.contains("/tmp/a3s/web/console.log"));
        assert!(message.contains("a3s-box logs -f web"));
        assert!(message.contains("a3s-box ps"));
    }

    #[test]
    fn attach_stream_sources_match_platform_runtime_output() {
        let box_dir = Path::new("box-dir");
        let console_log = box_dir.join("logs").join("console.log");
        let streams = attach_stream_sources(box_dir, &console_log);

        if cfg!(target_os = "windows") {
            assert_eq!(
                streams.stdout,
                box_dir.join("rootfs").join("guest-init.stdout.log")
            );
            assert_eq!(
                streams.stderr,
                box_dir.join("rootfs").join("guest-init.stderr.log")
            );
            assert!(streams.filter_runtime_noise);
        } else {
            assert_eq!(streams.stdout, console_log);
            assert_eq!(streams.stderr, box_dir.join("logs").join("console.err.log"));
            assert!(!streams.filter_runtime_noise);
        }
    }

    #[test]
    fn attach_liveness_requires_an_active_status_and_live_pid_identity() {
        let running = crate::test_helpers::fixtures::make_record(
            "id",
            "box",
            "running",
            Some(std::process::id()),
        );
        assert!(record_keeps_attach_open(&running));

        let missing_pid = crate::test_helpers::fixtures::make_record("id", "box", "running", None);
        assert!(!record_keeps_attach_open(&missing_pid));

        let dead = crate::test_helpers::fixtures::make_record(
            "id",
            "box",
            "dead",
            Some(std::process::id()),
        );
        assert!(!record_keeps_attach_open(&dead));
    }

    #[tokio::test]
    async fn attach_exit_waiter_returns_when_liveness_is_lost() {
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_for_attached_box_exit_with(std::time::Duration::from_millis(1), || false),
        )
        .await
        .expect("lost process liveness must end a read-only attach");
    }

    #[tokio::test]
    async fn attach_log_drain_waits_for_both_streams_at_eof() {
        let directory = tempfile::tempdir().unwrap();
        let stdout = directory.path().join("stdout.log");
        let stderr = directory.path().join("stderr.log");
        std::fs::write(&stdout, b"out").unwrap();
        std::fs::write(&stderr, b"final error").unwrap();

        let stdout_position = Arc::new(AtomicU64::new(3));
        let stderr_position = Arc::new(AtomicU64::new(0));
        let waiter_stdout_position = Arc::clone(&stdout_position);
        let waiter_stderr_position = Arc::clone(&stderr_position);
        let mut waiter = tokio::spawn(async move {
            wait_for_attach_log_drain(
                &[
                    (&stdout, waiter_stdout_position.as_ref()),
                    (&stderr, waiter_stderr_position.as_ref()),
                ],
                std::time::Duration::ZERO,
                std::time::Duration::from_millis(1),
                std::time::Duration::from_secs(1),
            )
            .await
        });

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), &mut waiter)
                .await
                .is_err(),
            "attach must not return while either stream still has unread bytes"
        );
        stderr_position.store(11, Ordering::Relaxed);

        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
                .await
                .expect("caught-up streams should finish draining")
                .unwrap()
        );
    }

    #[tokio::test]
    async fn attach_log_drain_finishes_for_empty_streams() {
        let directory = tempfile::tempdir().unwrap();
        let stdout = directory.path().join("stdout.log");
        let stderr = directory.path().join("stderr.log");
        std::fs::write(&stdout, b"").unwrap();
        std::fs::write(&stderr, b"").unwrap();
        let stdout_position = AtomicU64::new(0);
        let stderr_position = AtomicU64::new(0);

        let drained = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_for_attach_log_drain(
                &[(&stdout, &stdout_position), (&stderr, &stderr_position)],
                std::time::Duration::ZERO,
                std::time::Duration::from_millis(1),
                std::time::Duration::from_secs(1),
            ),
        )
        .await
        .expect("an exited box with no output must not leave attach hanging");

        assert!(drained);
    }
}
