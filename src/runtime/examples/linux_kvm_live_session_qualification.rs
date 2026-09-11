//! Destructive live-session qualification for Box over Linux KVM MicroVM OCI.
//!
//! Exercises MicroVM via `box-kvm-qualification-service` with
//! `A3S_OCI_KVM_SESSION_OWNER=1`, keeps the Box manager across Host Service
//! SIGKILL, proves retained streaming process-handle continuity, filesystem
//! continuity via public `transfer_file` (upload before kill, download after
//! reattach on the same generation), plus keyed captured exec / state /
//! inventory / stats / kill, without inventing an exit.
//!
//! Schema `a3s.box.linux-kvm-live-session.v2`.
//!
//! Honest scope (anti-overfit):
//! - Distinct from stopped-only `linux-kvm-oci-qualification`.
//! - Distinct from OCI smoke `retained_exec_io_proven` / filesystem smoke.
//! - `retained_stream_handle_proven` / `kvm_microvm_live_claimed` only when the
//!   same Box `start_process` handle continues after Host Service reopen.
//! - `retained_filesystem_proven` only when upload-before-kill bytes match
//!   download-after-reattach on the same Box generation.
//! - `fixture_stream_continuity_claimed` and `b2_process_session_recovery_closed`
//!   stay false in harness reports by design (individual reports never
//!   self-certify ROADMAP B2 close).

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
fn main() {
    eprintln!("linux-kvm-live-session-qualification requires Linux x86_64 or aarch64");
    std::process::exit(2);
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[tokio::main]
async fn main() {
    qualification::main().await;
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod qualification {
    use std::collections::BTreeMap;
    use std::error::Error;
    use std::fs::OpenOptions;
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    use a3s_box_core::{
        BoxConfig, CreateExecutionRequest, ExecEvent, ExecRequest, ExecutionBackend,
        ExecutionGeneration, ExecutionId, ExecutionIsolation, ExecutionManager,
        ExecutionManagerError, ExecutionProcessSignal, ExecutionProcessStream,
        ExecutionSessionManager, ExecutionState, FileOp, FileRequest,
        IsolationClass as BoxIsolationClass, NetworkMode, OperationId, ReconcileOutcome,
        ResourceConfig, StreamType,
    };
    use a3s_box_runtime::{
        LinuxKvmOciMigrationConfig, LocalExecutionManager, ManagedExecutionStore,
        ManagedRuntimeRoute, OciRuntimeBinding,
    };
    use a3s_oci_sdk::{DriverKind, IsolationClass as OciIsolationClass};
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use serde::Serialize;
    use serde_json::Value;

    const ENABLE_ENV: &str = "A3S_BOX_KVM_LIVE_SESSION_QUALIFICATION";
    const HOME_ENV: &str = "A3S_HOME";
    const HOST_ROOT_ENV: &str = "A3S_BOX_OCI_HOST_ROOT";
    const ENDPOINT_ENV: &str = "A3S_BOX_OCI_KVM_ENDPOINT";
    const IMAGE_ENV: &str = "A3S_BOX_KVM_LIVE_SESSION_IMAGE";
    const REPORT_ENV: &str = "A3S_BOX_KVM_LIVE_SESSION_REPORT";
    const BOX_SHA_ENV: &str = "A3S_BOX_KVM_LIVE_SESSION_BOX_SHA";
    const OCI_SHA_ENV: &str = "A3S_BOX_KVM_LIVE_SESSION_OCI_SHA";
    const SERVICE_PID_ENV: &str = "A3S_BOX_KVM_LIVE_SESSION_SERVICE_PID";
    const SERVICE_BIN_ENV: &str = "A3S_BOX_KVM_LIVE_SESSION_SERVICE_BIN";
    const SERVICE_ROOT_ENV: &str = "A3S_BOX_KVM_LIVE_SESSION_SERVICE_ROOT";
    const SERVICE_SHIM_ENV: &str = "A3S_BOX_KVM_LIVE_SESSION_SERVICE_SHIM";
    const SERVICE_MANIFEST_ENV: &str = "A3S_BOX_KVM_LIVE_SESSION_SERVICE_MANIFEST";
    const SERVICE_LOG_ENV: &str = "A3S_BOX_KVM_LIVE_SESSION_SERVICE_LOG";
    const SESSION_OWNER_ENV: &str = "A3S_OCI_KVM_SESSION_OWNER";
    const SCHEMA_VERSION: &str = "a3s.box.linux-kvm-live-session.v2";
    const KVM_LIVE_BINDING_SCHEMA: &str = "a3s.oci.kvm-live-session-binding.v1";
    const KVM_LIVE_BINDING_FILE: &str = ".a3s-oci-kvm-live-session-binding.json";
    const KEYED_EXEC_BEFORE: &str = "a3s.box.live-session.keyed-exec.before-owner-kill";
    const KEYED_EXEC_AFTER: &str = "a3s.box.live-session.keyed-exec.after-reopen";
    const KEYED_EXEC_MARKER: &[u8] = b"live-session-keyed-ok\n";
    const STREAM_MARKER: &[u8] = b"live-session-stream-ok\n";
    const STREAM_ECHO_BEFORE: &[u8] = b"before-owner-kill\n";
    const STREAM_ECHO_AFTER: &[u8] = b"after-owner-reopen\n";
    const FS_GUEST_PATH: &str = "/tmp/.a3s-box-kvm-live-fs.bin";
    const FS_PAYLOAD: &[u8] = b"a3s-box-kvm-live-fs\0binary\nv2\n";

    type AnyError = Box<dyn Error + Send + Sync>;

    #[derive(Debug, Clone)]
    struct ServiceRestartInputs {
        pid: u32,
        service_bin: PathBuf,
        service_root: PathBuf,
        service_shim: PathBuf,
        system_image_manifest: PathBuf,
        service_log: PathBuf,
    }

    #[derive(Debug, Clone)]
    struct Inputs {
        home_dir: PathBuf,
        state_path: PathBuf,
        runtime_root: PathBuf,
        endpoint: PathBuf,
        image: String,
        service: ServiceRestartInputs,
    }

    #[derive(Debug, Clone, Serialize)]
    struct ProcessIdentityReport {
        pid: u32,
        start_time_ticks: u64,
    }

    #[derive(Debug, Clone)]
    struct KvmLiveBindingSnapshot {
        schema_version: String,
        session_owner: ProcessIdentityReport,
        shim: ProcessIdentityReport,
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
        host_root: Option<PathBuf>,
        image: Option<String>,
        driver_target: &'static str,
        kvm_microvm_live_claimed: bool,
        utility_vm_claimed: bool,
        /// Set only when the same streaming `start_process` handle continues
        /// after a real Host Service SIGKILL + Live reopen.
        retained_stream_handle_proven: bool,
        /// Upload before Host Service SIGKILL via public Box `transfer_file`.
        file_upload_before_kill: bool,
        /// Download after Live reopen matches the pre-kill upload payload.
        file_download_after_reattach: bool,
        /// Set only when upload-before-kill bytes match download-after-reattach
        /// on the same Box generation that stayed Ready/Running without exit.
        retained_filesystem_proven: bool,
        /// Always false: fixture `process_restart` continuity is not this gate.
        fixture_stream_continuity_claimed: bool,
        /// Always false: harness reports never self-certify ROADMAP B2 close.
        b2_process_session_recovery_closed: bool,
        supervised_create_required: bool,
        supervised_create_enabled: bool,
        session_supervisor_recorded: bool,
        recovery_schema_version: Option<String>,
        operation_id: Option<String>,
        execution_id: Option<String>,
        box_generation: Option<ExecutionGeneration>,
        runtime_binding: Option<OciRuntimeBinding>,
        observed_running_before_owner_kill: bool,
        inventory_before_owner_kill_non_empty: bool,
        keyed_captured_exec_before_owner_kill: bool,
        owner_before_kill: Option<ProcessIdentityReport>,
        supervisor_before_kill: Option<ProcessIdentityReport>,
        launcher_before_kill: Option<ProcessIdentityReport>,
        init_before_kill: Option<ProcessIdentityReport>,
        owner_sigkilled: bool,
        owner_gone: bool,
        supervisor_survived_owner_kill: bool,
        launcher_survived_owner_kill: bool,
        init_survived_owner_kill: bool,
        owner_rebound: bool,
        owner_after_reopen: Option<ProcessIdentityReport>,
        reconciled_ready_after_reopen: bool,
        observed_running_after_reopen: bool,
        inventory_after_reopen_non_empty: bool,
        init_pid_continuous_after_reopen: bool,
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
                host_root: None,
                image: None,
                driver_target: "linux-kvm-dedicated-vm",
                kvm_microvm_live_claimed: false,
                utility_vm_claimed: false,
                retained_stream_handle_proven: false,
                file_upload_before_kill: false,
                file_download_after_reattach: false,
                retained_filesystem_proven: false,
                fixture_stream_continuity_claimed: false,
                b2_process_session_recovery_closed: false,
                supervised_create_required: true,
                supervised_create_enabled: false,
                session_supervisor_recorded: false,
                recovery_schema_version: None,
                operation_id: None,
                execution_id: None,
                box_generation: None,
                runtime_binding: None,
                observed_running_before_owner_kill: false,
                inventory_before_owner_kill_non_empty: false,
                keyed_captured_exec_before_owner_kill: false,
                owner_before_kill: None,
                supervisor_before_kill: None,
                launcher_before_kill: None,
                init_before_kill: None,
                owner_sigkilled: false,
                owner_gone: false,
                supervisor_survived_owner_kill: false,
                launcher_survived_owner_kill: false,
                init_survived_owner_kill: false,
                owner_rebound: false,
                owner_after_reopen: None,
                reconciled_ready_after_reopen: false,
                observed_running_after_reopen: false,
                inventory_after_reopen_non_empty: false,
                init_pid_continuous_after_reopen: false,
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
                eprintln!("KVM live-session qualification cannot select its report: {error}");
                std::process::exit(2);
            }
        };
        let mut report = QualificationReport::new();

        let inputs = load_inputs(&mut report);
        let outcome = match inputs {
            Ok(inputs) => {
                let operation_id =
                    OperationId::new(format!("linux-kvm-live-session-{}", uuid::Uuid::new_v4()));
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
                "KVM live-session qualification could not write {}: {error}",
                report_path.display()
            );
            std::process::exit(1);
        }
        if report.status != "passed" {
            eprintln!(
                "KVM live-session qualification failed: {}",
                report.error.as_deref().unwrap_or("unknown failure")
            );
            std::process::exit(1);
        }
        println!(
            "KVM live-session qualification passed: {}",
            report_path.display()
        );
    }

    fn load_inputs(report: &mut QualificationReport) -> Result<Inputs, AnyError> {
        require(
            std::env::var(ENABLE_ENV).as_deref() == Ok("1"),
            format!("set {ENABLE_ENV}=1 to acknowledge the destructive qualification"),
        )?;
        require(
            std::env::var(SESSION_OWNER_ENV).as_deref() == Ok("1"),
            format!(
                "set {SESSION_OWNER_ENV}=1; durable KVM session-owner create is required (fail-closed without it)"
            ),
        )?;
        report.supervised_create_enabled = true;

        let home_dir = absolute_environment_path(HOME_ENV)?;
        require(
            home_dir
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("kvm-live-session")),
            "A3S_HOME must name a dedicated kvm-live-session directory",
        )?;
        let runtime_root = absolute_environment_path(HOST_ROOT_ENV)?;
        let endpoint = absolute_environment_path(ENDPOINT_ENV)?;
        require(
            endpoint
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".sock")),
            format!("{ENDPOINT_ENV} must be an absolute Unix socket path"),
        )?;
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
        let service = load_service_restart_inputs()?;
        let state_path = home_dir.join("managed-executions.json");

        report.home_dir = Some(home_dir.clone());
        report.state_path = Some(state_path.clone());
        report.host_root = Some(runtime_root.clone());
        report.image = Some(image.clone());
        report.box_commit_sha = Some(box_sha.clone());
        report.oci_runtime_commit_sha = Some(oci_sha.clone());

        let _ = (box_sha, oci_sha);
        Ok(Inputs {
            home_dir,
            state_path,
            runtime_root,
            endpoint,
            image,
            service,
        })
    }

    fn load_service_restart_inputs() -> Result<ServiceRestartInputs, AnyError> {
        let pid_raw = required_environment_string(SERVICE_PID_ENV)?;
        let pid: u32 = pid_raw
            .parse()
            .map_err(|_| failure(format!("{SERVICE_PID_ENV} must be a positive pid")))?;
        require(pid > 1, format!("{SERVICE_PID_ENV} must be a positive pid"))?;
        Ok(ServiceRestartInputs {
            pid,
            service_bin: absolute_environment_path(SERVICE_BIN_ENV)?,
            service_root: absolute_environment_path(SERVICE_ROOT_ENV)?,
            service_shim: absolute_environment_path(SERVICE_SHIM_ENV)?,
            system_image_manifest: absolute_environment_path(SERVICE_MANIFEST_ENV)?,
            service_log: absolute_environment_path(SERVICE_LOG_ENV)?,
        })
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
        .map_err(|_| failure("timed out preparing or starting the KVM MicroVM execution"))??;
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
            binding.driver == DriverKind::LibkrunKvm
                && binding.isolation == OciIsolationClass::DedicatedVm,
            "runtime did not return the dedicated-VM libkrun/KVM binding",
        )?;
        report.runtime_binding = Some(binding.clone());

        wait_until_running(&manager, &reservation.execution_id, report, true).await?;

        let inventory = manager
            .list_processes(&reservation.execution_id, reservation.generation)
            .await?;
        require(
            !inventory.processes.is_empty(),
            "process inventory was empty before Host Service SIGKILL",
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

        prove_file_upload_before_kill(&manager, &reservation.execution_id, reservation.generation)
            .await?;
        report.file_upload_before_kill = true;

        let host_service = host_service_identity(inputs.service.pid)?;
        report.owner_before_kill = Some(host_service.clone());
        let live_binding = load_kvm_live_binding(&inputs.runtime_root, &binding)?;
        report.recovery_schema_version = Some(live_binding.schema_version.clone());
        report.session_supervisor_recorded = true;
        report.supervisor_before_kill = Some(live_binding.session_owner.clone());
        report.launcher_before_kill = Some(live_binding.shim.clone());
        let init = init_identity_from_inventory(&inventory, &binding)?;
        report.init_before_kill = Some(init.clone());
        require_live_identity("KVM session-owner", &live_binding.session_owner)?;
        require_live_identity("KVM shim", &live_binding.shim)?;
        // Guest init PID is not a host /proc identity — continuity is proven via
        // process inventory before/after Host reopen, not host PID liveness.

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
                    "streaming start_process before Host Service SIGKILL failed: {error}"
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
                    "streaming stdin write before Host Service SIGKILL failed: {error}"
                ))
            })?;
        expect_stream_echo(
            process.as_mut(),
            STREAM_ECHO_BEFORE,
            "before Host Service SIGKILL",
        )
        .await?;

        // Keep the Box manager: retained-handle continuity is the v1 gate.
        sigkill_host_service(inputs.service.pid)?;
        report.owner_sigkilled = true;
        wait_host_service_gone(
            inputs.service.pid,
            &inputs.endpoint,
            Duration::from_secs(30),
        )?;
        report.owner_gone = true;

        require_live_identity(
            "KVM session-owner after Host Service SIGKILL",
            &live_binding.session_owner,
        )?;
        report.supervisor_survived_owner_kill = true;
        require_live_identity("KVM shim after Host Service SIGKILL", &live_binding.shim)?;
        report.launcher_survived_owner_kill = true;
        // Guest init is not visible on the host; continuous inventory PID after
        // reopen is the authentic MicroVM init survival proof.
        let _ = &init;

        let disconnect = process.next_event().await;
        require(
            matches!(disconnect, Err(ExecutionManagerError::Unavailable(_))),
            format!(
                "retained stream must surface Unavailable on Host Service death, got {disconnect:?}"
            ),
        )?;

        restart_host_service(&inputs.service, &inputs.endpoint)?;
        let replacement_host = read_replacement_host_service_pid(&inputs.service.service_root)?;
        report.owner_rebound = replacement_host.pid != host_service.pid
            || replacement_host.start_time_ticks != host_service.start_time_ticks;
        report.owner_after_reopen = Some(replacement_host);

        let mut outcome = None;
        let reconcile_deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        while outcome.is_none() {
            match manager.reconcile(operation_id).await {
                Ok(value) => outcome = Some(value),
                Err(ExecutionManagerError::Unavailable(_)) => {
                    if tokio::time::Instant::now() >= reconcile_deadline {
                        return Err(failure(
                            "Box remained Unavailable for 120s after Host Service SIGKILL",
                        ));
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(error) => {
                    return Err(failure(format!(
                        "Box reconcile failed after Host Service SIGKILL: {error}"
                    )));
                }
            }
        }
        match outcome.expect("reconcile outcome assigned before loop exit") {
            ReconcileOutcome::Ready(recovered) => {
                require(
                    recovered.execution_id == reservation.execution_id
                        && recovered.generation == reservation.generation,
                    "Host Service reopen recovered a different Ready Box generation",
                )?;
                report.reconciled_ready_after_reopen = true;
            }
            ReconcileOutcome::Failed => {
                return Err(failure(
                    "Host Service reopen reconciled Failed/stopped instead of Live Ready (Live path unavailable)",
                ));
            }
            ReconcileOutcome::Created(_) => {
                return Err(failure(
                    "Host Service reopen left the generation Created instead of Live Ready",
                ));
            }
            ReconcileOutcome::Creating => {
                return Err(failure(
                    "Host Service reopen left the Box generation stuck creating",
                ));
            }
            ReconcileOutcome::Absent => {
                return Err(failure(
                    "Host Service reopen lost the Box operation before Live reconciliation",
                ));
            }
        }

        input
            .write_stdin(STREAM_ECHO_AFTER)
            .await
            .map_err(|error| {
                failure(format!(
                    "retained streaming stdin after Host Service reopen failed: {error}"
                ))
            })?;
        expect_stream_echo(
            process.as_mut(),
            STREAM_ECHO_AFTER,
            "after Host Service reopen",
        )
        .await?;
        input.close_stdin().await.map_err(|error| {
            failure(format!(
                "retained streaming close_stdin after Host Service reopen failed: {error}"
            ))
        })?;
        input
            .send_signal(ExecutionProcessSignal::Kill)
            .await
            .map_err(|error| {
                failure(format!(
                    "retained streaming signal after Host Service reopen failed: {error}"
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
                        "retained streaming drain after Host Service reopen failed: {error}"
                    )));
                }
            }
            if tokio::time::Instant::now() >= drain_deadline {
                return Err(failure(
                    "timed out draining retained streaming process after Host Service reopen",
                ));
            }
        }
        require(
            saw_exit,
            "retained streaming process did not publish Exit after reopen signal",
        )?;
        report.retained_stream_handle_proven = true;
        report.kvm_microvm_live_claimed = report.retained_stream_handle_proven;

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

        prove_file_download_after_reattach(
            &manager,
            &reservation.execution_id,
            reservation.generation,
        )
        .await?;
        report.file_download_after_reattach = true;
        report.retained_filesystem_proven = report.file_upload_before_kill
            && report.file_download_after_reattach
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
        let init_continuous = inventory_after
            .processes
            .iter()
            .any(|process| process.process_id == "init" && process.pid == Some(init.pid));
        require(
            init_continuous,
            format!(
                "Live reopen inventory lost continuous init PID {}",
                init.pid
            ),
        )?;
        report.init_pid_continuous_after_reopen = true;
        report.init_survived_owner_kill = true;

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
        // Short printf payloads can fully reap before OCI captures recovery
        // identity after Host reopen (retryable Unavailable). Retry the same
        // request_id so prepare-exec reconciles the partial journal instead of
        // minting `{id}.retry-N` process identities that orphan active_operation
        // claims and block generation delete (parity with Native Live #300).
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
                            "keyed captured exec `{request_id}` failed on Live generation: execution backend unavailable: {message}"
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

    async fn prove_file_upload_before_kill(
        manager: &LocalExecutionManager,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> Result<(), AnyError> {
        let response = manager
            .transfer_file(
                execution_id,
                generation,
                FileRequest {
                    op: FileOp::Upload,
                    guest_path: FS_GUEST_PATH.to_string(),
                    data: Some(STANDARD.encode(FS_PAYLOAD)),
                    user: None,
                    max_bytes: None,
                },
            )
            .await
            .map_err(|error| {
                failure(format!(
                    "Live retained file upload before Host Service SIGKILL failed: {error}"
                ))
            })?;
        require(
            response.success && response.error.is_none(),
            format!(
                "Live retained file upload before Host Service SIGKILL reported failure: {:?}",
                response.error
            ),
        )?;
        require(
            response.size == FS_PAYLOAD.len() as u64,
            format!(
                "Live retained file upload size mismatch: got {} expected {}",
                response.size,
                FS_PAYLOAD.len()
            ),
        )?;
        Ok(())
    }

    async fn prove_file_download_after_reattach(
        manager: &LocalExecutionManager,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> Result<(), AnyError> {
        // Downloads can hit retryable Unavailable briefly after Host reopen.
        let request = FileRequest {
            op: FileOp::Download,
            guest_path: FS_GUEST_PATH.to_string(),
            data: None,
            user: None,
            max_bytes: Some(FS_PAYLOAD.len() as u64),
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let response = loop {
            match manager
                .transfer_file(execution_id, generation, request.clone())
                .await
            {
                Ok(response) => break response,
                Err(ExecutionManagerError::Unavailable(message)) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(failure(format!(
                            "Live retained file download after reopen failed: execution backend unavailable: {message}"
                        )));
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => {
                    return Err(failure(format!(
                        "Live retained file download after reopen failed: {error}"
                    )));
                }
            }
        };
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
            "Live retained file download did not match the pre-SIGKILL upload payload",
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
                        "expected running KVM MicroVM generation, found {:?}",
                        status.state
                    )));
                }
                ExecutionState::Created | ExecutionState::Creating | ExecutionState::Paused => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(failure(
                    "timed out waiting for the KVM MicroVM execution to run",
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn connect(inputs: &Inputs) -> Result<LocalExecutionManager, AnyError> {
        require(
            std::env::var(SESSION_OWNER_ENV).as_deref() == Ok("1"),
            format!("{SESSION_OWNER_ENV}=1 must remain set for Live reconnect"),
        )?;
        let config =
            LinuxKvmOciMigrationConfig::new(inputs.runtime_root.clone(), inputs.endpoint.clone())?;
        Ok(LocalExecutionManager::with_linux_kvm_oci_qualification(
            &inputs.state_path,
            &inputs.home_dir,
            config,
        )
        .await?)
    }

    fn qualification_request(image: &str) -> CreateExecutionRequest {
        CreateExecutionRequest {
            external_sandbox_id: "linux-kvm-live-session".to_string(),
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
                    "printf 'a3s-box-kvm-live-session\\n'; while :; do sleep 60; done".to_string(),
                ],
                network: NetworkMode::None,
                persistent: false,
                ..Default::default()
            },
            labels: BTreeMap::from([("purpose".to_string(), "linux-kvm-live-session".to_string())]),
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

    fn load_kvm_live_binding(
        runtime_root: &Path,
        binding: &OciRuntimeBinding,
    ) -> Result<KvmLiveBindingSnapshot, AnyError> {
        // OCI publishes under shares/<container-id>/<generation>/ — walk
        // recursively. containerId in the JSON may be absent at publish time.
        let shares_root = runtime_root.join("shares");
        let target_id = binding.target.id.as_str();
        let mut candidates = Vec::new();
        walk_for_live_binding(&shares_root, &mut candidates)?;
        let matched: Vec<_> = candidates
            .into_iter()
            .filter(|path| {
                path.components()
                    .any(|component| component.as_os_str() == target_id)
            })
            .collect();
        let path = match matched.as_slice() {
            [path] => path.clone(),
            [] => {
                return Err(failure(format!(
                    "expected one KVM Live binding under {} for container {target_id}, found 0",
                    shares_root.display()
                )));
            }
            paths => {
                return Err(failure(format!(
                    "expected one KVM Live binding under {} for container {target_id}, found {}",
                    shares_root.display(),
                    paths.len()
                )));
            }
        };
        let value: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        let schema_version = value
            .get("schemaVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| failure("KVM Live binding lacks schemaVersion"))?
            .to_string();
        require(
            schema_version == KVM_LIVE_BINDING_SCHEMA,
            format!("unexpected KVM Live binding schema {schema_version}"),
        )?;
        Ok(KvmLiveBindingSnapshot {
            schema_version,
            session_owner: identity_from_binding(&value, "sessionOwner")?,
            shim: identity_from_binding(&value, "shim")?,
        })
    }

    fn walk_for_live_binding(root: &Path, found: &mut Vec<PathBuf>) -> Result<(), AnyError> {
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(failure(format!(
                    "failed to enumerate {}: {error}",
                    root.display()
                )));
            }
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                walk_for_live_binding(&path, found)?;
            } else if entry.file_name() == KVM_LIVE_BINDING_FILE {
                found.push(path);
            }
        }
        Ok(())
    }

    fn identity_from_binding(
        value: &Value,
        field: &str,
    ) -> Result<ProcessIdentityReport, AnyError> {
        let identity = value
            .get(field)
            .ok_or_else(|| failure(format!("KVM Live binding lacks {field} identity")))?;
        let pid = identity
            .get("pid")
            .and_then(Value::as_i64)
            .ok_or_else(|| failure(format!("KVM Live binding.{field} lacks pid")))?
            as u32;
        let start_time_ticks = identity
            .get("startTimeTicks")
            .and_then(Value::as_u64)
            .ok_or_else(|| failure(format!("KVM Live binding.{field} lacks startTimeTicks")))?;
        Ok(ProcessIdentityReport {
            pid,
            start_time_ticks,
        })
    }

    fn init_identity_from_inventory(
        inventory: &a3s_box_core::ExecutionProcessInventory,
        binding: &OciRuntimeBinding,
    ) -> Result<ProcessIdentityReport, AnyError> {
        let init = inventory
            .processes
            .iter()
            .find(|process| process.process_id == "init")
            .ok_or_else(|| failure("process inventory lacks init before Host Service SIGKILL"))?;
        let pid = init
            .pid
            .ok_or_else(|| failure("init process lacked a live PID before Host Service SIGKILL"))?;
        // Guest PID space — not visible in host /proc. Continuity is inventory-only.
        let _ = binding;
        Ok(ProcessIdentityReport {
            pid,
            start_time_ticks: 0,
        })
    }

    fn host_service_identity(pid: u32) -> Result<ProcessIdentityReport, AnyError> {
        let start_time_ticks = process_start_time(pid)?
            .ok_or_else(|| failure("Host Service PID was not alive before SIGKILL"))?;
        Ok(ProcessIdentityReport {
            pid,
            start_time_ticks,
        })
    }

    fn read_replacement_host_service_pid(
        service_root: &Path,
    ) -> Result<ProcessIdentityReport, AnyError> {
        let pid_path = service_root.join("qualification-service.pid");
        let raw = std::fs::read_to_string(&pid_path).map_err(|error| {
            failure(format!(
                "replacement Host Service pid file missing: {error}"
            ))
        })?;
        let pid: u32 = raw
            .trim()
            .parse()
            .map_err(|_| failure("replacement Host Service pid file is malformed"))?;
        host_service_identity(pid)
    }

    fn process_start_time(pid: u32) -> Result<Option<u64>, AnyError> {
        let raw = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(raw) => raw,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let closing = raw
            .rfind(") ")
            .ok_or_else(|| failure(format!("process {pid} has malformed stat evidence")))?;
        let fields: Vec<&str> = raw[closing + 2..].split_whitespace().collect();
        require(
            fields.len() > 19,
            format!("process {pid} has incomplete stat evidence"),
        )?;
        require(
            fields[0].len() == 1,
            format!("process {pid} has invalid state evidence"),
        )?;
        if matches!(fields[0], "Z" | "X" | "x") {
            return Ok(None);
        }
        Ok(Some(fields[19].parse()?))
    }

    fn require_live_identity(
        label: &str,
        identity: &ProcessIdentityReport,
    ) -> Result<(), AnyError> {
        require(identity.pid > 0 && identity.start_time_ticks > 0, {
            format!("{label} has an invalid process identity")
        })?;
        let observed = process_start_time(identity.pid)?;
        require(
            observed == Some(identity.start_time_ticks),
            format!("{label} is not alive with its recorded identity"),
        )
    }

    fn sigkill_host_service(pid: u32) -> Result<(), AnyError> {
        // SAFETY: qualification-only SIGKILL of the exact operator-supplied Host Service pid.
        let kill_rc = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        if kill_rc != 0 {
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::NotFound {
                return Err(failure(format!(
                    "failed to SIGKILL Host Service pid {pid}: {err}"
                )));
            }
        }
        Ok(())
    }

    fn wait_host_service_gone(
        pid: u32,
        endpoint: &Path,
        timeout: Duration,
    ) -> Result<(), AnyError> {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            let observed = process_start_time(pid)?;
            if observed.is_none() && !endpoint.exists() {
                return Ok(());
            }
            if endpoint.exists() {
                let _ = std::fs::remove_file(endpoint);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Err(failure(
            "Host Service remained alive or left its endpoint after expected SIGKILL",
        ))
    }

    fn restart_host_service(
        service: &ServiceRestartInputs,
        endpoint: &Path,
    ) -> Result<(), AnyError> {
        if let Some(parent) = service.service_log.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&service.service_log)?;
        let log_err = log.try_clone()?;
        let mut child = Command::new(&service.service_bin)
            .arg("box-kvm-qualification-service")
            .arg("--root")
            .arg(&service.service_root)
            .arg("--shim")
            .arg(&service.service_shim)
            .arg("--system-image-manifest")
            .arg(&service.system_image_manifest)
            .env(SESSION_OWNER_ENV, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .spawn()?;
        let replacement_pid = child.id();
        std::fs::write(
            service.service_root.join("qualification-service.pid"),
            format!("{replacement_pid}\n"),
        )?;

        let ready_deadline = std::time::Instant::now() + Duration::from_secs(60);
        while std::time::Instant::now() < ready_deadline {
            if endpoint.exists() {
                std::mem::forget(child);
                return Ok(());
            }
            if let Some(status) = child.try_wait()? {
                return Err(failure(format!(
                    "replacement Host Service exited before readiness: {status}"
                )));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        let _ = child.wait();
        Err(failure(
            "replacement Host Service did not recreate the Unix endpoint",
        ))
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
        message.into().into()
    }
}
