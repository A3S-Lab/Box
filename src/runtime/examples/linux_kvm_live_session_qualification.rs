//! Destructive live-session qualification for Box over KVM MicroVM OCI.
//!
//! Exercises the public Box Sandbox path with
//! `ExecutionIsolation::Microvm` via
//! `LocalExecutionManager::with_linux_kvm_oci_qualification`, keeps the Box
//! manager across a Host Service SIGKILL, proves retained streaming
//! process-handle continuity plus keyed captured exec / state / inventory /
//! stats / kill, without inventing an exit status.
//!
//! Host Service (first spawn + replacement) must run with
//! `A3S_OCI_KVM_SESSION_OWNER=1` so Guest/session-owner survive Host death and
//! Live reopen reconciles to Ready (not stopped-only).
//!
//! Schema `a3s.box.linux-kvm-live-session.v1`.
//!
//! Honest scope (anti-overfit):
//! - `retained_stream_handle_proven` is set only when the same
//!   `start_process` handle continues stdin/output/signal after Host reopen.
//! - `kvm_microvm_live_claimed` is set only when retained stream is proven.
//! - Fixture `process_restart` continuity is never claimed
//!   (`fixture_stream_continuity_claimed` stays false).
//! - Does **not** close Box B2 (`b2_process_session_recovery_closed` stays
//!   false) and does **not** overload the stopped-only
//!   `linux-kvm-oci-qualification` schema.

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
        ExecutionSessionManager, ExecutionState, IsolationClass as BoxIsolationClass, NetworkMode,
        OperationId, ReconcileOutcome, ResourceConfig, StreamType,
    };
    use a3s_box_runtime::{
        LinuxKvmOciMigrationConfig, LocalExecutionManager, ManagedExecutionStore,
        ManagedRuntimeRoute, OciRuntimeBinding,
    };
    use a3s_oci_sdk::{DriverKind, IsolationClass as OciIsolationClass};
    use serde::Serialize;

    const ENABLE_ENV: &str = "A3S_BOX_KVM_LIVE_SESSION_QUALIFICATION";
    const HOME_ENV: &str = "A3S_HOME";
    const HOST_ROOT_ENV: &str = "A3S_BOX_OCI_HOST_ROOT";
    const ENDPOINT_ENV: &str = "A3S_BOX_OCI_KVM_ENDPOINT";
    const IMAGE_ENV: &str = "A3S_BOX_KVM_LIVE_SESSION_IMAGE";
    const REPORT_ENV: &str = "A3S_BOX_KVM_LIVE_SESSION_REPORT";
    const BOX_SHA_ENV: &str = "A3S_BOX_KVM_LIVE_SESSION_BOX_SHA";
    const OCI_SHA_ENV: &str = "A3S_BOX_KVM_LIVE_SESSION_OCI_SHA";
    const SESSION_OWNER_ENV: &str = "A3S_OCI_KVM_SESSION_OWNER";
    const SERVICE_PID_ENV: &str = "A3S_BOX_KVM_OCI_SERVICE_PID";
    const SERVICE_BIN_ENV: &str = "A3S_BOX_KVM_OCI_SERVICE_BIN";
    const SERVICE_ROOT_ENV: &str = "A3S_BOX_KVM_OCI_SERVICE_ROOT";
    const SERVICE_SHIM_ENV: &str = "A3S_BOX_KVM_OCI_SERVICE_SHIM";
    const SERVICE_MANIFEST_ENV: &str = "A3S_BOX_KVM_OCI_SERVICE_MANIFEST";
    const SERVICE_LOG_ENV: &str = "A3S_BOX_KVM_OCI_SERVICE_LOG";
    const SCHEMA_VERSION: &str = "a3s.box.linux-kvm-live-session.v1";
    const KEYED_EXEC_BEFORE: &str = "a3s.box.kvm-live-session.keyed-exec.before-host-kill";
    const KEYED_EXEC_AFTER: &str = "a3s.box.kvm-live-session.keyed-exec.after-reopen";
    const KEYED_EXEC_MARKER: &[u8] = b"live-session-keyed-ok\n";
    const STREAM_MARKER: &[u8] = b"live-session-stream-ok\n";
    const STREAM_ECHO_BEFORE: &[u8] = b"before-host-kill\n";
    const STREAM_ECHO_AFTER: &[u8] = b"after-host-reopen\n";

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
        endpoint: Option<PathBuf>,
        image: Option<String>,
        driver_target: &'static str,
        /// Set only when retained streaming continuity is proven.
        kvm_microvm_live_claimed: bool,
        utility_vm_claimed: bool,
        /// Set only when the same streaming `start_process` handle continues
        /// after a real Host Service SIGKILL + Live reopen.
        retained_stream_handle_proven: bool,
        /// Always false: fixture `process_restart` continuity is not this gate.
        fixture_stream_continuity_claimed: bool,
        /// Always false: B2 still requires the plan's full criteria.
        b2_process_session_recovery_closed: bool,
        session_owner_required: bool,
        session_owner_enabled: bool,
        operation_id: Option<String>,
        execution_id: Option<String>,
        box_generation: Option<ExecutionGeneration>,
        runtime_binding: Option<OciRuntimeBinding>,
        observed_running_before_host_kill: bool,
        inventory_before_host_kill_non_empty: bool,
        init_pid_before_host_kill: Option<u32>,
        keyed_captured_exec_before_host_kill: bool,
        host_service_before_kill: Option<ProcessIdentityReport>,
        host_service_sigkilled: bool,
        host_service_gone: bool,
        host_service_restarted: bool,
        host_service_after_reopen: Option<ProcessIdentityReport>,
        host_service_rebound: bool,
        reconciled_ready_after_reopen: bool,
        observed_running_after_reopen: bool,
        inventory_after_reopen_non_empty: bool,
        init_pid_after_reopen: Option<u32>,
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
                runtime_root: None,
                endpoint: None,
                image: None,
                driver_target: "libkrun-kvm-dedicated-vm",
                kvm_microvm_live_claimed: false,
                utility_vm_claimed: false,
                retained_stream_handle_proven: false,
                fixture_stream_continuity_claimed: false,
                b2_process_session_recovery_closed: false,
                session_owner_required: true,
                session_owner_enabled: false,
                operation_id: None,
                execution_id: None,
                box_generation: None,
                runtime_binding: None,
                observed_running_before_host_kill: false,
                inventory_before_host_kill_non_empty: false,
                init_pid_before_host_kill: None,
                keyed_captured_exec_before_host_kill: false,
                host_service_before_kill: None,
                host_service_sigkilled: false,
                host_service_gone: false,
                host_service_restarted: false,
                host_service_after_reopen: None,
                host_service_rebound: false,
                reconciled_ready_after_reopen: false,
                observed_running_after_reopen: false,
                inventory_after_reopen_non_empty: false,
                init_pid_after_reopen: None,
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
                let operation_id = OperationId::new(format!(
                    "linux-kvm-live-session-{}",
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
                "set {SESSION_OWNER_ENV}=1; Live session-owner Host Service is required (fail-closed without it)"
            ),
        )?;
        report.session_owner_enabled = true;

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
        let state_path = home_dir.join("managed-executions.json");
        let service = load_service_restart_inputs()?;

        report.home_dir = Some(home_dir.clone());
        report.state_path = Some(state_path.clone());
        report.runtime_root = Some(runtime_root.clone());
        report.endpoint = Some(endpoint.clone());
        report.image = Some(image.clone());
        report.box_commit_sha = Some(box_sha);
        report.oci_runtime_commit_sha = Some(oci_sha);

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
        report.runtime_binding = Some(binding);

        wait_until_running(&manager, &reservation.execution_id, report, true).await?;

        let inventory = manager
            .list_processes(&reservation.execution_id, reservation.generation)
            .await?;
        require(
            !inventory.processes.is_empty(),
            "process inventory was empty before Host Service SIGKILL",
        )?;
        report.inventory_before_host_kill_non_empty = true;
        let init_before = inventory
            .processes
            .iter()
            .find(|process| process.process_id == "init")
            .and_then(|process| process.pid);
        report.init_pid_before_host_kill = init_before;

        prove_keyed_captured_exec(
            &manager,
            &reservation.execution_id,
            reservation.generation,
            KEYED_EXEC_BEFORE,
        )
        .await?;
        report.keyed_captured_exec_before_host_kill = true;

        let host_before = process_identity(inputs.service.pid)?;
        report.host_service_before_kill = Some(host_before.clone());
        require_live_identity("Host Service", &host_before)?;

        let mut process = manager
            .start_process(
                &reservation.execution_id,
                reservation.generation,
                ExecRequest {
                    // Streaming sessions are not one-shot keyed ops; request_id
                    // is reserved for captured exec replay (see oci_session).
                    request_id: None,
                    cmd: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        // Stay alive after stdin EOF so close_stdin + Kill both exercise
                        // the retained handle (a plain `while read` exits on EOF and
                        // makes Kill fail closed as not-live).
                        "printf 'live-session-stream-ok\\n'; while true; do if IFS= read -r line; then printf 'echo:%s\\n' \"$line\"; else while true; do sleep 3600; done; fi; done".into(),
                    ],
                    // Must outlive Host-gone wait + Host respawn + reattach +
                    // reconcile; a short watchdog poisons the retained stream
                    // before Live reopen completes.
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

        // Keep the Box manager: retained-handle continuity is the Live gate.
        // Kill Host first (do not require Guest death), surface Unavailable on
        // the retained stream, then recreate the Host with SESSION_OWNER=1.
        sigkill_host_service(&host_before)?;
        report.host_service_sigkilled = true;
        wait_host_gone_and_clear_endpoint(&host_before, &inputs.endpoint)?;
        report.host_service_gone = true;

        let disconnect = process.next_event().await;
        require(
            matches!(disconnect, Err(ExecutionManagerError::Unavailable(_))),
            format!(
                "retained stream must surface Unavailable on Host Service death, got {disconnect:?}"
            ),
        )?;

        let replacement_pid = spawn_replacement_host_service(&inputs.service, &inputs.endpoint)?;
        report.host_service_restarted = true;

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
                    "Host reopen recovered a different Ready Box generation",
                )?;
                report.reconciled_ready_after_reopen = true;
            }
            ReconcileOutcome::Failed => {
                return Err(failure(
                    "Host reopen reconciled Failed/stopped instead of Live Ready (Live path unavailable; ensure A3S_OCI_KVM_SESSION_OWNER=1)",
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

        let host_after = process_identity(replacement_pid)?;
        require(
            host_after.pid != host_before.pid
                || host_after.start_time_ticks != host_before.start_time_ticks,
            "replacement Host Service reused the killed identity",
        )?;
        report.host_service_rebound = true;
        report.host_service_after_reopen = Some(host_after);

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
        // Claim MicroVM Live only when the retained stream path actually passed.
        report.kvm_microvm_live_claimed = true;

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

        let inventory_after = manager
            .list_processes(&reservation.execution_id, reservation.generation)
            .await?;
        require(
            !inventory_after.processes.is_empty(),
            "process inventory was empty after Live reopen",
        )?;
        report.inventory_after_reopen_non_empty = true;
        let init_after = inventory_after
            .processes
            .iter()
            .find(|process| process.process_id == "init")
            .and_then(|process| process.pid);
        report.init_pid_after_reopen = init_after;
        if let (Some(before), Some(after_pid)) = (init_before, init_after) {
            require(
                before == after_pid,
                format!("Live reopen inventory lost continuous init PID {before} (now {after_pid})"),
            )?;
            report.init_pid_continuous_after_reopen = true;
        } else if init_before.is_some() || init_after.is_some() {
            return Err(failure(
                "Live reopen could not compare continuous init PID (missing before or after)",
            ));
        }
        // When inventory exposes no init PID both sides, continuity is unprovable;
        // do not invent a claim. Stream + Ready + no invented exit still stand.

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
        let output = manager
            .execute(
                execution_id,
                generation,
                ExecRequest {
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
                },
            )
            .await
            .map_err(|error| {
                failure(format!(
                    "keyed captured exec `{request_id}` failed on Live generation: {error}"
                ))
            })?;
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
        let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
        loop {
            let status = manager.inspect(execution_id).await?;
            match status.state {
                ExecutionState::Running => {
                    if before_kill {
                        report.observed_running_before_host_kill = true;
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

    fn sigkill_host_service(before: &ProcessIdentityReport) -> Result<(), AnyError> {
        // SAFETY: qualification-only SIGKILL of the authenticated Host identity.
        let kill_rc = unsafe { libc::kill(before.pid as i32, libc::SIGKILL) };
        if kill_rc != 0 {
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::NotFound {
                return Err(failure(format!(
                    "failed to SIGKILL Host Service pid {}: {err}",
                    before.pid
                )));
            }
        }
        Ok(())
    }

    /// Wait until Host pid is gone and clear `runtime.sock`. Does not require Guest death.
    fn wait_host_gone_and_clear_endpoint(
        before: &ProcessIdentityReport,
        endpoint: &Path,
    ) -> Result<(), AnyError> {
        let gone_deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < gone_deadline {
            let observed = process_start_time(before.pid)?;
            let host_gone = observed.is_none() || observed != Some(before.start_time_ticks);
            if host_gone {
                if endpoint.exists() {
                    let _ = std::fs::remove_file(endpoint);
                }
                if !endpoint.exists() {
                    return Ok(());
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let observed = process_start_time(before.pid)?;
        require(
            observed.is_none() || observed != Some(before.start_time_ticks),
            "Host Service pid remained alive after SIGKILL",
        )?;
        if endpoint.exists() {
            let _ = std::fs::remove_file(endpoint);
        }
        require(
            !endpoint.exists(),
            "Host Service Unix endpoint remained after SIGKILL cleanup",
        )?;
        Ok(())
    }

    /// Recreate Host Service with `A3S_OCI_KVM_SESSION_OWNER=1` and wait for `runtime.sock`.
    fn spawn_replacement_host_service(
        service: &ServiceRestartInputs,
        endpoint: &Path,
    ) -> Result<u32, AnyError> {
        require(
            std::env::var(SESSION_OWNER_ENV).as_deref() == Ok("1"),
            format!("{SESSION_OWNER_ENV}=1 must remain set for replacement Host spawn"),
        )?;
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
                // Keep the replacement Host Service running after this process exits.
                std::mem::forget(child);
                return Ok(replacement_pid);
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
                // Long-lived init so Host SIGKILL can observe Live survival.
                cmd: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "printf 'a3s-box-kvm-live-session\\n'; while :; do sleep 60; done".to_string(),
                ],
                network: NetworkMode::None,
                persistent: false,
                ..Default::default()
            },
            labels: BTreeMap::from([(
                "purpose".to_string(),
                "linux-kvm-live-session".to_string(),
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

    fn process_identity(pid: u32) -> Result<ProcessIdentityReport, AnyError> {
        let start_time_ticks = process_start_time(pid)?
            .ok_or_else(|| failure(format!("process {pid} is not alive")))?;
        Ok(ProcessIdentityReport {
            pid,
            start_time_ticks,
        })
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
        value.len() == 40
            && value
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
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
