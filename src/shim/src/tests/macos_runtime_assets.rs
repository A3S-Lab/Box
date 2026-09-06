#![cfg(target_os = "macos")]

#[path = "../../build_support/macos.rs"]
mod macos;

use std::fs;
use std::os::unix::fs::symlink;
use std::process::Command;

#[test]
fn stages_versioned_dylibs_and_aliases_without_invalidating_signatures() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let source_code = source.path().join("fixture.c");
    fs::write(&source_code, "int runtime_fixture(void) { return 42; }\n").unwrap();
    let versioned_name = "libkrun.9.42.0.dylib";
    let library = source.path().join(versioned_name);
    let output = Command::new("cc")
        .args([
            "-dynamiclib",
            "-Wl,-install_name,@rpath/libkrun.9.42.0.dylib",
        ])
        .arg(&source_code)
        .arg("-o")
        .arg(&library)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(Command::new("codesign")
        .args(["--force", "--sign", "-"])
        .arg(&library)
        .status()
        .unwrap()
        .success());
    symlink(versioned_name, source.path().join("libkrun.9.dylib")).unwrap();
    symlink("libkrun.9.dylib", source.path().join("libkrun.dylib")).unwrap();
    fs::write(
        source.path().join("unrelated.dylib"),
        "not a runtime library",
    )
    .unwrap();
    let signed_bytes = fs::read(&library).unwrap();

    macos::stage_runtime_dylibs(source.path(), destination.path()).unwrap();
    for name in [versioned_name, "libkrun.9.dylib", "libkrun.dylib"] {
        let staged = destination.path().join(name);
        assert_eq!(fs::read(&staged).unwrap(), signed_bytes);
        assert!(Command::new("codesign")
            .arg("--verify")
            .arg(&staged)
            .status()
            .unwrap()
            .success());
    }
    assert!(!destination.path().join("unrelated.dylib").exists());
}

#[test]
fn staging_same_file_or_hard_link_does_not_truncate_it() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let library = source.path().join("libkrunfw.5.dylib");
    fs::write(&library, "signed firmware fixture").unwrap();
    fs::hard_link(&library, destination.path().join("libkrunfw.5.dylib")).unwrap();

    macos::stage_runtime_dylibs(source.path(), source.path()).unwrap();
    macos::stage_runtime_dylibs(source.path(), destination.path()).unwrap();
    assert_eq!(
        fs::read_to_string(&library).unwrap(),
        "signed firmware fixture"
    );
}
