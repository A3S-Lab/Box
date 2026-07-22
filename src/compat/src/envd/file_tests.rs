use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use a3s_box_core::pty::PtyRequest;
use a3s_box_core::{
    ExecOutput, ExecRequest, ExecutionGeneration, ExecutionId, ExecutionManagerError,
    ExecutionManagerResult, ExecutionProcess, ExecutionSessionManager, FileOp, FileRequest,
    FileResponse,
};
use async_trait::async_trait;
use axum::body::Body;
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{Method, Request, StatusCode};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use hyper::body::to_bytes;
use serde_json::{json, Value};

use super::files::FileBroker;

#[derive(Default)]
struct TestSessions {
    requests: Mutex<Vec<(String, u64, FileRequest)>>,
    responses: Mutex<VecDeque<ExecutionManagerResult<FileResponse>>>,
}

impl TestSessions {
    fn queue(&self, response: ExecutionManagerResult<FileResponse>) {
        self.responses.lock().unwrap().push_back(response);
    }

    fn requests(&self) -> Vec<(String, u64, FileRequest)> {
        self.requests.lock().unwrap().clone()
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
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
        _request: ExecRequest,
    ) -> ExecutionManagerResult<ExecutionProcess> {
        Err(unsupported())
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
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        request: FileRequest,
    ) -> ExecutionManagerResult<FileResponse> {
        self.requests
            .lock()
            .unwrap()
            .push((execution_id.to_string(), generation.get(), request));
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("test file response was queued")
    }
}

fn unsupported() -> ExecutionManagerError {
    ExecutionManagerError::Unavailable("unsupported test operation".to_string())
}

fn execution_id() -> ExecutionId {
    ExecutionId::new("execution-file-test").unwrap()
}

fn generation() -> ExecutionGeneration {
    ExecutionGeneration::new(7).unwrap()
}

fn successful_response(size: usize, data: Option<String>) -> FileResponse {
    FileResponse {
        success: true,
        data,
        size: size as u64,
        error: None,
    }
}

async fn response_json(response: axum::http::Response<Body>) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body()).await.unwrap()).unwrap()
}

#[tokio::test]
async fn download_forwards_generation_user_and_binary_data() {
    let sessions = Arc::new(TestSessions::default());
    let data = vec![0, 0xff, b'a', 0];
    sessions.queue(Ok(successful_response(
        data.len(),
        Some(STANDARD.encode(&data)),
    )));
    let broker = FileBroker::new(sessions.clone());
    let request = Request::builder()
        .method(Method::GET)
        .uri("/files?path=project%2Fdata.bin&username=alice")
        .body(Body::empty())
        .unwrap();

    let response = broker.handle(request, &execution_id(), generation()).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], "application/octet-stream");
    assert_eq!(
        response.headers()[CONTENT_DISPOSITION],
        "inline; filename=\"data.bin\""
    );
    assert_eq!(to_bytes(response.into_body()).await.unwrap(), data);
    let requests = sessions.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, "execution-file-test");
    assert_eq!(requests[0].1, 7);
    assert_eq!(requests[0].2.op, FileOp::Download);
    assert_eq!(requests[0].2.guest_path, "/home/alice/project/data.bin");
    assert_eq!(requests[0].2.user.as_deref(), Some("alice"));
    assert!(requests[0].2.data.is_none());
}

#[tokio::test]
async fn raw_upload_preserves_non_utf8_bytes_and_returns_envd_shape() {
    let sessions = Arc::new(TestSessions::default());
    let data = vec![0, 0xfe, 0xff, b'z'];
    sessions.queue(Ok(successful_response(data.len(), None)));
    let broker = FileBroker::new(sessions.clone());
    let request = Request::builder()
        .method(Method::POST)
        .uri("/files?path=~%2Fout.bin")
        .header(CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(data.clone()))
        .unwrap();

    let response = broker.handle(request, &execution_id(), generation()).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response_json(response).await,
        json!([{
            "path": "/home/user/out.bin",
            "name": "out.bin",
            "type": "file"
        }])
    );
    let requests = sessions.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].2.op, FileOp::Upload);
    assert_eq!(requests[0].2.guest_path, "/home/user/out.bin");
    assert_eq!(requests[0].2.user.as_deref(), Some("user"));
    assert_eq!(
        STANDARD
            .decode(requests[0].2.data.as_deref().unwrap())
            .unwrap(),
        data
    );
}

#[tokio::test]
async fn multipart_upload_supports_multiple_filename_paths() {
    let sessions = Arc::new(TestSessions::default());
    sessions.queue(Ok(successful_response(3, None)));
    sessions.queue(Ok(successful_response(2, None)));
    let broker = FileBroker::new(sessions.clone());
    let body = concat!(
        "--A3S\r\n",
        "Content-Disposition: form-data; name=\"ignored\"\r\n\r\n",
        "value\r\n",
        "--A3S\r\n",
        "Content-Disposition: form-data; name=\"file\"; filename=\"dir/one.txt\"\r\n",
        "Content-Type: application/octet-stream\r\n\r\n",
        "one\r\n",
        "--A3S\r\n",
        "Content-Disposition: form-data; name=\"file\"; filename=\"/tmp/two.bin\"\r\n\r\n",
        "xy\r\n",
        "--A3S--\r\n"
    );
    let request = Request::builder()
        .method(Method::POST)
        .uri("/files?username=bob")
        .header(CONTENT_TYPE, "multipart/form-data; boundary=A3S")
        .body(Body::from(body))
        .unwrap();

    let response = broker.handle(request, &execution_id(), generation()).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response_json(response).await,
        json!([
            {"path": "/home/bob/dir/one.txt", "name": "one.txt", "type": "file"},
            {"path": "/tmp/two.bin", "name": "two.bin", "type": "file"}
        ])
    );
    let requests = sessions.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].2.guest_path, "/home/bob/dir/one.txt");
    assert_eq!(requests[1].2.guest_path, "/tmp/two.bin");
    assert_eq!(
        STANDARD
            .decode(requests[0].2.data.as_deref().unwrap())
            .unwrap(),
        b"one"
    );
    assert_eq!(
        STANDARD
            .decode(requests[1].2.data.as_deref().unwrap())
            .unwrap(),
        b"xy"
    );
}

#[tokio::test]
async fn file_errors_use_envd_status_and_numeric_code() {
    let sessions = Arc::new(TestSessions::default());
    sessions.queue(Ok(FileResponse {
        success: false,
        data: None,
        size: 0,
        error: Some("file not found: /home/user/missing".to_string()),
    }));
    let broker = FileBroker::new(sessions.clone());
    let missing = Request::builder()
        .method(Method::GET)
        .uri("/files?path=missing")
        .body(Body::empty())
        .unwrap();
    let response = broker.handle(missing, &execution_id(), generation()).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response_json(response).await["code"], 404);

    let unsupported = Request::builder()
        .method(Method::POST)
        .uri("/files?path=data")
        .header(CONTENT_TYPE, "text/plain")
        .body(Body::from("data"))
        .unwrap();
    let response = broker
        .handle(unsupported, &execution_id(), generation())
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response_json(response).await["code"], 400);
    assert_eq!(sessions.requests().len(), 1);
}

#[tokio::test]
async fn raw_upload_requires_path_before_opening_a_guest_session() {
    let sessions = Arc::new(TestSessions::default());
    let broker = FileBroker::new(sessions.clone());
    let request = Request::builder()
        .method(Method::POST)
        .uri("/files")
        .header(CONTENT_TYPE, "application/octet-stream")
        .body(Body::from("data"))
        .unwrap();

    let response = broker.handle(request, &execution_id(), generation()).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(sessions.requests().is_empty());
}
