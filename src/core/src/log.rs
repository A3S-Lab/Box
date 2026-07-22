//! Logging driver types and configuration.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Logging driver type.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LogDriver {
    /// Docker-compatible JSON lines format (default).
    #[default]
    JsonFile,
    /// Forward logs to a syslog endpoint.
    ///
    /// Options:
    /// - `syslog-address`: UDP/TCP address (e.g., "udp://localhost:514")
    /// - `syslog-facility`: Syslog facility (default: "daemon")
    /// - `tag`: Log tag template (default: box name)
    Syslog,
    /// Disable logging entirely.
    None,
}

impl std::fmt::Display for LogDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::JsonFile => write!(f, "json-file"),
            Self::Syslog => write!(f, "syslog"),
            Self::None => write!(f, "none"),
        }
    }
}

impl std::str::FromStr for LogDriver {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "json-file" => Ok(Self::JsonFile),
            "syslog" => Ok(Self::Syslog),
            "none" => Ok(Self::None),
            _ => Err(format!(
                "unknown log driver: '{}' (supported: json-file, syslog, none)",
                s
            )),
        }
    }
}

/// Logging configuration for a box.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogConfig {
    pub driver: LogDriver,
    #[serde(default)]
    pub options: HashMap<String, String>,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            driver: LogDriver::JsonFile,
            options: HashMap::new(),
        }
    }
}

impl LogConfig {
    /// Maximum log file size in bytes before rotation.
    /// Default: 10 MiB. Set via `max-size` option (e.g., "10m", "1g").
    pub fn max_size(&self) -> u64 {
        self.options
            .get("max-size")
            .and_then(|s| parse_size(s).ok())
            .unwrap_or(10 * 1024 * 1024)
    }

    /// Maximum number of rotated log files to keep.
    /// Default: 3. Set via `max-file` option.
    pub fn max_file(&self) -> u32 {
        self.options
            .get("max-file")
            .and_then(|s| s.parse().ok())
            .unwrap_or(3)
    }

    /// Syslog address (e.g., "udp://localhost:514").
    /// Only relevant when driver is `Syslog`.
    pub fn syslog_address(&self) -> &str {
        self.options
            .get("syslog-address")
            .map(|s| s.as_str())
            .unwrap_or("udp://localhost:514")
    }

    /// Syslog facility (e.g., "daemon", "local0").
    /// Only relevant when driver is `Syslog`.
    pub fn syslog_facility(&self) -> &str {
        self.options
            .get("syslog-facility")
            .map(|s| s.as_str())
            .unwrap_or("daemon")
    }

    /// Log tag (used by syslog driver as the program name).
    pub fn tag(&self) -> Option<&str> {
        self.options.get("tag").map(|s| s.as_str())
    }
}

/// A single structured log entry (Docker-compatible JSON format).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// The log message (including trailing newline).
    pub log: String,
    /// The output stream: "stdout" or "stderr".
    pub stream: String,
    /// RFC 3339 timestamp with nanosecond precision.
    pub time: String,
}

/// Schema used to hand one Sandbox log worker its immutable generation data.
pub const SANDBOX_LOG_WORKER_SCHEMA: &str = "a3s.box.sandbox-log-worker.v1";

/// Configuration for the host process that projects Sandbox stdout/stderr into
/// the configured logging driver after the launching client has detached.
///
/// The worker watches the exact `crun run` wrapper PID identity. Once that
/// process exits, both inherited output descriptors are closed and EOF is
/// authoritative, so the worker can drain the final bytes without a fixed
/// late-write delay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxLogWorkerSpec {
    pub schema: String,
    pub box_id: String,
    pub console_log: PathBuf,
    pub log_config: LogConfig,
    pub watched_pid: u32,
    pub watched_pid_start_time: u64,
    pub ready_file: PathBuf,
}

/// Parse a human-readable size string (e.g., "10m", "1g", "4096") into bytes.
fn parse_size(s: &str) -> std::result::Result<u64, String> {
    let s = s.trim().to_lowercase();
    if let Ok(n) = s.parse::<u64>() {
        return Ok(n);
    }
    let (num, mult) = if s.ends_with("gb") || s.ends_with('g') {
        (
            s.trim_end_matches("gb").trim_end_matches('g'),
            1024u64 * 1024 * 1024,
        )
    } else if s.ends_with("mb") || s.ends_with('m') {
        (
            s.trim_end_matches("mb").trim_end_matches('m'),
            1024u64 * 1024,
        )
    } else if s.ends_with("kb") || s.ends_with('k') {
        (s.trim_end_matches("kb").trim_end_matches('k'), 1024u64)
    } else if s.ends_with('b') {
        (s.trim_end_matches('b'), 1u64)
    } else {
        return Err(format!("unrecognized size format: {s}"));
    };
    let n: u64 = num.parse().map_err(|_| format!("invalid number: {num}"))?;
    Ok(n * mult)
}

// ===========================================================================
// Log processor — tails the VM console (`console.log`) and produces structured
// Docker-compatible output (`container.json`) or forwards to syslog.
//
// This runs in the SHIM (the box's own per-process lifetime), not the ephemeral
// CLI: the CLI exits on `run -d` detach, which would kill an in-CLI processor
// and truncate the logs. The shim writes `console.log` and lives exactly as
// long as the VM, so it is the correct, daemonless home (like containerd-shim).
// ===========================================================================

use std::io::{BufRead, BufReader, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "windows")]
type ConsoleFileIdentity = crate::windows_file::WindowsFileIdentity;
#[cfg(not(target_os = "windows"))]
type ConsoleFileIdentity = ();

/// Whether the producer may still publish bytes after its apparent exit.
///
/// libkrun can return before its console backend's final host write becomes
/// visible, whereas a reaped `crun run` wrapper has already closed stdout and
/// stderr. Keeping the distinction explicit avoids imposing the MicroVM's
/// half-second settle window on every short Sandbox execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleEofPolicy {
    MayReceiveLateWrites,
    WriterClosed,
}

/// Truncate `path` to empty if it has grown past `cap` bytes; returns whether it
/// truncated.
///
/// libkrun appends the guest console to the raw `console.log`/`console.err.log`
/// for the VM's entire lifetime, so a chatty long-running box grows them without
/// limit — only the rotated `container.json` was ever bounded. The tail loop
/// calls this at a clean line boundary (every line so far is already durable in
/// `container.json`), so truncation never drops queryable log data. libkrun
/// holds the file `O_APPEND`, so its next write resumes at offset 0 — no hole.
fn console_truncate_if_over(
    path: &Path,
    cap: u64,
    expected_identity: Option<ConsoleFileIdentity>,
) -> bool {
    #[cfg(not(target_os = "windows"))]
    let _ = expected_identity;

    #[cfg(target_os = "windows")]
    let file = crate::windows_file::open_regular_file_for_write(path, expected_identity)
        .map(|(file, _)| file);
    #[cfg(not(target_os = "windows"))]
    let file = std::fs::OpenOptions::new().write(true).open(path);

    let Ok(file) = file else {
        return false;
    };
    if file
        .metadata()
        .map_or(true, |metadata| metadata.len() <= cap)
    {
        return false;
    }
    if file.set_len(0).is_ok() {
        tracing::debug!(path = %path.display(), cap, "console log exceeded cap; truncated");
        true
    } else {
        false
    }
}

/// Path to the structured JSON log file inside a box's log dir.
pub fn json_log_path(log_dir: &Path) -> PathBuf {
    log_dir.join("container.json")
}

/// The phase-aware filter for libkrun's C-init console preamble.
///
/// C-init emits a small, fixed set of diagnostics before calling `execvp`.
/// stdout and stderr must share one instance: the `execvp(...) starting` line
/// can arrive on either stream and permanently ends filtering for both. Once
/// that sentinel has been observed, every subsequent line is workload output,
/// even if it has the same text as a preamble line.
#[derive(Debug)]
pub struct RuntimeConsoleFilter {
    preamble_active: AtomicBool,
}

impl RuntimeConsoleFilter {
    pub fn new() -> Self {
        Self {
            preamble_active: AtomicBool::new(true),
        }
    }

    /// Return whether `line` should be exposed as workload output.
    ///
    /// This method expects a complete logical line. Byte-stream callers must
    /// retain an unterminated final fragment rather than classify it.
    pub fn keep_line(&self, line: &str) -> bool {
        if !self.preamble_active.load(Ordering::Acquire) {
            return true;
        }

        match classify_runtime_console_line(line) {
            RuntimeConsoleLineKind::Workload => true,
            RuntimeConsoleLineKind::Preamble => {
                // A sentinel on the companion stream may have ended the phase
                // after our first load. Recheck so completed sentinel calls
                // globally disable filtering.
                !self.preamble_active.load(Ordering::Acquire)
            }
            RuntimeConsoleLineKind::EndPreamble => {
                // Exactly one concurrent sentinel ends the phase and is
                // hidden. A sentinel-shaped workload line after that is kept.
                !self.preamble_active.swap(false, Ordering::AcqRel)
            }
        }
    }

    pub fn preamble_active(&self) -> bool {
        self.preamble_active.load(Ordering::Acquire)
    }
}

impl Default for RuntimeConsoleFilter {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeConsoleLineKind {
    Workload,
    Preamble,
    EndPreamble,
}

fn classify_runtime_console_line(line: &str) -> RuntimeConsoleLineKind {
    let line = line.trim_end_matches(['\n', '\r']);

    if matches!(
        line,
        "init.krun: mount_filesystems ok"
            | "init.krun: root propagation ok"
            | "init.krun: tty/session configured"
            | "init.krun: config parsed"
            | "init.krun: setup_redirects ok"
    ) {
        return RuntimeConsoleLineKind::Preamble;
    }

    if line
        .strip_prefix("init.krun: entered main argc=")
        .is_some_and(is_ascii_decimal)
    {
        return RuntimeConsoleLineKind::Preamble;
    }

    if let Some(fields) = line.strip_prefix("init.krun: after cmdline env import KRUN_INIT=") {
        if let Some((krun_init, fields)) = fields.split_once(" KRUN_INIT_PID1=") {
            if let Some((krun_init_pid1, box_exec_exec)) = fields.split_once(" BOX_EXEC_EXEC=") {
                if [krun_init, krun_init_pid1, box_exec_exec]
                    .iter()
                    .all(|value| !value.is_empty())
                {
                    return RuntimeConsoleLineKind::Preamble;
                }
            }
        }
    }

    if let Some(selected) = line.strip_prefix("init.krun: selected exec=") {
        if let Some((executable, init_pid1)) = selected.rsplit_once(" init_pid1=") {
            if !executable.is_empty() && matches!(init_pid1, "0" | "1") {
                return RuntimeConsoleLineKind::Preamble;
            }
        }
    }

    if line
        .strip_prefix("init.krun: execvp(")
        .and_then(|rest| rest.strip_suffix(") starting"))
        .is_some_and(|executable| !executable.is_empty())
    {
        return RuntimeConsoleLineKind::EndPreamble;
    }

    RuntimeConsoleLineKind::Workload
}

fn is_ascii_decimal(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// True only for a line matching the known C-init preamble grammar.
///
/// This compatibility helper is phase-unaware. Consumers processing a stream
/// should instead share one [`RuntimeConsoleFilter`] across stdout and stderr.
pub fn is_runtime_console_noise(line: &str) -> bool {
    classify_runtime_console_line(line) != RuntimeConsoleLineKind::Workload
}

/// Read the next COMPLETE line from a tailed `console.log`, returning it without
/// the trailing newline. Polls on EOF like `tail -f` (so lines a container logs
/// after a quiet period are not dropped), accumulating a partial line across
/// reads. Returns `None` only when `stop` is set AND EOF is reached — i.e. the
/// VM has exited and `console.log` is fully drained — flushing any final partial
/// line as the last value before the subsequent `None`.
fn tail_next_line_with_completeness<R: BufRead + Seek>(
    reader: &mut R,
    buf: &mut String,
    stop: &AtomicBool,
    on_eof: Option<&dyn Fn() -> bool>,
    eof_policy: ConsoleEofPolicy,
    reopen_at_eof: Option<&dyn Fn(u64) -> Option<(R, u64)>>,
) -> Option<(String, bool)> {
    // `krun_start_enter()` can return a few scheduler ticks before the
    // virtio-console backend's final host write becomes visible. Treat the
    // first stopped EOFs as provisional; otherwise a very short detached
    // command can leave bytes in console.log after the processor has exited.
    const STOPPED_EOF_SETTLE_MILLIS: u64 = 20;
    let stopped_eof_settle_polls = stopped_eof_settle_polls(eof_policy);
    let mut stopped_eof_polls = 0u8;
    let mut refreshed_after_stop = false;
    loop {
        match reader.read_line(buf) {
            Ok(0) | Err(_) => {
                // Caught up at a clean line boundary (no partial line buffered):
                // let the caller bound the file's growth. If it truncated, seek
                // back to the start so reads don't sit forever past a stale EOF.
                let mut position = reader.stream_position().ok();
                if buf.is_empty() {
                    if let Some(on_eof) = on_eof {
                        if on_eof() {
                            let _ = reader.seek(std::io::SeekFrom::Start(0));
                            position = Some(0);
                        }
                    }
                }

                let stopping = stop.load(Ordering::Relaxed);

                // Windows shared-filesystem producers can replace the path or
                // append through a handle whose updates remain invisible to a
                // reader already parked at EOF. Path metadata may be cached as
                // well, so reopen unconditionally after each polling interval.
                // On shutdown, perform a fresh-handle read before each
                // late-write settle poll before declaring the source drained.
                if let (Some(position), Some(reopen_at_eof)) = (position, reopen_at_eof) {
                    if !(stopping && refreshed_after_stop) {
                        if !stopping {
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                        if let Some((mut replacement, replacement_position)) =
                            reopen_at_eof(position)
                        {
                            if replacement_position != position {
                                buf.clear();
                            }
                            std::mem::swap(reader, &mut replacement);
                            refreshed_after_stop = stopping;
                            continue;
                        }
                    }
                }

                if stopping {
                    stopped_eof_polls = stopped_eof_polls.saturating_add(1);
                    if stopped_eof_polls < stopped_eof_settle_polls {
                        // A later poll must use another fresh Windows handle;
                        // the producer may have replaced or appended the path.
                        refreshed_after_stop = false;
                        std::thread::sleep(std::time::Duration::from_millis(
                            STOPPED_EOF_SETTLE_MILLIS,
                        ));
                        continue;
                    }
                    // The producer has stopped and the current path is drained:
                    // flush a trailing partial line once, then finish.
                    if buf.is_empty() {
                        return None;
                    }
                    let line = std::mem::take(buf);
                    return Some((line.trim_end_matches(['\n', '\r']).to_string(), false));
                }
                if reopen_at_eof.is_none() {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                continue;
            }
            Ok(_) => {
                stopped_eof_polls = 0;
                refreshed_after_stop = false;
            }
        }
        if !buf.ends_with('\n') {
            // Partial line at EOF — keep it buffered and wait for the rest.
            continue;
        }
        let line = std::mem::take(buf);
        return Some((line.trim_end_matches(['\n', '\r']).to_string(), true));
    }
}

#[cfg(test)]
fn tail_next_line<R: BufRead + Seek>(
    reader: &mut R,
    buf: &mut String,
    stop: &AtomicBool,
    on_eof: Option<&dyn Fn() -> bool>,
    eof_policy: ConsoleEofPolicy,
    reopen_at_eof: Option<&dyn Fn(u64) -> Option<(R, u64)>>,
) -> Option<String> {
    tail_next_line_with_completeness(reader, buf, stop, on_eof, eof_policy, reopen_at_eof)
        .map(|(line, _complete)| line)
}

fn stopped_eof_settle_polls(eof_policy: ConsoleEofPolicy) -> u8 {
    const LATE_WRITE_POLLS: u8 = 25;
    match eof_policy {
        ConsoleEofPolicy::MayReceiveLateWrites => LATE_WRITE_POLLS,
        ConsoleEofPolicy::WriterClosed => 1,
    }
}

/// Run the log processor for a box, blocking until `stop` is set and the console
/// is drained. Intended to run on a dedicated thread for the VM's lifetime; set
/// `stop` after the VM exits, then join, to guarantee the final lines are
/// captured (no teardown race).
pub fn run_log_processor(
    console_log: &Path,
    log_dir: &Path,
    config: &LogConfig,
    stop: &AtomicBool,
) {
    run_log_processor_with_ready(console_log, log_dir, config, stop, None);
}

/// Run the processor and optionally count each console reader once it has
/// opened its file. A VM launcher can wait for two ready readers before start,
/// preventing a short guest from exiting before the tail threads are alive.
pub fn run_log_processor_with_ready(
    console_log: &Path,
    log_dir: &Path,
    config: &LogConfig,
    stop: &AtomicBool,
    ready: Option<&std::sync::atomic::AtomicUsize>,
) {
    run_log_processor_with_ready_and_eof_policy(
        console_log,
        log_dir,
        config,
        stop,
        ready,
        ConsoleEofPolicy::MayReceiveLateWrites,
    );
}

/// Run the log processor with an explicit final-EOF policy.
///
/// Sandbox workers use [`ConsoleEofPolicy::WriterClosed`] only after the exact
/// `crun run` wrapper has exited. Other callers should retain the conservative
/// default exposed by [`run_log_processor_with_ready`].
pub fn run_log_processor_with_ready_and_eof_policy(
    console_log: &Path,
    log_dir: &Path,
    config: &LogConfig,
    stop: &AtomicBool,
    ready: Option<&std::sync::atomic::AtomicUsize>,
    eof_policy: ConsoleEofPolicy,
) {
    let stderr_log = stderr_console_path(console_log);
    run_log_processor_streams_with_ready_and_eof_policy(
        console_log,
        &stderr_log,
        log_dir,
        config,
        stop,
        ready,
        eof_policy,
    );
}

/// Run the log processor against explicitly selected stdout and stderr files.
///
/// Most VMM backends write the conventional `console.log` and
/// `console.err.log` pair, which [`run_log_processor`] discovers automatically.
/// Backends that persist completed guest streams elsewhere can use this entry
/// point to process exactly those files without replaying an older raw console.
pub fn run_log_processor_streams(
    stdout_log: &Path,
    stderr_log: &Path,
    log_dir: &Path,
    config: &LogConfig,
    stop: &AtomicBool,
) {
    run_log_processor_streams_with_ready_and_eof_policy(
        stdout_log,
        stderr_log,
        log_dir,
        config,
        stop,
        None,
        ConsoleEofPolicy::WriterClosed,
    );
}

/// Run the log processor against live, explicitly selected stdout and stderr
/// files, optionally counting each reader after its source has been opened.
///
/// This is the explicit-stream counterpart to [`run_log_processor_with_ready`]
/// and retains the conservative late-write settle policy for live producers.
pub fn run_log_processor_streams_with_ready(
    stdout_log: &Path,
    stderr_log: &Path,
    log_dir: &Path,
    config: &LogConfig,
    stop: &AtomicBool,
    ready: Option<&std::sync::atomic::AtomicUsize>,
) {
    run_log_processor_streams_with_ready_and_eof_policy(
        stdout_log,
        stderr_log,
        log_dir,
        config,
        stop,
        ready,
        ConsoleEofPolicy::MayReceiveLateWrites,
    );
}

fn run_log_processor_streams_with_ready_and_eof_policy(
    stdout_log: &Path,
    stderr_log: &Path,
    log_dir: &Path,
    config: &LogConfig,
    stop: &AtomicBool,
    ready: Option<&std::sync::atomic::AtomicUsize>,
    eof_policy: ConsoleEofPolicy,
) {
    match config.driver {
        // `none` produces no structured output, but libkrun still writes the raw
        // console for the VM's lifetime — drain + bound it so a chatty box with
        // logging disabled doesn't fill the disk (same hazard as the other
        // drivers).
        LogDriver::None => run_discard_processor(
            stdout_log,
            stderr_log,
            Some(console_cap(config.max_size(), config.max_file())),
            stop,
            ready,
            eof_policy,
        ),
        LogDriver::JsonFile => run_json_file_processor(
            stdout_log, stderr_log, log_dir, config, stop, ready, eof_policy,
        ),
        LogDriver::Syslog => {
            run_syslog_processor(stdout_log, stderr_log, config, stop, ready, eof_policy)
        }
    }
}

/// Wait (bounded) for `console.log` to appear, then open it. Returns `None` if it
/// never shows up or `stop` fires first.
fn open_console(
    console_log: &Path,
    stop: &AtomicBool,
) -> Option<(std::fs::File, ConsoleFileIdentity)> {
    for _ in 0..300 {
        #[cfg(target_os = "windows")]
        match crate::windows_file::open_regular_file(console_log, None) {
            Ok(opened) => return Some(opened),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(path = %console_log.display(), %error, "Refusing unsafe Windows console source");
                return None;
            }
        }
        #[cfg(not(target_os = "windows"))]
        match std::fs::File::open(console_log) {
            Ok(file) => return Some((file, ())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
        if stop.load(Ordering::Relaxed) && !console_log.exists() {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    None
}

#[cfg(target_os = "windows")]
fn reopen_console(
    console_log: &Path,
    position: u64,
    expected_identity: ConsoleFileIdentity,
) -> Option<(BufReader<std::fs::File>, u64)> {
    let (mut file, _) =
        crate::windows_file::open_regular_file(console_log, Some(expected_identity)).ok()?;
    let visible_len = file.seek(std::io::SeekFrom::End(0)).ok()?;
    let replacement_position = if visible_len < position { 0 } else { position };
    file.seek(std::io::SeekFrom::Start(replacement_position))
        .ok()?;
    Some((BufReader::new(file), replacement_position))
}

/// Tail console.log and write one Docker-style JSON record per container line.
/// The stderr companion to `console.log` (libkrun's 3-fd console sends guest
/// stderr here, stdout to `console.log`).
pub fn stderr_console_path(console_log: &Path) -> PathBuf {
    console_log.with_file_name("console.err.log")
}

/// Tail one console file, emitting each container line via `emit(line, stream)`.
/// `runtime_filter` drops the strict libkrun C-init preamble. Both stream
/// tailers share the same filter. Blocks until `stop` is set and the file is
/// drained.
#[derive(Clone, Copy)]
struct TaggedTailOptions<'a> {
    stream: &'static str,
    runtime_filter: Option<&'a RuntimeConsoleFilter>,
    bound: Option<u64>,
    ready: Option<&'a std::sync::atomic::AtomicUsize>,
    eof_policy: ConsoleEofPolicy,
}

fn run_tagged_tail(
    file: &Path,
    stop: &AtomicBool,
    emit: &(dyn Fn(&str, &str) + Sync),
    options: TaggedTailOptions<'_>,
) {
    let (f, identity) = match open_console(file, stop) {
        Some(opened) => opened,
        None => return,
    };
    if let Some(ready) = options.ready {
        ready.fetch_add(1, Ordering::Release);
    }
    let mut reader = BufReader::new(f);
    let mut buf = String::new();
    // Bound the raw console file's growth at clean line boundaries (see
    // console_truncate_if_over). None = unbounded (used by tests).
    let truncate = options
        .bound
        .map(|cap| move || console_truncate_if_over(file, cap, Some(identity)));
    let on_eof: Option<&dyn Fn() -> bool> = truncate.as_ref().map(|t| t as &dyn Fn() -> bool);
    #[cfg(target_os = "windows")]
    let reopen = |position| reopen_console(file, position, identity);
    #[cfg(target_os = "windows")]
    let reopen_at_eof = Some(&reopen as &dyn Fn(u64) -> Option<(BufReader<std::fs::File>, u64)>);
    #[cfg(not(target_os = "windows"))]
    let reopen_at_eof = None;

    while let Some((line, complete)) = tail_next_line_with_completeness(
        &mut reader,
        &mut buf,
        stop,
        on_eof,
        options.eof_policy,
        reopen_at_eof,
    ) {
        if complete
            && options
                .runtime_filter
                .is_some_and(|filter| !filter.keep_line(&line))
        {
            continue;
        }
        emit(&line, options.stream);
    }
}

/// The raw `console.log`/`console.err.log` byte budget before the tail loop
/// truncates it. Tied to the rotated `container.json` budget (`max_size *
/// max_file`) so the raw console never outgrows the queryable log it feeds.
fn console_cap(max_size: u64, max_file: u32) -> u64 {
    max_size.saturating_mul(u64::from(max_file.max(1)))
}

/// Drain and bound the console for the `none` driver: tail both console files
/// (advancing to clean line boundaries) and truncate when over `cap`, emitting
/// nothing. Without this, `--log-driver none` would leave libkrun's raw
/// `console.log`/`console.err.log` to grow without limit.
fn run_discard_processor(
    console_log: &Path,
    err_log: &Path,
    cap: Option<u64>,
    stop: &AtomicBool,
    ready: Option<&std::sync::atomic::AtomicUsize>,
    eof_policy: ConsoleEofPolicy,
) {
    let discard = |_line: &str, _stream: &str| {};
    let discard: &(dyn Fn(&str, &str) + Sync) = &discard;
    std::thread::scope(|s| {
        s.spawn(|| {
            run_tagged_tail(
                console_log,
                stop,
                discard,
                TaggedTailOptions {
                    stream: "stdout",
                    runtime_filter: None,
                    bound: cap,
                    ready,
                    eof_policy,
                },
            )
        });
        s.spawn(|| {
            run_tagged_tail(
                err_log,
                stop,
                discard,
                TaggedTailOptions {
                    stream: "stderr",
                    runtime_filter: None,
                    bound: cap,
                    ready,
                    eof_policy,
                },
            )
        });
    });
}

fn run_json_file_processor(
    console_log: &Path,
    err_log: &Path,
    log_dir: &Path,
    config: &LogConfig,
    stop: &AtomicBool,
    ready: Option<&std::sync::atomic::AtomicUsize>,
    eof_policy: ConsoleEofPolicy,
) {
    let max_size = config.max_size();
    let max_file = config.max_file();
    let json_path = json_log_path(log_dir);
    let writer = std::sync::Mutex::new(
        match OrderedJsonWriter::new(&json_path, max_size, max_file) {
            Ok(writer) => writer,
            Err(_) => return,
        },
    );
    // Write one tagged JSON record per line; shared by the stdout and stderr
    // tail threads. Timestamp assignment is inside the same critical section
    // as the append, so file order cannot invert timestamps across streams.
    let emit = |line: &str, stream: &str| {
        if let Ok(mut writer) = writer.lock() {
            writer.write_entry(line, stream, chrono::Utc::now());
        }
    };
    let emit: &(dyn Fn(&str, &str) + Sync) = &emit;

    let cap = Some(console_cap(max_size, max_file));
    let runtime_filter = RuntimeConsoleFilter::new();
    std::thread::scope(|s| {
        s.spawn(|| {
            run_tagged_tail(
                console_log,
                stop,
                emit,
                TaggedTailOptions {
                    stream: "stdout",
                    runtime_filter: Some(&runtime_filter),
                    bound: cap,
                    ready,
                    eof_policy,
                },
            )
        });
        // libkrun's `init.krun:` preamble can land on EITHER stream, so filter
        // the noise on stderr too.
        s.spawn(|| {
            run_tagged_tail(
                err_log,
                stop,
                emit,
                TaggedTailOptions {
                    stream: "stderr",
                    runtime_filter: Some(&runtime_filter),
                    bound: cap,
                    ready,
                    eof_policy,
                },
            )
        });
    });
}

struct OrderedJsonWriter {
    output: RotatingWriter,
    last_timestamp: Option<chrono::DateTime<chrono::Utc>>,
}

impl OrderedJsonWriter {
    fn new(path: &Path, max_size: u64, max_file: u32) -> std::io::Result<Self> {
        Ok(Self {
            output: RotatingWriter::new(path, max_size, max_file)?,
            last_timestamp: None,
        })
    }

    fn write_entry(&mut self, line: &str, stream: &str, timestamp: chrono::DateTime<chrono::Utc>) {
        let timestamp = match &self.last_timestamp {
            Some(previous) if previous > &timestamp => previous.to_owned(),
            _ => timestamp,
        };
        let entry = LogEntry {
            log: format!("{line}\n"),
            stream: stream.to_string(),
            time: timestamp.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        };
        self.last_timestamp = Some(timestamp);
        if let Ok(json) = serde_json::to_string(&entry) {
            let _ = self.output.write_line(&json);
        }
    }
}

/// Forward both console streams (stdout + stderr) to a syslog endpoint.
fn run_syslog_processor(
    console_log: &Path,
    err_log: &Path,
    config: &LogConfig,
    stop: &AtomicBool,
    ready: Option<&std::sync::atomic::AtomicUsize>,
    eof_policy: ConsoleEofPolicy,
) {
    use std::net::UdpSocket;

    let address = config.syslog_address();
    let _facility = config.syslog_facility();
    let tag = config.tag().unwrap_or("a3s-box");
    let cap = Some(console_cap(config.max_size(), config.max_file()));
    let runtime_filter = RuntimeConsoleFilter::new();
    let (proto, addr) = if let Some(rest) = address.strip_prefix("udp://") {
        ("udp", rest)
    } else if let Some(rest) = address.strip_prefix("tcp://") {
        ("tcp", rest)
    } else {
        ("udp", address)
    };
    match proto {
        "udp" => {
            let socket = match UdpSocket::bind("0.0.0.0:0") {
                Ok(s) => s,
                Err(_) => return,
            };
            // RFC 3164: <priority>tag: message; daemon(3)*8 + info(6) = 30.
            let emit = |line: &str, _stream: &str| {
                let msg = format!("<30>{tag}: {line}");
                let _ = socket.send_to(msg.as_bytes(), addr);
            };
            let emit: &(dyn Fn(&str, &str) + Sync) = &emit;
            std::thread::scope(|s| {
                s.spawn(|| {
                    run_tagged_tail(
                        console_log,
                        stop,
                        emit,
                        TaggedTailOptions {
                            stream: "stdout",
                            runtime_filter: Some(&runtime_filter),
                            bound: cap,
                            ready,
                            eof_policy,
                        },
                    )
                });
                s.spawn(|| {
                    run_tagged_tail(
                        err_log,
                        stop,
                        emit,
                        TaggedTailOptions {
                            stream: "stderr",
                            runtime_filter: Some(&runtime_filter),
                            bound: cap,
                            ready,
                            eof_policy,
                        },
                    )
                });
            });
        }
        "tcp" => {
            let stream = match std::net::TcpStream::connect(addr) {
                Ok(s) => std::sync::Mutex::new(s),
                Err(_) => return,
            };
            let emit = |line: &str, _stream: &str| {
                let msg = format!("<30>{tag}: {line}\n");
                if let Ok(mut s) = stream.lock() {
                    if s.write_all(msg.as_bytes()).is_err() {
                        if let Ok(news) = std::net::TcpStream::connect(addr) {
                            *s = news;
                            let _ = s.write_all(msg.as_bytes());
                        }
                    }
                }
            };
            let emit: &(dyn Fn(&str, &str) + Sync) = &emit;
            std::thread::scope(|sc| {
                sc.spawn(|| {
                    run_tagged_tail(
                        console_log,
                        stop,
                        emit,
                        TaggedTailOptions {
                            stream: "stdout",
                            runtime_filter: Some(&runtime_filter),
                            bound: cap,
                            ready,
                            eof_policy,
                        },
                    )
                });
                sc.spawn(|| {
                    run_tagged_tail(
                        err_log,
                        stop,
                        emit,
                        TaggedTailOptions {
                            stream: "stderr",
                            runtime_filter: Some(&runtime_filter),
                            bound: cap,
                            ready,
                            eof_policy,
                        },
                    )
                });
            });
        }
        _ => {}
    }
}

/// A file writer that rotates (and gzips) when the file exceeds `max_size`.
struct RotatingWriter {
    path: PathBuf,
    file: std::fs::File,
    written: u64,
    max_size: u64,
    max_file: u32,
}

impl RotatingWriter {
    fn new(path: &Path, max_size: u64, max_file: u32) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let written = file.metadata()?.len();
        Ok(Self {
            path: path.to_path_buf(),
            file,
            written,
            max_size,
            max_file,
        })
    }

    fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        let bytes = format!("{line}\n");
        self.file.write_all(bytes.as_bytes())?;
        self.file.flush()?;
        self.written += bytes.len() as u64;
        if self.written >= self.max_size {
            self.rotate()?;
        }
        Ok(())
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        for i in (1..self.max_file).rev() {
            let from = rotated_path(&self.path, i);
            let to = rotated_path(&self.path, i + 1);
            if from.exists() {
                std::fs::rename(&from, &to)?;
            }
        }
        let oldest = rotated_path(&self.path, self.max_file);
        if oldest.exists() {
            std::fs::remove_file(&oldest)?;
        }
        let rotated = rotated_path(&self.path, 1);
        compress_file(&self.path, &rotated)?;
        std::fs::remove_file(&self.path)?;
        self.file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

/// Compress a file with gzip.
fn compress_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Read;

    let mut input = std::fs::File::open(src)?;
    let output = std::fs::File::create(dst)?;
    let mut encoder = GzEncoder::new(output, Compression::fast());
    let mut buf = [0u8; 8192];
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        encoder.write_all(&buf[..n])?;
    }
    encoder.finish()?;
    Ok(())
}

/// Generate a rotated file path: container.json → container.json.1.gz
fn rotated_path(base: &Path, index: u32) -> PathBuf {
    let mut p = base.as_os_str().to_owned();
    p.push(format!(".{index}.gz"));
    PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_driver_from_str() {
        assert_eq!(
            "json-file".parse::<LogDriver>().unwrap(),
            LogDriver::JsonFile
        );
        assert_eq!("syslog".parse::<LogDriver>().unwrap(), LogDriver::Syslog);
        assert_eq!("none".parse::<LogDriver>().unwrap(), LogDriver::None);
        assert!("unknown".parse::<LogDriver>().is_err());
    }

    #[test]
    fn test_log_config_defaults() {
        let config = LogConfig::default();
        assert_eq!(config.driver, LogDriver::JsonFile);
        assert_eq!(config.max_size(), 10 * 1024 * 1024);
        assert_eq!(config.max_file(), 3);
    }

    #[test]
    fn test_log_config_custom_options() {
        let mut config = LogConfig::default();
        config
            .options
            .insert("max-size".to_string(), "50m".to_string());
        config
            .options
            .insert("max-file".to_string(), "5".to_string());
        assert_eq!(config.max_size(), 50 * 1024 * 1024);
        assert_eq!(config.max_file(), 5);
    }

    #[test]
    fn test_parse_size() {
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("10m").unwrap(), 10 * 1024 * 1024);
        assert_eq!(parse_size("1g").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("512k").unwrap(), 512 * 1024);
        assert!(parse_size("abc").is_err());
    }

    #[test]
    fn test_log_entry_serialization() {
        let entry = LogEntry {
            log: "hello\n".to_string(),
            stream: "stdout".to_string(),
            time: "2026-02-12T06:00:00.000000000Z".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"log\":\"hello\\n\""));
        assert!(json.contains("\"stream\":\"stdout\""));
    }

    #[test]
    fn sandbox_log_worker_spec_round_trips_generation_identity() {
        let spec = SandboxLogWorkerSpec {
            schema: SANDBOX_LOG_WORKER_SCHEMA.to_string(),
            box_id: "sandbox-id".to_string(),
            console_log: PathBuf::from("/tmp/sandbox-id/logs/console.log"),
            log_config: LogConfig::default(),
            watched_pid: 123,
            watched_pid_start_time: 456,
            ready_file: PathBuf::from("/tmp/sandbox-id/sandbox/log-worker.ready"),
        };

        let encoded = serde_json::to_vec(&spec).unwrap();
        let decoded: SandboxLogWorkerSpec = serde_json::from_slice(&encoded).unwrap();

        assert_eq!(decoded, spec);
    }

    #[test]
    fn writer_closed_eof_skips_the_late_console_settle_window() {
        assert_eq!(
            stopped_eof_settle_polls(ConsoleEofPolicy::MayReceiveLateWrites),
            25
        );
        assert_eq!(stopped_eof_settle_polls(ConsoleEofPolicy::WriterClosed), 1);
    }

    #[test]
    fn test_syslog_config_defaults() {
        let config = LogConfig {
            driver: LogDriver::Syslog,
            options: HashMap::new(),
        };
        assert_eq!(config.syslog_address(), "udp://localhost:514");
        assert_eq!(config.syslog_facility(), "daemon");
        assert_eq!(config.tag(), None);
    }

    #[test]
    fn test_syslog_config_custom() {
        let mut options = HashMap::new();
        options.insert(
            "syslog-address".to_string(),
            "tcp://loghost:1514".to_string(),
        );
        options.insert("syslog-facility".to_string(), "local0".to_string());
        options.insert("tag".to_string(), "myapp".to_string());
        let config = LogConfig {
            driver: LogDriver::Syslog,
            options,
        };
        assert_eq!(config.syslog_address(), "tcp://loghost:1514");
        assert_eq!(config.syslog_facility(), "local0");
        assert_eq!(config.tag(), Some("myapp"));
    }

    #[test]
    fn test_log_driver_display() {
        assert_eq!(LogDriver::JsonFile.to_string(), "json-file");
        assert_eq!(LogDriver::Syslog.to_string(), "syslog");
        assert_eq!(LogDriver::None.to_string(), "none");
    }

    #[test]
    fn test_log_driver_serde_roundtrip() {
        let driver = LogDriver::Syslog;
        let json = serde_json::to_string(&driver).unwrap();
        assert_eq!(json, "\"syslog\"");
        let parsed: LogDriver = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, LogDriver::Syslog);
    }

    #[test]
    fn test_tail_next_line_returns_complete_lines() {
        use std::io::Cursor;
        // Two complete lines (CRLF then LF) returned newline-stripped; a third
        // read at EOF with stop=true returns None (VM exited, console drained).
        let mut reader = BufReader::new(Cursor::new(b"alpha\r\nbeta\n".to_vec()));
        let mut buf = String::new();
        let stop = AtomicBool::new(true);
        assert_eq!(
            tail_next_line(
                &mut reader,
                &mut buf,
                &stop,
                None,
                ConsoleEofPolicy::MayReceiveLateWrites,
                None,
            ),
            Some("alpha".to_string())
        );
        assert_eq!(
            tail_next_line(
                &mut reader,
                &mut buf,
                &stop,
                None,
                ConsoleEofPolicy::MayReceiveLateWrites,
                None,
            ),
            Some("beta".to_string())
        );
        assert_eq!(
            tail_next_line(
                &mut reader,
                &mut buf,
                &stop,
                None,
                ConsoleEofPolicy::MayReceiveLateWrites,
                None,
            ),
            None
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn test_tail_next_line_flushes_trailing_partial_on_stop() {
        use std::io::Cursor;
        // A final line without a trailing newline is still flushed once when the
        // VM has exited (stop=true) — no dropped last line.
        let mut reader = BufReader::new(Cursor::new(b"only-partial".to_vec()));
        let mut buf = String::new();
        let stop = AtomicBool::new(true);
        assert_eq!(
            tail_next_line(
                &mut reader,
                &mut buf,
                &stop,
                None,
                ConsoleEofPolicy::MayReceiveLateWrites,
                None,
            ),
            Some("only-partial".to_string())
        );
        assert_eq!(
            tail_next_line(
                &mut reader,
                &mut buf,
                &stop,
                None,
                ConsoleEofPolicy::MayReceiveLateWrites,
                None,
            ),
            None
        );
    }

    #[test]
    fn test_console_truncate_if_over_only_when_over_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.log");
        std::fs::write(&path, b"hello").unwrap(); // 5 bytes

        assert!(!console_truncate_if_over(&path, 10, None)); // under cap → untouched
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 5);

        assert!(console_truncate_if_over(&path, 4, None)); // over cap → truncated
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);

        // Missing file: false, no panic.
        assert!(!console_truncate_if_over(&dir.path().join("nope"), 0, None));
    }

    #[test]
    fn test_run_tagged_tail_truncates_over_cap_and_keeps_emitting() {
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.log");
        // Pre-fill past a tiny cap with three complete lines.
        std::fs::write(&path, b"l1\nl2\nl3\n").unwrap();
        let cap = 4u64;

        let collected = Arc::new(Mutex::new(Vec::<String>::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (c2, s2, p2) = (collected.clone(), stop.clone(), path.clone());
        let handle = std::thread::spawn(move || {
            let emit = move |line: &str, _stream: &str| c2.lock().unwrap().push(line.to_string());
            let emit: &(dyn Fn(&str, &str) + Sync) = &emit;
            run_tagged_tail(
                &p2,
                &s2,
                emit,
                TaggedTailOptions {
                    stream: "stdout",
                    runtime_filter: None,
                    bound: Some(cap),
                    ready: None,
                    eof_policy: ConsoleEofPolicy::MayReceiveLateWrites,
                },
            );
        });

        // Let the tail drain l1..l3, hit EOF, and truncate (9 bytes > cap 4).
        std::thread::sleep(Duration::from_millis(300));
        // libkrun-style O_APPEND write after the truncation.
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(b"l4\nl5\n").unwrap();
        }
        std::thread::sleep(Duration::from_millis(300));
        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();

        let got = collected.lock().unwrap().clone();
        // No data lost across the truncation: pre- and post-truncation lines both emit.
        for line in ["l1", "l3", "l4", "l5"] {
            assert!(got.contains(&line.to_string()), "missing {line} in {got:?}");
        }
        // And the raw file stayed bounded (truncated, not left at full history).
        let final_len = std::fs::metadata(&path).unwrap().len();
        assert!(
            final_len <= cap + 6,
            "console.log unbounded: {final_len} bytes"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn test_run_tagged_tail_refuses_replaced_source_identity() {
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("guest-init.stdout.log");
        let retired = dir.path().join("guest-init.stdout.log.retired");
        std::fs::write(&path, b"").unwrap();

        let collected = Arc::new(Mutex::new(Vec::<String>::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (c2, s2, p2) = (collected.clone(), stop.clone(), path.clone());
        let handle = std::thread::spawn(move || {
            let emit = move |line: &str, _stream: &str| c2.lock().unwrap().push(line.to_string());
            let emit: &(dyn Fn(&str, &str) + Sync) = &emit;
            run_tagged_tail(
                &p2,
                &s2,
                emit,
                TaggedTailOptions {
                    stream: "stdout",
                    runtime_filter: None,
                    bound: None,
                    ready: None,
                    eof_policy: ConsoleEofPolicy::MayReceiveLateWrites,
                },
            );
        });

        // A guest may replace the path after the host tailer has pinned the
        // original handle. Reopening must never switch to the replacement.
        std::thread::sleep(Duration::from_millis(300));
        std::fs::rename(&path, &retired).unwrap();
        std::fs::write(&path, b"late-line\n").unwrap();

        std::thread::sleep(Duration::from_millis(300));

        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();
        let got = collected.lock().unwrap().clone();
        assert!(!got.iter().any(|line| line == "late-line"), "{got:?}");
    }

    #[test]
    fn explicit_streams_with_ready_reports_both_open_readers() {
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let stdout = dir.path().join("guest.stdout.log");
        let stderr = dir.path().join("guest.stderr.log");
        std::fs::write(&stdout, b"").unwrap();
        std::fs::write(&stderr, b"").unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (thread_stop, thread_ready) = (Arc::clone(&stop), Arc::clone(&ready));
        let log_dir = dir.path().to_path_buf();
        let handle = std::thread::spawn(move || {
            run_log_processor_streams_with_ready(
                &stdout,
                &stderr,
                &log_dir,
                &LogConfig::default(),
                &thread_stop,
                Some(&thread_ready),
            );
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        while ready.load(Ordering::Acquire) < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(ready.load(Ordering::Acquire), 2);

        stop.store(true, Ordering::Release);
        handle.join().unwrap();
    }

    #[test]
    fn test_stopped_tail_waits_for_delayed_final_console_write() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.log");
        std::fs::write(&path, b"").unwrap();
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(writer_path)
                .unwrap();
            file.write_all(b"late-final-line\n").unwrap();
            file.flush().unwrap();
        });

        let file = std::fs::File::open(&path).unwrap();
        let mut reader = BufReader::new(file);
        let mut buffer = String::new();
        let stop = AtomicBool::new(true);
        assert_eq!(
            tail_next_line(
                &mut reader,
                &mut buffer,
                &stop,
                None,
                ConsoleEofPolicy::MayReceiveLateWrites,
                None,
            ),
            Some("late-final-line".to_string())
        );
        assert_eq!(
            tail_next_line(
                &mut reader,
                &mut buffer,
                &stop,
                None,
                ConsoleEofPolicy::MayReceiveLateWrites,
                None,
            ),
            None
        );
        writer.join().unwrap();
    }

    #[test]
    fn test_none_driver_still_bounds_console() {
        use std::sync::Arc;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let console = dir.path().join("console.log");
        std::fs::write(&console, b"l1\nl2\nl3\n").unwrap(); // 9 bytes
        std::fs::write(dir.path().join("console.err.log"), b"").unwrap();

        // none driver, tiny cap (max_size 4 * max_file 1).
        let mut options = HashMap::new();
        options.insert("max-size".to_string(), "4".to_string());
        options.insert("max-file".to_string(), "1".to_string());
        let config = LogConfig {
            driver: LogDriver::None,
            options,
        };

        let stop = Arc::new(AtomicBool::new(false));
        let (s2, c2, d2) = (stop.clone(), console.clone(), dir.path().to_path_buf());
        let handle = std::thread::spawn(move || run_log_processor(&c2, &d2, &config, &s2));

        std::thread::sleep(Duration::from_millis(300));
        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();

        // The raw console was bounded even though `none` produces no output, and
        // no container.json was written.
        assert!(std::fs::metadata(&console).unwrap().len() <= 4);
        assert!(!dir.path().join("container.json").exists());
    }

    #[test]
    fn test_is_runtime_console_noise() {
        assert!(is_runtime_console_noise("init.krun: mount_filesystems ok"));
        assert!(is_runtime_console_noise("init.krun: entered main argc=1"));
        assert!(is_runtime_console_noise(
            "init.krun: selected exec=/bin/app init_pid1=0"
        ));
        assert!(is_runtime_console_noise(
            "init.krun: execvp(/bin/app) starting"
        ));
        assert!(!is_runtime_console_noise("init.krun: business"));
        assert!(!is_runtime_console_noise(
            "init.krun: entered main argc=not-a-number"
        ));
        assert!(!is_runtime_console_noise(
            "init.krun: execvp(/bin/app) failed errno=2"
        ));
        assert!(!is_runtime_console_noise("L1"));
        assert!(!is_runtime_console_noise(
            "starting app (init.krun: ignored)"
        ));
        assert!(!is_runtime_console_noise(""));
    }

    #[test]
    fn runtime_console_filter_shares_sentinel_phase_across_streams() {
        let filter = RuntimeConsoleFilter::new();

        // Treat these calls as interleaved stdout/stderr records using the
        // same shared filter, as the structured log processor does.
        assert!(!filter.keep_line("init.krun: mount_filesystems ok"));
        assert!(filter.keep_line("init.krun: business"));
        assert!(!filter.keep_line("init.krun: execvp(/bin/app) starting"));
        assert!(!filter.preamble_active());
        assert!(filter.keep_line("init.krun: mount_filesystems ok"));
        assert!(filter.keep_line("init.krun: execvp(/bin/app) starting"));
        assert!(filter.keep_line("init.krun: execvp(/bin/app) failed errno=2"));
    }

    #[test]
    fn test_run_json_file_processor_captures_all_lines_after_stop() {
        // The processor must emit a record for EVERY console line, then stop
        // cleanly once the VM has exited (stop=true). The original bug dropped
        // every line logged after the first EOF (here: BBB after a quiet line).
        let dir = tempfile::tempdir().unwrap();
        let console = dir.path().join("console.log");
        let stderr = dir.path().join("persisted-stderr.log");
        std::fs::write(
            &console,
            concat!(
                "init.krun: entered main argc=1\n",
                "init.krun: mount_filesystems ok\n",
                "init.krun: execvp(/bin/app) starting\n",
                "AAA\n",
                "init.krun: business\n",
                "BBB\n",
            ),
        )
        .unwrap();
        std::fs::write(&stderr, "ERR\n").unwrap();
        let stop = AtomicBool::new(true);
        run_log_processor_streams(&console, &stderr, dir.path(), &LogConfig::default(), &stop);
        let json = std::fs::read_to_string(json_log_path(dir.path())).unwrap();
        assert!(json.contains("\"log\":\"AAA\\n\""), "AAA missing: {json}");
        assert!(
            json.contains("\"log\":\"BBB\\n\""),
            "BBB (after a quiet line) missing: {json}"
        );
        assert!(
            json.contains("\"log\":\"ERR\\n\"") && json.contains("\"stream\":\"stderr\""),
            "custom stderr stream missing: {json}"
        );
        assert!(
            json.contains("\"log\":\"init.krun: business\\n\""),
            "generic init.krun workload output missing: {json}"
        );
        assert!(
            !json.contains("entered main"),
            "C-init noise leaked: {json}"
        );
        assert!(
            !json.contains("mount_filesystems ok"),
            "C-init noise leaked: {json}"
        );
        assert!(
            !json.contains("execvp(/bin/app) starting"),
            "C-init sentinel leaked: {json}"
        );
    }

    #[test]
    fn test_run_json_file_processor_preserves_unterminated_prefix_line() {
        let dir = tempfile::tempdir().unwrap();
        let console = dir.path().join("console.log");
        let stderr = dir.path().join("console.err.log");
        std::fs::write(&console, "init.krun: mount_filesystems ok").unwrap();
        std::fs::write(&stderr, "").unwrap();

        let stop = AtomicBool::new(true);
        run_log_processor_streams(&console, &stderr, dir.path(), &LogConfig::default(), &stop);

        let json = std::fs::read_to_string(json_log_path(dir.path())).unwrap();
        assert!(
            json.contains("\"log\":\"init.krun: mount_filesystems ok\\n\""),
            "unterminated workload fragment was dropped: {json}"
        );
    }

    #[test]
    fn ordered_json_writer_clamps_a_regressed_clock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("container.json");
        let mut writer = OrderedJsonWriter::new(&path, 10 * 1024 * 1024, 3).unwrap();
        let newer = chrono::DateTime::parse_from_rfc3339("2026-07-19T12:00:01Z")
            .unwrap()
            .to_utc();
        let older = chrono::DateTime::parse_from_rfc3339("2026-07-19T12:00:00Z")
            .unwrap()
            .to_utc();

        writer.write_entry("first", "stdout", newer);
        writer.write_entry("second", "stderr", older);
        drop(writer);

        let entries = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<LogEntry>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].time, entries[1].time);
    }

    #[test]
    fn test_rotating_writer_rotates_and_gzips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("container.json");
        let mut w = RotatingWriter::new(&path, 20, 3).unwrap();
        for i in 0..10 {
            w.write_line(&format!("line-{i}")).unwrap();
        }
        assert!(
            rotated_path(&path, 1).exists(),
            "expected a rotated .1.gz file"
        );
    }
}
