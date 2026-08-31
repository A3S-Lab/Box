use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use tar::{Builder, EntryType, Header};

use super::publish_oci_layers_ext4;
use crate::oci::OciImage;
use crate::rootfs::Ext4ArtifactOptions;
use sha2::{Digest, Sha256};

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

fn write_blob(root: &Path, bytes: &[u8]) -> String {
    let digest = sha256(bytes);
    std::fs::create_dir_all(root.join("blobs/sha256")).unwrap();
    std::fs::write(
        root.join("blobs/sha256")
            .join(digest.strip_prefix("sha256:").unwrap()),
        bytes,
    )
    .unwrap();
    digest
}

fn image_from_layers(root: &Path, layers: &[&Path]) -> OciImage {
    let mut descriptors = Vec::with_capacity(layers.len());
    let mut diff_ids = Vec::with_capacity(layers.len());
    for layer in layers {
        let bytes = std::fs::read(layer).unwrap();
        descriptors.push(serde_json::json!({
            "mediaType": "application/vnd.oci.image.layer.v1.tar",
            "digest": write_blob(root, &bytes),
            "size": bytes.len()
        }));
        diff_ids.push(sha256(&bytes));
    }
    let config = serde_json::to_vec(&serde_json::json!({
        "architecture": "arm64",
        "os": "linux",
        "config": {"Cmd": ["/bin/true"]},
        "rootfs": {"type": "layers", "diff_ids": diff_ids}
    }))
    .unwrap();
    let config_digest = write_blob(root, &config);
    let manifest = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config.len()
        },
        "layers": descriptors
    }))
    .unwrap();
    let manifest_digest = write_blob(root, &manifest);
    std::fs::write(
        root.join("oci-layout"),
        br#"{"imageLayoutVersion":"1.0.0"}"#,
    )
    .unwrap();
    std::fs::write(
        root.join("index.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "manifests": [{
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": manifest_digest,
                "size": manifest.len(),
                "platform": {"os": "linux", "architecture": "arm64"}
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    OciImage::from_path(root).unwrap()
}

fn append_file(archive: &mut Builder<File>, path: &Path, content: &[u8], mode: u32) {
    let mut header = Header::new_gnu();
    header.set_size(content.len() as u64);
    header.set_mode(mode);
    header.set_uid(123);
    header.set_gid(456);
    header.set_mtime(1_704_067_200);
    header.set_cksum();
    archive.append_data(&mut header, path, content).unwrap();
}

fn append_marker(archive: &mut Builder<File>, path: &str) {
    append_file(archive, Path::new(path), b"", 0o000);
}

fn append_dir(archive: &mut Builder<File>, path: &str, mode: u32) {
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Directory);
    header.set_size(0);
    header.set_mode(mode);
    header.set_uid(123);
    header.set_gid(456);
    header.set_mtime(1_704_067_200);
    header.set_cksum();
    archive
        .append_data(&mut header, path, std::io::empty())
        .unwrap();
}

fn append_symlink(archive: &mut Builder<File>, path: &str, target: &Path) {
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    header.set_uid(7);
    header.set_gid(8);
    header.set_mtime(1_704_067_201);
    header.set_cksum();
    archive.append_link(&mut header, path, target).unwrap();
}

#[test]
fn assembles_layers_without_a_host_namespace() {
    let temporary = tempfile::tempdir().unwrap();
    let lower = temporary.path().join("lower.tar");
    let upper = temporary.path().join("upper.tar");
    let guest_init = temporary.path().join("guest-init");
    std::fs::write(&guest_init, b"guest-init").unwrap();

    let mut lower_tar = Builder::new(File::create(&lower).unwrap());
    lower_tar
        .append_pax_extensions([("SCHILY.xattr.user.a3s", b"preserved".as_slice())])
        .unwrap();
    append_file(&mut lower_tar, Path::new("README"), b"upper-case", 0o640);
    append_file(&mut lower_tar, Path::new("Readme"), b"mixed-case", 0o600);
    append_file(&mut lower_tar, Path::new("etc/removed"), b"lower", 0o644);
    append_file(&mut lower_tar, Path::new("opaque/old"), b"old", 0o644);
    let mut sparse = vec![0; 4 * 1024 * 1024];
    sparse.extend_from_slice(b"end");
    append_file(&mut lower_tar, Path::new("sparse"), &sparse, 0o644);
    let raw_name = std::ffi::OsStr::from_bytes(b"raw-\xff");
    append_file(&mut lower_tar, Path::new(raw_name), b"raw", 0o644);
    append_dir(&mut lower_tar, "usr", 0o755);
    append_dir(&mut lower_tar, "usr/sbin", 0o755);
    append_symlink(&mut lower_tar, "sbin", Path::new("/usr/sbin"));
    lower_tar.finish().unwrap();
    drop(lower_tar);

    let mut upper_tar = Builder::new(File::create(&upper).unwrap());
    append_marker(&mut upper_tar, "etc/.wh.removed");
    append_file(&mut upper_tar, Path::new("opaque/new"), b"new", 0o644);
    append_marker(&mut upper_tar, "opaque/.wh..wh..opq");
    append_file(&mut upper_tar, Path::new("same-layer"), b"survives", 0o644);
    append_marker(&mut upper_tar, ".wh.same-layer");
    append_symlink(&mut upper_tar, "link", Path::new("opaque/new"));
    let mut hardlink = Header::new_gnu();
    hardlink.set_entry_type(EntryType::Link);
    hardlink.set_size(0);
    hardlink.set_mode(0o777);
    hardlink.set_uid(7);
    hardlink.set_gid(8);
    hardlink.set_mtime(1_704_067_201);
    hardlink.set_cksum();
    upper_tar
        .append_link(&mut hardlink, "link.alias", Path::new("link"))
        .unwrap();
    upper_tar.finish().unwrap();
    drop(upper_tar);
    let image = image_from_layers(&temporary.path().join("image"), &[&lower, &upper]);

    let artifact = publish_oci_layers_ext4(
        &image,
        &guest_init,
        &sha256(b"guest-init"),
        &temporary.path().join("artifact"),
        Ext4ArtifactOptions::from_disk_mib(32, [0x31; 16]).unwrap(),
    )
    .unwrap();

    let filesystem = mkext4::reader::Fs::open(File::open(artifact.disk).unwrap()).unwrap();
    assert!(filesystem.verify().unwrap().is_empty());
    assert_eq!(
        filesystem
            .read_file(filesystem.resolve("/README").unwrap())
            .unwrap(),
        b"upper-case"
    );
    let readme = filesystem.resolve("/README").unwrap();
    let readme_inode = filesystem.inode(readme).unwrap();
    assert_eq!(
        (
            readme_inode.mode & 0o7777,
            readme_inode.uid,
            readme_inode.gid
        ),
        (0o640, 123, 456)
    );
    assert!(filesystem.xattrs(readme).unwrap().iter().any(|xattr| {
        xattr.full_name().as_deref() == Some(b"user.a3s") && xattr.value == b"preserved"
    }));
    assert_eq!(
        filesystem
            .read_file(filesystem.resolve("/Readme").unwrap())
            .unwrap(),
        b"mixed-case"
    );
    assert!(filesystem.resolve("/etc/removed").is_err());
    assert!(filesystem.resolve("/opaque/old").is_err());
    assert_eq!(
        filesystem
            .read_file(filesystem.resolve("/opaque/new").unwrap())
            .unwrap(),
        b"new"
    );
    assert_eq!(
        filesystem
            .read_file(filesystem.resolve("/same-layer").unwrap())
            .unwrap(),
        b"survives"
    );
    let raw = filesystem
        .lookup(mkext4::spec::ROOT_INO, raw_name.as_bytes())
        .unwrap()
        .unwrap();
    assert_eq!(filesystem.read_file(raw).unwrap(), b"raw");
    let sparse = filesystem.resolve("/sparse").unwrap();
    let sparse_inode = filesystem.inode(sparse).unwrap();
    assert_eq!(sparse_inode.size, 4 * 1024 * 1024 + 3);
    assert!(
        sparse_inode.blocks * 512 < sparse_inode.size / 2,
        "direct assembly should preserve large zero runs as ext4 holes: blocks={}, size={}",
        sparse_inode.blocks,
        sparse_inode.size
    );
    assert_eq!(
        filesystem.resolve("/link").unwrap(),
        filesystem.resolve("/link.alias").unwrap()
    );
    assert_eq!(
        filesystem
            .symlink_target(filesystem.resolve("/link").unwrap())
            .unwrap(),
        b"opaque/new"
    );
    assert_eq!(
        filesystem
            .read_file(filesystem.resolve("/usr/sbin/init").unwrap())
            .unwrap(),
        b"guest-init"
    );
    let init = filesystem.resolve("/usr/sbin/init").unwrap();
    let init_inode = filesystem.inode(init).unwrap();
    assert_eq!(
        (
            init_inode.mode & 0o7777,
            init_inode.uid,
            init_inode.gid,
            init_inode.mtime
        ),
        (0o755, 0, 0, 0)
    );
    assert!(filesystem.resolve("/.a3s_image_metadata_v1.json").is_err());
}

#[test]
fn failed_capacity_check_never_publishes_a_partial_generation() {
    let temporary = tempfile::tempdir().unwrap();
    let layer = temporary.path().join("oversized.tar");
    let guest_init = temporary.path().join("guest-init");
    std::fs::write(&guest_init, b"guest-init").unwrap();
    let payload = vec![0x5a; 20 * 1024 * 1024];
    let mut archive = Builder::new(File::create(&layer).unwrap());
    append_file(&mut archive, Path::new("payload"), &payload, 0o644);
    archive.finish().unwrap();
    drop(archive);
    let destination = temporary.path().join("artifact");
    let image = image_from_layers(&temporary.path().join("image"), &[&layer]);

    let error = publish_oci_layers_ext4(
        &image,
        &guest_init,
        &sha256(b"guest-init"),
        &destination,
        Ext4ArtifactOptions::from_disk_mib(16, [0x32; 16]).unwrap(),
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains("ext4"), "{error}");
    assert!(!destination.exists());
    assert!(std::fs::read_dir(temporary.path()).unwrap().all(|entry| {
        let name = entry.unwrap().file_name();
        let name = name.to_string_lossy();
        !name.starts_with(".a3s-rootfs-ext4-") && !name.starts_with(".a3s-oci-ext4-content-")
    }));
}

#[test]
fn rejects_internal_paths_and_symlink_parent_escapes() {
    for (name, build_layer) in [
        (
            "reserved",
            Box::new(|archive: &mut Builder<File>| {
                append_file(
                    archive,
                    Path::new(".a3s_rootfs_metadata_v1.json"),
                    b"forged",
                    0o644,
                );
            }) as Box<dyn Fn(&mut Builder<File>)>,
        ),
        (
            "escape",
            Box::new(|archive: &mut Builder<File>| {
                append_symlink(archive, "escape", Path::new("../../outside"));
                append_file(archive, Path::new("escape/payload"), b"forged", 0o644);
            }),
        ),
    ] {
        let temporary = tempfile::tempdir().unwrap();
        let layer = temporary.path().join(format!("{name}.tar"));
        let guest_init = temporary.path().join("guest-init");
        std::fs::write(&guest_init, b"guest-init").unwrap();
        let mut archive = Builder::new(File::create(&layer).unwrap());
        build_layer(&mut archive);
        archive.finish().unwrap();
        drop(archive);
        let destination = temporary.path().join("artifact");
        let image = image_from_layers(&temporary.path().join("image"), &[&layer]);

        let error = publish_oci_layers_ext4(
            &image,
            &guest_init,
            &sha256(b"guest-init"),
            &destination,
            Ext4ArtifactOptions::from_disk_mib(16, [0x33; 16]).unwrap(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("reserved internal path") || error.contains("escapes the guest root"),
            "{error}"
        );
        assert!(!destination.exists());
    }
}

#[test]
fn rejects_a_layer_changed_after_image_authentication() {
    let temporary = tempfile::tempdir().unwrap();
    let layer = temporary.path().join("layer.tar");
    let guest_init = temporary.path().join("guest-init");
    std::fs::write(&guest_init, b"guest-init").unwrap();
    let mut archive = Builder::new(File::create(&layer).unwrap());
    append_file(&mut archive, Path::new("payload"), b"authenticated", 0o644);
    archive.finish().unwrap();
    drop(archive);
    let image = image_from_layers(&temporary.path().join("image"), &[&layer]);
    let blob = image.layer_paths()[0].clone();
    let mut changed = std::fs::read(&blob).unwrap();
    changed[0] ^= 0xff;
    std::fs::write(&blob, changed).unwrap();
    let destination = temporary.path().join("artifact");

    let error = publish_oci_layers_ext4(
        &image,
        &guest_init,
        &sha256(b"guest-init"),
        &destination,
        Ext4ArtifactOptions::from_disk_mib(16, [0x34; 16]).unwrap(),
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains("descriptor digest"), "{error}");
    assert!(!destination.exists());
}

#[test]
fn assembles_an_authenticated_gzip_layer() {
    let temporary = tempfile::tempdir().unwrap();
    let plain = temporary.path().join("layer.tar");
    let compressed = temporary.path().join("layer.tar.gz");
    let guest_init = temporary.path().join("guest-init");
    std::fs::write(&guest_init, b"guest-init").unwrap();
    let mut archive = Builder::new(File::create(&plain).unwrap());
    append_file(&mut archive, Path::new("compressed"), b"verified", 0o644);
    archive.finish().unwrap();
    drop(archive);
    let mut encoder = flate2::write::GzEncoder::new(
        File::create(&compressed).unwrap(),
        flate2::Compression::default(),
    );
    std::io::copy(&mut File::open(&plain).unwrap(), &mut encoder).unwrap();
    encoder.finish().unwrap();
    let image = image_from_layers(&temporary.path().join("image"), &[&compressed]);

    let artifact = publish_oci_layers_ext4(
        &image,
        &guest_init,
        &sha256(b"guest-init"),
        &temporary.path().join("artifact"),
        Ext4ArtifactOptions::from_disk_mib(16, [0x35; 16]).unwrap(),
    )
    .unwrap();

    let filesystem = mkext4::reader::Fs::open(File::open(artifact.disk).unwrap()).unwrap();
    let file = filesystem.resolve("/compressed").unwrap();
    assert_eq!(filesystem.read_file(file).unwrap(), b"verified");
}

#[test]
fn rejects_guest_init_that_disagrees_with_its_cache_identity() {
    let temporary = tempfile::tempdir().unwrap();
    let layer = temporary.path().join("layer.tar");
    let guest_init = temporary.path().join("guest-init");
    std::fs::write(&guest_init, b"changed-init").unwrap();
    let mut archive = Builder::new(File::create(&layer).unwrap());
    append_file(&mut archive, Path::new("payload"), b"image", 0o644);
    archive.finish().unwrap();
    drop(archive);
    let image = image_from_layers(&temporary.path().join("image"), &[&layer]);
    let destination = temporary.path().join("artifact");

    let error = publish_oci_layers_ext4(
        &image,
        &guest_init,
        &sha256(b"original-init"),
        &destination,
        Ext4ArtifactOptions::from_disk_mib(16, [0x36; 16]).unwrap(),
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains("cache identity"), "{error}");
    assert!(!destination.exists());
}
