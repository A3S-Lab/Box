    #[test]
    fn creates_lists_and_removes_volumes_via_runtime_store() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);

        let created = client
            .create_volume(CreateVolume::new("cache").label("role", "build"))
            .unwrap();

        assert_eq!(created.name, "cache");
        assert!(Path::new(&created.mount_point).exists());
        assert_eq!(client.list_volumes().unwrap().len(), 1);
        assert_eq!(
            client.get_volume("cache").unwrap().unwrap().labels["role"],
            "build"
        );
        assert_eq!(client.remove_volume("cache", false).unwrap().name, "cache");
        assert!(client.list_volumes().unwrap().is_empty());
    }

    #[test]
    fn creates_network_and_connects_inactive_box_via_runtime_store() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        write_boxes(
            &client,
            &[box_record(
                "33333333-3333-4333-8333-333333333333",
                "api",
                "stopped",
            )],
        );

        let network = client
            .create_network(CreateNetwork::new("dev").subnet("10.89.44.0/24"))
            .unwrap();
        let endpoint = client.connect_network("dev", "api").unwrap();
        let updated = client.get_network("dev").unwrap().unwrap();
        let box_after = client.get_box("api").unwrap().unwrap();

        assert_eq!(network.name, "dev");
        assert_eq!(endpoint.box_name, "api");
        assert_eq!(updated.endpoint_count, 1);
        assert_eq!(box_after.network_name.as_deref(), Some("dev"));

        let endpoint = client.disconnect_network("dev", "api").unwrap();
        let updated = client.get_network("dev").unwrap().unwrap();
        let box_after = client.get_box("api").unwrap().unwrap();

        assert_eq!(endpoint.box_name, "api");
        assert_eq!(updated.endpoint_count, 0);
        assert_eq!(box_after.network_name, None);
    }

    #[test]
    fn prunes_only_unused_non_predefined_networks_via_runtime_store() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        let referenced = box_record("33333333-3333-4333-8333-333333333333", "api", "stopped");
        let mut referenced = referenced;
        referenced.network_name = Some("referenced".to_string());
        referenced.network_mode = NetworkMode::Bridge {
            network: "referenced".to_string(),
        };
        write_boxes(&client, &[referenced]);

        client
            .create_network(CreateNetwork::new("orphan").subnet("10.89.10.0/24"))
            .unwrap();
        client
            .create_network(CreateNetwork::new("referenced").subnet("10.89.11.0/24"))
            .unwrap();
        client
            .create_network(CreateNetwork::new("attached").subnet("10.89.12.0/24"))
            .unwrap();
        client
            .network_store()
            .with_write_lock(|networks| {
                networks
                    .get_mut("attached")
                    .unwrap()
                    .connect("box-2", "worker")
                    .map_err(a3s_box_core::error::BoxError::NetworkError)
            })
            .unwrap();

        let removed = client.prune_networks().unwrap();
        let remaining = client
            .list_networks()
            .unwrap()
            .into_iter()
            .map(|network| network.name)
            .collect::<Vec<_>>();

        assert_eq!(removed, vec!["orphan"]);
        assert_eq!(remaining, vec!["attached", "referenced"]);
    }

    #[test]
    fn lists_removes_and_prunes_snapshots_via_runtime_store() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        let source = dir.path().join("rootfs-source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("file"), "snapshot-data").unwrap();
        let store = SnapshotStore::new(&client.paths().snapshots_dir).unwrap();

        let first = SnapshotMetadata::new(
            "snap-1".to_string(),
            "before-upgrade".to_string(),
            "box-1".to_string(),
            "alpine:latest".to_string(),
        )
        .with_description("Before upgrade");
        let second = SnapshotMetadata::new(
            "snap-2".to_string(),
            "after-upgrade".to_string(),
            "box-1".to_string(),
            "alpine:latest".to_string(),
        );
        store.save(first, &source).unwrap();
        store.save(second, &source).unwrap();

        let snapshots = client.list_snapshots().unwrap();
        assert_eq!(snapshots.len(), 2);
        assert_eq!(
            client.get_snapshot("snap-1").unwrap().unwrap().name,
            "before-upgrade"
        );
        assert!(client.remove_snapshot("snap-1").unwrap());

        let removed = client.prune_snapshots(0, 1).unwrap();

        assert_eq!(removed, vec!["snap-2"]);
        assert!(client.list_snapshots().unwrap().is_empty());
    }

    #[test]
    fn restore_rejects_snapshot_without_resolved_image_config() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        let source = dir.path().join("rootfs-source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("app.txt"), "snapshot-data").unwrap();
        SnapshotStore::new(&client.paths().snapshots_dir)
            .unwrap()
            .save(
                SnapshotMetadata::new(
                    "legacy-snapshot".to_string(),
                    "legacy-snapshot".to_string(),
                    "source-box".to_string(),
                    "alpine:3.20".to_string(),
                ),
                &source,
            )
            .unwrap();

        let error = client
            .restore_snapshot("legacy-snapshot", RestoreSnapshot::new())
            .unwrap_err();

        assert!(matches!(
            &error,
            ClientError::Validation(message)
                if message.contains("resolved OCI image configuration")
        ));
        assert!(client.list_boxes(ListBoxesOptions::all()).unwrap().is_empty());
    }

    #[test]
    fn restores_snapshot_into_created_box_record_via_runtime_store() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        let source = dir.path().join("rootfs-source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("app.txt"), "snapshot-data").unwrap();
        let store = SnapshotStore::new(&client.paths().snapshots_dir).unwrap();

        let mut metadata = SnapshotMetadata::new(
            "snap-restore".to_string(),
            "after-migration".to_string(),
            "source-box".to_string(),
            "alpine:3.20".to_string(),
        )
        .with_resources(4, 2048)
        .with_description("After migration");
        metadata.volumes = vec!["data:/data".to_string()];
        metadata
            .env
            .insert("APP_ENV".to_string(), "test".to_string());
        metadata.cmd = vec!["sleep".to_string(), "infinity".to_string()];
        metadata.entrypoint = Some(vec!["/entrypoint.sh".to_string()]);
        metadata.workdir = Some("/srv/app".to_string());
        metadata.port_map = vec!["8080:80".to_string()];
        metadata
            .labels
            .insert("tier".to_string(), "api".to_string());
        metadata.network_mode = Some("bridge".to_string());
        store.save(metadata, &source).unwrap();

        let restored = client
            .restore_snapshot("snap-restore", RestoreSnapshot::new().name("restored-api"))
            .unwrap();

        assert_eq!(restored.name, "restored-api");
        assert_eq!(restored.image, "alpine:3.20");
        assert_eq!(restored.status, "created");
        assert!(!restored.active);
        assert_eq!(restored.cpus, 4);
        assert_eq!(restored.memory_mb, 2048);
        assert_eq!(restored.ports, vec!["8080:80"]);

        let state = StateFile::load(&client.paths().boxes_file).unwrap();
        let record = state.find_by_name("restored-api").unwrap();
        assert_eq!(record.id, restored.id);
        assert_eq!(record.volumes, vec!["data:/data".to_string()]);
        assert_eq!(record.env.get("APP_ENV").map(String::as_str), Some("test"));
        assert_eq!(
            record.cmd,
            vec!["sleep".to_string(), "infinity".to_string()]
        );
        assert_eq!(record.entrypoint, Some(vec!["/entrypoint.sh".to_string()]));
        assert_eq!(record.workdir.as_deref(), Some("/srv/app"));
        assert_eq!(record.labels.get("tier").map(String::as_str), Some("api"));
        assert_eq!(record.pid, None);
        assert_eq!(record.started_at, None);
        assert!(record.exec_socket_path.ends_with("sockets/exec.sock"));
        assert!(record.console_log.ends_with("logs/console.log"));
        assert!(record.box_dir.join("sockets").is_dir());
        assert!(record.box_dir.join("logs").is_dir());
        assert_eq!(
            std::fs::read_to_string(record.box_dir.join(".snapshot-lower")).unwrap(),
            store
                .rootfs_path("snap-restore")
                .to_string_lossy()
                .to_string()
        );
        assert_eq!(
            client.get_box("restored-api").unwrap().unwrap().id,
            restored.id
        );
    }

    #[test]
    fn restores_snapshot_by_name_and_chooses_available_default_box_name() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        write_boxes(
            &client,
            &[box_record(
                "44444444-4444-4444-8444-444444444444",
                "after-migration-restore",
                "stopped",
            )],
        );
        let source = dir.path().join("rootfs-source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("file"), "snapshot-data").unwrap();
        let store = SnapshotStore::new(&client.paths().snapshots_dir).unwrap();
        store
            .save(
                SnapshotMetadata::new(
                    "snap-by-name".to_string(),
                    "after-migration".to_string(),
                    "box-1".to_string(),
                    "alpine:latest".to_string(),
                ),
                &source,
            )
            .unwrap();

        let restored = client
            .restore_snapshot("after-migration", RestoreSnapshot::new())
            .unwrap();

        assert_eq!(restored.name, "after-migration-restore-2");
        assert_eq!(client.list_boxes(ListBoxesOptions::all()).unwrap().len(), 2);
    }

    #[test]
    fn restore_rejects_ambiguous_snapshot_name() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        let source = dir.path().join("rootfs-source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("file"), "snapshot-data").unwrap();
        let store = SnapshotStore::new(&client.paths().snapshots_dir).unwrap();
        store
            .save(
                SnapshotMetadata::new(
                    "snap-1".to_string(),
                    "same".to_string(),
                    "box-1".to_string(),
                    "alpine:latest".to_string(),
                ),
                &source,
            )
            .unwrap();
        store
            .save(
                SnapshotMetadata::new(
                    "snap-2".to_string(),
                    "same".to_string(),
                    "box-2".to_string(),
                    "alpine:latest".to_string(),
                ),
                &source,
            )
            .unwrap();

        let error = client
            .restore_snapshot("same", RestoreSnapshot::new())
            .unwrap_err();

        assert!(format!("{error}").contains("matched multiple snapshots"));
    }

    #[test]
    fn prunes_only_created_stopped_and_dead_boxes_without_cli() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        let mut created = box_record("51515151-5151-4151-8151-515151515151", "created", "created");
        let mut stopped = box_record("52525252-5252-4252-8252-525252525252", "stopped", "stopped");
        let mut dead = box_record("53535353-5353-4353-8353-535353535353", "dead", "dead");
        let mut running = box_record("54545454-5454-4454-8454-545454545454", "running", "running");
        let mut paused = box_record("55555555-5555-4555-8555-555555555555", "paused", "paused");
        for record in [
            &mut created,
            &mut stopped,
            &mut dead,
            &mut running,
            &mut paused,
        ] {
            record.box_dir = client.paths().home.join("boxes").join(&record.id);
            record.exec_socket_path = record.box_dir.join("sockets").join("exec.sock");
            record.console_log = record.box_dir.join("logs").join("console.log");
            std::fs::create_dir_all(record.box_dir.join("logs")).unwrap();
        }
        write_boxes(
            &client,
            &[
                created.clone(),
                stopped.clone(),
                dead.clone(),
                running.clone(),
                paused.clone(),
            ],
        );

        let removed = client.prune_boxes().unwrap();
        let removed_names = removed
            .iter()
            .map(|summary| summary.name.as_str())
            .collect::<Vec<_>>();
        let remaining = client
            .list_boxes(ListBoxesOptions::all())
            .unwrap()
            .into_iter()
            .map(|summary| summary.name)
            .collect::<Vec<_>>();

        assert_eq!(removed_names, vec!["created", "stopped", "dead"]);
        assert_eq!(remaining, vec!["running", "paused"]);
        assert!(!created.box_dir.exists());
        assert!(!stopped.box_dir.exists());
        assert!(!dead.box_dir.exists());
        assert!(running.box_dir.exists());
        assert!(paused.box_dir.exists());
    }
