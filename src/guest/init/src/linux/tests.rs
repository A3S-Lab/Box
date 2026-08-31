use super::*;

fn test_exec_config(env: Vec<(String, String)>) -> ExecConfig {
    ExecConfig {
        executable: "/bin/sh".into(),
        args: Vec::new(),
        env,
        workdir: "/".into(),
        user: None,
        stdin_null: true,
    }
}

#[test]
fn bootstrap_mode_is_explicit_and_fail_closed() {
    assert_eq!(
        BootstrapMode::from_value(None).unwrap(),
        BootstrapMode::Microvm
    );
    assert_eq!(
        BootstrapMode::from_value(Some("host-sandbox")).unwrap(),
        BootstrapMode::HostSandbox
    );
    assert!(BootstrapMode::from_value(Some("sandbox-ish")).is_err());
}

#[test]
fn host_sandbox_uses_owned_log_drain_instead_of_legacy_handoff() {
    assert_eq!(
        console_handoff_delay(BootstrapMode::HostSandbox),
        std::time::Duration::ZERO
    );
    assert_eq!(
        console_handoff_delay(BootstrapMode::Microvm),
        std::time::Duration::from_millis(250)
    );
}

#[test]
fn secret_environment_manifest_is_validated_consumed_and_injected() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let internal_root = directory.path().join(".a3s-box-secrets");
    let digest = "a".repeat(64);
    let secret_directory = internal_root.join(&digest);
    std::fs::create_dir_all(&secret_directory).unwrap();
    let secret_path = secret_directory.join("000.secret");
    std::fs::write(&secret_path, b"guest-secret-value").unwrap();
    std::fs::set_permissions(&secret_path, std::fs::Permissions::from_mode(0o400)).unwrap();
    let manifest = serde_json::to_string(&vec![SecretEnvironmentBinding {
        variable: "A3S_PROVIDER_TOKEN".into(),
        path: secret_path.to_string_lossy().into_owned(),
    }])
    .unwrap();
    let mut config = test_exec_config(vec![
        ("LANG".into(), "C.UTF-8".into()),
        (SECRET_ENVIRONMENT_MANIFEST.into(), manifest),
    ]);

    config
        .materialize_secret_environment_from(&internal_root)
        .unwrap();

    assert_eq!(
        config
            .env
            .iter()
            .find(|(key, _)| key == "A3S_PROVIDER_TOKEN")
            .map(|(_, value)| value.as_str()),
        Some("guest-secret-value")
    );
    assert!(config
        .env
        .iter()
        .all(|(key, _)| key != SECRET_ENVIRONMENT_MANIFEST));
    assert!(config.env.contains(&("LANG".into(), "C.UTF-8".into())));
}

#[test]
fn secret_environment_manifest_fails_closed_on_tamper() {
    let directory = tempfile::tempdir().unwrap();
    let internal_root = directory.path().join(".a3s-box-secrets");
    std::fs::create_dir_all(&internal_root).unwrap();

    let mut invalid_json = test_exec_config(vec![(
        SECRET_ENVIRONMENT_MANIFEST.into(),
        "not-json".into(),
    )]);
    assert!(invalid_json
        .materialize_secret_environment_from(&internal_root)
        .unwrap_err()
        .to_string()
        .contains("version-1 JSON"));
    assert!(invalid_json
        .env
        .iter()
        .all(|(key, _)| key != SECRET_ENVIRONMENT_MANIFEST));

    let escaped = serde_json::to_string(&vec![SecretEnvironmentBinding {
        variable: "A3S_PROVIDER_TOKEN".into(),
        path: "/etc/passwd".into(),
    }])
    .unwrap();
    let mut escaped_path = test_exec_config(vec![(SECRET_ENVIRONMENT_MANIFEST.into(), escaped)]);
    assert!(escaped_path
        .materialize_secret_environment_from(&internal_root)
        .unwrap_err()
        .to_string()
        .contains("escaped"));
}

#[test]
fn tmpfs_mount_parser_separates_access_mode_from_mount_data() {
    assert_eq!(
        parse_tmpfs_mount("/scratch:size=1048576,rw").unwrap(),
        ("/scratch", Some("size=1048576".into()), false)
    );
    assert_eq!(
        parse_tmpfs_mount("/sealed:size=4096,ro").unwrap(),
        ("/sealed", Some("size=4096".into()), true)
    );
    assert_eq!(
        parse_tmpfs_mount("/ephemeral").unwrap(),
        ("/ephemeral", None, false)
    );
    assert!(parse_tmpfs_mount("/scratch:ro,rw").is_err());
    assert!(parse_tmpfs_mount("/run/a3s-box/boot:size=4096").is_err());
}

#[test]
fn default_shm_mount_is_container_compatible() {
    let options = default_shm_mount_options();
    assert!(options.split(',').any(|option| option == "mode=1777"));
    assert!(options.split(',').any(|option| option == "size=67108864"));
}

fn set_sidecar_env(image: &str, vsock_port: u32, env: &[(&str, &str)]) {
    std::env::set_var("BOX_SIDECAR_IMAGE", image);
    std::env::set_var("BOX_SIDECAR_VSOCK_PORT", vsock_port.to_string());
    std::env::set_var("BOX_SIDECAR_ENV_COUNT", env.len().to_string());
    for (i, (k, v)) in env.iter().enumerate() {
        std::env::set_var(format!("BOX_SIDECAR_ENV_{}", i), format!("{}={}", k, v));
    }
}

fn clear_sidecar_env() {
    std::env::remove_var("BOX_SIDECAR_IMAGE");
    std::env::remove_var("BOX_SIDECAR_VSOCK_PORT");
    std::env::remove_var("BOX_SIDECAR_ENV_COUNT");
    for i in 0..10 {
        std::env::remove_var(format!("BOX_SIDECAR_ENV_{}", i));
    }
}

#[test]
fn test_virtiofs_mount_options_default_to_stable_cache_mode() {
    assert_eq!(
        virtiofs_mount_options_from_env_value(None).as_deref(),
        Some("cache=none")
    );
    assert_eq!(
        virtiofs_mount_options_from_env_value(Some("")).as_deref(),
        Some("cache=none")
    );
    assert_eq!(
        virtiofs_mount_options_from_env_value(Some("auto")).as_deref(),
        Some("cache=auto")
    );
    assert_eq!(virtiofs_mount_options_from_env_value(Some("default")), None);
}

#[test]
fn test_box_exec_auto_decode_accepts_runtime_encoded_exec() {
    assert!(is_plausible_exec(
        &decode_box_exec_value_if_valid("L2Jpbi9zaA").unwrap()
    ));
    assert_eq!(
        decode_box_exec_value("cnVudGltZS1oZWxwZXIuc2g".to_string(), true),
        "runtime-helper.sh"
    );
}

#[test]
fn test_box_exec_auto_decode_preserves_raw_legacy_values() {
    assert_eq!(
        decode_box_exec_value("/bin/sh".to_string(), false),
        "/bin/sh"
    );
    assert!(decode_box_exec_value_if_valid("/bin/sh").is_none());
    assert!(!is_plausible_exec(""));
}

#[test]
fn staged_exec_config_accepts_long_arguments_and_rejects_invalid_input() {
    let config = GuestExecConfig::new(
        "/bin/echo".to_string(),
        vec!["x".repeat(4096)],
        "/workspace".to_string(),
        Some("1000:1000".to_string()),
        true,
    );
    let bytes = serde_json::to_vec(&config).unwrap();
    assert_eq!(parse_staged_exec_config(&bytes).unwrap(), config);

    let mut wrong_schema = config;
    wrong_schema.schema = "a3s.box.guest-exec.v2".to_string();
    let error = parse_staged_exec_config(&serde_json::to_vec(&wrong_schema).unwrap())
        .unwrap_err()
        .to_string();
    assert!(error.contains("unsupported guest exec schema"), "{error}");

    let oversized = vec![b' '; MAX_RUNTIME_EXEC_CONFIG_BYTES + 1];
    let error = parse_staged_exec_config(&oversized)
        .unwrap_err()
        .to_string();
    assert!(error.contains("limit"), "{error}");
}

#[test]
fn guest_boot_config_parser_accepts_bundle_and_rejects_invalid_input() {
    let config = GuestBootConfig::new(
        GuestExecConfig::new(
            "/bin/echo".to_string(),
            vec!["hello".to_string()],
            "/workspace".to_string(),
            None,
            false,
        ),
        vec![("PATH".to_string(), "/bin".to_string())],
        GuestHostConfig {
            hostname: Some("web".to_string()),
            resolv_conf: Some("nameserver 1.1.1.1\n".to_string()),
            hosts: None,
        },
    );
    let bytes = serde_json::to_vec(&config).unwrap();

    assert_eq!(parse_guest_boot_config(&bytes).unwrap(), config);

    let mut wrong_schema = config;
    wrong_schema.schema = "a3s.box.guest-boot.v2".to_string();
    let error = parse_guest_boot_config(&serde_json::to_vec(&wrong_schema).unwrap())
        .unwrap_err()
        .to_string();
    assert!(error.contains("unsupported guest boot schema"), "{error}");

    let oversized = vec![b' '; MAX_GUEST_BOOT_CONFIG_BYTES + 1];
    let error = parse_guest_boot_config(&oversized).unwrap_err().to_string();
    assert!(error.contains("limit"), "{error}");
}

#[test]
fn guest_volume_mounts_cannot_cover_runtime_state() {
    assert!(guest_mount_path_overlaps_runtime_state("/run/a3s-box"));
    assert!(guest_mount_path_overlaps_runtime_state("/run"));
    assert!(guest_mount_path_overlaps_runtime_state(
        "/run//a3s-box/./boot"
    ));
    assert!(guest_mount_path_overlaps_runtime_state(
        "/srv/../run/a3s-box"
    ));
    assert!(!guest_mount_path_overlaps_runtime_state("/workspace"));
}

/// All sidecar env tests run sequentially in a single test to avoid
/// env var race conditions (env vars are process-global).
#[test]
fn test_sidecar_config_from_env() {
    // Subtest 1: no env vars → None
    clear_sidecar_env();
    assert!(SidecarConfig::from_env().is_none());

    // Subtest 2: empty image → None
    std::env::set_var("BOX_SIDECAR_IMAGE", "");
    assert!(SidecarConfig::from_env().is_none());
    std::env::remove_var("BOX_SIDECAR_IMAGE");

    // Subtest 3: basic config
    set_sidecar_env("safeclaw:latest", 4092, &[]);
    let config = SidecarConfig::from_env().unwrap();
    assert_eq!(config.image, "safeclaw:latest");
    assert_eq!(config.vsock_port, 4092);
    assert!(config.env.is_empty());
    clear_sidecar_env();

    // Subtest 4: with env vars
    set_sidecar_env(
        "ghcr.io/a3s-lab/safeclaw:latest",
        4092,
        &[("LOG_LEVEL", "debug"), ("MODE", "proxy")],
    );
    let config = SidecarConfig::from_env().unwrap();
    assert_eq!(config.image, "ghcr.io/a3s-lab/safeclaw:latest");
    assert_eq!(config.env.len(), 2);
    assert_eq!(
        config.env[0],
        ("LOG_LEVEL".to_string(), "debug".to_string())
    );
    assert_eq!(config.env[1], ("MODE".to_string(), "proxy".to_string()));
    clear_sidecar_env();

    // Subtest 5: default vsock port
    std::env::set_var("BOX_SIDECAR_IMAGE", "safeclaw:latest");
    std::env::remove_var("BOX_SIDECAR_VSOCK_PORT");
    std::env::remove_var("BOX_SIDECAR_ENV_COUNT");
    let config = SidecarConfig::from_env().unwrap();
    assert_eq!(config.vsock_port, 4092);
    clear_sidecar_env();

    // Subtest 6: custom vsock port
    set_sidecar_env("safeclaw:latest", 5000, &[]);
    let config = SidecarConfig::from_env().unwrap();
    assert_eq!(config.vsock_port, 5000);
    clear_sidecar_env();
}
