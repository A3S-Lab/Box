use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use a3s_box_core::pty::PtyRequest;
use a3s_box_core::{
    resolve_execution, BoxConfig, CreateExecutionRequest, ExecOutput, ExecRequest,
    ExecutionGeneration, ExecutionId, ExecutionIsolation, ExecutionLease, ExecutionManager,
    ExecutionManagerError, ExecutionManagerResult, ExecutionProcess, ExecutionReservation,
    ExecutionSessionManager, ExecutionSnapshot, ExecutionSnapshotId, ExecutionState,
    ExecutionStatus, FileOp, FileRequest, FileResponse, FilesystemEntry, FilesystemEntryKind,
    FilesystemOp, FilesystemRequest, FilesystemResponse, KillOutcome, OperationId,
    ReconcileOutcome,
};
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use chrono::Utc;

use super::{CommandRunOptions, Sandbox, SandboxCreateOptions, SandboxNetwork};
use crate::{A3sBoxClient, A3sBoxPaths};

#[derive(Debug)]
struct RecordingRuntime {
    config: Mutex<Option<BoxConfig>>,
    state: Mutex<ExecutionState>,
    create_requests: Mutex<Vec<CreateExecutionRequest>>,
    exec_requests: Mutex<Vec<ExecRequest>>,
    file_requests: Mutex<Vec<FileRequest>>,
    filesystem_requests: Mutex<Vec<FilesystemRequest>>,
    snapshot_requests: Mutex<Vec<ExecutionSnapshotId>>,
}

impl RecordingRuntime {
    fn new() -> Self {
        Self {
            config: Mutex::new(None),
            state: Mutex::new(ExecutionState::Created),
            create_requests: Mutex::new(Vec::new()),
            exec_requests: Mutex::new(Vec::new()),
            file_requests: Mutex::new(Vec::new()),
            filesystem_requests: Mutex::new(Vec::new()),
            snapshot_requests: Mutex::new(Vec::new()),
        }
    }

    fn execution_id() -> ExecutionId {
        ExecutionId::new("local-rust-sdk-test").unwrap()
    }

    fn lease(&self) -> ExecutionLease {
        let config = self.config.lock().unwrap().clone().unwrap();
        ExecutionLease {
            execution_id: Self::execution_id(),
            generation: ExecutionGeneration::INITIAL,
            plan: resolve_execution(&config).unwrap(),
            resources: config.resources,
            started_at: Utc::now(),
        }
    }
}

#[async_trait]
impl ExecutionManager for RecordingRuntime {
    async fn create(
        &self,
        request: CreateExecutionRequest,
        _operation_id: &OperationId,
    ) -> ExecutionManagerResult<ExecutionReservation> {
        let config = request.config.clone();
        *self.config.lock().unwrap() = Some(config.clone());
        self.create_requests.lock().unwrap().push(request);
        Ok(ExecutionReservation {
            execution_id: Self::execution_id(),
            generation: ExecutionGeneration::INITIAL,
            plan: resolve_execution(&config).unwrap(),
            resources: config.resources,
            created_at: Utc::now(),
        })
    }

    async fn create_and_start(
        &self,
        request: CreateExecutionRequest,
        _operation_id: &OperationId,
    ) -> ExecutionManagerResult<ExecutionLease> {
        *self.config.lock().unwrap() = Some(request.config.clone());
        self.create_requests.lock().unwrap().push(request);
        *self.state.lock().unwrap() = ExecutionState::Running;
        Ok(self.lease())
    }

    async fn inspect(&self, execution_id: &ExecutionId) -> ExecutionManagerResult<ExecutionStatus> {
        if execution_id != &Self::execution_id() {
            return Err(ExecutionManagerError::NotFound(execution_id.clone()));
        }
        let lease = self.lease();
        Ok(ExecutionStatus {
            execution_id: lease.execution_id,
            generation: lease.generation,
            state: *self.state.lock().unwrap(),
            plan: lease.plan,
        })
    }

    async fn create_filesystem_snapshot(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
        snapshot_id: &ExecutionSnapshotId,
    ) -> ExecutionManagerResult<ExecutionSnapshot> {
        self.snapshot_requests
            .lock()
            .unwrap()
            .push(snapshot_id.clone());
        Ok(ExecutionSnapshot {
            snapshot_id: snapshot_id.clone(),
            size_bytes: 5,
            state: *self.state.lock().unwrap(),
            lease: self.lease(),
        })
    }

    async fn filesystem_snapshot_size(
        &self,
        snapshot_id: &ExecutionSnapshotId,
    ) -> ExecutionManagerResult<Option<u64>> {
        Ok(self
            .snapshot_requests
            .lock()
            .unwrap()
            .iter()
            .any(|candidate| candidate == snapshot_id)
            .then_some(5))
    }

    async fn delete_filesystem_snapshot(
        &self,
        snapshot_id: &ExecutionSnapshotId,
    ) -> ExecutionManagerResult<bool> {
        let mut snapshots = self.snapshot_requests.lock().unwrap();
        let original_len = snapshots.len();
        snapshots.retain(|candidate| candidate != snapshot_id);
        Ok(snapshots.len() != original_len)
    }

    async fn pause(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
        _keep_memory: bool,
    ) -> ExecutionManagerResult<ExecutionLease> {
        *self.state.lock().unwrap() = ExecutionState::Paused;
        Ok(self.lease())
    }

    async fn resume(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<ExecutionLease> {
        *self.state.lock().unwrap() = ExecutionState::Running;
        Ok(self.lease())
    }

    async fn kill(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<KillOutcome> {
        *self.state.lock().unwrap() = ExecutionState::Stopped;
        Ok(KillOutcome::Killed)
    }

    async fn reconcile(
        &self,
        _operation_id: &OperationId,
    ) -> ExecutionManagerResult<ReconcileOutcome> {
        Ok(ReconcileOutcome::Absent)
    }
}

#[async_trait]
impl ExecutionSessionManager for RecordingRuntime {
    async fn execute(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
        request: ExecRequest,
    ) -> ExecutionManagerResult<ExecOutput> {
        self.exec_requests.lock().unwrap().push(request);
        Ok(ExecOutput {
            stdout: b"42\n".to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
            truncated: false,
        })
    }

    async fn start_process(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
        _request: ExecRequest,
    ) -> ExecutionManagerResult<ExecutionProcess> {
        Err(ExecutionManagerError::Unavailable(
            "streaming process is outside this test".to_string(),
        ))
    }

    async fn start_pty(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
        _request: PtyRequest,
    ) -> ExecutionManagerResult<ExecutionProcess> {
        Err(ExecutionManagerError::Unavailable(
            "PTY is outside this test".to_string(),
        ))
    }

    async fn transfer_file(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
        request: FileRequest,
    ) -> ExecutionManagerResult<FileResponse> {
        let response = match request.op {
            FileOp::Upload => FileResponse {
                success: true,
                data: None,
                size: request
                    .data
                    .as_deref()
                    .and_then(|data| STANDARD.decode(data).ok())
                    .map_or(0, |data| data.len() as u64),
                error: None,
            },
            FileOp::Download => FileResponse {
                success: true,
                data: Some(STANDARD.encode(b"hello")),
                size: 5,
                error: None,
            },
        };
        self.file_requests.lock().unwrap().push(request);
        Ok(response)
    }

    async fn filesystem(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
        request: FilesystemRequest,
    ) -> ExecutionManagerResult<FilesystemResponse> {
        let entry = (request.op == FilesystemOp::Stat).then(|| FilesystemEntry {
            name: "note.txt".to_string(),
            kind: FilesystemEntryKind::File,
            path: request.path.clone(),
            size: 5,
            mode: 0o644,
            permissions: "-rw-r--r--".to_string(),
            owner: "root".to_string(),
            group: "root".to_string(),
            modified_seconds: 1,
            modified_nanos: 0,
            symlink_target: None,
            metadata: BTreeMap::new(),
        });
        self.filesystem_requests.lock().unwrap().push(request);
        Ok(FilesystemResponse {
            success: true,
            entry,
            entries: Vec::new(),
            error: None,
        })
    }
}

fn test_client(runtime: Arc<RecordingRuntime>, home: &std::path::Path) -> A3sBoxClient {
    A3sBoxClient::with_execution_services(A3sBoxPaths::from_home(home), runtime.clone(), runtime)
}

#[tokio::test]
async fn e2b_style_rust_surface_supports_both_local_isolation_levels() {
    for isolation in [ExecutionIsolation::Microvm, ExecutionIsolation::Sandbox] {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(RecordingRuntime::new());
        let sandbox = Sandbox::create_with_client(
            test_client(Arc::clone(&runtime), temp.path()),
            SandboxCreateOptions::new("python:3.12-alpine")
                .timeout_seconds(120)
                .env("MODE", "test")
                .metadata("suite", "rust-sdk")
                .isolation(isolation),
        )
        .await
        .unwrap();

        assert_eq!(sandbox.id(), "local-rust-sdk-test");
        assert_eq!(sandbox.isolation(), isolation);
        assert_eq!(sandbox.info().state, ExecutionState::Running);

        let output = sandbox
            .commands
            .run_with_options(
                "python -c 'print(6 * 7)'",
                CommandRunOptions::default().cwd("/workspace"),
            )
            .await
            .unwrap();
        assert_eq!(output.stdout, "42\n");
        assert_eq!(output.exit_code, 0);

        let write = sandbox
            .files
            .write("/workspace/note.txt", b"hello")
            .await
            .unwrap();
        assert_eq!(write.size, 5);
        assert_eq!(
            sandbox
                .files
                .read_text("/workspace/note.txt")
                .await
                .unwrap(),
            "hello"
        );
        assert!(sandbox.files.exists("/workspace/note.txt").await.unwrap());

        sandbox.pause(true).await.unwrap();
        assert_eq!(sandbox.info().state, ExecutionState::Paused);
        sandbox.resume().await.unwrap();
        assert!(sandbox.is_running().await.unwrap());
        sandbox.kill().await.unwrap();
        assert!(!sandbox.is_running().await.unwrap());
        sandbox.kill().await.unwrap();

        let requests = runtime.create_requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].config.isolation, isolation);
        assert_eq!(requests[0].config.image, "python:3.12-alpine");
        assert_eq!(requests[0].config.resources.timeout, 120);
        assert_eq!(
            requests[0].config.extra_env,
            [("MODE".to_string(), "test".to_string())]
        );
        assert_eq!(
            requests[0].labels.get("suite").map(String::as_str),
            Some("rust-sdk")
        );
        drop(requests);

        let exec = runtime.exec_requests.lock().unwrap();
        assert_eq!(exec[0].cmd, ["/bin/sh", "-lc", "python -c 'print(6 * 7)'"]);
    }
}

#[tokio::test]
async fn sandbox_snapshot_api_uses_typed_runtime_managed_snapshots() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Arc::new(RecordingRuntime::new());
    let client = test_client(Arc::clone(&runtime), temp.path());
    let source_snapshot = ExecutionSnapshotId::new("ci-base-source").unwrap();
    let sandbox = Sandbox::create_with_client(
        client.clone(),
        SandboxCreateOptions::new("python:3.12-alpine")
            .isolation(ExecutionIsolation::Sandbox)
            .filesystem_snapshot(source_snapshot.clone()),
    )
    .await
    .unwrap();

    {
        let requests = runtime.create_requests.lock().unwrap();
        assert_eq!(
            requests[0].rootfs_snapshot_id.as_ref(),
            Some(&source_snapshot)
        );
    }

    let captured_id = ExecutionSnapshotId::new("ci-captured").unwrap();
    let captured = sandbox
        .create_filesystem_snapshot(captured_id.clone())
        .await
        .unwrap();
    assert_eq!(captured.snapshot_id, captured_id);
    assert_eq!(captured.size_bytes, 5);
    assert_eq!(
        client
            .execution_snapshot_size(&captured.snapshot_id)
            .await
            .unwrap(),
        Some(5)
    );

    sandbox.kill().await.unwrap();
    assert!(client
        .delete_execution_snapshot(&captured.snapshot_id)
        .await
        .unwrap());
    assert_eq!(
        client
            .execution_snapshot_size(&captured.snapshot_id)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn fluent_builders_configure_resources_and_stream_script_source() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Arc::new(RecordingRuntime::new());
    let client = test_client(Arc::clone(&runtime), temp.path());
    client
        .volume("build-cache")
        .label("purpose", "ci")
        .create()
        .unwrap();
    client
        .network("ci-net")
        .subnet("10.89.66.0/24")
        .create()
        .unwrap();

    let sandbox = client
        .sandbox("local/ci-base:latest")
        .cpus(4)
        .memory_mb(4096)
        .mount_named("build-cache", "/cache")
        .network(SandboxNetwork::bridge("ci-net"))
        .publish_tcp(8080, 80)
        .workdir("/workspace")
        .auto_remove(false)
        .start()
        .await
        .unwrap();

    let result = sandbox
        .script("print(6 * 7)\n")
        .interpreter(["python", "-"])
        .env("CI", "true")
        .cwd("/workspace")
        .run()
        .await
        .unwrap();
    assert_eq!(result.stdout, "42\n");

    let creates = runtime.create_requests.lock().unwrap();
    let request = &creates[0];
    assert_eq!(request.config.resources.vcpus, 4);
    assert_eq!(request.config.resources.memory_mb, 4096);
    assert_eq!(request.config.network.to_string(), "bridge:ci-net");
    assert_eq!(request.config.port_map, ["8080:80"]);
    assert_eq!(request.policy.volume_names, ["build-cache"]);
    assert!(!request.policy.auto_remove);
    drop(creates);

    let exec = runtime.exec_requests.lock().unwrap();
    assert_eq!(exec[0].cmd, ["python", "-"]);
    assert_eq!(exec[0].stdin.as_deref(), Some(b"print(6 * 7)\n".as_slice()));
    assert_eq!(exec[0].env, ["CI=true"]);
    assert_eq!(exec[0].working_dir.as_deref(), Some("/workspace"));
}

#[test]
fn local_sandbox_handle_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Sandbox>();
}
