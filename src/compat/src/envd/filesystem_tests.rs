use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use a3s_box_core::pty::PtyRequest;
use a3s_box_core::{
    ExecOutput, ExecRequest, ExecutionGeneration, ExecutionId, ExecutionManagerError,
    ExecutionManagerResult, ExecutionProcess, ExecutionSessionManager, FileRequest, FileResponse,
    FilesystemEntry as SessionEntry, FilesystemEntryKind, FilesystemOp, FilesystemRequest,
    FilesystemResponse,
};
use async_trait::async_trait;
use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{Method, Request, StatusCode};
use hyper::body::to_bytes;
use prost::Message;
use serde_json::{json, Value};

use super::{
    EntryInfo, FilesystemBroker, ListDirRequest, ListDirResponse, MakeDirRequest, MakeDirResponse,
    MoveRequest, RemoveRequest,
};

#[derive(Default)]
struct TestSessions {
    requests: Mutex<Vec<(String, u64, FilesystemRequest)>>,
    responses: Mutex<VecDeque<ExecutionManagerResult<FilesystemResponse>>>,
}

impl TestSessions {
    fn queue(&self, response: ExecutionManagerResult<FilesystemResponse>) {
        self.responses.lock().unwrap().push_back(response);
    }

    fn requests(&self) -> Vec<(String, u64, FilesystemRequest)> {
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
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
        _request: FileRequest,
    ) -> ExecutionManagerResult<FileResponse> {
        Err(unsupported())
    }

    async fn filesystem(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        request: FilesystemRequest,
    ) -> ExecutionManagerResult<FilesystemResponse> {
        self.requests
            .lock()
            .unwrap()
            .push((execution_id.to_string(), generation.get(), request));
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("test filesystem response was queued")
    }
}

fn unsupported() -> ExecutionManagerError {
    ExecutionManagerError::Unavailable("unsupported test operation".to_string())
}

fn execution_id() -> ExecutionId {
    ExecutionId::new("execution-filesystem-test").unwrap()
}

fn generation() -> ExecutionGeneration {
    ExecutionGeneration::new(9).unwrap()
}

fn entry(path: &str, kind: FilesystemEntryKind) -> SessionEntry {
    let mut metadata = BTreeMap::new();
    metadata.insert("purpose".to_string(), "fixture".to_string());
    SessionEntry {
        name: path.rsplit('/').next().unwrap().to_string(),
        kind,
        path: path.to_string(),
        size: 7,
        mode: if kind == FilesystemEntryKind::Directory {
            0o755
        } else {
            0o644
        },
        permissions: if kind == FilesystemEntryKind::Directory {
            "drwxr-xr-x"
        } else {
            "-rw-r--r--"
        }
        .to_string(),
        owner: "alice".to_string(),
        group: "alice".to_string(),
        modified_seconds: 1_720_000_000,
        modified_nanos: 123_000_000,
        symlink_target: None,
        metadata,
    }
}

fn success(entry: Option<SessionEntry>, entries: Vec<SessionEntry>) -> FilesystemResponse {
    FilesystemResponse {
        success: true,
        entry,
        entries,
        error: None,
    }
}

fn request(path: &str, content_type: &str, body: impl Into<Body>) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(CONTENT_TYPE, content_type)
        .header(AUTHORIZATION, "Basic YWxpY2U6")
        .body(body.into())
        .unwrap()
}

async fn response_bytes(response: axum::http::Response<Body>) -> Vec<u8> {
    to_bytes(response.into_body()).await.unwrap().to_vec()
}

async fn response_json(response: axum::http::Response<Body>) -> Value {
    serde_json::from_slice(&response_bytes(response).await).unwrap()
}

#[tokio::test]
async fn json_stat_preserves_generation_user_and_protobuf_json_types() {
    let sessions = Arc::new(TestSessions::default());
    sessions.queue(Ok(success(
        Some(entry("/home/alice/value.txt", FilesystemEntryKind::File)),
        Vec::new(),
    )));
    let broker = FilesystemBroker::new(sessions.clone());
    let response = broker
        .handle(
            request(
                "/filesystem.Filesystem/Stat",
                "application/json",
                Body::from(r#"{"path":"~/value.txt"}"#),
            ),
            &execution_id(),
            generation(),
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], "application/json");
    let value = response_json(response).await;
    assert_eq!(value["entry"]["type"], "FILE_TYPE_FILE");
    assert_eq!(value["entry"]["size"], "7");
    assert_eq!(value["entry"]["mode"], 0o644);
    assert_eq!(value["entry"]["metadata"], json!({"purpose": "fixture"}));
    assert!(value["entry"]["modifiedTime"]
        .as_str()
        .unwrap()
        .ends_with('Z'));

    let requests = sessions.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, "execution-filesystem-test");
    assert_eq!(requests[0].1, 9);
    assert_eq!(requests[0].2.op, FilesystemOp::Stat);
    assert_eq!(requests[0].2.path, "~/value.txt");
    assert_eq!(requests[0].2.user.as_deref(), Some("alice"));
}

#[tokio::test]
async fn protobuf_make_dir_preserves_wire_encoding() {
    let sessions = Arc::new(TestSessions::default());
    sessions.queue(Ok(success(
        Some(entry("/home/alice/new", FilesystemEntryKind::Directory)),
        Vec::new(),
    )));
    let broker = FilesystemBroker::new(sessions.clone());
    let body = MakeDirRequest {
        path: "new".to_string(),
    }
    .encode_to_vec();
    let response = broker
        .handle(
            request(
                "/filesystem.Filesystem/MakeDir",
                "application/proto",
                Body::from(body),
            ),
            &execution_id(),
            generation(),
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], "application/proto");
    let decoded = MakeDirResponse::decode(response_bytes(response).await.as_slice()).unwrap();
    let entry = decoded.entry.unwrap();
    assert_eq!(entry.path, "/home/alice/new");
    assert_eq!(entry.kind, 2);
    assert_eq!(entry.size, 7);
    assert_eq!(sessions.requests()[0].2.op, FilesystemOp::MakeDir);
}

#[tokio::test]
async fn move_list_and_remove_map_every_pinned_mutation() {
    let sessions = Arc::new(TestSessions::default());
    sessions.queue(Ok(success(
        Some(entry("/home/alice/after", FilesystemEntryKind::File)),
        Vec::new(),
    )));
    sessions.queue(Ok(success(
        None,
        vec![
            entry("/home/alice/dir/a", FilesystemEntryKind::File),
            entry("/home/alice/dir/nested", FilesystemEntryKind::Directory),
        ],
    )));
    sessions.queue(Ok(success(None, Vec::new())));
    let broker = FilesystemBroker::new(sessions.clone());

    let move_body = MoveRequest {
        source: "before".to_string(),
        destination: "after".to_string(),
    }
    .encode_to_vec();
    let moved = broker
        .handle(
            request(
                "/filesystem.Filesystem/Move",
                "application/proto",
                Body::from(move_body),
            ),
            &execution_id(),
            generation(),
        )
        .await;
    assert_eq!(moved.status(), StatusCode::OK);

    let list_body = ListDirRequest {
        path: "dir".to_string(),
        depth: 2,
    }
    .encode_to_vec();
    let listed = broker
        .handle(
            request(
                "/filesystem.Filesystem/ListDir",
                "application/proto",
                Body::from(list_body),
            ),
            &execution_id(),
            generation(),
        )
        .await;
    let listed = ListDirResponse::decode(response_bytes(listed).await.as_slice()).unwrap();
    assert_eq!(listed.entries.len(), 2);

    let remove_body = RemoveRequest {
        path: "dir".to_string(),
    }
    .encode_to_vec();
    let removed = broker
        .handle(
            request(
                "/filesystem.Filesystem/Remove",
                "application/proto",
                Body::from(remove_body),
            ),
            &execution_id(),
            generation(),
        )
        .await;
    assert_eq!(removed.status(), StatusCode::OK);
    assert!(response_bytes(removed).await.is_empty());

    let requests = sessions.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].2.op, FilesystemOp::Move);
    assert_eq!(requests[0].2.destination.as_deref(), Some("after"));
    assert_eq!(requests[1].2.op, FilesystemOp::ListDir);
    assert_eq!(requests[1].2.depth, 2);
    assert_eq!(requests[2].2.op, FilesystemOp::Remove);
}

#[tokio::test]
async fn filesystem_errors_and_watch_gaps_are_explicit() {
    let sessions = Arc::new(TestSessions::default());
    sessions.queue(Ok(FilesystemResponse {
        success: false,
        entry: None,
        entries: Vec::new(),
        error: Some("directory already exists: /home/alice/new".to_string()),
    }));
    let broker = FilesystemBroker::new(sessions);
    let exists = broker
        .handle(
            request(
                "/filesystem.Filesystem/MakeDir",
                "application/json",
                Body::from(r#"{"path":"new"}"#),
            ),
            &execution_id(),
            generation(),
        )
        .await;
    assert_eq!(exists.status(), StatusCode::CONFLICT);
    assert_eq!(response_json(exists).await["code"], "already_exists");

    let watch = broker
        .handle(
            request(
                "/filesystem.Filesystem/WatchDir",
                "application/connect+proto",
                Body::empty(),
            ),
            &execution_id(),
            generation(),
        )
        .await;
    assert_eq!(watch.status(), StatusCode::OK);
    assert_eq!(watch.headers()[CONTENT_TYPE], "application/connect+proto");
    assert!(String::from_utf8_lossy(&response_bytes(watch).await).contains("unimplemented"));
}

#[test]
fn pinned_filesystem_messages_keep_their_golden_wire_tags() {
    assert_eq!(
        MoveRequest {
            source: "a".to_string(),
            destination: "b".to_string(),
        }
        .encode_to_vec(),
        vec![0x0a, 0x01, b'a', 0x12, 0x01, b'b']
    );
    assert_eq!(
        ListDirRequest {
            path: "d".to_string(),
            depth: 2,
        }
        .encode_to_vec(),
        vec![0x0a, 0x01, b'd', 0x10, 0x02]
    );
    let wire = EntryInfo {
        name: "f".to_string(),
        kind: 1,
        path: "/f".to_string(),
        size: 7,
        mode: 0,
        permissions: String::new(),
        owner: String::new(),
        group: String::new(),
        modified_time: None,
        symlink_target: None,
        metadata: BTreeMap::new(),
    }
    .encode_to_vec();
    assert!(wire.windows(2).any(|bytes| bytes == [0x10, 0x01]));
    assert!(wire.windows(2).any(|bytes| bytes == [0x20, 0x07]));
}
