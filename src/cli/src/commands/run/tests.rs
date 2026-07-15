use super::*;

// --- build_resource_limits tests (using new struct layout) ---

fn default_run_args() -> RunArgs {
    RunArgs {
        common: common::CommonBoxArgs {
            image: "test".to_string(),
            isolation: None,
            name: None,
            cpus: 2,
            memory: "512m".to_string(),
            volumes: vec![],
            env: vec![],
            publish: vec![],
            dns: vec![],
            entrypoint: None,
            hostname: None,
            user: None,
            workdir: None,
            restart: "no".to_string(),
            labels: vec![],
            tmpfs: vec![],
            virtiofs_cache: None,
            network: None,
            health_cmd: None,
            health_interval: 30,
            health_timeout: 5,
            health_retries: 3,
            health_start_period: 0,
            pids_limit: None,
            cpuset_cpus: None,
            ulimits: vec![],
            cpu_shares: None,
            cpu_quota: None,
            cpu_period: None,
            memory_reservation: None,
            memory_swap: None,
            env_file: vec![],
            add_host: vec![],
            platform: None,
            init: false,
            read_only: false,
            cap_add: vec![],
            cap_drop: vec![],
            security_opt: vec![],
            privileged: false,
            device: vec![],
            gpus: None,
            shm_size: None,
            stop_signal: None,
            stop_timeout: None,
            no_healthcheck: false,
            oom_kill_disable: false,
            oom_score_adj: None,
            persistent: false,
        },
        detach: false,
        interactive: false,
        no_stdin: false,
        tty: false,
        timeout: None,
        rm: false,
        pool: false,
        pool_socket: DEFAULT_SOCKET.to_string(),
        pool_autostart: false,
        pool_exec: false,
        package_cache: vec![],
        cmd: vec![],
        log_driver: "json-file".to_string(),
        log_opts: vec![],
        tee: false,
        tee_workload_id: None,
        tee_simulate: false,
        sidecar: None,
        sidecar_vsock_port: 4092,
    }
}

fn default_pool_run_args() -> RunArgs {
    let mut args = default_run_args();
    args.pool = true;
    args.rm = true;
    args.cmd = vec!["echo".to_string(), "hello".to_string()];
    args
}

#[test]
fn test_foreground_auto_remove_skips_diff_baseline() {
    let mut args = default_run_args();
    args.rm = true;

    assert!(!should_create_diff_baseline(&args));
}

#[test]
fn test_detached_auto_remove_keeps_diff_baseline_while_running() {
    let mut args = default_run_args();
    args.rm = true;
    args.detach = true;

    assert!(should_create_diff_baseline(&args));
}

#[test]
fn test_persistent_run_keeps_diff_baseline() {
    let args = default_run_args();

    assert!(should_create_diff_baseline(&args));
}

#[test]
fn test_build_resource_limits_defaults() {
    let args = default_run_args();
    let limits = common::build_resource_limits(&args.common).unwrap();
    assert!(limits.pids_limit.is_none());
    assert!(limits.cpuset_cpus.is_none());
    assert!(limits.cpu_shares.is_none());
    assert!(limits.memory_reservation.is_none());
    assert!(limits.memory_swap.is_none());
}

#[test]
fn test_build_resource_limits_with_values() {
    let mut args = default_run_args();
    args.common.pids_limit = Some(100);
    args.common.cpuset_cpus = Some("0-3".to_string());
    args.common.ulimits = vec!["nofile=1024:4096".to_string()];
    args.common.cpu_shares = Some(512);
    args.common.cpu_quota = Some(50000);
    args.common.cpu_period = Some(100000);
    args.common.memory_reservation = Some("256m".to_string());
    args.common.memory_swap = Some("-1".to_string());

    let limits = common::build_resource_limits(&args.common).unwrap();
    assert_eq!(limits.pids_limit, Some(100));
    assert_eq!(limits.cpuset_cpus, Some("0-3".to_string()));
    assert_eq!(limits.cpu_shares, Some(512));
    assert_eq!(limits.cpu_quota, Some(50000));
    assert_eq!(limits.cpu_period, Some(100000));
    assert_eq!(limits.memory_reservation, Some(256 * 1024 * 1024));
    assert_eq!(limits.memory_swap, Some(-1));
}

#[test]
fn test_build_resource_limits_memory_swap_value() {
    let mut args = default_run_args();
    args.common.memory_swap = Some("1g".to_string());

    let limits = common::build_resource_limits(&args.common).unwrap();
    assert_eq!(limits.memory_swap, Some(1024 * 1024 * 1024));
}

#[test]
fn test_parse_health_check_none() {
    let args = default_run_args();
    assert!(parse_health_check(&args.common).is_none());
}

#[test]
fn test_parse_health_check_disabled() {
    let mut args = default_run_args();
    args.common.health_cmd = Some("curl localhost".to_string());
    args.common.no_healthcheck = true;
    assert!(parse_health_check(&args.common).is_none());
}

#[test]
fn test_parse_health_check_configured() {
    let mut args = default_run_args();
    args.common.health_cmd = Some("curl localhost".to_string());
    args.common.health_interval = 10;
    args.common.health_retries = 5;
    let hc = parse_health_check(&args.common).unwrap();
    assert_eq!(hc.cmd, vec!["sh", "-c", "curl localhost"]);
    assert_eq!(hc.interval_secs, 10);
    assert_eq!(hc.retries, 5);
}

#[test]
fn test_validate_run_mode_rejects_detached_tty_before_boot() {
    let mut args = default_run_args();
    args.detach = true;
    args.tty = true;

    let err = validate_run_mode(&args, true).unwrap_err();
    assert!(err.contains("Cannot use -t"));
}

#[test]
fn test_validate_run_mode_rejects_tty_without_terminal_before_boot() {
    let mut args = default_run_args();
    args.tty = true;

    let err = validate_run_mode(&args, false).unwrap_err();
    assert!(err.contains("requires a terminal"));
}

#[test]
fn test_validate_run_mode_allows_detached_without_tty() {
    let mut args = default_run_args();
    args.detach = true;

    assert!(validate_run_mode(&args, false).is_ok());
}

#[test]
fn test_validate_run_mode_rejects_no_stdin_with_interactive() {
    let mut args = default_run_args();
    args.interactive = true;
    args.no_stdin = true;

    let err = validate_run_mode(&args, true).unwrap_err();
    assert!(err.contains("--interactive"));
}

#[test]
fn test_validate_run_mode_rejects_invalid_timeout_modes() {
    let mut args = default_run_args();
    args.timeout = Some(0);
    let err = validate_run_mode(&args, true).unwrap_err();
    assert!(err.contains("greater than zero"));

    let mut args = default_run_args();
    args.timeout = Some(30);
    args.detach = true;
    let err = validate_run_mode(&args, true).unwrap_err();
    assert!(err.contains("--timeout"));
    assert!(err.contains("detach"));

    let mut args = default_run_args();
    args.timeout = Some(30);
    args.tty = true;
    let err = validate_run_mode(&args, true).unwrap_err();
    assert!(err.contains("--timeout"));
    assert!(err.contains("tty"));
}

#[test]
fn test_validate_pool_run_mode_requires_auto_remove_and_command() {
    let mut args = default_pool_run_args();
    args.rm = false;
    let err = validate_run_mode(&args, true).unwrap_err();
    assert!(err.contains("requires --rm"));

    let mut args = default_pool_run_args();
    args.cmd = vec![];
    let err = validate_run_mode(&args, true).unwrap_err();
    assert!(err.contains("explicit command"));
}

#[test]
fn test_validate_pool_run_mode_rejects_unsupported_modes() {
    let mut args = default_pool_run_args();
    args.detach = true;
    let err = validate_run_mode(&args, true).unwrap_err();
    assert!(err.contains("--pool"));
    assert!(err.contains("detach"));

    let mut args = default_pool_run_args();
    args.interactive = true;
    let err = validate_run_mode(&args, true).unwrap_err();
    assert!(err.contains("--interactive"));

    let mut args = default_pool_run_args();
    args.common.publish = vec!["8080:80".to_string()];
    let err = validate_run_mode(&args, true).unwrap_err();
    assert!(err.contains("currently supports only"));

    let mut args = default_pool_run_args();
    args.common.name = Some("named-pool-run".to_string());
    let err = validate_run_mode(&args, true).unwrap_err();
    assert!(err.contains("currently supports only"));
}

#[test]
fn test_validate_pool_run_mode_allows_timeout() {
    let mut args = default_pool_run_args();
    args.timeout = Some(30);

    assert!(validate_run_mode(&args, true).is_ok());
}

#[test]
fn test_selected_pool_socket_prefers_explicit_pool_socket() {
    let mut args = default_pool_run_args();
    args.pool_socket = "/tmp/explicit.sock".to_string();

    assert_eq!(
        selected_pool_socket(&args, Some("/tmp/env.sock")).as_deref(),
        Some("/tmp/explicit.sock")
    );
}

#[test]
fn test_selected_pool_socket_uses_env_for_compatible_foreground_run() {
    let mut args = default_run_args();
    args.rm = true;
    args.cmd = vec!["bash".to_string(), "-lc".to_string(), "echo ok".to_string()];
    args.package_cache = vec![PackageCache::Pnpm];
    args.common.volumes = vec!["/host/work:/workspace:rw".to_string()];
    args.common.workdir = Some("/workspace".to_string());

    assert_eq!(
        selected_pool_socket(&args, Some(" /tmp/runtime.sock ")).as_deref(),
        Some("/tmp/runtime.sock")
    );
}

#[test]
fn test_selected_pool_socket_ignores_env_for_incompatible_run() {
    let mut args = default_run_args();
    args.rm = true;
    args.detach = true;
    args.cmd = vec!["echo".to_string(), "ok".to_string()];

    assert!(selected_pool_socket(&args, Some("/tmp/runtime.sock")).is_none());

    let mut named = default_run_args();
    named.rm = true;
    named.common.name = Some("named-run".to_string());
    named.cmd = vec!["echo".to_string(), "ok".to_string()];
    assert!(selected_pool_socket(&named, Some("/tmp/runtime.sock")).is_none());
    assert!(selected_pool_socket(&args, Some("")).is_none());
}

#[test]
fn test_selected_pool_socket_uses_autostart_flag() {
    let mut args = default_pool_run_args();
    args.pool = false;
    args.pool_autostart = true;
    args.pool_socket = "/tmp/autostart.sock".to_string();

    assert_eq!(
        selected_pool_socket(&args, None).as_deref(),
        Some("/tmp/autostart.sock")
    );
}

#[test]
fn test_pool_autostart_config_prewarms_simple_run() {
    let args = default_pool_run_args();
    let config = pool_autostart_config_for_run(&args, "/tmp/pool.sock").unwrap();

    assert_eq!(config.socket, "/tmp/pool.sock");
    assert_eq!(config.image.as_deref(), Some("test"));
}

#[test]
fn test_pool_autostart_config_skips_prewarm_for_volume_shape() {
    let mut args = default_pool_run_args();
    args.common.volumes = vec!["/host:/work:ro".to_string()];
    let config = pool_autostart_config_for_run(&args, "/tmp/pool.sock").unwrap();

    assert!(config.image.is_none());
}

#[test]
fn test_build_pool_client_run_plumbs_supported_options() {
    let tmp = tempfile::tempdir().unwrap();
    let env_file = tmp.path().join("env.list");
    std::fs::write(&env_file, "B=file\nC=file\n").unwrap();
    let bind = format!("{}:/workspace:ro", tmp.path().display());

    let mut args = default_pool_run_args();
    args.common.image = "node:24-bookworm".to_string();
    args.common.cpus = 4;
    args.common.memory = "2g".to_string();
    args.common.volumes = vec![bind.clone()];
    args.common.env = vec!["A=cli".to_string(), "B=cli".to_string()];
    args.common.env_file = vec![env_file.display().to_string()];
    args.common.user = Some("root".to_string());
    args.common.workdir = Some("/workspace".to_string());
    args.pool_socket = "/tmp/a3s-box-test-pool.sock".to_string();
    args.pool_exec = true;
    args.timeout = Some(45);

    let req = build_pool_client_run(&args, &args.pool_socket).unwrap();

    assert_eq!(req.socket, "/tmp/a3s-box-test-pool.sock");
    assert_eq!(req.image.as_deref(), Some("node:24-bookworm"));
    assert_eq!(req.user.as_deref(), Some("0"));
    assert_eq!(req.workdir.as_deref(), Some("/workspace"));
    assert_eq!(req.volumes, vec![bind]);
    assert_eq!(req.vcpus, 4);
    assert_eq!(req.memory_mb, 2048);
    assert!(req.exec);
    assert_eq!(req.timeout_ns, Some(45_000_000_000));
    assert_eq!(req.cmd, vec!["echo", "hello"]);
    assert_eq!(req.env, vec!["A=cli", "B=cli", "C=file"]);
}

#[test]
fn test_apply_package_caches_adds_pnpm_volume_and_env() {
    let mut volumes = Vec::new();
    let mut env = std::collections::HashMap::new();

    apply_package_caches(&[PackageCache::Pnpm], &mut volumes, &mut env);

    assert_eq!(volumes, vec![PNPM_CACHE_VOLUME_SPEC.to_string()]);
    assert_eq!(
        env.get(PNPM_CONFIG_STORE_ENV).map(String::as_str),
        Some(PNPM_STORE_DIR)
    );
    assert_eq!(
        env.get(PNPM_STORE_ENV).map(String::as_str),
        Some(PNPM_STORE_DIR)
    );
    assert_eq!(
        env.get(PNPM_COREPACK_HOME_ENV).map(String::as_str),
        Some(PNPM_COREPACK_HOME_DIR)
    );
    assert_eq!(
        env.get(PNPM_HOME_ENV).map(String::as_str),
        Some(PNPM_HOME_DIR)
    );
    assert_eq!(
        env.get(PNPM_NPM_CACHE_ENV).map(String::as_str),
        Some(PNPM_NPM_CACHE_DIR)
    );
    assert_eq!(
        env.get(PNPM_CONFIG_PREFER_OFFLINE_ENV).map(String::as_str),
        Some(PNPM_PREFER_OFFLINE_VALUE)
    );
    assert_eq!(
        env.get(PNPM_PREFER_OFFLINE_ENV).map(String::as_str),
        Some(PNPM_PREFER_OFFLINE_VALUE)
    );
    assert_eq!(
        env.get(COREPACK_DOWNLOAD_PROMPT_ENV).map(String::as_str),
        Some(COREPACK_DOWNLOAD_PROMPT_VALUE)
    );
}

#[test]
fn test_apply_package_caches_preserves_user_pnpm_env() {
    let mut volumes = Vec::new();
    let mut env = std::collections::HashMap::from([
        (
            PNPM_CONFIG_STORE_ENV.to_string(),
            "/custom/pnpm-config-store".to_string(),
        ),
        (PNPM_STORE_ENV.to_string(), "/custom/pnpm-store".to_string()),
        (
            PNPM_COREPACK_HOME_ENV.to_string(),
            "/custom/corepack".to_string(),
        ),
        (PNPM_HOME_ENV.to_string(), "/custom/pnpm-home".to_string()),
        (
            PNPM_NPM_CACHE_ENV.to_string(),
            "/custom/npm-cache".to_string(),
        ),
        (
            PNPM_CONFIG_PREFER_OFFLINE_ENV.to_string(),
            "false".to_string(),
        ),
        (PNPM_PREFER_OFFLINE_ENV.to_string(), "false".to_string()),
        (COREPACK_DOWNLOAD_PROMPT_ENV.to_string(), "1".to_string()),
    ]);

    apply_package_caches(&[PackageCache::Pnpm], &mut volumes, &mut env);

    assert_eq!(
        env.get(PNPM_CONFIG_STORE_ENV).map(String::as_str),
        Some("/custom/pnpm-config-store")
    );
    assert_eq!(
        env.get(PNPM_STORE_ENV).map(String::as_str),
        Some("/custom/pnpm-store")
    );
    assert_eq!(
        env.get(PNPM_COREPACK_HOME_ENV).map(String::as_str),
        Some("/custom/corepack")
    );
    assert_eq!(
        env.get(PNPM_HOME_ENV).map(String::as_str),
        Some("/custom/pnpm-home")
    );
    assert_eq!(
        env.get(PNPM_NPM_CACHE_ENV).map(String::as_str),
        Some("/custom/npm-cache")
    );
    assert_eq!(
        env.get(PNPM_CONFIG_PREFER_OFFLINE_ENV).map(String::as_str),
        Some("false")
    );
    assert_eq!(
        env.get(PNPM_PREFER_OFFLINE_ENV).map(String::as_str),
        Some("false")
    );
    assert_eq!(
        env.get(COREPACK_DOWNLOAD_PROMPT_ENV).map(String::as_str),
        Some("1")
    );
}

#[test]
fn test_apply_package_caches_deduplicates_pnpm_volume() {
    let mut volumes = vec![PNPM_CACHE_VOLUME_SPEC.to_string()];
    let mut env = std::collections::HashMap::new();

    apply_package_caches(
        &[PackageCache::Pnpm, PackageCache::Pnpm],
        &mut volumes,
        &mut env,
    );

    assert_eq!(volumes, vec![PNPM_CACHE_VOLUME_SPEC.to_string()]);
}

#[test]
fn test_apply_package_caches_adds_npm_volume_and_env() {
    let mut volumes = Vec::new();
    let mut env = std::collections::HashMap::new();

    apply_package_caches(&[PackageCache::Npm], &mut volumes, &mut env);

    assert_eq!(volumes, vec![NPM_CACHE_VOLUME_SPEC.to_string()]);
    assert_eq!(
        env.get(NPM_CACHE_ENV).map(String::as_str),
        Some(NPM_CACHE_DIR)
    );
    assert_eq!(
        env.get(NPM_PREFER_OFFLINE_ENV).map(String::as_str),
        Some(NPM_PREFER_OFFLINE_VALUE)
    );
    assert!(!env.contains_key(PNPM_STORE_ENV));
    assert!(!env.contains_key(PNPM_COREPACK_HOME_ENV));
    assert!(!env.contains_key(PNPM_HOME_ENV));
}

#[test]
fn test_apply_package_caches_preserves_user_npm_env() {
    let mut volumes = Vec::new();
    let mut env = std::collections::HashMap::from([
        (NPM_CACHE_ENV.to_string(), "/custom/npm-cache".to_string()),
        (NPM_PREFER_OFFLINE_ENV.to_string(), "false".to_string()),
    ]);

    apply_package_caches(&[PackageCache::Npm], &mut volumes, &mut env);

    assert_eq!(
        env.get(NPM_CACHE_ENV).map(String::as_str),
        Some("/custom/npm-cache")
    );
    assert_eq!(
        env.get(NPM_PREFER_OFFLINE_ENV).map(String::as_str),
        Some("false")
    );
}

#[test]
fn test_apply_package_caches_deduplicates_npm_volume() {
    let mut volumes = vec![NPM_CACHE_VOLUME_SPEC.to_string()];
    let mut env = std::collections::HashMap::new();

    apply_package_caches(
        &[PackageCache::Npm, PackageCache::Npm],
        &mut volumes,
        &mut env,
    );

    assert_eq!(volumes, vec![NPM_CACHE_VOLUME_SPEC.to_string()]);
}

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
fn test_foreground_exit_code_has_deterministic_fallbacks() {
    assert_eq!(
        foreground_exit_code(ForegroundStopReason::ProcessExited, None),
        None
    );
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

#[path = "request_tests.rs"]
mod request_tests;

#[test]
fn test_resolve_volumes_empty() {
    let (resolved, names) = resolve_volumes(&[]).unwrap();
    assert!(resolved.is_empty());
    assert!(names.is_empty());
}
