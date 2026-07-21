use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use a3s_box_core::pty::PtyRequest;
use a3s_box_core::{
    ExecChunk, ExecEvent, ExecExit, ExecOutput, ExecRequest, ExecutionGeneration, ExecutionId,
    ExecutionManagerError, ExecutionManagerResult, ExecutionProcess, ExecutionProcessInput,
    ExecutionProcessSignal, ExecutionProcessStream, ExecutionSessionManager, FileRequest,
    FileResponse, StreamType,
};
use async_trait::async_trait;
use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{Request, StatusCode};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use hyper::body::{to_bytes, Bytes, HttpBody};
use prost::Message;
use serde_json::{json, Value};

use super::{
    process_user, CloseStdinRequest, ConnectRequest, EmptyRequest, EmptyResponse, ListResponse,
    ProcessBroker, ProcessConfig, ProcessInput, ProcessResponse, ProcessSelector, Pty, PtySize,
    SendInputRequest, SendSignalRequest, StartRequest, StreamInputData, StreamInputRequest,
    StreamInputStart, UpdateRequest,
};

#[derive(Default)]
struct TestInput {
    writes: Mutex<Vec<Vec<u8>>>,
    closed: Mutex<bool>,
    cancelled: Mutex<bool>,
    signals: Mutex<Vec<ExecutionProcessSignal>>,
    sizes: Mutex<Vec<(u16, u16)>>,
}

#[async_trait]
impl ExecutionProcessInput for TestInput {
    async fn write_stdin(&self, data: &[u8]) -> ExecutionManagerResult<()> {
        self.writes.lock().unwrap().push(data.to_vec());
        Ok(())
    }

    async fn close_stdin(&self) -> ExecutionManagerResult<()> {
        *self.closed.lock().unwrap() = true;
        Ok(())
    }

    async fn cancel(&self) -> ExecutionManagerResult<()> {
        *self.cancelled.lock().unwrap() = true;
        Ok(())
    }

    async fn send_signal(&self, signal: ExecutionProcessSignal) -> ExecutionManagerResult<()> {
        self.signals.lock().unwrap().push(signal);
        Ok(())
    }

    async fn resize_pty(&self, cols: u16, rows: u16) -> ExecutionManagerResult<()> {
        self.sizes.lock().unwrap().push((cols, rows));
        Ok(())
    }
}

struct TestProcess {
    events: VecDeque<ExecEvent>,
    input: Arc<TestInput>,
}

#[async_trait]
impl ExecutionProcessStream for TestProcess {
    fn input(&self) -> Arc<dyn ExecutionProcessInput> {
        self.input.clone()
    }

    async fn next_event(&mut self) -> ExecutionManagerResult<Option<ExecEvent>> {
        if let Some(event) = self.events.pop_front() {
            return Ok(Some(event));
        }
        std::future::pending().await
    }
}

#[derive(Default)]
struct TestSessions {
    queued_events: Mutex<VecDeque<Vec<ExecEvent>>>,
    requests: Mutex<Vec<(String, u64, ExecRequest)>>,
    inputs: Mutex<Vec<Arc<TestInput>>>,
}

impl TestSessions {
    fn queue(&self, events: Vec<ExecEvent>) {
        self.queued_events.lock().unwrap().push_back(events);
    }

    fn latest_input(&self) -> Arc<TestInput> {
        self.inputs.lock().unwrap().last().unwrap().clone()
    }
}

#[async_trait]
impl ExecutionSessionManager for TestSessions {
    async fn execute(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
        _request: ExecRequest,
    ) -> ExecutionManagerResult<ExecOutput> {
        Err(unsupported())
    }

    async fn start_process(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        request: ExecRequest,
    ) -> ExecutionManagerResult<ExecutionProcess> {
        self.requests
            .lock()
            .unwrap()
            .push((execution_id.to_string(), generation.get(), request));
        let input = Arc::new(TestInput::default());
        self.inputs.lock().unwrap().push(input.clone());
        let events = self
            .queued_events
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default()
            .into();
        Ok(Box::new(TestProcess { events, input }))
    }

    async fn start_pty(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
        _request: PtyRequest,
    ) -> ExecutionManagerResult<ExecutionProcess> {
        let input = Arc::new(TestInput::default());
        self.inputs.lock().unwrap().push(input.clone());
        let events = self
            .queued_events
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default()
            .into();
        Ok(Box::new(TestProcess { events, input }))
    }

    async fn transfer_file(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
        _request: FileRequest,
    ) -> ExecutionManagerResult<FileResponse> {
        Err(unsupported())
    }
}

fn unsupported() -> ExecutionManagerError {
    ExecutionManagerError::Unavailable("unsupported test operation".to_string())
}

fn execution_id() -> ExecutionId {
    ExecutionId::new("execution-process-test").unwrap()
}

fn stream_request(path: &str, value: Value) -> Request<Body> {
    stream_request_many(path, &[value])
}

fn stream_request_many(path: &str, values: &[Value]) -> Request<Body> {
    raw_stream_request(path, encode_request_frames(values))
}

fn chunked_stream_request(path: &str, bytes: Vec<u8>, chunk_size: usize) -> Request<Body> {
    let (mut sender, body) = Body::channel();
    let chunks = bytes
        .chunks(chunk_size)
        .map(Bytes::copy_from_slice)
        .collect::<Vec<_>>();
    tokio::spawn(async move {
        for chunk in chunks {
            if sender.send_data(chunk).await.is_err() {
                return;
            }
        }
    });
    connect_stream_request(path, body)
}

fn raw_stream_request(path: &str, body: Vec<u8>) -> Request<Body> {
    connect_stream_request(path, Body::from(body))
}

fn connect_stream_request(path: &str, body: Body) -> Request<Body> {
    Request::post(path)
        .header(CONTENT_TYPE, "application/connect+json")
        .header(AUTHORIZATION, "Basic dXNlcjo=")
        .header("connect-timeout-ms", "2500")
        .body(body)
        .unwrap()
}

fn encode_request_frames(values: &[Value]) -> Vec<u8> {
    let mut body = Vec::new();
    for value in values {
        let payload = serde_json::to_vec(value).unwrap();
        body.push(0);
        body.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        body.extend_from_slice(&payload);
    }
    body
}

fn unary_request(path: &str, value: Value) -> Request<Body> {
    Request::post(path)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(value.to_string()))
        .unwrap()
}

fn protobuf_stream_request<T: Message>(path: &str, value: &T) -> Request<Body> {
    protobuf_stream_request_many(path, std::slice::from_ref(value))
}

fn protobuf_stream_request_many<T: Message>(path: &str, values: &[T]) -> Request<Body> {
    let mut body = Vec::new();
    for value in values {
        let payload = value.encode_to_vec();
        body.push(0);
        body.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        body.extend_from_slice(&payload);
    }
    raw_protobuf_stream_request(path, body)
}

fn raw_protobuf_stream_request(path: &str, body: Vec<u8>) -> Request<Body> {
    Request::post(path)
        .header(CONTENT_TYPE, "application/connect+proto; charset=utf-8")
        .header(AUTHORIZATION, "Basic dXNlcjo=")
        .header("connect-timeout-ms", "2500")
        .body(Body::from(body))
        .unwrap()
}

fn protobuf_unary_request<T: Message>(path: &str, value: &T) -> Request<Body> {
    Request::post(path)
        .header(CONTENT_TYPE, "application/proto; charset=utf-8")
        .body(Body::from(value.encode_to_vec()))
        .unwrap()
}

fn decode_raw_frames(bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut frames = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let flags = bytes[offset];
        let length = u32::from_be_bytes([
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
            bytes[offset + 4],
        ]) as usize;
        let start = offset + 5;
        let end = start + length;
        frames.push((flags, bytes[start..end].to_vec()));
        offset = end;
    }
    frames
}

fn decode_frames(bytes: &[u8]) -> Vec<(u8, Value)> {
    decode_raw_frames(bytes)
        .into_iter()
        .map(|(flags, payload)| (flags, serde_json::from_slice(&payload).unwrap()))
        .collect()
}

#[test]
fn missing_user_header_selects_the_pinned_envd_default() {
    assert_eq!(
        process_user(&axum::http::HeaderMap::new())
            .unwrap()
            .as_deref(),
        Some("user")
    );
}

#[test]
fn protobuf_messages_match_the_pinned_process_wire_tags() {
    let start = StartRequest {
        process: Some(ProcessConfig {
            cmd: "x".to_string(),
            args: Vec::new(),
            envs: Default::default(),
            cwd: None,
        }),
        pty: None,
        tag: None,
        stdin: Some(true),
    };
    assert_eq!(
        start.encode_to_vec(),
        [0x0a, 0x03, 0x0a, 0x01, b'x', 0x20, 0x01]
    );

    let signal = SendSignalRequest {
        process: Some(ProcessSelector {
            pid: Some(7),
            tag: None,
        }),
        signal: 15,
    };
    assert_eq!(signal.encode_to_vec(), [0x0a, 0x02, 0x08, 0x07, 0x10, 0x0f]);

    let stream_data = StreamInputRequest {
        start: None,
        data: Some(StreamInputData {
            input: Some(ProcessInput {
                stdin: Some(vec![0, 0xff]),
                pty: None,
            }),
        }),
        keepalive: None,
    };
    assert_eq!(
        stream_data.encode_to_vec(),
        [0x12, 0x06, 0x12, 0x04, 0x0a, 0x02, 0, 0xff]
    );

    assert_eq!(
        ProcessResponse::end(-2).encode_to_vec(),
        [
            0x0a, 0x0e, 0x1a, 0x0c, 0x08, 0x03, 0x10, 0x01, 0x1a, 0x06, b'e', b'x', b'i', b't',
            b'e', b'd',
        ]
    );
}

fn pid_from_start_frame(bytes: &[u8]) -> u32 {
    let frames = decode_frames(bytes);
    frames[0].1["event"]["start"]["pid"].as_u64().unwrap() as u32
}

async fn start_running_process(broker: &ProcessBroker, command: &str) -> u32 {
    let mut response = broker
        .handle(
            stream_request(
                "/process.Process/Start",
                json!({
                    "process": {"cmd": command, "args": [], "envs": {}},
                    "stdin": true
                }),
            ),
            &execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await;
    let start = response.body_mut().data().await.unwrap().unwrap();
    pid_from_start_frame(&start)
}

async fn start_running_pty(broker: &ProcessBroker, command: &str) -> u32 {
    let mut response = broker
        .handle(
            stream_request(
                "/process.Process/Start",
                json!({
                    "process": {"cmd": command, "args": [], "envs": {}},
                    "pty": {"size": {"cols": 80, "rows": 24}}
                }),
            ),
            &execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await;
    let start = response.body_mut().data().await.unwrap().unwrap();
    pid_from_start_frame(&start)
}

#[tokio::test]
async fn start_maps_the_pinned_json_request_and_streams_ordered_events() {
    let sessions = Arc::new(TestSessions::default());
    sessions.queue(vec![
        ExecEvent::Chunk(ExecChunk {
            stream: StreamType::Stdout,
            data: b"hello".to_vec(),
        }),
        ExecEvent::Chunk(ExecChunk {
            stream: StreamType::Stderr,
            data: b"warning".to_vec(),
        }),
        ExecEvent::Exit(ExecExit {
            exit_code: 0,
            oom_killed: false,
        }),
    ]);
    let broker = ProcessBroker::new(sessions.clone());
    let response = broker
        .handle(
            stream_request(
                "/process.Process/Start",
                json!({
                    "process": {
                        "cmd": "/bin/bash",
                        "args": ["-l", "-c", "printf hello"],
                        "envs": {"ALPHA": "one"},
                        "cwd": "/tmp"
                    },
                    "stdin": true,
                    "tag": "job-one"
                }),
            ),
            &execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], "application/connect+json");
    let frames = decode_frames(&to_bytes(response.into_body()).await.unwrap());
    assert_eq!(frames.len(), 5);
    assert_eq!(frames[0].0, 0);
    assert!(frames[0].1["event"]["start"]["pid"].is_number());
    assert_eq!(frames[1].1["event"]["data"]["stdout"], "aGVsbG8=");
    assert_eq!(frames[2].1["event"]["data"]["stderr"], "d2FybmluZw==");
    assert_eq!(frames[3].1["event"]["end"]["exitCode"], 0);
    assert_eq!(frames[4], (2, json!({})));

    let requests = sessions.requests.lock().unwrap();
    let (id, generation, request) = &requests[0];
    assert_eq!(id, "execution-process-test");
    assert_eq!(*generation, 1);
    assert_eq!(request.cmd, ["/bin/bash", "-l", "-c", "printf hello"]);
    assert_eq!(request.timeout_ns, 2_500_000_000);
    assert_eq!(request.env, ["ALPHA=one"]);
    assert_eq!(request.working_dir.as_deref(), Some("/tmp"));
    assert_eq!(request.user.as_deref(), Some("user"));
    assert!(request.stdin_streaming);
}

#[tokio::test]
async fn protobuf_start_streams_typed_events_and_a_json_end_stream() {
    let sessions = Arc::new(TestSessions::default());
    sessions.queue(vec![
        ExecEvent::Chunk(ExecChunk {
            stream: StreamType::Stdout,
            data: vec![0, 0xff, b'a'],
        }),
        ExecEvent::Chunk(ExecChunk {
            stream: StreamType::Stderr,
            data: b"warning".to_vec(),
        }),
        ExecEvent::Exit(ExecExit {
            exit_code: -2,
            oom_killed: false,
        }),
    ]);
    let broker = ProcessBroker::new(sessions.clone());
    let request = StartRequest {
        process: Some(ProcessConfig {
            cmd: "/bin/bash".to_string(),
            args: vec![
                "-l".to_string(),
                "-c".to_string(),
                "printf hello".to_string(),
            ],
            envs: [("ALPHA".to_string(), "one".to_string())]
                .into_iter()
                .collect(),
            cwd: Some("/tmp".to_string()),
        }),
        pty: None,
        tag: Some("protobuf-job".to_string()),
        stdin: Some(true),
    };
    let response = broker
        .handle(
            protobuf_stream_request("/process.Process/Start", &request),
            &execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[CONTENT_TYPE],
        "application/connect+proto"
    );
    let frames = decode_raw_frames(&to_bytes(response.into_body()).await.unwrap());
    assert_eq!(frames.len(), 5);
    assert_eq!(
        frames.iter().map(|frame| frame.0).collect::<Vec<_>>(),
        [0, 0, 0, 0, 2]
    );

    let start = ProcessResponse::decode(frames[0].1.as_slice()).unwrap();
    assert!(start.event.unwrap().start.unwrap().pid >= 1000);
    let stdout = ProcessResponse::decode(frames[1].1.as_slice()).unwrap();
    assert_eq!(
        stdout.event.unwrap().data.unwrap().stdout,
        Some(vec![0, 0xff, b'a'])
    );
    let stderr = ProcessResponse::decode(frames[2].1.as_slice()).unwrap();
    assert_eq!(
        stderr.event.unwrap().data.unwrap().stderr,
        Some(b"warning".to_vec())
    );
    let end = ProcessResponse::decode(frames[3].1.as_slice()).unwrap();
    let end = end.event.unwrap().end.unwrap();
    assert_eq!(end.exit_code, -2);
    assert!(end.exited);
    assert_eq!(end.status, "exited");
    assert_eq!(
        serde_json::from_slice::<Value>(&frames[4].1).unwrap(),
        json!({})
    );

    let requests = sessions.requests.lock().unwrap();
    assert_eq!(requests[0].2.cmd, ["/bin/bash", "-l", "-c", "printf hello"]);
    assert_eq!(requests[0].2.env, ["ALPHA=one"]);
    assert_eq!(requests[0].2.working_dir.as_deref(), Some("/tmp"));
    assert!(requests[0].2.stdin_streaming);
}

#[tokio::test]
async fn protobuf_process_procedures_preserve_raw_bytes_and_response_encoding() {
    let sessions = Arc::new(TestSessions::default());
    let broker = ProcessBroker::new(sessions.clone());
    let pid = start_running_process(&broker, "/bin/cat").await;
    let input = sessions.latest_input();

    let listed = broker
        .handle(
            protobuf_unary_request("/process.Process/List", &EmptyRequest {}),
            &execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await;
    assert_eq!(listed.headers()[CONTENT_TYPE], "application/proto");
    let listed = ListResponse::decode(to_bytes(listed.into_body()).await.unwrap()).unwrap();
    assert_eq!(listed.processes.len(), 1);
    assert_eq!(listed.processes[0].pid, pid);
    assert_eq!(listed.processes[0].config.cmd, "/bin/cat");

    let sent = broker
        .handle(
            protobuf_unary_request(
                "/process.Process/SendInput",
                &SendInputRequest {
                    process: Some(ProcessSelector {
                        pid: Some(pid),
                        tag: None,
                    }),
                    input: Some(ProcessInput {
                        stdin: Some(vec![0, 0xff, b'x']),
                        pty: None,
                    }),
                },
            ),
            &execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await;
    assert_eq!(sent.headers()[CONTENT_TYPE], "application/proto");
    EmptyResponse::decode(to_bytes(sent.into_body()).await.unwrap()).unwrap();

    let signalled = broker
        .handle(
            protobuf_unary_request(
                "/process.Process/SendSignal",
                &SendSignalRequest {
                    process: Some(ProcessSelector {
                        pid: Some(pid),
                        tag: None,
                    }),
                    signal: 15,
                },
            ),
            &execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await;
    assert_eq!(signalled.headers()[CONTENT_TYPE], "application/proto");
    EmptyResponse::decode(to_bytes(signalled.into_body()).await.unwrap()).unwrap();

    let mut connected = broker
        .handle(
            protobuf_stream_request(
                "/process.Process/Connect",
                &ConnectRequest {
                    process: Some(ProcessSelector {
                        pid: Some(pid),
                        tag: None,
                    }),
                },
            ),
            &execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await;
    assert_eq!(
        connected.headers()[CONTENT_TYPE],
        "application/connect+proto"
    );
    let first = connected.body_mut().data().await.unwrap().unwrap();
    let first = decode_raw_frames(&first);
    let connected = ProcessResponse::decode(first[0].1.as_slice()).unwrap();
    assert_eq!(connected.event.unwrap().start.unwrap().pid, pid);

    let stream_events = [
        StreamInputRequest {
            start: Some(StreamInputStart {
                process: Some(ProcessSelector {
                    pid: Some(pid),
                    tag: None,
                }),
            }),
            data: None,
            keepalive: None,
        },
        StreamInputRequest {
            start: None,
            data: Some(StreamInputData {
                input: Some(ProcessInput {
                    stdin: Some(vec![b'y', 0, 0xfe]),
                    pty: None,
                }),
            }),
            keepalive: None,
        },
        StreamInputRequest {
            start: None,
            data: None,
            keepalive: Some(EmptyRequest {}),
        },
    ];
    let streamed = broker
        .handle(
            protobuf_stream_request_many("/process.Process/StreamInput", &stream_events),
            &execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await;
    assert_eq!(
        streamed.headers()[CONTENT_TYPE],
        "application/connect+proto"
    );
    let frames = decode_raw_frames(&to_bytes(streamed.into_body()).await.unwrap());
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].0, 0);
    EmptyResponse::decode(frames[0].1.as_slice()).unwrap();
    assert_eq!(frames[1].0, 2);
    assert_eq!(
        serde_json::from_slice::<Value>(&frames[1].1).unwrap(),
        json!({})
    );

    let closed = broker
        .handle(
            protobuf_unary_request(
                "/process.Process/CloseStdin",
                &CloseStdinRequest {
                    process: Some(ProcessSelector {
                        pid: Some(pid),
                        tag: None,
                    }),
                },
            ),
            &execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await;
    assert_eq!(closed.headers()[CONTENT_TYPE], "application/proto");
    EmptyResponse::decode(to_bytes(closed.into_body()).await.unwrap()).unwrap();

    assert_eq!(
        &*input.writes.lock().unwrap(),
        &[vec![0, 0xff, b'x'], vec![b'y', 0, 0xfe]]
    );
    assert_eq!(
        &*input.signals.lock().unwrap(),
        &[ExecutionProcessSignal::Terminate]
    );
    assert!(*input.closed.lock().unwrap());

    let pty_pid = start_running_pty(&broker, "/bin/sh").await;
    let pty_input = sessions.latest_input();
    let updated = broker
        .handle(
            protobuf_unary_request(
                "/process.Process/Update",
                &UpdateRequest {
                    process: Some(ProcessSelector {
                        pid: Some(pty_pid),
                        tag: None,
                    }),
                    pty: Some(Pty {
                        size: Some(PtySize {
                            cols: 132,
                            rows: 43,
                        }),
                    }),
                },
            ),
            &execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await;
    assert_eq!(updated.headers()[CONTENT_TYPE], "application/proto");
    EmptyResponse::decode(to_bytes(updated.into_body()).await.unwrap()).unwrap();
    assert_eq!(&*pty_input.sizes.lock().unwrap(), &[(132, 43)]);
}

#[tokio::test]
async fn list_and_input_are_scoped_to_the_exact_execution_generation() {
    let sessions = Arc::new(TestSessions::default());
    sessions.queue(Vec::new());
    let broker = ProcessBroker::new(sessions.clone());
    let mut response = broker
        .handle(
            stream_request(
                "/process.Process/Start",
                json!({
                    "process": {"cmd": "/bin/cat", "args": [], "envs": {}},
                    "stdin": true,
                    "tag": "interactive"
                }),
            ),
            &execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await;
    let first = response.body_mut().data().await.unwrap().unwrap();
    let pid = pid_from_start_frame(&first);
    drop(response);

    let listed = broker
        .handle(
            unary_request("/process.Process/List", json!({})),
            &execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await;
    let listed: Value =
        serde_json::from_slice(&to_bytes(listed.into_body()).await.unwrap()).unwrap();
    assert_eq!(listed["processes"][0]["pid"], pid);
    assert_eq!(listed["processes"][0]["tag"], "interactive");

    let sent = broker
        .handle(
            unary_request(
                "/process.Process/SendInput",
                json!({"process": {"pid": pid}, "input": {"stdin": "aGVsbG8="}}),
            ),
            &execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await;
    assert_eq!(sent.status(), StatusCode::OK);
    assert_eq!(sessions.latest_input().writes.lock().unwrap()[0], b"hello");

    let stale = broker
        .handle(
            unary_request(
                "/process.Process/SendInput",
                json!({"process": {"pid": pid}, "input": {"stdin": "eA=="}}),
            ),
            &execution_id(),
            ExecutionGeneration::new(2).unwrap(),
        )
        .await;
    assert_eq!(stale.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn send_signal_delivers_pinned_sigterm_and_sigkill_to_exec_and_pty() {
    let sessions = Arc::new(TestSessions::default());
    let broker = ProcessBroker::new(sessions.clone());

    let exec_pid = start_running_process(&broker, "/bin/cat").await;
    let exec_input = sessions.latest_input();
    let pty_pid = start_running_pty(&broker, "/bin/sh").await;
    let pty_input = sessions.latest_input();

    let terminate = broker
        .handle(
            unary_request(
                "/process.Process/SendSignal",
                json!({
                    "process": {"pid": exec_pid},
                    "signal": "SIGNAL_SIGTERM"
                }),
            ),
            &execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await;
    assert_eq!(terminate.status(), StatusCode::OK);

    let kill = broker
        .handle(
            unary_request(
                "/process.Process/SendSignal",
                json!({"process": {"pid": pty_pid}, "signal": 9}),
            ),
            &execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await;
    assert_eq!(kill.status(), StatusCode::OK);
    assert_eq!(
        &*exec_input.signals.lock().unwrap(),
        &[ExecutionProcessSignal::Terminate]
    );
    assert_eq!(
        &*pty_input.signals.lock().unwrap(),
        &[ExecutionProcessSignal::Kill]
    );

    let unsupported = broker
        .handle(
            unary_request(
                "/process.Process/SendSignal",
                json!({"process": {"pid": exec_pid}, "signal": 2}),
            ),
            &execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await;
    assert_eq!(unsupported.status(), StatusCode::NOT_IMPLEMENTED);

    let unspecified = broker
        .handle(
            unary_request(
                "/process.Process/SendSignal",
                json!({"process": {"pid": exec_pid}}),
            ),
            &execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await;
    assert_eq!(unspecified.status(), StatusCode::NOT_IMPLEMENTED);

    let missing_process_wins_before_signal_validation = broker
        .handle(
            unary_request(
                "/process.Process/SendSignal",
                json!({"process": {"pid": 999999}, "signal": 2}),
            ),
            &execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await;
    assert_eq!(
        missing_process_wins_before_signal_validation.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn stream_input_decodes_fragmented_frames_in_order_and_can_reselect() {
    let sessions = Arc::new(TestSessions::default());
    let broker = ProcessBroker::new(sessions.clone());

    let first_pid = start_running_process(&broker, "/bin/cat").await;
    let first_input = sessions.latest_input();
    let second_pid = start_running_process(&broker, "/bin/sh").await;
    let second_input = sessions.latest_input();

    let body = encode_request_frames(&[
        json!({"keepalive": {}}),
        json!({"start": {"process": {"pid": first_pid}}}),
        json!({"data": {"input": {"stdin": "Zmlyc3Q="}}}),
        json!({"start": {"process": {"pid": second_pid}}}),
        json!({"data": {"input": {"stdin": "c2Vjb25k"}}}),
        json!({"keepalive": {}}),
    ]);
    let response = broker
        .handle(
            chunked_stream_request("/process.Process/StreamInput", body, 3),
            &execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], "application/connect+json");
    assert_eq!(
        decode_frames(&to_bytes(response.into_body()).await.unwrap()),
        [(0, json!({})), (2, json!({}))]
    );
    assert_eq!(&*first_input.writes.lock().unwrap(), &[b"first".to_vec()]);
    assert_eq!(&*second_input.writes.lock().unwrap(), &[b"second".to_vec()]);

    let empty = broker
        .handle(
            raw_stream_request("/process.Process/StreamInput", Vec::new()),
            &execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await;
    assert_eq!(
        decode_frames(&to_bytes(empty.into_body()).await.unwrap()),
        [(0, json!({})), (2, json!({}))]
    );
}

#[tokio::test]
async fn stream_input_bounds_each_frame_instead_of_the_complete_stream() {
    let sessions = Arc::new(TestSessions::default());
    let broker = ProcessBroker::new(sessions.clone());
    let pid = start_running_process(&broker, "/bin/cat").await;
    let input = sessions.latest_input();
    let first = vec![b'a'; 400_000];
    let second = vec![b'b'; 400_000];
    let body = encode_request_frames(&[
        json!({"start": {"process": {"pid": pid}}}),
        json!({"data": {"input": {"stdin": STANDARD.encode(&first)}}}),
        json!({"data": {"input": {"stdin": STANDARD.encode(&second)}}}),
    ]);
    assert!(body.len() > 1024 * 1024);

    let response = broker
        .handle(
            raw_stream_request("/process.Process/StreamInput", body),
            &execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await;
    assert_eq!(
        decode_frames(&to_bytes(response.into_body()).await.unwrap()),
        [(0, json!({})), (2, json!({}))]
    );
    let writes = input.writes.lock().unwrap();
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0], first);
    assert_eq!(writes[1], second);
}

#[tokio::test]
async fn stream_input_rejects_data_before_start_and_invalid_oneof_events() {
    let sessions = Arc::new(TestSessions::default());
    let broker = ProcessBroker::new(sessions);
    let response = broker
        .handle(
            stream_request(
                "/process.Process/StreamInput",
                json!({"data": {"input": {"stdin": "eA=="}}}),
            ),
            &execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await;
    let frames = decode_frames(&to_bytes(response.into_body()).await.unwrap());
    assert_eq!(frames[0].0, 2);
    assert_eq!(frames[0].1["error"]["code"], "failed_precondition");

    for event in [
        json!({}),
        json!({
            "start": {"process": {"pid": 1}},
            "keepalive": {}
        }),
    ] {
        let response = broker
            .handle(
                stream_request("/process.Process/StreamInput", event),
                &execution_id(),
                ExecutionGeneration::INITIAL,
            )
            .await;
        let frames = decode_frames(&to_bytes(response.into_body()).await.unwrap());
        assert_eq!(frames[0].0, 2);
        assert_eq!(frames[0].1["error"]["code"], "invalid_argument");
    }

    let pid = start_running_process(&broker, "/bin/cat").await;
    let response = broker
        .handle(
            stream_request_many(
                "/process.Process/StreamInput",
                &[
                    json!({"start": {"process": {"pid": pid}}}),
                    json!({"data": {"input": {"stdin": "eA==", "pty": "eQ=="}}}),
                ],
            ),
            &execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await;
    let frames = decode_frames(&to_bytes(response.into_body()).await.unwrap());
    assert_eq!(frames[0].0, 2);
    assert_eq!(frames[0].1["error"]["code"], "invalid_argument");
}

#[tokio::test]
async fn connect_stream_decoder_rejects_invalid_and_oversized_envelopes() {
    let broker = ProcessBroker::new(Arc::new(TestSessions::default()));
    let oversized_length = (1024_u32 * 1024 + 1).to_be_bytes();
    let cases = [
        vec![0, 0, 0, 0],
        vec![0, 0, 0, 0, 2, b'{'],
        [vec![2], 2_u32.to_be_bytes().to_vec(), b"{}".to_vec()].concat(),
        [vec![0], oversized_length.to_vec()].concat(),
    ];
    for body in cases {
        let response = broker
            .handle(
                raw_stream_request("/process.Process/StreamInput", body),
                &execution_id(),
                ExecutionGeneration::INITIAL,
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let frames = decode_frames(&to_bytes(response.into_body()).await.unwrap());
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, 2);
        assert_eq!(frames[0].1["error"]["code"], "invalid_argument");
    }
}

#[tokio::test]
async fn protobuf_transport_rejects_wrong_media_types_and_malformed_messages() {
    let broker = ProcessBroker::new(Arc::new(TestSessions::default()));

    for request in [
        Request::post("/process.Process/List")
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(Body::empty())
            .unwrap(),
        Request::post("/process.Process/Start")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap(),
    ] {
        let response = broker
            .handle(request, &execution_id(), ExecutionGeneration::INITIAL)
            .await;
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(response.headers()[CONTENT_TYPE], "application/json");
        let error: Value =
            serde_json::from_slice(&to_bytes(response.into_body()).await.unwrap()).unwrap();
        assert_eq!(error["code"], "invalid_argument");
    }

    let malformed_unary = broker
        .handle(
            Request::post("/process.Process/List")
                .header(CONTENT_TYPE, "application/proto")
                .body(Body::from(vec![0x0a, 0xff]))
                .unwrap(),
            &execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await;
    assert_eq!(malformed_unary.status(), StatusCode::BAD_REQUEST);
    assert_eq!(malformed_unary.headers()[CONTENT_TYPE], "application/json");

    for body in [
        vec![0, 0, 0, 0, 2, 0x0a, 0xff],
        [vec![0], (1024_u32 * 1024 + 1).to_be_bytes().to_vec()].concat(),
    ] {
        let response = broker
            .handle(
                raw_protobuf_stream_request("/process.Process/StreamInput", body),
                &execution_id(),
                ExecutionGeneration::INITIAL,
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[CONTENT_TYPE],
            "application/connect+proto"
        );
        let frames = decode_raw_frames(&to_bytes(response.into_body()).await.unwrap());
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, 2);
        let error: Value = serde_json::from_slice(&frames[0].1).unwrap();
        assert_eq!(error["error"]["code"], "invalid_argument");
    }
}
