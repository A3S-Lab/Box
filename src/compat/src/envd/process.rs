//! Generation-scoped E2B Process service backed by A3S execution sessions.

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use a3s_box_core::pty::PtyRequest;
use a3s_box_core::{
    ExecEvent, ExecRequest, ExecutionGeneration, ExecutionId, ExecutionManagerError,
    ExecutionProcess, ExecutionProcessInput, ExecutionProcessSignal, ExecutionSessionManager,
    StreamType,
};
use axum::body::Body;
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, Method, Request, Response};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio::sync::{broadcast, RwLock};

use super::connect::{
    data_frame, decode_client_stream, decode_stream, decode_unary, stream_encoding, stream_error,
    stream_response, stream_unary_ok, success_end_stream_frame, unary_ok, ConnectEncoding,
    ConnectFailure,
};

const DEFAULT_PROCESS_TIMEOUT_MS: u64 = 60_000;
const MAX_PROCESSES_PER_GENERATION: usize = 1024;
const PROCESS_EVENT_CAPACITY: usize = 4096;
const PROCESS_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT_HEADER: &str = "connect-timeout-ms";
const DEFAULT_PROCESS_USER: &str = "user";

#[derive(Clone)]
pub(super) struct ProcessBroker {
    sessions: Arc<dyn ExecutionSessionManager>,
    registry: Arc<RwLock<ProcessRegistry>>,
    next_pid: Arc<AtomicU32>,
}

impl ProcessBroker {
    pub(super) fn new(sessions: Arc<dyn ExecutionSessionManager>) -> Self {
        Self {
            sessions,
            registry: Arc::new(RwLock::new(ProcessRegistry::default())),
            next_pid: Arc::new(AtomicU32::new(1000)),
        }
    }

    pub(super) async fn handle(
        &self,
        request: Request<Body>,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> Response<Body> {
        let path = request.uri().path().to_string();
        if request.method() != Method::POST {
            return ConnectFailure::invalid_argument("Connect procedures require POST")
                .unary_response();
        }
        let key = ProcessGeneration::new(execution_id, generation);
        self.drop_stale_generations(&key).await;
        match path.as_str() {
            "/process.Process/Start" => self.start(request, key).await,
            "/process.Process/Connect" => self.connect(request, &key).await,
            "/process.Process/List" => self.list(request, &key).await,
            "/process.Process/SendInput" => self.send_input(request, &key).await,
            "/process.Process/CloseStdin" => self.close_stdin(request, &key).await,
            "/process.Process/SendSignal" => self.send_signal(request, &key).await,
            "/process.Process/Update" => self.update(request, &key).await,
            "/process.Process/StreamInput" => self.stream_input(request, &key).await,
            _ => ConnectFailure::not_found("Process procedure not found").unary_response(),
        }
    }

    async fn start(&self, request: Request<Body>, key: ProcessGeneration) -> Response<Body> {
        let encoding = match stream_encoding(&request) {
            Ok(encoding) => encoding,
            Err(error) => return error.unary_response(),
        };
        let user = match process_user(request.headers()) {
            Ok(user) => user,
            Err(error) => return stream_error(&error, encoding),
        };
        let timeout_ns = match process_timeout_ns(request.headers()) {
            Ok(timeout) => timeout,
            Err(error) => return stream_error(&error, encoding),
        };
        let request: StartRequest = match decode_stream(request, encoding).await {
            Ok(request) => request,
            Err(error) => return stream_error(&error, encoding),
        };
        let config = match request.process {
            Some(config) => config,
            None => {
                return stream_error(
                    &ConnectFailure::invalid_argument("StartRequest.process is required"),
                    encoding,
                )
            }
        };
        if let Err(error) = config.validate() {
            return stream_error(&error, encoding);
        }
        let tag = match normalize_tag(request.tag) {
            Ok(tag) => tag,
            Err(error) => return stream_error(&error, encoding),
        };
        let process = if let Some(pty) = request.pty {
            let size = match pty.validated_size() {
                Ok(size) => size,
                Err(error) => return stream_error(&error, encoding),
            };
            self.sessions
                .start_pty(
                    &key.execution_id,
                    key.generation(),
                    PtyRequest {
                        cmd: config.argv(),
                        env: config.environment(),
                        working_dir: config.cwd.clone(),
                        rootfs: None,
                        user,
                        cols: size.cols,
                        rows: size.rows,
                    },
                )
                .await
                .map(|process| (process, true))
        } else {
            self.sessions
                .start_process(
                    &key.execution_id,
                    key.generation(),
                    ExecRequest {
                        request_id: None,
                        cmd: config.argv(),
                        timeout_ns,
                        env: config.environment(),
                        working_dir: config.cwd.clone(),
                        rootfs: None,
                        stdin: None,
                        stdin_streaming: request.stdin.unwrap_or(false),
                        user,
                        streaming: true,
                    },
                )
                .await
                .map(|process| (process, false))
        };
        let (process, pty) = match process {
            Ok(process) => process,
            Err(error) => return stream_error(&manager_failure(error), encoding),
        };
        match self.register(key, config, tag, pty, process).await {
            Ok((pid, subscription)) => process_stream(pid, subscription, encoding),
            Err(error) => stream_error(&error, encoding),
        }
    }

    async fn connect(&self, request: Request<Body>, key: &ProcessGeneration) -> Response<Body> {
        let encoding = match stream_encoding(&request) {
            Ok(encoding) => encoding,
            Err(error) => return error.unary_response(),
        };
        let request: ConnectRequest = match decode_stream(request, encoding).await {
            Ok(request) => request,
            Err(error) => return stream_error(&error, encoding),
        };
        let entry = match self.entry(key, &request.process).await {
            Ok(entry) => entry,
            Err(error) => return stream_error(&error, encoding),
        };
        process_stream(entry.pid, entry.subscribe(), encoding)
    }

    async fn list(&self, request: Request<Body>, key: &ProcessGeneration) -> Response<Body> {
        let (_, encoding) = match decode_unary::<EmptyRequest>(request).await {
            Ok(decoded) => decoded,
            Err(error) => return error.unary_response(),
        };
        let mut processes = self
            .registry
            .read()
            .await
            .generations
            .get(key)
            .map(|entries| {
                entries
                    .values()
                    .filter(|entry| entry.is_running())
                    .map(|entry| ProcessInfo {
                        config: entry.config.clone(),
                        pid: entry.pid,
                        tag: entry.tag.clone(),
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        processes.sort_by_key(|process| process.pid);
        unary_ok(&ListResponse { processes }, encoding)
    }

    async fn send_input(&self, request: Request<Body>, key: &ProcessGeneration) -> Response<Body> {
        let (request, encoding): (SendInputRequest, _) = match decode_unary(request).await {
            Ok(decoded) => decoded,
            Err(error) => return error.unary_response(),
        };
        let entry = match self.entry(key, &request.process).await {
            Ok(entry) => entry,
            Err(error) => return error.unary_response(),
        };
        match write_process_input(&entry, request.input, "SendInputRequest.input").await {
            Ok(()) => unary_ok(&EmptyResponse {}, encoding),
            Err(error) => error.unary_response(),
        }
    }

    async fn stream_input(
        &self,
        request: Request<Body>,
        key: &ProcessGeneration,
    ) -> Response<Body> {
        let encoding = match stream_encoding(&request) {
            Ok(encoding) => encoding,
            Err(error) => return error.unary_response(),
        };
        let mut stream = decode_client_stream::<StreamInputRequest>(request, encoding);
        let mut entry = None;
        loop {
            let request = match stream.next().await {
                Ok(Some(request)) => request,
                Ok(None) => return stream_unary_ok(&EmptyResponse {}, encoding),
                Err(error) => return stream_error(&error, encoding),
            };
            match request.into_event() {
                Ok(StreamInputEvent::Start(selector)) => {
                    entry = match self.entry(key, &selector).await {
                        Ok(entry) => Some(entry),
                        Err(error) => return stream_error(&error, encoding),
                    };
                }
                Ok(StreamInputEvent::Data(input)) => {
                    let Some(entry) = entry.as_ref() else {
                        return stream_error(
                            &ConnectFailure::failed_precondition(
                                "StreamInput data requires a preceding start event",
                            ),
                            encoding,
                        );
                    };
                    if let Err(error) =
                        write_process_input(entry, input, "StreamInputRequest.data.input").await
                    {
                        return stream_error(&error, encoding);
                    }
                }
                Ok(StreamInputEvent::KeepAlive) => {}
                Err(error) => return stream_error(&error, encoding),
            }
        }
    }

    async fn close_stdin(&self, request: Request<Body>, key: &ProcessGeneration) -> Response<Body> {
        let (request, encoding): (CloseStdinRequest, _) = match decode_unary(request).await {
            Ok(decoded) => decoded,
            Err(error) => return error.unary_response(),
        };
        let entry = match self.entry(key, &request.process).await {
            Ok(entry) => entry,
            Err(error) => return error.unary_response(),
        };
        if entry.pty {
            return ConnectFailure::failed_precondition(
                "CloseStdin is valid only for non-PTY processes",
            )
            .unary_response();
        }
        match entry.input.close_stdin().await {
            Ok(()) => unary_ok(&EmptyResponse {}, encoding),
            Err(error) => manager_failure(error).unary_response(),
        }
    }

    async fn send_signal(&self, request: Request<Body>, key: &ProcessGeneration) -> Response<Body> {
        let (request, encoding): (SendSignalRequest, _) = match decode_unary(request).await {
            Ok(decoded) => decoded,
            Err(error) => return error.unary_response(),
        };
        let entry = match self.entry(key, &request.process).await {
            Ok(entry) => entry,
            Err(error) => return error.unary_response(),
        };
        let Some(signal) = request.process_signal() else {
            return ConnectFailure::unimplemented("invalid process signal").unary_response();
        };
        match entry.input.send_signal(signal).await {
            Ok(()) => unary_ok(&EmptyResponse {}, encoding),
            Err(error) => manager_failure(error).unary_response(),
        }
    }

    async fn update(&self, request: Request<Body>, key: &ProcessGeneration) -> Response<Body> {
        let (request, encoding): (UpdateRequest, _) = match decode_unary(request).await {
            Ok(decoded) => decoded,
            Err(error) => return error.unary_response(),
        };
        let entry = match self.entry(key, &request.process).await {
            Ok(entry) => entry,
            Err(error) => return error.unary_response(),
        };
        if !entry.pty {
            return ConnectFailure::failed_precondition("UpdateRequest.pty requires a PTY process")
                .unary_response();
        }
        let size = match request.pty.and_then(|pty| pty.size) {
            Some(size) => match size.validate() {
                Ok(size) => size,
                Err(error) => return error.unary_response(),
            },
            None => {
                return ConnectFailure::invalid_argument("UpdateRequest.pty.size is required")
                    .unary_response()
            }
        };
        match entry.input.resize_pty(size.cols, size.rows).await {
            Ok(()) => unary_ok(&EmptyResponse {}, encoding),
            Err(error) => manager_failure(error).unary_response(),
        }
    }

    async fn register(
        &self,
        key: ProcessGeneration,
        config: ProcessConfig,
        tag: Option<String>,
        pty: bool,
        process: ExecutionProcess,
    ) -> Result<(u32, ProcessSubscription), ConnectFailure> {
        let input = process.input();
        let mut registry = self.registry.write().await;
        let entries = registry.generations.entry(key.clone()).or_default();
        if entries.len() >= MAX_PROCESSES_PER_GENERATION {
            drop(registry);
            let _ = input.cancel().await;
            return Err(ConnectFailure::resource_exhausted(format!(
                "process limit of {MAX_PROCESSES_PER_GENERATION} reached"
            )));
        }
        let pid = self.allocate_pid(entries)?;
        let entry = Arc::new(ProcessEntry::new(pid, config, tag, pty, input));
        let subscription = entry.subscribe();
        entries.insert(pid, entry.clone());
        drop(registry);

        let registry = self.registry.clone();
        tokio::spawn(async move {
            pump_process(process, entry.clone()).await;
            remove_process(&registry, &key, pid, &entry).await;
        });
        Ok((pid, subscription))
    }

    fn allocate_pid(
        &self,
        entries: &HashMap<u32, Arc<ProcessEntry>>,
    ) -> Result<u32, ConnectFailure> {
        for _ in 0..=MAX_PROCESSES_PER_GENERATION {
            let candidate = self.next_pid.fetch_add(1, Ordering::Relaxed);
            if candidate != 0 && !entries.contains_key(&candidate) {
                return Ok(candidate);
            }
        }
        Err(ConnectFailure::resource_exhausted(
            "unable to allocate a synthetic process ID",
        ))
    }

    async fn entry(
        &self,
        key: &ProcessGeneration,
        selector: &Option<ProcessSelector>,
    ) -> Result<Arc<ProcessEntry>, ConnectFailure> {
        let selector = selector
            .as_ref()
            .ok_or_else(|| ConnectFailure::invalid_argument("process selector is required"))?;
        let registry = self.registry.read().await;
        let entries = registry
            .generations
            .get(key)
            .ok_or_else(|| ConnectFailure::not_found("process not found"))?;
        match selector.selection()? {
            Selection::Pid(pid) => entries
                .get(&pid)
                .filter(|entry| entry.is_running())
                .cloned()
                .ok_or_else(|| ConnectFailure::not_found(format!("process {pid} not found"))),
            Selection::Tag(tag) => entries
                .values()
                .filter(|entry| entry.is_running() && entry.tag.as_deref() == Some(tag))
                .min_by_key(|entry| entry.pid)
                .cloned()
                .ok_or_else(|| ConnectFailure::not_found(format!("process tag {tag:?} not found"))),
        }
    }

    async fn drop_stale_generations(&self, current: &ProcessGeneration) {
        self.registry.write().await.generations.retain(|key, _| {
            key.execution_id != current.execution_id || key.generation == current.generation
        });
    }
}

#[derive(Default)]
struct ProcessRegistry {
    generations: HashMap<ProcessGeneration, HashMap<u32, Arc<ProcessEntry>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProcessGeneration {
    execution_id: ExecutionId,
    generation: ExecutionGeneration,
}

impl ProcessGeneration {
    fn new(execution_id: &ExecutionId, generation: ExecutionGeneration) -> Self {
        Self {
            execution_id: execution_id.clone(),
            generation,
        }
    }

    fn generation(&self) -> ExecutionGeneration {
        self.generation
    }
}

impl Hash for ProcessGeneration {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.execution_id.hash(state);
        self.generation.get().hash(state);
    }
}

struct ProcessEntry {
    pid: u32,
    config: ProcessConfig,
    tag: Option<String>,
    pty: bool,
    input: Arc<dyn ExecutionProcessInput>,
    events: broadcast::Sender<BrokerEvent>,
    terminal: std::sync::Mutex<Option<BrokerEvent>>,
}

impl ProcessEntry {
    fn new(
        pid: u32,
        config: ProcessConfig,
        tag: Option<String>,
        pty: bool,
        input: Arc<dyn ExecutionProcessInput>,
    ) -> Self {
        let (events, _) = broadcast::channel(PROCESS_EVENT_CAPACITY);
        Self {
            pid,
            config,
            tag,
            pty,
            input,
            events,
            terminal: std::sync::Mutex::new(None),
        }
    }

    fn subscribe(&self) -> ProcessSubscription {
        let receiver = self.events.subscribe();
        let terminal = self
            .terminal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        ProcessSubscription { receiver, terminal }
    }

    fn is_running(&self) -> bool {
        self.terminal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_none()
    }

    fn publish(&self, event: BrokerEvent) {
        let _ = self.events.send(event);
    }

    fn finish(&self, event: BrokerEvent) {
        let mut terminal = self
            .terminal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if terminal.is_none() {
            *terminal = Some(event.clone());
            let _ = self.events.send(event);
        }
    }
}

struct ProcessSubscription {
    receiver: broadcast::Receiver<BrokerEvent>,
    terminal: Option<BrokerEvent>,
}

impl ProcessSubscription {
    async fn next(&mut self) -> Result<BrokerEvent, ConnectFailure> {
        if let Some(event) = self.terminal.take() {
            return Ok(event);
        }
        tokio::select! {
            event = self.receiver.recv() => match event {
                Ok(event) => Ok(event),
                Err(broadcast::error::RecvError::Lagged(count)) => Err(
                    ConnectFailure::internal(format!(
                        "process subscriber fell behind by {count} events"
                    )),
                ),
                Err(broadcast::error::RecvError::Closed) => Err(
                    ConnectFailure::unavailable(
                        "process event stream closed before an exit event",
                    ),
                ),
            },
            () = tokio::time::sleep(PROCESS_KEEPALIVE_INTERVAL) => Ok(BrokerEvent::KeepAlive),
        }
    }
}

#[derive(Clone)]
enum BrokerEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Pty(Vec<u8>),
    KeepAlive,
    End { exit_code: i32 },
    Failure(ConnectFailure),
}

impl BrokerEvent {
    fn is_terminal(&self) -> bool {
        matches!(self, Self::End { .. } | Self::Failure(_))
    }

    fn response(&self) -> Option<ProcessResponse> {
        match self {
            Self::Stdout(data) => Some(ProcessResponse::data(Some(data.clone()), None, None)),
            Self::Stderr(data) => Some(ProcessResponse::data(None, Some(data.clone()), None)),
            Self::Pty(data) => Some(ProcessResponse::data(None, None, Some(data.clone()))),
            Self::KeepAlive => Some(ProcessResponse::keepalive()),
            Self::End { exit_code } => Some(ProcessResponse::end(*exit_code)),
            Self::Failure(_) => None,
        }
    }
}

async fn pump_process(mut process: ExecutionProcess, entry: Arc<ProcessEntry>) {
    loop {
        match process.next_event().await {
            Ok(Some(ExecEvent::Chunk(chunk))) => {
                let event = if entry.pty {
                    BrokerEvent::Pty(chunk.data)
                } else {
                    match chunk.stream {
                        StreamType::Stdout => BrokerEvent::Stdout(chunk.data),
                        StreamType::Stderr => BrokerEvent::Stderr(chunk.data),
                    }
                };
                entry.publish(event);
            }
            Ok(Some(ExecEvent::FlushAck)) => {}
            Ok(Some(ExecEvent::Exit(exit))) => {
                entry.finish(BrokerEvent::End {
                    exit_code: exit.exit_code,
                });
                return;
            }
            Ok(None) => {
                entry.finish(BrokerEvent::Failure(ConnectFailure::unavailable(
                    "execution stream closed before an exit event",
                )));
                return;
            }
            Err(error) => {
                entry.finish(BrokerEvent::Failure(manager_failure(error)));
                return;
            }
        }
    }
}

async fn remove_process(
    registry: &RwLock<ProcessRegistry>,
    key: &ProcessGeneration,
    pid: u32,
    expected: &Arc<ProcessEntry>,
) {
    let mut registry = registry.write().await;
    let remove_generation = if let Some(entries) = registry.generations.get_mut(key) {
        if entries
            .get(&pid)
            .is_some_and(|entry| Arc::ptr_eq(entry, expected))
        {
            entries.remove(&pid);
        }
        entries.is_empty()
    } else {
        false
    };
    if remove_generation {
        registry.generations.remove(key);
    }
}

fn process_stream(
    pid: u32,
    mut subscription: ProcessSubscription,
    encoding: ConnectEncoding,
) -> Response<Body> {
    let (mut sender, body) = Body::channel();
    tokio::spawn(async move {
        let start = match data_frame(&ProcessResponse::start(pid), encoding) {
            Ok(start) => start,
            Err(error) => {
                let _ = sender.send_data(error.end_stream_frame().into()).await;
                return;
            }
        };
        if sender.send_data(start).await.is_err() {
            return;
        }
        loop {
            let event = match subscription.next().await {
                Ok(event) => event,
                Err(error) => {
                    let _ = sender.send_data(error.end_stream_frame().into()).await;
                    return;
                }
            };
            if let BrokerEvent::Failure(error) = &event {
                let _ = sender.send_data(error.end_stream_frame().into()).await;
                return;
            }
            let terminal = event.is_terminal();
            if let Some(value) = event.response() {
                match data_frame(&value, encoding) {
                    Ok(frame) => {
                        if sender.send_data(frame).await.is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send_data(error.end_stream_frame().into()).await;
                        return;
                    }
                }
            }
            if terminal {
                let _ = sender.send_data(success_end_stream_frame()).await;
                return;
            }
        }
    });
    stream_response(body, encoding)
}

async fn write_process_input(
    entry: &ProcessEntry,
    input: Option<ProcessInput>,
    field: &'static str,
) -> Result<(), ConnectFailure> {
    let input = input.and_then(ProcessInput::into_input).ok_or_else(|| {
        ConnectFailure::invalid_argument(format!(
            "{field} must contain exactly one stdin or PTY value"
        ))
    })?;
    if input.is_pty() != entry.pty {
        return Err(ConnectFailure::failed_precondition(if entry.pty {
            "PTY processes require PTY input"
        } else {
            "non-PTY processes require stdin input"
        }));
    }
    entry
        .input
        .write_stdin(input.value())
        .await
        .map_err(manager_failure)
}

fn manager_failure(error: ExecutionManagerError) -> ConnectFailure {
    match error {
        ExecutionManagerError::InvalidRequest(message) => ConnectFailure::invalid_argument(message),
        ExecutionManagerError::NotFound(execution_id) => {
            ConnectFailure::not_found(format!("execution {execution_id} not found"))
        }
        ExecutionManagerError::Conflict { message, .. } => {
            ConnectFailure::failed_precondition(message)
        }
        ExecutionManagerError::Unavailable(message) => ConnectFailure::unavailable(message),
        ExecutionManagerError::Internal(message) => ConnectFailure::internal(message),
    }
}

fn process_timeout_ns(headers: &HeaderMap) -> Result<u64, ConnectFailure> {
    let timeout_ms = match headers.get(CONNECT_TIMEOUT_HEADER) {
        Some(value) => value
            .to_str()
            .map_err(|_| ConnectFailure::invalid_argument("Connect timeout is not UTF-8"))?
            .parse::<u64>()
            .map_err(|_| {
                ConnectFailure::invalid_argument("Connect timeout must be milliseconds")
            })?,
        None => DEFAULT_PROCESS_TIMEOUT_MS,
    };
    timeout_ms.checked_mul(1_000_000).ok_or_else(|| {
        ConnectFailure::invalid_argument("Connect timeout is too large to represent")
    })
}

pub(super) fn process_user(headers: &HeaderMap) -> Result<Option<String>, ConnectFailure> {
    let Some(value) = headers.get(AUTHORIZATION) else {
        // envd 0.4.0 and newer applies the user selected during /init when a
        // request omits Basic authentication. The pinned A3S E2B runtime uses
        // the upstream SDK default, `user`; applying it here preserves that
        // behavior while the host-side broker owns the Process service.
        return Ok(Some(DEFAULT_PROCESS_USER.to_string()));
    };
    let value = value
        .to_str()
        .map_err(|_| ConnectFailure::invalid_argument("Authorization is not UTF-8"))?;
    let (scheme, encoded) = value.split_once(' ').ok_or_else(|| {
        ConnectFailure::invalid_argument("Authorization must use Basic user selection")
    })?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return Err(ConnectFailure::invalid_argument(
            "Authorization must use Basic user selection",
        ));
    }
    let decoded = STANDARD
        .decode(encoded)
        .map_err(|_| ConnectFailure::invalid_argument("invalid Basic Authorization payload"))?;
    let decoded = String::from_utf8(decoded)
        .map_err(|_| ConnectFailure::invalid_argument("Basic user is not UTF-8"))?;
    let (user, password) = decoded.split_once(':').ok_or_else(|| {
        ConnectFailure::invalid_argument("Basic user selection must contain a colon")
    })?;
    if user.is_empty() || user.len() > 128 || user.contains('\0') || !password.is_empty() {
        return Err(ConnectFailure::invalid_argument(
            "Basic user selection is invalid",
        ));
    }
    Ok(Some(user.to_string()))
}

fn normalize_tag(tag: Option<String>) -> Result<Option<String>, ConnectFailure> {
    match tag {
        Some(tag) if tag.trim().is_empty() || tag.len() > 128 || tag.contains('\0') => Err(
            ConnectFailure::invalid_argument("process tag must be 1 to 128 safe characters"),
        ),
        tag => Ok(tag),
    }
}

#[derive(Clone, PartialEq, ::prost::Message, Serialize)]
struct ProcessResponse {
    #[prost(message, optional, tag = "1")]
    #[serde(skip_serializing_if = "Option::is_none")]
    event: Option<ProcessEvent>,
}

impl ProcessResponse {
    fn start(pid: u32) -> Self {
        Self {
            event: Some(ProcessEvent {
                start: Some(ProcessStartEvent { pid }),
                ..ProcessEvent::default()
            }),
        }
    }

    fn data(stdout: Option<Vec<u8>>, stderr: Option<Vec<u8>>, pty: Option<Vec<u8>>) -> Self {
        Self {
            event: Some(ProcessEvent {
                data: Some(ProcessDataEvent {
                    stdout,
                    stderr,
                    pty,
                }),
                ..ProcessEvent::default()
            }),
        }
    }

    fn end(exit_code: i32) -> Self {
        Self {
            event: Some(ProcessEvent {
                end: Some(ProcessEndEvent {
                    exit_code,
                    exited: true,
                    status: "exited".to_string(),
                    error: None,
                }),
                ..ProcessEvent::default()
            }),
        }
    }

    fn keepalive() -> Self {
        Self {
            event: Some(ProcessEvent {
                keepalive: Some(ProcessKeepAlive {}),
                ..ProcessEvent::default()
            }),
        }
    }
}

#[derive(Clone, PartialEq, ::prost::Message, Serialize)]
struct ProcessEvent {
    #[prost(message, optional, tag = "1")]
    #[serde(skip_serializing_if = "Option::is_none")]
    start: Option<ProcessStartEvent>,
    #[prost(message, optional, tag = "2")]
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<ProcessDataEvent>,
    #[prost(message, optional, tag = "3")]
    #[serde(skip_serializing_if = "Option::is_none")]
    end: Option<ProcessEndEvent>,
    #[prost(message, optional, tag = "4")]
    #[serde(skip_serializing_if = "Option::is_none")]
    keepalive: Option<ProcessKeepAlive>,
}

#[derive(Clone, PartialEq, ::prost::Message, Serialize)]
struct ProcessStartEvent {
    #[prost(uint32, tag = "1")]
    pid: u32,
}

#[derive(Clone, PartialEq, ::prost::Message, Serialize)]
struct ProcessDataEvent {
    #[prost(bytes = "vec", optional, tag = "1")]
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_bytes"
    )]
    stdout: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "2")]
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_bytes"
    )]
    stderr: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "3")]
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_bytes"
    )]
    pty: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, ::prost::Message, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProcessEndEvent {
    #[prost(sint32, tag = "1")]
    exit_code: i32,
    #[prost(bool, tag = "2")]
    exited: bool,
    #[prost(string, tag = "3")]
    status: String,
    #[prost(string, optional, tag = "4")]
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Clone, PartialEq, ::prost::Message, Serialize)]
struct ProcessKeepAlive {}

#[derive(Clone, PartialEq, ::prost::Message, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProcessConfig {
    #[prost(string, tag = "1")]
    cmd: String,
    #[prost(string, repeated, tag = "2")]
    #[serde(default)]
    args: Vec<String>,
    #[prost(btree_map = "string, string", tag = "3")]
    #[serde(default)]
    envs: BTreeMap<String, String>,
    #[prost(string, optional, tag = "4")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
}

impl ProcessConfig {
    fn validate(&self) -> Result<(), ConnectFailure> {
        if self.cmd.trim().is_empty() || self.cmd.contains('\0') {
            return Err(ConnectFailure::invalid_argument(
                "process command cannot be empty or contain NUL",
            ));
        }
        if self.args.iter().any(|argument| argument.contains('\0')) {
            return Err(ConnectFailure::invalid_argument(
                "process arguments cannot contain NUL",
            ));
        }
        if self.envs.iter().any(|(key, value)| {
            key.is_empty() || key.contains('=') || key.contains('\0') || value.contains('\0')
        }) {
            return Err(ConnectFailure::invalid_argument(
                "process environment contains an invalid name or value",
            ));
        }
        if self.cwd.as_deref().is_some_and(|cwd| cwd.contains('\0')) {
            return Err(ConnectFailure::invalid_argument(
                "process working directory cannot contain NUL",
            ));
        }
        Ok(())
    }

    fn argv(&self) -> Vec<String> {
        std::iter::once(self.cmd.clone())
            .chain(self.args.iter().cloned())
            .collect()
    }

    fn environment(&self) -> Vec<String> {
        self.envs
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect()
    }
}

#[derive(Clone, PartialEq, ::prost::Message, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartRequest {
    #[prost(message, optional, tag = "1")]
    #[serde(default)]
    process: Option<ProcessConfig>,
    #[prost(message, optional, tag = "2")]
    #[serde(default)]
    pty: Option<Pty>,
    #[prost(string, optional, tag = "3")]
    #[serde(default)]
    tag: Option<String>,
    #[prost(bool, optional, tag = "4")]
    #[serde(default)]
    stdin: Option<bool>,
}

#[derive(Clone, PartialEq, ::prost::Message, Deserialize)]
struct Pty {
    #[prost(message, optional, tag = "1")]
    #[serde(default)]
    size: Option<PtySize>,
}

impl Pty {
    fn validated_size(self) -> Result<ValidatedPtySize, ConnectFailure> {
        self.size
            .ok_or_else(|| ConnectFailure::invalid_argument("PTY.size is required"))?
            .validate()
    }
}

#[derive(Clone, PartialEq, ::prost::Message, Deserialize)]
struct PtySize {
    #[prost(uint32, tag = "1")]
    cols: u32,
    #[prost(uint32, tag = "2")]
    rows: u32,
}

impl PtySize {
    fn validate(self) -> Result<ValidatedPtySize, ConnectFailure> {
        let cols = u16::try_from(self.cols)
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| ConnectFailure::invalid_argument("PTY columns must be 1 to 65535"))?;
        let rows = u16::try_from(self.rows)
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| ConnectFailure::invalid_argument("PTY rows must be 1 to 65535"))?;
        Ok(ValidatedPtySize { cols, rows })
    }
}

struct ValidatedPtySize {
    cols: u16,
    rows: u16,
}

#[derive(Clone, PartialEq, ::prost::Message, Deserialize)]
struct EmptyRequest {}

#[derive(Clone, PartialEq, ::prost::Message, Serialize)]
struct EmptyResponse {}

#[derive(Clone, PartialEq, ::prost::Message, Serialize)]
struct ListResponse {
    #[prost(message, repeated, tag = "1")]
    processes: Vec<ProcessInfo>,
}

#[derive(Clone, PartialEq, ::prost::Message, Serialize)]
struct ProcessInfo {
    #[prost(message, required, tag = "1")]
    config: ProcessConfig,
    #[prost(uint32, tag = "2")]
    pid: u32,
    #[prost(string, optional, tag = "3")]
    #[serde(skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
}

#[derive(Clone, PartialEq, ::prost::Message, Deserialize)]
struct ConnectRequest {
    #[prost(message, optional, tag = "1")]
    #[serde(default)]
    process: Option<ProcessSelector>,
}

#[derive(Clone, PartialEq, ::prost::Message, Deserialize)]
struct ProcessSelector {
    #[prost(uint32, optional, tag = "1")]
    #[serde(default)]
    pid: Option<u32>,
    #[prost(string, optional, tag = "2")]
    #[serde(default)]
    tag: Option<String>,
}

impl ProcessSelector {
    fn selection(&self) -> Result<Selection<'_>, ConnectFailure> {
        match (self.pid, self.tag.as_deref()) {
            (Some(pid), None) if pid > 0 => Ok(Selection::Pid(pid)),
            (None, Some(tag)) if !tag.is_empty() => Ok(Selection::Tag(tag)),
            _ => Err(ConnectFailure::invalid_argument(
                "process selector must contain exactly one non-empty pid or tag",
            )),
        }
    }
}

enum Selection<'a> {
    Pid(u32),
    Tag(&'a str),
}

#[derive(Clone, PartialEq, ::prost::Message, Deserialize)]
struct SendInputRequest {
    #[prost(message, optional, tag = "1")]
    #[serde(default)]
    process: Option<ProcessSelector>,
    #[prost(message, optional, tag = "2")]
    #[serde(default)]
    input: Option<ProcessInput>,
}

#[derive(Clone, PartialEq, ::prost::Message, Deserialize)]
struct StreamInputRequest {
    #[prost(message, optional, tag = "1")]
    #[serde(default)]
    start: Option<StreamInputStart>,
    #[prost(message, optional, tag = "2")]
    #[serde(default)]
    data: Option<StreamInputData>,
    #[prost(message, optional, tag = "3")]
    #[serde(default)]
    keepalive: Option<EmptyRequest>,
}

impl StreamInputRequest {
    fn into_event(self) -> Result<StreamInputEvent, ConnectFailure> {
        match (self.start, self.data, self.keepalive) {
            (Some(start), None, None) => Ok(StreamInputEvent::Start(start.process)),
            (None, Some(data), None) => Ok(StreamInputEvent::Data(data.input)),
            (None, None, Some(_)) => Ok(StreamInputEvent::KeepAlive),
            _ => Err(ConnectFailure::invalid_argument(
                "StreamInputRequest must contain exactly one start, data, or keepalive event",
            )),
        }
    }
}

#[derive(Clone, PartialEq, ::prost::Message, Deserialize)]
struct StreamInputStart {
    #[prost(message, optional, tag = "1")]
    #[serde(default)]
    process: Option<ProcessSelector>,
}

#[derive(Clone, PartialEq, ::prost::Message, Deserialize)]
struct StreamInputData {
    #[prost(message, optional, tag = "2")]
    #[serde(default)]
    input: Option<ProcessInput>,
}

enum StreamInputEvent {
    Start(Option<ProcessSelector>),
    Data(Option<ProcessInput>),
    KeepAlive,
}

#[derive(Clone, PartialEq, ::prost::Message, Deserialize)]
struct ProcessInput {
    #[prost(bytes = "vec", optional, tag = "1")]
    #[serde(default, deserialize_with = "deserialize_optional_bytes")]
    stdin: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "2")]
    #[serde(default, deserialize_with = "deserialize_optional_bytes")]
    pty: Option<Vec<u8>>,
}

impl ProcessInput {
    fn into_input(self) -> Option<DecodedInput> {
        match (self.stdin, self.pty) {
            (Some(data), None) => Some(DecodedInput::Stdin(data)),
            (None, Some(data)) => Some(DecodedInput::Pty(data)),
            _ => None,
        }
    }
}

enum DecodedInput {
    Stdin(Vec<u8>),
    Pty(Vec<u8>),
}

impl DecodedInput {
    fn is_pty(&self) -> bool {
        matches!(self, Self::Pty(_))
    }

    fn value(&self) -> &[u8] {
        match self {
            Self::Stdin(value) | Self::Pty(value) => value,
        }
    }
}

#[derive(Clone, PartialEq, ::prost::Message, Deserialize)]
struct CloseStdinRequest {
    #[prost(message, optional, tag = "1")]
    #[serde(default)]
    process: Option<ProcessSelector>,
}

#[derive(Clone, PartialEq, ::prost::Message, Deserialize)]
struct SendSignalRequest {
    #[prost(message, optional, tag = "1")]
    #[serde(default)]
    process: Option<ProcessSelector>,
    #[prost(int32, tag = "2")]
    #[serde(default, deserialize_with = "deserialize_signal")]
    signal: i32,
}

impl SendSignalRequest {
    fn process_signal(&self) -> Option<ExecutionProcessSignal> {
        match self.signal {
            15 => Some(ExecutionProcessSignal::Terminate),
            9 => Some(ExecutionProcessSignal::Kill),
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq, ::prost::Message, Deserialize)]
struct UpdateRequest {
    #[prost(message, optional, tag = "1")]
    #[serde(default)]
    process: Option<ProcessSelector>,
    #[prost(message, optional, tag = "2")]
    #[serde(default)]
    pty: Option<Pty>,
}

fn serialize_optional_bytes<S>(value: &Option<Vec<u8>>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match value {
        Some(value) => serializer.serialize_some(&STANDARD.encode(value)),
        None => serializer.serialize_none(),
    }
}

fn deserialize_optional_bytes<'de, D>(deserializer: D) -> Result<Option<Vec<u8>>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    value
        .map(|value| STANDARD.decode(value).map_err(serde::de::Error::custom))
        .transpose()
}

fn deserialize_signal<'de, D>(deserializer: D) -> Result<i32, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum SignalValue {
        Name(String),
        Number(i32),
    }

    match Option::<SignalValue>::deserialize(deserializer)? {
        None => Ok(0),
        Some(SignalValue::Name(name)) if name == "SIGNAL_UNSPECIFIED" => Ok(0),
        Some(SignalValue::Name(name)) if name == "SIGNAL_SIGTERM" => Ok(15),
        Some(SignalValue::Name(name)) if name == "SIGNAL_SIGKILL" => Ok(9),
        Some(SignalValue::Name(name)) => Err(serde::de::Error::custom(format!(
            "unknown process signal {name:?}"
        ))),
        Some(SignalValue::Number(number)) => Ok(number),
    }
}

#[cfg(test)]
#[path = "process_tests.rs"]
mod tests;
