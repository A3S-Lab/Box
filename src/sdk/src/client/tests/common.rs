    fn client_for(dir: &tempfile::TempDir) -> A3sBoxClient {
        A3sBoxClient::from_home(dir.path()).with_image_cache_size(1024 * 1024 * 1024)
    }

    fn write_boxes(client: &A3sBoxClient, records: &[BoxRecord]) {
        std::fs::create_dir_all(&client.paths().home).unwrap();
        std::fs::write(
            &client.paths().boxes_file,
            serde_json::to_vec_pretty(records).unwrap(),
        )
        .unwrap();
    }

    fn write_minimal_oci_layout(path: &Path) {
        let blobs = path.join("blobs").join("sha256");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::write(path.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();

        let config_digest = "c".repeat(64);
        let manifest_digest = "d".repeat(64);
        let layer_digest = "e".repeat(64);
        let diff_id = "f".repeat(64);
        let layer = b"layer-bytes";
        std::fs::write(blobs.join(&layer_digest), layer).unwrap();

        let config = serde_json::json!({
            "architecture": "amd64",
            "os": "linux",
            "config": {
                "Entrypoint": ["/init"],
                "Cmd": ["serve"],
                "Env": ["A=1", "B=two"],
                "WorkingDir": "/srv/app",
                "User": "1000",
                "ExposedPorts": {"8080/tcp": {}},
                "Volumes": {"/data": {}},
                "Labels": {"org.opencontainers.image.title": "fixture"},
                "StopSignal": "SIGTERM",
                "Healthcheck": {
                    "Test": ["CMD-SHELL", "true"],
                    "Interval": 1000000000u64,
                    "Timeout": 2000000000u64,
                    "Retries": 3,
                    "StartPeriod": 3000000000u64
                },
                "OnBuild": ["RUN echo later"]
            },
            "rootfs": {
                "type": "layers",
                "diff_ids": [format!("sha256:{diff_id}")]
            },
            "history": [
                {
                    "created": "2026-07-08T00:00:00Z",
                    "created_by": "COPY app /srv/app",
                    "comment": "fixture layer"
                },
                {
                    "created": "2026-07-08T00:00:01Z",
                    "created_by": "CMD [\"serve\"]",
                    "empty_layer": true
                }
            ]
        });
        std::fs::write(
            blobs.join(&config_digest),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();

        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": format!("sha256:{config_digest}"),
                "size": 1
            },
            "layers": [{
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": format!("sha256:{layer_digest}"),
                "size": layer.len()
            }]
        });
        std::fs::write(
            blobs.join(&manifest_digest),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        let index = serde_json::json!({
            "schemaVersion": 2,
            "manifests": [{
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": format!("sha256:{manifest_digest}"),
                "size": 1
            }]
        });
        std::fs::write(path.join("index.json"), serde_json::to_vec(&index).unwrap()).unwrap();
    }

    fn box_record(id: &str, name: &str, status: &str) -> BoxRecord {
        BoxRecord {
            id: id.to_string(),
            short_id: BoxRecord::make_short_id(id),
            name: name.to_string(),
            image: "alpine:latest".to_string(),
            isolation: Default::default(),
            status: status.to_string(),
            pid: if matches!(status, "running" | "paused") {
                Some(std::process::id())
            } else {
                None
            },
            pid_start_time: None,
            cpus: 2,
            memory_mb: 512,
            volumes: vec![],
            env: HashMap::new(),
            cmd: vec!["sh".to_string()],
            entrypoint: None,
            box_dir: Path::new("/tmp").join(id),
            exec_socket_path: Path::new("/tmp").join(id).join("exec.sock"),
            console_log: Path::new("/tmp").join(id).join("console.log"),
            created_at: Utc::now(),
            started_at: None,
            auto_remove: false,
            hostname: None,
            user: None,
            workdir: None,
            restart_policy: "no".to_string(),
            port_map: vec!["8080:80".to_string()],
            labels: HashMap::new(),
            stopped_by_user: false,
            restart_count: 0,
            max_restart_count: 0,
            exit_code: None,
            health_check: None,
            healthcheck_disabled: false,
            health_status: "none".to_string(),
            health_retries: 0,
            health_last_check: None,
            network_mode: NetworkMode::default(),
            network_name: None,
            volume_names: vec![],
            tmpfs: vec![],
            anonymous_volumes: vec![],
            resource_limits: a3s_box_core::config::ResourceLimits::default(),
            log_config: a3s_box_core::log::LogConfig::default(),
            add_host: vec![],
            platform: None,
            init: false,
            read_only: false,
            cap_add: vec![],
            cap_drop: vec![],
            security_opt: vec![],
            privileged: false,
            devices: vec![],
            gpus: None,
            shm_size: None,
            stop_signal: None,
            stop_timeout: None,
            oom_kill_disable: false,
            oom_score_adj: None,
        }
    }
