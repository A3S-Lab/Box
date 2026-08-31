use super::*;

#[tokio::test]
async fn test_destroy_runs_host_teardown_even_when_handler_stop_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let box_id = "box-stopfail".to_string();
    // persistent=false (the default) → the box dir is removed on destroy.
    let mut vm =
        VmManager::with_box_id(BoxConfig::default(), EventEmitter::new(16), box_id.clone());
    vm.home_dir = tmp.path().to_path_buf();
    *vm.handler.write().await = Some(Box::new(FailingHandler));

    let box_dir = tmp.path().join("boxes").join(&box_id);
    std::fs::create_dir_all(box_dir.join("logs")).unwrap();

    let result = vm.destroy_with_options(default_stop_signal(), 100).await;

    // The stop failure is still surfaced to the caller...
    assert!(
        result.is_err(),
        "a handler-stop failure must still be reported"
    );
    // ...but the host teardown ran anyway: handler taken + box dir removed.
    assert!(vm.handler.read().await.is_none());
    assert!(
        !box_dir.exists(),
        "non-persistent box dir must be removed even when the stop failed"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn destroy_uses_guest_stop_and_verifies_raw_rootfs_handoff() {
    use a3s_box_core::guest_exec::{GuestTerminalStatus, GUEST_TERMINAL_STATUS_FILE_NAME};

    let tmp = tempfile::tempdir().unwrap();
    let box_id = "box-guest-stop".to_string();
    let config = BoxConfig {
        persistent: true,
        ..BoxConfig::default()
    };
    let mut vm = VmManager::with_box_id(config, EventEmitter::new(16), box_id.clone());
    vm.home_dir = tmp.path().to_path_buf();
    vm.set_rootfs_provider(Box::new(crate::rootfs::CopyProvider));

    let box_dir = tmp.path().join("boxes").join(&box_id);
    std::fs::create_dir_all(box_dir.join("rootfs-ext4-v1")).unwrap();
    let terminal_path = box_dir
        .join("runtime-control")
        .join(GUEST_TERMINAL_STATUS_FILE_NAME);
    std::fs::create_dir_all(terminal_path.parent().unwrap()).unwrap();
    std::fs::write(&terminal_path, []).unwrap();

    let socket_dir = vm.socket_dir();
    std::fs::create_dir_all(&socket_dir).unwrap();
    let exec_socket = socket_dir.join("exec.sock");
    let listener = tokio::net::UnixListener::bind(&exec_socket).unwrap();
    vm.exec_socket_path = Some(exec_socket);

    let exited = Arc::new(AtomicBool::new(false));
    let backend_finalized = Arc::new(AtomicBool::new(false));
    *vm.handler.write().await = Some(Box::new(GuestStopHandler {
        exited: Arc::clone(&exited),
        backend_finalized: Arc::clone(&backend_finalized),
    }));

    let server_exited = Arc::clone(&exited);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, write) = tokio::io::split(stream);
        let mut reader = a3s_transport::FrameReader::new(read);
        let mut writer = a3s_transport::FrameWriter::new(write);
        let frame = reader.read_frame().await.unwrap().unwrap();
        assert_eq!(frame.frame_type, a3s_transport::FrameType::Control);
        assert_eq!(frame.payload, b"signal-main:15");

        let status = GuestTerminalStatus::new(143).with_rootfs_quiesced();
        std::fs::write(&terminal_path, serde_json::to_vec(&status).unwrap()).unwrap();
        server_exited.store(true, Ordering::SeqCst);
        writer.write_control(b"signal-main-ack").await.unwrap();
    });

    vm.destroy_with_options(libc::SIGTERM, 1_000).await.unwrap();
    server.await.unwrap();

    assert!(backend_finalized.load(Ordering::SeqCst));
    assert_eq!(vm.exit_code(), Some(143));
    assert!(box_dir.exists());
}

#[tokio::test]
async fn test_cleanup_boot_failure_stops_handler_and_removes_created_volumes() {
    let tmp = tempfile::tempdir().unwrap();
    let box_id = "box-test".to_string();
    let mut vm =
        VmManager::with_box_id(BoxConfig::default(), EventEmitter::new(16), box_id.clone());
    vm.home_dir = tmp.path().to_path_buf();
    vm.anonymous_volumes = vec!["created-volume".to_string(), "reused-volume".to_string()];
    vm.created_anonymous_volumes = vec!["created-volume".to_string()];

    let stopped = Arc::new(AtomicBool::new(false));
    *vm.handler.write().await = Some(Box::new(RecordingHandler {
        stopped: stopped.clone(),
    }));

    let box_dir = tmp.path().join("boxes").join(&box_id);
    std::fs::create_dir_all(box_dir.join("logs")).unwrap();

    let store = crate::volume::VolumeStore::new(
        tmp.path().join("volumes.json"),
        tmp.path().join("volumes"),
    );
    store
        .create(a3s_box_core::volume::VolumeConfig::new(
            "created-volume",
            "",
        ))
        .unwrap();
    store
        .create(a3s_box_core::volume::VolumeConfig::new("reused-volume", ""))
        .unwrap();

    vm.cleanup_boot_failure().await;

    assert!(stopped.load(Ordering::SeqCst));
    assert!(vm.handler.read().await.is_none());
    assert!(vm.created_anonymous_volumes.is_empty());
    assert_eq!(vm.anonymous_volumes, vec!["reused-volume".to_string()]);
    assert!(store.get("created-volume").unwrap().is_none());
    assert!(store.get("reused-volume").unwrap().is_some());
    assert!(!box_dir.exists());
}

#[tokio::test]
async fn test_cleanup_boot_failure_retains_an_exact_terminal_status() {
    let tmp = tempfile::tempdir().unwrap();
    let box_id = "box-completed-during-boot".to_string();
    let mut vm =
        VmManager::with_box_id(BoxConfig::default(), EventEmitter::new(16), box_id.clone());
    vm.home_dir = tmp.path().to_path_buf();
    *vm.handler.write().await = Some(Box::new(CompletedHandler { code: 23 }));

    let box_dir = tmp.path().join("boxes").join(&box_id);
    std::fs::create_dir_all(box_dir.join("logs")).unwrap();

    vm.cleanup_boot_failure().await;

    assert_eq!(vm.exit_code(), Some(23));
    assert!(vm.handler.read().await.is_none());
    assert!(!box_dir.exists());
}

#[tokio::test]
async fn test_cleanup_boot_completion_preserves_first_persistent_rootfs() {
    let tmp = tempfile::tempdir().unwrap();
    let box_id = "box-persistent-completed-during-boot".to_string();
    let config = BoxConfig {
        persistent: true,
        ..BoxConfig::default()
    };
    let mut vm = VmManager::with_box_id(config, EventEmitter::new(16), box_id.clone());
    vm.home_dir = tmp.path().to_path_buf();
    vm.set_rootfs_provider(Box::new(crate::rootfs::CopyProvider));
    *vm.handler.write().await = Some(Box::new(CompletionCollectedByStopHandler {
        code: 17,
        collected: false,
    }));

    let marker = tmp
        .path()
        .join("boxes")
        .join(&box_id)
        .join("rootfs/r17-restart-marker");
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(&marker, b"generation-one").unwrap();

    vm.cleanup_boot_failure().await;

    assert_eq!(vm.exit_code(), Some(17));
    assert_eq!(std::fs::read(&marker).unwrap(), b"generation-one");
    assert!(vm.preserve_rootfs_on_boot_failure);
}

#[tokio::test]
async fn test_cleanup_boot_failure_waits_for_delayed_terminal_status() {
    let tmp = tempfile::tempdir().unwrap();
    let box_id = "box-delayed-completion-during-boot".to_string();
    let config = BoxConfig {
        persistent: true,
        ..BoxConfig::default()
    };
    let mut vm = VmManager::with_box_id(config, EventEmitter::new(16), box_id.clone());
    vm.home_dir = tmp.path().to_path_buf();
    vm.set_rootfs_provider(Box::new(crate::rootfs::CopyProvider));
    let polls = Arc::new(AtomicUsize::new(0));
    *vm.handler.write().await = Some(Box::new(DelayedCompletionHandler {
        polls: Arc::clone(&polls),
        available_after: 3,
    }));

    let marker = tmp
        .path()
        .join("boxes")
        .join(&box_id)
        .join("rootfs/r17-delayed-exit-marker");
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(&marker, b"completed").unwrap();

    vm.cleanup_boot_failure().await;

    assert_eq!(vm.exit_code(), Some(0));
    assert_eq!(polls.load(Ordering::SeqCst), 4);
    assert_eq!(std::fs::read(&marker).unwrap(), b"completed");
    assert!(vm.preserve_rootfs_on_boot_failure);
}

#[tokio::test]
async fn test_cleanup_boot_failure_preserves_persistent_rootfs() {
    let tmp = tempfile::tempdir().unwrap();
    let box_id = "box-persistent-boot-failure".to_string();
    let config = BoxConfig {
        persistent: true,
        ..BoxConfig::default()
    };
    let mut vm = VmManager::with_box_id(config, EventEmitter::new(16), box_id.clone());
    vm.home_dir = tmp.path().to_path_buf();
    vm.set_rootfs_provider(Box::new(crate::rootfs::CopyProvider));

    let box_dir = tmp.path().join("boxes").join(&box_id);
    let sentinel = box_dir.join("rootfs/var/lib/application/data.db");
    std::fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
    std::fs::write(&sentinel, b"persistent guest data").unwrap();
    std::fs::create_dir_all(vm.socket_dir()).unwrap();
    std::fs::write(vm.socket_dir().join("stale.sock"), b"stale").unwrap();
    vm.preserve_rootfs_on_boot_failure = true;

    vm.cleanup_boot_failure().await;

    assert_eq!(
        std::fs::read(&sentinel).unwrap(),
        b"persistent guest data",
        "a failed restart must not erase the persistent writable rootfs"
    );
    assert!(box_dir.exists());
    assert!(
        !vm.socket_dir().exists(),
        "transient sockets should still be removed after a failed restart"
    );
}

#[tokio::test]
async fn test_cleanup_boot_failure_discards_partial_first_persistent_rootfs() {
    let tmp = tempfile::tempdir().unwrap();
    let box_id = "box-partial-first-boot".to_string();
    let config = BoxConfig {
        persistent: true,
        ..BoxConfig::default()
    };
    let mut vm = VmManager::with_box_id(config, EventEmitter::new(16), box_id.clone());
    vm.home_dir = tmp.path().to_path_buf();
    vm.set_rootfs_provider(Box::new(crate::rootfs::CopyProvider));

    let box_dir = tmp.path().join("boxes").join(&box_id);
    let partial = box_dir.join("rootfs/partially-extracted");
    std::fs::create_dir_all(partial.parent().unwrap()).unwrap();
    std::fs::write(&partial, b"incomplete").unwrap();

    vm.cleanup_boot_failure().await;

    assert!(box_dir.exists(), "a retained box keeps its host directory");
    assert!(
        !box_dir.join("rootfs").exists(),
        "a failed first boot must discard its incomplete rootfs generation"
    );
}

#[tokio::test]
async fn test_wait_for_vm_running_returns_error_when_handler_exited() {
    let vm = VmManager::with_box_id(
        BoxConfig::default(),
        EventEmitter::new(16),
        "box-exited".to_string(),
    );
    *vm.handler.write().await = Some(Box::new(ExitStateHandler { exited: true }));

    let err = vm.wait_for_vm_running().await.unwrap_err();

    assert!(err
        .to_string()
        .contains("VM process exited immediately after start"));
}

#[tokio::test]
async fn test_wait_for_vm_running_succeeds_when_handler_stays_running() {
    let config = BoxConfig {
        restore_from: Some("snapshot-path".to_string()),
        ..BoxConfig::default()
    };
    let vm = VmManager::with_box_id(config, EventEmitter::new(16), "box-running".to_string());
    *vm.handler.write().await = Some(Box::new(ExitStateHandler { exited: false }));

    vm.wait_for_vm_running().await.unwrap();
}

#[cfg(not(target_os = "windows"))]
#[tokio::test]
async fn test_try_wait_exit_reads_guest_persisted_exit_code() {
    let tmp = tempfile::tempdir().unwrap();
    let box_id = "box-exit-code".to_string();
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
    std::fs::write(&exit_path, "42\n").unwrap();

    assert_eq!(vm.try_wait_exit().await.unwrap(), Some(42));
    assert_eq!(vm.exit_code(), Some(42));
    assert!(vm.has_exited().await);
}

#[cfg(not(target_os = "windows"))]
#[tokio::test]
async fn test_try_wait_exit_rereads_guest_exit_code_after_provider_completion() {
    let tmp = tempfile::tempdir().unwrap();
    let box_id = "box-exit-code-during-completion".to_string();
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
    *vm.handler.write().await = Some(Box::new(PersistedExitOnCompletionHandler {
        provider_code: 1,
        persisted_code: 0,
        exit_path,
    }));

    assert_eq!(vm.try_wait_exit().await.unwrap(), Some(0));
    assert_eq!(vm.exit_code(), Some(0));
    assert!(vm.has_exited().await);
}

#[cfg(not(target_os = "windows"))]
#[tokio::test]
async fn test_boot_cleanup_prefers_guest_exit_written_during_provider_completion() {
    let tmp = tempfile::tempdir().unwrap();
    let box_id = "box-boot-cleanup-exit-code".to_string();
    let mut vm = VmManager::with_box_id(
        BoxConfig {
            persistent: true,
            ..BoxConfig::default()
        },
        EventEmitter::new(16),
        box_id.clone(),
    );
    vm.home_dir = tmp.path().to_path_buf();

    let exit_path = tmp
        .path()
        .join("boxes")
        .join(&box_id)
        .join("upper")
        .join(".a3s_exit_code");
    std::fs::create_dir_all(exit_path.parent().unwrap()).unwrap();
    *vm.handler.write().await = Some(Box::new(PersistedExitOnCompletionHandler {
        provider_code: 1,
        persisted_code: 0,
        exit_path,
    }));

    vm.cleanup_boot_failure().await;

    assert_eq!(vm.exit_code(), Some(0));
    assert!(vm.preserve_rootfs_on_boot_failure);
}

#[cfg(not(target_os = "windows"))]
#[tokio::test]
async fn test_guest_exit_code_waits_for_runtime_log_drain() {
    let tmp = tempfile::tempdir().unwrap();
    let box_id = "box-pending-log-drain".to_string();
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
        .join("upper")
        .join(".a3s_exit_code");
    std::fs::create_dir_all(exit_path.parent().unwrap()).unwrap();
    std::fs::write(exit_path, "7\n").unwrap();

    assert_eq!(vm.try_wait_exit().await.unwrap(), None);
    assert_eq!(vm.exit_code(), None);
    assert!(!vm.has_exited().await);
}

#[tokio::test]
async fn test_try_wait_exit_reads_windows_rootfs_exit_code() {
    let tmp = tempfile::tempdir().unwrap();
    let box_id = "box-windows-exit-code".to_string();
    let mut vm =
        VmManager::with_box_id(BoxConfig::default(), EventEmitter::new(16), box_id.clone());
    vm.home_dir = tmp.path().to_path_buf();
    *vm.handler.write().await = Some(Box::new(CompletedHandler { code: 23 }));

    let exit_path = tmp
        .path()
        .join("boxes")
        .join(&box_id)
        .join("rootfs")
        .join(".a3s_exit_code");
    std::fs::create_dir_all(exit_path.parent().unwrap()).unwrap();
    std::fs::write(&exit_path, "23\n").unwrap();
    #[cfg(target_os = "windows")]
    {
        std::fs::write(
            exit_path.parent().unwrap().join(WINDOWS_GUEST_STDOUT),
            "guest stdout\n",
        )
        .unwrap();
        std::fs::write(
            exit_path.parent().unwrap().join(WINDOWS_GUEST_STDERR),
            concat!(
                "init.krun: mount_filesystems ok\n",
                "init.krun: execvp(/bin/app) starting\n",
                "guest stderr\n",
            ),
        )
        .unwrap();
    }

    assert_eq!(vm.try_wait_exit().await.unwrap(), Some(23));
    assert_eq!(vm.exit_code(), Some(23));
    assert!(vm.has_exited().await);
    #[cfg(target_os = "windows")]
    {
        let logs = tmp.path().join("boxes").join(&box_id).join("logs");
        assert_eq!(
            std::fs::read_to_string(logs.join("console.log")).unwrap(),
            "guest stdout\n"
        );
        assert_eq!(
            std::fs::read_to_string(logs.join("console.err.log")).unwrap(),
            "guest stderr\n"
        );
        let json = std::fs::read_to_string(logs.join("container.json")).unwrap();
        assert!(json.contains("\"log\":\"guest stdout\\n\""));
        assert!(json.contains("\"log\":\"guest stderr\\n\""));
        assert!(!json.contains("init.krun"));
    }
}
