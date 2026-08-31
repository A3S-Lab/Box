//! Real Apple Silicon/HVF qualification for the guest-native rootfs lifecycle.
//!
//! The test is ignored by default because it boots real MicroVMs and may pull
//! an OCI image. CI runs it only on a physical Apple Silicon HVF runner.

#![cfg(target_os = "macos")]

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use a3s_box_core::guest_exec::{GuestTerminalStatus, GUEST_TERMINAL_STATUS_FILE_NAME};

// Set A3S_BOX_KEEP_SMOKE_HOME to retain the isolated A3S_HOME after a failure.
const LEGACY_APFS_ROOTFS_ENV: &str = "A3S_BOX_MACOS_LEGACY_APFS_ROOTFS";
const DEFAULT_IMAGE: &str = "docker.io/library/alpine:3.20";
const RECOVER_FEATURE: u32 = 0x4;

struct GuestNativeSmoke {
    binary: PathBuf,
    home: tempfile::TempDir,
    name: String,
    removed: bool,
    preserve_on_drop: bool,
}

impl GuestNativeSmoke {
    fn new() -> Self {
        let preserve_on_drop = std::env::var_os("A3S_BOX_KEEP_SMOKE_HOME").is_some();
        let mut home = tempfile::tempdir().expect("temporary A3S_HOME");
        home.disable_cleanup(preserve_on_drop);
        if preserve_on_drop {
            eprintln!("    preserving A3S_HOME at {}", home.path().display());
        }
        Self {
            binary: find_binary(),
            home,
            name: format!("guest-native-{}", std::process::id()),
            removed: false,
            preserve_on_drop,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .args(args)
            .env("A3S_HOME", self.home.path())
            .env_remove(LEGACY_APFS_ROOTFS_ENV);
        command
    }

    fn output(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
        eprintln!("    $ a3s-box {}", args.join(" "));
        let output = self
            .command(args)
            .envs(environment.iter().copied())
            .output()
            .unwrap_or_else(|error| panic!("failed to run `a3s-box {}`: {error}", args.join(" ")));
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        output
    }

    fn ok(&self, args: &[&str]) -> String {
        self.ok_with_env(args, &[])
    }

    fn ok_with_env(&self, args: &[&str], environment: &[(&str, &str)]) -> String {
        let output = self.output(args, environment);
        assert!(
            output.status.success(),
            "`a3s-box {}` failed\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("a3s-box stdout is UTF-8")
    }

    fn inspect(&self) -> serde_json::Value {
        let output = self.ok(&["inspect", &self.name]);
        let mut records = serde_json::from_str::<Vec<serde_json::Value>>(&output)
            .expect("box inspect JSON array");
        assert_eq!(
            records.len(),
            1,
            "inspect returned an unexpected record set"
        );
        records.remove(0)
    }

    fn box_dir(&self) -> PathBuf {
        PathBuf::from(
            self.inspect()["box_dir"]
                .as_str()
                .expect("box_dir in inspect"),
        )
    }

    fn terminal_status(&self) -> GuestTerminalStatus {
        let path = self
            .box_dir()
            .join("runtime-control")
            .join(GUEST_TERMINAL_STATUS_FILE_NAME);
        let status =
            serde_json::from_slice::<GuestTerminalStatus>(&std::fs::read(&path).unwrap_or_else(
                |error| panic!("failed to read terminal status {}: {error}", path.display()),
            ))
            .expect("versioned terminal status");
        status.validate().expect("valid terminal status schema");
        status
    }

    fn assert_no_host_attachment(&self) {
        for (program, args) in [("mount", &[][..]), ("hdiutil", &["info"][..])] {
            let output = Command::new(program)
                .args(args)
                .output()
                .unwrap_or_else(|error| panic!("failed to run {program}: {error}"));
            assert!(output.status.success(), "{program} failed");
            let evidence = String::from_utf8_lossy(&output.stdout);
            assert!(
                !evidence.contains(self.home.path().to_string_lossy().as_ref()),
                "guest-native box left a host mount attached:\n{evidence}"
            );
        }
    }

    fn assert_no_runtime_mount(&self) {
        self.assert_no_host_attachment();
        let box_dir = self.box_dir();
        assert!(
            !contains_sparse_image(&box_dir),
            "guest-native box retained an APFS construction image under {}",
            box_dir.display()
        );
    }

    fn remove(&mut self) {
        self.ok(&["rm", "--force", &self.name]);
        self.removed = true;
    }
}

#[test]
#[ignore]
fn legacy_apfs_migration_retains_verified_rollback_generation() {
    let mut smoke = GuestNativeSmoke::new();
    smoke.name = format!("guest-native-migration-{}", std::process::id());
    let image = std::env::var("A3S_BOX_SMOKE_IMAGE").unwrap_or_else(|_| DEFAULT_IMAGE.to_string());
    let name = smoke.name.clone();

    // Create and stop one compatibility generation first. The explicit legacy
    // switch is scoped to this command so a developer's shell cannot
    // accidentally bypass the default migration path.
    smoke.ok_with_env(
        &[
            "run",
            "--detach",
            "--persistent",
            "--name",
            &name,
            &image,
            "--",
            "/bin/sh",
            "-c",
            "printf legacy-generation >/legacy-generation; sync; exec sleep 3600",
        ],
        &[(LEGACY_APFS_ROOTFS_ENV, "1")],
    );
    smoke.ok(&[
        "exec",
        &name,
        "--",
        "/bin/sh",
        "-c",
        "test \"$(cat /legacy-generation)\" = legacy-generation",
    ]);
    smoke.ok(&["stop", &name]);

    let box_dir = smoke.box_dir();
    let legacy = box_dir.join("rootfs-apfs-v2.sparseimage");
    let artifact = box_dir.join("rootfs-ext4-v1/rootfs.ext4");
    let migration = box_dir.join("rootfs-migration-v1.json");
    assert!(legacy.is_file(), "compatibility APFS image is missing");
    assert!(
        !artifact.exists(),
        "legacy box unexpectedly has an ext4 disk"
    );
    assert!(
        !migration.exists(),
        "legacy box unexpectedly has migration state"
    );

    // Starting a stopped legacy box on the default provider begins a durable
    // two-phase migration. The original sparse image remains as rollback
    // evidence but is detached before the ext4-backed workload starts.
    smoke.ok(&["start", &name]);
    smoke.ok(&[
        "exec",
        &name,
        "--",
        "/bin/sh",
        "-c",
        "test \"$(cat /legacy-generation)\" = legacy-generation; printf migrated-generation >/migrated-generation; sync",
    ]);
    assert!(
        artifact.is_file(),
        "migration did not publish the ext4 disk"
    );
    assert!(
        legacy.is_file(),
        "migration removed its rollback generation"
    );
    assert_eq!(migration_state(&migration), "artifact_ready");
    smoke.assert_no_host_attachment();

    smoke.ok(&["stop", &name]);
    assert_eq!(migration_state(&migration), "clean_stop_verified");
    assert!(legacy.is_file(), "clean stop removed rollback evidence");
    smoke.assert_no_host_attachment();

    // Provider selection is now bound to the retained ext4 generation, not to
    // the process environment that initiated migration.
    smoke.ok(&["start", &name]);
    smoke.ok(&[
        "exec",
        &name,
        "--",
        "/bin/sh",
        "-c",
        "test \"$(cat /legacy-generation)\" = legacy-generation; test \"$(cat /migrated-generation)\" = migrated-generation",
    ]);
    smoke.assert_no_host_attachment();
    smoke.ok(&["stop", &name]);
    smoke.remove();
}

impl Drop for GuestNativeSmoke {
    fn drop(&mut self) {
        if !self.removed && !self.preserve_on_drop {
            let _ = self.command(&["rm", "--force", &self.name]).output();
        }
    }
}

#[test]
#[ignore]
fn guest_native_persistent_restart_and_crash_recovery() {
    let mut smoke = GuestNativeSmoke::new();
    let image = std::env::var("A3S_BOX_SMOKE_IMAGE").unwrap_or_else(|_| DEFAULT_IMAGE.to_string());
    let name = smoke.name.clone();

    smoke.ok(&[
        "run",
        "--detach",
        "--persistent",
        "--name",
        &name,
        &image,
        "--",
        "/bin/sh",
        "-c",
        "printf generation-one >/generation-one; exec sleep 3600",
    ]);
    smoke.ok(&[
        "exec",
        &name,
        "--",
        "/bin/sh",
        "-c",
        "test \"$(cat /generation-one)\" = generation-one; sync",
    ]);
    smoke.assert_no_runtime_mount();

    smoke.ok(&["stop", &name]);
    let clean_status = smoke.terminal_status();
    assert_eq!(clean_status.exit_code, 143);
    assert!(clean_status.rootfs_quiesced);

    // The retained raw generation remains authoritative on every later start.
    // Generation two writes state that must survive a crash.
    smoke.ok(&["start", &name]);
    smoke.ok(&[
        "exec",
        &name,
        "--",
        "/bin/sh",
        "-c",
        "test \"$(cat /generation-one)\" = generation-one; printf generation-two >/generation-two; sync",
    ]);
    let crashed_pid = smoke.inspect()["pid"].as_u64().expect("running shim PID") as i32;
    assert_eq!(unsafe { libc::kill(crashed_pid, libc::SIGKILL) }, 0);
    wait_for_process_exit(crashed_pid, Duration::from_secs(10));
    smoke.ok(&["ps", "--all"]);

    let unsafe_offline_read = smoke.output(&["diff", &name], &[]);
    assert!(
        !unsafe_offline_read.status.success(),
        "offline diff unexpectedly read a journal-dirty rootfs"
    );
    assert!(
        String::from_utf8_lossy(&unsafe_offline_read.stderr)
            .contains("needs ext4 journal recovery"),
        "offline diff did not explain its recovery fence:\n{}",
        String::from_utf8_lossy(&unsafe_offline_read.stderr)
    );

    let recovery = smoke.ok_with_env(&["start", &name], &[("RUST_LOG", "a3s_box_runtime=info")]);
    assert!(
        recovery.contains(&name),
        "recovery start did not report the box name"
    );
    smoke.ok(&[
        "exec",
        &name,
        "--",
        "/bin/sh",
        "-c",
        "test \"$(cat /generation-one)\" = generation-one; test \"$(cat /generation-two)\" = generation-two",
    ]);

    // A clean stop after replay must clear RECOVER and return to the strict
    // full-reader validation path on the following start.
    smoke.ok(&["stop", &name]);
    assert!(smoke.terminal_status().rootfs_quiesced);
    let disk = smoke.box_dir().join("rootfs-ext4-v1/rootfs.ext4");
    assert_eq!(superblock_incompat_features(&disk) & RECOVER_FEATURE, 0);

    // Stopped inspection runs a trusted one-shot maintenance VM whose current
    // guest-init is separate from the mutable user disk. The disk is attached
    // read-only, mounted with noload, and must remain byte-generation stable
    // across every operation.
    let disk_before = disk_observation(&disk);
    let terminal_path = smoke
        .box_dir()
        .join("runtime-control")
        .join(GUEST_TERMINAL_STATUS_FILE_NAME);
    let terminal_before =
        std::fs::read(&terminal_path).expect("read terminal status before maintenance");

    let diff = smoke.ok(&["diff", &name]);
    assert!(
        diff.lines().any(|line| line == "A /generation-one"),
        "stopped diff omitted generation one:\n{diff}"
    );
    assert!(
        diff.lines().any(|line| line == "A /generation-two"),
        "stopped diff omitted generation two:\n{diff}"
    );
    smoke.assert_no_runtime_mount();

    let export_path = smoke.home.path().join("guest-native-export.tar");
    let export_path_text = export_path.to_string_lossy().into_owned();
    smoke.ok(&["export", &name, "--output", &export_path_text]);
    assert!(archive_contains(&export_path, "generation-one"));
    assert!(archive_contains(&export_path, "generation-two"));
    smoke.assert_no_runtime_mount();

    let committed_reference = format!("guest-native-smoke-{}:latest", std::process::id());
    let commit = smoke.ok(&["commit", &name, &committed_reference]);
    assert!(
        commit.lines().any(|line| line.starts_with("sha256:")),
        "stopped commit did not publish an image digest:\n{commit}"
    );
    smoke.assert_no_runtime_mount();

    assert_eq!(disk_observation(&disk), disk_before);
    assert_eq!(
        std::fs::read(&terminal_path).expect("read terminal status after maintenance"),
        terminal_before
    );
    assert_eq!(superblock_incompat_features(&disk) & RECOVER_FEATURE, 0);

    smoke.ok(&["start", &name]);
    smoke.ok(&[
        "exec",
        &name,
        "--",
        "/bin/sh",
        "-c",
        "test \"$(cat /generation-two)\" = generation-two",
    ]);
    smoke.assert_no_runtime_mount();
    smoke.ok(&["stop", &name]);
    smoke.remove();
}

fn find_binary() -> PathBuf {
    if let Ok(test_binary) = std::env::current_exe() {
        if let Some(profile_dir) = test_binary.parent().and_then(Path::parent) {
            let candidate = profile_dir.join("a3s-box");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CLI crate is inside the workspace");
    for profile in ["debug", "release"] {
        let candidate = workspace.join("target").join(profile).join("a3s-box");
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from("a3s-box")
}

fn contains_sparse_image(root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "sparseimage")
        {
            return true;
        }
        if path.is_dir() && contains_sparse_image(&path) {
            return true;
        }
    }
    false
}

fn migration_state(path: &Path) -> String {
    serde_json::from_slice::<serde_json::Value>(
        &std::fs::read(path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display())),
    )
    .expect("valid rootfs migration manifest")["state"]
        .as_str()
        .expect("rootfs migration state")
        .to_string()
}

fn wait_for_process_exit(pid: i32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if unsafe { libc::kill(pid, 0) } != 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("shim process {pid} did not exit after SIGKILL");
}

fn superblock_incompat_features(path: &Path) -> u32 {
    let mut file = File::open(path).expect("open ext4 disk");
    file.seek(SeekFrom::Start(1024 + 0x60))
        .expect("seek to ext4 feature field");
    let mut bytes = [0u8; 4];
    file.read_exact(&mut bytes)
        .expect("read ext4 feature field");
    u32::from_le_bytes(bytes)
}

fn disk_observation(path: &Path) -> (u64, std::time::SystemTime) {
    let metadata = std::fs::metadata(path).expect("rootfs disk metadata");
    (
        metadata.len(),
        metadata.modified().expect("rootfs disk modification time"),
    )
}

fn archive_contains(path: &Path, expected: &str) -> bool {
    let file = File::open(path).expect("open exported rootfs archive");
    let mut archive = tar::Archive::new(file);
    archive
        .entries()
        .expect("read exported rootfs archive")
        .any(|entry| {
            let entry = entry.expect("read exported rootfs entry");
            let path = entry.path().expect("read exported rootfs path");
            let mut components = path.components().filter_map(|component| match component {
                std::path::Component::Normal(value) => Some(value),
                std::path::Component::CurDir => None,
                _ => panic!("unsafe path in exported archive: {}", path.display()),
            });
            components.next().is_some_and(|value| value == expected) && components.next().is_none()
        })
}
