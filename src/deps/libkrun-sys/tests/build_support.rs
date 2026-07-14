#[path = "../build_support.rs"]
mod build_support;

use std::path::{Path, PathBuf};

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
