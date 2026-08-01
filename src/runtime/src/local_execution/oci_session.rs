//! Exact-generation process sessions over the public A3S OCI SDK.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use a3s_box_core::exec::{
    ExecChunk, ExecEvent, ExecExit, ExecOutput, ExecRequest as BoxExecRequest, StreamType,
    DEFAULT_EXEC_TIMEOUT_NS, MAX_ONE_SHOT_OUTPUT_BYTES,
};
use a3s_box_core::pty::PtyRequest;
use a3s_box_core::{
    ExecutionGeneration, ExecutionManagerError, ExecutionManagerResult, ExecutionProcess,
    ExecutionProcessInput, ExecutionProcessSignal, ExecutionProcessStream,
};
use a3s_oci_sdk::oci_spec::runtime::{
    Capabilities, LinuxCapabilitiesBuilder, Process, ProcessBuilder, UserBuilder,
};
use a3s_oci_sdk::{
    CloseStdinRequest, ErrorCode, ExecRequest as OciExecRequest, ExitStatus, IoMode, OutputChunk,
    OutputStream, ProcessId, ProcessIo, ProcessRecord, ProcessTarget, ReadOutputRequest,
    ResizeRequest, RuntimeClient, RuntimeOperation, Signal, SignalProcessRequest, TerminalSize,
    WaitProcessRequest, WriteStdinRequest,
};
use async_trait::async_trait;
use tokio::sync::Mutex;

use super::oci_backend::{exit_code, operation_context, sdk_error, OciLocalExecutionBackend};
use crate::BoxRecord;

const OUTPUT_POLL_BYTES: u32 = 64 * 1024;
const OUTPUT_POLL_WAIT_MS: u64 = 250;
const MAX_EMPTY_POLLS_AFTER_EXIT: u8 = 4;
const TIMEOUT_SIGNAL: i32 = 9;
const MAX_REQUEST_ID_BYTES: usize = 512;
const WATCHDOG_WAITING: u8 = 0;
const WATCHDOG_TIMED_OUT: u8 = 1;
const WATCHDOG_FINISHED: u8 = 2;
const WATCHDOG_FAILED: u8 = 3;
const WATCHDOG_TARGET_RETRIES: u8 = 40;

pub(super) async fn execute(
    backend: &OciLocalExecutionBackend,
    record: &BoxRecord,
    request: BoxExecRequest,
) -> ExecutionManagerResult<ExecOutput> {
    let launch = LaunchRequest::from_exec(request, true)?;
    let mut stream = launch_process(backend, record, launch).await?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut truncated = false;

    while let Some(event) = stream.next_event().await? {
        match event {
            ExecEvent::Chunk(chunk) => {
                let destination = match chunk.stream {
                    StreamType::Stdout => &mut stdout,
                    StreamType::Stderr => &mut stderr,
                };
                retain_bounded(destination, &chunk.data, &mut truncated);
            }
            ExecEvent::FlushAck => {}
            ExecEvent::Exit(status) => {
                return Ok(ExecOutput {
                    stdout,
                    stderr,
                    exit_code: status.exit_code,
                    truncated,
                });
            }
        }
    }

    Err(ExecutionManagerError::Internal(format!(
        "A3S OCI exec process for {} ended without an exact terminal status",
        record.id
    )))
}

pub(super) async fn start_process(
    backend: &OciLocalExecutionBackend,
    record: &BoxRecord,
    request: BoxExecRequest,
) -> ExecutionManagerResult<ExecutionProcess> {
    if request.request_id.is_some() {
        return Err(ExecutionManagerError::InvalidRequest(
            "streaming A3S OCI exec does not accept a one-shot request_id".to_string(),
        ));
    }
    Ok(Box::new(
        launch_process(backend, record, LaunchRequest::from_exec(request, false)?).await?,
    ))
}

pub(super) async fn start_pty(
    backend: &OciLocalExecutionBackend,
    record: &BoxRecord,
    request: PtyRequest,
) -> ExecutionManagerResult<ExecutionProcess> {
    Ok(Box::new(
        launch_process(backend, record, LaunchRequest::from_pty(request)?).await?,
    ))
}

struct LaunchRequest {
    seed: String,
    args: Vec<String>,
    env: Vec<String>,
    working_dir: Option<String>,
    rootfs: Option<String>,
    user: Option<String>,
    io: ProcessIo,
    initial_stdin: Option<Vec<u8>>,
    keep_stdin_open: bool,
    timeout_ns: Option<u64>,
}

impl LaunchRequest {
    fn from_exec(request: BoxExecRequest, one_shot: bool) -> ExecutionManagerResult<Self> {
        let BoxExecRequest {
            request_id,
            cmd,
            timeout_ns,
            env,
            working_dir,
            rootfs,
            stdin,
            stdin_streaming,
            user,
            streaming: _,
        } = request;
        if let Some(request_id) = request_id.as_deref() {
            if request_id.is_empty()
                || request_id.len() > MAX_REQUEST_ID_BYTES
                || request_id.contains('\0')
            {
                return Err(ExecutionManagerError::InvalidRequest(
                    "A3S OCI exec request ID is invalid".to_string(),
                ));
            }
        }
        let stdin_enabled = stdin.is_some() || stdin_streaming;
        let seed =
            request_id.unwrap_or_else(|| format!("managed-exec-{}", uuid::Uuid::new_v4().simple()));
        let timeout_ns = if timeout_ns == 0 {
            DEFAULT_EXEC_TIMEOUT_NS
        } else {
            timeout_ns
        };
        Ok(Self {
            seed,
            args: cmd,
            env,
            working_dir,
            rootfs,
            user,
            io: ProcessIo {
                stdin: if stdin_enabled {
                    IoMode::Pipe
                } else {
                    IoMode::Null
                },
                stdout: IoMode::Capture,
                stderr: IoMode::Capture,
                terminal_size: None,
            },
            initial_stdin: stdin,
            // A captured one-shot call has no input handle to keep alive.
            keep_stdin_open: !one_shot && stdin_streaming,
            timeout_ns: Some(timeout_ns),
        })
    }

    fn from_pty(request: PtyRequest) -> ExecutionManagerResult<Self> {
        if request.cols == 0 || request.rows == 0 {
            return Err(ExecutionManagerError::InvalidRequest(
                "PTY width and height must both be positive".to_string(),
            ));
        }
        Ok(Self {
            seed: format!("managed-pty-{}", uuid::Uuid::new_v4().simple()),
            args: request.cmd,
            env: request.env,
            working_dir: request.working_dir,
            rootfs: request.rootfs,
            user: request.user,
            io: ProcessIo {
                stdin: IoMode::Terminal,
                stdout: IoMode::Terminal,
                stderr: IoMode::Terminal,
                terminal_size: Some(TerminalSize {
                    width: request.cols,
                    height: request.rows,
                }),
            },
            initial_stdin: None,
            keep_stdin_open: true,
            timeout_ns: None,
        })
    }
}

async fn launch_process(
    backend: &OciLocalExecutionBackend,
    record: &BoxRecord,
    launch: LaunchRequest,
) -> ExecutionManagerResult<OciProcessStream> {
    let deadline = launch
        .timeout_ns
        .map(Duration::from_nanos)
        .map(|timeout| {
            Instant::now().checked_add(timeout).ok_or_else(|| {
                ExecutionManagerError::InvalidRequest(
                    "A3S OCI exec timeout exceeds the host clock range".to_string(),
                )
            })
        })
        .transpose()?;
    let execution_id = backend.execution_id(record)?;
    let box_generation = backend.metadata(record)?.generation;
    let binding = backend.binding(record)?.ok_or_else(|| {
        ExecutionManagerError::Internal(format!(
            "execution {execution_id} has no exact A3S OCI binding for exec"
        ))
    })?;
    binding.validate_for(&execution_id)?;
    let terminal = matches!(launch.io.stdin, IoMode::Terminal);
    let process = build_process(
        launch.args,
        launch.env,
        launch.working_dir,
        launch.rootfs,
        launch.user,
        terminal,
    )?;
    let client = backend.client();
    require_process_operations(&client, launch.io.stdin, terminal).await?;
    let initial_stdin_digest = launch.initial_stdin.as_deref().map(sha256_digest);
    let context = operation_context(
        &launch.seed,
        box_generation,
        "exec",
        (
            "a3s.box.oci-exec.v1",
            &binding.target,
            terminal,
            process.user().uid(),
            process.user().gid(),
            process.args(),
            process.env(),
            process.cwd(),
            process.no_new_privileges(),
            &launch.io,
            initial_stdin_digest,
            launch.keep_stdin_open,
            launch.timeout_ns,
        ),
    )?;
    // A caller-supplied one-shot key always names one process in one exact
    // container generation. The exec operation identity also fingerprints the
    // complete request above, so changed content cannot become a second
    // process merely by producing a different operation digest.
    let process_context = operation_context(
        &launch.seed,
        box_generation,
        "exec-process",
        ("a3s.box.oci-exec-process.v1", &binding.target),
    )?;
    let process_id = ProcessId::new(process_context.operation_id.as_str().to_string())
        .map_err(|error| sdk_error("exec process identity", error))?;
    let expected_target = ProcessTarget {
        container: binding.target.clone(),
        process_id: process_id.clone(),
    };
    let exec_request = OciExecRequest {
        context,
        container: binding.target,
        process_id,
        process,
        io: launch.io.clone(),
    };
    let watchdog = Arc::new(AtomicU8::new(if deadline.is_some() {
        WATCHDOG_WAITING
    } else {
        WATCHDOG_FINISHED
    }));
    if let Some(deadline) = deadline {
        let timeout_context = operation_context(
            expected_target.process_id.as_str(),
            box_generation,
            "exec-timeout",
            &expected_target,
        )?;
        spawn_timeout_watchdog(
            client.clone(),
            expected_target.clone(),
            exec_request.clone(),
            terminal,
            deadline,
            timeout_context,
            watchdog.clone(),
        );
    }
    let process_record = client
        .exec(exec_request)
        .await
        .map_err(|error| sdk_error("exec", error))?;
    validate_process_record(&process_record, &expected_target, terminal)?;

    let input = Arc::new(OciProcessInput {
        client: client.clone(),
        process: expected_target.clone(),
        execution_id: execution_id.to_string(),
        box_generation,
        stdin_enabled: matches!(launch.io.stdin, IoMode::Pipe | IoMode::Terminal),
        terminal,
        state: Mutex::new(InputState::default()),
    });
    if let Some(data) = launch.initial_stdin.as_deref() {
        input.write_stdin(data).await?;
    }
    if matches!(launch.io.stdin, IoMode::Pipe) && !launch.keep_stdin_open {
        input.close_stdin().await?;
    }
    Ok(OciProcessStream {
        client,
        input,
        target: expected_target,
        cursor: 0,
        pending: VecDeque::new(),
        stdout_eof: false,
        stderr_eof: terminal,
        terminal,
        status: None,
        deadline,
        watchdog,
        timeout_notice_queued: false,
        exit_queued: false,
        done: false,
        empty_polls_after_exit: 0,
    })
}

fn spawn_timeout_watchdog(
    client: RuntimeClient,
    target: ProcessTarget,
    exec_request: OciExecRequest,
    terminal: bool,
    deadline: Instant,
    context: a3s_oci_sdk::OperationContext,
    state: Arc<AtomicU8>,
) {
    tokio::spawn(async move {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
        let mut found_running = false;
        for attempt in 0..WATCHDOG_TARGET_RETRIES {
            match client
                .wait_process(WaitProcessRequest {
                    process: target.clone(),
                    timeout_ms: Some(0),
                })
                .await
            {
                Ok(_) => {
                    state.store(WATCHDOG_FINISHED, Ordering::SeqCst);
                    return;
                }
                Err(error) if error.code == ErrorCode::DeadlineExceeded => {
                    found_running = true;
                    break;
                }
                Err(error)
                    if error.code == ErrorCode::NotFound
                        && attempt + 1 < WATCHDOG_TARGET_RETRIES =>
                {
                    // The watchdog is armed before exec dispatch so a lost
                    // exec response cannot orphan a process. A very short
                    // timeout can therefore fire while the exact process is
                    // still being registered.
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(error) if error.code == ErrorCode::NotFound => {
                    state.store(WATCHDOG_FINISHED, Ordering::SeqCst);
                    return;
                }
                Err(error) => {
                    tracing::warn!(
                        process_id = %target.process_id,
                        %error,
                        "A3S OCI exec timeout watchdog could not inspect the process"
                    );
                    state.store(WATCHDOG_FAILED, Ordering::SeqCst);
                    return;
                }
            }
        }
        if !found_running {
            state.store(WATCHDOG_FINISHED, Ordering::SeqCst);
            return;
        }

        // Confirm that the running stable process ID belongs to this exact
        // keyed request before signaling it. This replay is read-only at the
        // durable operation boundary. A same-key request with changed command,
        // stdin, timeout, or I/O receives a conflicting exec identity and must
        // never let its detached watchdog kill the original process.
        let mut validated = false;
        for attempt in 0..3_u8 {
            match client.exec(exec_request.clone()).await {
                Ok(record) => match validate_process_record(&record, &target, terminal) {
                    Ok(()) => {
                        validated = true;
                        break;
                    }
                    Err(error) => {
                        tracing::warn!(
                            process_id = %target.process_id,
                            %error,
                            "A3S OCI exec timeout watchdog received an invalid replay binding"
                        );
                        state.store(WATCHDOG_FAILED, Ordering::SeqCst);
                        return;
                    }
                },
                Err(error) if error.retryable || error.code == ErrorCode::Unavailable => {
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        process_id = %target.process_id,
                        %error,
                        "A3S OCI exec timeout watchdog refused a non-matching replay"
                    );
                    state.store(WATCHDOG_FAILED, Ordering::SeqCst);
                    return;
                }
            }
        }
        if !validated {
            tracing::warn!(
                process_id = %target.process_id,
                "A3S OCI exec timeout watchdog could not validate its exact replay"
            );
            state.store(WATCHDOG_FAILED, Ordering::SeqCst);
            return;
        }

        let signal = match Signal::new(TIMEOUT_SIGNAL) {
            Ok(signal) => signal,
            Err(error) => {
                tracing::warn!(
                    process_id = %target.process_id,
                    %error,
                    "A3S OCI exec timeout watchdog could not construct SIGKILL"
                );
                state.store(WATCHDOG_FAILED, Ordering::SeqCst);
                return;
            }
        };
        let request = SignalProcessRequest {
            context,
            process: target.clone(),
            signal,
        };
        for attempt in 0..3_u8 {
            match client.signal_process(request.clone()).await {
                Ok(()) => {
                    state.store(WATCHDOG_TIMED_OUT, Ordering::SeqCst);
                    return;
                }
                Err(error) if error.code == ErrorCode::NotFound => {
                    state.store(WATCHDOG_FINISHED, Ordering::SeqCst);
                    return;
                }
                Err(error) if error.retryable || error.code == ErrorCode::Unavailable => {
                    match client
                        .wait_process(WaitProcessRequest {
                            process: target.clone(),
                            timeout_ms: Some(0),
                        })
                        .await
                    {
                        Ok(_) => {
                            // The only mutation this task dispatched was the
                            // exact timeout signal, so a terminal replay after
                            // a lost response belongs to this watchdog.
                            state.store(WATCHDOG_TIMED_OUT, Ordering::SeqCst);
                            return;
                        }
                        Err(wait_error) if wait_error.code == ErrorCode::DeadlineExceeded => {}
                        Err(wait_error) if wait_error.code == ErrorCode::NotFound => {
                            state.store(WATCHDOG_FINISHED, Ordering::SeqCst);
                            return;
                        }
                        Err(wait_error) => {
                            tracing::warn!(
                                process_id = %target.process_id,
                                %wait_error,
                                "A3S OCI exec timeout watchdog could not reconcile a lost signal response"
                            );
                            state.store(WATCHDOG_FAILED, Ordering::SeqCst);
                            return;
                        }
                    }
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        process_id = %target.process_id,
                        %error,
                        "A3S OCI exec timeout watchdog could not signal the process"
                    );
                    state.store(WATCHDOG_FAILED, Ordering::SeqCst);
                    return;
                }
            }
        }
        tracing::warn!(
            process_id = %target.process_id,
            "A3S OCI exec timeout watchdog exhausted signal retries"
        );
        state.store(WATCHDOG_FAILED, Ordering::SeqCst);
    });
}

fn build_process(
    args: Vec<String>,
    env: Vec<String>,
    working_dir: Option<String>,
    rootfs: Option<String>,
    user: Option<String>,
    terminal: bool,
) -> ExecutionManagerResult<Process> {
    if args.is_empty() {
        return Err(ExecutionManagerError::InvalidRequest(
            "A3S OCI exec requires a non-empty command".to_string(),
        ));
    }
    if rootfs.is_some() {
        return Err(ExecutionManagerError::InvalidRequest(
            "A3S OCI exec cannot reinterpret a second rootfs; prepare it as the container attachment"
                .to_string(),
        ));
    }
    let (uid, gid) = parse_process_user(user.as_deref())?;
    let user = UserBuilder::default()
        .uid(uid)
        .gid(gid)
        .build()
        .map_err(|error| {
            ExecutionManagerError::InvalidRequest(format!(
                "failed to build A3S OCI exec user: {error}"
            ))
        })?;
    let empty = Capabilities::new();
    let capabilities = LinuxCapabilitiesBuilder::default()
        .bounding(empty.clone())
        .effective(empty.clone())
        .inheritable(empty.clone())
        .permitted(empty.clone())
        .ambient(empty)
        .build()
        .map_err(|error| {
            ExecutionManagerError::InvalidRequest(format!(
                "failed to build A3S OCI exec capabilities: {error}"
            ))
        })?;
    ProcessBuilder::default()
        .terminal(terminal)
        .user(user)
        .args(args)
        .env(env)
        .cwd(PathBuf::from(
            working_dir.unwrap_or_else(|| "/".to_string()),
        ))
        .capabilities(capabilities)
        .no_new_privileges(true)
        .build()
        .map_err(|error| {
            ExecutionManagerError::InvalidRequest(format!(
                "failed to build A3S OCI exec process: {error}"
            ))
        })
}

fn parse_process_user(user: Option<&str>) -> ExecutionManagerResult<(u32, u32)> {
    let Some(user) = user else {
        return Ok((0, 0));
    };
    let (uid, gid) = user
        .split_once(':')
        .map_or((user, None), |(uid, gid)| (uid, Some(gid)));
    let uid = parse_id_component(uid, "user")?;
    // The legacy Box guest keeps group 0 when a numeric UID has no matching
    // passwd entry. An explicit UID:GID remains exact.
    let gid = gid.map_or(Ok(0), |gid| parse_id_component(gid, "group"))?;
    Ok((uid, gid))
}

fn parse_id_component(value: &str, label: &str) -> ExecutionManagerResult<u32> {
    if value.eq_ignore_ascii_case("root") {
        return Ok(0);
    }
    value.parse::<u32>().map_err(|_| {
        ExecutionManagerError::InvalidRequest(format!(
            "A3S OCI exec {label} {value:?} is not root or a numeric ID"
        ))
    })
}

fn sha256_digest(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    hex::encode(Sha256::digest(data))
}

async fn require_process_operations(
    client: &RuntimeClient,
    stdin: IoMode,
    terminal: bool,
) -> ExecutionManagerResult<()> {
    let info = client
        .features()
        .await
        .map_err(|error| sdk_error("features", error))?;
    let mut required = vec![
        (RuntimeOperation::Exec, "exec"),
        (RuntimeOperation::ReadOutput, "read-output"),
        (RuntimeOperation::SignalProcess, "signal-process"),
        (RuntimeOperation::WaitProcess, "wait-process"),
    ];
    if matches!(stdin, IoMode::Pipe | IoMode::Terminal) {
        required.extend([
            (RuntimeOperation::WriteStdin, "write-stdin"),
            (RuntimeOperation::CloseStdin, "close-stdin"),
        ]);
    }
    if terminal {
        required.push((RuntimeOperation::Resize, "resize"));
    }
    for (operation, label) in required {
        if !info.operations.contains(&operation) {
            return Err(ExecutionManagerError::Unavailable(format!(
                "A3S OCI Runtime does not advertise {label}"
            )));
        }
    }
    Ok(())
}

fn validate_process_record(
    record: &ProcessRecord,
    target: &ProcessTarget,
    terminal: bool,
) -> ExecutionManagerResult<()> {
    if record.target != *target
        || record.terminal != terminal
        || record.pid.is_some_and(|pid| pid == 0)
        || record.target.container.generation.is_none()
    {
        return Err(ExecutionManagerError::Internal(format!(
            "A3S OCI exec returned an invalid process binding for {}",
            target.process_id
        )));
    }
    Ok(())
}

#[derive(Default)]
struct InputState {
    next_mutation: u64,
    stdin_closed: bool,
}

struct OciProcessInput {
    client: RuntimeClient,
    process: ProcessTarget,
    execution_id: String,
    box_generation: ExecutionGeneration,
    stdin_enabled: bool,
    terminal: bool,
    state: Mutex<InputState>,
}

impl OciProcessInput {
    fn context(
        &self,
        sequence: u64,
        operation: &str,
    ) -> ExecutionManagerResult<a3s_oci_sdk::OperationContext> {
        operation_context(
            self.process.process_id.as_str(),
            self.box_generation,
            operation,
            (&self.process, sequence),
        )
    }

    fn advance(state: &mut InputState) -> ExecutionManagerResult<()> {
        state.next_mutation = state.next_mutation.checked_add(1).ok_or_else(|| {
            ExecutionManagerError::Internal(
                "A3S OCI process mutation sequence is exhausted".to_string(),
            )
        })?;
        Ok(())
    }
}

#[async_trait]
impl ExecutionProcessInput for OciProcessInput {
    async fn write_stdin(&self, data: &[u8]) -> ExecutionManagerResult<()> {
        if !self.stdin_enabled {
            return Err(ExecutionManagerError::InvalidRequest(format!(
                "A3S OCI process for {} has no writable stdin",
                self.execution_id
            )));
        }
        let mut state = self.state.lock().await;
        if state.stdin_closed {
            return Err(ExecutionManagerError::InvalidRequest(format!(
                "A3S OCI process stdin for {} is already closed",
                self.execution_id
            )));
        }
        let context = self.context(state.next_mutation, "write-stdin")?;
        self.client
            .write_stdin(WriteStdinRequest {
                context,
                process: self.process.clone(),
                data: data.to_vec(),
            })
            .await
            .map_err(|error| sdk_error("write stdin", error))?;
        Self::advance(&mut state)
    }

    async fn close_stdin(&self) -> ExecutionManagerResult<()> {
        if !self.stdin_enabled {
            return Err(ExecutionManagerError::InvalidRequest(format!(
                "A3S OCI process for {} has no stdin to close",
                self.execution_id
            )));
        }
        let mut state = self.state.lock().await;
        if state.stdin_closed {
            return Ok(());
        }
        let context = self.context(state.next_mutation, "close-stdin")?;
        self.client
            .close_stdin(CloseStdinRequest {
                context,
                process: self.process.clone(),
            })
            .await
            .map_err(|error| sdk_error("close stdin", error))?;
        Self::advance(&mut state)?;
        state.stdin_closed = true;
        Ok(())
    }

    async fn cancel(&self) -> ExecutionManagerResult<()> {
        self.send_signal(ExecutionProcessSignal::Kill).await
    }

    async fn send_signal(&self, signal: ExecutionProcessSignal) -> ExecutionManagerResult<()> {
        let mut state = self.state.lock().await;
        let number = signal.linux_number();
        let context = self.context(state.next_mutation, "signal-process")?;
        self.client
            .signal_process(SignalProcessRequest {
                context,
                process: self.process.clone(),
                signal: Signal::new(number).map_err(|error| sdk_error("signal process", error))?,
            })
            .await
            .map_err(|error| sdk_error("signal process", error))?;
        Self::advance(&mut state)
    }

    async fn resize_pty(&self, cols: u16, rows: u16) -> ExecutionManagerResult<()> {
        if !self.terminal {
            return Err(ExecutionManagerError::InvalidRequest(
                "A3S OCI process does not have a PTY".to_string(),
            ));
        }
        let mut state = self.state.lock().await;
        let size = TerminalSize {
            width: cols,
            height: rows,
        };
        let context = self.context(state.next_mutation, "resize")?;
        self.client
            .resize(ResizeRequest {
                context,
                process: self.process.clone(),
                size,
            })
            .await
            .map_err(|error| sdk_error("resize PTY", error))?;
        Self::advance(&mut state)
    }
}

struct OciProcessStream {
    client: RuntimeClient,
    input: Arc<OciProcessInput>,
    target: ProcessTarget,
    cursor: u64,
    pending: VecDeque<ExecEvent>,
    stdout_eof: bool,
    stderr_eof: bool,
    terminal: bool,
    status: Option<ExitStatus>,
    deadline: Option<Instant>,
    watchdog: Arc<AtomicU8>,
    timeout_notice_queued: bool,
    exit_queued: bool,
    done: bool,
    empty_polls_after_exit: u8,
}

impl OciProcessStream {
    fn pop_pending(&mut self) -> Option<ExecEvent> {
        let event = self.pending.pop_front()?;
        if matches!(event, ExecEvent::Exit(_)) {
            self.done = true;
        }
        Some(event)
    }

    fn output_complete(&self) -> bool {
        self.stdout_eof && self.stderr_eof
    }

    fn output_wait_ms(&self) -> u64 {
        let Some(deadline) = self
            .deadline
            .filter(|_| self.watchdog.load(Ordering::SeqCst) == WATCHDOG_WAITING)
        else {
            return OUTPUT_POLL_WAIT_MS;
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        remaining
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
            .clamp(1, OUTPUT_POLL_WAIT_MS)
    }

    fn accept_output(&mut self, chunks: Vec<OutputChunk>) -> ExecutionManagerResult<()> {
        for chunk in chunks {
            let width = if chunk.eof {
                if !chunk.data.is_empty() {
                    return Err(self.invalid_output("EOF frame contains data"));
                }
                1
            } else {
                if chunk.data.is_empty() {
                    return Err(self.invalid_output("data frame is empty"));
                }
                u64::try_from(chunk.data.len()).map_err(|_| {
                    self.invalid_output("output frame length does not fit its cursor")
                })?
            };
            let expected = self
                .cursor
                .checked_add(width)
                .ok_or_else(|| self.invalid_output("output sequence cursor is exhausted"))?;
            if chunk.sequence != expected {
                return Err(self.invalid_output("output sequence is not contiguous"));
            }
            self.cursor = chunk.sequence;
            if self.terminal && chunk.stream != OutputStream::Stdout {
                return Err(self.invalid_output("terminal output was not merged into stdout"));
            }
            if chunk.eof {
                match chunk.stream {
                    OutputStream::Stdout => self.stdout_eof = true,
                    OutputStream::Stderr => self.stderr_eof = true,
                }
            } else {
                self.pending.push_back(ExecEvent::Chunk(ExecChunk {
                    stream: match chunk.stream {
                        OutputStream::Stdout => StreamType::Stdout,
                        OutputStream::Stderr => StreamType::Stderr,
                    },
                    data: chunk.data,
                }));
            }
        }
        Ok(())
    }

    fn invalid_output(&self, reason: &str) -> ExecutionManagerError {
        ExecutionManagerError::Internal(format!(
            "A3S OCI process output for {} is invalid: {reason}",
            self.target.process_id
        ))
    }

    fn queue_terminal_events(&mut self) -> ExecutionManagerResult<()> {
        if self.exit_queued || !self.output_complete() {
            return Ok(());
        }
        let Some(status) = self.status.as_ref() else {
            return Ok(());
        };
        if self.watchdog.load(Ordering::SeqCst) == WATCHDOG_TIMED_OUT && !self.timeout_notice_queued
        {
            self.pending.push_back(ExecEvent::Chunk(ExecChunk {
                stream: if self.terminal {
                    StreamType::Stdout
                } else {
                    StreamType::Stderr
                },
                data: b"\nProcess killed: timeout exceeded".to_vec(),
            }));
            self.timeout_notice_queued = true;
        }
        self.pending.push_back(ExecEvent::Exit(ExecExit {
            exit_code: exit_code(status)?,
            oom_killed: status.oom_killed,
        }));
        self.exit_queued = true;
        Ok(())
    }
}

#[async_trait]
impl ExecutionProcessStream for OciProcessStream {
    fn input(&self) -> Arc<dyn ExecutionProcessInput> {
        self.input.clone()
    }

    async fn next_event(&mut self) -> ExecutionManagerResult<Option<ExecEvent>> {
        if self.done {
            return Ok(None);
        }
        if let Some(event) = self.pop_pending() {
            return Ok(Some(event));
        }

        loop {
            let chunks = self
                .client
                .read_output(ReadOutputRequest {
                    process: self.target.clone(),
                    after_sequence: self.cursor,
                    max_bytes: OUTPUT_POLL_BYTES,
                    wait_timeout_ms: Some(self.output_wait_ms()),
                })
                .await
                .map_err(|error| sdk_error("read process output", error))?;
            let was_empty = chunks.is_empty();
            self.accept_output(chunks)?;
            if let Some(event) = self.pop_pending() {
                return Ok(Some(event));
            }

            if self.status.is_none() {
                match self
                    .client
                    .wait_process(WaitProcessRequest {
                        process: self.target.clone(),
                        timeout_ms: Some(0),
                    })
                    .await
                {
                    Ok(status) => self.status = Some(status),
                    Err(error) if error.code == ErrorCode::DeadlineExceeded => {}
                    Err(error) => return Err(sdk_error("wait process", error)),
                }
            }

            if self.status.is_none() {
                match self.watchdog.load(Ordering::SeqCst) {
                    WATCHDOG_FAILED => {
                        return Err(ExecutionManagerError::Unavailable(format!(
                            "A3S OCI exec timeout watchdog failed for {}",
                            self.target.process_id
                        )))
                    }
                    WATCHDOG_WAITING
                        if self
                            .deadline
                            .is_some_and(|deadline| Instant::now() >= deadline) =>
                    {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                        continue;
                    }
                    _ => {}
                }
            }

            if self.status.is_some() && !self.output_complete() && was_empty {
                self.empty_polls_after_exit = self.empty_polls_after_exit.saturating_add(1);
                if self.empty_polls_after_exit >= MAX_EMPTY_POLLS_AFTER_EXIT {
                    return Err(self.invalid_output(
                        "process exited before every captured stream produced EOF",
                    ));
                }
            } else if !was_empty {
                self.empty_polls_after_exit = 0;
            }

            self.queue_terminal_events()?;
            if let Some(event) = self.pop_pending() {
                return Ok(Some(event));
            }
            if was_empty && self.status.is_none() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    }
}

fn retain_bounded(destination: &mut Vec<u8>, data: &[u8], truncated: &mut bool) {
    let remaining = MAX_ONE_SHOT_OUTPUT_BYTES.saturating_sub(destination.len());
    if data.len() > remaining {
        *truncated = true;
    }
    destination.extend_from_slice(&data[..data.len().min(remaining)]);
}

#[cfg(test)]
mod tests {
    use super::parse_process_user;

    #[test]
    fn process_users_preserve_legacy_numeric_fallbacks() {
        assert_eq!(parse_process_user(None).unwrap(), (0, 0));
        assert_eq!(parse_process_user(Some("root")).unwrap(), (0, 0));
        assert_eq!(parse_process_user(Some("1000")).unwrap(), (1000, 0));
        assert_eq!(parse_process_user(Some("1000:1001")).unwrap(), (1000, 1001));
        assert!(parse_process_user(Some("alice")).is_err());
    }
}
