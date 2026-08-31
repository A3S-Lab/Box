use super::*;
use a3s_box_core::rootfs_metadata::ROOTFS_METADATA_SCHEMA;
use sha2::{Digest, Sha256};
use std::io::Write;

fn options() -> Ext4ArtifactOptions {
    Ext4ArtifactOptions::from_disk_mib(32, [0x42; 16]).unwrap()
}

fn image_entry(
    path: &[u8],
    kind: RootfsEntryKind,
    mode: u32,
    uid: u64,
    gid: u64,
) -> RootfsMetadataEntry {
    RootfsMetadataEntry {
        path_base64: base64::engine::general_purpose::STANDARD.encode(path),
        kind,
        mode,
        uid,
        gid,
        mtime: 1_704_067_200,
        size: 0,
        link_target_base64: None,
    }
}

fn sample_source(root: &Path) {
    std::fs::create_dir_all(root.join("etc")).unwrap();
    std::fs::write(root.join("etc/config"), b"guest-native\n").unwrap();
    xattr::set(root.join("etc/config"), "user.a3s.test", b"preserved").unwrap();
    std::fs::hard_link(root.join("etc/config"), root.join("etc/config.link")).unwrap();
    std::os::unix::fs::symlink("config", root.join("etc/current")).unwrap();

    let manifest = RootfsMetadataManifest {
        schema: ROOTFS_METADATA_SCHEMA.to_string(),
        entries: vec![
            image_entry(b".", RootfsEntryKind::Directory, 0o755, 0, 0),
            image_entry(b"./etc", RootfsEntryKind::Directory, 0o750, 20, 21),
            image_entry(b"./etc/config", RootfsEntryKind::Regular, 0o640, 123, 456),
            image_entry(
                b"./etc/config.link",
                RootfsEntryKind::Regular,
                0o640,
                123,
                456,
            ),
            RootfsMetadataEntry {
                link_target_base64: Some(
                    base64::engine::general_purpose::STANDARD.encode(b"config"),
                ),
                ..image_entry(b"./etc/current", RootfsEntryKind::Symlink, 0o777, 0, 0)
            },
        ],
    };
    std::fs::write(
        root.join(IMAGE_ROOTFS_METADATA_PATH.trim_start_matches('/')),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
}

#[test]
fn publishes_verified_metadata_faithful_artifact() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    sample_source(&source);

    let artifact =
        publish_ext4_artifact(&source, &temp.path().join("artifact"), options()).unwrap();
    assert!(artifact.disk.is_file());
    assert_eq!(artifact.manifest.schema, EXT4_ARTIFACT_SCHEMA);
    assert_eq!(artifact.manifest.builder, EXT4_BUILDER_ID);

    let file = File::open(&artifact.disk).unwrap();
    let filesystem = mkext4::reader::Fs::open(&file).unwrap();
    assert!(filesystem.verify().unwrap().is_empty());
    let config = filesystem.resolve("/etc/config").unwrap();
    let hardlink = filesystem.resolve("/etc/config.link").unwrap();
    assert_eq!(config, hardlink);
    assert_eq!(filesystem.read_file(config).unwrap(), b"guest-native\n");
    let inode = filesystem.inode(config).unwrap();
    assert_eq!(inode.mode & 0o7777, 0o640);
    assert_eq!((inode.uid, inode.gid), (123, 456));
    let xattrs = filesystem.xattrs(config).unwrap();
    assert!(xattrs.iter().any(|xattr| {
        xattr.full_name().as_deref() == Some(b"user.a3s.test") && xattr.value == b"preserved"
    }));
    let symlink = filesystem.resolve("/etc/current").unwrap();
    assert_eq!(filesystem.symlink_target(symlink).unwrap(), b"config");
}

#[test]
fn identical_inputs_produce_identical_sparse_images() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    sample_source(&source);
    let sparse = source.join("sparse");
    let mut file = File::create(&sparse).unwrap();
    file.write_all(b"start").unwrap();
    file.seek(SeekFrom::Start(8 * 1024 * 1024 - 3)).unwrap();
    file.write_all(b"end").unwrap();

    let first = publish_ext4_artifact(&source, &temp.path().join("first"), options()).unwrap();
    let second = publish_ext4_artifact(&source, &temp.path().join("second"), options()).unwrap();
    assert_eq!(sha256(&first.disk), sha256(&second.disk));

    let filesystem = mkext4::reader::Fs::open(File::open(&first.disk).unwrap()).unwrap();
    let sparse_inode = filesystem.resolve("/sparse").unwrap();
    let inode = filesystem.inode(sparse_inode).unwrap();
    assert_eq!(inode.size, 8 * 1024 * 1024);
    let extents = filesystem.extents(sparse_inode, &inode).unwrap();
    let mut ending = [0u8; 3];
    assert_eq!(
        filesystem
            .read_file_at(&inode, &extents, 8 * 1024 * 1024 - 3, &mut ending)
            .unwrap(),
        ending.len()
    );
    assert_eq!(&ending, b"end");
}

#[test]
fn runtime_managed_init_uses_canonical_metadata_not_stale_image_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir_all(source.join("sbin")).unwrap();
    std::fs::write(source.join("sbin/init"), b"runtime-init").unwrap();
    let manifest = RootfsMetadataManifest {
        schema: ROOTFS_METADATA_SCHEMA.to_string(),
        entries: vec![
            image_entry(b"./sbin", RootfsEntryKind::Directory, 0o700, 41, 42),
            image_entry(b"./sbin/init", RootfsEntryKind::Regular, 0o600, 123, 456),
        ],
    };
    std::fs::write(
        source.join(IMAGE_ROOTFS_METADATA_PATH.trim_start_matches('/')),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();

    let artifact =
        publish_ext4_artifact(&source, &temp.path().join("artifact"), options()).unwrap();
    let filesystem = mkext4::reader::Fs::open(File::open(&artifact.disk).unwrap()).unwrap();
    let init = filesystem.resolve("/sbin/init").unwrap();
    let inode = filesystem.inode(init).unwrap();
    assert_eq!(inode.mode & 0o7777, 0o755);
    assert_eq!((inode.uid, inode.gid), (0, 0));
    assert_eq!(inode.mtime, 0);
}

#[test]
fn failed_build_never_publishes_partial_generation() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("file"), b"data").unwrap();
    let manifest = RootfsMetadataManifest {
        schema: ROOTFS_METADATA_SCHEMA.to_string(),
        entries: vec![image_entry(
            b"./file",
            RootfsEntryKind::Directory,
            0o755,
            0,
            0,
        )],
    };
    std::fs::write(
        source.join(IMAGE_ROOTFS_METADATA_PATH.trim_start_matches('/')),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let destination = temp.path().join("artifact");

    let error = publish_ext4_artifact(&source, &destination, options())
        .unwrap_err()
        .to_string();
    assert!(error.contains("kind does not match"));
    assert!(!destination.exists());
    assert!(
        std::fs::read_dir(temp.path()).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".a3s-rootfs-ext4-")),
        "temporary artifact directories must be cleaned on failure"
    );
}

#[test]
fn preserves_raw_filename_and_symlink_target_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir(&source).unwrap();

    let raw_name = std::ffi::OsString::from_vec(vec![b'n', b'a', b'm', b'e', b'-', 0xff]);
    let staged_name = crate::rootfs::host_staging_path(Path::new(&raw_name)).unwrap();
    std::fs::write(source.join(&staged_name), b"raw-name-content").unwrap();
    std::os::unix::fs::symlink("host-placeholder", source.join("raw-link")).unwrap();

    let mut raw_manifest_path = b"./".to_vec();
    raw_manifest_path.extend_from_slice(raw_name.as_bytes());
    let manifest = RootfsMetadataManifest {
        schema: ROOTFS_METADATA_SCHEMA.to_string(),
        entries: vec![
            image_entry(
                &raw_manifest_path,
                RootfsEntryKind::Regular,
                0o640,
                123,
                456,
            ),
            RootfsMetadataEntry {
                link_target_base64: Some(
                    base64::engine::general_purpose::STANDARD.encode(b"../target-\xfe"),
                ),
                ..image_entry(b"./raw-link", RootfsEntryKind::Symlink, 0o777, 0, 0)
            },
        ],
    };
    std::fs::write(
        source.join(IMAGE_ROOTFS_METADATA_PATH.trim_start_matches('/')),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();

    let artifact =
        publish_ext4_artifact(&source, &temp.path().join("artifact"), options()).unwrap();
    let filesystem = mkext4::reader::Fs::open(File::open(&artifact.disk).unwrap()).unwrap();
    let raw_file = filesystem
        .lookup(mkext4::spec::ROOT_INO, raw_name.as_bytes())
        .unwrap()
        .expect("raw filename in ext4 directory");
    assert_eq!(filesystem.read_file(raw_file).unwrap(), b"raw-name-content");
    let inode = filesystem.inode(raw_file).unwrap();
    assert_eq!(inode.mode & 0o7777, 0o640);
    assert_eq!((inode.uid, inode.gid), (123, 456));

    let symlink = filesystem.resolve("/raw-link").unwrap();
    assert_eq!(
        filesystem.symlink_target(symlink).unwrap(),
        b"../target-\xfe"
    );
}

#[test]
fn oci_raw_bytes_survive_staging_and_ext4_publication_end_to_end() {
    let temp = tempfile::tempdir().unwrap();
    let layer = temp.path().join("layer.tar");
    let source = temp.path().join("source");
    let raw_name = std::ffi::OsString::from_vec(vec![b'f', b'i', b'l', b'e', b'-', 0xff]);
    let raw_target = std::ffi::OsString::from_vec(b"../target-\xfe".to_vec());

    let file = File::create(&layer).unwrap();
    let mut archive = tar::Builder::new(file);
    let mut file_header = tar::Header::new_gnu();
    file_header.set_size(7);
    file_header.set_mode(0o640);
    file_header.set_uid(123);
    file_header.set_gid(456);
    file_header.set_mtime(1_704_067_200);
    file_header.set_cksum();
    archive
        .append_data(
            &mut file_header,
            Path::new(&raw_name),
            b"payload".as_slice(),
        )
        .unwrap();
    let mut link_header = tar::Header::new_gnu();
    link_header.set_entry_type(tar::EntryType::Symlink);
    link_header.set_size(0);
    link_header.set_mode(0o777);
    link_header.set_uid(0);
    link_header.set_gid(0);
    link_header.set_mtime(1_704_067_200);
    link_header.set_cksum();
    archive
        .append_link(&mut link_header, "raw-link", Path::new(&raw_target))
        .unwrap();
    archive.finish().unwrap();
    drop(archive);

    crate::oci::extract_layer_with_metadata(&layer, &source).unwrap();
    let artifact =
        publish_ext4_artifact(&source, &temp.path().join("artifact"), options()).unwrap();
    let filesystem = mkext4::reader::Fs::open(File::open(&artifact.disk).unwrap()).unwrap();
    let raw_file = filesystem
        .lookup(mkext4::spec::ROOT_INO, raw_name.as_bytes())
        .unwrap()
        .expect("raw OCI filename in ext4");
    assert_eq!(filesystem.read_file(raw_file).unwrap(), b"payload");
    let inode = filesystem.inode(raw_file).unwrap();
    assert_eq!(
        (inode.mode & 0o7777, inode.uid, inode.gid),
        (0o640, 123, 456)
    );
    let link = filesystem.resolve("/raw-link").unwrap();
    assert_eq!(
        filesystem.symlink_target(link).unwrap(),
        raw_target.as_bytes()
    );
}

#[test]
fn published_legacy_artifacts_remain_resumable_after_builder_upgrade() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("payload"), b"legacy").unwrap();
    let artifact =
        publish_ext4_artifact(&source, &temp.path().join("artifact"), options()).unwrap();
    for builder in LEGACY_EXT4_BUILDER_IDS {
        let mut manifest = artifact.manifest.clone();
        manifest.builder = (*builder).to_string();
        std::fs::write(
            artifact.directory.join(MANIFEST_FILE_NAME),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let reopened =
            crate::rootfs::ext4_artifact::open_ext4_artifact(&artifact.directory).unwrap();
        assert_eq!(reopened.manifest.builder, *builder);
    }
}

#[test]
fn capacity_policy_rejects_implicit_or_oversized_disks() {
    assert!(Ext4ArtifactOptions::from_disk_mib(0, [0; 16]).is_err());
    assert!(Ext4ArtifactOptions::from_disk_mib(65 * 1024, [0; 16]).is_err());
}

#[test]
fn resume_delegates_only_the_expected_recovery_state_to_the_guest() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    sample_source(&source);
    let artifact =
        publish_ext4_artifact(&source, &temp.path().join("artifact"), options()).unwrap();

    assert_eq!(
        validate_ext4_image_for_resume(
            &artifact.disk,
            artifact.manifest.capacity_bytes,
            [0x42; 16],
        )
        .unwrap(),
        Ext4ResumeValidation::Clean
    );

    rewrite_superblock(&artifact.disk, |superblock| {
        superblock.feature_incompat |= mkext4::spec::incompat::RECOVER;
    });
    assert!(validate_ext4_image(&artifact.disk, artifact.manifest.capacity_bytes).is_err());
    assert_eq!(
        validate_ext4_image_for_resume(
            &artifact.disk,
            artifact.manifest.capacity_bytes,
            [0x42; 16],
        )
        .unwrap(),
        Ext4ResumeValidation::JournalRecoveryRequired
    );
}

#[test]
fn recovery_boot_rejects_a_changed_superblock_contract() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    sample_source(&source);
    let artifact =
        publish_ext4_artifact(&source, &temp.path().join("artifact"), options()).unwrap();

    rewrite_superblock(&artifact.disk, |superblock| {
        superblock.feature_incompat |= mkext4::spec::incompat::RECOVER;
        superblock.uuid = [0x99; 16];
    });
    let error = validate_ext4_image_for_resume(
        &artifact.disk,
        artifact.manifest.capacity_bytes,
        [0x42; 16],
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("UUID"));
}

fn rewrite_superblock(path: &Path, mutate: impl FnOnce(&mut mkext4::spec::Superblock)) {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let mut bytes = [0u8; mkext4::spec::Superblock::LEN];
    file.seek(SeekFrom::Start(1024)).unwrap();
    file.read_exact(&mut bytes).unwrap();
    let mut superblock = mkext4::spec::Superblock::decode(&bytes).unwrap();
    mutate(&mut superblock);
    superblock.checksum = 0;
    superblock.encode(&mut bytes);
    superblock.checksum = mkext4::csum::superblock(&bytes);
    superblock.encode(&mut bytes);
    file.seek(SeekFrom::Start(1024)).unwrap();
    file.write_all(&bytes).unwrap();
    file.sync_all().unwrap();
}

fn sha256(path: &Path) -> Vec<u8> {
    let mut file = File::open(path).unwrap();
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    hasher.finalize().to_vec()
}
