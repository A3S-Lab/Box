use super::*;

#[test]
fn test_build_box_config_uses_keepalive_for_interactive_tty_boot() {
    let mut args = default_run_args();
    args.tty = true;
    args.cmd = vec!["/bin/echo".to_string(), "hello".to_string()];

    let config = build_box_config(
        &args,
        512,
        Default::default(),
        None,
        vec![],
        vec![],
        vec![],
        a3s_box_core::NetworkMode::Tsi,
        vec![],
        TeeConfig::None,
    )
    .unwrap();

    assert_eq!(config.cmd, vec!["a3s-box-pty-keepalive"]);
    assert_eq!(
        config.entrypoint_override,
        Some(interactive_keepalive_entrypoint())
    );
}

#[test]
fn test_build_box_config_plumbs_virtiofs_cache_mode() {
    let mut args = default_run_args();
    args.common.virtiofs_cache = Some(common::VirtiofsCacheMode::Always);

    let config = build_box_config(
        &args,
        512,
        Default::default(),
        None,
        vec![],
        vec![],
        vec![],
        a3s_box_core::NetworkMode::Tsi,
        vec![],
        TeeConfig::None,
    )
    .unwrap();

    assert_eq!(config.virtiofs_cache.as_deref(), Some("always"));
}

#[test]
fn test_build_box_config_preserves_non_tty_command() {
    let mut args = default_run_args();
    args.cmd = vec!["/bin/echo".to_string(), "hello".to_string()];
    let entrypoint = Some(vec!["/custom-entrypoint".to_string()]);

    let config = build_box_config(
        &args,
        512,
        Default::default(),
        entrypoint.clone(),
        vec![],
        vec![],
        vec![],
        a3s_box_core::NetworkMode::Tsi,
        vec![],
        TeeConfig::None,
    )
    .unwrap();

    assert_eq!(config.cmd, args.cmd);
    assert_eq!(config.entrypoint_override, entrypoint);
}

#[test]
fn test_build_box_config_controls_stdin_open() {
    let args = default_run_args();
    let config = build_box_config(
        &args,
        512,
        Default::default(),
        None,
        vec![],
        vec![],
        vec![],
        a3s_box_core::NetworkMode::Tsi,
        vec![],
        TeeConfig::None,
    )
    .unwrap();
    assert!(!config.stdin_open);

    let mut args = default_run_args();
    args.interactive = true;
    let config = build_box_config(
        &args,
        512,
        Default::default(),
        None,
        vec![],
        vec![],
        vec![],
        a3s_box_core::NetworkMode::Tsi,
        vec![],
        TeeConfig::None,
    )
    .unwrap();
    assert!(config.stdin_open);

    let mut args = default_run_args();
    args.no_stdin = true;
    let config = build_box_config(
        &args,
        512,
        Default::default(),
        None,
        vec![],
        vec![],
        vec![],
        a3s_box_core::NetworkMode::Tsi,
        vec![],
        TeeConfig::None,
    )
    .unwrap();
    assert!(!config.stdin_open);
}

#[test]
fn test_mark_record_stopped_persists_exit_context() {
    let record = crate::test_helpers::fixtures::make_record(
        "550e8400-e29b-41d4-a716-446655440000",
        "run-exit",
        "running",
        Some(1234),
    );
    let (_tmp, mut state) = crate::test_helpers::fixtures::setup_state(vec![record]);

    mark_record_stopped(
        &mut state,
        "550e8400-e29b-41d4-a716-446655440000",
        Some(42),
        true,
    );

    let record = state
        .find_by_id("550e8400-e29b-41d4-a716-446655440000")
        .unwrap();
    assert_eq!(record.status, "stopped");
    assert_eq!(record.pid, None);
    assert_eq!(record.exit_code, Some(42));
    assert!(record.stopped_by_user);
}

#[test]
fn test_completed_managed_start_requires_stopped_state_and_exact_exit_code() {
    fn managed_record(status: a3s_box_runtime::ManagedExecutionState) -> BoxRecord {
        let id = "11111111-1111-4111-8111-111111111111";
        let mut record = crate::test_helpers::fixtures::make_record(
            id,
            "startup-completion",
            status.as_status(),
            None,
        );
        record.isolation = a3s_box_core::ExecutionIsolation::Sandbox;
        record.managed_execution = Some(
            a3s_box_runtime::ManagedExecutionMetadata::new(
                OperationId::new("operation-startup-completion").unwrap(),
                ExecutionGeneration::INITIAL,
                CreateExecutionRequest {
                    external_sandbox_id: "startup-completion".to_string(),
                    config: BoxConfig {
                        isolation: a3s_box_core::ExecutionIsolation::Sandbox,
                        image: record.image.clone(),
                        ..Default::default()
                    },
                    labels: std::collections::BTreeMap::new(),
                    policy: Default::default(),
                    rootfs_snapshot_id: None,
                },
            )
            .unwrap(),
        );
        record
    }

    let mut stopped = managed_record(a3s_box_runtime::ManagedExecutionState::Stopped);
    stopped.exit_code = Some(17);
    assert!(is_completed_managed_start(&stopped));

    stopped.exit_code = None;
    assert!(!is_completed_managed_start(&stopped));

    let mut failed = managed_record(a3s_box_runtime::ManagedExecutionState::Failed);
    failed.exit_code = Some(17);
    assert!(!is_completed_managed_start(&failed));
}

#[tokio::test]
async fn test_cleanup_failure_is_reported_and_preserves_recovery_state() {
    let temporary = tempfile::tempdir().unwrap();
    let id = "550e8400-e29b-41d4-a716-446655440000";
    let mut record =
        crate::test_helpers::fixtures::make_record(id, "run-cleanup", "running", Some(1234));
    record.box_dir = temporary.path().join("boxes").join(id);
    record.exec_socket_path = record.box_dir.join("sockets/exec.sock");
    std::fs::create_dir_all(&record.box_dir).unwrap();

    let backend = VmLocalExecutionBackend::new(temporary.path());
    let manager = LocalExecutionManager::new(
        temporary.path().join("empty-state.json"),
        temporary.path(),
        std::sync::Arc::new(backend),
    );
    let mut context = RunContext {
        manager,
        execution_id: ExecutionId::new(id).unwrap(),
        generation: ExecutionGeneration::new(1).unwrap(),
        box_id: id.to_string(),
        box_dir: record.box_dir.clone(),
        name: record.name.clone(),
        record,
        exec_socket_path: temporary.path().join("exec.sock"),
        pty_socket_path: temporary.path().join("pty.sock"),
        anonymous_volumes: Vec::new(),
        health_checker: None,
        completed_during_start: false,
    };

    let error = cleanup_managed_execution(&mut context, true, Some(1), false, false)
        .await
        .unwrap_err();

    assert!(error
        .to_string()
        .contains("state was preserved for recovery"));
    assert!(context.box_dir.exists());
}

#[test]
fn test_foreground_exit_code_preserves_vm_code() {
    assert_eq!(
        foreground_exit_code(
            ForegroundStopReason::UserInterrupted(FOREGROUND_SIGTERM),
            Some(143)
        ),
        Some(143)
    );
    assert_eq!(
        foreground_exit_code(ForegroundStopReason::VmUnhealthy, Some(2)),
        Some(2)
    );
    assert_eq!(
        foreground_exit_code(ForegroundStopReason::TimedOut, Some(0)),
        Some(124)
    );
}

#[test]
fn test_foreground_workload_exit_code_falls_back_to_completed_start_record() {
    let temporary = tempfile::tempdir().unwrap();

    assert_eq!(
        foreground_workload_exit_code(temporary.path(), Some(0)),
        Some(0)
    );

    let exit_path = temporary.path().join("upper/.a3s_exit_code");
    std::fs::create_dir_all(exit_path.parent().unwrap()).unwrap();
    std::fs::write(exit_path, "7\n").unwrap();
    assert_eq!(
        foreground_workload_exit_code(temporary.path(), Some(1)),
        Some(7)
    );
}

#[test]
fn test_foreground_natural_exit_without_guest_result_fails_closed() {
    assert_eq!(
        foreground_exit_code(ForegroundStopReason::ProcessExited, None),
        Some(1),
        "a dead runtime without a trusted guest result must not report success"
    );
}

#[test]
fn test_foreground_exit_code_has_deterministic_fallbacks() {
    assert_eq!(
        foreground_exit_code(
            ForegroundStopReason::UserInterrupted(FOREGROUND_SIGINT),
            None
        ),
        Some(130)
    );
    assert_eq!(
        foreground_exit_code(
            ForegroundStopReason::UserInterrupted(FOREGROUND_SIGTERM),
            None
        ),
        Some(143)
    );
    assert_eq!(
        foreground_exit_code(ForegroundStopReason::VmUnhealthy, None),
        Some(1)
    );
    assert_eq!(
        foreground_exit_code(ForegroundStopReason::TimedOut, None),
        Some(124)
    );
}

#[test]
fn test_foreground_stop_reason_user_flag() {
    assert!(ForegroundStopReason::UserInterrupted(FOREGROUND_SIGINT).stopped_by_user());
    assert!(!ForegroundStopReason::ProcessExited.stopped_by_user());
    assert!(!ForegroundStopReason::VmUnhealthy.stopped_by_user());
    assert!(!ForegroundStopReason::TimedOut.stopped_by_user());
}

#[test]
fn test_foreground_poll_cadence_avoids_fixed_startup_delay() {
    assert!(FOREGROUND_EXIT_POLL <= std::time::Duration::from_millis(20));
    assert!(FOREGROUND_EXIT_POLL < FOREGROUND_HEALTH_POLL);
    assert!(FOREGROUND_LOG_DRAIN_QUIET <= std::time::Duration::from_millis(50));
    assert!(FOREGROUND_LOG_DRAIN_POLL < FOREGROUND_LOG_DRAIN_QUIET);
}

#[tokio::test]
async fn finished_foreground_writers_need_no_additional_quiet_period() {
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("console.log");
    std::fs::write(&log, b"complete").unwrap();
    let position = AtomicU64::new(8);

    tokio::time::timeout(
        std::time::Duration::from_millis(5),
        wait_for_foreground_log_drain(&[(&log, &position)], true),
    )
    .await
    .expect("a caught-up tail must return immediately after every writer exited");
}

#[tokio::test]
async fn finished_foreground_writer_wait_still_requires_tail_catch_up() {
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("console.log");
    std::fs::write(&log, b"pending").unwrap();
    let position = AtomicU64::new(0);

    assert!(tokio::time::timeout(
        std::time::Duration::from_millis(5),
        wait_for_foreground_log_drain(&[(&log, &position)], true),
    )
    .await
    .is_err());
}

#[test]
fn test_retained_log_hint_only_for_non_user_failures() {
    assert!(should_print_retained_log_hint(Some(1), false));
    assert!(!should_print_retained_log_hint(Some(0), false));
    assert!(!should_print_retained_log_hint(None, false));
    assert!(!should_print_retained_log_hint(Some(130), true));
}

#[test]
fn test_foreground_completion_messages() {
    assert_eq!(
        foreground_completion_message(ForegroundStopReason::ProcessExited, true, "box"),
        "Box box exited and was removed."
    );
    assert_eq!(
        foreground_completion_message(
            ForegroundStopReason::UserInterrupted(FOREGROUND_SIGINT),
            false,
            "box"
        ),
        "Box box stopped."
    );
    assert_eq!(
        foreground_completion_message(ForegroundStopReason::VmUnhealthy, true, "box"),
        "Box box stopped after VM health check failed and was removed."
    );
    assert_eq!(
        foreground_completion_message(ForegroundStopReason::TimedOut, false, "box"),
        "Box box stopped after --timeout expired."
    );
}

#[test]
fn terminal_status_turns_a_lost_heartbeat_into_natural_completion() {
    assert_eq!(
        foreground_health_stop_reason(Some(0)),
        ForegroundStopReason::ProcessExited
    );
    assert_eq!(
        foreground_health_stop_reason(Some(23)),
        ForegroundStopReason::ProcessExited
    );
    assert_eq!(
        foreground_health_stop_reason(None),
        ForegroundStopReason::VmUnhealthy
    );
}

#[test]
fn test_build_box_config_passes_security_options() {
    let mut args = default_run_args();
    args.common.cap_add = vec!["NET_ADMIN".to_string()];
    args.common.cap_drop = vec!["NET_RAW".to_string()];
    args.common.security_opt = vec!["seccomp=unconfined".to_string()];
    args.common.privileged = true;

    let config = build_box_config(
        &args,
        512,
        a3s_box_core::config::ResourceLimits::default(),
        None,
        vec![],
        vec![],
        vec![],
        a3s_box_core::NetworkMode::Tsi,
        vec![],
        TeeConfig::None,
    )
    .unwrap();

    assert_eq!(config.cap_add, vec!["NET_ADMIN"]);
    assert_eq!(config.cap_drop, vec!["NET_RAW"]);
    assert_eq!(config.security_opt, vec!["seccomp=unconfined"]);
    assert!(config.privileged);
}

#[test]
fn test_build_box_config_passes_user_and_workdir() {
    let mut args = default_run_args();
    args.common.user = Some("root:root".to_string());
    args.common.workdir = Some("/app".to_string());

    let config = build_box_config(
        &args,
        512,
        a3s_box_core::config::ResourceLimits::default(),
        None,
        vec![],
        vec![],
        vec![],
        a3s_box_core::NetworkMode::Tsi,
        vec![],
        TeeConfig::None,
    )
    .unwrap();

    assert_eq!(config.user.as_deref(), Some("0:0"));
    assert_eq!(config.workdir.as_deref(), Some("/app"));
}

#[test]
fn test_build_box_config_passes_hostname_and_add_hosts() {
    let mut args = default_run_args();
    args.common.hostname = Some("web".to_string());
    args.common.add_host = vec!["db.local:10.88.0.10".to_string()];

    let config = build_box_config(
        &args,
        512,
        a3s_box_core::config::ResourceLimits::default(),
        None,
        vec![],
        vec![],
        vec![],
        a3s_box_core::NetworkMode::Tsi,
        vec![],
        TeeConfig::None,
    )
    .unwrap();

    assert_eq!(config.hostname.as_deref(), Some("web"));
    assert_eq!(config.add_hosts, vec!["db.local:10.88.0.10"]);
}

#[test]
fn test_resolve_volumes_empty() {
    let (resolved, names) = resolve_volumes(&[]).unwrap();
    assert!(resolved.is_empty());
    assert!(names.is_empty());
}
