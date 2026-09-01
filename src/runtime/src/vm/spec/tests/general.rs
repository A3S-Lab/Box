use super::*;

#[test]
fn anonymous_volume_plan_normalizes_deduplicates_and_honors_explicit_mounts() {
    let explicit = tempfile::tempdir().unwrap();
    let vm = test_vm_manager(BoxConfig {
        volumes: vec![format!("{}:/data/./", explicit.path().display())],
        ..Default::default()
    });
    let mut image = test_oci_config(None, None);
    image.volumes = vec![
        "/data".to_string(),
        "/data/".to_string(),
        "/cache/./".to_string(),
        "/cache".to_string(),
    ];

    let plan = vm.plan_anonymous_volumes(&image).unwrap();

    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].guest_path, "/cache");
}

#[test]
fn anonymous_volume_identity_is_deterministic_and_bound_to_the_full_execution_id() {
    let image = OciImageConfig {
        volumes: vec!["/var/cache".to_string(), "/data".to_string()],
        ..test_oci_config(None, None)
    };
    let reordered_image = OciImageConfig {
        volumes: vec!["/data".to_string(), "/var/cache".to_string()],
        ..test_oci_config(None, None)
    };
    let first = VmManager::with_box_id(
        BoxConfig::default(),
        EventEmitter::new(1),
        "12345678-0000-0000-0000-000000000001".to_string(),
    );
    let second = VmManager::with_box_id(
        BoxConfig::default(),
        EventEmitter::new(1),
        "12345678-0000-0000-0000-000000000002".to_string(),
    );

    let first_plan = first.plan_anonymous_volumes(&image).unwrap();
    let replay = first.plan_anonymous_volumes(&reordered_image).unwrap();
    let second_plan = second.plan_anonymous_volumes(&image).unwrap();

    assert_eq!(first_plan, replay);
    assert_ne!(first_plan[0].name, second_plan[0].name);
    assert!(first_plan[0].name.starts_with("anon_12345678_"));
}

#[test]
fn test_build_instance_spec_passes_configured_virtiofs_cache_mode() {
    let dir = tempdir().unwrap();
    let layout = test_layout(dir.path(), Some(test_oci_config(None, None)), true);
    let mut vm = test_vm_manager(BoxConfig {
        virtiofs_cache: Some("always".to_string()),
        ..Default::default()
    });

    let spec = vm.build_instance_spec(&layout).unwrap();

    assert_eq!(env_value(&spec, "A3S_VIRTIOFS_CACHE"), Some("always"));
}

#[test]
fn test_build_instance_spec_disables_tsi_only_for_network_none() {
    let none_dir = tempdir().unwrap();
    let none_layout = test_layout(none_dir.path(), Some(test_oci_config(None, None)), true);
    let mut none_vm = test_vm_manager(BoxConfig {
        network: a3s_box_core::NetworkMode::None,
        ..Default::default()
    });
    assert!(
        none_vm
            .build_instance_spec(&none_layout)
            .unwrap()
            .disable_tsi
    );

    let tsi_dir = tempdir().unwrap();
    let tsi_layout = test_layout(tsi_dir.path(), Some(test_oci_config(None, None)), true);
    let mut tsi_vm = test_vm_manager(BoxConfig {
        network: a3s_box_core::NetworkMode::Tsi,
        ..Default::default()
    });
    assert!(!tsi_vm.build_instance_spec(&tsi_layout).unwrap().disable_tsi);
}

#[test]
fn test_persistent_box_requests_terminal_rootfs_metadata() {
    let dir = tempdir().unwrap();
    let layout = test_layout(dir.path(), Some(test_oci_config(None, None)), true);
    let mut vm = test_vm_manager(BoxConfig {
        persistent: true,
        ..Default::default()
    });

    let spec = vm.build_instance_spec(&layout).unwrap();

    assert_eq!(env_value(&spec, "BOX_PERSIST_ROOTFS_METADATA"), Some("1"));
}

#[cfg(windows)]
#[test]
fn test_windows_box_enables_host_control_without_published_ports() {
    let dir = tempdir().unwrap();
    let layout = test_layout(dir.path(), Some(test_oci_config(None, None)), true);
    let mut vm = test_vm_manager(BoxConfig::default());

    let spec = vm.build_instance_spec(&layout).unwrap();

    assert!(spec.port_map.is_empty());
    assert_eq!(env_value(&spec, "BOX_WINDOWS_PORT_FWD"), Some("1"));
}

#[test]
fn test_run_path_plumbs_cpu_cgroup_limits_to_guest() {
    // The `run` boot path must hand the CPU cgroup limits to guest-init as
    // A3S_SEC_CPU_* (the same vars the CRI path emits and the guest consumes),
    // so `run --cpu-quota/--cpu-shares` is actually enforced in-guest instead
    // of silently dropped.
    let temp = tempdir().unwrap();
    let mut config = BoxConfig::default();
    config.resource_limits.cpu_quota = Some(50_000);
    config.resource_limits.cpu_period = Some(100_000);
    config.resource_limits.cpu_shares = Some(512);
    config.resource_limits.pids_limit = Some(100);

    let mut vm = test_vm_manager(config);
    let layout = test_layout(temp.path(), Some(test_oci_config(None, None)), true);
    let spec = vm.build_instance_spec(&layout).unwrap();

    assert_eq!(env_value(&spec, "A3S_SEC_CPU_QUOTA"), Some("50000"));
    assert_eq!(env_value(&spec, "A3S_SEC_CPU_PERIOD"), Some("100000"));
    assert_eq!(env_value(&spec, "A3S_SEC_CPU_SHARES"), Some("512"));
    assert_eq!(env_value(&spec, "A3S_SEC_PIDS_LIMIT"), Some("100"));
}

#[test]
fn test_run_path_plumbs_memory_reservation_and_swap_to_guest() {
    // --memory-reservation (memory.low) and --memory-swap (memory.swap.max)
    // must reach guest-init as A3S_SEC_MEM_LOW / A3S_SEC_MEM_SWAP so the
    // in-guest cgroup enforces them (the broken host path was removed).
    let temp = tempdir().unwrap();
    let mut config = BoxConfig::default();
    config.resource_limits.memory_reservation = Some(256 * 1024 * 1024);
    config.resource_limits.memory_swap = Some(-1);

    let mut vm = test_vm_manager(config);
    let layout = test_layout(temp.path(), Some(test_oci_config(None, None)), true);
    let spec = vm.build_instance_spec(&layout).unwrap();

    assert_eq!(env_value(&spec, "A3S_SEC_MEM_LOW"), Some("268435456"));
    assert_eq!(env_value(&spec, "A3S_SEC_MEM_SWAP"), Some("-1"));
    // The hard --memory limit is VM-sized, not an in-guest memory.max.
    assert_eq!(env_value(&spec, "A3S_SEC_MEM_LIMIT"), None);
}

#[test]
fn test_run_path_omits_cpu_limits_when_unset_or_unlimited() {
    // No limits set, plus an explicit unlimited quota (-1): nothing should be
    // emitted, so the guest leaves cpu.max at "max".
    let temp = tempdir().unwrap();
    let mut config = BoxConfig::default();
    config.resource_limits.cpu_quota = Some(-1);
    config.resource_limits.cpu_period = Some(100_000);

    let mut vm = test_vm_manager(config);
    let layout = test_layout(temp.path(), Some(test_oci_config(None, None)), true);
    let spec = vm.build_instance_spec(&layout).unwrap();

    assert!(
        !spec
            .entrypoint
            .env
            .iter()
            .any(|(k, _)| k.starts_with("A3S_SEC_CPU_")),
        "no A3S_SEC_CPU_* must be emitted for an unset/unlimited quota"
    );
}

#[test]
fn test_parse_volume_mount_host_guest() {
    let temp = TempDir::new().unwrap();
    let host_path = temp.path().to_str().unwrap();
    let volume = format!("{}:/data", host_path);

    let mount = VmManager::parse_volume_mount(&volume, 0, std::path::Path::new("/tmp")).unwrap();
    assert_eq!(mount.tag, "vol0");
    assert_eq!(mount.host_path, temp.path().canonicalize().unwrap());
    assert!(!mount.read_only);
}

#[test]
fn test_parse_volume_spec_rejects_runtime_control_namespace() {
    for guest_path in [
        "/run",
        "/run/a3s-box",
        "/run/a3s-box/boot",
        "/run//a3s-box/./boot/config.json",
    ] {
        let error = VmManager::parse_volume_spec(&format!("/tmp/data:{guest_path}"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("reserved runtime state"), "{error}");
    }

    let error = VmManager::parse_volume_spec("/tmp/data:/srv/../run/a3s-box")
        .unwrap_err()
        .to_string();
    assert!(error.contains("Invalid guest volume path"), "{error}");
}

#[test]
fn test_parse_volume_mount_read_only() {
    let temp = TempDir::new().unwrap();
    let host_path = temp.path().to_str().unwrap();
    let volume = format!("{}:/data:ro", host_path);

    let mount = VmManager::parse_volume_mount(&volume, 1, std::path::Path::new("/tmp")).unwrap();
    assert_eq!(mount.tag, "vol1");
    assert!(mount.read_only);
}

#[test]
fn test_parse_volume_mount_explicit_rw() {
    let temp = TempDir::new().unwrap();
    let host_path = temp.path().to_str().unwrap();
    let volume = format!("{}:/data:rw", host_path);

    let mount = VmManager::parse_volume_mount(&volume, 2, std::path::Path::new("/tmp")).unwrap();
    assert_eq!(mount.tag, "vol2");
    assert!(!mount.read_only);
}

#[test]
fn test_build_instance_spec_marks_named_volume_for_copy_up() {
    let home = tempdir().unwrap();
    let layout_dir = tempdir().unwrap();
    let layout = test_layout(layout_dir.path(), Some(test_oci_config(None, None)), true);
    let store = crate::volume::VolumeStore::new(
        home.path().join("volumes.json"),
        home.path().join("volumes"),
    );
    let volume = store
        .create(a3s_box_core::volume::VolumeConfig::new("data", ""))
        .unwrap();
    let mut vm = test_vm_manager(BoxConfig {
        volumes: vec![format!("{}:/data", volume.mount_point)],
        ..Default::default()
    });
    vm.home_dir = home.path().to_path_buf();

    let spec = vm.build_instance_spec(&layout).unwrap();

    assert_eq!(env_value(&spec, "BOX_VOL_0"), Some("vol0:/data:copy"));
}

#[test]
fn test_parse_volume_spec_preserves_windows_drive_path() {
    for (volume, host) in [
        (r"C:\Users\Temp:/data:ro", r"C:\Users\Temp"),
        (r"C:/Users/Temp:/data:ro", r"C:/Users/Temp"),
    ] {
        let parsed = VmManager::parse_volume_spec(volume).unwrap();

        assert_eq!(parsed.host_path, PathBuf::from(host));
        assert_eq!(parsed.guest_path, "/data");
        assert!(parsed.read_only);
    }
}

#[test]
fn test_parse_volume_spec_preserves_windows_unc_path() {
    let parsed = VmManager::parse_volume_spec(r"\\server\share\folder:/workspace:rw").unwrap();

    assert_eq!(parsed.host_path, PathBuf::from(r"\\server\share\folder"));
    assert_eq!(parsed.guest_path, "/workspace");
    assert!(!parsed.read_only);
}

#[cfg(target_os = "windows")]
#[test]
fn test_build_instance_spec_windows_bind_uses_linux_guest_target() {
    let home = tempdir().unwrap();
    let host = tempdir().unwrap();
    let layout_dir = tempdir().unwrap();
    let mut oci_config = test_oci_config(None, None);
    oci_config.volumes = vec!["/tests".to_string()];
    let layout = test_layout(layout_dir.path(), Some(oci_config), true);
    let mut vm = test_vm_manager(BoxConfig {
        volumes: vec![format!(r"{}:/tests:ro", host.path().display())],
        ..Default::default()
    });
    vm.home_dir = home.path().to_path_buf();

    let spec = vm.build_instance_spec(&layout).unwrap();

    assert_eq!(env_value(&spec, "BOX_VOL_0"), Some("vol0:/tests:ro"));
    assert!(
        vm.anonymous_volumes.is_empty(),
        "the user bind must cover the matching OCI volume"
    );
    assert_eq!(spec.fs_mounts.len(), 2);
}

#[test]
fn test_parse_volume_mount_single_file_is_staged_as_dir() {
    let temp = TempDir::new().unwrap();
    // A real source FILE (not a directory).
    let src = temp.path().join("hostfile.txt");
    std::fs::write(&src, b"DATA").unwrap();
    let stage_base = temp.path().join("filemounts");
    let volume = format!("{}:/etc/myconf", src.display());

    let mount = VmManager::parse_volume_mount(&volume, 3, &stage_base).unwrap();

    // virtio-fs shares directories, so host_path must be the staging DIR, not
    // the bare file.
    assert!(
        mount.host_path.is_dir(),
        "single-file bind must be staged into a directory, got {}",
        mount.host_path.display()
    );
    // The staged dir holds the file under the GUEST basename — what the guest
    // binds onto the guest path.
    let staged = mount.host_path.join("myconf");
    assert!(
        staged.exists(),
        "staged file under guest basename must exist"
    );
    assert_eq!(std::fs::read(&staged).unwrap(), b"DATA");
}

#[cfg(target_os = "linux")]
#[test]
fn managed_secret_single_file_staging_never_uses_durable_box_directory() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let transient = TempDir::new().unwrap();
    let durable = TempDir::new().unwrap();
    let identity = "a".repeat(64);
    let secret_dir = transient.path().join(&identity);
    std::fs::create_dir_all(&secret_dir).unwrap();
    let source = secret_dir.join("000.secret");
    std::fs::write(&source, b"TRANSIENT").unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o400)).unwrap();
    let parsed = VmManager::parse_volume_spec(&format!(
        "{}:/.a3s-box-secrets/{identity}/000.secret:ro",
        source.display()
    ))
    .unwrap();

    let mount = VmManager::prepare_volume_mount(
        &parsed,
        0,
        durable.path(),
        Some(transient.path()),
        "compose-box-id",
    )
    .unwrap();

    assert!(mount.host_path.starts_with(&secret_dir));
    assert!(!mount.host_path.starts_with(durable.path()));
    let staged = mount.host_path.join("000.secret");
    assert_eq!(std::fs::read(&staged).unwrap(), b"TRANSIENT");
    let metadata = std::fs::metadata(&staged).unwrap();
    assert_eq!(metadata.mode() & 0o777, 0o400);
    assert_eq!(metadata.nlink(), 1);
}

#[test]
fn missing_managed_secret_source_is_never_created_as_a_directory() {
    let transient = TempDir::new().unwrap();
    let durable = TempDir::new().unwrap();
    let identity = "a".repeat(64);
    let source = transient.path().join(identity).join("000.secret");
    let parsed = ParsedVolumeMount {
        host_path: source.clone(),
        guest_path: "/.a3s-box-secrets/missing/000.secret".into(),
        read_only: true,
        copy_up: false,
    };

    let error = VmManager::prepare_volume_mount(
        &parsed,
        0,
        durable.path(),
        Some(transient.path()),
        "compose-box-id",
    )
    .unwrap_err();

    assert!(error.to_string().contains("Secret source is missing"));
    assert!(!source.exists());
}

#[test]
fn managed_secret_source_must_be_read_only() {
    let transient = TempDir::new().unwrap();
    let durable = TempDir::new().unwrap();
    let identity = "a".repeat(64);
    let secret_dir = transient.path().join(identity);
    std::fs::create_dir_all(&secret_dir).unwrap();
    let source = secret_dir.join("000.secret");
    std::fs::write(&source, b"TRANSIENT").unwrap();
    let parsed = ParsedVolumeMount {
        host_path: source,
        guest_path: "/.a3s-box-secrets/managed/000.secret".into(),
        read_only: false,
        copy_up: false,
    };

    let error = VmManager::prepare_volume_mount(
        &parsed,
        0,
        durable.path(),
        Some(transient.path()),
        "compose-box-id",
    )
    .unwrap_err();

    assert!(error.to_string().contains("read-only regular files"));
}

#[test]
fn test_parse_volume_mount_invalid_mode() {
    let temp = TempDir::new().unwrap();
    let host_path = temp.path().to_str().unwrap();
    let volume = format!("{}:/data:invalid", host_path);

    let result = VmManager::parse_volume_mount(&volume, 0, std::path::Path::new("/tmp"));
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("Invalid volume mode"));
}

#[test]
fn test_parse_volume_mount_invalid_format() {
    let result = VmManager::parse_volume_mount("invalid", 0, std::path::Path::new("/tmp"));
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("Invalid volume format"));
}

#[test]
fn test_parse_volume_mount_creates_missing_dir() {
    let temp = TempDir::new().unwrap();
    let host_path = temp.path().join("nonexistent");
    let volume = format!("{}:/data", host_path.display());

    assert!(!host_path.exists());
    let mount = VmManager::parse_volume_mount(&volume, 0, std::path::Path::new("/tmp")).unwrap();
    assert!(host_path.exists());
    assert_eq!(mount.host_path, host_path.canonicalize().unwrap());
}

#[test]
fn test_resolve_oci_entrypoint_with_entrypoint_and_cmd() {
    let config = OciImageConfig {
        entrypoint: Some(vec!["/bin/app".to_string()]),
        cmd: Some(vec!["--flag".to_string()]),
        env: vec![],
        working_dir: None,
        user: None,
        exposed_ports: vec![],
        labels: std::collections::HashMap::new(),
        volumes: vec![],
        stop_signal: None,
        health_check: None,
        onbuild: vec![],
    };

    let (exec, args) = VmManager::resolve_oci_entrypoint(&config, &[], None);
    assert_eq!(exec, "/bin/app");
    assert_eq!(args, vec!["--flag"]);
}

#[test]
fn test_resolve_oci_entrypoint_cmd_only() {
    let config = OciImageConfig {
        entrypoint: None,
        cmd: Some(vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "echo hi".to_string(),
        ]),
        env: vec![],
        working_dir: None,
        user: None,
        exposed_ports: vec![],
        labels: std::collections::HashMap::new(),
        volumes: vec![],
        stop_signal: None,
        health_check: None,
        onbuild: vec![],
    };

    let (exec, args) = VmManager::resolve_oci_entrypoint(&config, &[], None);
    assert_eq!(exec, "/bin/sh");
    assert_eq!(args, vec!["-c", "echo hi"]);
}

#[test]
fn test_resolve_oci_entrypoint_neither() {
    let config = OciImageConfig {
        entrypoint: None,
        cmd: None,
        env: vec![],
        working_dir: None,
        user: None,
        exposed_ports: vec![],
        labels: std::collections::HashMap::new(),
        volumes: vec![],
        stop_signal: None,
        health_check: None,
        onbuild: vec![],
    };

    let (exec, _args) = VmManager::resolve_oci_entrypoint(&config, &[], None);
    assert_eq!(exec, "/bin/sh");
}

#[test]
fn test_resolve_oci_entrypoint_cmd_override() {
    let config = OciImageConfig {
        entrypoint: None,
        cmd: Some(vec!["/bin/sh".to_string()]),
        env: vec![],
        working_dir: None,
        user: None,
        exposed_ports: vec![],
        labels: std::collections::HashMap::new(),
        volumes: vec![],
        stop_signal: None,
        health_check: None,
        onbuild: vec![],
    };

    let override_cmd = vec!["sleep".to_string(), "3600".to_string()];
    let (exec, args) = VmManager::resolve_oci_entrypoint(&config, &override_cmd, None);
    assert_eq!(exec, "sleep");
    assert_eq!(args, vec!["3600"]);
}

#[test]
fn test_resolve_oci_entrypoint_with_override() {
    let config = OciImageConfig {
        entrypoint: Some(vec!["/bin/app".to_string()]),
        cmd: Some(vec!["--flag".to_string()]),
        env: vec![],
        working_dir: None,
        user: None,
        exposed_ports: vec![],
        labels: std::collections::HashMap::new(),
        volumes: vec![],
        stop_signal: None,
        health_check: None,
        onbuild: vec![],
    };

    // Override replaces the image entrypoint entirely
    let override_ep = vec!["/bin/sh".to_string(), "-c".to_string()];
    let (exec, args) = VmManager::resolve_oci_entrypoint(&config, &[], Some(&override_ep));
    assert_eq!(exec, "/bin/sh");
    // args = entrypoint[1:] + cmd
    assert_eq!(args, vec!["-c", "--flag"]);
}

#[test]
fn test_resolve_oci_entrypoint_override_with_cmd_override() {
    let config = OciImageConfig {
        entrypoint: Some(vec!["/bin/app".to_string()]),
        cmd: Some(vec!["--flag".to_string()]),
        env: vec![],
        working_dir: None,
        user: None,
        exposed_ports: vec![],
        labels: std::collections::HashMap::new(),
        volumes: vec![],
        stop_signal: None,
        health_check: None,
        onbuild: vec![],
    };

    // Both entrypoint and cmd overridden
    let override_ep = vec!["/bin/sh".to_string()];
    let cmd_override = vec!["echo".to_string(), "hello".to_string()];
    let (exec, args) =
        VmManager::resolve_oci_entrypoint(&config, &cmd_override, Some(&override_ep));
    assert_eq!(exec, "/bin/sh");
    assert_eq!(args, vec!["echo", "hello"]);
}

#[test]
fn test_resolve_config_entrypoint_preserves_overrides() {
    let entrypoint = vec!["/custom".to_string(), "--flag".to_string()];
    let cmd = vec!["echo".to_string(), "restored".to_string()];

    let (exec, args) = VmManager::resolve_config_entrypoint(&cmd, Some(&entrypoint));

    assert_eq!(exec, "/custom");
    assert_eq!(args, vec!["--flag", "echo", "restored"]);
}
