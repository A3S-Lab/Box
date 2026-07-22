use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use a3s_box_core::{
    BoxConfig, CreateExecutionRequest, ExecEvent, ExecRequest, ExecutionGeneration,
    ExecutionHealthCheck, ExecutionId, ExecutionIsolation, ExecutionManager, ExecutionManagerError,
    ExecutionManagerResult, ExecutionRecordPolicy, ExecutionRestartPolicy, ExecutionSessionManager,
    ExecutionSnapshotId, ExecutionState, KillExecutionOptions, KillOutcome, NetworkMode,
    OperationId, ReconcileOutcome, RestartExecutionOptions, SnapshotImageConfig,
};
use async_trait::async_trait;
use chrono::{TimeZone, Utc};

use super::*;
use crate::{ManagedExecutionState, ManagedExecutionStore};

#[derive(Clone)]
struct FakeExecution {
    state: ExecutionState,
    handle: LocalExecutionHandle,
    exit_code: Option<i32>,
}

#[derive(Default)]
struct FakeBackend {
    executions: Mutex<HashMap<String, FakeExecution>>,
    start_attempts: AtomicUsize,
    starts: AtomicUsize,
    pauses: AtomicUsize,
    resumes: AtomicUsize,
    kills: AtomicUsize,
    quiescent_rootfs_prepares: AtomicUsize,
    quiescent_rootfs_cleanups: AtomicUsize,
    fail_quiescent_rootfs_cleanup: AtomicBool,
    fail_start: AtomicBool,
    fail_start_after_effect: AtomicBool,
    fail_kill: AtomicBool,
    fail_kill_after_effect: AtomicBool,
    fail_pause: AtomicBool,
    fail_pause_after_effect: AtomicBool,
    last_keep_memory: Mutex<Option<bool>>,
    last_kill_signal: Mutex<Option<Option<i32>>>,
    last_kill_timeout: Mutex<Option<Option<u64>>>,
    last_restart_timeout: Mutex<Option<Option<u64>>>,
}

impl FakeBackend {
    fn handle(record: &BoxRecord) -> LocalExecutionHandle {
        LocalExecutionHandle {
            started_at: Utc.with_ymd_and_hms(2026, 7, 14, 12, 30, 0).unwrap(),
            pid: Some(4242),
            pid_start_time: Some(777),
            exec_socket_path: record.box_dir.join("sockets/exec.sock"),
            console_log: record.box_dir.join("logs/console.log"),
            anonymous_volumes: vec!["anonymous-1".to_string()],
        }
    }

    fn execution_id(record: &BoxRecord) -> ExecutionId {
        ExecutionId::new(record.id.clone()).unwrap()
    }

    fn stop_externally(&self, execution_id: &ExecutionId, exit_code: i32) {
        let mut executions = self.executions.lock().unwrap();
        let execution = executions.get_mut(execution_id.as_str()).unwrap();
        execution.state = ExecutionState::Stopped;
        execution.exit_code = Some(exit_code);
    }

    fn fail_externally(&self, execution_id: &ExecutionId, exit_code: i32) {
        let mut executions = self.executions.lock().unwrap();
        let execution = executions.get_mut(execution_id.as_str()).unwrap();
        execution.state = ExecutionState::Failed;
        execution.exit_code = Some(exit_code);
    }
}

#[async_trait]
impl LocalExecutionBackend for FakeBackend {
    async fn start(&self, record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
        self.start_attempts.fetch_add(1, Ordering::Relaxed);
        let mut executions = self.executions.lock().unwrap();
        if let Some(execution) = executions.get(&record.id) {
            if matches!(
                execution.state,
                ExecutionState::Running | ExecutionState::Paused
            ) {
                return Ok(execution.handle.clone());
            }
        }
        self.starts.fetch_add(1, Ordering::Relaxed);
        if self.fail_start.load(Ordering::Relaxed) {
            executions.remove(&record.id);
            return Err(ExecutionManagerError::Unavailable(
                "fake start is unavailable".to_string(),
            ));
        }
        #[cfg(target_os = "linux")]
        write_fake_sandbox_bundle(record)?;
        write_fake_resolved_image_config(record)?;
        let handle = Self::handle(record);
        executions.insert(
            record.id.clone(),
            FakeExecution {
                state: ExecutionState::Running,
                handle: handle.clone(),
                exit_code: None,
            },
        );
        if self.fail_start_after_effect.load(Ordering::Relaxed) {
            return Err(ExecutionManagerError::Unavailable(
                "fake start response was lost".to_string(),
            ));
        }
        Ok(handle)
    }

    async fn inspect(
        &self,
        record: &BoxRecord,
    ) -> ExecutionManagerResult<LocalExecutionObservation> {
        let executions = self.executions.lock().unwrap();
        let execution = executions
            .get(&record.id)
            .ok_or_else(|| ExecutionManagerError::NotFound(Self::execution_id(record)))?;
        Ok(LocalExecutionObservation {
            state: execution.state,
            handle: matches!(
                execution.state,
                ExecutionState::Running | ExecutionState::Paused
            )
            .then(|| execution.handle.clone()),
            exit_code: execution.exit_code,
        })
    }

    async fn pause(
        &self,
        record: &BoxRecord,
        keep_memory: bool,
    ) -> ExecutionManagerResult<LocalExecutionHandle> {
        self.pauses.fetch_add(1, Ordering::Relaxed);
        *self.last_keep_memory.lock().unwrap() = Some(keep_memory);
        if self.fail_pause.load(Ordering::Relaxed) {
            return Err(ExecutionManagerError::Unavailable(
                "fake pause is unavailable".to_string(),
            ));
        }
        let mut executions = self.executions.lock().unwrap();
        let execution = executions
            .get_mut(&record.id)
            .ok_or_else(|| ExecutionManagerError::NotFound(Self::execution_id(record)))?;
        if execution.state != ExecutionState::Running {
            return Err(ExecutionManagerError::Conflict {
                execution_id: Self::execution_id(record),
                message: "fake execution is not running".to_string(),
            });
        }
        execution.state = ExecutionState::Paused;
        if self.fail_pause_after_effect.load(Ordering::Relaxed) {
            return Err(ExecutionManagerError::Unavailable(
                "fake pause response was lost".to_string(),
            ));
        }
        Ok(execution.handle.clone())
    }

    async fn resume(&self, record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
        self.resumes.fetch_add(1, Ordering::Relaxed);
        let mut executions = self.executions.lock().unwrap();
        let execution = executions
            .get_mut(&record.id)
            .ok_or_else(|| ExecutionManagerError::NotFound(Self::execution_id(record)))?;
        if execution.state != ExecutionState::Paused {
            return Err(ExecutionManagerError::Conflict {
                execution_id: Self::execution_id(record),
                message: "fake execution is not paused".to_string(),
            });
        }
        execution.state = ExecutionState::Running;
        Ok(execution.handle.clone())
    }

    async fn prepare_quiescent_rootfs(&self, _record: &BoxRecord) -> ExecutionManagerResult<()> {
        self.quiescent_rootfs_prepares
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn cleanup_quiescent_rootfs(&self, _record: &BoxRecord) -> ExecutionManagerResult<()> {
        self.quiescent_rootfs_cleanups
            .fetch_add(1, Ordering::Relaxed);
        if self.fail_quiescent_rootfs_cleanup.load(Ordering::Relaxed) {
            return Err(ExecutionManagerError::Unavailable(
                "fake quiescent rootfs cleanup is unavailable".to_string(),
            ));
        }
        Ok(())
    }

    async fn stop_for_restart(
        &self,
        record: &BoxRecord,
        timeout_secs: Option<u64>,
    ) -> ExecutionManagerResult<KillOutcome> {
        *self.last_restart_timeout.lock().unwrap() = Some(timeout_secs);
        self.kill(record).await
    }

    async fn kill(&self, record: &BoxRecord) -> ExecutionManagerResult<KillOutcome> {
        self.kills.fetch_add(1, Ordering::Relaxed);
        *self.last_kill_signal.lock().unwrap() = Some(
            record
                .stop_signal
                .as_deref()
                .map(a3s_box_core::vmm::parse_signal_name),
        );
        *self.last_kill_timeout.lock().unwrap() = Some(record.stop_timeout);
        if self.fail_kill.load(Ordering::Relaxed) {
            return Err(ExecutionManagerError::Unavailable(
                "fake kill is unavailable".to_string(),
            ));
        }
        let mut executions = self.executions.lock().unwrap();
        let execution = executions
            .get_mut(&record.id)
            .ok_or_else(|| ExecutionManagerError::NotFound(Self::execution_id(record)))?;
        if execution.state == ExecutionState::Stopped {
            return Ok(KillOutcome::AlreadyStopped);
        }
        execution.state = ExecutionState::Stopped;
        if self.fail_kill_after_effect.load(Ordering::Relaxed) {
            return Err(ExecutionManagerError::Unavailable(
                "fake kill response was lost".to_string(),
            ));
        }
        Ok(KillOutcome::Killed)
    }
}

#[cfg(target_os = "linux")]
fn write_fake_sandbox_bundle(record: &BoxRecord) -> ExecutionManagerResult<()> {
    let bundle = record.box_dir.join("sandbox/bundle");
    std::fs::create_dir_all(&bundle).map_err(|error| {
        ExecutionManagerError::Internal(format!("failed to create fake OCI bundle: {error}"))
    })?;
    let config = serde_json::json!({
        "ociVersion": "1.1.0",
        "root": {
            "path": record.box_dir.join("rootfs"),
            "readonly": false
        },
        "linux": {
            "uidMappings": [{
                "containerID": 0,
                "hostID": unsafe { libc::geteuid() },
                "size": 1
            }],
            "gidMappings": [{
                "containerID": 0,
                "hostID": unsafe { libc::getegid() },
                "size": 1
            }]
        }
    });
    std::fs::write(
        bundle.join("config.json"),
        serde_json::to_vec(&config).map_err(|error| {
            ExecutionManagerError::Internal(format!("failed to encode fake OCI bundle: {error}"))
        })?,
    )
    .map_err(|error| {
        ExecutionManagerError::Internal(format!("failed to write fake OCI bundle: {error}"))
    })
}

fn write_fake_resolved_image_config(record: &BoxRecord) -> ExecutionManagerResult<()> {
    let config = SnapshotImageConfig {
        entrypoint: Some(vec!["/usr/local/bin/envd".to_string()]),
        cmd: Some(vec!["--port".to_string(), "49983".to_string()]),
        env: vec![("PATH".to_string(), "/usr/local/bin:/usr/bin".to_string())],
        working_dir: Some("/home/user".to_string()),
        user: Some("1000:1000".to_string()),
        ..Default::default()
    };
    std::fs::create_dir_all(&record.box_dir).map_err(|error| {
        ExecutionManagerError::Internal(format!(
            "failed to create fake resolved image configuration directory: {error}"
        ))
    })?;
    std::fs::write(
        record.box_dir.join(crate::RESOLVED_IMAGE_CONFIG_FILE),
        serde_json::to_vec_pretty(&config).map_err(|error| {
            ExecutionManagerError::Internal(format!(
                "failed to encode fake resolved image configuration: {error}"
            ))
        })?,
    )
    .map_err(|error| {
        ExecutionManagerError::Internal(format!(
            "failed to write fake resolved image configuration: {error}"
        ))
    })
}

fn harness() -> (tempfile::TempDir, LocalExecutionManager, Arc<FakeBackend>) {
    let directory = tempfile::tempdir().unwrap();
    let backend = Arc::new(FakeBackend::default());
    let manager = LocalExecutionManager::new(
        directory.path().join("boxes.json"),
        directory.path().join("home"),
        backend.clone(),
    );
    (directory, manager, backend)
}

fn request(external_id: &str) -> CreateExecutionRequest {
    let mut labels = BTreeMap::new();
    labels.insert("purpose".to_string(), "test".to_string());
    CreateExecutionRequest {
        external_sandbox_id: external_id.to_string(),
        config: BoxConfig {
            image: "alpine:3.20".to_string(),
            isolation: ExecutionIsolation::Sandbox,
            network: NetworkMode::None,
            resources: a3s_box_core::ResourceConfig {
                vcpus: 1,
                memory_mb: 128,
                disk_mb: 512,
                timeout: 300,
            },
            ..Default::default()
        },
        labels,
        policy: Default::default(),
        rootfs_snapshot_id: None,
    }
}

fn operation(value: &str) -> OperationId {
    OperationId::new(value).unwrap()
}

fn persisted(manager: &LocalExecutionManager, execution_id: &ExecutionId) -> BoxRecord {
    ManagedExecutionStore::new(manager.state_path().to_path_buf())
        .get(execution_id)
        .unwrap()
        .unwrap()
}

async fn reserve_starting(
    manager: &LocalExecutionManager,
    execution_id: &ExecutionId,
    operation_id: &OperationId,
) -> BoxRecord {
    let record = build_managed_record(
        &manager.home_dir,
        execution_id,
        operation_id.clone(),
        request("external-recovery-id"),
        Utc::now(),
    )
    .unwrap();
    let record = manager.reserve(record).await.unwrap().into_record();
    manager
        .transition(
            &record,
            ManagedExecutionState::Created,
            ManagedExecutionState::Starting,
            RuntimeUpdate::None,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn create_persists_trusted_identity_and_returns_running_lease() {
    let (directory, manager, backend) = harness();
    let operation_id = operation("operation-1");

    let lease = manager
        .create_and_start(request("external/../../sandbox"), &operation_id)
        .await
        .unwrap();
    let status = manager.inspect(&lease.execution_id).await.unwrap();
    let record = persisted(&manager, &lease.execution_id);

    assert_eq!(status.state, ExecutionState::Running);
    assert_eq!(status.generation, ExecutionGeneration::INITIAL);
    assert_eq!(backend.starts.load(Ordering::Relaxed), 1);
    assert_eq!(
        record
            .managed_execution
            .as_ref()
            .unwrap()
            .request
            .external_sandbox_id,
        "external/../../sandbox"
    );
    assert!(!record.name.contains("external"));
    assert!(record
        .box_dir
        .starts_with(directory.path().join("home/boxes")));
    assert_eq!(record.pid, Some(4242));
    assert_eq!(record.anonymous_volumes, vec!["anonymous-1"]);
}

#[tokio::test]
async fn create_rejects_unsupported_microvm_security_before_store_or_backend() {
    let (_directory, manager, backend) = harness();

    for (index, (security_option, expected)) in [
        ("apparmor=runtime/default", "AppArmor"),
        ("label=type:container_t", "SELinux"),
        ("seccomp=/profiles/restricted.json", "custom seccomp"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut create_request = request(&format!("unsupported-security-{index}"));
        create_request.config.isolation = ExecutionIsolation::Microvm;
        create_request.config.security_opt = vec![security_option.to_string()];

        let error = manager
            .create_and_start(
                create_request,
                &operation(&format!("unsupported-security-operation-{index}")),
            )
            .await
            .unwrap_err();

        assert!(matches!(
            &error,
            ExecutionManagerError::InvalidRequest(message) if message.contains(expected)
        ));
        assert!(!manager.state_path().exists());
        assert_eq!(backend.starts.load(Ordering::Relaxed), 0);
        assert!(backend.executions.lock().unwrap().is_empty());
    }
}

#[cfg(windows)]
#[tokio::test]
async fn create_rejects_health_check_before_store_or_backend() {
    let (_directory, manager, backend) = harness();
    let mut create_request = request("unsupported-windows-health");
    create_request.policy.health_check = Some(ExecutionHealthCheck {
        cmd: vec!["true".to_string()],
        interval_secs: 30,
        timeout_secs: 5,
        retries: 3,
        start_period_secs: 0,
    });
    create_request.rootfs_snapshot_id = Some(ExecutionSnapshotId::new("restore-source").unwrap());

    let error = manager
        .create(
            create_request,
            &operation("unsupported-windows-health-operation"),
        )
        .await
        .unwrap_err();

    assert!(matches!(
        &error,
        ExecutionManagerError::InvalidRequest(message)
            if message.contains("health checks are not supported on Windows")
    ));
    assert!(!manager.state_path().exists());
    assert_eq!(backend.starts.load(Ordering::Relaxed), 0);
    assert!(backend.executions.lock().unwrap().is_empty());
}

#[cfg(windows)]
#[tokio::test]
async fn start_rejects_a_persisted_health_check_before_claim_or_backend() {
    let (_directory, manager, backend) = harness();
    let execution_id = ExecutionId::new("persisted-windows-health").unwrap();
    let mut create_request = request("persisted-windows-health");
    create_request.policy.health_check = Some(ExecutionHealthCheck {
        cmd: vec!["true".to_string()],
        interval_secs: 30,
        timeout_secs: 5,
        retries: 3,
        start_period_secs: 0,
    });
    // Disabled health metadata is safe to persist on Windows. Flip the stored
    // effective policy afterward to model a record written by an older client.
    create_request.policy.healthcheck_disabled = true;
    let mut record = build_managed_record(
        &manager.home_dir,
        &execution_id,
        operation("persisted-windows-health-operation"),
        create_request,
        Utc::now(),
    )
    .unwrap();
    record.healthcheck_disabled = false;
    record
        .managed_execution
        .as_mut()
        .unwrap()
        .request
        .policy
        .healthcheck_disabled = false;
    manager.reserve(record).await.unwrap();

    let error = manager
        .start(&execution_id, ExecutionGeneration::INITIAL)
        .await
        .unwrap_err();

    assert!(matches!(
        &error,
        ExecutionManagerError::InvalidRequest(message)
            if message.contains("health checks are not supported on Windows")
    ));
    assert_eq!(
        persisted(&manager, &execution_id).managed_state().unwrap(),
        Some(ManagedExecutionState::Created)
    );
    assert_eq!(backend.starts.load(Ordering::Relaxed), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn process_session_inherits_environment_from_persisted_record() {
    let (_directory, manager, _backend) = harness();
    let execution_id = ExecutionId::new("execution-session-environment").unwrap();
    let operation_id = operation("operation-session-environment");
    let mut create_request = request("external-session-environment");
    create_request.config.extra_env = vec![
        ("OFFICIAL_CLIENT".to_string(), "python-sync".to_string()),
        ("OVERRIDE".to_string(), "container".to_string()),
    ];
    let record = build_managed_record(
        &manager.home_dir,
        &execution_id,
        operation_id,
        create_request,
        Utc::now(),
    )
    .unwrap();
    let record = manager.reserve(record).await.unwrap().into_record();
    let record = manager
        .transition(
            &record,
            ManagedExecutionState::Created,
            ManagedExecutionState::Starting,
            RuntimeUpdate::None,
        )
        .await
        .unwrap();

    let socket_path = record.box_dir.join("sockets/exec.sock");
    std::fs::create_dir_all(socket_path.parent().unwrap()).unwrap();
    let process_id = std::process::id();
    let record = manager
        .complete_with_handle(
            &record,
            ManagedExecutionState::Starting,
            ManagedExecutionState::Running,
            LocalExecutionHandle {
                started_at: Utc::now(),
                pid: Some(process_id),
                pid_start_time: crate::process::pid_start_time(process_id),
                exec_socket_path: socket_path.clone(),
                console_log: record.box_dir.join("logs/console.log"),
                anonymous_volumes: Vec::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        persisted(&manager, &execution_id).env,
        HashMap::from([
            ("OFFICIAL_CLIENT".to_string(), "python-sync".to_string()),
            ("OVERRIDE".to_string(), "container".to_string()),
        ])
    );

    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
    let (request_sender, request_receiver) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, write) = tokio::io::split(stream);
        let mut reader = a3s_transport::FrameReader::new(read);
        let mut writer = a3s_transport::FrameWriter::new(write);
        let frame = reader.read_frame().await.unwrap().unwrap();
        assert_eq!(frame.frame_type, a3s_transport::FrameType::Data);
        let request: ExecRequest = serde_json::from_slice(&frame.payload).unwrap();
        request_sender.send(request).unwrap();
        writer
            .write_control(
                &serde_json::to_vec(&a3s_box_core::exec::ExecExit {
                    exit_code: 0,
                    oom_killed: false,
                })
                .unwrap(),
            )
            .await
            .unwrap();
    });

    let mut process = manager
        .start_process(
            &execution_id,
            record.managed_execution.as_ref().unwrap().generation,
            ExecRequest {
                request_id: None,
                cmd: vec!["env".to_string()],
                timeout_ns: 1_000_000_000,
                env: vec!["OVERRIDE=request".to_string()],
                working_dir: None,
                rootfs: None,
                stdin: None,
                stdin_streaming: false,
                user: None,
                streaming: false,
            },
        )
        .await
        .unwrap();
    let forwarded = request_receiver.await.unwrap();
    assert_eq!(
        forwarded.env,
        ["OFFICIAL_CLIENT=python-sync", "OVERRIDE=request"]
    );
    assert!(forwarded.streaming);
    assert!(matches!(
        process.next_event().await.unwrap(),
        Some(ExecEvent::Exit(exit)) if exit.exit_code == 0
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn create_reserves_without_start_and_start_is_generation_fenced() {
    let (_directory, manager, backend) = harness();
    let operation_id = operation("operation-1");
    let create_request = request("sandbox-1");

    let reservation = manager
        .create(create_request.clone(), &operation_id)
        .await
        .unwrap();
    let retry = manager.create(create_request, &operation_id).await.unwrap();
    let status = manager.inspect(&reservation.execution_id).await.unwrap();
    let record = persisted(&manager, &reservation.execution_id);

    assert_eq!(retry.execution_id, reservation.execution_id);
    assert_eq!(status.state, ExecutionState::Created);
    assert_eq!(record.status, "created");
    assert_eq!(
        record.managed_state().unwrap(),
        Some(ManagedExecutionState::Created)
    );
    assert!(!record.is_active());
    assert_eq!(backend.starts.load(Ordering::Relaxed), 0);

    let stale = manager
        .start(
            &reservation.execution_id,
            ExecutionGeneration::new(2).unwrap(),
        )
        .await;
    assert!(matches!(stale, Err(ExecutionManagerError::Conflict { .. })));

    let lease = manager
        .start(&reservation.execution_id, reservation.generation)
        .await
        .unwrap();
    let repeated = manager
        .start(&reservation.execution_id, reservation.generation)
        .await
        .unwrap();

    assert_eq!(lease.execution_id, reservation.execution_id);
    assert_eq!(repeated.execution_id, reservation.execution_id);
    assert_eq!(backend.starts.load(Ordering::Relaxed), 1);
    assert_eq!(
        manager
            .inspect(&reservation.execution_id)
            .await
            .unwrap()
            .state,
        ExecutionState::Running
    );
}

#[cfg(not(windows))]
#[tokio::test]
async fn create_preserves_complete_caller_record_policy() {
    let (_directory, manager, backend) = harness();
    let mut create_request = request("sandbox-1");
    create_request.policy = ExecutionRecordPolicy {
        name: Some("sdk-worker".to_string()),
        auto_remove: true,
        restart_policy: ExecutionRestartPolicy::OnFailure,
        max_restart_count: 4,
        health_check: Some(ExecutionHealthCheck {
            cmd: vec!["test".to_string(), "-f".to_string(), "/ready".to_string()],
            interval_secs: 11,
            timeout_secs: 3,
            retries: 7,
            start_period_secs: 5,
        }),
        healthcheck_disabled: false,
        log_config: a3s_box_core::log::LogConfig {
            driver: a3s_box_core::log::LogDriver::None,
            options: HashMap::from([("tag".to_string(), "worker".to_string())]),
        },
        volume_names: vec!["workspace".to_string()],
        platform: Some("linux/arm64".to_string()),
        init: true,
        devices: vec!["/dev/fuse:/dev/fuse".to_string()],
        gpus: Some("all".to_string()),
        shm_size: Some(64 * 1024 * 1024),
        stop_signal: Some("SIGINT".to_string()),
        stop_timeout: Some(12),
        oom_kill_disable: true,
        oom_score_adj: Some(100),
    };

    let reservation = manager
        .create(create_request.clone(), &operation("operation-policy"))
        .await
        .unwrap();
    let record = persisted(&manager, &reservation.execution_id);

    assert_eq!(backend.starts.load(Ordering::Relaxed), 0);
    assert_eq!(record.name, "sdk-worker");
    assert!(record.auto_remove);
    assert_eq!(record.restart_policy, "on-failure");
    assert_eq!(record.max_restart_count, 4);
    assert_eq!(record.health_check, create_request.policy.health_check);
    assert_eq!(record.log_config, create_request.policy.log_config);
    assert_eq!(record.volume_names, vec!["workspace"]);
    assert_eq!(record.platform.as_deref(), Some("linux/arm64"));
    assert!(record.init);
    assert_eq!(record.devices, vec!["/dev/fuse:/dev/fuse"]);
    assert_eq!(record.gpus.as_deref(), Some("all"));
    assert_eq!(record.shm_size, Some(64 * 1024 * 1024));
    assert_eq!(record.stop_signal.as_deref(), Some("SIGINT"));
    assert_eq!(record.stop_timeout, Some(12));
    assert!(record.oom_kill_disable);
    assert_eq!(record.oom_score_adj, Some(100));
    assert_eq!(
        record.managed_execution.as_ref().unwrap().request.policy,
        create_request.policy
    );
}

#[cfg(not(windows))]
#[tokio::test]
async fn first_start_initializes_health_state_from_persisted_policy() {
    let (_directory, manager, _backend) = harness();
    let mut create_request = request("sandbox-1");
    create_request.policy.health_check = Some(ExecutionHealthCheck {
        cmd: vec!["test".to_string(), "-f".to_string(), "/ready".to_string()],
        interval_secs: 11,
        timeout_secs: 3,
        retries: 7,
        start_period_secs: 5,
    });
    let reservation = manager
        .create(create_request, &operation("operation-health"))
        .await
        .unwrap();

    manager
        .start(&reservation.execution_id, reservation.generation)
        .await
        .unwrap();

    let record = persisted(&manager, &reservation.execution_id);
    assert_eq!(record.health_status, "starting");
    assert_eq!(record.health_retries, 0);
    assert!(record.health_last_check.is_none());
}

#[tokio::test]
async fn ordinary_start_never_revives_a_terminal_managed_execution() {
    let (_directory, manager, backend) = harness();
    let reservation = manager
        .create(request("sandbox-1"), &operation("operation-terminal"))
        .await
        .unwrap();
    let lease = manager
        .start(&reservation.execution_id, reservation.generation)
        .await
        .unwrap();
    manager
        .kill(&lease.execution_id, lease.generation)
        .await
        .unwrap();

    let error = manager
        .start(&lease.execution_id, lease.generation)
        .await
        .unwrap_err();

    assert!(matches!(error, ExecutionManagerError::Conflict { .. }));
    assert_eq!(backend.starts.load(Ordering::Relaxed), 1);
    assert_eq!(
        persisted(&manager, &lease.execution_id)
            .managed_state()
            .unwrap(),
        Some(ManagedExecutionState::Stopped)
    );
}

#[tokio::test]
async fn repeated_create_rejects_caller_policy_drift() {
    let (_directory, manager, backend) = harness();
    let operation_id = operation("operation-policy-drift");
    let create_request = request("sandbox-1");
    manager
        .create(create_request.clone(), &operation_id)
        .await
        .unwrap();
    let mut drifted = create_request;
    drifted.policy.stop_timeout = Some(30);

    let error = manager.create(drifted, &operation_id).await.unwrap_err();

    assert!(matches!(error, ExecutionManagerError::Conflict { .. }));
    assert_eq!(backend.starts.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn reconciliation_reports_created_reservation_without_starting_it() {
    let (_directory, manager, backend) = harness();
    let operation_id = operation("operation-1");
    let reservation = manager
        .create(request("sandbox-1"), &operation_id)
        .await
        .unwrap();

    let outcome = manager.reconcile(&operation_id).await.unwrap();

    let ReconcileOutcome::Created(recovered) = outcome else {
        panic!("expected created reconciliation");
    };
    assert_eq!(recovered.execution_id, reservation.execution_id);
    assert_eq!(recovered.generation, reservation.generation);
    assert_eq!(backend.starts.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn legacy_creating_reservation_remains_recoverable_after_upgrade() {
    let (_directory, manager, backend) = harness();
    let operation_id = operation("operation-1");
    let reservation = manager
        .create(request("sandbox-1"), &operation_id)
        .await
        .unwrap();
    let created = persisted(&manager, &reservation.execution_id);
    let starting = manager
        .transition(
            &created,
            ManagedExecutionState::Created,
            ManagedExecutionState::Starting,
            RuntimeUpdate::None,
        )
        .await
        .unwrap();
    manager
        .transition(
            &starting,
            ManagedExecutionState::Starting,
            ManagedExecutionState::Creating,
            RuntimeUpdate::None,
        )
        .await
        .unwrap();

    let outcome = manager.reconcile(&operation_id).await.unwrap();
    let ReconcileOutcome::Created(recovered) = outcome else {
        panic!("expected legacy creating reservation to reconcile as created");
    };
    assert_eq!(recovered.execution_id, reservation.execution_id);
    assert_eq!(backend.starts.load(Ordering::Relaxed), 0);

    manager
        .start(&recovered.execution_id, recovered.generation)
        .await
        .unwrap();
    assert_eq!(backend.starts.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn repeated_create_is_idempotent_and_request_drift_conflicts() {
    let (_directory, manager, backend) = harness();
    let operation_id = operation("operation-1");
    let create_request = request("sandbox-1");

    let first = manager
        .create_and_start(create_request.clone(), &operation_id)
        .await
        .unwrap();
    let retry = manager
        .create_and_start(create_request, &operation_id)
        .await
        .unwrap();
    let drift = manager
        .create_and_start(request("sandbox-2"), &operation_id)
        .await;

    assert_eq!(retry.execution_id, first.execution_id);
    assert_eq!(backend.starts.load(Ordering::Relaxed), 1);
    assert!(matches!(drift, Err(ExecutionManagerError::Conflict { .. })));
}

#[tokio::test]
async fn concurrent_create_calls_start_one_internal_execution() {
    let (_directory, manager, backend) = harness();
    let operation_id = operation("operation-1");
    let create_request = request("sandbox-1");

    let (left, right) = tokio::join!(
        manager.create_and_start(create_request.clone(), &operation_id),
        manager.create_and_start(create_request.clone(), &operation_id),
    );
    let retry = manager
        .create_and_start(create_request, &operation_id)
        .await
        .unwrap();

    let successes: Vec<_> = [left, right].into_iter().filter_map(Result::ok).collect();
    assert!(!successes.is_empty());
    assert!(successes
        .iter()
        .all(|lease| lease.execution_id == retry.execution_id));
    assert_eq!(backend.starts.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn pause_and_resume_are_generation_fenced_and_persist_policy() {
    let (_directory, manager, backend) = harness();
    let operation_id = operation("operation-1");
    let running = manager
        .create_and_start(request("sandbox-1"), &operation_id)
        .await
        .unwrap();

    let paused = manager
        .pause(&running.execution_id, running.generation, true)
        .await
        .unwrap();
    let stale = manager
        .resume(&running.execution_id, running.generation)
        .await;
    let resumed = manager
        .resume(&paused.execution_id, paused.generation)
        .await
        .unwrap();

    assert_eq!(paused.generation, ExecutionGeneration::new(2).unwrap());
    assert_eq!(resumed.generation, ExecutionGeneration::new(3).unwrap());
    assert!(matches!(stale, Err(ExecutionManagerError::Conflict { .. })));
    assert_eq!(*backend.last_keep_memory.lock().unwrap(), Some(true));
    assert_eq!(backend.pauses.load(Ordering::Relaxed), 1);
    assert_eq!(backend.resumes.load(Ordering::Relaxed), 1);
    let record = persisted(&manager, &running.execution_id);
    assert_eq!(
        record.managed_state().unwrap(),
        Some(ManagedExecutionState::Running)
    );
    assert!(record
        .managed_execution
        .unwrap()
        .pending_operation
        .is_none());
}

#[tokio::test]
async fn failed_pause_rolls_back_without_changing_backend_or_generation() {
    let (_directory, manager, backend) = harness();
    let running = manager
        .create_and_start(request("sandbox-1"), &operation("operation-1"))
        .await
        .unwrap();
    backend.fail_pause.store(true, Ordering::Relaxed);

    let error = manager
        .pause(&running.execution_id, running.generation, true)
        .await
        .unwrap_err();

    assert!(matches!(error, ExecutionManagerError::Unavailable(_)));
    let record = persisted(&manager, &running.execution_id);
    assert_eq!(
        record.managed_state().unwrap(),
        Some(ManagedExecutionState::Running)
    );
    assert_eq!(
        record.managed_execution.unwrap().generation,
        ExecutionGeneration::INITIAL
    );
    assert_eq!(backend.starts.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn filesystem_only_pause_restarts_the_runtime_and_preserves_generation_fencing() {
    let (_directory, manager, backend) = harness();
    let running = manager
        .create_and_start(request("cold-pause"), &operation("cold-pause-create"))
        .await
        .unwrap();

    let paused = manager
        .pause(&running.execution_id, running.generation, false)
        .await
        .unwrap();

    assert_eq!(paused.generation, ExecutionGeneration::new(2).unwrap());
    assert_eq!(backend.kills.load(Ordering::Relaxed), 1);
    assert_eq!(backend.pauses.load(Ordering::Relaxed), 0);
    let record = persisted(&manager, &running.execution_id);
    assert_eq!(
        record.managed_state().unwrap(),
        Some(ManagedExecutionState::Paused)
    );
    let metadata = record.managed_execution.as_ref().unwrap();
    assert!(!metadata.paused_with_memory);
    assert!(metadata.finished_at.is_none());
    assert!(record.pid.is_none());
    assert_eq!(
        manager.inspect(&running.execution_id).await.unwrap().state,
        ExecutionState::Paused
    );

    let stale = manager
        .resume(&running.execution_id, running.generation)
        .await;
    let resumed = manager
        .resume(&paused.execution_id, paused.generation)
        .await
        .unwrap();

    assert!(matches!(stale, Err(ExecutionManagerError::Conflict { .. })));
    assert_eq!(resumed.generation, ExecutionGeneration::new(3).unwrap());
    assert_eq!(backend.starts.load(Ordering::Relaxed), 2);
    assert_eq!(backend.resumes.load(Ordering::Relaxed), 0);
    let record = persisted(&manager, &running.execution_id);
    assert_eq!(
        record.managed_state().unwrap(),
        Some(ManagedExecutionState::Running)
    );
    assert!(record.managed_execution.unwrap().paused_with_memory);
    assert_eq!(record.pid, Some(4242));
}

#[tokio::test]
async fn failed_filesystem_only_pause_rolls_back_to_the_running_generation() {
    let (_directory, manager, backend) = harness();
    let running = manager
        .create_and_start(
            request("cold-pause-failure"),
            &operation("cold-pause-failure-create"),
        )
        .await
        .unwrap();
    backend.fail_kill.store(true, Ordering::Relaxed);

    let error = manager
        .pause(&running.execution_id, running.generation, false)
        .await
        .unwrap_err();

    assert!(matches!(error, ExecutionManagerError::Unavailable(_)));
    let record = persisted(&manager, &running.execution_id);
    assert_eq!(
        record.managed_state().unwrap(),
        Some(ManagedExecutionState::Running)
    );
    assert_eq!(
        record.managed_execution.as_ref().unwrap().generation,
        running.generation
    );
    assert!(record.managed_execution.unwrap().paused_with_memory);
}

#[tokio::test]
async fn failed_filesystem_only_resume_remains_retryable_without_advancing_generation() {
    let (_directory, manager, backend) = harness();
    let running = manager
        .create_and_start(
            request("cold-resume-failure"),
            &operation("cold-resume-failure-create"),
        )
        .await
        .unwrap();
    let paused = manager
        .pause(&running.execution_id, running.generation, false)
        .await
        .unwrap();
    backend.fail_start.store(true, Ordering::Relaxed);

    let error = manager
        .resume(&paused.execution_id, paused.generation)
        .await
        .unwrap_err();

    assert!(matches!(error, ExecutionManagerError::Unavailable(_)));
    let record = persisted(&manager, &running.execution_id);
    assert_eq!(
        record.managed_state().unwrap(),
        Some(ManagedExecutionState::Paused)
    );
    assert_eq!(
        record.managed_execution.as_ref().unwrap().generation,
        paused.generation
    );
    assert!(!record.managed_execution.unwrap().paused_with_memory);
    assert_eq!(
        manager.inspect(&running.execution_id).await.unwrap().state,
        ExecutionState::Paused
    );
}

#[tokio::test]
async fn filesystem_only_pause_and_resume_publish_ambiguous_backend_successes() {
    let (_directory, manager, backend) = harness();
    let running = manager
        .create_and_start(
            request("cold-ambiguous"),
            &operation("cold-ambiguous-create"),
        )
        .await
        .unwrap();
    backend
        .fail_kill_after_effect
        .store(true, Ordering::Relaxed);

    let paused = manager
        .pause(&running.execution_id, running.generation, false)
        .await
        .unwrap();
    backend
        .fail_kill_after_effect
        .store(false, Ordering::Relaxed);
    backend
        .fail_start_after_effect
        .store(true, Ordering::Relaxed);

    let resumed = manager
        .resume(&paused.execution_id, paused.generation)
        .await
        .unwrap();

    assert_eq!(resumed.generation, ExecutionGeneration::new(3).unwrap());
    assert_eq!(backend.kills.load(Ordering::Relaxed), 1);
    assert_eq!(backend.starts.load(Ordering::Relaxed), 2);
    assert!(
        persisted(&manager, &running.execution_id)
            .managed_execution
            .unwrap()
            .paused_with_memory
    );
}

#[tokio::test]
async fn reconcile_finishes_a_crashed_filesystem_only_pause() {
    let (directory, manager, backend) = harness();
    let create_operation = operation("cold-reconcile-create");
    let running = manager
        .create_and_start(request("cold-reconcile"), &create_operation)
        .await
        .unwrap();
    let record = persisted(&manager, &running.execution_id);
    let claimed = manager
        .transition(
            &record,
            ManagedExecutionState::Running,
            ManagedExecutionState::Pausing,
            RuntimeUpdate::PauseClaim(false),
        )
        .await
        .unwrap();
    backend
        .stop_for_restart(&claimed, claimed.stop_timeout)
        .await
        .unwrap();

    let restarted = LocalExecutionManager::new(
        directory.path().join("boxes.json"),
        directory.path().join("home"),
        backend.clone(),
    );
    let outcome = restarted.reconcile(&create_operation).await.unwrap();
    let ReconcileOutcome::Ready(paused) = outcome else {
        panic!("expected cold pause reconciliation to publish a ready lease");
    };

    assert_eq!(paused.generation, ExecutionGeneration::new(2).unwrap());
    assert_eq!(
        restarted
            .inspect(&running.execution_id)
            .await
            .unwrap()
            .state,
        ExecutionState::Paused
    );
    assert!(
        !persisted(&restarted, &running.execution_id)
            .managed_execution
            .unwrap()
            .paused_with_memory
    );
}

#[tokio::test]
async fn reconcile_publishes_a_cold_resume_started_before_record_commit() {
    let (directory, manager, backend) = harness();
    let create_operation = operation("cold-resume-reconcile-create");
    let running = manager
        .create_and_start(request("cold-resume-reconcile"), &create_operation)
        .await
        .unwrap();
    let _paused = manager
        .pause(&running.execution_id, running.generation, false)
        .await
        .unwrap();
    let record = persisted(&manager, &running.execution_id);
    let claimed = manager
        .transition(
            &record,
            ManagedExecutionState::Paused,
            ManagedExecutionState::Resuming,
            RuntimeUpdate::None,
        )
        .await
        .unwrap();
    backend.start(&claimed).await.unwrap();
    assert_eq!(backend.start_attempts.load(Ordering::Relaxed), 2);

    let restarted = LocalExecutionManager::new(
        directory.path().join("boxes.json"),
        directory.path().join("home"),
        backend.clone(),
    );
    let outcome = restarted.reconcile(&create_operation).await.unwrap();
    let ReconcileOutcome::Ready(resumed) = outcome else {
        panic!("expected cold resume reconciliation to publish a ready lease");
    };

    assert_eq!(resumed.generation, ExecutionGeneration::new(3).unwrap());
    assert_eq!(backend.start_attempts.load(Ordering::Relaxed), 2);
    assert!(
        persisted(&restarted, &running.execution_id)
            .managed_execution
            .unwrap()
            .paused_with_memory
    );
}

#[tokio::test]
async fn ambiguous_pause_error_uses_backend_evidence_and_publishes_success() {
    let (_directory, manager, backend) = harness();
    let running = manager
        .create_and_start(request("sandbox-1"), &operation("operation-1"))
        .await
        .unwrap();
    backend
        .fail_pause_after_effect
        .store(true, Ordering::Relaxed);

    let paused = manager
        .pause(&running.execution_id, running.generation, true)
        .await
        .unwrap();

    assert_eq!(paused.generation, ExecutionGeneration::new(2).unwrap());
    assert_eq!(
        persisted(&manager, &running.execution_id)
            .managed_state()
            .unwrap(),
        Some(ManagedExecutionState::Paused)
    );
}

#[tokio::test]
async fn kill_is_generation_fenced_and_idempotent() {
    let (_directory, manager, backend) = harness();
    let running = manager
        .create_and_start(request("sandbox-1"), &operation("operation-1"))
        .await
        .unwrap();

    let killed = manager
        .kill(&running.execution_id, running.generation)
        .await
        .unwrap();
    let repeated = manager
        .kill(&running.execution_id, running.generation)
        .await
        .unwrap();

    assert_eq!(killed, KillOutcome::Killed);
    assert_eq!(repeated, KillOutcome::AlreadyStopped);
    assert_eq!(backend.kills.load(Ordering::Relaxed), 1);
    assert_eq!(
        manager.inspect(&running.execution_id).await.unwrap().state,
        ExecutionState::Stopped
    );
    assert!(persisted(&manager, &running.execution_id).stopped_by_user);
}

#[tokio::test]
async fn option_aware_kill_persists_intent_and_replays_it_after_a_crash() {
    let (directory, manager, backend) = harness();
    let create_operation = operation("operation-option-aware-kill");
    let running = manager
        .create_and_start(request("option-aware-kill"), &create_operation)
        .await
        .unwrap();
    let options = KillExecutionOptions {
        signal: Some(2),
        timeout_secs: Some(4),
    };
    backend.fail_kill.store(true, Ordering::Relaxed);

    let error = manager
        .kill_with_options(&running.execution_id, running.generation, options)
        .await
        .unwrap_err();

    assert!(matches!(error, ExecutionManagerError::Unavailable(_)));
    let claimed = persisted(&manager, &running.execution_id);
    assert_eq!(
        claimed
            .managed_execution
            .as_ref()
            .unwrap()
            .pending_operation,
        Some(ManagedExecutionOperation::Kill {
            signal: Some(2),
            timeout_secs: Some(4),
        })
    );
    assert_eq!(
        claimed.managed_state().unwrap(),
        Some(ManagedExecutionState::Killing)
    );

    backend.fail_kill.store(false, Ordering::Relaxed);
    *backend.last_kill_signal.lock().unwrap() = None;
    *backend.last_kill_timeout.lock().unwrap() = None;
    let restarted = LocalExecutionManager::new(
        directory.path().join("boxes.json"),
        directory.path().join("home"),
        backend.clone(),
    );

    assert!(matches!(
        restarted.reconcile(&create_operation).await.unwrap(),
        ReconcileOutcome::Failed
    ));
    assert_eq!(*backend.last_kill_signal.lock().unwrap(), Some(Some(2)));
    assert_eq!(*backend.last_kill_timeout.lock().unwrap(), Some(Some(4)));
    let stopped = persisted(&restarted, &running.execution_id);
    assert!(stopped.stopped_by_user);
    assert_eq!(
        stopped.managed_state().unwrap(),
        Some(ManagedExecutionState::Stopped)
    );
}

#[tokio::test]
async fn startup_reconciliation_restarts_a_claim_without_backend_evidence() {
    let (_directory, manager, backend) = harness();
    let operation_id = operation("operation-1");
    let execution_id = ExecutionId::new("execution-recovery-1").unwrap();
    reserve_starting(&manager, &execution_id, &operation_id).await;

    let outcome = manager.reconcile(&operation_id).await.unwrap();

    let ReconcileOutcome::Ready(lease) = outcome else {
        panic!("expected ready reconciliation");
    };
    assert_eq!(lease.execution_id, execution_id);
    assert_eq!(backend.starts.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn startup_reconciliation_publishes_an_already_started_backend_once() {
    let (_directory, manager, backend) = harness();
    let operation_id = operation("operation-1");
    let execution_id = ExecutionId::new("execution-recovery-1").unwrap();
    let starting = reserve_starting(&manager, &execution_id, &operation_id).await;
    backend.start(&starting).await.unwrap();

    let outcome = manager.reconcile(&operation_id).await.unwrap();

    let ReconcileOutcome::Ready(lease) = outcome else {
        panic!("expected ready reconciliation");
    };
    assert_eq!(lease.execution_id, execution_id);
    assert_eq!(backend.starts.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn reconciliation_completes_pause_after_backend_side_effect() {
    let (_directory, manager, backend) = harness();
    let operation_id = operation("operation-1");
    let running = manager
        .create_and_start(request("sandbox-1"), &operation_id)
        .await
        .unwrap();
    let record = persisted(&manager, &running.execution_id);
    let pausing = manager
        .transition(
            &record,
            ManagedExecutionState::Running,
            ManagedExecutionState::Pausing,
            RuntimeUpdate::PauseClaim(true),
        )
        .await
        .unwrap();
    backend.pause(&pausing, true).await.unwrap();

    let outcome = manager.reconcile(&operation_id).await.unwrap();

    let ReconcileOutcome::Ready(lease) = outcome else {
        panic!("expected ready reconciliation");
    };
    assert_eq!(lease.generation, ExecutionGeneration::new(2).unwrap());
    assert_eq!(backend.pauses.load(Ordering::Relaxed), 1);
    assert_eq!(
        manager.inspect(&running.execution_id).await.unwrap().state,
        ExecutionState::Paused
    );
}

#[tokio::test]
async fn inspection_persists_an_external_terminal_observation() {
    let (_directory, manager, backend) = harness();
    let running = manager
        .create_and_start(request("sandbox-1"), &operation("operation-1"))
        .await
        .unwrap();
    backend.stop_externally(&running.execution_id, 7);

    let status = manager.inspect(&running.execution_id).await.unwrap();
    let record = persisted(&manager, &running.execution_id);

    assert_eq!(status.state, ExecutionState::Stopped);
    assert_eq!(record.exit_code, Some(7));
    assert_eq!(record.pid, None);
    assert_eq!(
        record.managed_state().unwrap(),
        Some(ManagedExecutionState::Stopped)
    );
}

#[tokio::test]
async fn inspection_releases_resources_after_an_external_terminal_observation() {
    use a3s_box_core::{network::NetworkConfig, volume::VolumeConfig};

    let (_directory, manager, backend) = harness();
    let volumes = crate::VolumeStore::new(
        manager.home_dir.join("volumes.json"),
        manager.home_dir.join("volumes"),
    );
    volumes.create(VolumeConfig::new("workspace", "")).unwrap();
    let networks = crate::NetworkStore::new(manager.home_dir.join("networks.json"));
    networks
        .create(NetworkConfig::new("dev", "10.88.0.0/24").unwrap())
        .unwrap();

    let mut create_request = request("sandbox-1");
    create_request.config.isolation = ExecutionIsolation::Microvm;
    create_request.config.network = NetworkMode::Bridge {
        network: "dev".to_string(),
    };
    create_request.policy.name = Some("terminal-resources".to_string());
    create_request.policy.volume_names = vec!["workspace".to_string()];
    let running = manager
        .create_and_start(create_request, &operation("operation-terminal-resources"))
        .await
        .unwrap();
    volumes
        .modify("workspace", |volume| {
            volume.attach(running.execution_id.as_str())
        })
        .unwrap();
    networks
        .with_write_lock(|entries| -> Result<(), a3s_box_core::BoxError> {
            entries
                .get_mut("dev")
                .unwrap()
                .connect(running.execution_id.as_str(), "terminal-resources")
                .map_err(a3s_box_core::BoxError::NetworkError)?;
            Ok(())
        })
        .unwrap();

    backend.stop_externally(&running.execution_id, 0);
    manager.inspect(&running.execution_id).await.unwrap();

    assert!(volumes
        .get("workspace")
        .unwrap()
        .unwrap()
        .in_use_by
        .is_empty());
    assert!(!networks
        .get("dev")
        .unwrap()
        .unwrap()
        .endpoints
        .contains_key(running.execution_id.as_str()));
}

#[tokio::test]
async fn restart_running_execution_advances_generation_once_and_is_idempotent() {
    let (_directory, manager, backend) = harness();
    let running = manager
        .create_and_start(request("sandbox-1"), &operation("operation-create"))
        .await
        .unwrap();
    let restart_operation = operation("operation-restart");

    let restarted = manager
        .restart(
            &running.execution_id,
            running.generation,
            &restart_operation,
        )
        .await
        .unwrap();
    let retry = manager
        .restart(
            &running.execution_id,
            running.generation,
            &restart_operation,
        )
        .await
        .unwrap();

    assert_eq!(restarted.generation, ExecutionGeneration::new(2).unwrap());
    assert_eq!(retry.generation, restarted.generation);
    assert_eq!(backend.kills.load(Ordering::Relaxed), 1);
    assert_eq!(backend.starts.load(Ordering::Relaxed), 2);
    let record = persisted(&manager, &running.execution_id);
    assert_eq!(
        record.managed_state().unwrap(),
        Some(ManagedExecutionState::Running)
    );
    let completed = record
        .managed_execution
        .unwrap()
        .last_restart
        .expect("completed restart must remain durable for idempotent retries");
    assert_eq!(completed.operation_id, restart_operation);
    assert_eq!(completed.source_generation, running.generation);
    assert_eq!(completed.target_generation, restarted.generation);
}

#[tokio::test]
async fn stale_restart_generation_has_no_backend_side_effects() {
    let (_directory, manager, backend) = harness();
    let running = manager
        .create_and_start(request("sandbox-1"), &operation("operation-create"))
        .await
        .unwrap();

    let error = manager
        .restart(
            &running.execution_id,
            ExecutionGeneration::new(2).unwrap(),
            &operation("operation-stale-restart"),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, ExecutionManagerError::Conflict { .. }));
    assert_eq!(backend.kills.load(Ordering::Relaxed), 0);
    assert_eq!(backend.starts.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn restart_persists_stop_options_and_rejects_retry_drift() {
    let (_directory, manager, backend) = harness();
    let running = manager
        .create_and_start(request("sandbox-1"), &operation("operation-create"))
        .await
        .unwrap();
    let restart_operation = operation("operation-restart");
    let options = RestartExecutionOptions {
        stop_timeout_secs: Some(7),
    };

    manager
        .restart_with_options(
            &running.execution_id,
            running.generation,
            &restart_operation,
            options,
        )
        .await
        .unwrap();
    let starts_after_restart = backend.starts.load(Ordering::Relaxed);
    let drift = manager
        .restart_with_options(
            &running.execution_id,
            running.generation,
            &restart_operation,
            RestartExecutionOptions {
                stop_timeout_secs: Some(8),
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(drift, ExecutionManagerError::Conflict { .. }));
    assert_eq!(backend.starts.load(Ordering::Relaxed), starts_after_restart);
    assert_eq!(*backend.last_restart_timeout.lock().unwrap(), Some(Some(7)));
    assert_eq!(
        persisted(&manager, &running.execution_id)
            .managed_execution
            .unwrap()
            .last_restart
            .unwrap()
            .stop_timeout_secs,
        Some(7)
    );
}

#[tokio::test]
async fn restart_supports_paused_stopped_and_failed_executions() {
    let (_directory, manager, backend) = harness();
    let running = manager
        .create_and_start(request("paused"), &operation("create-paused"))
        .await
        .unwrap();
    let paused = manager
        .pause(&running.execution_id, running.generation, true)
        .await
        .unwrap();
    let restarted_paused = manager
        .restart(
            &paused.execution_id,
            paused.generation,
            &operation("restart-paused"),
        )
        .await
        .unwrap();
    assert_eq!(
        restarted_paused.generation,
        ExecutionGeneration::new(3).unwrap()
    );

    let stopped = manager
        .create_and_start(request("stopped"), &operation("create-stopped"))
        .await
        .unwrap();
    manager
        .kill(&stopped.execution_id, stopped.generation)
        .await
        .unwrap();
    let kills_before_stopped_restart = backend.kills.load(Ordering::Relaxed);
    let restarted_stopped = manager
        .restart(
            &stopped.execution_id,
            stopped.generation,
            &operation("restart-stopped"),
        )
        .await
        .unwrap();
    assert_eq!(
        restarted_stopped.generation,
        ExecutionGeneration::new(2).unwrap()
    );
    assert_eq!(
        backend.kills.load(Ordering::Relaxed),
        kills_before_stopped_restart
    );

    let failed = manager
        .create_and_start(request("failed"), &operation("create-failed"))
        .await
        .unwrap();
    backend.fail_externally(&failed.execution_id, 17);
    assert_eq!(
        manager.inspect(&failed.execution_id).await.unwrap().state,
        ExecutionState::Failed
    );
    let kills_before_failed_restart = backend.kills.load(Ordering::Relaxed);
    let restarted_failed = manager
        .restart(
            &failed.execution_id,
            failed.generation,
            &operation("restart-failed"),
        )
        .await
        .unwrap();
    assert_eq!(
        restarted_failed.generation,
        ExecutionGeneration::new(2).unwrap()
    );
    assert_eq!(
        backend.kills.load(Ordering::Relaxed),
        kills_before_failed_restart
    );
}

#[tokio::test]
async fn restart_of_an_unstarted_reservation_starts_generation_two_without_kill() {
    let (_directory, manager, backend) = harness();
    let created = manager
        .create(request("created"), &operation("create-created"))
        .await
        .unwrap();

    let restarted = manager
        .restart(
            &created.execution_id,
            created.generation,
            &operation("restart-created"),
        )
        .await
        .unwrap();

    assert_eq!(restarted.generation, ExecutionGeneration::new(2).unwrap());
    assert_eq!(backend.kills.load(Ordering::Relaxed), 0);
    assert_eq!(backend.starts.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn restart_recovers_when_kill_succeeded_but_its_response_was_lost() {
    let (directory, manager, backend) = harness();
    let running = manager
        .create_and_start(request("sandbox-1"), &operation("operation-create"))
        .await
        .unwrap();
    let restart_operation = operation("operation-restart");
    let record = persisted(&manager, &running.execution_id);
    let claimed = manager
        .transition(
            &record,
            ManagedExecutionState::Running,
            ManagedExecutionState::RestartStopping,
            RuntimeUpdate::RestartClaim {
                operation_id: restart_operation.clone(),
                options: Default::default(),
            },
        )
        .await
        .unwrap();
    backend
        .fail_kill_after_effect
        .store(true, Ordering::Relaxed);
    assert!(backend.kill(&claimed).await.is_err());
    backend
        .fail_kill_after_effect
        .store(false, Ordering::Relaxed);

    let restarted_manager = LocalExecutionManager::new(
        directory.path().join("boxes.json"),
        directory.path().join("home"),
        backend.clone(),
    );
    let restarted = restarted_manager
        .restart(
            &running.execution_id,
            running.generation,
            &restart_operation,
        )
        .await
        .unwrap();

    assert_eq!(restarted.generation, ExecutionGeneration::new(2).unwrap());
    assert_eq!(backend.starts.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn restart_recovers_after_generation_advance_before_backend_start() {
    let (directory, manager, backend) = harness();
    let create_operation = operation("operation-create");
    let running = manager
        .create_and_start(request("sandbox-1"), &create_operation)
        .await
        .unwrap();
    let restart_operation = operation("operation-restart");
    let record = persisted(&manager, &running.execution_id);
    let claimed = manager
        .transition(
            &record,
            ManagedExecutionState::Running,
            ManagedExecutionState::RestartStopping,
            RuntimeUpdate::RestartClaim {
                operation_id: restart_operation.clone(),
                options: Default::default(),
            },
        )
        .await
        .unwrap();
    backend.kill(&claimed).await.unwrap();
    let restarting = manager
        .transition(
            &claimed,
            ManagedExecutionState::RestartStopping,
            ManagedExecutionState::RestartStarting,
            RuntimeUpdate::RestartAdvance,
        )
        .await
        .unwrap();
    assert_eq!(
        restarting.managed_execution.as_ref().unwrap().generation,
        ExecutionGeneration::new(2).unwrap()
    );

    let restarted_manager = LocalExecutionManager::new(
        directory.path().join("boxes.json"),
        directory.path().join("home"),
        backend.clone(),
    );
    let ReconcileOutcome::Ready(restarted) = restarted_manager
        .reconcile(&create_operation)
        .await
        .unwrap()
    else {
        panic!("expected restart reconciliation to return a ready lease");
    };

    assert_eq!(restarted.generation, ExecutionGeneration::new(2).unwrap());
    assert_eq!(backend.starts.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn concurrent_retries_of_one_restart_start_the_new_generation_once() {
    let (_directory, manager, backend) = harness();
    let running = manager
        .create_and_start(request("sandbox-1"), &operation("operation-create"))
        .await
        .unwrap();
    let restart_operation = operation("operation-restart");

    let (left, right) = tokio::join!(
        manager.restart(
            &running.execution_id,
            running.generation,
            &restart_operation,
        ),
        manager.restart(
            &running.execution_id,
            running.generation,
            &restart_operation,
        ),
    );

    let successes = [left, right]
        .into_iter()
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    assert_eq!(successes.len(), 2);
    assert!(successes
        .iter()
        .all(|lease| lease.generation == ExecutionGeneration::new(2).unwrap()));
    assert_eq!(backend.starts.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn a_different_operation_cannot_take_over_an_in_progress_restart() {
    let (_directory, manager, backend) = harness();
    let running = manager
        .create_and_start(request("sandbox-1"), &operation("operation-create"))
        .await
        .unwrap();
    let record = persisted(&manager, &running.execution_id);
    manager
        .transition(
            &record,
            ManagedExecutionState::Running,
            ManagedExecutionState::RestartStopping,
            RuntimeUpdate::RestartClaim {
                operation_id: operation("restart-owner"),
                options: Default::default(),
            },
        )
        .await
        .unwrap();

    let error = manager
        .restart(
            &running.execution_id,
            running.generation,
            &operation("restart-contender"),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, ExecutionManagerError::Conflict { .. }));
    assert_eq!(backend.kills.load(Ordering::Relaxed), 0);
    assert_eq!(backend.starts.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn ambiguous_restart_start_error_uses_backend_evidence() {
    let (_directory, manager, backend) = harness();
    let running = manager
        .create_and_start(request("sandbox-1"), &operation("operation-create"))
        .await
        .unwrap();
    backend
        .fail_start_after_effect
        .store(true, Ordering::Relaxed);

    let restarted = manager
        .restart(
            &running.execution_id,
            running.generation,
            &operation("operation-restart"),
        )
        .await
        .unwrap();

    assert_eq!(restarted.generation, ExecutionGeneration::new(2).unwrap());
    assert_eq!(backend.starts.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn failed_restart_start_is_terminal_at_the_new_generation() {
    let (_directory, manager, backend) = harness();
    let running = manager
        .create_and_start(request("sandbox-1"), &operation("operation-create"))
        .await
        .unwrap();
    manager
        .kill(&running.execution_id, running.generation)
        .await
        .unwrap();
    backend.fail_start.store(true, Ordering::Relaxed);
    let restart_operation = operation("operation-restart");

    let error = manager
        .restart(
            &running.execution_id,
            running.generation,
            &restart_operation,
        )
        .await
        .unwrap_err();
    let attempts_after_failure = backend.starts.load(Ordering::Relaxed);
    let retry = manager
        .restart(
            &running.execution_id,
            running.generation,
            &restart_operation,
        )
        .await
        .unwrap_err();

    assert!(matches!(error, ExecutionManagerError::Unavailable(_)));
    assert!(matches!(retry, ExecutionManagerError::Conflict { .. }));
    assert_eq!(
        backend.starts.load(Ordering::Relaxed),
        attempts_after_failure
    );
    let record = persisted(&manager, &running.execution_id);
    assert_eq!(
        record.managed_state().unwrap(),
        Some(ManagedExecutionState::Failed)
    );
    assert_eq!(
        record.managed_execution.as_ref().unwrap().generation,
        ExecutionGeneration::new(2).unwrap()
    );
    assert_eq!(
        record
            .managed_execution
            .unwrap()
            .last_restart
            .unwrap()
            .outcome,
        crate::ManagedRestartOutcome::Failed
    );
}

fn populate_rootfs(manager: &LocalExecutionManager, execution_id: &ExecutionId, value: &str) {
    let rootfs = persisted(manager, execution_id).box_dir.join("rootfs");
    std::fs::create_dir_all(rootfs.join("workspace")).unwrap();
    std::fs::write(rootfs.join("workspace/state.txt"), value).unwrap();
}

#[tokio::test]
async fn filesystem_snapshot_quiesces_and_restores_without_changing_generation() {
    let (directory, manager, backend) = harness();
    let running = manager
        .create_and_start(request("snapshot-source"), &operation("snapshot-create"))
        .await
        .unwrap();
    populate_rootfs(&manager, &running.execution_id, "captured-state");
    let snapshot_id = ExecutionSnapshotId::new("managed-snapshot-1").unwrap();

    let snapshot = manager
        .create_filesystem_snapshot(&running.execution_id, running.generation, &snapshot_id)
        .await
        .unwrap();

    assert_eq!(snapshot.lease.generation, running.generation);
    assert_eq!(snapshot.state, ExecutionState::Running);
    assert!(snapshot.size_bytes > 0);
    assert_eq!(backend.pauses.load(Ordering::Relaxed), 1);
    assert_eq!(backend.resumes.load(Ordering::Relaxed), 1);
    assert_eq!(*backend.last_keep_memory.lock().unwrap(), Some(true));
    assert_eq!(
        std::fs::read_to_string(
            directory
                .path()
                .join("home/snapshots/managed-snapshot-1/rootfs/workspace/state.txt")
        )
        .unwrap(),
        "captured-state"
    );
    assert_eq!(
        persisted(&manager, &running.execution_id)
            .managed_execution
            .unwrap()
            .generation,
        running.generation
    );

    let retry = manager
        .create_filesystem_snapshot(&running.execution_id, running.generation, &snapshot_id)
        .await
        .unwrap();
    assert_eq!(retry.size_bytes, snapshot.size_bytes);
    assert_eq!(backend.pauses.load(Ordering::Relaxed), 1);
    assert_eq!(backend.resumes.load(Ordering::Relaxed), 1);
    assert!(manager
        .delete_filesystem_snapshot(&snapshot_id)
        .await
        .unwrap());
    assert_eq!(
        manager
            .filesystem_snapshot_size(&snapshot_id)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn filesystem_snapshot_after_manager_restart_keeps_resolved_image_config() {
    let (directory, manager, backend) = harness();
    let running = manager
        .create_and_start(
            request("snapshot-image-config"),
            &operation("snapshot-image-config-create"),
        )
        .await
        .unwrap();
    populate_rootfs(&manager, &running.execution_id, "captured-state");

    let restarted = LocalExecutionManager::new(
        directory.path().join("boxes.json"),
        directory.path().join("home"),
        backend,
    );
    let snapshot_id = ExecutionSnapshotId::new("snapshot-image-config").unwrap();
    restarted
        .create_filesystem_snapshot(&running.execution_id, running.generation, &snapshot_id)
        .await
        .unwrap();

    let metadata = crate::SnapshotStore::new(&directory.path().join("home/snapshots"))
        .unwrap()
        .get(snapshot_id.as_str())
        .unwrap()
        .unwrap();
    let image_config = metadata
        .image_config
        .expect("resolved image configuration must survive a control-plane restart");
    assert_eq!(
        image_config.entrypoint,
        Some(vec!["/usr/local/bin/envd".to_string()])
    );
    assert_eq!(
        image_config.cmd,
        Some(vec!["--port".to_string(), "49983".to_string()])
    );
    assert_eq!(image_config.working_dir.as_deref(), Some("/home/user"));
    assert_eq!(image_config.user.as_deref(), Some("1000:1000"));
}

#[tokio::test]
async fn paused_snapshot_remains_paused_and_does_not_resume() {
    let (_directory, manager, backend) = harness();
    let running = manager
        .create_and_start(request("paused-source"), &operation("paused-create"))
        .await
        .unwrap();
    let paused = manager
        .pause(&running.execution_id, running.generation, true)
        .await
        .unwrap();
    populate_rootfs(&manager, &running.execution_id, "paused-state");
    let pauses_before = backend.pauses.load(Ordering::Relaxed);
    let resumes_before = backend.resumes.load(Ordering::Relaxed);

    let snapshot = manager
        .create_filesystem_snapshot(
            &running.execution_id,
            paused.generation,
            &ExecutionSnapshotId::new("paused-snapshot").unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(snapshot.state, ExecutionState::Paused);
    assert_eq!(snapshot.lease.generation, paused.generation);
    assert_eq!(backend.pauses.load(Ordering::Relaxed), pauses_before);
    assert_eq!(backend.resumes.load(Ordering::Relaxed), resumes_before);
}

#[tokio::test]
async fn filesystem_only_paused_snapshot_uses_the_quiescent_rootfs_without_a_runtime() {
    let (directory, manager, backend) = harness();
    let running = manager
        .create_and_start(
            request("cold-paused-source"),
            &operation("cold-paused-create"),
        )
        .await
        .unwrap();
    populate_rootfs(&manager, &running.execution_id, "cold-paused-state");
    #[cfg(target_os = "linux")]
    super::snapshot::persist_sandbox_snapshot_mappings(&persisted(&manager, &running.execution_id))
        .unwrap();
    let paused = manager
        .pause(&running.execution_id, running.generation, false)
        .await
        .unwrap();
    #[cfg(target_os = "linux")]
    std::fs::remove_dir_all(
        persisted(&manager, &running.execution_id)
            .box_dir
            .join("sandbox/bundle"),
    )
    .unwrap();
    let pauses_before = backend.pauses.load(Ordering::Relaxed);
    let resumes_before = backend.resumes.load(Ordering::Relaxed);
    let starts_before = backend.starts.load(Ordering::Relaxed);
    let snapshot_id = ExecutionSnapshotId::new("cold-paused-snapshot").unwrap();

    let snapshot = manager
        .create_filesystem_snapshot(&running.execution_id, paused.generation, &snapshot_id)
        .await
        .unwrap();

    assert_eq!(snapshot.state, ExecutionState::Paused);
    assert_eq!(snapshot.lease.generation, paused.generation);
    assert_eq!(backend.pauses.load(Ordering::Relaxed), pauses_before);
    assert_eq!(backend.resumes.load(Ordering::Relaxed), resumes_before);
    assert_eq!(backend.starts.load(Ordering::Relaxed), starts_before);
    assert_eq!(backend.quiescent_rootfs_prepares.load(Ordering::Relaxed), 1);
    assert_eq!(backend.quiescent_rootfs_cleanups.load(Ordering::Relaxed), 1);
    assert_eq!(
        std::fs::read_to_string(
            directory
                .path()
                .join("home/snapshots/cold-paused-snapshot/rootfs/workspace/state.txt")
        )
        .unwrap(),
        "cold-paused-state"
    );
    let record = persisted(&manager, &running.execution_id);
    assert_eq!(
        record.managed_state().unwrap(),
        Some(ManagedExecutionState::Paused)
    );
    assert!(!record.managed_execution.unwrap().paused_with_memory);
}

#[tokio::test]
async fn cold_paused_snapshot_cleanup_failure_remains_recoverable() {
    let (_directory, manager, backend) = harness();
    let running = manager
        .create_and_start(
            request("cold-snapshot-cleanup"),
            &operation("cold-snapshot-cleanup-create"),
        )
        .await
        .unwrap();
    populate_rootfs(&manager, &running.execution_id, "recoverable-cold-snapshot");
    let paused = manager
        .pause(&running.execution_id, running.generation, false)
        .await
        .unwrap();
    let snapshot_id = ExecutionSnapshotId::new("cold-snapshot-cleanup-retry").unwrap();
    backend
        .fail_quiescent_rootfs_cleanup
        .store(true, Ordering::Relaxed);

    let error = manager
        .create_filesystem_snapshot(&running.execution_id, paused.generation, &snapshot_id)
        .await
        .unwrap_err();

    assert!(error
        .to_string()
        .contains("quiescent rootfs cleanup is unavailable"));
    assert_eq!(
        persisted(&manager, &running.execution_id)
            .managed_state()
            .unwrap(),
        Some(ManagedExecutionState::Snapshotting)
    );

    backend
        .fail_quiescent_rootfs_cleanup
        .store(false, Ordering::Relaxed);
    let status = manager.inspect(&running.execution_id).await.unwrap();

    assert_eq!(status.state, ExecutionState::Paused);
    assert_eq!(status.generation, paused.generation);
    assert_eq!(backend.quiescent_rootfs_prepares.load(Ordering::Relaxed), 2);
    assert_eq!(backend.quiescent_rootfs_cleanups.load(Ordering::Relaxed), 2);
    assert!(manager
        .filesystem_snapshot_size(&snapshot_id)
        .await
        .unwrap()
        .is_some());
    let record = persisted(&manager, &running.execution_id);
    assert_eq!(
        record.managed_state().unwrap(),
        Some(ManagedExecutionState::Paused)
    );
    assert!(!record.managed_execution.unwrap().paused_with_memory);
}

#[tokio::test]
async fn snapshot_failure_restores_running_state_at_the_same_generation() {
    let (_directory, manager, backend) = harness();
    let running = manager
        .create_and_start(
            request("missing-rootfs"),
            &operation("missing-rootfs-create"),
        )
        .await
        .unwrap();
    let snapshot_id = ExecutionSnapshotId::new("missing-rootfs-snapshot").unwrap();

    let error = manager
        .create_filesystem_snapshot(&running.execution_id, running.generation, &snapshot_id)
        .await
        .unwrap_err();

    assert!(matches!(error, ExecutionManagerError::Unavailable(_)));
    assert_eq!(backend.pauses.load(Ordering::Relaxed), 1);
    assert_eq!(backend.resumes.load(Ordering::Relaxed), 1);
    let record = persisted(&manager, &running.execution_id);
    assert_eq!(
        record.managed_state().unwrap(),
        Some(ManagedExecutionState::Running)
    );
    assert_eq!(
        record.managed_execution.unwrap().generation,
        running.generation
    );
    assert_eq!(
        manager
            .filesystem_snapshot_size(&snapshot_id)
            .await
            .unwrap(),
        None
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn special_file_snapshot_failure_resumes_running_source() {
    use std::os::unix::ffi::OsStrExt;

    let (directory, manager, backend) = harness();
    let running = manager
        .create_and_start(
            request("special-file-source"),
            &operation("special-file-create"),
        )
        .await
        .unwrap();
    populate_rootfs(&manager, &running.execution_id, "still-running");
    let rootfs = persisted(&manager, &running.execution_id)
        .box_dir
        .join("rootfs");
    let fifo = rootfs.join("workspace/blocking-fifo");
    let fifo_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
    let snapshot_id = ExecutionSnapshotId::new("special-file-snapshot").unwrap();

    let error = manager
        .create_filesystem_snapshot(&running.execution_id, running.generation, &snapshot_id)
        .await
        .unwrap_err();

    assert!(matches!(
        &error,
        ExecutionManagerError::Unavailable(message)
            if message.contains("unsupported special file") && message.contains("fifo")
    ));
    assert_eq!(backend.pauses.load(Ordering::Relaxed), 1);
    assert_eq!(backend.resumes.load(Ordering::Relaxed), 1);
    assert_eq!(
        manager.inspect(&running.execution_id).await.unwrap().state,
        ExecutionState::Running
    );
    assert_eq!(
        manager
            .filesystem_snapshot_size(&snapshot_id)
            .await
            .unwrap(),
        None
    );
    assert!(std::fs::read_dir(directory.path().join("home/snapshots"))
        .unwrap()
        .flatten()
        .all(|entry| !entry.file_name().to_string_lossy().starts_with(".staging-")));
}

#[tokio::test]
async fn reconcile_recovers_a_crash_after_snapshot_pause() {
    let (directory, manager, backend) = harness();
    let create_operation = operation("recovered-snapshot-create");
    let running = manager
        .create_and_start(request("recovered-source"), &create_operation)
        .await
        .unwrap();
    populate_rootfs(&manager, &running.execution_id, "recovered-state");
    let snapshot_id = ExecutionSnapshotId::new("recovered-snapshot").unwrap();
    let record = persisted(&manager, &running.execution_id);
    let claimed = manager
        .transition(
            &record,
            ManagedExecutionState::Running,
            ManagedExecutionState::Snapshotting,
            RuntimeUpdate::SnapshotClaim {
                snapshot_id: snapshot_id.clone(),
                source_state: ManagedExecutionState::Running,
            },
        )
        .await
        .unwrap();
    backend.pause(&claimed, true).await.unwrap();

    let restarted = LocalExecutionManager::new(
        directory.path().join("boxes.json"),
        directory.path().join("home"),
        backend.clone(),
    );
    let ReconcileOutcome::Ready(lease) = restarted.reconcile(&create_operation).await.unwrap()
    else {
        panic!("expected snapshot reconciliation to return a ready lease");
    };

    assert_eq!(lease.generation, running.generation);
    assert_eq!(backend.resumes.load(Ordering::Relaxed), 1);
    assert_eq!(
        persisted(&restarted, &running.execution_id)
            .managed_state()
            .unwrap(),
        Some(ManagedExecutionState::Running)
    );
    assert!(restarted
        .filesystem_snapshot_size(&snapshot_id)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn legacy_snapshot_without_image_config_is_rejected_before_reservation() {
    let (directory, manager, _backend) = harness();
    let source = directory.path().join("legacy-snapshot-source");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("state.txt"), "captured").unwrap();
    let snapshot_id = ExecutionSnapshotId::new("legacy-snapshot").unwrap();
    crate::SnapshotStore::new(&directory.path().join("home/snapshots"))
        .unwrap()
        .save(
            a3s_box_core::SnapshotMetadata::new(
                snapshot_id.to_string(),
                snapshot_id.to_string(),
                "source-execution".to_string(),
                "alpine:3.20".to_string(),
            ),
            &source,
        )
        .unwrap();
    let mut restore = request("legacy-snapshot-restore");
    restore.rootfs_snapshot_id = Some(snapshot_id);
    let operation_id = operation("legacy-snapshot-restore-create");

    let error = manager.create(restore, &operation_id).await.unwrap_err();

    assert!(matches!(
        &error,
        ExecutionManagerError::Unavailable(message)
            if message.contains("resolved OCI image configuration")
    ));
    assert!(manager
        .get_by_operation(&operation_id)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn snapshot_delete_refuses_an_unstarted_restored_execution() {
    let (_directory, manager, _backend) = harness();
    let running = manager
        .create_and_start(request("delete-source"), &operation("delete-source-create"))
        .await
        .unwrap();
    populate_rootfs(&manager, &running.execution_id, "delete-state");
    let snapshot_id = ExecutionSnapshotId::new("delete-protected-snapshot").unwrap();
    manager
        .create_filesystem_snapshot(&running.execution_id, running.generation, &snapshot_id)
        .await
        .unwrap();
    let mut restored_request = request("restored-reservation");
    restored_request.rootfs_snapshot_id = Some(snapshot_id.clone());
    let restored = manager
        .create(restored_request, &operation("restored-reservation-create"))
        .await
        .unwrap();

    assert!(matches!(
        manager.delete_filesystem_snapshot(&snapshot_id).await,
        Err(ExecutionManagerError::Conflict { .. })
    ));
    let record = persisted(&manager, &restored.execution_id);
    manager
        .transition(
            &record,
            ManagedExecutionState::Created,
            ManagedExecutionState::Stopped,
            RuntimeUpdate::Terminal(None),
        )
        .await
        .unwrap();
    assert!(manager
        .delete_filesystem_snapshot(&snapshot_id)
        .await
        .unwrap());
}

#[tokio::test]
async fn snapshot_delete_and_restored_reservation_are_atomic() {
    let (_directory, manager, _backend) = harness();
    let running = manager
        .create_and_start(
            request("atomic-delete-source"),
            &operation("atomic-delete-source-create"),
        )
        .await
        .unwrap();
    populate_rootfs(&manager, &running.execution_id, "atomic-delete-state");

    for index in 0..16 {
        let snapshot_id =
            ExecutionSnapshotId::new(format!("atomic-delete-snapshot-{index}")).unwrap();
        manager
            .create_filesystem_snapshot(&running.execution_id, running.generation, &snapshot_id)
            .await
            .unwrap();
        let mut restored_request = request(&format!("atomic-restored-{index}"));
        restored_request.rootfs_snapshot_id = Some(snapshot_id.clone());
        let create_operation = operation(&format!("atomic-restored-create-{index}"));
        let create_manager = manager.clone();
        let delete_manager = manager.clone();
        let delete_snapshot_id = snapshot_id.clone();

        let (created, deleted) = tokio::join!(
            create_manager.create(restored_request, &create_operation),
            delete_manager.delete_filesystem_snapshot(&delete_snapshot_id),
        );

        match (created, deleted) {
            (Ok(restored), Err(ExecutionManagerError::Conflict { .. })) => {
                let record = persisted(&manager, &restored.execution_id);
                manager
                    .transition(
                        &record,
                        ManagedExecutionState::Created,
                        ManagedExecutionState::Stopped,
                        RuntimeUpdate::Terminal(None),
                    )
                    .await
                    .unwrap();
                assert!(manager
                    .delete_filesystem_snapshot(&snapshot_id)
                    .await
                    .unwrap());
            }
            (Err(ExecutionManagerError::Unavailable(_)), Ok(true)) => {
                assert!(matches!(
                    manager.reconcile(&create_operation).await.unwrap(),
                    ReconcileOutcome::Absent
                ));
            }
            (created, deleted) => {
                panic!(
                    "restored reservation and Snapshot deletion were not atomic: \
                     create={created:?}, delete={deleted:?}"
                );
            }
        }
    }
}
