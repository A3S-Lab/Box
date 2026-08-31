use super::*;

#[cfg(target_os = "windows")]
#[test]
fn test_append_windows_guest_stream_uses_shared_phase_and_keeps_partial_lines() {
    let tmp = tempfile::tempdir().unwrap();
    let stdout_source = tmp.path().join("stdout.source");
    let stderr_source = tmp.path().join("stderr.source");
    let stdout_destination = tmp.path().join("stdout.destination");
    let stderr_destination = tmp.path().join("stderr.destination");
    std::fs::write(
        &stdout_source,
        concat!(
            "init.krun: mount_filesystems ok\n",
            "init.krun: business\n",
            "init.krun: config parsed",
        ),
    )
    .unwrap();
    std::fs::write(
        &stderr_source,
        concat!(
            "init.krun: execvp(/bin/app) starting\n",
            "init.krun: mount_filesystems ok\n",
        ),
    )
    .unwrap();

    let filter = a3s_box_core::log::RuntimeConsoleFilter::new();
    append_windows_guest_stream(&stdout_source, &stdout_destination, &filter).unwrap();
    append_windows_guest_stream(&stderr_source, &stderr_destination, &filter).unwrap();

    assert_eq!(
        std::fs::read_to_string(stdout_destination).unwrap(),
        "init.krun: business\ninit.krun: config parsed"
    );
    assert_eq!(
        std::fs::read_to_string(stderr_destination).unwrap(),
        "init.krun: mount_filesystems ok\n"
    );
}

#[cfg(target_os = "windows")]
#[test]
fn test_collect_windows_guest_result_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let box_dir = tmp.path().join("box");
    let rootfs = box_dir.join("rootfs");
    std::fs::create_dir_all(&rootfs).unwrap();
    std::fs::write(rootfs.join(WINDOWS_GUEST_STDOUT), "once\n").unwrap();
    std::fs::write(rootfs.join(WINDOWS_GUEST_STDERR), "error once\n").unwrap();
    std::fs::write(rootfs.join(WINDOWS_GUEST_EXIT_CODE), "7\n").unwrap();

    let config = a3s_box_core::log::LogConfig::default();
    assert_eq!(
        collect_windows_guest_result(&box_dir, &config, 0).unwrap(),
        7
    );
    assert_eq!(
        collect_windows_guest_result(&box_dir, &config, 0).unwrap(),
        7
    );

    let logs = box_dir.join("logs");
    assert_eq!(
        std::fs::read_to_string(logs.join("console.log")).unwrap(),
        "once\n"
    );
    assert_eq!(
        std::fs::read_to_string(logs.join("console.err.log")).unwrap(),
        "error once\n"
    );
    let json = std::fs::read_to_string(logs.join("container.json")).unwrap();
    assert_eq!(json.matches("\"log\":\"once\\n\"").count(), 1);
    assert_eq!(json.matches("\"log\":\"error once\\n\"").count(), 1);
}

#[cfg(target_os = "windows")]
#[test]
fn test_collect_windows_guest_result_does_not_replay_drained_live_logs() {
    let tmp = tempfile::tempdir().unwrap();
    let box_dir = tmp.path().join("box");
    let rootfs = box_dir.join("rootfs");
    let logs = box_dir.join("logs");
    std::fs::create_dir_all(&rootfs).unwrap();
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(rootfs.join(WINDOWS_GUEST_STDOUT), "live once\n").unwrap();
    std::fs::write(rootfs.join(WINDOWS_GUEST_STDERR), "live error once\n").unwrap();
    std::fs::write(rootfs.join(WINDOWS_GUEST_EXIT_CODE), "4\n").unwrap();
    std::fs::write(rootfs.join(WINDOWS_LIVE_LOGS_DRAINED_MARKER), "drained\n").unwrap();
    let live_json =
        "{\"log\":\"live once\\n\",\"stream\":\"stdout\",\"time\":\"2026-01-01T00:00:00Z\"}\n";
    std::fs::write(logs.join("container.json"), live_json).unwrap();

    let config = a3s_box_core::log::LogConfig::default();
    assert_eq!(
        collect_windows_guest_result(&box_dir, &config, 0).unwrap(),
        4
    );

    assert_eq!(
        std::fs::read_to_string(logs.join("container.json")).unwrap(),
        live_json
    );
    assert_eq!(
        std::fs::read_to_string(logs.join("console.log")).unwrap(),
        "live once\n"
    );
    assert_eq!(
        std::fs::read_to_string(logs.join("console.err.log")).unwrap(),
        "live error once\n"
    );
    assert!(rootfs.join(WINDOWS_GUEST_RESULT_MARKER).exists());
}

#[cfg(target_os = "windows")]
#[test]
fn test_collect_windows_guest_result_replaces_marker_symlink_without_touching_target() {
    let tmp = tempfile::tempdir().unwrap();
    let box_dir = tmp.path().join("box");
    let rootfs = box_dir.join("rootfs");
    std::fs::create_dir_all(&rootfs).unwrap();
    std::fs::write(rootfs.join(WINDOWS_GUEST_STDOUT), "safe output\n").unwrap();
    std::fs::write(rootfs.join(WINDOWS_GUEST_STDERR), "").unwrap();
    std::fs::write(rootfs.join(WINDOWS_GUEST_EXIT_CODE), "0\n").unwrap();

    let host_target = tmp.path().join("host-target.txt");
    std::fs::write(&host_target, "host secret").unwrap();
    let marker = rootfs.join(WINDOWS_GUEST_RESULT_MARKER);
    match std::os::windows::fs::symlink_file(&host_target, &marker) {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == Some(1314) => return,
        Err(error) => panic!("failed to create marker symlink: {error}"),
    }

    let config = a3s_box_core::log::LogConfig::default();
    assert_eq!(
        collect_windows_guest_result(&box_dir, &config, 0).unwrap(),
        0
    );
    assert_eq!(
        std::fs::read_to_string(&host_target).unwrap(),
        "host secret"
    );
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "collected\n");
    assert!(!std::fs::symlink_metadata(marker)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[cfg(target_os = "windows")]
#[test]
fn test_collect_windows_guest_result_refuses_stream_symlink() {
    let tmp = tempfile::tempdir().unwrap();
    let box_dir = tmp.path().join("box");
    let rootfs = box_dir.join("rootfs");
    std::fs::create_dir_all(&rootfs).unwrap();
    let host_secret = tmp.path().join("host-secret.txt");
    std::fs::write(&host_secret, "must not be logged\n").unwrap();
    match std::os::windows::fs::symlink_file(&host_secret, rootfs.join(WINDOWS_GUEST_STDOUT)) {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == Some(1314) => return,
        Err(error) => panic!("failed to create stream symlink: {error}"),
    }
    std::fs::write(rootfs.join(WINDOWS_GUEST_STDERR), "").unwrap();
    std::fs::write(rootfs.join(WINDOWS_GUEST_EXIT_CODE), "0\n").unwrap();

    let config = a3s_box_core::log::LogConfig::default();
    let error = collect_windows_guest_result(&box_dir, &config, 0)
        .unwrap_err()
        .to_string();

    assert!(error.contains("Failed to collect Windows guest output"));
    let console = box_dir.join("logs").join("console.log");
    assert!(
        !console.exists()
            || !std::fs::read_to_string(console)
                .unwrap()
                .contains("must not")
    );
}

#[cfg(target_os = "windows")]
#[test]
fn test_collect_windows_guest_result_rejects_false_success() {
    let tmp = tempfile::tempdir().unwrap();
    let box_dir = tmp.path().join("box");
    std::fs::create_dir_all(box_dir.join("rootfs")).unwrap();
    let config = a3s_box_core::log::LogConfig::default();

    let error = collect_windows_guest_result(&box_dir, &config, 0)
        .unwrap_err()
        .to_string();
    assert!(error.contains("before the guest persisted its exit code"));
    assert_eq!(
        collect_windows_guest_result(&box_dir, &config, 9).unwrap(),
        9
    );
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn test_windows_exit_file_waits_for_shim_log_relay() {
    let tmp = tempfile::tempdir().unwrap();
    let box_id = "box-windows-pending-relay".to_string();
    let mut vm =
        VmManager::with_box_id(BoxConfig::default(), EventEmitter::new(16), box_id.clone());
    vm.home_dir = tmp.path().to_path_buf();
    *vm.handler.write().await = Some(Box::new(RecordingHandler {
        stopped: Arc::new(AtomicBool::new(false)),
    }));

    let exit_path = tmp
        .path()
        .join("boxes")
        .join(&box_id)
        .join("rootfs")
        .join(".a3s_exit_code");
    std::fs::create_dir_all(exit_path.parent().unwrap()).unwrap();
    std::fs::write(exit_path, "0\n").unwrap();

    assert_eq!(vm.try_wait_exit().await.unwrap(), None);
    assert_eq!(vm.exit_code(), None);
    assert!(!vm.has_exited().await);
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn test_wait_for_exec_ready_classifies_persisted_windows_exit() {
    let tmp = tempfile::tempdir().unwrap();
    let box_id = "box-windows-completed-before-ready".to_string();
    let mut vm =
        VmManager::with_box_id(BoxConfig::default(), EventEmitter::new(16), box_id.clone());
    vm.home_dir = tmp.path().to_path_buf();
    *vm.handler.write().await = Some(Box::new(RecordingHandler {
        stopped: Arc::new(AtomicBool::new(false)),
    }));

    let rootfs = tmp.path().join("boxes").join(&box_id).join("rootfs");
    std::fs::create_dir_all(&rootfs).unwrap();
    std::fs::write(rootfs.join(WINDOWS_GUEST_EXIT_CODE), "42\n").unwrap();

    let error = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        vm.wait_for_exec_ready(&tmp.path().join("missing-exec.sock")),
    )
    .await
    .unwrap()
    .unwrap_err()
    .to_string();

    assert!(error.contains("completed with exit code 42"));
    assert_eq!(vm.exit_code(), None, "log relay has not completed yet");
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn test_boot_cleanup_collects_windows_guest_completed_before_readiness() {
    let tmp = tempfile::tempdir().unwrap();
    let box_id = "box-windows-cleanup-completed-before-ready".to_string();
    let mut vm = VmManager::with_box_id(
        BoxConfig {
            persistent: true,
            ..BoxConfig::default()
        },
        EventEmitter::new(16),
        box_id.clone(),
    );
    vm.home_dir = tmp.path().to_path_buf();
    vm.set_rootfs_provider(Box::new(crate::rootfs::CopyProvider));
    let stopped = Arc::new(AtomicBool::new(false));
    *vm.handler.write().await = Some(Box::new(RecordingHandler {
        stopped: Arc::clone(&stopped),
    }));

    let box_dir = tmp.path().join("boxes").join(&box_id);
    let rootfs = box_dir.join("rootfs");
    std::fs::create_dir_all(&rootfs).unwrap();
    std::fs::write(rootfs.join(WINDOWS_GUEST_STDOUT), "guest output\n").unwrap();
    std::fs::write(rootfs.join(WINDOWS_GUEST_STDERR), "guest failure\n").unwrap();
    std::fs::write(rootfs.join(WINDOWS_GUEST_EXIT_CODE), "42\n").unwrap();

    vm.cleanup_boot_failure().await;

    assert!(stopped.load(Ordering::SeqCst));
    assert_eq!(vm.exit_code(), Some(42));
    assert!(vm.preserve_rootfs_on_boot_failure);
    assert_eq!(
        std::fs::read_to_string(box_dir.join("logs").join("console.log")).unwrap(),
        "guest output\n"
    );
    assert!(rootfs.is_dir());
}

#[cfg(unix)]
#[tokio::test]
async fn test_wait_for_exec_ready_returns_when_handler_already_exited() {
    let mut vm = VmManager::with_box_id(
        BoxConfig::default(),
        EventEmitter::new(16),
        "box-exec-exited".to_string(),
    );
    *vm.handler.write().await = Some(Box::new(ExitStateHandler { exited: true }));
    let tmp = tempfile::tempdir().unwrap();

    vm.wait_for_exec_ready(&tmp.path().join("missing-exec.sock"))
        .await
        .unwrap();

    assert!(vm.exec_client.is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn test_wait_for_exec_ready_returns_when_guest_exit_code_persisted() {
    let tmp = tempfile::tempdir().unwrap();
    let box_id = "box-exec-finished".to_string();
    let mut vm =
        VmManager::with_box_id(BoxConfig::default(), EventEmitter::new(16), box_id.clone());
    vm.home_dir = tmp.path().to_path_buf();

    let exit_path = tmp
        .path()
        .join("boxes")
        .join(&box_id)
        .join("upper")
        .join(".a3s_exit_code");
    std::fs::create_dir_all(exit_path.parent().unwrap()).unwrap();
    std::fs::write(&exit_path, "17\n").unwrap();

    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        vm.wait_for_exec_ready(&tmp.path().join("missing-exec.sock")),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(vm.exit_code(), Some(17));
    assert!(vm.exec_client.is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn test_probe_exec_ready_once_ignores_missing_socket() {
    let mut vm = VmManager::with_box_id(
        BoxConfig::default(),
        EventEmitter::new(16),
        "box-probe".to_string(),
    );
    let tmp = tempfile::tempdir().unwrap();

    vm.probe_exec_ready_once(&tmp.path().join("missing-exec.sock"))
        .await;

    assert!(vm.exec_client.is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn test_attach_running_process_infers_port_forward_socket_path() {
    let mut vm = VmManager::with_box_id(
        BoxConfig::default(),
        EventEmitter::new(16),
        "box-test".to_string(),
    );
    let tmp = tempfile::tempdir().unwrap();
    let exec_socket_path = tmp.path().join("exec.sock");
    let pty_socket_path = Some(tmp.path().join("pty.sock"));

    vm.attach_running_process(
        std::process::id(),
        exec_socket_path.clone(),
        pty_socket_path.clone(),
    )
    .await
    .unwrap();

    assert_eq!(vm.exec_socket_path(), Some(exec_socket_path.as_path()));
    assert_eq!(vm.pty_socket_path(), pty_socket_path.as_deref());
    assert_eq!(
        vm.port_forward_socket_path(),
        Some(exec_socket_path.with_file_name("portfwd.sock").as_path())
    );
    assert_eq!(vm.state().await, BoxState::Ready);
}
