use super::super::BoxState;
use super::*;
use crate::cache::RootfsCache;
use a3s_box_core::config::BoxConfig;
use a3s_box_core::{SnapshotImageConfig, SnapshotMetadata};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::RwLock;

struct RuntimeSocketDirGuard(PathBuf);

impl Drop for RuntimeSocketDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct ResumeOnlyProvider {
    resumed: crate::rootfs::ResumedRootfs,
}

impl crate::rootfs::RootfsProvider for ResumeOnlyProvider {
    fn resume_for_boot(
        &self,
        _box_dir: &Path,
        options: crate::rootfs::RootfsResumeOptions,
    ) -> a3s_box_core::Result<Option<crate::rootfs::ResumedRootfs>> {
        assert!(options.persistent);
        Ok(Some(self.resumed.clone()))
    }

    fn prepare(&self, _box_dir: &Path, _cache_dir: &Path) -> a3s_box_core::Result<PathBuf> {
        Err(a3s_box_core::error::BoxError::StateError(
            "resume-only provider must not prepare a host tree".to_string(),
        ))
    }

    fn cleanup(&self, _box_dir: &Path, _persistent: bool) -> a3s_box_core::Result<()> {
        Ok(())
    }

    fn name(&self) -> &'static str {
        "resume-only-test"
    }
}

fn make_vm_manager_with_home(home_dir: &Path) -> VmManager {
    use a3s_box_core::event::EventEmitter;
    let config = BoxConfig::default();
    let emitter = EventEmitter::new(10);
    VmManager {
        config,
        box_id: "test-box".to_string(),
        boot_mode: super::super::VmBootMode::Workload,
        state: Arc::new(RwLock::new(BoxState::Created)),
        event_emitter: emitter,
        provider: None,
        handler: Arc::new(RwLock::new(None)),
        #[cfg(unix)]
        exec_client: None,
        net_manager: None,
        home_dir: home_dir.to_path_buf(),
        anonymous_volumes: Vec::new(),
        created_anonymous_volumes: Vec::new(),
        image_config: None,
        restore_rootfs_cache_key: None,
        healthcheck_disabled: false,
        preserve_rootfs_on_boot_failure: false,
        #[cfg(unix)]
        tee: None,
        rootfs_provider: crate::rootfs::default_provider(),
        exec_socket_path: None,
        pty_socket_path: None,
        port_forward_socket_path: None,
        prom: None,
        shim_exit_code: None,
        pull_progress_fn: None,
        log_config: a3s_box_core::log::LogConfig::default(),
        resolved_execution_plan: None,
        managed_secret_root: None,
        transient_registry_auth: None,
    }
}

fn write_static_test_elf(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut elf = vec![0_u8; 64];
    elf[0..4].copy_from_slice(b"\x7fELF");
    elf[4] = 2;
    elf[5] = 1;
    let machine = match std::env::consts::ARCH {
        "x86_64" => 62_u16,
        "aarch64" => 183_u16,
        _ => 0_u16,
    };
    elf[0x12..0x14].copy_from_slice(&machine.to_le_bytes());
    std::fs::write(path, elf).unwrap();
}

#[test]
fn installed_guest_init_cannot_be_shadowed_by_a_stale_development_build() {
    let temporary = TempDir::new().unwrap();
    let installed = temporary.path().join("home/bin/a3s-box-guest-init");
    let development = temporary
        .path()
        .join("target/x86_64-unknown-linux-musl/debug/a3s-box-guest-init");
    write_static_test_elf(&installed);
    write_static_test_elf(&development);

    assert_eq!(
        VmManager::select_guest_init(&installed, vec![development]),
        Some(installed)
    );
}

#[test]
fn invalid_installed_guest_init_falls_back_to_a_static_development_build() {
    let temporary = TempDir::new().unwrap();
    let installed = temporary.path().join("home/bin/a3s-box-guest-init");
    let development = temporary
        .path()
        .join("target/x86_64-unknown-linux-musl/release/a3s-box-guest-init");
    std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
    std::fs::write(&installed, b"not an ELF executable").unwrap();
    write_static_test_elf(&development);

    assert_eq!(
        VmManager::select_guest_init(&installed, vec![development.clone()]),
        Some(development)
    );
}

#[test]
fn release_guest_init_wins_over_a_stale_debug_artifact() {
    let temporary = TempDir::new().unwrap();
    let installed = temporary.path().join("home/bin/a3s-box-guest-init");
    let debug = temporary
        .path()
        .join("target/aarch64-unknown-linux-musl/debug/a3s-box-guest-init");
    let release = temporary
        .path()
        .join("target/aarch64-unknown-linux-musl/release/a3s-box-guest-init");
    write_static_test_elf(&debug);
    write_static_test_elf(&release);

    assert_eq!(
        VmManager::select_guest_init(&installed, vec![debug, release.clone()]),
        Some(release)
    );
}

#[test]
fn guest_init_for_a_different_machine_is_rejected() {
    let temporary = TempDir::new().unwrap();
    let path = temporary.path().join("a3s-box-guest-init");
    write_static_test_elf(&path);
    let mut bytes = std::fs::read(&path).unwrap();
    let other_machine = if std::env::consts::ARCH == "aarch64" {
        62_u16
    } else {
        183_u16
    };
    bytes[0x12..0x14].copy_from_slice(&other_machine.to_le_bytes());
    std::fs::write(&path, bytes).unwrap();

    assert!(!VmManager::is_linux_elf(&path));
}

#[cfg(unix)]
#[test]
fn sandbox_runtime_socket_stays_below_the_unix_path_limit_for_long_homes() {
    let long_home = PathBuf::from("/var/lib")
        .join("a3s-home-component".repeat(16))
        .join("nested-provider-namespace");
    let box_id = "01234567-89ab-cdef-0123-456789abcdef";
    let runtime_root = sandbox_runtime_root(&long_home, box_id);
    let runtime_socket = runtime_root.join("runtime.sock");

    assert!(runtime_root.starts_with(runtime_socket_dir(&long_home, box_id)));
    assert!(runtime_socket.as_os_str().len() < 108);
    assert!(!runtime_root.starts_with(&long_home));
}

fn image_health_check(test: &[&str]) -> crate::oci::OciHealthCheck {
    crate::oci::OciHealthCheck {
        test: test.iter().map(|part| (*part).to_string()).collect(),
        interval: None,
        timeout: None,
        retries: None,
        start_period: None,
    }
}

#[test]
fn image_health_support_is_platform_aware_and_honors_disable() {
    let enabled = image_health_check(&["CMD", "/bin/true"]);
    let result = validate_image_health_support(Some(&enabled), false);

    if cfg!(windows) {
        let error = result.expect_err("Windows must reject effective image health checks");
        assert!(error
            .to_string()
            .contains("health checks are not supported on Windows"));
    } else {
        result.expect("Unix guests support image health checks");
    }

    validate_image_health_support(Some(&enabled), true)
        .expect("an explicitly disabled image health check must not block boot");
    validate_image_health_support(Some(&image_health_check(&["NONE"])), false)
        .expect("Docker NONE is not an effective health check");
    validate_image_health_support(Some(&image_health_check(&["CMD"])), false)
        .expect("an empty CMD is not an effective health check");
}

#[tokio::test]
async fn persistent_guest_owned_layout_skips_image_pull_and_host_staging() {
    let home = TempDir::new().unwrap();
    let mut vm = make_vm_manager_with_home(home.path());
    vm.box_id = format!("persistent-layout-{}", uuid::Uuid::new_v4().simple());
    vm.config.image = "example.invalid/must-not-pull:latest".to_string();
    vm.config.persistent = true;
    let box_dir = home.path().join("boxes").join(&vm.box_id);
    std::fs::create_dir_all(&box_dir).unwrap();
    let config = crate::oci::OciImageConfig {
        entrypoint: Some(vec!["/bin/app".to_string()]),
        cmd: Some(vec!["--resume".to_string()]),
        env: vec![],
        working_dir: None,
        user: None,
        exposed_ports: vec![],
        labels: std::collections::HashMap::new(),
        volumes: vec![],
        stop_signal: None,
        health_check: None,
        onbuild: vec![],
    };
    crate::resolved_image::persist_resolved_image_config(&box_dir, &config).unwrap();
    let disk = box_dir.join("rootfs-ext4-v1/rootfs.ext4");
    vm.rootfs_provider = Box::new(ResumeOnlyProvider {
        resumed: crate::rootfs::ResumedRootfs {
            source: a3s_box_core::vmm::RootfsSource::ext4_disk(&disk, false),
            guest_init_exec: "/sbin/init".to_string(),
        },
    });
    let _socket_dir_guard = RuntimeSocketDirGuard(vm.socket_dir());

    let layout = vm.prepare_layout().await.unwrap();

    assert_eq!(
        layout.resumed_rootfs.as_ref().unwrap().source,
        a3s_box_core::vmm::RootfsSource::ext4_disk(disk, false)
    );
    assert_eq!(
        layout.oci_config.as_ref().unwrap().cmd,
        Some(vec!["--resume".to_string()])
    );
    assert!(!box_dir.join("rootfs").exists());
    assert!(!home.path().join("images").exists());
}

#[test]
fn vm_image_auth_uses_the_managers_explicit_home() {
    let home = TempDir::new().unwrap();
    let store = crate::oci::CredentialStore::new(home.path().join("auth/credentials.json"));
    store
        .store(
            "manager-layout.invalid:5443",
            "layout-user",
            "layout-secret",
        )
        .unwrap();

    let auth = registry_auth_for_image(
        home.path(),
        "manager-layout.invalid:5443/a3s/private:latest",
        None,
    )
    .unwrap();

    assert_eq!(
        auth.basic_credentials(),
        Some(("layout-user".to_string(), "layout-secret".to_string()))
    );
}

#[test]
fn transient_image_auth_overrides_the_persistent_store() {
    let home = TempDir::new().unwrap();
    let store = crate::oci::CredentialStore::new(home.path().join("auth/credentials.json"));
    store
        .store(
            "manager-layout.invalid:5443",
            "persistent-user",
            "persistent-secret",
        )
        .unwrap();

    let auth = registry_auth_for_image(
        home.path(),
        "manager-layout.invalid:5443/a3s/private:latest",
        Some(crate::RegistryAuth::basic(
            "transient-user",
            "transient-secret",
        )),
    )
    .unwrap();

    assert_eq!(
        auth.basic_credentials(),
        Some(("transient-user".to_string(), "transient-secret".to_string()))
    );
}

#[test]
fn explicit_anonymous_image_auth_disables_the_persistent_store() {
    let home = TempDir::new().unwrap();
    let store = crate::oci::CredentialStore::new(home.path().join("auth/credentials.json"));
    store
        .store(
            "manager-layout.invalid:5443",
            "persistent-user",
            "persistent-secret",
        )
        .unwrap();

    let auth = registry_auth_for_image(
        home.path(),
        "manager-layout.invalid:5443/a3s/private:latest",
        Some(crate::RegistryAuth::anonymous()),
    )
    .unwrap();

    assert!(auth.basic_credentials().is_none());
}

#[test]
fn test_snapshot_lower_dir_marker() {
    let tmp = TempDir::new().unwrap();
    let box_dir = tmp.path();
    // missing marker -> None
    assert!(snapshot_lower_dir(box_dir).is_none());
    // blank marker -> None
    std::fs::write(box_dir.join(".snapshot-lower"), "  \n").unwrap();
    assert!(snapshot_lower_dir(box_dir).is_none());
    // populated marker -> trimmed path
    std::fs::write(
        box_dir.join(".snapshot-lower"),
        "/root/.a3s/snapshots/snap-1/rootfs\n",
    )
    .unwrap();
    assert_eq!(
        snapshot_lower_dir(box_dir),
        Some(PathBuf::from("/root/.a3s/snapshots/snap-1/rootfs"))
    );
}

#[test]
fn retained_rootfs_cache_marker_is_strict_and_canonical() {
    let temporary = TempDir::new().unwrap();
    let box_dir = temporary.path();
    assert_eq!(retained_rootfs_cache_key(box_dir).unwrap(), None);

    std::fs::write(box_dir.join(".rootfs-cache-key"), "not-a-digest\n").unwrap();
    assert!(retained_rootfs_cache_key(box_dir).is_err());

    let uppercase = "A".repeat(64);
    std::fs::write(box_dir.join(".rootfs-cache-key"), format!(" {uppercase}\n")).unwrap();
    assert_eq!(
        retained_rootfs_cache_key(box_dir).unwrap(),
        Some("a".repeat(64))
    );
}

#[test]
fn snapshot_restore_requires_its_exact_cached_rootfs() {
    let cache_key = "a".repeat(64);
    let cached_path = PathBuf::from("cached-rootfs");

    assert!(require_snapshot_restore_rootfs(None, Some(cached_path.clone())).is_err());
    assert!(require_snapshot_restore_rootfs(Some(&cache_key), None).is_err());
    assert_eq!(
        require_snapshot_restore_rootfs(Some(&cache_key), Some(cached_path.clone())).unwrap(),
        (cache_key.as_str(), cached_path)
    );
}

#[test]
fn persistent_rootfs_generation_detection_ignores_empty_directories() {
    let temporary = TempDir::new().unwrap();
    let box_dir = temporary.path();
    std::fs::create_dir(box_dir.join("rootfs")).unwrap();
    std::fs::create_dir(box_dir.join("upper")).unwrap();
    assert!(!persistent_rootfs_generation_exists(box_dir).unwrap());

    std::fs::write(box_dir.join("upper/.a3s_rootfs_metadata_v1.json"), b"{}").unwrap();
    assert!(persistent_rootfs_generation_exists(box_dir).unwrap());
}

#[tokio::test]
async fn snapshot_lower_layout_restores_the_resolved_image_entrypoint() {
    let home = TempDir::new().unwrap();
    let snapshot_id = "snapshot-with-image-config";
    let snapshot_dir = home.path().join("snapshots").join(snapshot_id);
    let lower = snapshot_dir.join("rootfs");
    std::fs::create_dir_all(lower.join("usr/local/bin")).unwrap();
    std::fs::write(lower.join("usr/local/bin/envd"), b"envd").unwrap();

    let mut metadata = SnapshotMetadata::new(
        snapshot_id.to_string(),
        snapshot_id.to_string(),
        "source-box".to_string(),
        "example.invalid/runtime:latest".to_string(),
    );
    metadata.image_config = Some(SnapshotImageConfig {
        entrypoint: Some(vec!["/usr/local/bin/envd".to_string()]),
        cmd: Some(vec!["--port".to_string(), "49983".to_string()]),
        env: vec![("RUNTIME".to_string(), "a3s".to_string())],
        working_dir: Some("/home/user".to_string()),
        user: Some("1000:1000".to_string()),
        ..Default::default()
    });
    std::fs::write(
        snapshot_dir.join("metadata.json"),
        serde_json::to_vec_pretty(&metadata).unwrap(),
    )
    .unwrap();

    let mut vm = make_vm_manager_with_home(home.path());
    vm.box_id = format!("layout-test-{}", uuid::Uuid::new_v4().simple());
    let _socket_dir_guard = RuntimeSocketDirGuard(vm.socket_dir());
    let box_dir = home.path().join("boxes").join(&vm.box_id);
    std::fs::create_dir_all(&box_dir).unwrap();
    std::fs::write(
        box_dir.join(".snapshot-lower"),
        lower.to_string_lossy().as_bytes(),
    )
    .unwrap();
    vm.config.image = "example.invalid/runtime:latest".to_string();
    vm.rootfs_provider = Box::new(crate::rootfs::CopyProvider);

    let layout = vm.prepare_layout().await.unwrap();
    let image_config = layout
        .oci_config
        .as_ref()
        .expect("snapshot layout must restore the resolved image configuration");
    assert_eq!(
        image_config.entrypoint,
        Some(vec!["/usr/local/bin/envd".to_string()])
    );
    assert_eq!(
        image_config.cmd,
        Some(vec!["--port".to_string(), "49983".to_string()])
    );

    // Keep the assertion independent of whether a guest-init test artifact is
    // available next to the test binary on this host.
    let _ = std::fs::remove_file(layout.rootfs_path.join("sbin/init"));
    let spec = vm.build_instance_spec(&layout).unwrap();
    assert_eq!(spec.entrypoint.executable, "/usr/local/bin/envd");
    assert_eq!(spec.entrypoint.args, vec!["--port", "49983"]);
    assert!(spec
        .entrypoint
        .env
        .iter()
        .any(|(key, value)| key == "RUNTIME" && value == "a3s"));
    assert_eq!(spec.workdir, "/home/user");
}

#[test]
fn test_resolve_cache_dir_default() {
    let tmp = TempDir::new().unwrap();
    let vm = make_vm_manager_with_home(tmp.path());

    let cache_dir = vm.resolve_cache_dir();
    assert_eq!(cache_dir, tmp.path().join("cache"));
}

#[test]
fn test_resolve_cache_dir_custom() {
    let tmp = TempDir::new().unwrap();
    let mut vm = make_vm_manager_with_home(tmp.path());
    vm.config.cache.cache_dir = Some(PathBuf::from("/custom/cache"));

    let cache_dir = vm.resolve_cache_dir();
    assert_eq!(cache_dir, PathBuf::from("/custom/cache"));
}

#[test]
fn test_try_rootfs_cache_disabled() {
    let tmp = TempDir::new().unwrap();
    let mut vm = make_vm_manager_with_home(tmp.path());
    vm.config.cache.enabled = false;

    let target = tmp.path().join("target");
    let result = vm.try_rootfs_cache("some_key", &target).unwrap();
    assert!(result.is_none());
}

#[test]
fn test_try_rootfs_cache_miss() {
    let tmp = TempDir::new().unwrap();
    let vm = make_vm_manager_with_home(tmp.path());

    let target = tmp.path().join("target");
    let result = vm.try_rootfs_cache("nonexistent_key", &target).unwrap();
    assert!(result.is_none());
}

#[test]
fn test_try_rootfs_cache_hit() {
    let tmp = TempDir::new().unwrap();
    let vm = make_vm_manager_with_home(tmp.path());

    // Pre-populate the cache
    let cache_dir = tmp.path().join("cache").join("rootfs");
    let cache = RootfsCache::new(&cache_dir).unwrap();
    let source = tmp.path().join("source_rootfs");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("agent.bin"), "binary").unwrap();
    cache.put("test_key", &source, "test").unwrap();

    // Now try_rootfs_cache should hit
    let target = tmp.path().join("target_rootfs");
    let result = vm.try_rootfs_cache("test_key", &target).unwrap();
    assert!(result.is_some());
    assert_eq!(result.unwrap(), target);
    assert!(target.join("agent.bin").is_file());
    assert_eq!(
        std::fs::read_to_string(target.join("agent.bin")).unwrap(),
        "binary"
    );
}

#[test]
fn test_store_rootfs_cache_disabled() {
    let tmp = TempDir::new().unwrap();
    let mut vm = make_vm_manager_with_home(tmp.path());
    vm.config.cache.enabled = false;

    let source = tmp.path().join("rootfs");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("f.txt"), "data").unwrap();

    // Should not store anything
    vm.store_rootfs_cache("key", &source, "test");

    // Cache directory should not even be created
    let cache_dir = tmp.path().join("cache").join("rootfs");
    assert!(!cache_dir.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn guest_native_provider_skips_the_legacy_apfs_cache() {
    let tmp = TempDir::new().unwrap();
    let mut vm = make_vm_manager_with_home(tmp.path());
    vm.set_rootfs_provider(Box::new(crate::rootfs::GuestNativeExt4Provider));

    vm.store_rootfs_cache(
        "guest-native",
        &tmp.path().join("missing-staging-root"),
        "guest-native image",
    );

    assert!(!tmp.path().join("cache/rootfs-apfs-v2").exists());
}

#[cfg(not(target_os = "macos"))]
#[test]
fn test_store_rootfs_cache_success() {
    let tmp = TempDir::new().unwrap();
    let vm = make_vm_manager_with_home(tmp.path());

    let source = tmp.path().join("rootfs");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("agent.bin"), "binary").unwrap();

    vm.store_rootfs_cache("store_key", &source, "test image");

    // Verify it was stored
    let cache_dir = tmp.path().join("cache").join("rootfs");
    let cache = RootfsCache::new(&cache_dir).unwrap();
    let result = cache.get("store_key").unwrap();
    assert!(result.is_some());
}

#[cfg(not(target_os = "macos"))]
#[test]
fn test_store_rootfs_cache_prunes_on_store() {
    let tmp = TempDir::new().unwrap();
    let mut vm = make_vm_manager_with_home(tmp.path());
    vm.config.cache.max_rootfs_entries = 2;

    let source = tmp.path().join("rootfs");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("f.txt"), "data").unwrap();

    // Store 3 entries (exceeds max_rootfs_entries=2)
    for i in 0..3 {
        vm.store_rootfs_cache(&format!("key{}", i), &source, &format!("entry {}", i));
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // After pruning, should have at most 2 entries
    let cache_dir = tmp.path().join("cache").join("rootfs");
    let cache = RootfsCache::new(&cache_dir).unwrap();
    assert!(cache.entry_count().unwrap() <= 2);
}

#[cfg(target_os = "macos")]
#[test]
fn test_prune_apfs_rootfs_cache_bounds_entries_and_protects_new_entry() {
    let tmp = TempDir::new().unwrap();
    let cache_dir = tmp.path().join("rootfs-apfs");
    std::fs::create_dir_all(&cache_dir).unwrap();

    for key in ["oldest", "middle", "new"] {
        std::fs::write(
            cache_dir.join(format!("{key}.sparseimage")),
            vec![b'x'; 4096],
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    std::fs::write(cache_dir.join(".partial.tmp-1"), b"temporary").unwrap();

    prune_apfs_rootfs_cache(&cache_dir, 1, u64::MAX, "new").unwrap();

    assert!(!cache_dir.join("oldest.sparseimage").exists());
    assert!(!cache_dir.join("middle.sparseimage").exists());
    assert!(cache_dir.join("new.sparseimage").exists());
    assert!(cache_dir.join(".partial.tmp-1").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn test_prune_apfs_rootfs_cache_uses_allocated_bytes_not_virtual_length() {
    use std::os::unix::fs::MetadataExt;

    let tmp = TempDir::new().unwrap();
    let cache_dir = tmp.path().join("rootfs-apfs");
    std::fs::create_dir_all(&cache_dir).unwrap();
    let old = cache_dir.join("old.sparseimage");
    let protected = cache_dir.join("protected.sparseimage");
    std::fs::write(&old, vec![b'x'; 8192]).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(&protected, vec![b'y'; 4096]).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&protected)
        .unwrap()
        .set_len(64 * 1024 * 1024 * 1024)
        .unwrap();
    let protected_allocated = protected.metadata().unwrap().blocks() * 512;

    prune_apfs_rootfs_cache(&cache_dir, usize::MAX, protected_allocated, "protected").unwrap();

    assert!(!old.exists());
    assert!(protected.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn test_exec_command_rejects_created_state() {
    let tmp = TempDir::new().unwrap();
    let vm = make_vm_manager_with_home(tmp.path());

    let result = vm.exec_command(vec!["echo".to_string()], 0).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("not yet booted"));
}

#[cfg(unix)]
#[tokio::test]
async fn test_exec_command_rejects_stopped_state() {
    let tmp = TempDir::new().unwrap();
    let vm = make_vm_manager_with_home(tmp.path());
    *vm.state.write().await = BoxState::Stopped;

    let result = vm.exec_command(vec!["echo".to_string()], 0).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("stopped"));
}

#[cfg(unix)]
#[tokio::test]
async fn test_exec_command_no_client() {
    let tmp = TempDir::new().unwrap();
    let vm = make_vm_manager_with_home(tmp.path());
    *vm.state.write().await = BoxState::Ready;

    let result = vm.exec_command(vec!["echo".to_string()], 0).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("not connected"));
}

#[cfg(unix)]
#[tokio::test]
async fn test_exec_request_rejects_empty_command() {
    let tmp = TempDir::new().unwrap();
    let vm = make_vm_manager_with_home(tmp.path());
    *vm.state.write().await = BoxState::Ready;

    let request = a3s_box_core::exec::ExecRequest {
        request_id: None,
        cmd: vec![],
        timeout_ns: 0,
        env: vec!["ENV=test".to_string()],
        working_dir: Some("/app".to_string()),
        rootfs: None,
        stdin: None,
        stdin_streaming: false,
        user: None,
        streaming: false,
    };
    let result = vm.exec_request(&request).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("non-empty command"));
}

#[cfg(unix)]
#[tokio::test]
async fn test_exec_request_no_client_preserves_request_fields() {
    let tmp = TempDir::new().unwrap();
    let vm = make_vm_manager_with_home(tmp.path());
    *vm.state.write().await = BoxState::Ready;

    let request = a3s_box_core::exec::ExecRequest {
        request_id: None,
        cmd: vec!["printenv".to_string()],
        timeout_ns: 123,
        env: vec!["ENV=test".to_string()],
        working_dir: Some("/app".to_string()),
        rootfs: Some("/run/a3s/cri/container-rootfs/sb/c/rootfs".to_string()),
        stdin: Some(b"input".to_vec()),
        stdin_streaming: false,
        user: Some("1000:1000".to_string()),
        streaming: false,
    };
    let result = vm.exec_request(&request).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("not connected"));
    assert_eq!(request.env, vec!["ENV=test".to_string()]);
    assert_eq!(request.working_dir, Some("/app".to_string()));
    assert_eq!(request.stdin, Some(b"input".to_vec()));
    assert_eq!(request.user, Some("1000:1000".to_string()));
}

#[cfg(not(target_os = "macos"))]
#[test]
fn test_try_and_store_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let vm = make_vm_manager_with_home(tmp.path());

    // First call: cache miss
    let target1 = tmp.path().join("target1");
    let result = vm.try_rootfs_cache("roundtrip_key", &target1).unwrap();
    assert!(result.is_none());

    // Build rootfs manually
    let built_rootfs = tmp.path().join("built");
    std::fs::create_dir_all(&built_rootfs).unwrap();
    std::fs::write(built_rootfs.join("init"), "init_binary").unwrap();
    std::fs::create_dir_all(built_rootfs.join("etc")).unwrap();
    std::fs::write(built_rootfs.join("etc/config"), "config_data").unwrap();

    // Store in cache
    vm.store_rootfs_cache("roundtrip_key", &built_rootfs, "roundtrip test");

    // Second call: cache hit
    let target2 = tmp.path().join("target2");
    let result = vm.try_rootfs_cache("roundtrip_key", &target2).unwrap();
    assert!(result.is_some());
    assert!(target2.join("init").is_file());
    assert_eq!(
        std::fs::read_to_string(target2.join("init")).unwrap(),
        "init_binary"
    );
    assert_eq!(
        std::fs::read_to_string(target2.join("etc/config")).unwrap(),
        "config_data"
    );
}
