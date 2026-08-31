use std::fs;
use std::path::Path;

use a3s_box_core::config::BoxConfig;
use a3s_box_core::event::EventEmitter;

use super::*;
use tempfile::tempdir;
use tempfile::TempDir;

#[cfg(unix)]
fn create_dir_symlink(target: &Path, link: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).unwrap();
    true
}

#[cfg(unix)]
fn create_file_symlink(target: &Path, link: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).unwrap();
    true
}

#[cfg(windows)]
fn create_file_symlink(target: &Path, link: &Path) -> bool {
    match std::os::windows::fs::symlink_file(target, link) {
        Ok(()) => true,
        Err(error) if error.raw_os_error() == Some(1314) => false,
        Err(error) => panic!("failed to create Windows test symlink: {error}"),
    }
}

#[cfg(windows)]
fn create_dir_symlink(target: &Path, link: &Path) -> bool {
    match std::os::windows::fs::symlink_dir(target, link) {
        Ok(()) => true,
        Err(error) if error.raw_os_error() == Some(1314) => false,
        Err(error) => panic!("failed to create Windows test symlink: {error}"),
    }
}

/// Decode a base64 (URL-safe, no pad) staged environment value the way
/// guest-init does, so assertions can compare against the original value.
fn b64d(s: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.as_bytes())
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_else(|| s.to_string())
}

fn test_oci_config(workdir: Option<&str>, user: Option<&str>) -> OciImageConfig {
    OciImageConfig {
        entrypoint: Some(vec!["/bin/app".to_string()]),
        cmd: Some(vec!["--serve".to_string()]),
        env: vec![],
        working_dir: workdir.map(str::to_string),
        user: user.map(str::to_string),
        exposed_ports: vec![],
        labels: std::collections::HashMap::new(),
        volumes: vec![],
        stop_signal: None,
        health_check: None,
        onbuild: vec![],
    }
}

fn test_layout(
    base: &Path,
    oci_config: Option<OciImageConfig>,
    with_guest_init: bool,
) -> BoxLayout {
    let rootfs_path = base.join("rootfs");
    fs::create_dir_all(&rootfs_path).unwrap();
    if with_guest_init {
        fs::create_dir_all(rootfs_path.join("sbin")).unwrap();
        fs::write(rootfs_path.join("sbin").join("init"), b"guest-init").unwrap();
    }

    BoxLayout {
        rootfs_path,
        resumed_rootfs: None,
        exec_socket_path: base.join("exec.sock"),
        pty_socket_path: base.join("pty.sock"),
        attest_socket_path: base.join("attest.sock"),
        port_forward_socket_path: base.join("portfwd.sock"),
        workspace_path: base.join("workspace"),
        console_output: None,
        oci_config,
        #[cfg(target_os = "macos")]
        oci_manifest_digest: None,
        prefer_image_rootfs_metadata: false,
        tee_instance_config: None,
    }
}

fn test_vm_manager(config: BoxConfig) -> VmManager {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;

    static TEST_HOME: OnceLock<TempDir> = OnceLock::new();
    static NEXT_HOME: AtomicU64 = AtomicU64::new(0);

    let mut manager = VmManager::with_box_id(config, EventEmitter::new(16), "test-box".to_string());
    let test_home = TEST_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    manager.home_dir = test_home.path().join(format!(
        "home-{}",
        NEXT_HOME.fetch_add(1, Ordering::Relaxed)
    ));
    manager
}

fn env_value<'a>(spec: &'a InstanceSpec, key: &str) -> Option<&'a str> {
    spec.entrypoint
        .env
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn staged_exec_config(layout: &BoxLayout) -> GuestExecConfig {
    serde_json::from_slice(&fs::read(layout.rootfs_path.join(".a3s-box-exec.json")).unwrap())
        .unwrap()
}

fn staged_boot_config(spec: &InstanceSpec) -> GuestBootConfig {
    let mount = spec
        .fs_mounts
        .iter()
        .find(|mount| mount.tag == GUEST_BOOT_CONTROL_TAG)
        .unwrap();
    serde_json::from_slice(&fs::read(mount.host_path.join(GUEST_BOOT_CONFIG_FILE_NAME)).unwrap())
        .unwrap()
}

#[path = "tests/boot.rs"]
mod boot;
#[path = "tests/general.rs"]
mod general;
