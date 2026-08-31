use super::*;
use std::ffi::OsString;
use std::fs::File;
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use tempfile::TempDir;

fn raw(bytes: &[u8]) -> OsString {
    OsString::from_vec(bytes.to_vec())
}

fn append_regular<W: Write>(
    archive: &mut tar::Builder<W>,
    path: &Path,
    content: &[u8],
    uid: u64,
    gid: u64,
    mode: u32,
) {
    let mut header = tar::Header::new_gnu();
    header.set_size(content.len() as u64);
    header.set_mode(mode);
    header.set_uid(uid);
    header.set_gid(gid);
    header.set_mtime(1_704_067_200);
    header.set_cksum();
    archive.append_data(&mut header, path, content).unwrap();
}

fn append_hardlink<W: Write>(archive: &mut tar::Builder<W>, path: &Path, target: &Path) {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Link);
    header.set_size(0);
    header.set_mode(0o640);
    header.set_uid(123);
    header.set_gid(456);
    header.set_mtime(1_704_067_200);
    header.set_cksum();
    archive.append_link(&mut header, path, target).unwrap();
}

#[test]
fn macos_extraction_stages_raw_names_and_retains_guest_bytes() {
    let temp = TempDir::new().unwrap();
    let layer = temp.path().join("raw-name.tar");
    let target = temp.path().join("rootfs");
    let logical = PathBuf::from(raw(b"name-\xff"));

    let file = File::create(&layer).unwrap();
    let mut archive = tar::Builder::new(file);
    append_regular(&mut archive, &logical, b"payload", 123, 456, 0o640);
    archive.finish().unwrap();
    drop(archive);

    extract_layer_with_metadata(&layer, &target).unwrap();

    let staged = crate::rootfs::host_staging_path(&logical).unwrap();
    assert_eq!(std::fs::read(target.join(staged)).unwrap(), b"payload");
    let metadata = load_image_metadata(&target).unwrap();
    let entry = metadata.get(&logical).expect("raw guest path metadata");
    assert_eq!(
        (entry.uid, entry.gid, entry.mode & 0o7777),
        (123, 456, 0o640)
    );
}

#[test]
fn macos_raw_whiteout_removes_the_physical_staging_entry_and_metadata() {
    let temp = TempDir::new().unwrap();
    let lower = temp.path().join("lower.tar");
    let upper = temp.path().join("upper.tar");
    let target = temp.path().join("rootfs");
    let logical_parent = PathBuf::from(raw(b"dir-\xfe"));
    let victim_name = raw(b"victim-\xff");
    let victim = logical_parent.join(&victim_name);

    let file = File::create(&lower).unwrap();
    let mut archive = tar::Builder::new(file);
    append_regular(&mut archive, &victim, b"lower", 0, 0, 0o644);
    archive.finish().unwrap();
    drop(archive);
    extract_layer_with_metadata(&lower, &target).unwrap();

    let mut marker_name = b".wh.".to_vec();
    marker_name.extend_from_slice(victim_name.as_encoded_bytes());
    let marker = logical_parent.join(raw(&marker_name));
    let file = File::create(&upper).unwrap();
    let mut archive = tar::Builder::new(file);
    append_regular(&mut archive, &marker, b"", 0, 0, 0o000);
    archive.finish().unwrap();
    drop(archive);
    extract_layer_with_metadata(&upper, &target).unwrap();

    let staged = crate::rootfs::host_staging_path(&victim).unwrap();
    assert!(!target.join(staged).exists());
    assert!(!load_image_metadata(&target).unwrap().contains_key(&victim));
}

#[test]
fn macos_raw_hardlinks_keep_guest_paths_and_inode_identity() {
    let temp = TempDir::new().unwrap();
    let layer = temp.path().join("hardlinks.tar");
    let target = temp.path().join("rootfs");
    let logical_parent = PathBuf::from(raw(b"dir-\xfd"));
    let original = logical_parent.join(raw(b"target-\xff"));
    // Keep the destination ASCII so translation is required solely by the
    // raw target. This guards against deciding from the link name alone.
    let link = PathBuf::from("ascii-link");

    let file = File::create(&layer).unwrap();
    let mut archive = tar::Builder::new(file);
    append_regular(&mut archive, &original, b"shared", 123, 456, 0o640);
    append_hardlink(&mut archive, &link, &original);
    archive.finish().unwrap();
    drop(archive);

    extract_layer_with_metadata(&layer, &target).unwrap();

    let original_staged = target.join(crate::rootfs::host_staging_path(&original).unwrap());
    let link_staged = target.join(crate::rootfs::host_staging_path(&link).unwrap());
    assert_eq!(std::fs::read(&link_staged).unwrap(), b"shared");
    assert_eq!(
        std::fs::metadata(original_staged).unwrap().ino(),
        std::fs::metadata(link_staged).unwrap().ino()
    );
    let metadata = load_image_metadata(&target).unwrap();
    assert!(metadata.contains_key(&original));
    assert!(metadata.contains_key(&link));
    assert!(metadata.contains_key(&logical_parent));
}

#[test]
fn macos_raw_hardlink_to_symlink_preserves_the_link_inode() {
    let temp = TempDir::new().unwrap();
    let layer = temp.path().join("hardlink-to-symlink.tar");
    let target = temp.path().join("rootfs");
    let raw_symlink = PathBuf::from(raw(b"symlink-\xff"));
    let hardlink = PathBuf::from("ascii-hardlink");

    let file = File::create(&layer).unwrap();
    let mut archive = tar::Builder::new(file);
    append_regular(&mut archive, Path::new("payload"), b"data", 0, 0, 0o644);
    let mut symlink_header = tar::Header::new_gnu();
    symlink_header.set_entry_type(tar::EntryType::Symlink);
    symlink_header.set_size(0);
    symlink_header.set_mode(0o777);
    symlink_header.set_uid(0);
    symlink_header.set_gid(0);
    symlink_header.set_mtime(1_704_067_200);
    symlink_header.set_cksum();
    archive
        .append_link(&mut symlink_header, &raw_symlink, Path::new("payload"))
        .unwrap();
    append_hardlink(&mut archive, &hardlink, &raw_symlink);
    archive.finish().unwrap();
    drop(archive);

    extract_layer_with_metadata(&layer, &target).unwrap();

    let symlink_staged = target.join(crate::rootfs::host_staging_path(&raw_symlink).unwrap());
    let hardlink_staged = target.join(crate::rootfs::host_staging_path(&hardlink).unwrap());
    let symlink_metadata = std::fs::symlink_metadata(&symlink_staged).unwrap();
    let hardlink_metadata = std::fs::symlink_metadata(&hardlink_staged).unwrap();
    assert!(symlink_metadata.file_type().is_symlink());
    assert!(hardlink_metadata.file_type().is_symlink());
    assert_eq!(symlink_metadata.ino(), hardlink_metadata.ino());
    assert_eq!(
        std::fs::read_link(hardlink_staged).unwrap(),
        Path::new("payload")
    );
}

#[test]
fn macos_unicode_normalization_variants_never_alias_on_apfs() {
    let temp = TempDir::new().unwrap();
    let layer = temp.path().join("unicode.tar");
    let target = temp.path().join("rootfs");
    let composed = PathBuf::from("caf\u{e9}");
    let decomposed = PathBuf::from("cafe\u{301}");
    assert_ne!(composed.as_os_str(), decomposed.as_os_str());

    let file = File::create(&layer).unwrap();
    let mut archive = tar::Builder::new(file);
    append_regular(&mut archive, &composed, b"composed", 0, 0, 0o644);
    append_regular(&mut archive, &decomposed, b"decomposed", 0, 0, 0o644);
    archive.finish().unwrap();
    drop(archive);

    extract_layer_with_metadata(&layer, &target).unwrap();

    let composed_staged = crate::rootfs::host_staging_path(&composed).unwrap();
    let decomposed_staged = crate::rootfs::host_staging_path(&decomposed).unwrap();
    assert_ne!(composed_staged, decomposed_staged);
    assert_eq!(
        std::fs::read(target.join(composed_staged)).unwrap(),
        b"composed"
    );
    assert_eq!(
        std::fs::read(target.join(decomposed_staged)).unwrap(),
        b"decomposed"
    );
    let metadata = load_image_metadata(&target).unwrap();
    assert!(metadata.contains_key(&composed));
    assert!(metadata.contains_key(&decomposed));
}
