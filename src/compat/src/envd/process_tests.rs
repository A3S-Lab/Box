use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use a3s_box_core::pty::PtyRequest;
use a3s_box_core::{
    ExecChunk, ExecEvent, ExecExit, ExecOutput, ExecRequest, ExecutionGeneration, ExecutionId,
    ExecutionManagerError, ExecutionManagerResult, ExecutionProcess, ExecutionProcessInput,
    ExecutionProcessStream, ExecutionSessionManager, FileRequest, FileResponse, StreamType,
};
use async_trait::async_trait;
use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{Request, StatusCode};
use hyper::body::{to_bytes, HttpBody};
use serde_json::{json, Value};

use super::{process_user, ProcessBroker};

#[derive(Default)]
struct TestInput {
    writes: Mutex<Vec<Vec<u8>>>,
    closed: Mutex<bool>,
    cancelled: Mutex<bool>,
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
        Err(unsupported())
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
    let payload = serde_json::to_vec(&value).unwrap();
    let mut body = Vec::with_capacity(payload.len() + 5);
    body.push(0);
    body.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    body.extend_from_slice(&payload);
    Request::post(path)
        .header(CONTENT_TYPE, "application/connect+json")
        .header(AUTHORIZATION, "Basic dXNlcjo=")
        .header("connect-timeout-ms", "2500")
        .body(Body::from(body))
        .unwrap()
}

fn unary_request(path: &str, value: Value) -> Request<Body> {
    Request::post(path)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(value.to_string()))
        .unwrap()
}

fn decode_frames(bytes: &[u8]) -> Vec<(u8, Value)> {
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
        frames.push((flags, serde_json::from_slice(&bytes[start..end]).unwrap()));
        offset = end;
    }
    frames
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

fn pid_from_start_frame(bytes: &[u8]) -> u32 {
    let frames = decode_frames(bytes);
    frames[0].1["event"]["start"]["pid"].as_u64().unwrap() as u32
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
