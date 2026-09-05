use super::*;

#[cfg(not(target_os = "windows"))]
mod raw_disk_ownership;

#[test]
fn validates_directory_rootfs_source() {
    let temp = tempfile::tempdir().unwrap();

    validate_rootfs_source(&RootfsSource::directory(temp.path())).unwrap();
}

#[test]
fn rejects_file_as_directory_rootfs_source() {
    let temp = tempfile::tempdir().unwrap();
    let rootfs = temp.path().join("rootfs");
    std::fs::write(&rootfs, b"not a directory").unwrap();

    let error = validate_rootfs_source(&RootfsSource::directory(&rootfs))
        .unwrap_err()
        .to_string();

    assert!(error.contains("not a directory"));
    assert!(error.contains(&rootfs.display().to_string()));
}

#[cfg(not(target_os = "windows"))]
#[test]
fn validates_nonempty_ext4_disk_rootfs_source() {
    let temp = tempfile::tempdir().unwrap();
    let rootfs = temp.path().join("rootfs.ext4");
    std::fs::write(&rootfs, b"ext4 image bytes").unwrap();

    validate_rootfs_source(&RootfsSource::ext4_disk(rootfs, false)).unwrap();
}

#[test]
fn rejects_empty_ext4_disk_rootfs_source() {
    let temp = tempfile::tempdir().unwrap();
    let rootfs = temp.path().join("rootfs.ext4");
    std::fs::File::create(&rootfs).unwrap();

    let error = validate_rootfs_source(&RootfsSource::ext4_disk(&rootfs, false))
        .unwrap_err()
        .to_string();

    assert!(error.contains("is empty"));
    assert!(error.contains(&rootfs.display().to_string()));
}

#[cfg(unix)]
#[test]
fn rejects_symlink_as_ext4_disk_rootfs_source() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("target.ext4");
    let rootfs = temp.path().join("rootfs.ext4");
    std::fs::write(&target, b"ext4 image bytes").unwrap();
    std::os::unix::fs::symlink(&target, &rootfs).unwrap();

    let error = validate_rootfs_source(&RootfsSource::ext4_disk(&rootfs, false))
        .unwrap_err()
        .to_string();

    assert!(error.contains("not a regular file"));
    assert!(error.contains(&rootfs.display().to_string()));
}

#[cfg(target_os = "linux")]
#[test]
fn parses_sandbox_worker_pid_identity_after_complex_comm() {
    let stat = "123 (a3s oci (sandbox) worker) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 4242";
    assert_eq!(linux_process_identity_from_stat(stat), Some(('S', 4242)));
    assert_eq!(linux_process_identity_from_stat("malformed"), None);
}

#[cfg(target_os = "linux")]
#[test]
fn validates_complete_sandbox_log_worker_identity() {
    let mut spec = a3s_box_core::log::SandboxLogWorkerSpec {
        schema: a3s_box_core::log::SANDBOX_LOG_WORKER_SCHEMA.to_string(),
        box_id: "sandbox-id".to_string(),
        console_log: std::path::PathBuf::from("/tmp/sandbox-id/console.log"),
        log_config: a3s_box_core::log::LogConfig::default(),
        watched_pid: 123,
        watched_pid_start_time: 456,
        ready_file: std::path::PathBuf::from("/tmp/sandbox-id/log-worker.ready"),
    };
    validate_sandbox_log_worker_spec(&spec).unwrap();

    spec.watched_pid_start_time = 0;
    assert!(validate_sandbox_log_worker_spec(&spec).is_err());
}

#[cfg(target_os = "windows")]
#[test]
fn test_windows_kernel_format_from_magic() {
    assert_eq!(
        kernel_format_from_magic([0x7f, b'E', b'L', b'F']),
        Some(KRUN_KERNEL_FORMAT_ELF)
    );
    assert_eq!(
        kernel_format_from_magic([b'M', b'Z', 0x90, 0x00]),
        Some(KRUN_KERNEL_FORMAT_IMAGE_GZ)
    );
    assert_eq!(kernel_format_from_magic([0, 1, 2, 3]), None);
}

#[cfg(target_os = "windows")]
#[test]
fn test_prepare_windows_guest_rejects_smp() {
    let spec = InstanceSpec {
        vcpus: 2,
        ..InstanceSpec::default()
    };

    let error = prepare_windows_guest(&spec).unwrap_err().to_string();
    assert!(error.contains("WHPX"));
    assert!(error.contains("--cpus 1"));
}

#[cfg(target_os = "windows")]
#[test]
fn test_prepare_windows_guest_resets_stale_results_and_streams() {
    let temp = tempfile::tempdir().unwrap();
    let rootfs = temp.path().join("rootfs");
    fs::create_dir_all(&rootfs).unwrap();
    for name in [
        WINDOWS_GUEST_EXIT_CODE,
        WINDOWS_GUEST_STDOUT,
        WINDOWS_GUEST_STDERR,
        WINDOWS_GUEST_RESULT_MARKER,
        WINDOWS_LIVE_LOGS_DRAINED_MARKER,
    ] {
        fs::write(rootfs.join(name), b"stale").unwrap();
    }

    let spec = InstanceSpec {
        vcpus: 1,
        rootfs: RootfsSource::directory(rootfs.clone()),
        ..InstanceSpec::default()
    };
    prepare_windows_guest(&spec).unwrap();

    for name in [
        WINDOWS_GUEST_EXIT_CODE,
        WINDOWS_GUEST_RESULT_MARKER,
        WINDOWS_LIVE_LOGS_DRAINED_MARKER,
    ] {
        assert!(!rootfs.join(name).exists());
    }
    for name in [WINDOWS_GUEST_STDOUT, WINDOWS_GUEST_STDERR] {
        let path = rootfs.join(name);
        assert!(path.is_file());
        assert_eq!(fs::metadata(path).unwrap().len(), 0);
    }
}

#[cfg(target_os = "windows")]
#[test]
fn test_prepare_windows_guest_unlinks_stream_reparse_without_touching_target() {
    let temp = tempfile::tempdir().unwrap();
    let rootfs = temp.path().join("rootfs");
    fs::create_dir_all(&rootfs).unwrap();
    let host_target = temp.path().join("host-target.txt");
    fs::write(&host_target, b"host secret").unwrap();
    let stream = rootfs.join(WINDOWS_GUEST_STDOUT);
    match std::os::windows::fs::symlink_file(&host_target, &stream) {
        Ok(()) => {}
        Err(error) if matches!(error.raw_os_error(), Some(5) | Some(1314)) => return,
        Err(error) => panic!("failed to create stream symlink: {error}"),
    }

    let spec = InstanceSpec {
        vcpus: 1,
        rootfs: RootfsSource::directory(rootfs),
        ..InstanceSpec::default()
    };
    prepare_windows_guest(&spec).unwrap();

    assert_eq!(fs::read(&host_target).unwrap(), b"host secret");
    assert_eq!(fs::read(&stream).unwrap(), b"");
    assert!(!fs::symlink_metadata(stream)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn test_parse_ulimit_nofile() {
    assert_eq!(
        parse_ulimit("nofile=1024:4096"),
        Some("7=1024:4096".to_string())
    );
}

#[test]
fn test_parse_ulimit_nproc() {
    assert_eq!(parse_ulimit("nproc=256:512"), Some("6=256:512".to_string()));
}

#[test]
fn test_parse_ulimit_stack() {
    assert_eq!(
        parse_ulimit("stack=8192:8192"),
        Some("3=8192:8192".to_string())
    );
}

#[test]
fn test_parse_ulimit_core() {
    assert_eq!(parse_ulimit("core=0:0"), Some("4=0:0".to_string()));
}

#[test]
fn test_parse_ulimit_case_insensitive() {
    assert_eq!(
        parse_ulimit("NOFILE=1024:4096"),
        Some("7=1024:4096".to_string())
    );
    assert_eq!(parse_ulimit("Nproc=100:200"), Some("6=100:200".to_string()));
}

#[test]
fn test_parse_ulimit_unknown() {
    assert_eq!(parse_ulimit("unknown=1:2"), None);
}

#[test]
fn test_parse_ulimit_no_equals() {
    assert_eq!(parse_ulimit("nofile"), None);
}

#[test]
fn test_parse_ulimit_all_resources() {
    assert!(parse_ulimit("cpu=10:20").is_some());
    assert!(parse_ulimit("fsize=100:200").is_some());
    assert!(parse_ulimit("data=100:200").is_some());
    assert!(parse_ulimit("locks=100:200").is_some());
    assert!(parse_ulimit("memlock=100:200").is_some());
    assert!(parse_ulimit("msgqueue=100:200").is_some());
    assert!(parse_ulimit("nice=10:20").is_some());
    assert!(parse_ulimit("rss=100:200").is_some());
    assert!(parse_ulimit("rtprio=10:20").is_some());
    assert!(parse_ulimit("rttime=100:200").is_some());
    assert!(parse_ulimit("sigpending=100:200").is_some());
}

#[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
#[test]
fn test_tsi_port_map_for_spec_filters_auto_assigned_host_ports() {
    let spec = InstanceSpec {
        port_map: vec![
            "0:80".to_string(),
            "8080:80".to_string(),
            "9090:90".to_string(),
        ],
        ..Default::default()
    };

    assert_eq!(
        tsi_port_map_for_spec(&spec),
        vec!["8080:80".to_string(), "9090:90".to_string()]
    );
}

#[cfg(target_os = "macos")]
#[test]
fn test_tsi_port_map_for_spec_skips_macos_bridge_ports() {
    let spec = InstanceSpec {
        port_map: vec!["8080:80".to_string()],
        network: Some(test_network_config()),
        ..Default::default()
    };

    assert!(tsi_port_map_for_spec(&spec).is_empty());
}

#[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
#[test]
fn test_tsi_port_map_for_spec_skips_linux_bridge_ports() {
    // On Linux, bridge-mode ports are forwarded by passt, not TSI.
    let spec = InstanceSpec {
        port_map: vec!["8080:80".to_string()],
        network: Some(test_network_config()),
        ..Default::default()
    };

    assert!(tsi_port_map_for_spec(&spec).is_empty());
}

#[cfg(not(target_os = "windows"))]
fn test_network_config() -> a3s_box_core::vmm::NetworkInstanceConfig {
    a3s_box_core::vmm::NetworkInstanceConfig {
        net_socket_path: std::path::PathBuf::from("/tmp/a3s-box-test-net.sock"),
        net_stats_path: Some(std::path::PathBuf::from("/tmp/a3s-box-test-net.stats.json")),
        #[cfg(unix)]
        net_socket_fd: Some(42),
        #[cfg(unix)]
        net_proxy_fd: Some(43),
        #[cfg(unix)]
        bridge_socket_dir: Some(std::path::PathBuf::from("/tmp/a3s-switch")),
        ip_address: "10.89.0.2".parse().unwrap(),
        gateway: "10.89.0.1".parse().unwrap(),
        prefix_len: 24,
        mac_address: [0x02, 0x42, 0x0a, 0x59, 0x00, 0x02],
        dns_servers: vec!["8.8.8.8".parse().unwrap()],
    }
}

#[cfg(target_os = "linux")]
#[test]
fn test_parse_cpuset_spec_single() {
    assert_eq!(parse_cpuset_spec("0").unwrap(), vec![0]);
    assert_eq!(parse_cpuset_spec("3").unwrap(), vec![3]);
}

#[cfg(target_os = "linux")]
#[test]
fn test_parse_cpuset_spec_list() {
    assert_eq!(parse_cpuset_spec("0,1,3").unwrap(), vec![0, 1, 3]);
}

#[cfg(target_os = "linux")]
#[test]
fn test_parse_cpuset_spec_range() {
    assert_eq!(parse_cpuset_spec("0-3").unwrap(), vec![0, 1, 2, 3]);
}

#[cfg(target_os = "linux")]
#[test]
fn test_parse_cpuset_spec_mixed() {
    assert_eq!(parse_cpuset_spec("0,2-4,7").unwrap(), vec![0, 2, 3, 4, 7]);
}

#[cfg(target_os = "linux")]
#[test]
fn test_parse_cpuset_spec_invalid_range() {
    assert!(parse_cpuset_spec("3-1").is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn test_parse_cpuset_spec_invalid_number() {
    assert!(parse_cpuset_spec("abc").is_err());
}
