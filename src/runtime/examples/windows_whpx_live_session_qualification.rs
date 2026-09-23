//! Destructive live-session qualification for Box over Windows WHPX MicroVM OCI.
//!
//! Exercises MicroVM via Box-owned packaged WHPX Host (endpoint unset / derived
//! from service root), keeps the Box manager across Host `taskkill /F /PID`,
//! attempts retained streaming process-handle continuity, mutating filesystem
//! continuity via public Box `filesystem` (keyed MakeDir / Move / Remove before
//! kill, ListDir after reattach) plus `transfer_file` (keyed upload before kill,
//! download after reattach on the same generation), and keyed captured exec /
//! state / inventory / stats / kill, without inventing an exit.
//!
//! Schema `a3s.box.windows-whpx-live-session.v1`.
//!
//! Honest scope (anti-overfit):
//! - Distinct from stopped-only `windows-whpx-oci-qualification`.
//! - Distinct from KVM Live (`linux-kvm-live-session-qualification`).
//! - `retained_stream_handle_proven` / `whpx_microvm_live_claimed` only when the
//!   same Box `start_process` handle continues after Host taskkill + reopen.
//! - `retained_filesystem_proven` only when keyed MakeDir + Move + Remove +
//!   keyed upload before kill are visible after reattach (ListDir + download
//!   of the moved tree) on the same Box generation (stable `mkdir_request_id` /
//!   `move_request_id` / `remove_request_id` / `file_upload_request_id`, same
//!   Unavailable retry policy as keyed exec).
//! - `fixture_stream_continuity_claimed` and `b2_process_session_recovery_closed`
//!   stay false in harness reports by design (individual reports never
//!   self-certify ROADMAP B2 close).
//! - `box_owned_ensure_proven` is true only when Host taskkill recovery went
//!   through Box identity-fenced ensure (not example-local Host respawn).
//! - Does **not** claim MicroVM cutover or Enterprise GA.
//! - If WHPX Live reattach is unavailable at the OCI layer, the harness fails
//!   closed with `live_path_unavailable=true` and an honest error message.

#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
fn main() {
    eprintln!("windows-whpx-live-session-qualification requires Windows x86_64");
    std::process::exit(2);
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[tokio::main]
async fn main() {
    qualification::main().await;
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod qualification {
    use std::collections::BTreeMap;
    use std::error::Error;
    use std::fs::OpenOptions;
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use a3s_box_core::{
        BoxConfig, CreateExecutionRequest, ExecEvent, ExecRequest, ExecutionBackend,
        ExecutionGeneration, ExecutionId, ExecutionIsolation, ExecutionManager,
        ExecutionManagerError, ExecutionProcessSignal, ExecutionProcessStream,
        ExecutionSessionManager, ExecutionState, FileOp, FileRequest, FileResponse,
        FilesystemEntryKind, FilesystemOp, FilesystemRequest, FilesystemResponse,
        IsolationClass as BoxIsolationClass, NetworkMode, OperationId, ReconcileOutcome,
        ResourceConfig, StreamType,
    };
    use a3s_box_runtime::{
        LocalExecutionManager, ManagedExecutionStore, ManagedRuntimeRoute, OciRuntimeBinding,
        WindowsWhpxOciMigrationConfig,
    };
    use a3s_oci_sdk::{DriverKind, IsolationClass as OciIsolationClass};
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use serde::Serialize;

    const ENABLE_ENV: &str = "A3S_BOX_WHPX_LIVE_SESSION_QUALIFICATION";
    const HOME_ENV: &str = "A3S_HOME";
    const RUNTIME_ROOT_ENV: &str = "A3S_BOX_OCI_HOST_ROOT";
    const IMAGE_ENV: &str = "A3S_BOX_WHPX_LIVE_SESSION_IMAGE";
    const REPORT_ENV: &str = "A3S_BOX_WHPX_LIVE_SESSION_REPORT";
    const BOX_SHA_ENV: &str = "A3S_BOX_WHPX_LIVE_SESSION_BOX_SHA";
    const OCI_SHA_ENV: &str = "A3S_BOX_WHPX_LIVE_SESSION_OCI_SHA";
    const BOX_OWNED_ENV: &str = "A3S_BOX_WHPX_OCI_BOX_OWNED";
    const SERVICE_ROOT_ENV: &str = "A3S_BOX_WHPX_OCI_SERVICE_ROOT";
    const SERVICE_BIN_ENV: &str = "A3S_BOX_WHPX_OCI_SERVICE_BIN";
    const SERVICE_SHIM_ENV: &str = "A3S_BOX_WHPX_OCI_SERVICE_SHIM";
    const SERVICE_VM_ROOTFS_ENV: &str = "A3S_BOX_WHPX_OCI_SERVICE_VM_ROOTFS";
    const SERVICE_MANIFEST_ENV: &str = "A3S_BOX_WHPX_OCI_SERVICE_MANIFEST";
    const ENDPOINT_ENV: &str = "A3S_BOX_OCI_WHPX_ENDPOINT";
    const SCHEMA_VERSION: &str = "a3s.box.windows-whpx-live-session.v1";
    const KEYED_EXEC_BEFORE: &str = "a3s.box.live-session.keyed-exec.before-owner-kill";
    const KEYED_EXEC_AFTER: &str = "a3s.box.live-session.keyed-exec.after-reopen";
    const KEYED_FILE_UPLOAD_BEFORE: &str = "a3s.box.live-session.keyed-file.before-owner-kill";
    const KEYED_MKDIR_BEFORE: &str = "a3s.box.live-session.keyed-mkdir.before-owner-kill";
    const KEYED_MOVE_BEFORE: &str = "a3s.box.live-session.keyed-move.before-owner-kill";
    const KEYED_REMOVE_BEFORE: &str = "a3s.box.live-session.keyed-remove.before-owner-kill";
    const KEYED_EXEC_MARKER: &[u8] = b"live-session-keyed-ok\n";
    const STREAM_MARKER: &[u8] = b"live-session-stream-ok\n";
    const STREAM_ECHO_BEFORE: &[u8] = b"before-owner-kill\n";
    const STREAM_ECHO_AFTER: &[u8] = b"after-owner-reopen\n";
    const FS_GUEST_DIR: &str = "/tmp/.a3s-box-whpx-live-fs.d";
    const FS_GUEST_PATH: &str = "/tmp/.a3s-box-whpx-live-fs.d/payload.bin";
    const FS_GUEST_DIR_KEPT: &str = "/tmp/.a3s-box-whpx-live-fs.kept.d";
    const FS_GUEST_PATH_KEPT: &str = "/tmp/.a3s-box-whpx-live-fs.kept.d/payload.bin";
    const FS_GUEST_EPHEMERAL: &str = "/tmp/.a3s-box-whpx-live-fs.ephemeral.d";
    const FS_PAYLOAD: &[u8] = b"a3s-box-whpx-live-fs\0binary\nv1\n";

    type AnyError = Box<dyn Error + Send + Sync>;

    #[derive(Debug, Clone, Serialize)]
    struct ProcessIdentityReport {
        pid: u32,
        start_time_ticks: u64,
    }

    #[derive(Debug, Clone)]
    struct Inputs {
        home_dir: PathBuf,
        state_path: PathBuf,
        runtime_root: PathBuf,
        endpoint: String,
        image: String,
        service_root: PathBuf,
        service_bin: PathBuf,
        service_shim: PathBuf,
        service_vm_rootfs: PathBuf,
        service_manifest: PathBuf,
    }

    #[derive(Debug, Serialize)]
    struct QualificationReport {
        schema_version: &'static str,
        status: &'static str,
        started_at_utc: String,
        completed_at_utc: String,
        error: Option<String>,
        cleanup_error: Option<String>,
        box_commit_sha: Option<String>,
        oci_runtime_commit_sha: Option<String>,
        home_dir: Option<PathBuf>,
        state_path: Option<PathBuf>,
        runtime_root: Option<PathBuf>,
        image: Option<String>,
        driver_target: &'static str,
        whpx_microvm_live_claimed: bool,
        /// Set only when the same streaming `start_process` handle continues
        /// after a real Host taskkill + Live reopen.
        retained_stream_handle_proven: bool,
        /// Upload before Host taskkill via public Box `transfer_file`.
        file_upload_before_kill: bool,
        /// Durable upload identity used before Host taskkill (harness-stable).
        file_upload_request_id: Option<String>,
        /// Keyed MakeDir before Host taskkill via public Box `filesystem`.
        mkdir_before_kill: bool,
        /// Durable MakeDir identity used before Host taskkill (harness-stable).
        mkdir_request_id: Option<String>,
        /// Keyed Move before Host taskkill via public Box `filesystem`.
        move_before_kill: bool,
        /// Durable Move identity used before Host taskkill (harness-stable).
        move_request_id: Option<String>,
        /// Keyed Remove before Host taskkill via public Box `filesystem`.
        remove_before_kill: bool,
        /// Durable Remove identity used before Host taskkill (harness-stable).
        remove_request_id: Option<String>,
        /// ListDir after Live reopen sees the pre-kill directory contents.
        list_dir_after_reattach: bool,
        /// Download after Live reopen matches the pre-kill upload payload.
        file_download_after_reattach: bool,
        /// Aggregate: keyed MakeDir, Move, Remove, and keyed upload before kill,
        /// plus ListDir and exact download match after reattach on the same
        /// Running generation (no invented stop).
        retained_filesystem_proven: bool,
        /// Always false: fixture `process_restart` continuity is not this gate.
        fixture_stream_continuity_claimed: bool,
        /// Always false: harness reports never self-certify ROADMAP B2 close.
        b2_process_session_recovery_closed: bool,
        /// True only when Host recovery used Box-owned ensure (not script respawn).
        box_owned_ensure_proven: bool,
        /// True when WHPX Live reattach is not yet available at OCI layer.
        live_path_unavailable: bool,
        operation_id: Option<String>,
        execution_id: Option<String>,
        box_generation: Option<ExecutionGeneration>,
        runtime_binding: Option<OciRuntimeBinding>,
        observed_running_before_owner_kill: bool,
        inventory_before_owner_kill_non_empty: bool,
        keyed_captured_exec_before_owner_kill: bool,
        owner_before_kill: Option<ProcessIdentityReport>,
        owner_killed: bool,
        owner_gone: bool,
        owner_rebound: bool,
        owner_after_reopen: Option<ProcessIdentityReport>,
        reconciled_ready_after_reopen: bool,
        observed_running_after_reopen: bool,
        inventory_after_reopen_non_empty: bool,
        exit_code_absent_after_reopen: bool,
        stats_after_reopen: bool,
        keyed_captured_exec_after_reopen: bool,
        live_kill_after_reopen: bool,
        removed: bool,
    }

    impl QualificationReport {
        fn new() -> Self {
            Self {
                schema_version: SCHEMA_VERSION,
                status: "failed",
                started_at_utc: chrono::Utc::now().to_rfc3339(),
                completed_at_utc: String::new(),
                error: None,
                cleanup_error: None,
                box_commit_sha: None,
                oci_runtime_commit_sha: None,
                home_dir: None,
                state_path: None,
                runtime_root: None,
                image: None,
                driver_target: "windows-whpx-dedicated-vm",
                whpx_microvm_live_claimed: false,
                retained_stream_handle_proven: false,
                file_upload_before_kill: false,
                file_upload_request_id: None,
                mkdir_before_kill: false,
                mkdir_request_id: None,
                move_before_kill: false,
                move_request_id: None,
                remove_before_kill: false,
                remove_request_id: None,
                list_dir_after_reattach: false,
                file_download_after_reattach: false,
                retained_filesystem_proven: false,
                fixture_stream_continuity_claimed: false,
                b2_process_session_recovery_closed: false,
                box_owned_ensure_proven: false,
                live_path_unavailable: false,
                operation_id: None,
                execution_id: None,
                box_generation: None,
                runtime_binding: None,
                observed_running_before_owner_kill: false,
                inventory_before_owner_kill_non_empty: false,
                keyed_captured_exec_before_owner_kill: false,
                owner_before_kill: None,
                owner_killed: false,
                owner_gone: false,
                owner_rebound: false,
                owner_after_reopen: None,
                reconciled_ready_after_reopen: false,
                observed_running_after_reopen: false,
                inventory_after_reopen_non_empty: false,
                exit_code_absent_after_reopen: false,
                stats_after_reopen: false,
                keyed_captured_exec_after_reopen: false,
                live_kill_after_reopen: false,
                removed: false,
            }
        }
    }

    pub(super) async fn main() {
        let report_path = match absolute_environment_path(REPORT_ENV) {
            Ok(path) => path,
            Err(error) => {
                eprintln!("WHPX live-session qualification cannot select its report: {error}");
                std::process::exit(2);
            }
        };
        let mut report = QualificationReport::new();

        let inputs = load_inputs(&mut report);
        let outcome = match inputs {
            Ok(inputs) => {
                let operation_id = OperationId::new(format!(
                    "windows-whpx-live-session-{}",
                    uuid::Uuid::new_v4()
                ));
                match operation_id {
                    Ok(operation_id) => {
                        report.operation_id = Some(operation_id.to_string());
                        let outcome = exercise(&inputs, &operation_id, &mut report).await;
                        if outcome.is_err() {
                            let mut cleanup_error = None;
                            if let Err(error) = cleanup(&inputs, &operation_id).await {
                                cleanup_error = Some(error.to_string());
                            }
                            report.cleanup_error = cleanup_error;
                        }
                        outcome
                    }
                    Err(error) => Err(error.into()),
                }
            }
            Err(error) => Err(error),
        };

        match outcome {
            Ok(()) => report.status = "passed",
            Err(error) => report.error = Some(error.to_string()),
        }
        report.completed_at_utc = chrono::Utc::now().to_rfc3339();

        if let Err(error) = write_report(&report_path, &report) {
            eprintln!(
                "WHPX live-session qualification could not write {}: {error}",
                report_path.display()
            );
            std::process::exit(1);
        }
        if report.status != "passed" {
            eprintln!(
                "WHPX live-session qualification failed: {}",
                report.error.as_deref().unwrap_or("unknown failure")
            );
            std::process::exit(1);
        }
        println!(
            "WHPX live-session qualification passed: {}",
            report_path.display()
        );
    }

    fn load_inputs(report: &mut QualificationReport) -> Result<Inputs, AnyError> {
        require(
            std::env::var(ENABLE_ENV).as_deref() == Ok("1"),
            format!("set {ENABLE_ENV}=1 to acknowledge the destructive qualification"),
        )?;
        require(
            matches!(
                std::env::var(BOX_OWNED_ENV).ok().as_deref().map(str::trim),
                Some("1" | "true" | "on" | "yes" | "box-owned")
            ),
            format!(
                "set {BOX_OWNED_ENV}=1; WHPX live-session requires Box-owned Host ensure \
                 (fail-closed without it)"
            ),
        )?;

        let home_dir = absolute_environment_path(HOME_ENV)?;
        require(
            home_dir
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("whpx-live-session")),
            "A3S_HOME must name a dedicated whpx-live-session directory",
        )?;
        let runtime_root = absolute_environment_path(RUNTIME_ROOT_ENV)?;
        let image = required_environment_string(IMAGE_ENV)?;
        let box_sha = required_environment_string(BOX_SHA_ENV)?;
        let oci_sha = required_environment_string(OCI_SHA_ENV)?;
        require(
            looks_like_git_sha(&box_sha),
            format!("{BOX_SHA_ENV} must be a 40-character lowercase hex SHA"),
        )?;
        require(
            looks_like_git_sha(&oci_sha),
            format!("{OCI_SHA_ENV} must be a 40-character lowercase hex SHA"),
        )?;

        let service_root = absolute_environment_path(SERVICE_ROOT_ENV)?;
        let service_bin = absolute_environment_path(SERVICE_BIN_ENV)?;
        let service_shim = absolute_environment_path(SERVICE_SHIM_ENV)?;
        let service_vm_rootfs = absolute_environment_path(SERVICE_VM_ROOTFS_ENV)?;
        let service_manifest = absolute_environment_path(SERVICE_MANIFEST_ENV)?;
        let endpoint = required_environment_string(ENDPOINT_ENV)?;

        let state_path = home_dir.join("managed-executions.json");

        report.home_dir = Some(home_dir.clone());
        report.state_path = Some(state_path.clone());
        report.runtime_root = Some(runtime_root.clone());
        report.image = Some(image.clone());
        report.box_commit_sha = Some(box_sha);
        report.oci_runtime_commit_sha = Some(oci_sha);

        Ok(Inputs {
            home_dir,
            state_path,
            runtime_root,
            endpoint,
            image,
            service_root,
            service_bin,
            service_shim,
            service_vm_rootfs,
            service_manifest,
        })
    }

    fn owner_pid_from_box_record(service_root: &Path) -> Result<u32, AnyError> {
        let path = service_root.join("box-owner.json");
        let bytes = std::fs::read(&path).map_err(|error| {
            failure(format!(
                "failed to read Box-owned WHPX owner record {}: {error}",
                path.display()
            ))
        })?;
        let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
            failure(format!(
                "Box-owned WHPX owner record {} is invalid JSON: {error}",
                path.display()
            ))
        })?;
        let pid = value
            .get("pid")
            .and_then(|value| value.as_u64())
            .ok_or_else(|| {
                failure(format!(
                    "Box-owned WHPX owner record {} is missing pid",
                    path.display()
                ))
            })?;
        let pid = u32::try_from(pid).map_err(|_| {
            failure(format!(
                "Box-owned WHPX owner record {} has an out-of-range pid",
                path.display()
            ))
        })?;
        require(pid > 0, "Box-owned WHPX owner pid must be non-zero")?;
        Ok(pid)
    }

    fn host_service_identity(pid: u32) -> Result<ProcessIdentityReport, AnyError> {
        let start_time_ticks = a3s_box_runtime::pid_start_time(pid)
            .ok_or_else(|| failure("Host Service PID was not alive before taskkill"))?;
        Ok(ProcessIdentityReport {
            pid,
            start_time_ticks,
        })
    }

    fn taskkill_host_service(pid: u32) -> Result<(), AnyError> {
        let status = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .status()
            .map_err(|error| {
                failure(format!("failed to launch taskkill for pid {pid}: {error}"))
            })?;
        if !status.success() {
            let still_running = a3s_box_runtime::is_process_running_with_identity(pid, None);
            require(
                !still_running,
                format!("taskkill failed and Host pid {pid} is still running"),
            )?;
        }
        Ok(())
    }

    fn wait_host_gone(pid: u32, timeout: Duration) -> Result<(), AnyError> {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if !a3s_box_runtime::is_process_running_with_identity(pid, None) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(failure(format!(
            "Host Service pid {pid} remained alive after taskkill"
        )))
    }

    async fn exercise(
        inputs: &Inputs,
        operation_id: &OperationId,
        report: &mut QualificationReport,
    ) -> Result<(), AnyError> {
        let request = qualification_request(&inputs.image);
        let manager = connect(inputs).await?;
        let reservation = manager.create(request, operation_id).await?;
        report.execution_id = Some(reservation.execution_id.to_string());
        report.box_generation = Some(reservation.generation);
        require(
            reservation.plan.backend == ExecutionBackend::Krun
                && reservation.plan.isolation_class == BoxIsolationClass::HardwareVm,
            "the Box request did not retain its dedicated MicroVM product plan",
        )?;

        let lease = tokio::time::timeout(
            Duration::from_secs(30 * 60),
            manager.start(&reservation.execution_id, reservation.generation),
        )
        .await
        .map_err(|_| failure("timed out preparing or starting the WHPX MicroVM execution"))??;
        require(
            lease.execution_id == reservation.execution_id
                && lease.generation == reservation.generation,
            "started Box lease differs from its durable reservation",
        )?;

        let store = ManagedExecutionStore::new(&inputs.state_path);
        let running = store
            .get(&reservation.execution_id)?
            .ok_or_else(|| failure("started Box record is missing"))?;
        let metadata = running
            .managed_execution
            .as_ref()
            .ok_or_else(|| failure("started Box record has no managed metadata"))?;
        require(
            metadata.runtime_route == ManagedRuntimeRoute::OciSdk,
            "created Box record is not durably routed through the OCI SDK",
        )?;
        let binding = metadata
            .oci_runtime
            .clone()
            .ok_or_else(|| failure("started Box record has no exact OCI runtime binding"))?;
        require(
            binding.driver == DriverKind::LibkrunWhpx
                && binding.isolation == OciIsolationClass::DedicatedVm,
            "runtime did not return the dedicated-VM libkrun/WHPX binding",
        )?;
        report.runtime_binding = Some(binding);

        wait_until_running(&manager, &reservation.execution_id, report, true).await?;

        let inventory = manager
            .list_processes(&reservation.execution_id, reservation.generation)
            .await?;
        require(
            !inventory.processes.is_empty(),
            "process inventory was empty before Host taskkill",
        )?;
        report.inventory_before_owner_kill_non_empty = true;

        prove_keyed_captured_exec(
            &manager,
            &reservation.execution_id,
            reservation.generation,
            KEYED_EXEC_BEFORE,
        )
        .await?;
        report.keyed_captured_exec_before_owner_kill = true;

        prove_mkdir_before_kill(
            &manager,
            &reservation.execution_id,
            reservation.generation,
            KEYED_MKDIR_BEFORE,
        )
        .await?;
        report.mkdir_before_kill = true;
        report.mkdir_request_id = Some(KEYED_MKDIR_BEFORE.to_string());

        prove_file_upload_before_kill(
            &manager,
            &reservation.execution_id,
            reservation.generation,
            KEYED_FILE_UPLOAD_BEFORE,
        )
        .await?;
        report.file_upload_before_kill = true;
        report.file_upload_request_id = Some(KEYED_FILE_UPLOAD_BEFORE.to_string());

        prove_move_before_kill(
            &manager,
            &reservation.execution_id,
            reservation.generation,
            KEYED_MOVE_BEFORE,
        )
        .await?;
        report.move_before_kill = true;
        report.move_request_id = Some(KEYED_MOVE_BEFORE.to_string());

        prove_remove_before_kill(
            &manager,
            &reservation.execution_id,
            reservation.generation,
            KEYED_REMOVE_BEFORE,
        )
        .await?;
        report.remove_before_kill = true;
        report.remove_request_id = Some(KEYED_REMOVE_BEFORE.to_string());

        let host_pid = owner_pid_from_box_record(&inputs.service_root)?;
        let host_service = host_service_identity(host_pid)?;
        report.owner_before_kill = Some(host_service.clone());

        let mut process = manager
            .start_process(
                &reservation.execution_id,
                reservation.generation,
                ExecRequest {
                    request_id: None,
                    cmd: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "printf 'live-session-stream-ok\\n'; while true; do if IFS= read -r line; then printf 'echo:%s\\n' \"$line\"; else while true; do sleep 3600; done; fi; done".into(),
                    ],
                    timeout_ns: 600_000_000_000,
                    env: Vec::new(),
                    working_dir: Some("/".into()),
                    rootfs: None,
                    stdin: None,
                    stdin_streaming: true,
                    user: None,
                    streaming: true,
                },
            )
            .await
            .map_err(|error| {
                failure(format!(
                    "streaming start_process before Host taskkill failed: {error}"
                ))
            })?;
        let input = process.input();
        let first_event = process
            .next_event()
            .await
            .map_err(|error| {
                failure(format!(
                    "streaming process failed before first chunk: {error}"
                ))
            })?
            .ok_or_else(|| failure("streaming process ended before the marker chunk"))?;
        require(
            matches!(
                &first_event,
                ExecEvent::Chunk(chunk)
                    if chunk.stream == StreamType::Stdout && chunk.data == STREAM_MARKER
            ),
            format!("streaming process first chunk drifted: {first_event:?}"),
        )?;
        input
            .write_stdin(STREAM_ECHO_BEFORE)
            .await
            .map_err(|error| {
                failure(format!(
                    "streaming stdin write before Host taskkill failed: {error}"
                ))
            })?;
        expect_stream_echo(process.as_mut(), STREAM_ECHO_BEFORE, "before Host taskkill").await?;

        // Keep the Box manager: retained-handle continuity is the v1 gate.
        taskkill_host_service(host_pid)?;
        report.owner_killed = true;
        wait_host_gone(host_pid, Duration::from_secs(30))?;
        report.owner_gone = true;

        let disconnect = process.next_event().await;
        let live_available = !matches!(disconnect, Err(ExecutionManagerError::Unavailable(_)));

        if !live_available && matches!(disconnect, Err(ExecutionManagerError::Unavailable(_))) {
            // WHPX Live reattach is not yet wired at the OCI layer — fail
            // closed honestly instead of inventing success.
            report.live_path_unavailable = true;
        }

        require(
            matches!(disconnect, Err(ExecutionManagerError::Unavailable(_))),
            format!("retained stream must surface Unavailable on Host death, got {disconnect:?}"),
        )?;

        // Retained-manager reconcile/inspect must respawn via ensure.
        let mut outcome = None;
        let reconcile_deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        while outcome.is_none() {
            match manager.reconcile(operation_id).await {
                Ok(value) => outcome = Some(value),
                Err(ExecutionManagerError::Unavailable(_)) => {
                    if tokio::time::Instant::now() >= reconcile_deadline {
                        return Err(failure(
                            "Box remained Unavailable for 120s after Host taskkill",
                        ));
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(error) => {
                    return Err(failure(format!(
                        "Box reconcile failed after Host taskkill: {error}"
                    )));
                }
            }
        }
        match outcome.expect("reconcile outcome assigned before loop exit") {
            ReconcileOutcome::Ready(recovered) => {
                require(
                    recovered.execution_id == reservation.execution_id
                        && recovered.generation == reservation.generation,
                    "Host reopen recovered a different Ready Box generation",
                )?;
                report.reconciled_ready_after_reopen = true;
            }
            ReconcileOutcome::Failed => {
                report.live_path_unavailable = true;
                return Err(failure(
                    "Host reopen reconciled Failed/stopped instead of Live Ready \
                     (WHPX Live path unavailable — gate 9 stays open)",
                ));
            }
            ReconcileOutcome::Created(_) => {
                return Err(failure(
                    "Host reopen left the generation Created instead of Live Ready",
                ));
            }
            ReconcileOutcome::Creating => {
                return Err(failure(
                    "Host reopen left the Box generation stuck creating",
                ));
            }
            ReconcileOutcome::Absent => {
                return Err(failure(
                    "Host reopen lost the Box operation before Live reconciliation",
                ));
            }
        }

        let replacement_host =
            host_service_identity(owner_pid_from_box_record(&inputs.service_root)?)?;
        report.owner_rebound = replacement_host.pid != host_service.pid
            || replacement_host.start_time_ticks != host_service.start_time_ticks;
        report.owner_after_reopen = Some(replacement_host);
        require(
            report.owner_rebound,
            "Box-owned ensure did not publish a distinct Host identity after taskkill",
        )?;
        report.box_owned_ensure_proven = true;

        input
            .write_stdin(STREAM_ECHO_AFTER)
            .await
            .map_err(|error| {
                failure(format!(
                    "retained streaming stdin after Host reopen failed: {error}"
                ))
            })?;
        expect_stream_echo(process.as_mut(), STREAM_ECHO_AFTER, "after Host reopen").await?;
        input.close_stdin().await.map_err(|error| {
            failure(format!(
                "retained streaming close_stdin after Host reopen failed: {error}"
            ))
        })?;
        input
            .send_signal(ExecutionProcessSignal::Kill)
            .await
            .map_err(|error| {
                failure(format!(
                    "retained streaming signal after Host reopen failed: {error}"
                ))
            })?;
        let mut saw_exit = false;
        let drain_deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            match process.next_event().await {
                Ok(Some(ExecEvent::Exit(_))) => {
                    saw_exit = true;
                    break;
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(error) => {
                    return Err(failure(format!(
                        "retained streaming drain after Host reopen failed: {error}"
                    )));
                }
            }
            if tokio::time::Instant::now() >= drain_deadline {
                return Err(failure(
                    "timed out draining retained streaming process after Host reopen",
                ));
            }
        }
        require(
            saw_exit,
            "retained streaming process did not publish Exit after reopen signal",
        )?;
        report.retained_stream_handle_proven = true;
        report.whpx_microvm_live_claimed = report.retained_stream_handle_proven;

        wait_until_running(&manager, &reservation.execution_id, report, false).await?;
        let after = store
            .get(&reservation.execution_id)?
            .ok_or_else(|| failure("Box record missing after Live reopen"))?;
        require(
            after.exit_code.is_none(),
            format!(
                "Live reopen invented exit status {:?} (must remain absent while Running)",
                after.exit_code
            ),
        )?;
        report.exit_code_absent_after_reopen = true;

        prove_list_dir_after_reattach(&manager, &reservation.execution_id, reservation.generation)
            .await?;
        report.list_dir_after_reattach = true;

        prove_file_download_after_reattach(
            &manager,
            &reservation.execution_id,
            reservation.generation,
        )
        .await?;
        report.file_download_after_reattach = true;
        report.retained_filesystem_proven = report.mkdir_before_kill
            && report.move_before_kill
            && report.remove_before_kill
            && report.list_dir_after_reattach
            && report.file_upload_before_kill
            && report.file_download_after_reattach
            && report.mkdir_request_id.as_deref() == Some(KEYED_MKDIR_BEFORE)
            && report.move_request_id.as_deref() == Some(KEYED_MOVE_BEFORE)
            && report.remove_request_id.as_deref() == Some(KEYED_REMOVE_BEFORE)
            && report.file_upload_request_id.as_deref() == Some(KEYED_FILE_UPLOAD_BEFORE)
            && report.reconciled_ready_after_reopen
            && report.observed_running_after_reopen
            && report.exit_code_absent_after_reopen;
        require(
            report.retained_filesystem_proven,
            "Live retained filesystem evidence failed its completeness audit",
        )?;

        let inventory_after = manager
            .list_processes(&reservation.execution_id, reservation.generation)
            .await?;
        require(
            !inventory_after.processes.is_empty(),
            "process inventory was empty after Live reopen",
        )?;
        report.inventory_after_reopen_non_empty = true;

        let stats = manager
            .stats(&reservation.execution_id, reservation.generation)
            .await?;
        require(
            stats.execution_id == reservation.execution_id
                && stats.generation == reservation.generation
                && stats.process_count > 0,
            "Live reopen stats were not authentic for the running generation",
        )?;
        report.stats_after_reopen = true;

        prove_keyed_captured_exec(
            &manager,
            &reservation.execution_id,
            reservation.generation,
            KEYED_EXEC_AFTER,
        )
        .await?;
        report.keyed_captured_exec_after_reopen = true;

        manager
            .kill(&reservation.execution_id, reservation.generation)
            .await?;
        report.live_kill_after_reopen = true;

        let stop_deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            let status = manager.inspect(&reservation.execution_id).await?;
            if matches!(
                status.state,
                ExecutionState::Stopped | ExecutionState::Failed
            ) {
                break;
            }
            if tokio::time::Instant::now() >= stop_deadline {
                return Err(failure(
                    "timed out waiting for Live kill to reach a terminal Box state",
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        report.removed = manager
            .remove(&reservation.execution_id, reservation.generation)
            .await?;
        require(report.removed, "terminal Box generation was not removed")?;
        require(
            matches!(
                manager.reconcile(operation_id).await?,
                ReconcileOutcome::Absent
            ),
            "removed Box operation remained reconcilable",
        )?;
        require(
            !inputs
                .home_dir
                .join("boxes")
                .join(reservation.execution_id.as_str())
                .exists(),
            "Box directory remained after deletion",
        )?;
        Ok(())
    }

    async fn prove_keyed_captured_exec(
        manager: &LocalExecutionManager,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        request_id: &str,
    ) -> Result<(), AnyError> {
        let request = ExecRequest {
            request_id: Some(request_id.to_string()),
            cmd: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf 'live-session-keyed-ok\\n'".into(),
            ],
            timeout_ns: 30_000_000_000,
            env: Vec::new(),
            working_dir: Some("/".into()),
            rootfs: None,
            stdin: None,
            stdin_streaming: false,
            user: None,
            streaming: false,
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let output = loop {
            match manager
                .execute(execution_id, generation, request.clone())
                .await
            {
                Ok(output) => break output,
                Err(ExecutionManagerError::Unavailable(message)) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(failure(format!(
                            "keyed captured exec `{request_id}` failed on Live generation: \
                             execution backend unavailable: {message}"
                        )));
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => {
                    return Err(failure(format!(
                        "keyed captured exec `{request_id}` failed on Live generation: {error}"
                    )));
                }
            }
        };
        require(
            output.exit_code == 0,
            format!(
                "keyed captured exec `{request_id}` exited {} instead of 0",
                output.exit_code
            ),
        )?;
        require(
            output.stdout == KEYED_EXEC_MARKER,
            format!(
                "keyed captured exec `{request_id}` stdout drifted: {:?}",
                String::from_utf8_lossy(&output.stdout)
            ),
        )?;
        Ok(())
    }

    async fn transfer_file_until_ready(
        manager: &LocalExecutionManager,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        request: FileRequest,
        phase: &str,
    ) -> Result<FileResponse, AnyError> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            match manager
                .transfer_file(execution_id, generation, request.clone())
                .await
            {
                Ok(response) => return Ok(response),
                Err(ExecutionManagerError::Unavailable(message)) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(failure(format!(
                            "{phase}: execution backend unavailable: {message}"
                        )));
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => {
                    return Err(failure(format!("{phase}: {error}")));
                }
            }
        }
    }

    async fn filesystem_until_ready(
        manager: &LocalExecutionManager,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        request: FilesystemRequest,
        phase: &str,
    ) -> Result<FilesystemResponse, AnyError> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            match manager
                .filesystem(execution_id, generation, request.clone())
                .await
            {
                Ok(response) => return Ok(response),
                Err(ExecutionManagerError::Unavailable(message)) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(failure(format!(
                            "{phase}: execution backend unavailable: {message}"
                        )));
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => {
                    return Err(failure(format!("{phase}: {error}")));
                }
            }
        }
    }

    async fn prove_mkdir_before_kill(
        manager: &LocalExecutionManager,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        request_id: &str,
    ) -> Result<(), AnyError> {
        let response = filesystem_until_ready(
            manager,
            execution_id,
            generation,
            FilesystemRequest {
                op: FilesystemOp::MakeDir,
                path: FS_GUEST_DIR.to_string(),
                destination: None,
                depth: 0,
                user: None,
                request_id: Some(request_id.to_string()),
            },
            &format!("Live retained keyed MakeDir `{request_id}` before Host taskkill"),
        )
        .await?;
        require(
            response.success && response.error.is_none(),
            format!(
                "Live retained keyed MakeDir `{request_id}` before Host taskkill reported \
                 failure: {:?}",
                response.error
            ),
        )?;
        let entry = response.entry.as_ref().ok_or_else(|| {
            failure(format!(
                "Live retained keyed MakeDir `{request_id}` omitted directory entry"
            ))
        })?;
        require(
            entry.kind == FilesystemEntryKind::Directory && entry.path == FS_GUEST_DIR,
            format!(
                "Live retained keyed MakeDir `{request_id}` returned unexpected entry kind/path"
            ),
        )?;
        Ok(())
    }

    async fn prove_file_upload_before_kill(
        manager: &LocalExecutionManager,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        request_id: &str,
    ) -> Result<(), AnyError> {
        let response = transfer_file_until_ready(
            manager,
            execution_id,
            generation,
            FileRequest {
                op: FileOp::Upload,
                guest_path: FS_GUEST_PATH.to_string(),
                data: Some(STANDARD.encode(FS_PAYLOAD)),
                user: None,
                max_bytes: None,
                request_id: Some(request_id.to_string()),
            },
            &format!("Live retained keyed file upload `{request_id}` before Host taskkill"),
        )
        .await?;
        require(
            response.success && response.error.is_none(),
            format!(
                "Live retained keyed file upload `{request_id}` before Host taskkill reported \
                 failure: {:?}",
                response.error
            ),
        )?;
        require(
            response.size == FS_PAYLOAD.len() as u64,
            format!(
                "Live retained keyed file upload `{request_id}` size mismatch: got {} expected {}",
                response.size,
                FS_PAYLOAD.len()
            ),
        )?;
        Ok(())
    }

    async fn prove_move_before_kill(
        manager: &LocalExecutionManager,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        request_id: &str,
    ) -> Result<(), AnyError> {
        let response = filesystem_until_ready(
            manager,
            execution_id,
            generation,
            FilesystemRequest {
                op: FilesystemOp::Move,
                path: FS_GUEST_DIR.to_string(),
                destination: Some(FS_GUEST_DIR_KEPT.to_string()),
                depth: 0,
                user: None,
                request_id: Some(request_id.to_string()),
            },
            &format!("Live retained keyed Move `{request_id}` before Host taskkill"),
        )
        .await?;
        require(
            response.success && response.error.is_none(),
            format!(
                "Live retained keyed Move `{request_id}` before Host taskkill reported \
                 failure: {:?}",
                response.error
            ),
        )?;
        Ok(())
    }

    async fn prove_remove_before_kill(
        manager: &LocalExecutionManager,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        request_id: &str,
    ) -> Result<(), AnyError> {
        filesystem_until_ready(
            manager,
            execution_id,
            generation,
            FilesystemRequest {
                op: FilesystemOp::MakeDir,
                path: FS_GUEST_EPHEMERAL.to_string(),
                destination: None,
                depth: 0,
                user: None,
                request_id: Some(format!("{request_id}.mkdir-ephemeral")),
            },
            &format!("Live retained ephemeral MakeDir before Remove `{request_id}`"),
        )
        .await?;
        let response = filesystem_until_ready(
            manager,
            execution_id,
            generation,
            FilesystemRequest {
                op: FilesystemOp::Remove,
                path: FS_GUEST_EPHEMERAL.to_string(),
                destination: None,
                depth: 0,
                user: None,
                request_id: Some(request_id.to_string()),
            },
            &format!("Live retained keyed Remove `{request_id}` before Host taskkill"),
        )
        .await?;
        require(
            response.success && response.error.is_none(),
            format!(
                "Live retained keyed Remove `{request_id}` before Host taskkill reported \
                 failure: {:?}",
                response.error
            ),
        )?;
        Ok(())
    }

    async fn prove_list_dir_after_reattach(
        manager: &LocalExecutionManager,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> Result<(), AnyError> {
        let response = filesystem_until_ready(
            manager,
            execution_id,
            generation,
            FilesystemRequest {
                op: FilesystemOp::ListDir,
                path: FS_GUEST_DIR_KEPT.to_string(),
                destination: None,
                depth: 0,
                user: None,
                request_id: None,
            },
            "Live retained ListDir after reopen",
        )
        .await?;
        require(
            response.success && response.error.is_none(),
            format!(
                "Live retained ListDir after reopen reported failure: {:?}",
                response.error
            ),
        )?;
        let saw_payload = response.entries.iter().any(|entry| {
            entry.path == FS_GUEST_PATH_KEPT && entry.kind == FilesystemEntryKind::File
        });
        require(
            saw_payload,
            format!(
                "Live retained ListDir after reopen missing uploaded payload at \
                 {FS_GUEST_PATH_KEPT}; entries={:?}",
                response
                    .entries
                    .iter()
                    .map(|entry| (&entry.path, entry.kind))
                    .collect::<Vec<_>>()
            ),
        )?;
        Ok(())
    }

    async fn prove_file_download_after_reattach(
        manager: &LocalExecutionManager,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> Result<(), AnyError> {
        let response = transfer_file_until_ready(
            manager,
            execution_id,
            generation,
            FileRequest {
                op: FileOp::Download,
                guest_path: FS_GUEST_PATH_KEPT.to_string(),
                data: None,
                user: None,
                max_bytes: Some(FS_PAYLOAD.len() as u64),
                request_id: None,
            },
            "Live retained file download after reopen",
        )
        .await?;
        require(
            response.success && response.error.is_none(),
            format!(
                "Live retained file download after reopen reported failure: {:?}",
                response.error
            ),
        )?;
        let decoded = response
            .data
            .as_deref()
            .map(|value| STANDARD.decode(value))
            .transpose()
            .map_err(|error| {
                failure(format!(
                    "Live retained file download was not base64: {error}"
                ))
            })?
            .ok_or_else(|| {
                failure("Live retained file download omitted payload data".to_string())
            })?;
        require(
            decoded == FS_PAYLOAD,
            "Live retained file download did not match the pre-taskkill upload payload",
        )?;
        require(
            response.size == FS_PAYLOAD.len() as u64,
            format!(
                "Live retained file download size mismatch: got {} expected {}",
                response.size,
                FS_PAYLOAD.len()
            ),
        )?;
        Ok(())
    }

    async fn expect_stream_echo(
        process: &mut dyn ExecutionProcessStream,
        line: &[u8],
        phase: &str,
    ) -> Result<(), AnyError> {
        let payload = line.strip_suffix(b"\n").unwrap_or(line);
        let expected = format!("echo:{}\n", String::from_utf8_lossy(payload));
        let expected_bytes = expected.as_bytes();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), process.next_event())
                .await
                .map_err(|_| failure(format!("timed out waiting for stream echo {phase}")))?
                .map_err(|error| {
                    failure(format!(
                        "streaming process failed while waiting for echo {phase}: {error}"
                    ))
                })?
                .ok_or_else(|| failure(format!("streaming process ended before echo {phase}")))?;
            match event {
                ExecEvent::Chunk(chunk)
                    if chunk.stream == StreamType::Stdout && chunk.data == expected_bytes =>
                {
                    return Ok(());
                }
                ExecEvent::Chunk(chunk) if chunk.stream == StreamType::Stdout => {
                    return Err(failure(format!(
                        "streaming stdout drifted {phase}: {:?}",
                        String::from_utf8_lossy(&chunk.data)
                    )));
                }
                ExecEvent::FlushAck => {}
                ExecEvent::Exit(exit) => {
                    return Err(failure(format!(
                        "streaming process exited before echo {phase}: {exit:?}"
                    )));
                }
                ExecEvent::Chunk(_) => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(failure(format!(
                    "deadline exceeded waiting for stream echo {phase}"
                )));
            }
        }
    }

    async fn wait_until_running(
        manager: &LocalExecutionManager,
        execution_id: &ExecutionId,
        report: &mut QualificationReport,
        before_kill: bool,
    ) -> Result<(), AnyError> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            let status = manager.inspect(execution_id).await?;
            match status.state {
                ExecutionState::Running => {
                    if before_kill {
                        report.observed_running_before_owner_kill = true;
                    } else {
                        report.observed_running_after_reopen = true;
                    }
                    return Ok(());
                }
                ExecutionState::Failed | ExecutionState::Stopped => {
                    return Err(failure(format!(
                        "expected running WHPX MicroVM generation, found {:?}",
                        status.state
                    )));
                }
                ExecutionState::Created | ExecutionState::Creating | ExecutionState::Paused => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(failure(
                    "timed out waiting for the WHPX MicroVM execution to run",
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn connect(inputs: &Inputs) -> Result<LocalExecutionManager, AnyError> {
        let config = WindowsWhpxOciMigrationConfig::new(
            inputs.runtime_root.clone(),
            inputs.endpoint.clone(),
        )?
        .with_box_owned_owner(
            inputs.service_root.clone(),
            inputs.service_bin.clone(),
            inputs.service_shim.clone(),
            inputs.service_vm_rootfs.clone(),
            inputs.service_manifest.clone(),
        )?;
        Ok(LocalExecutionManager::with_windows_whpx_oci_qualification(
            &inputs.state_path,
            &inputs.home_dir,
            config,
        )
        .await?)
    }

    fn qualification_request(image: &str) -> CreateExecutionRequest {
        CreateExecutionRequest {
            external_sandbox_id: "windows-whpx-live-session".to_string(),
            config: BoxConfig {
                isolation: ExecutionIsolation::Microvm,
                image: image.to_string(),
                resources: ResourceConfig {
                    vcpus: 1,
                    memory_mb: 512,
                    ..Default::default()
                },
                cmd: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "printf 'a3s-box-whpx-live-session\\n'; while :; do sleep 60; done".to_string(),
                ],
                network: NetworkMode::None,
                persistent: false,
                ..Default::default()
            },
            labels: BTreeMap::from([(
                "purpose".to_string(),
                "windows-whpx-live-session".to_string(),
            )]),
            policy: Default::default(),
            rootfs_snapshot_id: None,
        }
    }

    async fn cleanup(inputs: &Inputs, operation_id: &OperationId) -> Result<(), AnyError> {
        if !inputs.state_path.exists() {
            return Ok(());
        }
        let store = ManagedExecutionStore::new(&inputs.state_path);
        let Some(record) = store.get_by_operation_id(operation_id)? else {
            return Ok(());
        };
        let generation = record
            .managed_execution
            .as_ref()
            .ok_or_else(|| failure("qualification cleanup lost managed metadata"))?
            .generation;
        let execution_id = ExecutionId::new(record.id)?;
        let manager = connect(inputs).await?;
        let state = manager.inspect(&execution_id).await?.state;
        if matches!(state, ExecutionState::Running | ExecutionState::Paused) {
            manager.kill(&execution_id, generation).await?;
        }
        manager.remove(&execution_id, generation).await?;
        Ok(())
    }

    fn absolute_environment_path(name: &str) -> Result<PathBuf, AnyError> {
        let path = std::env::var_os(name)
            .map(PathBuf::from)
            .ok_or_else(|| failure(format!("{name} must be set")))?;
        require(path.is_absolute(), format!("{name} must be absolute"))?;
        require(
            !path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::CurDir | std::path::Component::ParentDir
                )
            }),
            format!("{name} must be normalized"),
        )?;
        Ok(path)
    }

    fn required_environment_string(name: &str) -> Result<String, AnyError> {
        let value = std::env::var(name).map_err(|_| failure(format!("{name} must be set")))?;
        require(
            !value.trim().is_empty(),
            format!("{name} must not be empty"),
        )?;
        Ok(value)
    }

    fn looks_like_git_sha(value: &str) -> bool {
        value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
    }

    fn write_report(path: &Path, report: &QualificationReport) -> io::Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("report path has no parent"))?;
        std::fs::create_dir_all(parent)?;
        if path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("refusing to overwrite {}", path.display()),
            ));
        }
        let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
        let contents = serde_json::to_vec_pretty(report).map_err(io::Error::other)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&contents)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(temporary, path)
    }

    fn require(condition: bool, message: impl Into<String>) -> Result<(), AnyError> {
        if condition {
            Ok(())
        } else {
            Err(failure(message))
        }
    }

    fn failure(message: impl Into<String>) -> AnyError {
        Box::new(io::Error::other(message.into()))
    }
}
