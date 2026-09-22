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
    let guard = a3s_box_core::windows_symlink::WindowsSymlinkPrivilegeGuard::acquire();
    let assigned_privilege_enabled = guard.assigned_privilege_enabled();
    match std::os::windows::fs::symlink_file(&host_target, &marker) {
        Ok(()) => {}
        Err(error)
            if a3s_box_core::windows_symlink::is_capability_denial(
                &error,
                assigned_privilege_enabled,
            ) =>
        {
            return
        }
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
    let guard = a3s_box_core::windows_symlink::WindowsSymlinkPrivilegeGuard::acquire();
    let assigned_privilege_enabled = guard.assigned_privilege_enabled();
    match std::os::windows::fs::symlink_file(&host_secret, rootfs.join(WINDOWS_GUEST_STDOUT)) {
        Ok(()) => {}
        Err(error)
            if a3s_box_core::windows_symlink::is_capability_denial(
                &error,
                assigned_privilege_enabled,
            ) =>
        {
            return
        }
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
async fn test_wait_for_exec_ready_rejects_provider_exit_without_guest_status() {
    // Provider/shim exit alone must not invent exec-ready success — Unix
    // already fails closed; Windows historically returned Ok(()) on has_exited.
    let tmp = tempfile::tempdir().unwrap();
    let box_id = "box-windows-provider-exit-before-ready".to_string();
    let mut vm =
        VmManager::with_box_id(BoxConfig::default(), EventEmitter::new(16), box_id.clone());
    vm.home_dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(tmp.path().join("boxes").join(&box_id).join("rootfs")).unwrap();
    *vm.handler.write().await = Some(Box::new(ExitStateHandler { exited: true }));

    let error = vm
        .wait_for_exec_ready(&tmp.path().join("missing-exec.sock"))
        .await
        .expect_err("provider exit without durable guest status must not invent ready")
        .to_string();

    assert!(
        error.contains("before the guest exec server became ready")
            || error.contains("before the exec server became ready"),
        "{error}"
    );
    assert_eq!(
        vm.exit_code(),
        None,
        "must not invent shim_exit_code from bare provider exit"
    );
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn test_wait_for_exec_ready_rejects_collected_provider_exit_without_guest_file() {
    // Nonzero provider reap authenticates collect_windows_guest_result without a
    // guest exit file — still must not invent Ok(()) for exec readiness.
    let tmp = tempfile::tempdir().unwrap();
    let box_id = "box-windows-collected-provider-before-ready".to_string();
    let mut vm =
        VmManager::with_box_id(BoxConfig::default(), EventEmitter::new(16), box_id.clone());
    vm.home_dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(tmp.path().join("boxes").join(&box_id).join("rootfs")).unwrap();
    *vm.handler.write().await = Some(Box::new(CompletedHandler { code: 1 }));

    let error = vm
        .wait_for_exec_ready(&tmp.path().join("missing-exec.sock"))
        .await
        .expect_err("collected provider exit must classify as boot failure, not ready")
        .to_string();

    assert!(
        error.contains("exit code 1") || error.contains("before"),
        "{error}"
    );
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
async fn test_wait_for_exec_ready_rejects_provider_exit_without_guest_status() {
    let mut vm = VmManager::with_box_id(
        BoxConfig::default(),
        EventEmitter::new(16),
        "box-exec-exited".to_string(),
    );
    *vm.handler.write().await = Some(Box::new(CompletedHandler { code: 1 }));
    let tmp = tempfile::tempdir().unwrap();
    vm.home_dir = tmp.path().to_path_buf();

    let error = vm
        .wait_for_exec_ready(&tmp.path().join("missing-exec.sock"))
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("exited with code 1"), "{error}");
    assert!(vm.exec_client.is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn test_wait_for_exec_ready_polls_for_delayed_guest_exit_before_heartbeat() {
    // #576: short tasks can publish the host-backed exit a few polls after the
    // shim looks exited. Readiness must wait within the terminal bound and keep
    // the authenticated code — never invent Ready from that exit alone.
    let tmp = tempfile::tempdir().unwrap();
    let box_id = "box-exec-delayed-guest-exit".to_string();
    let mut vm =
        VmManager::with_box_id(BoxConfig::default(), EventEmitter::new(16), box_id.clone());
    vm.home_dir = tmp.path().to_path_buf();
    let polls = Arc::new(AtomicUsize::new(0));
    let durable_exit_path = tmp
        .path()
        .join("boxes")
        .join(&box_id)
        .join("rootfs/.a3s_exit_code");
    *vm.handler.write().await = Some(Box::new(DelayedCompletionHandler {
        polls: Arc::clone(&polls),
        available_after: 2,
        durable_exit_path: Some(durable_exit_path),
    }));

    let error = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        vm.wait_for_exec_ready(&tmp.path().join("missing-exec.sock")),
    )
    .await
    .expect("delayed guest exit must resolve inside the terminal poll bound")
    .expect_err("delayed guest exit must not invent Ready")
    .to_string();

    assert!(
        error.contains("exited with code 0") || error.contains("before the guest exec server"),
        "{error}"
    );
    assert_eq!(vm.exit_code(), Some(0));
    assert!(polls.load(Ordering::SeqCst) >= 3);
    assert!(vm.exec_client.is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn test_wait_for_exec_ready_refuses_provider_zero_when_guest_exit_never_arrives() {
    let tmp = tempfile::tempdir().unwrap();
    let box_id = "box-exec-provider-zero-no-guest".to_string();
    let mut vm =
        VmManager::with_box_id(BoxConfig::default(), EventEmitter::new(16), box_id.clone());
    vm.home_dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(tmp.path().join("boxes").join(&box_id).join("logs")).unwrap();
    *vm.handler.write().await = Some(Box::new(CompletedHandler { code: 0 }));

    let error = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        vm.wait_for_exec_ready(&tmp.path().join("missing-exec.sock")),
    )
    .await
    .expect("provider-zero poll must finish within the terminal bound")
    .expect_err("provider zero without guest proof must fail closed")
    .to_string();

    assert!(
        error.contains("before the guest exec server became ready"),
        "{error}"
    );
    assert_eq!(
        vm.exit_code(),
        None,
        "must not invent shim_exit_code from bare provider zero"
    );
    assert!(vm.exec_client.is_none());
}

#[cfg(windows)]
#[tokio::test]
async fn test_wait_for_exec_available_fails_closed_without_heartbeat() {
    let tmp = tempfile::tempdir().unwrap();
    let box_id = "box-pool-no-exec".to_string();
    let mut vm =
        VmManager::with_box_id(BoxConfig::default(), EventEmitter::new(16), box_id.clone());
    vm.home_dir = tmp.path().to_path_buf();
    let layout = tmp
        .path()
        .join("boxes")
        .join(&box_id)
        .join("sockets")
        .join("exec.sock");
    std::fs::create_dir_all(layout.parent().unwrap()).unwrap();
    vm.exec_socket_path = Some(layout);

    let error = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        vm.wait_for_exec_available(std::time::Duration::from_millis(400)),
    )
    .await
    .unwrap()
    .expect_err("layout path alone must not invent pool exec availability")
    .to_string();

    assert!(
        error.contains("did not become ready") || error.contains("heartbeat"),
        "{error}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn test_wait_for_exec_ready_fails_closed_when_guest_exit_persisted_before_heartbeat() {
    // Durable guest exit before exec heartbeat must not invent Ready / Ok(())
    // — Windows #407/#408 fail-closed parity. Boot cleanup collects the exit.
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

    let error = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        vm.wait_for_exec_ready(&tmp.path().join("missing-exec.sock")),
    )
    .await
    .unwrap()
    .expect_err("guest exit before exec ready must not invent Ready")
    .to_string();

    assert!(
        error.contains("exited with code 17") || error.contains("before the guest exec server"),
        "{error}"
    );
    assert_eq!(vm.exit_code(), Some(17));
    assert!(vm.exec_client.is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn test_wait_for_exec_ready_fails_closed_when_cap_elapses_without_heartbeat() {
    std::env::set_var("A3S_EXEC_READY_TIMEOUT_MS", "400");

    let tmp = tempfile::tempdir().unwrap();
    let box_id = "box-exec-cap".to_string();
    let mut vm =
        VmManager::with_box_id(BoxConfig::default(), EventEmitter::new(16), box_id.clone());
    vm.home_dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(tmp.path().join("boxes").join(&box_id).join("logs")).unwrap();
    *vm.handler.write().await = Some(Box::new(ExitStateHandler { exited: false }));

    let started = std::time::Instant::now();
    let error = vm
        .wait_for_exec_ready(&tmp.path().join("missing-exec.sock"))
        .await
        .expect_err("wedged guest without heartbeat must fail closed");
    std::env::remove_var("A3S_EXEC_READY_TIMEOUT_MS");

    assert!(
        error
            .to_string()
            .contains("Guest exec server did not become ready"),
        "{error}"
    );
    assert!(vm.exec_client.is_none());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "fail-closed must not wait out the historical 15s soft-proceed window"
    );
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
async fn test_restore_boot_completion_refuses_ready_without_exec_heartbeat() {
    let mut vm = VmManager::with_box_id(
        BoxConfig::default(),
        EventEmitter::new(16),
        "box-restore-no-hb".to_string(),
    );
    let tmp = tempfile::tempdir().unwrap();

    // Mirror restore soft-proceed: one best-effort probe, then the boot
    // completion gate — never invent Ready when heartbeat did not authenticate.
    vm.probe_exec_ready_once(&tmp.path().join("missing-exec.sock"))
        .await;
    assert!(
        vm.exec_client().is_none(),
        "failed restore probe must not invent an exec client"
    );

    let ready = vm.set_boot_completion_state().await;
    assert!(
        !ready,
        "restore boot completion must not authorize Ready without heartbeat"
    );
    assert_eq!(
        vm.state().await,
        BoxState::Created,
        "restore soft-proceed must leave Created for observe/promote_if_ready"
    );
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
    // Missing exec heartbeat must not invent Ready — leave Created for
    // promote_if_ready / observe to authenticate later.
    assert_eq!(vm.state().await, BoxState::Created);
}

#[cfg(unix)]
#[tokio::test]
async fn test_attach_running_process_refuses_ready_without_exec_heartbeat() {
    let mut vm = VmManager::with_box_id(
        BoxConfig::default(),
        EventEmitter::new(16),
        "box-attach-no-hb".to_string(),
    );
    let tmp = tempfile::tempdir().unwrap();
    let exec_socket_path = tmp.path().join("exec.sock");

    vm.attach_running_process(
        std::process::id(),
        exec_socket_path,
        Some(tmp.path().join("pty.sock")),
    )
    .await
    .unwrap();

    assert!(
        vm.exec_client().is_none(),
        "attach without a live exec endpoint must not invent a client"
    );
    assert_eq!(
        vm.state().await,
        BoxState::Created,
        "attach must not invent Ready without authenticated exec heartbeat"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn test_promote_ready_if_exec_authenticated_refuses_without_heartbeat() {
    let mut vm = VmManager::with_box_id(
        BoxConfig::default(),
        EventEmitter::new(16),
        "box-sandbox-promote-no-hb".to_string(),
    );
    let tmp = tempfile::tempdir().unwrap();
    vm.exec_socket_path = Some(tmp.path().join("missing-exec.sock"));

    let ready = vm.promote_ready_if_exec_authenticated().await;
    assert!(
        !ready,
        "Sandbox recover promote must not invent Ready without exec heartbeat"
    );
    assert!(
        vm.exec_client().is_none(),
        "failed promote must not invent an exec client"
    );
    assert_eq!(
        vm.state().await,
        BoxState::Created,
        "Sandbox recover must leave Created for observe/promote_if_ready"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn test_health_check_refuses_ready_sustained_by_pid_without_exec_heartbeat() {
    struct LivePidHandler;

    impl crate::vmm::VmHandler for LivePidHandler {
        fn stop(&mut self, _signal: i32, _timeout_ms: u64) -> Result<()> {
            Ok(())
        }

        fn metrics(&self) -> crate::vmm::VmMetrics {
            crate::vmm::VmMetrics::default()
        }

        fn is_running(&self) -> bool {
            true
        }

        fn has_exited(&self) -> bool {
            false
        }

        fn pid(&self) -> u32 {
            42
        }

        fn exit_code(&self) -> Option<i32> {
            None
        }

        fn try_wait_exit(&mut self) -> Result<Option<i32>> {
            Ok(None)
        }
    }

    let vm = VmManager::with_box_id(
        BoxConfig::default(),
        EventEmitter::new(16),
        "box-health-pid-only".to_string(),
    );
    *vm.state.write().await = BoxState::Ready;
    *vm.handler.write().await = Some(Box::new(LivePidHandler));
    // No exec_client and no exec_socket_path — PID alone must not sustain Ready.

    let healthy = vm
        .health_check()
        .await
        .expect("health_check should not error");
    assert!(
        !healthy,
        "Ready + live PID without exec heartbeat must not invent healthy/Running"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn test_health_check_refuses_ready_sustained_by_pid_without_exec_heartbeat() {
    struct LivePidHandler;

    impl crate::vmm::VmHandler for LivePidHandler {
        fn stop(&mut self, _signal: i32, _timeout_ms: u64) -> Result<()> {
            Ok(())
        }

        fn metrics(&self) -> crate::vmm::VmMetrics {
            crate::vmm::VmMetrics::default()
        }

        fn is_running(&self) -> bool {
            true
        }

        fn has_exited(&self) -> bool {
            false
        }

        fn pid(&self) -> u32 {
            42
        }

        fn exit_code(&self) -> Option<i32> {
            None
        }

        fn try_wait_exit(&mut self) -> Result<Option<i32>> {
            Ok(None)
        }
    }

    let vm = VmManager::with_box_id(
        BoxConfig::default(),
        EventEmitter::new(16),
        "box-health-pid-only-win".to_string(),
    );
    *vm.state.write().await = BoxState::Ready;
    *vm.handler.write().await = Some(Box::new(LivePidHandler));
    // No exec_client, no guest-control.ready, no live named pipe — PID alone
    // must not sustain Ready / invent healthy Running (#420 / #419 parity).

    let healthy = vm
        .health_check()
        .await
        .expect("health_check should not error");
    assert!(
        !healthy,
        "Ready + live PID without named-pipe heartbeat must not invent healthy/Running"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn test_attach_running_process_refuses_ready_without_exec_heartbeat() {
    let mut vm = VmManager::with_box_id(
        BoxConfig::default(),
        EventEmitter::new(16),
        "box-attach-no-hb".to_string(),
    );
    let tmp = tempfile::tempdir().unwrap();
    let exec_socket_path = tmp.path().join("exec.sock");

    vm.attach_running_process(
        std::process::id(),
        exec_socket_path,
        Some(tmp.path().join("pty.sock")),
    )
    .await
    .unwrap();

    assert_eq!(
        vm.state().await,
        BoxState::Created,
        "Windows attach must not invent Ready from shim PID + layout path alone"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn microvm_pause_demotes_ready_when_exec_frozen() {
    use std::sync::atomic::{AtomicBool, Ordering};

    struct SleepHandler {
        pid: u32,
        stopped: Arc<AtomicBool>,
    }

    impl crate::vmm::VmHandler for SleepHandler {
        fn stop(&mut self, _: i32, _: u64) -> a3s_box_core::error::Result<()> {
            self.stopped.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn metrics(&self) -> crate::vmm::VmMetrics {
            crate::vmm::VmMetrics::default()
        }
        fn is_running(&self) -> bool {
            !self.stopped.load(Ordering::SeqCst)
        }
        fn has_exited(&self) -> bool {
            self.stopped.load(Ordering::SeqCst)
        }
        fn pid(&self) -> u32 {
            self.pid
        }
    }

    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("spawn sleep for SIGSTOP fixture");
    let pid = child.id();

    let vm = VmManager::with_box_id(
        BoxConfig::default(),
        EventEmitter::new(16),
        "box-pause-demote".to_string(),
    );
    *vm.state.write().await = BoxState::Ready;
    *vm.handler.write().await = Some(Box::new(SleepHandler {
        pid,
        stopped: Arc::new(AtomicBool::new(false)),
    }));

    vm.pause().await.expect("SIGSTOP pause");
    assert_eq!(
        vm.state().await,
        BoxState::Paused,
        "pause must demote Ready so frozen exec is not inventable"
    );
    assert!(
        !vm.health_check().await.expect("health"),
        "Paused must fail health_check"
    );

    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
#[tokio::test]
async fn microvm_resume_refuses_ready_without_post_cont_heartbeat() {
    use std::sync::atomic::{AtomicBool, Ordering};

    struct SleepHandler {
        pid: u32,
        stopped: Arc<AtomicBool>,
    }

    impl crate::vmm::VmHandler for SleepHandler {
        fn stop(&mut self, _: i32, _: u64) -> a3s_box_core::error::Result<()> {
            self.stopped.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn metrics(&self) -> crate::vmm::VmMetrics {
            crate::vmm::VmMetrics::default()
        }
        fn is_running(&self) -> bool {
            !self.stopped.load(Ordering::SeqCst)
        }
        fn has_exited(&self) -> bool {
            self.stopped.load(Ordering::SeqCst)
        }
        fn pid(&self) -> u32 {
            self.pid
        }
    }

    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("spawn sleep for SIGCONT fixture");
    let pid = child.id();

    let mut vm = VmManager::with_box_id(
        BoxConfig::default(),
        EventEmitter::new(16),
        "box-resume-no-hb".to_string(),
    );
    *vm.state.write().await = BoxState::Paused;
    *vm.handler.write().await = Some(Box::new(SleepHandler {
        pid,
        stopped: Arc::new(AtomicBool::new(false)),
    }));
    // No exec socket — post-CONT re-auth must fail closed.
    vm.exec_socket_path = None;

    let error = vm
        .resume()
        .await
        .expect_err("SIGCONT alone must not invent Ready");
    assert!(
        error.to_string().contains("re-authenticate") || error.to_string().contains("Paused"),
        "expected fail-closed resume, got {error}"
    );
    assert_eq!(
        vm.state().await,
        BoxState::Paused,
        "failed re-auth must leave Paused, not invent Ready"
    );

    let _ = child.kill();
    let _ = child.wait();
}
