use super::*;
use crate::sandbox::runtime_record::{SandboxRuntimeRecord, SANDBOX_RUNTIME_RECORD_SCHEMA};

fn write_runtime_record(
    home_dir: &Path,
    box_dir: &Path,
    box_id: &str,
    mutate: impl FnOnce(&mut SandboxRuntimeRecord),
) {
    let runtime_root = home_dir.join("run/a3s-oci").join(box_id);
    let mut record = SandboxRuntimeRecord {
        schema: SANDBOX_RUNTIME_RECORD_SCHEMA.to_string(),
        container_id: box_id.to_string(),
        runtime_path: Path::new("/definitely/missing/a3s-oci").to_path_buf(),
        runtime_sha256: Some("a".repeat(64)),
        agent_path: Some(Path::new("/definitely/missing/a3s-oci-agent").to_path_buf()),
        agent_sha256: Some("b".repeat(64)),
        runtime_root: runtime_root.clone(),
        runtime_socket: Some(runtime_root.join("runtime.sock")),
        bundle_dir: box_dir.join("sandbox/bundle"),
        init_pid: 42,
        generation: Some(7),
        owner_pid: Some(43),
        owner_pid_start_time: Some(11),
        log_worker_pid: None,
        log_worker_pid_start_time: None,
    };
    mutate(&mut record);
    std::fs::create_dir_all(box_dir.join("sandbox")).unwrap();
    std::fs::write(
        box_dir.join("sandbox/runtime.json"),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
}

#[test]
fn test_reap_removes_box_dir() {
    // A box dir with no live shim / mount (e.g. left by a crash) is removed.
    let home = tempfile::tempdir().unwrap();
    let box_id = "reap-test-no-such-shim-uuid";
    let box_dir = home.path().join("boxes").join(box_id);
    std::fs::create_dir_all(box_dir.join("logs")).unwrap();
    std::fs::write(box_dir.join("logs/shim.stdout.log"), b"x").unwrap();
    assert!(box_dir.exists());

    reap_orphaned_box_in(home.path(), box_id);
    assert!(!box_dir.exists(), "orphaned box dir should be removed");
}

#[test]
fn test_reap_absent_box_is_noop() {
    let home = tempfile::tempdir().unwrap();
    // No boxes/<id> dir at all - must not panic or error.
    reap_orphaned_box_in(home.path(), "absent-box-uuid");
}

#[test]
fn cleanup_absent_sandbox_runtime_preserves_box_directory() {
    let home = tempfile::tempdir().unwrap();
    let box_id = "cleanup-test-no-runtime-record";
    let box_dir = home.path().join("boxes").join(box_id);
    std::fs::create_dir_all(&box_dir).unwrap();

    cleanup_recorded_sandbox_runtime_in(home.path(), &box_dir, box_id).unwrap();

    assert!(box_dir.exists());
}

#[test]
fn recorded_sandbox_runtime_rejects_an_unexpected_box_directory() {
    let home = tempfile::tempdir().unwrap();
    let box_id = "recorded-sandbox-unexpected-directory";
    let box_dir = home.path().join("external").join(box_id);
    write_runtime_record(home.path(), &box_dir, box_id, |_| {});

    let error = load_recorded_sandbox_runtime(home.path(), &box_dir, box_id).unwrap_err();

    assert!(error.to_string().contains("unexpected host directory"));
}

#[test]
fn recorded_sandbox_runtime_rejects_invalid_paths() {
    let home = tempfile::tempdir().unwrap();
    let box_id = "recorded-sandbox-invalid-paths";
    let box_dir = home.path().join("boxes").join(box_id);
    write_runtime_record(home.path(), &box_dir, box_id, |record| {
        record.runtime_root = home.path().join("run/a3s-oci/another-box");
    });

    let error = load_recorded_sandbox_runtime(home.path(), &box_dir, box_id).unwrap_err();

    assert!(error.to_string().contains("path or identity validation"));
}

#[test]
fn cleanup_reports_the_exact_runtime_record_failure_and_preserves_the_rootfs() {
    let home = tempfile::tempdir().unwrap();
    let box_id = "cleanup-recorded-sandbox-invalid-paths";
    let box_dir = home.path().join("boxes").join(box_id);
    write_runtime_record(home.path(), &box_dir, box_id, |record| {
        record.runtime_root = home.path().join("run/a3s-oci/another-box");
    });

    let error = cleanup_recorded_sandbox_runtime_in(home.path(), &box_dir, box_id).unwrap_err();

    assert!(error.to_string().contains("path or identity validation"));
    assert!(error.to_string().contains("refusing to touch its rootfs"));
    assert!(box_dir.exists());
}

#[test]
fn recorded_sandbox_runtime_rejects_an_unknown_schema() {
    let home = tempfile::tempdir().unwrap();
    let box_id = "recorded-sandbox-unknown-schema";
    let box_dir = home.path().join("boxes").join(box_id);
    write_runtime_record(home.path(), &box_dir, box_id, |record| {
        record.schema = "unsupported".to_string();
    });

    let error = load_recorded_sandbox_runtime_identity(home.path(), &box_dir, box_id).unwrap_err();

    assert!(error.to_string().contains("path or identity validation"));
}

#[test]
fn structurally_valid_runtime_record_loads_before_owner_certification() {
    let home = tempfile::tempdir().unwrap();
    let box_id = "recorded-sandbox-a3s-oci";
    let box_dir = home.path().join("boxes").join(box_id);
    write_runtime_record(home.path(), &box_dir, box_id, |_| {});

    let record = load_recorded_sandbox_runtime_identity(home.path(), &box_dir, box_id).unwrap();

    assert_eq!(
        record.map(|record| record.runtime_root),
        Some(home.path().join("run/a3s-oci").join(box_id))
    );
}

#[test]
fn runtime_record_rejects_a_socket_outside_its_runtime_root() {
    let home = tempfile::tempdir().unwrap();
    let box_id = "recorded-sandbox-a3s-oci-wrong-socket";
    let box_dir = home.path().join("boxes").join(box_id);
    write_runtime_record(home.path(), &box_dir, box_id, |record| {
        record.runtime_socket = Some(home.path().join("run/a3s-oci/other/runtime.sock"));
    });

    let error = load_recorded_sandbox_runtime_identity(home.path(), &box_dir, box_id).unwrap_err();

    assert!(error.to_string().contains("path or identity validation"));
}

#[test]
fn runtime_record_rejects_invalid_generation_and_digest_identity() {
    let home = tempfile::tempdir().unwrap();
    let box_id = "recorded-sandbox-a3s-oci-invalid-identity";
    let box_dir = home.path().join("boxes").join(box_id);
    write_runtime_record(home.path(), &box_dir, box_id, |record| {
        record.generation = Some(0);
        record.agent_sha256 = Some("not-a-digest".to_string());
    });

    let error = load_recorded_sandbox_runtime_identity(home.path(), &box_dir, box_id).unwrap_err();

    assert!(error.to_string().contains("path or identity validation"));
}

#[test]
fn current_record_log_drain_check_is_read_only() {
    let home = tempfile::tempdir().unwrap();
    let box_id = "recorded-sandbox-log-drain";
    let box_dir = home.path().join("boxes").join(box_id);
    write_runtime_record(home.path(), &box_dir, box_id, |_| {});

    assert!(wait_for_recorded_sandbox_log_drain_in(
        home.path(),
        &box_dir,
        box_id,
        std::time::Duration::ZERO,
    )
    .unwrap());
}

#[test]
fn cleanup_reaps_a_terminal_recovered_log_worker() {
    let worker = std::process::Command::new("true").spawn().unwrap();
    let pid = worker.id();
    let start_time = crate::process::pid_start_time(pid).unwrap();
    drop(worker);
    let record = RecordedSandboxRuntime {
        runtime_path: Path::new("/bin/true").to_path_buf(),
        runtime_sha256: None,
        agent_path: None,
        agent_sha256: None,
        runtime_root: Path::new("/tmp/recovered-runtime").to_path_buf(),
        runtime_socket: None,
        bundle_dir: Path::new("/tmp/recovered-bundle").to_path_buf(),
        init_pid: 42,
        generation: None,
        owner_pid: None,
        owner_pid_start_time: None,
        log_worker_pid: Some(pid),
        log_worker_pid_start_time: Some(start_time),
    };

    drain_recorded_log_worker(&record, "terminal-recovered-worker");

    assert!(
        !crate::process::is_process_alive_with_identity(pid, Some(start_time)),
        "cleanup must not leave its completed child as a zombie"
    );
}
