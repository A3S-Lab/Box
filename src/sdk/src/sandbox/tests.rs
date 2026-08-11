use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use a3s_box_core::log::LogEntry;
use a3s_box_core::pty::PtyRequest;
use a3s_box_core::{
    resolve_execution, BoxConfig, CreateExecutionRequest, ExecOutput, ExecRequest,
    ExecutionEventBatch, ExecutionEventKind, ExecutionEventsRequest, ExecutionGeneration,
    ExecutionId, ExecutionIsolation, ExecutionLease, ExecutionManager, ExecutionManagerError,
    ExecutionManagerResult, ExecutionProcess, ExecutionReservation, ExecutionRuntimeEvent,
    ExecutionSessionManager, ExecutionSnapshot, ExecutionSnapshotId, ExecutionState,
    ExecutionStatus, FileOp, FileRequest, FileResponse, FilesystemEntry, FilesystemEntryKind,
    FilesystemOp, FilesystemRequest, FilesystemResponse, KillOutcome, OperationId,
    ReconcileOutcome, RestartExecutionOptions,
};
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use chrono::Utc;
use futures::StreamExt;

use super::{
    ArtifactExportOptions, CommandRunOptions, FilesystemOptions, Sandbox, SandboxCreateOptions,
    SandboxEventStreamOptions, SandboxLogOptions, SandboxNetwork, SandboxRestartOptions,
    MAX_ARTIFACT_BYTES,
};
use crate::{A3sBoxClient, A3sBoxPaths, ClientError};

#[derive(Debug)]
struct RecordingRuntime {
    config: Mutex<Option<BoxConfig>>,
    generation: Mutex<ExecutionGeneration>,
    state: Mutex<ExecutionState>,
    removed: Mutex<bool>,
    create_requests: Mutex<Vec<CreateExecutionRequest>>,
    exec_requests: Mutex<Vec<ExecRequest>>,
    file_requests: Mutex<Vec<FileRequest>>,
    filesystem_requests: Mutex<Vec<FilesystemRequest>>,
    download_response: Mutex<FileResponse>,
    stat_entry: Mutex<FilesystemEntry>,
    snapshot_requests: Mutex<Vec<ExecutionSnapshotId>>,
    log_requests: Mutex<Vec<ExecutionGeneration>>,
    restart_requests: Mutex<Vec<(ExecutionGeneration, OperationId, RestartExecutionOptions)>>,
    kill_requests: Mutex<Vec<ExecutionGeneration>>,
    remove_requests: Mutex<Vec<ExecutionGeneration>>,
    logs: Mutex<Vec<LogEntry>>,
    event_requests: Mutex<VecDeque<(ExecutionGeneration, ExecutionEventsRequest)>>,
    runtime_events: Mutex<Vec<ExecutionRuntimeEvent>>,
}

impl RecordingRuntime {
    fn new() -> Self {
        Self {
            config: Mutex::new(None),
            generation: Mutex::new(ExecutionGeneration::INITIAL),
            state: Mutex::new(ExecutionState::Created),
            removed: Mutex::new(false),
            create_requests: Mutex::new(Vec::new()),
            exec_requests: Mutex::new(Vec::new()),
            file_requests: Mutex::new(Vec::new()),
            filesystem_requests: Mutex::new(Vec::new()),
            download_response: Mutex::new(FileResponse {
                success: true,
                data: Some(STANDARD.encode(b"hello")),
                size: 5,
                error: None,
            }),
            stat_entry: Mutex::new(FilesystemEntry {
                name: "note.txt".to_string(),
                kind: FilesystemEntryKind::File,
                path: "/workspace/note.txt".to_string(),
                size: 5,
                mode: 0o644,
                permissions: "-rw-r--r--".to_string(),
                owner: "root".to_string(),
                group: "root".to_string(),
                modified_seconds: 1,
                modified_nanos: 0,
                symlink_target: None,
                metadata: BTreeMap::new(),
            }),
            snapshot_requests: Mutex::new(Vec::new()),
            log_requests: Mutex::new(Vec::new()),
            restart_requests: Mutex::new(Vec::new()),
            kill_requests: Mutex::new(Vec::new()),
            remove_requests: Mutex::new(Vec::new()),
            logs: Mutex::new(vec![
                LogEntry {
                    log: "first\n".to_string(),
                    stream: "stdout".to_string(),
                    time: "2026-07-23T00:00:00Z".to_string(),
                },
                LogEntry {
                    log: "second\n".to_string(),
                    stream: "stderr".to_string(),
                    time: "2026-07-23T00:00:01Z".to_string(),
                },
            ]),
            event_requests: Mutex::new(VecDeque::new()),
            runtime_events: Mutex::new(vec![
                ExecutionRuntimeEvent {
                    sequence: 2,
                    timestamp_unix_ns: 1_700_000_000_000_000_002,
                    process_id: None,
                    kind: ExecutionEventKind::ContainerStarted,
                    attributes: BTreeMap::new(),
                },
                ExecutionRuntimeEvent {
                    sequence: 5,
                    timestamp_unix_ns: 1_700_000_000_000_000_005,
                    process_id: Some("init".to_string()),
                    kind: ExecutionEventKind::ProcessStarted,
                    attributes: BTreeMap::new(),
                },
                ExecutionRuntimeEvent {
                    sequence: 9,
                    timestamp_unix_ns: 1_700_000_000_000_000_009,
                    process_id: None,
                    kind: ExecutionEventKind::ResourcesUpdated,
                    attributes: BTreeMap::new(),
                },
            ]),
        }
    }

    fn execution_id() -> ExecutionId {
        ExecutionId::new("local-rust-sdk-test").unwrap()
    }

    fn lease(&self) -> ExecutionLease {
        let config = self.config.lock().unwrap().clone().unwrap();
        ExecutionLease {
            execution_id: Self::execution_id(),
            generation: *self.generation.lock().unwrap(),
            plan: resolve_execution(&config).unwrap(),
            resources: config.resources,
            started_at: Utc::now(),
        }
    }

    fn require_current_generation(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<()> {
        if execution_id != &Self::execution_id() || *self.removed.lock().unwrap() {
            return Err(ExecutionManagerError::NotFound(execution_id.clone()));
        }
        let current = *self.generation.lock().unwrap();
        if current != generation {
            return Err(ExecutionManagerError::Conflict {
                execution_id: execution_id.clone(),
                message: format!(
                    "expected generation {}, received {}",
                    current.get(),
                    generation.get()
                ),
            });
        }
        Ok(())
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
        *self.generation.lock().unwrap() = ExecutionGeneration::INITIAL;
        *self.removed.lock().unwrap() = false;
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
        *self.generation.lock().unwrap() = ExecutionGeneration::INITIAL;
        *self.removed.lock().unwrap() = false;
        self.create_requests.lock().unwrap().push(request);
        *self.state.lock().unwrap() = ExecutionState::Running;
        Ok(self.lease())
    }

    async fn inspect(&self, execution_id: &ExecutionId) -> ExecutionManagerResult<ExecutionStatus> {
        if execution_id != &Self::execution_id() || *self.removed.lock().unwrap() {
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

    async fn read_logs(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<Vec<LogEntry>> {
        self.require_current_generation(execution_id, generation)?;
        self.log_requests.lock().unwrap().push(generation);
        Ok(self.logs.lock().unwrap().clone())
    }

    async fn events(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        request: ExecutionEventsRequest,
    ) -> ExecutionManagerResult<ExecutionEventBatch> {
        self.require_current_generation(execution_id, generation)?;
        request.validate()?;
        self.event_requests
            .lock()
            .unwrap()
            .push_back((generation, request.clone()));
        let events = self
            .runtime_events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.sequence > request.after_sequence)
            .take(request.limit as usize)
            .cloned()
            .collect::<Vec<_>>();
        let next_sequence = events
            .last()
            .map_or(request.after_sequence, |event| event.sequence);
        Ok(ExecutionEventBatch {
            execution_id: execution_id.clone(),
            generation,
            events,
            next_sequence,
        })
    }

    async fn create_filesystem_snapshot(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        snapshot_id: &ExecutionSnapshotId,
    ) -> ExecutionManagerResult<ExecutionSnapshot> {
        self.require_current_generation(execution_id, generation)?;
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
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        _keep_memory: bool,
    ) -> ExecutionManagerResult<ExecutionLease> {
        self.require_current_generation(execution_id, generation)?;
        *self.state.lock().unwrap() = ExecutionState::Paused;
        Ok(self.lease())
    }

    async fn resume(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<ExecutionLease> {
        self.require_current_generation(execution_id, generation)?;
        *self.state.lock().unwrap() = ExecutionState::Running;
        Ok(self.lease())
    }

    async fn restart_with_options(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        operation_id: &OperationId,
        options: RestartExecutionOptions,
    ) -> ExecutionManagerResult<ExecutionLease> {
        if self
            .restart_requests
            .lock()
            .unwrap()
            .iter()
            .any(|(_, existing, _)| existing == operation_id)
        {
            return Ok(self.lease());
        }
        self.require_current_generation(execution_id, generation)?;
        self.restart_requests
            .lock()
            .unwrap()
            .push((generation, operation_id.clone(), options));
        let next = ExecutionGeneration::new(generation.get() + 1)?;
        *self.generation.lock().unwrap() = next;
        *self.state.lock().unwrap() = ExecutionState::Running;
        Ok(self.lease())
    }

    async fn kill(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<KillOutcome> {
        self.require_current_generation(execution_id, generation)?;
        self.kill_requests.lock().unwrap().push(generation);
        if *self.state.lock().unwrap() == ExecutionState::Stopped {
            return Ok(KillOutcome::AlreadyStopped);
        }
        *self.state.lock().unwrap() = ExecutionState::Stopped;
        Ok(KillOutcome::Killed)
    }

    async fn remove(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<bool> {
        if *self.removed.lock().unwrap() {
            return Ok(false);
        }
        self.require_current_generation(execution_id, generation)?;
        if !matches!(
            *self.state.lock().unwrap(),
            ExecutionState::Created | ExecutionState::Stopped | ExecutionState::Failed
        ) {
            return Err(ExecutionManagerError::Conflict {
                execution_id: execution_id.clone(),
                message: "execution must be terminal before removal".to_string(),
            });
        }
        self.remove_requests.lock().unwrap().push(generation);
        *self.removed.lock().unwrap() = true;
        Ok(true)
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
            FileOp::Download => self.download_response.lock().unwrap().clone(),
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
        let entry = (request.op == FilesystemOp::Stat).then(|| {
            let mut entry = self.stat_entry.lock().unwrap().clone();
            entry.path.clone_from(&request.path);
            entry
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
async fn local_sandbox_surface_supports_both_isolation_levels() {
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
async fn artifact_export_is_bounded_hashed_and_never_overwrites() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Arc::new(RecordingRuntime::new());
    let sandbox = Sandbox::create_with_client(
        test_client(Arc::clone(&runtime), temp.path()),
        SandboxCreateOptions::new("alpine:3.20"),
    )
    .await
    .unwrap();
    let destination = temp.path().join("artifact.bin");

    let artifact = sandbox
        .files
        .export_with_options(
            "/workspace/note.txt",
            ArtifactExportOptions::default()
                .max_bytes(5)
                .destination(&destination)
                .user("1000"),
        )
        .await
        .unwrap();
    assert_eq!(artifact.path, "/workspace/note.txt");
    assert_eq!(artifact.data, b"hello");
    assert_eq!(artifact.size, 5);
    assert_eq!(
        artifact.sha256,
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
    assert_eq!(artifact.host_path.as_deref(), Some(destination.as_path()));
    assert_eq!(std::fs::read(&destination).unwrap(), b"hello");

    std::fs::write(&destination, b"keep").unwrap();
    let error = sandbox
        .files
        .export_with_options(
            "/workspace/note.txt",
            ArtifactExportOptions::default().destination(&destination),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ClientError::State(_)));
    assert_eq!(std::fs::read(&destination).unwrap(), b"keep");

    let filesystem_requests = runtime.filesystem_requests.lock().unwrap();
    let file_requests = runtime.file_requests.lock().unwrap();
    assert_eq!(filesystem_requests[0].user.as_deref(), Some("1000"));
    assert_eq!(file_requests[0].user.as_deref(), Some("1000"));
    assert_eq!(file_requests[0].max_bytes, Some(5));
}

#[tokio::test]
async fn artifact_export_rejects_invalid_sources_and_racing_reads() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Arc::new(RecordingRuntime::new());
    let sandbox = Sandbox::create_with_client(
        test_client(Arc::clone(&runtime), temp.path()),
        SandboxCreateOptions::new("alpine:3.20"),
    )
    .await
    .unwrap();

    for max_bytes in [0, MAX_ARTIFACT_BYTES + 1] {
        let error = sandbox
            .files
            .export_with_options(
                "/workspace/output",
                ArtifactExportOptions::default().max_bytes(max_bytes),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ClientError::Validation(_)));
    }
    for (path, options) in [
        ("  ", ArtifactExportOptions::default()),
        (
            "/workspace/output",
            ArtifactExportOptions::default().destination("  "),
        ),
    ] {
        let error = sandbox
            .files
            .export_with_options(path, options)
            .await
            .unwrap_err();
        assert!(matches!(error, ClientError::Validation(_)));
    }
    assert!(runtime.filesystem_requests.lock().unwrap().is_empty());
    assert!(runtime.file_requests.lock().unwrap().is_empty());

    runtime.stat_entry.lock().unwrap().kind = FilesystemEntryKind::Directory;
    let error = sandbox.files.export("/workspace/output").await.unwrap_err();
    assert!(matches!(error, ClientError::Validation(_)));
    assert!(runtime.file_requests.lock().unwrap().is_empty());

    {
        let mut entry = runtime.stat_entry.lock().unwrap();
        entry.kind = FilesystemEntryKind::File;
        entry.size = 6;
    }
    let error = sandbox
        .files
        .export_with_options(
            "/workspace/output",
            ArtifactExportOptions::default().max_bytes(5),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ClientError::Validation(_)));
    assert!(runtime.file_requests.lock().unwrap().is_empty());

    runtime.stat_entry.lock().unwrap().size = 5;
    *runtime.download_response.lock().unwrap() = FileResponse {
        success: true,
        data: Some(STANDARD.encode(b"sixsix")),
        size: 6,
        error: None,
    };
    let error = sandbox
        .files
        .read_bounded_with_options("/workspace/output", 5, FilesystemOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(error, ClientError::Guest(_)));

    *runtime.download_response.lock().unwrap() = FileResponse {
        success: true,
        data: Some(STANDARD.encode(b"hello")),
        size: 6,
        error: None,
    };
    let error = sandbox.files.export("/workspace/output").await.unwrap_err();
    assert!(matches!(error, ClientError::Guest(_)));

    *runtime.download_response.lock().unwrap() = FileResponse {
        success: true,
        data: Some(STANDARD.encode(b"four")),
        size: 4,
        error: None,
    };
    let error = sandbox.files.export("/workspace/output").await.unwrap_err();
    assert!(matches!(error, ClientError::Guest(_)));
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

#[tokio::test]
async fn sandbox_lifecycle_logs_and_removal_share_generation_fencing() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Arc::new(RecordingRuntime::new());
    let sandbox = Sandbox::create_with_client(
        test_client(Arc::clone(&runtime), temp.path()),
        SandboxCreateOptions::new("alpine:3.20")
            .isolation(ExecutionIsolation::Sandbox)
            .auto_remove(false),
    )
    .await
    .unwrap();

    sandbox.stop().await.unwrap();
    assert_eq!(sandbox.info().state, ExecutionState::Stopped);
    sandbox.stop().await.unwrap();

    let operation_id = OperationId::new("sdk-test-restart").unwrap();
    sandbox
        .restart(
            SandboxRestartOptions::default()
                .operation_id(operation_id.clone())
                .stop_timeout_seconds(7),
        )
        .await
        .unwrap();
    assert_eq!(sandbox.info().generation, 2);
    assert_eq!(sandbox.info().state, ExecutionState::Running);

    let logs = sandbox.logs(SandboxLogOptions::tail(1)).await.unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].stream, "stderr");
    assert_eq!(logs[0].log, "second\n");

    sandbox.stop().await.unwrap();
    sandbox.remove().await.unwrap();
    sandbox.remove().await.unwrap();
    assert!(!sandbox.is_running().await.unwrap());

    assert_eq!(
        *runtime.kill_requests.lock().unwrap(),
        [
            ExecutionGeneration::INITIAL,
            ExecutionGeneration::new(2).unwrap()
        ]
    );
    assert_eq!(
        *runtime.remove_requests.lock().unwrap(),
        [ExecutionGeneration::new(2).unwrap()]
    );
    let restarts = runtime.restart_requests.lock().unwrap();
    assert_eq!(restarts.len(), 1);
    assert_eq!(restarts[0].0, ExecutionGeneration::INITIAL);
    assert_eq!(restarts[0].1, operation_id);
    assert_eq!(restarts[0].2.stop_timeout_secs, Some(7));
}

#[tokio::test]
async fn sandbox_logs_reject_invalid_bounds_and_stale_generations() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Arc::new(RecordingRuntime::new());
    let client = test_client(Arc::clone(&runtime), temp.path());
    let sandbox = Sandbox::create_with_client(
        client.clone(),
        SandboxCreateOptions::new("alpine:3.20")
            .isolation(ExecutionIsolation::Sandbox)
            .auto_remove(false),
    )
    .await
    .unwrap();

    for tail in [0, 10_001] {
        let error = sandbox
            .logs(SandboxLogOptions::tail(tail))
            .await
            .unwrap_err();
        assert!(matches!(error, ClientError::Validation(_)));
    }
    assert!(runtime.log_requests.lock().unwrap().is_empty());

    sandbox
        .restart(
            SandboxRestartOptions::default()
                .operation_id(OperationId::new("sdk-stale-generation").unwrap()),
        )
        .await
        .unwrap();
    let error = client
        .read_execution_logs(
            &RecordingRuntime::execution_id(),
            ExecutionGeneration::INITIAL,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ClientError::Execution(ExecutionManagerError::Conflict { .. })
    ));
    assert!(runtime.log_requests.lock().unwrap().is_empty());

    sandbox.kill().await.unwrap();
}

#[tokio::test]
async fn sandbox_event_stream_is_backpressured_and_generation_fenced() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Arc::new(RecordingRuntime::new());
    let sandbox = Sandbox::create_with_client(
        test_client(Arc::clone(&runtime), temp.path()),
        SandboxCreateOptions::new("alpine:3.20"),
    )
    .await
    .unwrap();

    let mut stream = sandbox
        .stream_events(
            SandboxEventStreamOptions::default()
                .batch_items(2)
                .wait_timeout_ms(1),
        )
        .unwrap();
    assert_eq!(stream.next().await.unwrap().unwrap().sequence, 2);
    assert_eq!(stream.next().await.unwrap().unwrap().sequence, 5);
    assert_eq!(stream.next().await.unwrap().unwrap().sequence, 9);

    {
        let requests = runtime.event_requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].1.after_sequence, 0);
        assert_eq!(requests[1].1.after_sequence, 5);
        assert_eq!(requests[0].1.wait_timeout_ms, Some(1));
    }

    sandbox.pause(true).await.unwrap();
    let paused = sandbox
        .events(ExecutionEventsRequest {
            after_sequence: 9,
            limit: 1,
            wait_timeout_ms: Some(1),
        })
        .await
        .unwrap();
    assert!(paused.events.is_empty());
    sandbox.resume().await.unwrap();

    let mut fenced = sandbox
        .stream_events(
            SandboxEventStreamOptions::default()
                .batch_items(1)
                .wait_timeout_ms(1),
        )
        .unwrap();
    assert_eq!(fenced.next().await.unwrap().unwrap().sequence, 2);
    sandbox
        .restart(
            SandboxRestartOptions::default()
                .operation_id(OperationId::new("event-stream-restart").unwrap()),
        )
        .await
        .unwrap();
    let error = fenced.next().await.unwrap().unwrap_err();
    assert!(matches!(
        error,
        ClientError::Execution(ExecutionManagerError::Conflict { .. })
    ));
    assert!(fenced.next().await.is_none());
}

#[tokio::test]
async fn sandbox_event_stream_rejects_unbounded_polling_options() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Arc::new(RecordingRuntime::new());
    let sandbox = Sandbox::create_with_client(
        test_client(Arc::clone(&runtime), temp.path()),
        SandboxCreateOptions::new("alpine:3.20"),
    )
    .await
    .unwrap();

    for options in [
        SandboxEventStreamOptions::default().batch_items(0),
        SandboxEventStreamOptions::default().wait_timeout_ms(0),
    ] {
        let error = match sandbox.stream_events(options) {
            Ok(_) => panic!("invalid event stream options must fail"),
            Err(error) => error,
        };
        assert!(matches!(error, ClientError::Validation(_)));
    }
    assert!(runtime.event_requests.lock().unwrap().is_empty());
}

#[test]
fn local_sandbox_handle_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Sandbox>();
}
