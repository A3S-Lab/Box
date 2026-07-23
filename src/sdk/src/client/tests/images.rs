    #[test]
    fn creates_snapshot_from_box_rootfs_without_cli() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        let mut record = box_record("17171717-1717-4171-8171-171717171717", "api", "stopped");
        record.box_dir = dir.path().join("boxes").join(&record.id);
        record.cmd = vec!["sh".to_string(), "-lc".to_string(), "echo ok".to_string()];
        record.env.insert("ENV".to_string(), "test".to_string());
        record.health_check = Some(a3s_box_core::ExecutionHealthCheck {
            cmd: vec!["test".to_string(), "-f".to_string(), "/ready".to_string()],
            interval_secs: 11,
            timeout_secs: 3,
            retries: 7,
            start_period_secs: 2,
        });
        record.healthcheck_disabled = true;
        let rootfs = record.box_dir.join("rootfs");
        std::fs::create_dir_all(rootfs.join("etc")).unwrap();
        std::fs::write(rootfs.join("etc").join("hostname"), "api").unwrap();
        write_resolved_image_config(&record);
        write_boxes(&client, &[record.clone()]);

        let snapshot = client
            .create_snapshot(
                "api",
                CreateSnapshot::new()
                    .name("before-upgrade")
                    .description("Created by SDK test"),
            )
            .unwrap();

        assert_eq!(snapshot.name, "before-upgrade");
        assert_eq!(snapshot.source_box_id, record.id);
        assert_eq!(snapshot.image, record.image);
        assert_eq!(snapshot.command, record.cmd);
        assert_eq!(snapshot.description, "Created by SDK test");
        assert!(snapshot.size_bytes > 0);
        assert_eq!(
            std::fs::read_to_string(
                client
                    .snapshot_store()
                    .unwrap()
                    .rootfs_path(&snapshot.id)
                    .join("etc")
                    .join("hostname")
            )
            .unwrap(),
            "api"
        );
        assert!(client.get_snapshot(&snapshot.id).unwrap().is_some());
        let metadata = client
            .snapshot_store()
            .unwrap()
            .get(&snapshot.id)
            .unwrap()
            .unwrap();
        assert_eq!(metadata.health_check, record.health_check);
        assert!(metadata.healthcheck_disabled);
        let image_config = metadata.image_config.unwrap();
        assert_eq!(
            image_config.entrypoint,
            Some(vec!["/usr/local/bin/envd".to_string()])
        );
        assert_eq!(image_config.working_dir.as_deref(), Some("/home/user"));
    }

    #[test]
    fn create_snapshot_rejects_an_active_box_before_rootfs_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        let mut record = box_record(
            "18181818-1818-4181-8181-181818181818",
            "active-api",
            "running",
        );
        record.pid = Some(std::process::id());
        record.box_dir = dir.path().join("boxes").join(&record.id);
        write_boxes(&client, &[record]);

        let error = client
            .create_snapshot("active-api", CreateSnapshot::new())
            .unwrap_err();

        assert!(matches!(
            error,
            ClientError::Validation(message)
                if message.contains("stop it first")
                    && message.contains("filesystem traversal")
        ));
    }

    #[tokio::test]
    async fn lists_images_from_runtime_store_index() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        let digest_hex = "a".repeat(64);
        let digest = format!("sha256:{digest_hex}");
        let image_path = client.paths().images_dir.join("sha256").join(&digest_hex);
        std::fs::create_dir_all(&image_path).unwrap();
        std::fs::create_dir_all(&client.paths().images_dir).unwrap();
        let now = Utc::now();
        let index = serde_json::json!({
            "images": [{
                "reference": "docker.io/library/alpine:latest",
                "digest": digest,
                "size_bytes": 42,
                "pulled_at": now,
                "last_used": now,
                "path": image_path
            }]
        });
        std::fs::write(
            client.paths().images_dir.join("index.json"),
            serde_json::to_vec_pretty(&index).unwrap(),
        )
        .unwrap();

        let images = client.list_images().await.unwrap();

        assert_eq!(images.len(), 1);
        assert_eq!(images[0].reference, "docker.io/library/alpine:latest");
        assert_eq!(images[0].size_bytes, 42);
    }

    #[tokio::test]
    async fn inspects_image_metadata_and_history_from_runtime_store() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        let store = client.open_image_store().unwrap();
        let source = dir.path().join("image-source");
        let digest = write_minimal_oci_layout(&source);
        store
            .put("docker.io/library/alpine:latest", &digest, &source)
            .await
            .unwrap();

        let inspect = client
            .inspect_image("alpine:latest")
            .await
            .unwrap()
            .unwrap();
        let history = client
            .image_history("alpine:latest")
            .await
            .unwrap()
            .unwrap();

        assert_eq!(inspect.reference, "docker.io/library/alpine:latest");
        assert_eq!(inspect.entrypoint, Some(vec!["/init".to_string()]));
        assert_eq!(inspect.command, Some(vec!["serve".to_string()]));
        assert_eq!(inspect.env.get("A").map(String::as_str), Some("1"));
        assert_eq!(inspect.working_dir.as_deref(), Some("/srv/app"));
        assert_eq!(inspect.user.as_deref(), Some("1000"));
        assert_eq!(inspect.exposed_ports, vec!["8080/tcp"]);
        assert_eq!(inspect.volumes, vec!["/data"]);
        assert_eq!(inspect.stop_signal.as_deref(), Some("SIGTERM"));
        assert_eq!(
            inspect
                .labels
                .get("org.opencontainers.image.title")
                .map(String::as_str),
            Some("fixture")
        );
        assert_eq!(
            inspect.health_check.as_ref().map(|health| health.retries),
            Some(Some(3))
        );
        assert_eq!(inspect.layer_count, 1);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].created_by, "COPY app /srv/app");
        assert_eq!(history[0].size_bytes, "layer-bytes".len() as u64);
        assert_eq!(history[0].comment, "fixture layer");
        assert!(history[1].empty_layer);
        assert_eq!(history[1].size_bytes, 0);
    }

    #[tokio::test]
    async fn tags_image_via_runtime_store_without_copying_layout() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        let store = client.open_image_store().unwrap();
        let source = dir.path().join("image-source");
        let digest = write_minimal_oci_layout(&source);
        let original = store
            .put("docker.io/library/alpine:latest", &digest, &source)
            .await
            .unwrap();

        let tagged = client
            .tag_image(TagImage::new("alpine:latest", "local/alpine:desktop"))
            .await
            .unwrap();
        let images = client.list_images().await.unwrap();

        assert_eq!(tagged.reference, "local/alpine:desktop");
        assert_eq!(tagged.digest, original.digest);
        assert_eq!(tagged.path, original.path);
        assert!(images
            .iter()
            .any(|image| image.reference == "docker.io/library/alpine:latest"));
        assert!(images
            .iter()
            .any(|image| image.reference == "local/alpine:desktop"));
    }

    #[tokio::test]
    async fn removes_and_evicts_images_via_runtime_store() {
        let dir = tempfile::tempdir().unwrap();
        let client = A3sBoxClient::from_home(dir.path()).with_image_cache_size(1);
        let store = client.open_image_store().unwrap();
        let source = dir.path().join("image-source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("layer"), "image-data").unwrap();

        store
            .put(
                "docker.io/library/alpine:latest",
                "sha256:1111111111111111111111111111111111111111111111111111111111111111",
                &source,
            )
            .await
            .unwrap();
        assert_eq!(client.list_images().await.unwrap().len(), 1);

        client
            .remove_image("docker.io/library/alpine:latest")
            .await
            .unwrap();
        assert!(client.list_images().await.unwrap().is_empty());

        let store = client.open_image_store().unwrap();
        store
            .put(
                "docker.io/library/busybox:latest",
                "sha256:2222222222222222222222222222222222222222222222222222222222222222",
                &source,
            )
            .await
            .unwrap();
        let evicted = client.evict_images().await.unwrap();

        assert_eq!(evicted, vec!["docker.io/library/busybox:latest"]);
        assert!(client.list_images().await.unwrap().is_empty());
    }

    #[test]
    fn image_operation_requests_validate_without_cli() {
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        std::fs::write(&dockerfile, "FROM scratch\n").unwrap();

        let pull = PullImage::new("alpine:latest")
            .force(true)
            .platform("linux/amd64")
            .credentials(RegistryCredentials::basic("user", "secret"));
        let build = BuildImage::new(dir.path())
            .tag("local/test:latest")
            .build_arg("MODE", "test")
            .platform(Platform::linux_amd64())
            .no_cache(true);
        let push = PushImage::new("local/test:latest", "example.com/acme/test:latest")
            .credentials(RegistryCredentials::basic("user", "secret"))
            .plain_http(true);

        assert!(pull.validate().is_ok());
        assert!(build.validate().is_ok());
        assert!(push.validate().is_ok());
        assert_eq!(build.dockerfile_path, dockerfile);
        assert_eq!(build.platforms, vec![Platform::linux_amd64()]);
        assert_eq!(push.registry_protocol, RegistryProtocol::Http);

        let empty_credentials = PullImage::new("alpine:latest")
            .credentials(RegistryCredentials::basic("", "secret"));
        assert!(matches!(
            empty_credentials.validate(),
            Err(ClientError::Validation(message))
                if message.contains("username cannot be empty")
        ));
        let empty_signature = PullImage::new("alpine:latest").signature_policy(
            SignaturePolicy::CosignKey {
                public_key: " ".to_string(),
            },
        );
        assert!(matches!(
            empty_signature.validate(),
            Err(ClientError::Validation(message))
                if message.contains("public key path cannot be empty")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn resolves_runtime_socket_paths_without_cli_helpers() {
        let mut record = box_record("11111111-1111-4111-8111-111111111111", "api", "running");
        record.exec_socket_path = Path::new("/tmp/custom-sockets").join("exec.sock");

        assert_eq!(
            runtime_socket(&record, RuntimeSocket::Exec),
            Path::new("/tmp/custom-sockets").join("exec.sock")
        );
        assert_eq!(
            runtime_socket(&record, RuntimeSocket::Pty),
            Path::new("/tmp/custom-sockets").join("pty.sock")
        );
        assert_eq!(
            runtime_socket(&record, RuntimeSocket::Attest),
            Path::new("/tmp/custom-sockets").join("attest.sock")
        );
    }

    #[cfg(unix)]
    #[test]
    fn runtime_socket_requires_running_box_and_existing_socket() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        let mut record = box_record("11111111-1111-4111-8111-111111111111", "api", "stopped");
        record.exec_socket_path = dir.path().join("sockets").join("exec.sock");
        write_boxes(&client, &[record.clone()]);

        let stopped = client
            .require_runtime_socket("api", RuntimeSocket::Exec)
            .unwrap_err();
        assert!(format!("{stopped}").contains("because it is stopped"));

        record.status = "running".to_string();
        record.pid = Some(std::process::id());
        write_boxes(&client, &[record]);
        let missing = client
            .require_runtime_socket("api", RuntimeSocket::Exec)
            .unwrap_err();
        assert!(format!("{missing}").contains("socket is missing"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn opens_exec_client_directly_against_runtime_socket() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&dir);
        let socket = dir.path().join("exec.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let accept = tokio::spawn(async move {
            let _ = listener.accept().await.unwrap();
        });

        let mut record = box_record("11111111-1111-4111-8111-111111111111", "api", "running");
        record.exec_socket_path = socket.clone();
        write_boxes(&client, &[record]);

        let exec = client.exec_client("api").await.unwrap();
        assert_eq!(exec.socket_path(), socket);
        accept.await.unwrap();
    }
