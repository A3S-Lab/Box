use super::super::*;

#[test]
fn validates_one_read_only_auxiliary_raw_disk() {
    let temp = tempfile::tempdir().unwrap();
    let rootfs = temp.path().join("rootfs");
    let disk = temp.path().join("data.ext4");
    std::fs::create_dir(&rootfs).unwrap();
    std::fs::write(&disk, b"raw disk bytes").unwrap();
    let spec = maintenance_spec(&rootfs, &disk);

    validate_raw_block_devices(&spec).unwrap();
}

#[test]
fn rejects_reserved_or_repeated_auxiliary_raw_disks() {
    let temp = tempfile::tempdir().unwrap();
    let rootfs = temp.path().join("rootfs");
    let disk = temp.path().join("data.ext4");
    std::fs::create_dir(&rootfs).unwrap();
    std::fs::write(&disk, b"raw disk bytes").unwrap();

    let reserved = InstanceSpec {
        rootfs: RootfsSource::directory(&rootfs),
        block_devices: vec![RawBlockDevice::new("rootfs", &disk, true)],
        ..InstanceSpec::default()
    };
    assert!(validate_raw_block_devices(&reserved)
        .unwrap_err()
        .to_string()
        .contains("reserved"));

    let repeated = InstanceSpec {
        rootfs: RootfsSource::directory(rootfs),
        block_devices: vec![
            RawBlockDevice::new("first", &disk, true),
            RawBlockDevice::new("second", &disk, true),
        ],
        ..InstanceSpec::default()
    };
    assert!(validate_raw_block_devices(&repeated)
        .unwrap_err()
        .to_string()
        .contains("more than once"));
}

#[cfg(unix)]
#[test]
fn raw_disk_ownership_lock_rejects_a_second_a3s_owner() {
    let temp = tempfile::tempdir().unwrap();
    let rootfs = temp.path().join("rootfs");
    let disk = temp.path().join("data.ext4");
    std::fs::create_dir(&rootfs).unwrap();
    std::fs::write(&disk, b"raw disk bytes").unwrap();
    let spec = maintenance_spec(&rootfs, &disk);

    let first_owner = lock_raw_disk_ownership(&spec).unwrap();
    assert_owned_elsewhere(&spec);

    drop(first_owner);
    lock_raw_disk_ownership(&spec).unwrap();
}

#[cfg(unix)]
const OWNER_CHILD_ENV: &str = "A3S_BOX_TEST_RAW_DISK_OWNER_CHILD";
#[cfg(unix)]
const OWNER_DISK_ENV: &str = "A3S_BOX_TEST_RAW_DISK_OWNER_DISK";
#[cfg(unix)]
const OWNER_ROOT_ENV: &str = "A3S_BOX_TEST_RAW_DISK_OWNER_ROOT";
#[cfg(unix)]
const OWNER_READY_ENV: &str = "A3S_BOX_TEST_RAW_DISK_OWNER_READY";
#[cfg(unix)]
const OWNER_ROLE_ENV: &str = "A3S_BOX_TEST_RAW_DISK_OWNER_ROLE";
#[cfg(unix)]
const OWNER_CHILD_TEST: &str =
    "tests::raw_disk_ownership::raw_disk_ownership_survives_owner_process_crashes";

/// Fault-inject owner-process crashes in both directions of the handoff.
///
/// The child process models the shim after it has validated and locked its raw
/// disk but before libkrun exits. Killing that process must release the kernel
/// lock, while every competing run or maintenance spec must fail closed until
/// the exact owner is gone.
#[cfg(unix)]
#[test]
fn raw_disk_ownership_survives_owner_process_crashes() {
    if std::env::var_os(OWNER_CHILD_ENV).is_some() {
        run_owner_child();
        return;
    }

    let temp = tempfile::tempdir().expect("temporary raw-disk ownership fixture");
    let rootfs = temp.path().join("maintenance-root");
    let disk = temp.path().join("rootfs.ext4");
    std::fs::create_dir(&rootfs).expect("create maintenance root");
    std::fs::write(&disk, b"raw disk bytes").expect("create raw disk fixture");

    let maintenance = maintenance_spec(&rootfs, &disk);
    let running = running_spec(&disk);

    let mut maintenance_owner = RawDiskOwnerChild::spawn(
        "maintenance",
        &rootfs,
        &disk,
        temp.path().join("maintenance.ready"),
        temp.path().join("maintenance.stderr"),
    );
    maintenance_owner.wait_until_ready();
    assert_owned_elsewhere(&running);
    maintenance_owner.terminate();
    drop(acquire_validated_ownership(&running).expect("run owns disk after maintenance crash"));

    let mut running_owner = RawDiskOwnerChild::spawn(
        "run",
        &rootfs,
        &disk,
        temp.path().join("run.ready"),
        temp.path().join("run.stderr"),
    );
    running_owner.wait_until_ready();
    assert_owned_elsewhere(&maintenance);
    running_owner.terminate();
    drop(
        acquire_validated_ownership(&maintenance)
            .expect("maintenance owns disk after running shim crash"),
    );
}

fn maintenance_spec(rootfs: &std::path::Path, disk: &std::path::Path) -> InstanceSpec {
    InstanceSpec {
        rootfs: RootfsSource::directory(rootfs),
        block_devices: vec![RawBlockDevice::new("a3s-rootfs", disk, true)],
        ..InstanceSpec::default()
    }
}

#[cfg(unix)]
fn running_spec(disk: &std::path::Path) -> InstanceSpec {
    InstanceSpec {
        rootfs: RootfsSource::ext4_disk(disk, false),
        ..InstanceSpec::default()
    }
}

#[cfg(unix)]
fn acquire_validated_ownership(spec: &InstanceSpec) -> Result<Vec<std::fs::File>> {
    validate_rootfs_source(&spec.rootfs)?;
    validate_raw_block_devices(spec)?;
    lock_raw_disk_ownership(spec)
}

#[cfg(unix)]
fn assert_owned_elsewhere(spec: &InstanceSpec) {
    let error = acquire_validated_ownership(spec)
        .expect_err("a second shim unexpectedly acquired the raw disk")
        .to_string();
    assert!(
        error.contains("already owned by another A3S VM"),
        "unexpected ownership error: {error}"
    );
}

#[cfg(unix)]
fn run_owner_child() {
    let disk = required_path(OWNER_DISK_ENV);
    let rootfs = required_path(OWNER_ROOT_ENV);
    let ready = required_path(OWNER_READY_ENV);
    let role = std::env::var(OWNER_ROLE_ENV).expect("raw-disk owner role");
    let spec = match role.as_str() {
        "maintenance" => maintenance_spec(&rootfs, &disk),
        "run" => running_spec(&disk),
        other => panic!("unexpected raw-disk owner role: {other}"),
    };
    let _ownership = acquire_validated_ownership(&spec).expect("child acquires raw disk");
    std::fs::write(&ready, b"ready").expect("publish raw-disk owner readiness");
    loop {
        std::thread::park();
    }
}

#[cfg(unix)]
fn required_path(name: &'static str) -> std::path::PathBuf {
    std::env::var_os(name)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| panic!("missing raw-disk ownership fixture environment {name}"))
}

#[cfg(unix)]
struct RawDiskOwnerChild {
    child: Option<std::process::Child>,
    ready: std::path::PathBuf,
    stderr: std::path::PathBuf,
}

#[cfg(unix)]
impl RawDiskOwnerChild {
    fn spawn(
        role: &str,
        rootfs: &std::path::Path,
        disk: &std::path::Path,
        ready: std::path::PathBuf,
        stderr: std::path::PathBuf,
    ) -> Self {
        let stderr_file = std::fs::File::create(&stderr).expect("create owner stderr file");
        let child = std::process::Command::new(
            std::env::current_exe().expect("resolve shim test executable"),
        )
        .args(["--exact", OWNER_CHILD_TEST, "--nocapture"])
        .env(OWNER_CHILD_ENV, "1")
        .env(OWNER_DISK_ENV, disk)
        .env(OWNER_ROOT_ENV, rootfs)
        .env(OWNER_READY_ENV, &ready)
        .env(OWNER_ROLE_ENV, role)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(stderr_file)
        .spawn()
        .expect("spawn raw-disk owner fixture");
        Self {
            child: Some(child),
            ready,
            stderr,
        }
    }

    fn wait_until_ready(&mut self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if self.ready.is_file() {
                return;
            }
            if let Some(status) = self
                .child
                .as_mut()
                .expect("raw-disk owner child")
                .try_wait()
                .expect("inspect raw-disk owner child")
            {
                let stderr = std::fs::read_to_string(&self.stderr).unwrap_or_default();
                panic!("raw-disk owner exited before readiness ({status}): {stderr}");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "raw-disk owner did not become ready: {}",
                std::fs::read_to_string(&self.stderr).unwrap_or_default()
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn terminate(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        child.kill().expect("fault-inject raw-disk owner crash");
        child.wait().expect("reap raw-disk owner fixture");
    }
}

#[cfg(unix)]
impl Drop for RawDiskOwnerChild {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = child.kill();
        let _ = child.wait();
    }
}
