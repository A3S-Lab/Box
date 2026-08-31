use super::*;

#[test]
fn test_apply_changes_cmd() {
    let mut config = serde_json::json!({"config": {}});
    apply_changes(&mut config, &["CMD /bin/bash".to_string()]);
    assert_eq!(
        config["config"]["Cmd"],
        serde_json::json!(["/bin/sh", "-c", "/bin/bash"])
    );
}

#[test]
fn test_apply_changes_entrypoint() {
    let mut config = serde_json::json!({"config": {}});
    apply_changes(&mut config, &["ENTRYPOINT /app/start".to_string()]);
    assert_eq!(
        config["config"]["Entrypoint"],
        serde_json::json!(["/bin/sh", "-c", "/app/start"])
    );
}

#[test]
fn test_apply_changes_env() {
    let mut config = serde_json::json!({"config": {}});
    apply_changes(&mut config, &["ENV FOO=bar".to_string()]);
    let env = config["config"]["Env"].as_array().unwrap();
    assert_eq!(env.len(), 1);
    assert_eq!(env[0], "FOO=bar");
}

#[test]
fn test_apply_changes_workdir() {
    let mut config = serde_json::json!({"config": {}});
    apply_changes(&mut config, &["WORKDIR /app".to_string()]);
    assert_eq!(config["config"]["WorkingDir"], "/app");
}

#[test]
fn test_apply_changes_user() {
    let mut config = serde_json::json!({"config": {}});
    apply_changes(&mut config, &["USER nobody".to_string()]);
    assert_eq!(config["config"]["User"], "nobody");
}

#[test]
fn test_apply_changes_label() {
    let mut config = serde_json::json!({"config": {}});
    apply_changes(&mut config, &["LABEL version=1.0".to_string()]);
    assert_eq!(config["config"]["Labels"]["version"], "1.0");
}

#[test]
fn test_apply_changes_expose() {
    let mut config = serde_json::json!({"config": {}});
    apply_changes(&mut config, &["EXPOSE 8080".to_string()]);
    assert!(config["config"]["ExposedPorts"]["8080/tcp"].is_object());
}

#[test]
fn test_apply_changes_multiple() {
    let mut config = serde_json::json!({"config": {}});
    apply_changes(
        &mut config,
        &[
            "CMD /start".to_string(),
            "ENV APP=test".to_string(),
            "WORKDIR /opt".to_string(),
        ],
    );
    assert!(config["config"]["Cmd"].is_array());
    assert!(config["config"]["Env"].is_array());
    assert_eq!(config["config"]["WorkingDir"], "/opt");
}

#[test]
fn test_apply_changes_empty() {
    let mut config = serde_json::json!({"config": {}});
    apply_changes(&mut config, &[]);
    assert_eq!(config["config"], serde_json::json!({}));
}

#[test]
fn test_compute_diff_id() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), "world").unwrap();
    let id = compute_diff_id(dir.path()).unwrap();
    assert!(!id.is_empty());
    assert_eq!(id.len(), 64); // sha256 hex
}

#[test]
fn test_guest_metadata_overrides_host_uid_gid_and_mode_in_tar() {
    use a3s_box_core::rootfs_metadata::{
        RootfsEntryKind, RootfsMetadataEntry, RootfsMetadataManifest,
    };
    let rootfs = tempfile::TempDir::new().unwrap();
    let file = rootfs.path().join("probe");
    std::fs::write(&file, b"payload").unwrap();
    let encoded = base64::engine::general_purpose::STANDARD.encode(b"probe");
    let manifest = RootfsMetadataManifest::new(vec![RootfsMetadataEntry {
        path_base64: encoded,
        kind: RootfsEntryKind::Regular,
        mode: 0o100755,
        uid: 0,
        gid: 0,
        mtime: 123,
        size: 7,
        link_target_base64: None,
    }]);
    let output = rootfs.path().join("rootfs.tar");

    create_tar_from_guest_metadata(rootfs.path(), &manifest, &output).unwrap();

    let mut archive = tar::Archive::new(std::fs::File::open(output).unwrap());
    let entry = archive.entries().unwrap().next().unwrap().unwrap();
    assert_eq!(entry.path().unwrap(), Path::new("probe"));
    assert_eq!(entry.header().mode().unwrap() & 0o7777, 0o755);
    assert_eq!(entry.header().uid().unwrap(), 0);
    assert_eq!(entry.header().gid().unwrap(), 0);
    assert_eq!(entry.header().mtime().unwrap(), 123);
}

#[test]
fn test_guest_metadata_preserves_hardlinks_without_duplicate_payloads() {
    use a3s_box_core::rootfs_metadata::{
        RootfsEntryKind, RootfsMetadataEntry, RootfsMetadataManifest,
    };
    let rootfs = tempfile::TempDir::new().unwrap();
    std::fs::write(rootfs.path().join("busybox"), b"payload").unwrap();
    std::fs::hard_link(rootfs.path().join("busybox"), rootfs.path().join("sh")).unwrap();
    let entries = ["busybox", "sh"]
        .into_iter()
        .map(|path| RootfsMetadataEntry {
            path_base64: base64::engine::general_purpose::STANDARD.encode(path.as_bytes()),
            kind: RootfsEntryKind::Regular,
            mode: 0o100755,
            uid: 0,
            gid: 0,
            mtime: 123,
            size: 7,
            link_target_base64: None,
        })
        .collect();
    let output = rootfs.path().join("rootfs.tar");

    create_tar_from_guest_metadata(
        rootfs.path(),
        &RootfsMetadataManifest::new(entries),
        &output,
    )
    .unwrap();

    let mut archive = tar::Archive::new(std::fs::File::open(output).unwrap());
    let mut entries = archive.entries().unwrap();
    let first = entries.next().unwrap().unwrap();
    assert_eq!(first.header().entry_type(), tar::EntryType::Regular);
    drop(first);
    let second = entries.next().unwrap().unwrap();
    assert_eq!(second.header().entry_type(), tar::EntryType::Link);
    assert_eq!(second.link_name().unwrap().unwrap(), Path::new("busybox"));
}

#[test]
fn test_guest_metadata_rejects_parent_traversal() {
    use a3s_box_core::rootfs_metadata::{
        RootfsEntryKind, RootfsMetadataEntry, RootfsMetadataManifest,
    };
    let rootfs = tempfile::TempDir::new().unwrap();
    let encoded = base64::engine::general_purpose::STANDARD.encode(b"../escape");
    let manifest = RootfsMetadataManifest::new(vec![RootfsMetadataEntry {
        path_base64: encoded,
        kind: RootfsEntryKind::Regular,
        mode: 0o100600,
        uid: 0,
        gid: 0,
        mtime: 0,
        size: 0,
        link_target_base64: None,
    }]);

    let error =
        create_tar_from_guest_metadata(rootfs.path(), &manifest, &rootfs.path().join("rootfs.tar"))
            .unwrap_err();
    assert!(error.to_string().contains("Unsafe rootfs metadata path"));
}

#[test]
fn test_guest_metadata_rejects_symlink_parent_without_reading_outside_rootfs() {
    use a3s_box_core::rootfs_metadata::{
        RootfsEntryKind, RootfsMetadataEntry, RootfsMetadataManifest,
    };

    let rootfs = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::write(outside.path().join("secret"), b"host secret").unwrap();
    let link = rootfs.path().join("escape");
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), &link).unwrap();
    #[cfg(windows)]
    if let Err(error) = std::os::windows::fs::symlink_dir(outside.path(), &link) {
        if error.raw_os_error() == Some(1314) {
            return;
        }
        panic!("failed to create directory symlink: {error}");
    }

    let manifest = RootfsMetadataManifest::new(vec![RootfsMetadataEntry {
        path_base64: base64::engine::general_purpose::STANDARD.encode(b"escape/secret"),
        kind: RootfsEntryKind::Regular,
        mode: 0o100600,
        uid: 0,
        gid: 0,
        mtime: 0,
        size: 11,
        link_target_base64: None,
    }]);
    let output = rootfs.path().join("rootfs.tar");
    let error = create_tar_from_guest_metadata(rootfs.path(), &manifest, &output).unwrap_err();

    assert!(error.to_string().contains("Link or non-directory parent"));
    assert_eq!(
        std::fs::read(outside.path().join("secret")).unwrap(),
        b"host secret"
    );
}

#[test]
fn guest_metadata_preserves_raw_linux_symlink_target_bytes() {
    use a3s_box_core::rootfs_metadata::{
        RootfsEntryKind, RootfsMetadataEntry, RootfsMetadataManifest,
    };

    let rootfs = tempfile::TempDir::new().unwrap();
    let link = rootfs.path().join("link");
    #[cfg(unix)]
    std::os::unix::fs::symlink("actual", &link).unwrap();
    #[cfg(windows)]
    if let Err(error) = std::os::windows::fs::symlink_file("actual", &link) {
        if error.raw_os_error() == Some(1314) {
            return;
        }
        panic!("failed to create file symlink: {error}");
    }

    let target = b"name\\with-backslash-\xff";
    let manifest = RootfsMetadataManifest::new(vec![RootfsMetadataEntry {
        path_base64: base64::engine::general_purpose::STANDARD.encode(b"link"),
        kind: RootfsEntryKind::Symlink,
        mode: 0o120777,
        uid: 0,
        gid: 0,
        mtime: 0,
        size: 0,
        link_target_base64: Some(base64::engine::general_purpose::STANDARD.encode(target)),
    }]);
    let output = rootfs.path().join("rootfs.tar");

    create_tar_from_guest_metadata(rootfs.path(), &manifest, &output).unwrap();

    let mut archive = tar::Archive::new(std::fs::File::open(output).unwrap());
    let entry = archive.entries().unwrap().next().unwrap().unwrap();
    let actual = entry
        .link_name_bytes()
        .expect("symlink archive entry should contain a link target");
    assert_eq!(actual.as_ref(), target);
}

#[test]
fn reserved_metadata_path_is_detected_after_curdir_normalization() {
    assert!(
        a3s_box_core::rootfs_metadata::is_runtime_internal_rootfs_path(Path::new(
            "./.a3s_rootfs_metadata_v1.json"
        ))
    );
    assert!(
        a3s_box_core::rootfs_metadata::is_runtime_internal_rootfs_path(Path::new(
            ".a3s_rootfs_metadata_v1.previous.json"
        ))
    );
    assert!(
        a3s_box_core::rootfs_metadata::is_runtime_internal_rootfs_path(Path::new("init-rust.log"))
    );
}

#[cfg(windows)]
#[test]
fn windows_guest_paths_reject_aliases_and_reserved_names() {
    for path in [
        b"file:stream".as_slice(),
        b"CON".as_slice(),
        b"dir/name.".as_slice(),
        b"dir/name ".as_slice(),
        b"dir\\name".as_slice(),
    ] {
        assert!(guest_entry_bytes_to_host_path(path, "test path").is_err());
    }
    assert_eq!(
        windows_guest_path_key(b"Dir/Foo", "test path").unwrap(),
        windows_guest_path_key(b"dir/foo", "test path").unwrap()
    );
}

#[cfg(windows)]
#[test]
fn windows_guest_metadata_rejects_case_equivalent_duplicates() {
    use a3s_box_core::rootfs_metadata::{
        RootfsEntryKind, RootfsMetadataEntry, RootfsMetadataManifest,
    };

    let rootfs = tempfile::TempDir::new().unwrap();
    std::fs::write(rootfs.path().join("Probe"), b"payload").unwrap();
    let entries = ["Probe", "probe"]
        .into_iter()
        .map(|path| RootfsMetadataEntry {
            path_base64: base64::engine::general_purpose::STANDARD.encode(path.as_bytes()),
            kind: RootfsEntryKind::Regular,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            mtime: 0,
            size: 7,
            link_target_base64: None,
        })
        .collect();

    let error = create_tar_from_guest_metadata(
        rootfs.path(),
        &RootfsMetadataManifest::new(entries),
        &rootfs.path().join("rootfs.tar"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("Windows-equivalent duplicate"));
}

#[cfg(windows)]
#[test]
fn windows_guest_metadata_rejects_final_directory_reparse_point() {
    use a3s_box_core::rootfs_metadata::{
        RootfsEntryKind, RootfsMetadataEntry, RootfsMetadataManifest,
    };

    let rootfs = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    let link = rootfs.path().join("junction");
    if let Err(error) = std::os::windows::fs::symlink_dir(outside.path(), &link) {
        if error.raw_os_error() == Some(1314) {
            return;
        }
        panic!("failed to create directory symlink: {error}");
    }
    let manifest = RootfsMetadataManifest::new(vec![RootfsMetadataEntry {
        path_base64: base64::engine::general_purpose::STANDARD.encode(b"junction"),
        kind: RootfsEntryKind::Directory,
        mode: 0o40755,
        uid: 0,
        gid: 0,
        mtime: 0,
        size: 0,
        link_target_base64: None,
    }]);

    let error =
        create_tar_from_guest_metadata(rootfs.path(), &manifest, &rootfs.path().join("rootfs.tar"))
            .unwrap_err();
    assert!(error.to_string().contains("changed type"));
}

#[test]
fn offline_commit_rejects_transitional_state_and_live_pid() {
    use crate::test_helpers::fixtures::make_record;

    let transitional = make_record("id", "box", "starting", None);
    assert!(commit_capture_mode(&transitional).is_err());

    let stopped_but_live = make_record("id", "box", "stopped", Some(std::process::id()));
    assert!(commit_capture_mode(&stopped_but_live).is_err());

    let stopped = make_record("id", "box", "stopped", None);
    assert_eq!(
        commit_capture_mode(&stopped).unwrap(),
        CommitCaptureMode::OfflineDirectory
    );
}

#[test]
fn stopped_guest_native_commit_selects_maintenance_capture() {
    use crate::test_helpers::fixtures::make_record;

    let temporary = tempfile::tempdir().unwrap();
    let mut stopped = make_record("id", "box", "stopped", None);
    stopped.box_dir = temporary.path().join("box");
    std::fs::create_dir_all(stopped.box_dir.join("rootfs-ext4-v1")).unwrap();

    assert_eq!(
        commit_capture_mode(&stopped).unwrap(),
        CommitCaptureMode::OfflineGuestNative
    );
}

#[test]
fn test_build_oci_image() {
    let rootfs = tempfile::tempdir().unwrap();
    std::fs::write(rootfs.path().join("test.txt"), "data").unwrap();

    let output = tempfile::tempdir().unwrap();
    build_oci_image(
        output.path(),
        rootfs.path(),
        "test:latest",
        &Some("test commit".to_string()),
        &Some("tester".to_string()),
        &[],
    )
    .unwrap();

    // Verify OCI layout
    assert!(output.path().join("oci-layout").exists());
    assert!(output.path().join("index.json").exists());
    assert!(output.path().join("blobs/sha256").exists());

    // Verify index.json is valid
    let index: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(output.path().join("index.json")).unwrap())
            .unwrap();
    assert_eq!(index["schemaVersion"], 2);
    assert!(index["manifests"][0]["digest"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
}
