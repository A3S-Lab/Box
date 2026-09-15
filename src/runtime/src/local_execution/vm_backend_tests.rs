use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use a3s_box_core::{
    BoxConfig, CreateExecutionRequest, ExecutionGeneration, ExecutionIsolation, NetworkMode,
    OperationId, VmHandler, VmMetrics,
};

use super::*;
use crate::local_execution::record::build_managed_record;

fn record(home_dir: &Path, isolation: ExecutionIsolation) -> BoxRecord {
    let id = ExecutionId::new("11111111-1111-4111-8111-111111111111").unwrap();
    let mut config = BoxConfig {
        isolation,
        image: "alpine:latest".to_string(),
        dns: vec!["1.1.1.1".to_string()],
        ..Default::default()
    };
    if isolation == ExecutionIsolation::Microvm {
        config.sysctls = vec![("net.ipv4.ip_forward".to_string(), "1".to_string())];
    }
    config.resources.memory_mb = 256;
    build_managed_record(
        home_dir,
        &id,
        OperationId::new("operation-1").unwrap(),
        CreateExecutionRequest {
            external_sandbox_id: "external-untrusted-label".to_string(),
            config,
            labels: BTreeMap::new(),
            policy: Default::default(),
            rootfs_snapshot_id: None,
        },
        chrono::Utc::now(),
    )
    .unwrap()
}

#[tokio::test]
async fn sandbox_resource_planning_persists_image_volume_ownership_without_runtime_side_effects() {
    let temporary = tempfile::tempdir().unwrap();
    let backend = VmLocalExecutionBackend::new(temporary.path());
    let record = record(temporary.path(), ExecutionIsolation::Sandbox);
    std::fs::create_dir_all(&record.box_dir).unwrap();
    crate::resolved_image::persist_resolved_image_config(
        &record.box_dir,
        &crate::oci::OciImageConfig {
            entrypoint: None,
            cmd: None,
            env: Vec::new(),
            working_dir: None,
            user: None,
            exposed_ports: Vec::new(),
            labels: std::collections::HashMap::new(),
            volumes: vec!["/data".to_string(), "/data/./".to_string()],
            stop_signal: None,
            health_check: None,
            onbuild: Vec::new(),
        },
    )
    .unwrap();

    let plan = backend.plan_create_resources(&record).await.unwrap();

    assert_eq!(plan.anonymous_volumes.len(), 1);
    assert!(plan.anonymous_volumes[0].starts_with("anon_11111111_"));
    assert!(!temporary.path().join("images").exists());
    assert!(!temporary.path().join("volumes.json").exists());
    assert!(!temporary.path().join("volumes").exists());
    assert!(!record.box_dir.join("rootfs").exists());
    assert!(!record.box_dir.join("workspace").exists());
    assert!(!record.box_dir.join("sockets").exists());
}

struct DelayedExitStatusHandler {
    exit_polls: Arc<AtomicUsize>,
    stop_calls: Arc<AtomicUsize>,
    available_after: usize,
    reports_running: bool,
    /// When set, publish a legacy guest exit marker only once the delayed
    /// provider exit becomes available — so cleanup waits for authenticated
    /// guest status instead of inventing success from provider zero.
    durable_exit_path: Option<std::path::PathBuf>,
}

impl DelayedExitStatusHandler {
    fn publish_durable_exit_if_ready(&self) {
        let Some(path) = self.durable_exit_path.as_ref() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, "0\n");
    }
}

impl VmHandler for DelayedExitStatusHandler {
    fn stop(&mut self, _signal: i32, _timeout_ms: u64) -> a3s_box_core::Result<()> {
        self.stop_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn metrics(&self) -> VmMetrics {
        VmMetrics::default()
    }

    fn is_running(&self) -> bool {
        self.reports_running
    }

    fn has_exited(&self) -> bool {
        !self.reports_running
    }

    fn pid(&self) -> u32 {
        u32::MAX
    }

    fn exit_code(&self) -> Option<i32> {
        (self.exit_polls.load(Ordering::SeqCst) > self.available_after).then_some(0)
    }

    fn try_wait_exit(&mut self) -> a3s_box_core::Result<Option<i32>> {
        let poll = self.exit_polls.fetch_add(1, Ordering::SeqCst);
        if poll >= self.available_after {
            self.publish_durable_exit_if_ready();
            Ok(Some(0))
        } else {
            Ok(None)
        }
    }
}

#[test]
fn manager_uses_the_full_persisted_request_config() {
    let temporary = tempfile::tempdir().unwrap();
    let backend = VmLocalExecutionBackend::new(temporary.path());
    let record = record(temporary.path(), ExecutionIsolation::Microvm);

    let manager = backend.new_manager(&record).unwrap();

    assert_eq!(manager.config.dns, vec!["1.1.1.1"]);
    assert_eq!(
        manager.config.sysctls,
        vec![("net.ipv4.ip_forward".to_string(), "1".to_string())]
    );
    assert_eq!(manager.config.resources.memory_mb, 256);
    assert_eq!(manager.box_id(), record.id);
    assert_eq!(manager.home_dir, temporary.path());
}

#[test]
fn manager_uses_the_mutable_record_network_config() {
    let temporary = tempfile::tempdir().unwrap();
    let backend = VmLocalExecutionBackend::new(temporary.path());
    let mut record = record(temporary.path(), ExecutionIsolation::Microvm);
    record.network_mode = NetworkMode::Bridge {
        network: "connected-after-create".to_string(),
    };
    record.network_name = Some("connected-after-create".to_string());

    let manager = backend.new_manager(&record).unwrap();

    assert_eq!(manager.config.network, record.network_mode);
    assert!(matches!(
        record
            .managed_execution
            .as_ref()
            .unwrap()
            .request
            .config
            .network,
        NetworkMode::Tsi
    ));
}

#[test]
fn manager_uses_the_backend_pull_progress_callback() {
    let temporary = tempfile::tempdir().unwrap();
    let callback: crate::PullProgressFn = Arc::new(|_, _, _, _| {});
    let backend =
        VmLocalExecutionBackend::new(temporary.path()).with_pull_progress_fn(Arc::clone(&callback));
    let record = record(temporary.path(), ExecutionIsolation::Microvm);

    let manager = backend.new_manager(&record).unwrap();

    assert!(manager.pull_progress_fn.is_some());
}

#[test]
#[cfg(target_os = "linux")]
fn recovery_manager_does_not_consume_transient_registry_authorization() {
    let temporary = tempfile::tempdir().unwrap();
    let broker = TransientRegistryAuthBroker::default();
    let backend =
        VmLocalExecutionBackend::new(temporary.path()).with_transient_registry_auth(broker.clone());
    let record = record(temporary.path(), ExecutionIsolation::Microvm);
    let lease = broker
        .bind(
            &record.id,
            crate::RegistryAuth::basic("transient-user", "transient-password"),
        )
        .unwrap();

    let manager = backend.new_manager(&record).unwrap();

    assert_eq!(broker.pending(), 1);
    assert!(manager.transient_registry_auth.is_none());
    drop(lease);
    assert_eq!(broker.pending(), 0);
}

#[test]
#[cfg(target_os = "linux")]
fn boot_claim_consumes_only_its_transient_registry_authorization() {
    let temporary = tempfile::tempdir().unwrap();
    let broker = TransientRegistryAuthBroker::default();
    let backend =
        VmLocalExecutionBackend::new(temporary.path()).with_transient_registry_auth(broker.clone());
    let record = record(temporary.path(), ExecutionIsolation::Microvm);
    let lease = broker
        .bind(
            &record.id,
            crate::RegistryAuth::basic("transient-user", "transient-password"),
        )
        .unwrap();
    let mut manager = backend.new_manager(&record).unwrap();

    backend.claim_transient_registry_auth_for_boot(&mut manager);

    assert_eq!(broker.pending(), 0);
    assert_eq!(
        manager
            .transient_registry_auth
            .as_ref()
            .and_then(crate::RegistryAuth::basic_credentials),
        Some(("transient-user".into(), "transient-password".into()))
    );
    drop(lease);
    assert_eq!(broker.pending(), 0);
}

#[test]
fn manager_applies_persisted_shared_memory_policy_to_runtime_config() {
    let temporary = tempfile::tempdir().unwrap();
    let backend = VmLocalExecutionBackend::new(temporary.path());
    let mut record = record(temporary.path(), ExecutionIsolation::Microvm);
    let shm_size = 64 * 1024 * 1024;
    record.shm_size = Some(shm_size);
    record
        .managed_execution
        .as_mut()
        .unwrap()
        .request
        .policy
        .shm_size = Some(shm_size);

    let manager = backend.new_manager(&record).unwrap();

    assert!(manager
        .config
        .tmpfs
        .contains(&format!("/dev/shm:size={shm_size}")));
}

#[test]
fn validation_rejects_a_host_path_derived_from_external_input() {
    let temporary = tempfile::tempdir().unwrap();
    let backend = VmLocalExecutionBackend::new(temporary.path());
    let mut record = record(temporary.path(), ExecutionIsolation::Microvm);
    record.box_dir = temporary.path().join("external-untrusted-label");

    let error = backend.new_manager(&record).err().unwrap();

    assert!(error.to_string().contains("unexpected host directory"));
}

#[test]
fn transitional_states_retry_idempotent_pause_and_resume_operations() {
    let temporary = tempfile::tempdir().unwrap();
    let mut record = record(temporary.path(), ExecutionIsolation::Microvm);
    record.status = ManagedExecutionState::Pausing.as_status().to_string();
    record.managed_execution.as_mut().unwrap().pending_operation =
        Some(crate::ManagedExecutionOperation::Pause {
            keep_memory: true,
            operation_id: None,
        });
    assert_eq!(
        visible_active_state(&record).unwrap(),
        ExecutionState::Running
    );

    record.status = ManagedExecutionState::Resuming.as_status().to_string();
    record.managed_execution.as_mut().unwrap().pending_operation =
        Some(crate::ManagedExecutionOperation::Resume { operation_id: None });
    assert_eq!(
        visible_active_state(&record).unwrap(),
        ExecutionState::Paused
    );

    record
        .managed_execution
        .as_mut()
        .unwrap()
        .paused_with_memory = false;
    assert_eq!(
        visible_active_state(&record).unwrap(),
        ExecutionState::Running
    );
}

#[tokio::test]
async fn startup_terminal_observation_preserves_assets_for_foreground_drain() {
    let temporary = tempfile::tempdir().unwrap();
    let backend = VmLocalExecutionBackend::new(temporary.path());
    let mut record = record(temporary.path(), ExecutionIsolation::Microvm);
    record.status = ManagedExecutionState::Starting.as_status().to_string();
    record.managed_execution.as_mut().unwrap().pending_operation =
        Some(crate::ManagedExecutionOperation::Start);
    let console = record.box_dir.join("logs/console.log");
    let rootfs_sentinel = record.box_dir.join("rootfs/startup-result.txt");
    std::fs::create_dir_all(console.parent().unwrap()).unwrap();
    std::fs::create_dir_all(rootfs_sentinel.parent().unwrap()).unwrap();
    std::fs::write(&console, b"startup output\n").unwrap();
    std::fs::write(&rootfs_sentinel, b"retained").unwrap();
    let mut runtime = backend.new_manager(&record).unwrap();
    runtime.shim_exit_code = Some(17);
    *runtime.state.write().await = crate::BoxState::Ready;
    let manager = Arc::new(Mutex::new(runtime));
    backend
        .managers
        .insert(record.id.clone(), Arc::clone(&manager));

    let observation = backend.inspect_registered(&record, manager).await.unwrap();

    assert_eq!(observation.state, ExecutionState::Stopped);
    assert_eq!(observation.exit_code, Some(17));
    assert_eq!(std::fs::read(&console).unwrap(), b"startup output\n");
    assert_eq!(std::fs::read(&rootfs_sentinel).unwrap(), b"retained");
    assert!(backend.managers.is_empty());
}

#[tokio::test]
async fn cold_resume_observation_preserves_rootfs_when_the_replacement_exits() {
    let temporary = tempfile::tempdir().unwrap();
    let backend = VmLocalExecutionBackend::new(temporary.path());
    let mut record = record(temporary.path(), ExecutionIsolation::Microvm);
    record.status = ManagedExecutionState::Resuming.as_status().to_string();
    let metadata = record.managed_execution.as_mut().unwrap();
    metadata.pending_operation =
        Some(crate::ManagedExecutionOperation::Resume { operation_id: None });
    metadata.paused_with_memory = false;
    let sentinel = record.box_dir.join("rootfs/cold-resume-state.txt");
    std::fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
    std::fs::write(&sentinel, b"retained").unwrap();
    let mut replacement = backend.new_manager(&record).unwrap();
    // Cached shim zero without durable guest status must not invent Stopped(0).
    replacement.shim_exit_code = Some(0);
    *replacement.state.write().await = crate::BoxState::Ready;
    let manager = Arc::new(Mutex::new(replacement));
    backend
        .managers
        .insert(record.id.clone(), Arc::clone(&manager));

    let error = backend
        .inspect_registered(&record, Arc::clone(&manager))
        .await
        .expect_err("cached shim zero without durable guest exit must not invent Stopped");

    assert!(
        matches!(error, ExecutionManagerError::Unavailable(_)),
        "{error:?}"
    );
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"retained");
    assert!(
        backend.managers.contains_key(&record.id),
        "unauthenticated exit must retain the runtime for a later durable status"
    );
}

#[tokio::test]
async fn terminal_observation_refuses_cached_shim_zero_without_durable_guest_exit() {
    let temporary = tempfile::tempdir().unwrap();
    let backend = VmLocalExecutionBackend::new(temporary.path());
    let record = record(temporary.path(), ExecutionIsolation::Microvm);
    let sentinel = record.box_dir.join("rootfs/no-invent-state.txt");
    std::fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
    std::fs::write(&sentinel, b"retained").unwrap();
    let mut manager = backend.new_manager(&record).unwrap();
    manager.shim_exit_code = Some(0);
    *manager.state.write().await = crate::BoxState::Ready;
    let manager = Arc::new(Mutex::new(manager));
    backend
        .managers
        .insert(record.id.clone(), Arc::clone(&manager));

    let error = backend
        .inspect_registered(&record, Arc::clone(&manager))
        .await
        .unwrap_err();

    assert!(
        matches!(error, ExecutionManagerError::Unavailable(_)),
        "{error:?}"
    );
    assert!(backend.managers.contains_key(&record.id));
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"retained");
}

#[tokio::test]
async fn terminal_observation_accepts_cached_shim_zero_with_durable_guest_exit() {
    let temporary = tempfile::tempdir().unwrap();
    let backend = VmLocalExecutionBackend::new(temporary.path());
    let record = record(temporary.path(), ExecutionIsolation::Microvm);
    let rootfs = record.box_dir.join("rootfs");
    std::fs::create_dir_all(&rootfs).unwrap();
    std::fs::write(rootfs.join(".a3s_exit_code"), "0\n").unwrap();
    let mut manager = backend.new_manager(&record).unwrap();
    manager.shim_exit_code = Some(0);
    *manager.state.write().await = crate::BoxState::Ready;
    let manager = Arc::new(Mutex::new(manager));
    backend
        .managers
        .insert(record.id.clone(), Arc::clone(&manager));

    let observation = backend.inspect_registered(&record, manager).await.unwrap();

    assert_eq!(observation.state, ExecutionState::Stopped);
    assert_eq!(observation.exit_code, Some(0));
    assert!(backend.managers.is_empty());
}

#[tokio::test]
async fn terminal_health_probe_waits_for_delayed_durable_exit_status_before_cleanup() {
    let temporary = tempfile::tempdir().unwrap();
    let backend = VmLocalExecutionBackend::new(temporary.path());
    let record = record(temporary.path(), ExecutionIsolation::Sandbox);
    let rootfs = record.box_dir.join("rootfs");
    std::fs::create_dir_all(&rootfs).unwrap();
    let durable_exit_path = rootfs.join(".a3s_exit_code");
    let exit_polls = Arc::new(AtomicUsize::new(0));
    let stop_calls = Arc::new(AtomicUsize::new(0));
    let manager = Arc::new(Mutex::new(backend.new_manager(&record).unwrap()));
    {
        let manager = manager.lock().await;
        *manager.state.write().await = crate::BoxState::Ready;
        *manager.handler.write().await = Some(Box::new(DelayedExitStatusHandler {
            exit_polls: Arc::clone(&exit_polls),
            stop_calls: Arc::clone(&stop_calls),
            available_after: 60,
            reports_running: false,
            durable_exit_path: Some(durable_exit_path),
        }));
    }
    backend
        .managers
        .insert(record.id.clone(), Arc::clone(&manager));

    let observation = backend.inspect_registered(&record, manager).await.unwrap();

    assert_eq!(observation.state, ExecutionState::Stopped);
    assert_eq!(observation.exit_code, Some(0));
    assert_eq!(exit_polls.load(Ordering::SeqCst), 61);
    assert_eq!(stop_calls.load(Ordering::SeqCst), 1);
    assert!(backend.managers.is_empty());
}

#[tokio::test]
async fn terminal_observation_retains_runtime_without_an_exact_exit_status() {
    let temporary = tempfile::tempdir().unwrap();
    let backend = VmLocalExecutionBackend::new(temporary.path());
    let record = record(temporary.path(), ExecutionIsolation::Sandbox);
    let exit_polls = Arc::new(AtomicUsize::new(0));
    let stop_calls = Arc::new(AtomicUsize::new(0));
    let manager = Arc::new(Mutex::new(backend.new_manager(&record).unwrap()));
    {
        let manager = manager.lock().await;
        *manager.state.write().await = crate::BoxState::Ready;
        *manager.handler.write().await = Some(Box::new(DelayedExitStatusHandler {
            exit_polls: Arc::clone(&exit_polls),
            stop_calls: Arc::clone(&stop_calls),
            available_after: usize::MAX,
            reports_running: false,
            durable_exit_path: None,
        }));
    }
    backend
        .managers
        .insert(record.id.clone(), Arc::clone(&manager));

    let error = backend
        .inspect_registered(&record, manager)
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        ExecutionManagerError::Unavailable(message)
            if message.contains("exact exit status")
    ));
    assert!(exit_polls.load(Ordering::SeqCst) > 1);
    assert_eq!(stop_calls.load(Ordering::SeqCst), 0);
    assert!(backend.managers.contains_key(&record.id));
}

#[tokio::test]
async fn disappearing_live_handle_waits_for_delayed_terminal_status() {
    let temporary = tempfile::tempdir().unwrap();
    let backend = VmLocalExecutionBackend::new(temporary.path());
    let mut record = record(temporary.path(), ExecutionIsolation::Sandbox);
    let rootfs = record.box_dir.join("rootfs");
    std::fs::create_dir_all(&rootfs).unwrap();
    std::fs::write(rootfs.join(".a3s_exit_code"), "0\n").unwrap();
    record.status = ManagedExecutionState::Running.as_status().to_string();
    let exit_polls = Arc::new(AtomicUsize::new(0));
    let stop_calls = Arc::new(AtomicUsize::new(0));
    let mut runtime = backend.new_manager(&record).unwrap();
    runtime.exec_socket_path = Some(record.exec_socket_path.clone());
    *runtime.state.write().await = crate::BoxState::Ready;
    *runtime.handler.write().await = Some(Box::new(DelayedExitStatusHandler {
        exit_polls: Arc::clone(&exit_polls),
        stop_calls: Arc::clone(&stop_calls),
        available_after: 3,
        reports_running: true,
        durable_exit_path: None,
    }));
    let manager = Arc::new(Mutex::new(runtime));
    backend
        .managers
        .insert(record.id.clone(), Arc::clone(&manager));

    let observation = backend.inspect_registered(&record, manager).await.unwrap();

    assert_eq!(observation.state, ExecutionState::Stopped);
    assert_eq!(observation.exit_code, Some(0));
    assert_eq!(exit_polls.load(Ordering::SeqCst), 4);
    assert_eq!(stop_calls.load(Ordering::SeqCst), 1);
    assert!(backend.managers.is_empty());
}

#[test]
fn restart_teardown_preserves_old_runtime_visibility_until_generation_advance() {
    let temporary = tempfile::tempdir().unwrap();
    let mut record = record(temporary.path(), ExecutionIsolation::Microvm);
    record.status = ManagedExecutionState::RestartStopping
        .as_status()
        .to_string();
    record.managed_execution.as_mut().unwrap().pending_operation =
        Some(crate::ManagedExecutionOperation::Restart {
            operation_id: OperationId::new("operation-restart").unwrap(),
            source_generation: ExecutionGeneration::INITIAL,
            source_state: ManagedExecutionState::Paused,
            stop_timeout_secs: None,
        });
    assert_eq!(
        visible_active_state(&record).unwrap(),
        ExecutionState::Paused
    );

    record.status = ManagedExecutionState::RestartStarting
        .as_status()
        .to_string();
    record.managed_execution.as_mut().unwrap().generation = ExecutionGeneration::new(2).unwrap();
    assert_eq!(
        visible_active_state(&record).unwrap(),
        ExecutionState::Running
    );
}

#[tokio::test]
async fn filesystem_only_pause_fails_before_starting_a_runtime() {
    let temporary = tempfile::tempdir().unwrap();
    let backend = VmLocalExecutionBackend::new(temporary.path());
    let sandbox = record(temporary.path(), ExecutionIsolation::Sandbox);
    let microvm = record(temporary.path(), ExecutionIsolation::Microvm);

    let sandbox_error = backend.pause(&sandbox, false).await.unwrap_err();
    let memory_error = backend.pause(&microvm, false).await.unwrap_err();

    assert!(sandbox_error
        .to_string()
        .contains("pause without memory retention"));
    assert!(memory_error
        .to_string()
        .contains("pause without memory retention"));
    assert!(backend.managers.is_empty());
}

#[test]
fn unsupported_backend_capabilities_are_unavailable() {
    let temporary = tempfile::tempdir().unwrap();
    let record = record(temporary.path(), ExecutionIsolation::Microvm);

    let error = unsupported(&record, "pause", "the test backend");

    assert!(matches!(
        error,
        ExecutionManagerError::Unavailable(message)
            if message.contains("pause")
                && message.contains("the test backend")
                && message.contains(&record.id)
    ));
}

#[tokio::test]
async fn retained_stops_preserve_anonymous_volumes_but_auto_remove_kill_removes_them() {
    let temporary = tempfile::tempdir().unwrap();
    let backend = VmLocalExecutionBackend::new(temporary.path());
    let mut record = record(temporary.path(), ExecutionIsolation::Microvm);
    let volume_name = "anon_restart_volume";
    let volumes = crate::VolumeStore::new(
        temporary.path().join("volumes.json"),
        temporary.path().join("volumes"),
    );
    volumes.claim_anonymous(volume_name, &record.id).unwrap();
    record.anonymous_volumes = vec![volume_name.to_string()];
    let sentinel = record.box_dir.join("rootfs/workspace/cold-pause.txt");
    std::fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
    std::fs::write(&sentinel, b"retained").unwrap();

    let manager = Arc::new(Mutex::new(backend.new_manager(&record).unwrap()));
    backend.managers.insert(record.id.clone(), manager);
    backend.stop_for_restart(&record, Some(0)).await.unwrap();
    assert!(volumes.get(volume_name).unwrap().is_some());
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"retained");

    let manager = Arc::new(Mutex::new(backend.new_manager(&record).unwrap()));
    backend.managers.insert(record.id.clone(), manager);
    backend.kill(&record).await.unwrap();
    assert!(volumes.get(volume_name).unwrap().is_some());
    assert!(!sentinel.exists());

    record.auto_remove = true;
    record
        .managed_execution
        .as_mut()
        .unwrap()
        .request
        .policy
        .auto_remove = true;
    let manager = Arc::new(Mutex::new(backend.new_manager(&record).unwrap()));
    backend.managers.insert(record.id.clone(), manager);
    backend.kill(&record).await.unwrap();
    assert!(volumes.get(volume_name).unwrap().is_none());
}

#[tokio::test]
async fn terminal_kill_cleans_a_cold_paused_rootfs_without_runtime_evidence() {
    let temporary = tempfile::tempdir().unwrap();
    let backend = VmLocalExecutionBackend::new(temporary.path());
    let mut record = record(temporary.path(), ExecutionIsolation::Microvm);
    record.status = ManagedExecutionState::Killing.as_status().to_string();
    let metadata = record.managed_execution.as_mut().unwrap();
    metadata.paused_with_memory = false;
    metadata.pending_operation = Some(crate::ManagedExecutionOperation::Kill {
        signal: None,
        timeout_secs: None,
    });
    let sentinel = record.box_dir.join("rootfs/workspace/cold-pause.txt");
    std::fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
    std::fs::write(&sentinel, b"retained").unwrap();

    assert_eq!(backend.kill(&record).await.unwrap(), KillOutcome::Killed);
    assert!(!record.box_dir.exists());
    assert!(backend.managers.is_empty());
}

#[test]
fn managed_kill_uses_persisted_stop_signal_and_timeout() {
    let temporary = tempfile::tempdir().unwrap();
    let mut record = record(temporary.path(), ExecutionIsolation::Microvm);

    assert_eq!(graceful_stop_options(&record, None).unwrap(), None);

    record.stop_signal = Some("SIGINT".to_string());
    assert_eq!(
        graceful_stop_options(&record, None).unwrap(),
        Some((libc::SIGINT, a3s_box_core::DEFAULT_SHUTDOWN_TIMEOUT_MS))
    );

    record.stop_timeout = Some(7);
    assert_eq!(
        graceful_stop_options(&record, record.stop_timeout).unwrap(),
        Some((libc::SIGINT, 7_000))
    );
    assert_eq!(
        graceful_stop_options(&record, Some(3)).unwrap(),
        Some((libc::SIGINT, 3_000))
    );
}

#[test]
fn managed_kill_rejects_stop_timeout_overflow() {
    let temporary = tempfile::tempdir().unwrap();
    let record = record(temporary.path(), ExecutionIsolation::Microvm);

    let error = graceful_stop_options(&record, Some(u64::MAX)).unwrap_err();

    assert!(error.to_string().contains("stop timeout is too large"));
}

#[test]
fn visible_state_rejects_terminal_records() {
    let temporary = tempfile::tempdir().unwrap();
    let mut record = record(temporary.path(), ExecutionIsolation::Microvm);
    record.status = ManagedExecutionState::Stopped.as_status().to_string();
    record.managed_execution.as_mut().unwrap().generation = ExecutionGeneration::INITIAL;

    assert!(visible_active_state(&record).is_err());
}

#[tokio::test]
async fn starting_observation_stays_creating_without_exec_heartbeat() {
    // Path presence alone must not invent Running — Unix and Windows both
    // require an authenticated exec heartbeat (#407/#408/#411 parity).
    let temporary = tempfile::tempdir().unwrap();
    let backend = VmLocalExecutionBackend::new(temporary.path());
    let mut record = record(temporary.path(), ExecutionIsolation::Microvm);
    record.status = ManagedExecutionState::Starting.as_status().to_string();
    record.managed_execution.as_mut().unwrap().pending_operation =
        Some(crate::ManagedExecutionOperation::Start);

    let socket_dir = crate::vm::runtime_socket_dir(temporary.path(), &record.id);
    let exec_socket = socket_dir.join("exec.sock");
    std::fs::create_dir_all(&socket_dir).unwrap();

    let mut manager = backend.new_manager(&record).unwrap();
    manager.exec_socket_path = Some(exec_socket);
    *manager.state.write().await = crate::BoxState::Ready;
    *manager.handler.write().await = Some(Box::new(DelayedExitStatusHandler {
        exit_polls: Arc::new(AtomicUsize::new(0)),
        stop_calls: Arc::new(AtomicUsize::new(0)),
        available_after: usize::MAX,
        reports_running: true,
        durable_exit_path: None,
    }));
    let manager = Arc::new(Mutex::new(manager));
    backend
        .managers
        .insert(record.id.clone(), Arc::clone(&manager));

    let observation = backend.inspect_registered(&record, manager).await.unwrap();

    assert_eq!(
        observation.state,
        ExecutionState::Creating,
        "exec path without heartbeat must not invent Running"
    );
    assert!(observation.handle.is_none());
    assert!(observation.exit_code.is_none());
}

#[tokio::test]
async fn created_observation_stays_creating_without_exec_heartbeat() {
    // promote_if_ready must not invent Ready from a constructed layout path.
    let temporary = tempfile::tempdir().unwrap();
    let backend = VmLocalExecutionBackend::new(temporary.path());
    let mut record = record(temporary.path(), ExecutionIsolation::Microvm);
    record.status = ManagedExecutionState::Created.as_status().to_string();

    let manager = backend.new_manager(&record).unwrap();
    *manager.state.write().await = crate::BoxState::Created;
    *manager.handler.write().await = Some(Box::new(DelayedExitStatusHandler {
        exit_polls: Arc::new(AtomicUsize::new(0)),
        stop_calls: Arc::new(AtomicUsize::new(0)),
        available_after: usize::MAX,
        reports_running: true,
        durable_exit_path: None,
    }));
    let manager = Arc::new(Mutex::new(manager));
    backend
        .managers
        .insert(record.id.clone(), Arc::clone(&manager));

    let observation = backend.inspect_registered(&record, manager).await.unwrap();

    assert_eq!(
        observation.state,
        ExecutionState::Creating,
        "layout exec.sock path alone must not invent Ready/Running"
    );
    assert!(observation.handle.is_none());
}
