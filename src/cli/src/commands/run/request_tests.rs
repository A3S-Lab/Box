use super::*;

#[test]
fn test_build_box_config_selects_requested_sandbox_isolation() {
    let mut args = default_run_args();
    args.common.isolation = Some(common::IsolationArg::Sandbox);

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

    assert_eq!(config.isolation, a3s_box_core::ExecutionIsolation::Sandbox);
}

#[test]
fn test_managed_run_request_preserves_complete_caller_intent() {
    let mut args = default_run_args();
    args.common.image = "registry.example/worker:v2".to_string();
    args.common.cpus = 6;
    args.common.dns = vec!["1.1.1.1".to_string(), "8.8.8.8".to_string()];
    args.common.hostname = Some("worker".to_string());
    args.common.user = Some("root".to_string());
    args.common.workdir = Some("/workspace".to_string());
    args.common.virtiofs_cache = Some(common::VirtiofsCacheMode::Always);
    args.common.tmpfs = vec!["/tmp:size=16m".to_string()];
    args.common.add_host = vec!["db.internal:10.0.0.2".to_string()];
    args.common.read_only = true;
    args.common.cap_add = vec!["NET_ADMIN".to_string()];
    args.common.cap_drop = vec!["NET_RAW".to_string()];
    args.common.security_opt = vec!["no-new-privileges".to_string()];
    args.common.privileged = true;
    args.common.pids_limit = Some(128);
    args.common.cpuset_cpus = Some("0-2".to_string());
    args.common.cpu_shares = Some(1024);
    args.common.memory_reservation = Some("512m".to_string());
    args.common.memory_swap = Some("2g".to_string());
    args.common.platform = Some("linux/arm64".to_string());
    args.common.init = true;
    args.common.device = vec!["/dev/fuse:/dev/fuse".to_string()];
    args.common.gpus = Some("all".to_string());
    args.common.stop_timeout = Some(9);
    args.common.oom_kill_disable = true;
    args.common.oom_score_adj = Some(125);
    args.common.persistent = true;
    args.rm = true;
    args.cmd = vec!["python".to_string(), "worker.py".to_string()];
    args.sidecar = Some("registry.example/proxy:v1".to_string());
    args.sidecar_vsock_port = 5001;

    let resource_limits = common::build_resource_limits(&args.common).unwrap();
    let tee = TeeConfig::SevSnp {
        workload_id: "worker-v2".to_string(),
        generation: Default::default(),
        simulate: true,
    };
    let config = build_box_config(
        &args,
        4096,
        resource_limits.clone(),
        Some(vec!["/entrypoint".to_string()]),
        vec!["/host/workspace:/workspace:rw".to_string()],
        vec![("MODE".to_string(), "test".to_string())],
        vec!["8080:80".to_string()],
        a3s_box_core::NetworkMode::Bridge {
            network: "dev".to_string(),
        },
        args.common.tmpfs.clone(),
        tee.clone(),
    )
    .unwrap();
    let health_check = crate::state::HealthCheck {
        cmd: vec!["test".to_string(), "-f".to_string(), "/ready".to_string()],
        interval_secs: 11,
        timeout_secs: 3,
        retries: 7,
        start_period_secs: 5,
    };
    let log_config = a3s_box_core::log::LogConfig {
        driver: a3s_box_core::log::LogDriver::None,
        options: std::collections::HashMap::from([("tag".to_string(), "worker".to_string())]),
    };
    let operation_id = OperationId::new("cli-run-request-test").unwrap();
    let labels = std::collections::BTreeMap::from([("team".to_string(), "sandbox".to_string())]);
    let request = build_execution_request(
        &args,
        &operation_id,
        config,
        labels.clone(),
        RunRecordPolicy {
            name: "managed-worker".to_string(),
            restart_policy: ExecutionRestartPolicy::OnFailure,
            max_restart_count: 4,
            health_check: Some(health_check.clone()),
            log_config: log_config.clone(),
            volume_names: vec!["workspace".to_string()],
            shm_size: Some(64 * 1024 * 1024),
            stop_signal: Some("SIGINT".to_string()),
        },
    );

    assert_eq!(request.external_sandbox_id, operation_id.as_str());
    assert_eq!(request.labels, labels);
    assert_eq!(request.config.image, "registry.example/worker:v2");
    assert_eq!(request.config.resources.vcpus, 6);
    assert_eq!(request.config.resources.memory_mb, 4096);
    assert_eq!(
        serde_json::to_value(&request.config.resource_limits).unwrap(),
        serde_json::to_value(&resource_limits).unwrap()
    );
    assert_eq!(request.config.cmd, vec!["python", "worker.py"]);
    assert_eq!(
        request.config.entrypoint_override,
        Some(vec!["/entrypoint".to_string()])
    );
    assert_eq!(
        request.config.extra_env,
        vec![("MODE".to_string(), "test".to_string())]
    );
    assert_eq!(request.config.dns, vec!["1.1.1.1", "8.8.8.8"]);
    assert_eq!(request.config.cap_add, vec!["NET_ADMIN"]);
    assert_eq!(request.config.cap_drop, vec!["NET_RAW"]);
    assert_eq!(request.config.security_opt, vec!["no-new-privileges"]);
    assert!(request.config.privileged);
    assert_eq!(request.config.tee, tee);
    assert_eq!(
        request
            .config
            .sidecar
            .as_ref()
            .map(|sidecar| (sidecar.image.as_str(), sidecar.vsock_port)),
        Some(("registry.example/proxy:v1", 5001))
    );
    assert!(request.config.persistent);
    assert_eq!(request.policy.name.as_deref(), Some("managed-worker"));
    assert!(request.policy.auto_remove);
    assert_eq!(
        request.policy.restart_policy,
        ExecutionRestartPolicy::OnFailure
    );
    assert_eq!(request.policy.max_restart_count, 4);
    assert_eq!(request.policy.health_check, Some(health_check));
    assert_eq!(request.policy.log_config, log_config);
    assert_eq!(request.policy.volume_names, vec!["workspace"]);
    assert_eq!(request.policy.platform.as_deref(), Some("linux/arm64"));
    assert!(request.policy.init);
    assert_eq!(request.policy.devices, vec!["/dev/fuse:/dev/fuse"]);
    assert_eq!(request.policy.gpus.as_deref(), Some("all"));
    assert_eq!(request.policy.shm_size, Some(64 * 1024 * 1024));
    assert_eq!(request.policy.stop_signal.as_deref(), Some("SIGINT"));
    assert_eq!(request.policy.stop_timeout, Some(9));
    assert!(request.policy.oom_kill_disable);
    assert_eq!(request.policy.oom_score_adj, Some(125));
}
