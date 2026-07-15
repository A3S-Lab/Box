use std::collections::BTreeMap;

use a3s_box_core::{
    volume::VolumeConfig, BoxConfig, CreateExecutionRequest, ExecutionGeneration,
    ExecutionIsolation, OperationId,
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
        },
        chrono::Utc::now(),
    )
    .unwrap()
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
        Some(crate::ManagedExecutionOperation::Pause { keep_memory: true });
    assert_eq!(
        visible_active_state(&record).unwrap(),
        ExecutionState::Running
    );

    record.status = ManagedExecutionState::Resuming.as_status().to_string();
    record.managed_execution.as_mut().unwrap().pending_operation =
        Some(crate::ManagedExecutionOperation::Resume);
    assert_eq!(
        visible_active_state(&record).unwrap(),
        ExecutionState::Paused
    );
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
async fn unsupported_pause_modes_fail_before_starting_a_runtime() {
    let temporary = tempfile::tempdir().unwrap();
    let backend = VmLocalExecutionBackend::new(temporary.path());
    let sandbox = record(temporary.path(), ExecutionIsolation::Sandbox);
    let microvm = record(temporary.path(), ExecutionIsolation::Microvm);

    let sandbox_error = backend.pause(&sandbox, true).await.unwrap_err();
    let memory_error = backend.pause(&microvm, false).await.unwrap_err();

    assert!(sandbox_error.to_string().contains("Sandbox backend"));
    assert!(memory_error
        .to_string()
        .contains("pause without memory retention"));
    assert!(backend.managers.is_empty());
}

#[tokio::test]
async fn retained_stops_preserve_anonymous_volumes_but_auto_remove_kill_removes_them() {
    let temporary = tempfile::tempdir().unwrap();
    let backend = VmLocalExecutionBackend::new(temporary.path());
    let mut record = record(temporary.path(), ExecutionIsolation::Microvm);
    let volume_name = "anonymous-restart-volume";
    let volumes = crate::VolumeStore::new(
        temporary.path().join("volumes.json"),
        temporary.path().join("volumes"),
    );
    volumes.create(VolumeConfig::new(volume_name, "")).unwrap();
    record.anonymous_volumes = vec![volume_name.to_string()];

    let manager = Arc::new(Mutex::new(backend.new_manager(&record).unwrap()));
    backend.managers.insert(record.id.clone(), manager);
    backend.stop_for_restart(&record, Some(0)).await.unwrap();
    assert!(volumes.get(volume_name).unwrap().is_some());

    let manager = Arc::new(Mutex::new(backend.new_manager(&record).unwrap()));
    backend.managers.insert(record.id.clone(), manager);
    backend.kill(&record).await.unwrap();
    assert!(volumes.get(volume_name).unwrap().is_some());

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
