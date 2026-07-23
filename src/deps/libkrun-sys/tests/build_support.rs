#[path = "../build_support.rs"]
mod build_support;

use std::path::{Path, PathBuf};

#[test]
fn sha256_file_hashes_known_content() {
    let temp = tempfile::tempdir().expect("create temporary directory");
    let path = temp.path().join("payload.bin");
    std::fs::write(&path, b"abc").expect("write checksum fixture");

    assert_eq!(
        build_support::sha256_file(&path).expect("hash fixture"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn nested_cargo_home_is_target_local() {
    let install_dir = Path::new("/tmp/a3s-target/build/libkrun-sys/out/libkrun");

    assert_eq!(
        build_support::nested_cargo_home(install_dir),
        PathBuf::from("/tmp/a3s-target/build/libkrun-sys/out/libkrun-cargo-home")
    );
}

#[test]
fn libkrun_builds_share_only_the_isolated_nested_cache() {
    let out_dir = Path::new("/tmp/a3s-target/build/libkrun-sys/out");
    let libkrun_home = build_support::nested_cargo_home(&out_dir.join("libkrun"));
    let libkrunfw_home = build_support::nested_cargo_home(&out_dir.join("libkrunfw"));

    assert_eq!(libkrun_home, libkrunfw_home);
    assert_eq!(
        libkrun_home,
        PathBuf::from("/tmp/a3s-target/build/libkrun-sys/out/libkrun-cargo-home")
    );
}
