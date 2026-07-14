    #[test]
    fn lists_boxes_from_state_without_spawning_cli() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        write_boxes(
            &client,
            &[
                box_record("11111111-1111-4111-8111-111111111111", "web", "running"),
                box_record("22222222-2222-4222-8222-222222222222", "db", "stopped"),
            ],
        );

        let all = client.list_boxes(ListBoxesOptions::all()).unwrap();
        let active = client.list_boxes(ListBoxesOptions::active()).unwrap();

        assert_eq!(all.len(), 2);
        assert_eq!(active.len(), 1);
        assert_eq!(all[0].name, "web");
        assert_eq!(all[0].ports, vec!["8080:80"]);
        assert_eq!(client.get_box("db").unwrap().unwrap().status, "stopped");
    }

    #[test]
    fn old_state_defaults_to_microvm_and_sandbox_state_is_visible() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        let old_record = box_record("old-id", "old", "created");
        let mut old_json = serde_json::to_value(old_record).unwrap();
        old_json.as_object_mut().unwrap().remove("isolation");
        let mut sandbox_record = box_record("sandbox-id", "sandbox", "created");
        sandbox_record.isolation = a3s_box_core::ExecutionIsolation::Sandbox;
        std::fs::create_dir_all(&client.paths().home).unwrap();
        std::fs::write(
            &client.paths().boxes_file,
            serde_json::to_vec_pretty(&serde_json::json!([
                old_json,
                serde_json::to_value(sandbox_record).unwrap()
            ]))
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            client.get_box("old").unwrap().unwrap().isolation,
            a3s_box_core::ExecutionIsolation::Microvm
        );
        assert_eq!(
            client.get_box("sandbox").unwrap().unwrap().isolation,
            a3s_box_core::ExecutionIsolation::Sandbox
        );
    }

    #[test]
    fn collects_runtime_diagnostics_without_spawning_cli() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);

        let diagnostics = client.runtime_diagnostics();

        assert_eq!(diagnostics.home, dir.path());
        assert!(!diagnostics.core_version.is_empty());
        assert!(!diagnostics.runtime_version.is_empty());
        assert!(!diagnostics.sdk_version.is_empty());
        assert!(!diagnostics.virtualization.details.is_empty());
        if diagnostics.virtualization.available {
            assert!(diagnostics.virtualization.backend.is_some());
        } else {
            assert!(diagnostics.virtualization.backend.is_none());
        }
    }

    #[test]
    fn collects_runtime_disk_usage_without_spawning_cli() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);

        std::fs::create_dir_all(client.paths().home.join("boxes").join("box-1")).unwrap();
        std::fs::create_dir_all(client.paths().images_dir.join("sha256-test")).unwrap();
        std::fs::create_dir_all(client.paths().volumes_dir.join("data")).unwrap();
        std::fs::create_dir_all(client.paths().snapshots_dir.join("snap-1")).unwrap();
        std::fs::write(
            client
                .paths()
                .home
                .join("boxes")
                .join("box-1")
                .join("rootfs"),
            b"box",
        )
        .unwrap();
        std::fs::write(
            client.paths().images_dir.join("sha256-test").join("layer"),
            b"image",
        )
        .unwrap();
        std::fs::write(
            client.paths().volumes_dir.join("data").join("file"),
            b"volume",
        )
        .unwrap();
        std::fs::write(
            client.paths().snapshots_dir.join("snap-1").join("rootfs"),
            b"snapshot",
        )
        .unwrap();
        std::fs::write(&client.paths().boxes_file, b"[]").unwrap();
        std::fs::write(&client.paths().volumes_file, b"{}").unwrap();
        std::fs::write(&client.paths().networks_file, b"{}").unwrap();
        std::fs::write(client.paths().home.join("audit.log"), b"other").unwrap();

        let usage = client.runtime_disk_usage().unwrap();

        assert_eq!(usage.home, dir.path());
        assert_eq!(usage.boxes_bytes, 3);
        assert_eq!(usage.images_bytes, 5);
        assert_eq!(usage.volumes_bytes, 6);
        assert_eq!(usage.snapshots_bytes, 8);
        assert_eq!(usage.state_bytes, 6);
        assert_eq!(usage.other_bytes, 5);
        assert_eq!(usage.total_bytes, 33);
    }

    #[cfg(unix)]
    #[test]
    fn pauses_and_unpauses_box_with_host_signal_and_locked_state_update() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let mut record = box_record("12121212-1212-4121-8121-121212121212", "api", "running");
        record.pid = Some(pid);
        record.pid_start_time = pid_start_time(pid);
        record.virtiofs_cache = Some("always".to_string());
        write_boxes(&client, &[record]);

        let paused = client.pause_box("api").unwrap();
        assert_eq!(paused.status, "paused");
        assert_eq!(
            client.get_box("api").unwrap().unwrap().status,
            "paused",
            "pause should persist the status transition through the SDK state writer"
        );

        let running = client.unpause_box("api").unwrap();
        assert_eq!(running.status, "running");
        assert_eq!(client.get_box("api").unwrap().unwrap().status, "running");
        let persisted: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&client.paths().boxes_file).unwrap(),
        )
        .unwrap();
        assert_eq!(persisted[0]["virtiofs_cache"], "always");

        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn pause_rejects_stale_pid_identity_without_mutating_state() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        let mut record = box_record("13131313-1313-4131-8131-131313131313", "api", "running");
        record.pid = Some(std::process::id());
        record.pid_start_time = Some(u64::MAX);
        write_boxes(&client, &[record]);

        let error = client.pause_box("api").unwrap_err();
        assert!(format!("{error}").contains("recorded PID"));
        assert_eq!(client.get_box("api").unwrap().unwrap().status, "running");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_box_falls_back_to_host_signal_and_updates_state() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let mut record = box_record("14141414-1414-4141-8141-141414141414", "api", "running");
        record.pid = Some(pid);
        record.pid_start_time = pid_start_time(pid);
        record.box_dir = dir.path().join("boxes").join(&record.id);
        record.exec_socket_path = record.box_dir.join("sockets").join("missing.sock");
        record.volume_names = vec!["data".to_string()];
        std::fs::create_dir_all(record.box_dir.join("merged")).unwrap();
        let mut volume = VolumeConfig::new("data", "");
        volume.in_use_by = vec![record.id.clone()];
        client.volume_store().create(volume).unwrap();
        write_boxes(&client, &[record.clone()]);

        let stopped = client
            .stop_box("api", StopBox::new().timeout_secs(0))
            .await
            .unwrap();

        assert_eq!(stopped.id, record.id);
        assert_eq!(stopped.outcome, StopOutcome::ForceKilled);
        assert_eq!(stopped.exit_code, Some(137));
        assert_eq!(
            stopped
                .box_summary
                .as_ref()
                .map(|summary| summary.status.as_str()),
            Some("stopped")
        );
        let stored = client.get_box("api").unwrap().unwrap();
        assert_eq!(stored.status, "stopped");
        assert_eq!(stored.pid, None);
        assert_eq!(stored.health, "none");
        assert_eq!(stored.status_summary, "stopped (Exit 137)");
        assert!(client
            .get_volume("data")
            .unwrap()
            .unwrap()
            .in_use_by
            .is_empty());

        let _ = child.wait();
    }

    #[test]
    fn removes_inactive_box_and_runtime_resources_without_cli() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        let mut record = box_record("15151515-1515-4151-8151-151515151515", "api", "stopped");
        record.box_dir = dir.path().join("boxes").join(&record.id);
        record.exec_socket_path = dir.path().join("external-sockets").join("exec.sock");
        record.volume_names = vec!["data".to_string()];
        record.anonymous_volumes = vec!["anon".to_string()];
        std::fs::create_dir_all(record.box_dir.join("merged")).unwrap();
        std::fs::create_dir_all(record.exec_socket_path.parent().unwrap()).unwrap();
        write_boxes(&client, &[record.clone()]);

        client.create_volume(CreateVolume::new("data")).unwrap();
        client.create_volume(CreateVolume::new("anon")).unwrap();
        client
            .volume_store()
            .modify("data", |volume| {
                volume.in_use_by = vec![record.id.clone()];
            })
            .unwrap();
        client
            .volume_store()
            .modify("anon", |volume| {
                volume.in_use_by = vec![record.id.clone()];
            })
            .unwrap();
        client
            .create_network(CreateNetwork::new("dev").subnet("10.89.55.0/24"))
            .unwrap();
        client.connect_network("dev", "api").unwrap();

        let removed = client.remove_box("api", RemoveBox::new()).unwrap();

        assert_eq!(removed.id, record.id);
        assert_eq!(removed.name, "api");
        assert!(client.get_box("api").unwrap().is_none());
        assert!(!record.box_dir.exists());
        assert!(!record.exec_socket_path.parent().unwrap().exists());
        assert!(client
            .get_volume("data")
            .unwrap()
            .unwrap()
            .in_use_by
            .is_empty());
        assert!(client.get_volume("anon").unwrap().is_none());
        assert_eq!(
            client.get_network("dev").unwrap().unwrap().endpoint_count,
            0
        );
    }

    #[test]
    fn remove_box_rejects_active_box_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        let record = box_record("16161616-1616-4161-8161-161616161616", "api", "running");
        write_boxes(&client, &[record]);

        let error = client.remove_box("api", RemoveBox::new()).unwrap_err();

        assert!(format!("{error}").contains("Stop it before removing it"));
        assert!(client.get_box("api").unwrap().is_some());
    }

    #[test]
    fn reads_recent_structured_box_logs_without_spawning_cli() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        let mut record = box_record("11111111-1111-4111-8111-111111111111", "api", "running");
        record.box_dir = dir.path().join("boxes").join(&record.id);
        record.console_log = record.box_dir.join("logs").join("console.log");
        write_boxes(&client, &[record.clone()]);

        let log_dir = record.box_dir.join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            json_log_path(&log_dir),
            [
                r#"{"log":"first line\n","stream":"stdout","time":"2026-07-08T00:00:00Z"}"#,
                r#"{"log":"second line\n","stream":"stderr","time":"2026-07-08T00:00:01Z"}"#,
            ]
            .join("\n"),
        )
        .unwrap();

        let logs = client
            .read_box_logs("api", ReadBoxLogsOptions::tail(1))
            .unwrap();

        assert_eq!(
            logs,
            vec![BoxLogLine {
                stream: "stderr".to_string(),
                timestamp: Some("2026-07-08T00:00:01Z".to_string()),
                message: "second line".to_string(),
            }]
        );
    }

    #[test]
    fn reads_console_log_fallback_and_filters_runtime_noise() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        let mut record = box_record("11111111-1111-4111-8111-111111111111", "api", "stopped");
        record.box_dir = dir.path().join("boxes").join(&record.id);
        record.console_log = record.box_dir.join("logs").join("console.log");
        write_boxes(&client, &[record.clone()]);

        std::fs::create_dir_all(record.console_log.parent().unwrap()).unwrap();
        std::fs::write(
            &record.console_log,
            "init.krun: boot internals\ncontainer line\n",
        )
        .unwrap();

        let logs = client
            .read_box_logs(&record.id, ReadBoxLogsOptions::default())
            .unwrap();

        assert_eq!(
            logs,
            vec![BoxLogLine {
                stream: "stdout".to_string(),
                timestamp: None,
                message: "container line".to_string(),
            }]
        );
    }

    #[test]
    fn collects_active_box_stats_without_spawning_cli() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        let mut record = box_record("11111111-1111-4111-8111-111111111111", "api", "running");
        record.box_dir = dir.path().join("boxes").join(&record.id);
        std::fs::create_dir_all(record.box_dir.join("sockets")).unwrap();
        std::fs::write(
            record.box_dir.join("sockets").join("net.stats.json"),
            r#"{"schema":"a3s-box.netproxy.stats.v1","rx_bytes":1024,"tx_bytes":2048}"#,
        )
        .unwrap();
        write_boxes(&client, &[record.clone()]);

        let stats = client.list_box_stats().unwrap();
        let one = client.get_box_stats("api").unwrap().unwrap();

        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].id, record.id);
        assert_eq!(stats[0].network_rx_bytes, 1024);
        assert_eq!(stats[0].network_tx_bytes, 2048);
        assert_eq!(stats[0].memory_limit_bytes, 512 * 1024 * 1024);
        assert_eq!(one.id, stats[0].id);
    }

    #[test]
    fn inactive_box_stats_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        write_boxes(
            &client,
            &[box_record(
                "11111111-1111-4111-8111-111111111111",
                "api",
                "stopped",
            )],
        );

        assert!(client.get_box_stats("api").unwrap().is_none());
        assert!(client.list_box_stats().unwrap().is_empty());
    }
