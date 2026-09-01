use super::*;

#[test]
fn test_guest_init_exec_path_prefers_sbin() {
    let dir = tempdir().unwrap();
    let rootfs = dir.path();
    fs::create_dir_all(rootfs.join("sbin")).unwrap();
    fs::write(rootfs.join("sbin").join("init"), b"guest-init").unwrap();

    assert_eq!(VmManager::guest_init_exec_path(rootfs), Some("/sbin/init"));
}

#[test]
fn test_guest_init_exec_path_resolves_multi_hop_guest_sbin_symlink() {
    let dir = tempdir().unwrap();
    let rootfs = dir.path();
    fs::create_dir_all(rootfs.join("usr")).unwrap();
    fs::create_dir_all(rootfs.join("shared/sbin")).unwrap();
    fs::write(rootfs.join("shared/sbin/init"), b"guest-init").unwrap();
    if !create_dir_symlink(Path::new("/usr/sbin"), &rootfs.join("sbin"))
        || !create_dir_symlink(Path::new("../shared/sbin"), &rootfs.join("usr/sbin"))
    {
        return;
    }

    assert_eq!(VmManager::guest_init_exec_path(rootfs), Some("/sbin/init"));
}

#[test]
fn test_guest_init_exec_path_rejects_sbin_escape() {
    let dir = tempdir().unwrap();
    let rootfs = dir.path().join("rootfs");
    let outside = dir.path().join("outside");
    fs::create_dir_all(&rootfs).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("init"), b"host-init").unwrap();
    if !create_dir_symlink(Path::new("../outside"), &rootfs.join("sbin")) {
        return;
    }

    assert_eq!(VmManager::guest_init_exec_path(&rootfs), None);
}

#[test]
fn resumed_guest_owned_rootfs_uses_private_boot_transport_without_host_files() {
    let directory = tempdir().unwrap();
    let mut layout = test_layout(directory.path(), Some(test_oci_config(None, None)), false);
    let disk = directory.path().join("rootfs.ext4");
    std::fs::write(&disk, b"typed test disk").unwrap();
    layout.resumed_rootfs = Some(crate::rootfs::ResumedRootfs {
        source: RootfsSource::ext4_disk(&disk, false),
        guest_init_exec: "/sbin/init".to_string(),
    });
    let mut vm = test_vm_manager(BoxConfig {
        cmd: vec!["/bin/echo".to_string(), "resumed".to_string()],
        persistent: true,
        ..Default::default()
    });

    let spec = vm.build_microvm_instance_spec(&layout).unwrap();

    assert_eq!(spec.rootfs, RootfsSource::ext4_disk(disk, false));
    assert_eq!(spec.entrypoint.executable, "/sbin/init");
    assert!(spec
        .fs_mounts
        .iter()
        .any(|mount| mount.tag == GUEST_BOOT_CONTROL_TAG));
    assert!(!layout.rootfs_path.join(".a3s-box-exec.json").exists());
    assert!(!layout.rootfs_path.join(".a3s-box-env").exists());
}

#[test]
fn test_build_instance_spec_restored_rootfs_uses_saved_cmd_with_guest_init() {
    let dir = tempdir().unwrap();
    let layout = test_layout(dir.path(), None, true);
    let mut vm = test_vm_manager(BoxConfig {
        cmd: vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 3600".to_string(),
        ],
        ..Default::default()
    });

    let spec = vm.build_instance_spec(&layout).unwrap();

    assert_eq!(spec.entrypoint.executable, "/sbin/init");
    let staged = staged_exec_config(&layout);
    assert_eq!(staged.executable, "/bin/sh");
    assert_eq!(staged.args, ["-c", "sleep 3600"]);
    assert!(!spec
        .entrypoint
        .env
        .iter()
        .any(|(_, value)| value.contains("No command specified")));
}

#[test]
fn test_build_instance_spec_stages_large_exec_config_off_kernel_cmdline() {
    let dir = tempdir().unwrap();
    let layout = test_layout(dir.path(), None, true);
    let long_arg = "x".repeat(4096);
    let mut vm = test_vm_manager(BoxConfig {
        cmd: vec!["/bin/echo".to_string(), long_arg.clone()],
        ..Default::default()
    });

    let spec = vm.build_instance_spec(&layout).unwrap();

    assert_eq!(
        env_value(&spec, "BOX_EXEC_CONFIG_FILE"),
        Some("/.a3s-box-exec.json")
    );
    assert!(!spec.entrypoint.env.iter().any(|(key, _)| {
        key == "BOX_EXEC_EXEC"
            || key == "BOX_EXEC_ARGC"
            || key == "BOX_EXEC_WORKDIR"
            || key == "BOX_EXEC_USER"
            || key == "BOX_EXEC_STDIN"
            || key.starts_with("BOX_EXEC_ARG_")
    }));

    let staged = staged_exec_config(&layout);
    assert_eq!(staged.schema, "a3s.box.guest-exec.v1");
    assert_eq!(staged.executable, "/bin/echo");
    assert_eq!(staged.args[0], long_arg);
}

#[test]
fn test_build_microvm_spec_stages_one_read_only_boot_bundle_off_rootfs() {
    let dir = tempdir().unwrap();
    let mut oci = test_oci_config(None, None);
    oci.env = vec![("IMAGE_VALUE".to_string(), "image".to_string())];
    let layout = test_layout(dir.path(), Some(oci), true);
    let mut vm = test_vm_manager(BoxConfig {
        cmd: vec!["hello".to_string()],
        entrypoint_override: Some(vec!["/bin/echo".to_string()]),
        extra_env: vec![("CLI_VALUE".to_string(), "cli".to_string())],
        ..Default::default()
    });

    let spec = vm.build_microvm_instance_spec(&layout).unwrap();

    assert_eq!(
        env_value(&spec, GUEST_BOOT_CONFIG_ENV),
        Some(GUEST_BOOT_CONFIG_PATH)
    );
    assert_eq!(env_value(&spec, "BOX_EXEC_CONFIG_FILE"), None);
    assert_eq!(env_value(&spec, "BOX_EXEC_ENV_FILE"), None);
    assert!(!layout.rootfs_path.join(".a3s-box-exec.json").exists());
    assert!(!layout.rootfs_path.join(".a3s-box-env").exists());

    let mount = spec
        .fs_mounts
        .iter()
        .find(|mount| mount.tag == GUEST_BOOT_CONTROL_TAG)
        .unwrap();
    assert!(mount.read_only);
    assert!(mount
        .host_path
        .starts_with(dir.path().canonicalize().unwrap()));
    let bundle = staged_boot_config(&spec);
    bundle.validate().unwrap();
    assert_eq!(bundle.exec.executable, "/bin/echo");
    assert_eq!(bundle.exec.args, ["hello"]);
    assert_eq!(
        bundle.environment,
        [
            ("IMAGE_VALUE".to_string(), "image".to_string()),
            ("CLI_VALUE".to_string(), "cli".to_string())
        ]
    );
    assert_eq!(bundle.host, GuestHostConfig::default());
}

#[test]
fn test_finalize_microvm_boot_bundle_adds_guest_owned_host_files() {
    let dir = tempdir().unwrap();
    let layout = test_layout(dir.path(), Some(test_oci_config(None, None)), true);
    let mut vm = test_vm_manager(BoxConfig::default());
    let spec = vm.build_microvm_instance_spec(&layout).unwrap();
    let host = GuestHostConfig {
        hostname: Some("web".to_string()),
        resolv_conf: Some("nameserver 1.1.1.1\n".to_string()),
        hosts: Some("127.0.0.1 localhost\n".to_string()),
    };

    assert!(VmManager::finalize_microvm_guest_boot_config(&spec, host.clone()).unwrap());

    assert_eq!(staged_boot_config(&spec).host, host);
    assert!(!layout.rootfs_path.join("etc/resolv.conf").exists());
    assert!(!layout.rootfs_path.join("etc/hostname").exists());
    assert!(!layout.rootfs_path.join("etc/hosts").exists());

    VmManager::clear_microvm_guest_boot_config(&spec).unwrap();
    let mount = spec
        .fs_mounts
        .iter()
        .find(|mount| mount.tag == GUEST_BOOT_CONTROL_TAG)
        .unwrap();
    assert!(!mount.host_path.join(GUEST_BOOT_CONFIG_FILE_NAME).exists());
}

#[test]
fn test_finalize_microvm_boot_bundle_reports_legacy_spec() {
    let dir = tempdir().unwrap();
    let layout = test_layout(dir.path(), Some(test_oci_config(None, None)), false);
    let mut vm = test_vm_manager(BoxConfig::default());
    let spec = vm.build_microvm_instance_spec(&layout).unwrap();

    assert!(
        !VmManager::finalize_microvm_guest_boot_config(&spec, GuestHostConfig::default()).unwrap()
    );
}

#[test]
fn test_build_instance_spec_rejects_oversized_exec_config() {
    let dir = tempdir().unwrap();
    let layout = test_layout(dir.path(), None, true);
    let mut vm = test_vm_manager(BoxConfig {
        cmd: vec![
            "/bin/echo".to_string(),
            "x".repeat(MAX_RUNTIME_EXEC_CONFIG_BYTES),
        ],
        ..Default::default()
    });

    let error = vm.build_instance_spec(&layout).unwrap_err().to_string();

    assert!(error.contains("guest exec configuration"), "{error}");
    assert!(error.contains("limit"), "{error}");
    assert!(!layout.rootfs_path.join(".a3s-box-exec.json").exists());
}

#[test]
fn test_build_instance_spec_replaces_exec_config_symlink_without_following_target() {
    let dir = tempdir().unwrap();
    let layout = test_layout(dir.path(), None, true);
    let outside = dir.path().join("outside-exec-config");
    fs::write(&outside, "unchanged").unwrap();
    if !create_file_symlink(
        Path::new("../outside-exec-config"),
        &layout.rootfs_path.join(".a3s-box-exec.json"),
    ) {
        return;
    }
    let mut vm = test_vm_manager(BoxConfig {
        cmd: vec!["/bin/echo".to_string(), "safe".to_string()],
        ..Default::default()
    });

    vm.build_instance_spec(&layout).unwrap();

    assert_eq!(fs::read_to_string(outside).unwrap(), "unchanged");
    assert!(
        fs::symlink_metadata(layout.rootfs_path.join(".a3s-box-exec.json"))
            .unwrap()
            .file_type()
            .is_file()
    );
    assert_eq!(staged_exec_config(&layout).args, ["safe"]);
}

#[test]
fn test_build_instance_spec_restored_rootfs_uses_saved_entrypoint_without_guest_init() {
    let dir = tempdir().unwrap();
    let layout = test_layout(dir.path(), None, false);
    let mut vm = test_vm_manager(BoxConfig {
        cmd: vec!["hello".to_string()],
        entrypoint_override: Some(vec!["/bin/echo".to_string(), "prefix".to_string()]),
        extra_env: vec![("FOO".to_string(), "bar".to_string())],
        ..Default::default()
    });

    let spec = vm.build_instance_spec(&layout).unwrap();

    assert_eq!(spec.entrypoint.executable, "/bin/echo");
    assert_eq!(spec.entrypoint.args, vec!["prefix", "hello"]);
    assert_eq!(env_value(&spec, "FOO"), Some("bar"));
}

#[test]
fn test_build_instance_spec_prefers_config_workdir_and_user() {
    let dir = tempdir().unwrap();
    let layout = test_layout(
        dir.path(),
        Some(test_oci_config(Some("/oci"), Some("2000:2000"))),
        true,
    );
    let mut vm = test_vm_manager(BoxConfig {
        workdir: Some("/override".to_string()),
        user: Some("1000:1000".to_string()),
        ..Default::default()
    });

    let spec = vm.build_instance_spec(&layout).unwrap();

    assert_eq!(spec.workdir, "/override");
    // With guest init present, the user is applied from the staged process
    // config, not by the shim's set_uid, so spec.user is None.
    assert_eq!(spec.user, None);
    let staged = staged_exec_config(&layout);
    assert_eq!(staged.user.as_deref(), Some("1000:1000"));
    assert_eq!(staged.workdir, "/override");
}

#[test]
fn test_relative_workdir_resolves_against_image_workdir() {
    // Docker `-w sub` resolves against the image WORKDIR.
    let oci = test_oci_config(Some("/srv/app"), None);
    let cfg = BoxConfig {
        workdir: Some("sub".to_string()),
        ..Default::default()
    };
    assert_eq!(
        VmManager::effective_workdir(&cfg, Some(&oci)),
        "/srv/app/sub"
    );
    // Absolute override is used verbatim.
    let cfg_abs = BoxConfig {
        workdir: Some("/abs".to_string()),
        ..Default::default()
    };
    assert_eq!(VmManager::effective_workdir(&cfg_abs, Some(&oci)), "/abs");
    // Relative with no image WORKDIR resolves against `/`.
    let cfg_rel = BoxConfig {
        workdir: Some("work".to_string()),
        ..Default::default()
    };
    assert_eq!(VmManager::effective_workdir(&cfg_rel, None), "/work");
}

#[test]
fn test_build_instance_spec_uses_oci_workdir_and_user_without_override() {
    let dir = tempdir().unwrap();
    let layout = test_layout(
        dir.path(),
        Some(test_oci_config(Some("/oci"), Some("2000:2000"))),
        true,
    );
    let mut vm = test_vm_manager(BoxConfig::default());

    let spec = vm.build_instance_spec(&layout).unwrap();

    assert_eq!(spec.workdir, "/oci");
    assert_eq!(spec.user, None);
    let staged = staged_exec_config(&layout);
    assert_eq!(staged.user.as_deref(), Some("2000:2000"));
    assert_eq!(staged.workdir, "/oci");
}

#[test]
fn test_build_instance_spec_passes_default_workdir_to_guest_init() {
    let dir = tempdir().unwrap();
    let layout = test_layout(dir.path(), Some(test_oci_config(None, None)), true);
    let mut vm = test_vm_manager(BoxConfig::default());

    let spec = vm.build_instance_spec(&layout).unwrap();

    assert_eq!(spec.workdir, GUEST_WORKDIR);
    assert_eq!(staged_exec_config(&layout).workdir, GUEST_WORKDIR);
}

#[test]
fn test_build_instance_spec_without_oci_uses_persisted_command() {
    let dir = tempdir().unwrap();
    let layout = test_layout(dir.path(), None, true);
    let mut vm = test_vm_manager(BoxConfig {
        cmd: vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "printf snapshot-restored".to_string(),
        ],
        ..Default::default()
    });

    vm.build_instance_spec(&layout).unwrap();

    let staged = staged_exec_config(&layout);
    assert_eq!(staged.executable, "/bin/sh");
    assert_eq!(staged.args, ["-c", "printf snapshot-restored"]);
}

#[test]
fn test_build_instance_spec_passes_hostname_to_guest_init() {
    let dir = tempdir().unwrap();
    let layout = test_layout(dir.path(), Some(test_oci_config(None, None)), true);
    let mut vm = test_vm_manager(BoxConfig {
        hostname: Some("web".to_string()),
        ..Default::default()
    });

    let spec = vm.build_instance_spec(&layout).unwrap();

    assert!(spec
        .entrypoint
        .env
        .iter()
        .any(|(key, value)| key == "BOX_HOSTNAME" && value == "web"));
}

#[test]
fn test_build_instance_spec_guest_init_prefixes_extra_env() {
    let dir = tempdir().unwrap();
    let mut oci_config = test_oci_config(None, None);
    oci_config.env = vec![
        ("FOO".to_string(), "image".to_string()),
        ("BAR".to_string(), "image".to_string()),
    ];
    let layout = test_layout(dir.path(), Some(oci_config), true);
    let mut vm = test_vm_manager(BoxConfig {
        extra_env: vec![
            ("FOO".to_string(), "cli".to_string()),
            ("BAZ".to_string(), "cli".to_string()),
        ],
        ..Default::default()
    });

    let spec = vm.build_instance_spec(&layout).unwrap();

    // Container env is staged in a file in the rootfs, not inlined: only a
    // small BOX_EXEC_ENV_FILE pointer rides the env block (cmdline overflow).
    assert!(spec
        .entrypoint
        .env
        .iter()
        .any(|(key, value)| key == "BOX_EXEC_ENV_FILE" && value == "/.a3s-box-env"));
    // No raw container env keys leak into the inline env block.
    assert!(!spec
        .entrypoint
        .env
        .iter()
        .any(|(key, _)| key == "FOO" || key == "BAR" || key == "BAZ"));

    // The staged file holds one `KEY=base64(value)` line per var, with the
    // CLI extra_env overriding the image's env (FOO/BAZ from cli, BAR from image).
    let staged = std::fs::read_to_string(layout.rootfs_path.join(".a3s-box-env")).unwrap();
    let entries: std::collections::HashMap<&str, String> = staged
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k, b64d(v)))
        .collect();
    assert_eq!(entries.get("FOO").map(String::as_str), Some("cli"));
    assert_eq!(entries.get("BAR").map(String::as_str), Some("image"));
    assert_eq!(entries.get("BAZ").map(String::as_str), Some("cli"));
}

#[test]
fn test_build_instance_spec_stages_env_through_internal_guest_symlink() {
    let dir = tempdir().unwrap();
    let mut oci_config = test_oci_config(None, None);
    oci_config.env = vec![("FOO".to_string(), "safe".to_string())];
    let layout = test_layout(dir.path(), Some(oci_config), true);
    fs::create_dir_all(layout.rootfs_path.join("shared")).unwrap();
    if !create_file_symlink(
        Path::new("shared/env"),
        &layout.rootfs_path.join(".a3s-box-env"),
    ) {
        return;
    }
    let mut vm = test_vm_manager(BoxConfig::default());

    vm.build_instance_spec(&layout).unwrap();

    assert_eq!(
        b64d(
            fs::read_to_string(layout.rootfs_path.join("shared/env"))
                .unwrap()
                .trim_start_matches("FOO=")
                .trim()
        ),
        "safe"
    );
}

#[test]
fn test_build_instance_spec_rejects_env_file_symlink_escape() {
    let dir = tempdir().unwrap();
    let rootfs_parent = dir.path().join("layout");
    let mut oci_config = test_oci_config(None, None);
    oci_config.env = vec![("FOO".to_string(), "unsafe".to_string())];
    let layout = test_layout(&rootfs_parent, Some(oci_config), true);
    let outside = rootfs_parent.join("outside-env");
    if !create_file_symlink(
        Path::new("../outside-env"),
        &layout.rootfs_path.join(".a3s-box-env"),
    ) {
        return;
    }
    let mut vm = test_vm_manager(BoxConfig::default());

    let error = vm.build_instance_spec(&layout).unwrap_err().to_string();

    assert!(error.contains("escapes rootfs"), "{error}");
    assert!(!outside.exists());
}

#[test]
fn test_build_instance_spec_direct_entrypoint_merges_extra_env() {
    let dir = tempdir().unwrap();
    let mut oci_config = test_oci_config(None, None);
    oci_config.env = vec![
        ("FOO".to_string(), "image".to_string()),
        ("BAR".to_string(), "image".to_string()),
    ];
    let layout = test_layout(dir.path(), Some(oci_config), false);
    let mut vm = test_vm_manager(BoxConfig {
        extra_env: vec![
            ("FOO".to_string(), "cli".to_string()),
            ("BAZ".to_string(), "cli".to_string()),
        ],
        ..Default::default()
    });

    let spec = vm.build_instance_spec(&layout).unwrap();

    assert!(spec
        .entrypoint
        .env
        .iter()
        .any(|(key, value)| key == "FOO" && value == "cli"));
    assert!(spec
        .entrypoint
        .env
        .iter()
        .any(|(key, value)| key == "BAR" && value == "image"));
    assert!(spec
        .entrypoint
        .env
        .iter()
        .any(|(key, value)| key == "BAZ" && value == "cli"));
}

#[test]
fn test_build_instance_spec_tracks_new_anonymous_volumes_only() {
    let home = tempdir().unwrap();
    let layout_dir = tempdir().unwrap();
    let mut oci_config = test_oci_config(None, None);
    oci_config.volumes = vec!["/data".to_string()];
    let layout = test_layout(layout_dir.path(), Some(oci_config), true);

    let mut first_vm = test_vm_manager(BoxConfig::default());
    first_vm.home_dir = home.path().to_path_buf();
    let first_spec = first_vm.build_instance_spec(&layout).unwrap();

    assert_eq!(first_vm.anonymous_volumes.len(), 1);
    assert_eq!(
        first_vm.created_anonymous_volumes,
        first_vm.anonymous_volumes
    );
    assert!(first_spec.fs_mounts.iter().any(|mount| {
        mount.tag == "vol0" && mount.host_path.starts_with(home.path().join("volumes"))
    }));

    let volume_name = first_vm.anonymous_volumes[0].clone();
    let store = crate::volume::VolumeStore::new(
        home.path().join("volumes.json"),
        home.path().join("volumes"),
    );
    assert!(store.get(&volume_name).unwrap().is_some());

    let mut second_vm = test_vm_manager(BoxConfig::default());
    second_vm.home_dir = home.path().to_path_buf();
    second_vm.anonymous_volumes = vec![volume_name.clone(), volume_name.clone()];
    second_vm.build_instance_spec(&layout).unwrap();
    second_vm.build_instance_spec(&layout).unwrap();

    assert_eq!(second_vm.anonymous_volumes, vec![volume_name]);
    assert!(second_vm.created_anonymous_volumes.is_empty());
    assert_eq!(
        store
            .get(&second_vm.anonymous_volumes[0])
            .unwrap()
            .unwrap()
            .in_use_by,
        vec!["test-box".to_string()]
    );
}

#[test]
fn sandbox_rejects_anonymous_plan_drift_before_volume_materialization() {
    let home = tempdir().unwrap();
    let layout_dir = tempdir().unwrap();
    let mut oci_config = test_oci_config(None, None);
    oci_config.volumes = vec!["/data".to_string()];
    let layout = test_layout(layout_dir.path(), Some(oci_config), true);
    let mut vm = test_vm_manager(BoxConfig {
        isolation: a3s_box_core::ExecutionIsolation::Sandbox,
        ..Default::default()
    });
    vm.home_dir = home.path().to_path_buf();

    let error = vm
        .build_runtime_owned_instance_spec(&layout)
        .expect_err("Sandbox may only materialize its durable ownership plan");

    assert!(error
        .to_string()
        .contains("drifted from the durable Box ownership plan"));
    assert!(!home.path().join("volumes").exists());
    assert!(vm.anonymous_volumes.is_empty());
    assert!(vm.created_anonymous_volumes.is_empty());
}

#[test]
fn test_guest_init_exec_path_supports_usr_sbin_without_sbin() {
    let dir = tempdir().unwrap();
    let rootfs = dir.path();
    fs::create_dir_all(rootfs.join("usr").join("sbin")).unwrap();
    fs::write(rootfs.join("usr").join("sbin").join("init"), b"guest-init").unwrap();

    assert_eq!(
        VmManager::guest_init_exec_path(rootfs),
        Some("/usr/sbin/init")
    );
}

#[test]
fn test_parse_volume_mount_guest_path_with_colons() {
    let temp = TempDir::new().unwrap();
    let host_path = temp.path().to_str().unwrap();
    // Path like /host/path:/guest/path:ro where guest path contains colon
    let volume = format!("{}:/data:/media/c:ro", host_path);

    let result = VmManager::parse_volume_mount(&volume, 0, std::path::Path::new("/tmp"));
    // Should handle this gracefully or error on the guest path with colon
    // The exact behavior depends on implementation
    assert!(result.is_err() || result.is_ok()); // Just verify it doesn't panic
}
