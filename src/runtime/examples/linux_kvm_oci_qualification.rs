//! Destructive product-lifecycle qualification for Box over A3S OCI Runtime/KVM.
//!
//! A host operator owns image import and process-leak checks. This executable
//! exercises the public Box lifecycle boundary, including an optional Host
//! Service SIGKILL/restart while a MicroVM is running. It always emits a
//! versioned JSON report when `A3S_BOX_KVM_OCI_REPORT` names an absolute path.
//!
//! Schema `a3s.box.linux-kvm-oci-qualification.v2` covers:
//! 1. create replay + Box-manager reopen + start + exact exit `23` + delete
//! 2. when service restart inputs are present: a second generation that is
//!    observed running, interrupted by Host Service SIGKILL/restart, and
//!    reconciled to stopped without an invented exit status before delete

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
fn main() {
    eprintln!("linux-kvm-oci-qualification requires Linux x86_64 or aarch64");
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

#[cfg_attr(
    not(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )),
    allow(dead_code)
)]
mod qualification {
    use std::collections::BTreeMap;
    use std::error::Error;
    use std::fs::OpenOptions;
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    use a3s_box_core::{
        BoxConfig, CreateExecutionRequest, ExecutionBackend, ExecutionGeneration, ExecutionId,
        ExecutionIsolation, ExecutionManager, ExecutionManagerError, ExecutionState,
        IsolationClass as BoxIsolationClass, NetworkMode, OperationId, ReconcileOutcome,
        ResourceConfig,
    };
    use a3s_box_runtime::{
        LinuxKvmOciMigrationConfig, LocalExecutionManager, ManagedExecutionStore,
        ManagedRuntimeRoute, OciRuntimeBinding,
    };
    use a3s_oci_sdk::{DriverKind, IsolationClass as OciIsolationClass};
    use serde::Serialize;

    const ENABLE_ENV: &str = "A3S_BOX_KVM_OCI_QUALIFICATION";
    const HOME_ENV: &str = "A3S_HOME";
    const RUNTIME_ROOT_ENV: &str = "A3S_BOX_OCI_HOST_ROOT";
    const ENDPOINT_ENV: &str = "A3S_BOX_OCI_KVM_ENDPOINT";
    const IMAGE_ENV: &str = "A3S_BOX_KVM_OCI_IMAGE";
    const REPORT_ENV: &str = "A3S_BOX_KVM_OCI_REPORT";
    const SERVICE_PID_ENV: &str = "A3S_BOX_KVM_OCI_SERVICE_PID";
    const SERVICE_BIN_ENV: &str = "A3S_BOX_KVM_OCI_SERVICE_BIN";
    const SERVICE_ROOT_ENV: &str = "A3S_BOX_KVM_OCI_SERVICE_ROOT";
    const SERVICE_SHIM_ENV: &str = "A3S_BOX_KVM_OCI_SERVICE_SHIM";
    const SERVICE_MANIFEST_ENV: &str = "A3S_BOX_KVM_OCI_SERVICE_MANIFEST";
    const SERVICE_LOG_ENV: &str = "A3S_BOX_KVM_OCI_SERVICE_LOG";
    const SCHEMA_VERSION: &str = "a3s.box.linux-kvm-oci-qualification.v2";
    const STDOUT_MARKER: &str = "a3s-box-kvm-oci-stdout";
    const STDERR_MARKER: &str = "a3s-box-kvm-oci-stderr";
    const EXPECTED_EXIT_CODE: i32 = 23;

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
        service_restart: Option<ServiceRestartInputs>,
    }

    #[derive(Debug, Serialize)]
    struct QualificationReport {
        schema_version: &'static str,
        status: &'static str,
        started_at_utc: String,
        completed_at_utc: String,
        error: Option<String>,
        cleanup_error: Option<String>,
        home_dir: Option<PathBuf>,
        state_path: Option<PathBuf>,
        runtime_root: Option<PathBuf>,
        endpoint: Option<PathBuf>,
        image: Option<String>,
        operation_id: Option<String>,
        execution_id: Option<String>,
        box_generation: Option<ExecutionGeneration>,
        create_replay_exact: bool,
        manager_restart_reconciled: bool,
        observed_running: bool,
        terminal_state: Option<ExecutionState>,
        exit_code: Option<i32>,
        runtime_binding: Option<OciRuntimeBinding>,
        removed: bool,
        remove_replay_absent: bool,
        reconcile_absent: bool,
        box_directory_absent: bool,
        runtime_shares_absent: bool,
        bundle_handoffs_absent: bool,
        runtime_service_restart_requested: bool,
        runtime_service_restarted: bool,
        restart_operation_id: Option<String>,
        restart_execution_id: Option<String>,
        restart_observed_running: bool,
        restart_reconciled_stopped: bool,
        restart_exit_code_absent: bool,
        restart_removed: bool,
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
                home_dir: None,
                state_path: None,
                runtime_root: None,
                endpoint: None,
                image: None,
                operation_id: None,
                execution_id: None,
                box_generation: None,
                create_replay_exact: false,
                manager_restart_reconciled: false,
                observed_running: false,
                terminal_state: None,
                exit_code: None,
                runtime_binding: None,
                removed: false,
                remove_replay_absent: false,
                reconcile_absent: false,
                box_directory_absent: false,
                runtime_shares_absent: false,
                bundle_handoffs_absent: false,
                runtime_service_restart_requested: false,
                runtime_service_restarted: false,
                restart_operation_id: None,
                restart_execution_id: None,
                restart_observed_running: false,
                restart_reconciled_stopped: false,
                restart_exit_code_absent: false,
                restart_removed: false,
            }
        }
    }

    pub(super) async fn main() {
        let report_path = match absolute_environment_path(REPORT_ENV) {
            Ok(path) => path,
            Err(error) => {
                eprintln!("KVM OCI qualification cannot select its report: {error}");
                std::process::exit(2);
            }
        };
        let mut report = QualificationReport::new();

        let inputs = load_inputs(&mut report);
        let outcome = match inputs {
            Ok(inputs) => {
                let operation_id = OperationId::new(format!(
                    "linux-kvm-oci-qualification-{}",
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
                            if let Some(restart_operation) = report
                                .restart_operation_id
                                .as_deref()
                                .and_then(|value| OperationId::new(value.to_string()).ok())
                            {
                                if let Err(error) = cleanup(&inputs, &restart_operation).await {
                                    let message = error.to_string();
                                    cleanup_error = Some(match cleanup_error {
                                        Some(existing) => format!("{existing}; {message}"),
                                        None => message,
                                    });
                                }
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
                "KVM OCI qualification could not write {}: {error}",
                report_path.display()
            );
            std::process::exit(1);
        }
        if report.status != "passed" {
            eprintln!(
                "KVM OCI qualification failed: {}",
                report.error.as_deref().unwrap_or("unknown failure")
            );
            std::process::exit(1);
        }
        println!("KVM OCI qualification passed: {}", report_path.display());
    }

    fn load_inputs(report: &mut QualificationReport) -> Result<Inputs, AnyError> {
        require(
            std::env::var(ENABLE_ENV).as_deref() == Ok("1"),
            format!("set {ENABLE_ENV}=1 to acknowledge the destructive qualification"),
        )?;
        let home_dir = absolute_environment_path(HOME_ENV)?;
        require(
            home_dir
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("kvm-oci-qualification")),
            "A3S_HOME must name a dedicated kvm-oci-qualification directory",
        )?;
        let runtime_root = absolute_environment_path(RUNTIME_ROOT_ENV)?;
        // The qualification Host Service owns the runtime root; it is not
        // nested under Box home (unlike the WHPX pipe composition).
        let endpoint = absolute_environment_path(ENDPOINT_ENV)?;
        require(
            endpoint
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".sock")),
            format!("{ENDPOINT_ENV} must be an absolute Unix socket path"),
        )?;
        let image = required_environment_string(IMAGE_ENV)?;
        let state_path = home_dir.join("managed-executions.json");
        let service_restart = load_service_restart_inputs()?;
        report.runtime_service_restart_requested = service_restart.is_some();

        report.home_dir = Some(home_dir.clone());
        report.state_path = Some(state_path.clone());
        report.runtime_root = Some(runtime_root.clone());
        report.endpoint = Some(endpoint.clone());
        report.image = Some(image.clone());
        Ok(Inputs {
            home_dir,
            state_path,
            runtime_root,
            endpoint,
            image,
            service_restart,
        })
    }

    fn load_service_restart_inputs() -> Result<Option<ServiceRestartInputs>, AnyError> {
        let pid_raw = match std::env::var(SERVICE_PID_ENV) {
            Ok(value) if !value.trim().is_empty() => value,
            _ => return Ok(None),
        };
        let pid: u32 = pid_raw
            .parse()
            .map_err(|_| failure(format!("{SERVICE_PID_ENV} must be a positive pid")))?;
        require(pid > 1, format!("{SERVICE_PID_ENV} must be a positive pid"))?;
        Ok(Some(ServiceRestartInputs {
            pid,
            service_bin: absolute_environment_path(SERVICE_BIN_ENV)?,
            service_root: absolute_environment_path(SERVICE_ROOT_ENV)?,
            service_shim: absolute_environment_path(SERVICE_SHIM_ENV)?,
            system_image_manifest: absolute_environment_path(SERVICE_MANIFEST_ENV)?,
            service_log: absolute_environment_path(SERVICE_LOG_ENV)?,
        }))
    }

    async fn exercise(
        inputs: &Inputs,
        operation_id: &OperationId,
        report: &mut QualificationReport,
    ) -> Result<(), AnyError> {
        exercise_exact_exit(inputs, operation_id, report).await?;
        if let Some(service) = inputs.service_restart.as_ref() {
            exercise_runtime_service_restart(inputs, service, report).await?;
        }
        Ok(())
    }

    async fn exercise_exact_exit(
        inputs: &Inputs,
        operation_id: &OperationId,
        report: &mut QualificationReport,
    ) -> Result<(), AnyError> {
        let request = qualification_request(&inputs.image, false);
        let manager = connect(inputs).await?;
        let reservation = manager.create(request.clone(), operation_id).await?;
        report.execution_id = Some(reservation.execution_id.to_string());
        report.box_generation = Some(reservation.generation);

        require(
            reservation.plan.backend == ExecutionBackend::Krun
                && reservation.plan.isolation_class == BoxIsolationClass::HardwareVm,
            "the Box request did not retain its dedicated MicroVM product plan",
        )?;
        let replay = manager.create(request, operation_id).await?;
        require(
            replay.execution_id == reservation.execution_id
                && replay.generation == reservation.generation
                && replay.plan == reservation.plan
                && same_resources(&replay.resources, &reservation.resources),
            "idempotent Box create replay changed its reservation",
        )?;
        report.create_replay_exact = true;

        let store = ManagedExecutionStore::new(&inputs.state_path);
        let created = store
            .get(&reservation.execution_id)?
            .ok_or_else(|| failure("created Box record is missing"))?;
        let metadata = created
            .managed_execution
            .as_ref()
            .ok_or_else(|| failure("created Box record has no managed metadata"))?;
        require(
            metadata.runtime_route == ManagedRuntimeRoute::OciSdk,
            "created Box record is not durably routed through the OCI SDK",
        )?;
        require(
            metadata.oci_runtime.is_none(),
            "unstarted Box record unexpectedly contains a runtime generation",
        )?;

        drop(manager);
        let restarted = connect(inputs).await?;
        let recovered = match restarted.reconcile(operation_id).await? {
            ReconcileOutcome::Created(reservation) => reservation,
            _ => {
                return Err(failure(
                    "restarted Box manager did not recover created state",
                ))
            }
        };
        require(
            recovered.execution_id == reservation.execution_id
                && recovered.generation == reservation.generation
                && recovered.plan == reservation.plan
                && same_resources(&recovered.resources, &reservation.resources),
            "restarted Box manager recovered different reservation evidence",
        )?;
        report.manager_restart_reconciled = true;

        let lease = tokio::time::timeout(
            Duration::from_secs(30 * 60),
            restarted.start(&recovered.execution_id, recovered.generation),
        )
        .await
        .map_err(|_| failure("timed out preparing or starting the KVM execution"))??;
        require(
            lease.execution_id == recovered.execution_id
                && lease.generation == recovered.generation
                && lease.plan == recovered.plan
                && same_resources(&lease.resources, &recovered.resources),
            "started Box lease differs from its durable reservation",
        )?;
        let running = store
            .get(&recovered.execution_id)?
            .ok_or_else(|| failure("started Box record is missing"))?;
        let binding = running
            .managed_execution
            .as_ref()
            .and_then(|metadata| metadata.oci_runtime.clone())
            .ok_or_else(|| failure("started Box record has no exact OCI runtime binding"))?;
        require(
            binding.driver == DriverKind::LibkrunKvm
                && binding.isolation == OciIsolationClass::DedicatedVm,
            "runtime did not return the dedicated-VM libkrun/KVM binding",
        )?;
        report.runtime_binding = Some(binding);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
        loop {
            let status = restarted.inspect(&recovered.execution_id).await?;
            match status.state {
                ExecutionState::Running => report.observed_running = true,
                ExecutionState::Stopped => {
                    report.terminal_state = Some(status.state);
                    break;
                }
                ExecutionState::Failed => {
                    return Err(failure("KVM execution entered failed state"));
                }
                ExecutionState::Created | ExecutionState::Creating => {}
                ExecutionState::Paused => {
                    return Err(failure("KVM execution unexpectedly entered paused state"));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(failure("timed out waiting for the KVM execution to stop"));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        require(
            report.observed_running,
            "Box never observed the KVM execution in running state",
        )?;

        let stopped = store
            .get(&recovered.execution_id)?
            .ok_or_else(|| failure("terminal Box record is missing"))?;
        report.exit_code = stopped.exit_code;
        require(
            stopped.exit_code == Some(EXPECTED_EXIT_CODE),
            format!(
                "expected exact exit code {EXPECTED_EXIT_CODE}, found {:?}",
                stopped.exit_code
            ),
        )?;
        let stopped_metadata = stopped
            .managed_execution
            .as_ref()
            .ok_or_else(|| failure("terminal Box record lost managed metadata"))?;
        require(
            stopped_metadata.oci_runtime.is_none() && stopped_metadata.finished_at.is_some(),
            "terminal Box record retained live runtime state or lost its completion time",
        )?;

        report.removed = restarted
            .remove(&recovered.execution_id, recovered.generation)
            .await?;
        require(report.removed, "terminal Box generation was not removed")?;
        report.remove_replay_absent = !restarted
            .remove(&recovered.execution_id, recovered.generation)
            .await?;
        require(
            report.remove_replay_absent,
            "remove replay did not report the generation as absent",
        )?;
        report.reconcile_absent = matches!(
            restarted.reconcile(operation_id).await?,
            ReconcileOutcome::Absent
        );
        require(
            report.reconcile_absent,
            "removed Box operation remained reconcilable",
        )?;

        report.box_directory_absent = !inputs
            .home_dir
            .join("boxes")
            .join(recovered.execution_id.as_str())
            .exists();
        report.runtime_shares_absent =
            directory_absent_or_empty(&inputs.runtime_root.join("shares"))?;
        report.bundle_handoffs_absent =
            directory_absent_or_empty(&inputs.runtime_root.join("bundle-handoffs"))?;
        require(
            report.box_directory_absent
                && report.runtime_shares_absent
                && report.bundle_handoffs_absent,
            "Box or OCI runtime-owned lifecycle paths remained after deletion",
        )?;
        Ok(())
    }

    async fn exercise_runtime_service_restart(
        inputs: &Inputs,
        service: &ServiceRestartInputs,
        report: &mut QualificationReport,
    ) -> Result<(), AnyError> {
        let operation_id = OperationId::new(format!(
            "linux-kvm-oci-runtime-restart-{}",
            uuid::Uuid::new_v4()
        ))?;
        report.restart_operation_id = Some(operation_id.to_string());

        let request = qualification_request(&inputs.image, true);
        let manager = connect(inputs).await?;
        let reservation = manager.create(request, &operation_id).await?;
        report.restart_execution_id = Some(reservation.execution_id.to_string());
        let lease = tokio::time::timeout(
            Duration::from_secs(30 * 60),
            manager.start(&reservation.execution_id, reservation.generation),
        )
        .await
        .map_err(|_| failure("timed out starting the runtime-restart KVM execution"))??;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            let status = manager.inspect(&lease.execution_id).await?;
            if matches!(status.state, ExecutionState::Running) {
                report.restart_observed_running = true;
                break;
            }
            if matches!(
                status.state,
                ExecutionState::Failed | ExecutionState::Stopped
            ) {
                return Err(failure(
                    "runtime-restart generation left running before Host Service SIGKILL",
                ));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(failure(
                    "timed out waiting for runtime-restart generation to run",
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        drop(manager);
        restart_host_service(service, &inputs.endpoint)?;
        report.runtime_service_restarted = true;

        let reconnected = connect(inputs).await?;
        // Host Service reopen may briefly report Unavailable while the
        // replacement owner recovers durable state; retry only that class.
        let mut outcome = None;
        let reconcile_deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        while outcome.is_none() {
            match reconnected.reconcile(&operation_id).await {
                Ok(value) => outcome = Some(value),
                Err(ExecutionManagerError::Unavailable(_)) => {
                    if tokio::time::Instant::now() >= reconcile_deadline {
                        return Err(failure(
                            "Box remained Unavailable for 120s after Host Service restart",
                        ));
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(error) => {
                    return Err(failure(format!(
                        "Box reconcile failed after Host Service restart: {error}"
                    )));
                }
            }
        }
        let outcome = outcome.expect("reconcile outcome assigned before loop exit");
        // Ready and Created carry different payload types; match them apart.
        // Failed is the durable terminal outcome for stopped-only owner loss.
        match outcome {
            ReconcileOutcome::Ready(recovered) => {
                require(
                    recovered.execution_id == reservation.execution_id
                        && recovered.generation == reservation.generation,
                    "Host Service restart recovered a different Ready Box generation",
                )?;
            }
            ReconcileOutcome::Created(recovered) => {
                require(
                    recovered.execution_id == reservation.execution_id
                        && recovered.generation == reservation.generation,
                    "Host Service restart recovered a different Created Box generation",
                )?;
            }
            ReconcileOutcome::Failed => {}
            ReconcileOutcome::Creating => {
                return Err(failure(
                    "Host Service restart left the Box generation stuck creating",
                ));
            }
            ReconcileOutcome::Absent => {
                return Err(failure(
                    "Host Service restart lost the Box operation before stopped reconciliation",
                ));
            }
        }

        let status = reconnected.inspect(&reservation.execution_id).await?;
        require(
            matches!(status.state, ExecutionState::Stopped),
            format!(
                "expected stopped-only reconciliation after Host Service restart, found {:?}",
                status.state
            ),
        )?;
        report.restart_reconciled_stopped = true;

        let store = ManagedExecutionStore::new(&inputs.state_path);
        let stopped = store
            .get(&reservation.execution_id)?
            .ok_or_else(|| failure("restart generation record is missing after reconcile"))?;
        require(
            stopped.exit_code.is_none(),
            format!(
                "Host Service restart invented exit status {:?}",
                stopped.exit_code
            ),
        )?;
        report.restart_exit_code_absent = true;

        report.restart_removed = reconnected
            .remove(&reservation.execution_id, reservation.generation)
            .await?;
        require(
            report.restart_removed,
            "stopped restart generation was not removed",
        )?;
        require(
            matches!(
                reconnected.reconcile(&operation_id).await?,
                ReconcileOutcome::Absent
            ),
            "removed restart operation remained reconcilable",
        )?;
        require(
            !inputs
                .home_dir
                .join("boxes")
                .join(reservation.execution_id.as_str())
                .exists(),
            "restart Box directory remained after deletion",
        )?;
        require(
            directory_absent_or_empty(&inputs.runtime_root.join("shares"))?,
            "runtime shares remained after restart-generation deletion",
        )?;
        require(
            directory_absent_or_empty(&inputs.runtime_root.join("bundle-handoffs"))?,
            "bundle handoffs remained after restart-generation deletion",
        )?;
        Ok(())
    }

    fn restart_host_service(
        service: &ServiceRestartInputs,
        endpoint: &Path,
    ) -> Result<(), AnyError> {
        // SAFETY: qualification-only SIGKILL of the exact operator-supplied pid.
        let kill_rc = unsafe { libc::kill(service.pid as i32, libc::SIGKILL) };
        if kill_rc != 0 {
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::NotFound {
                return Err(failure(format!(
                    "failed to SIGKILL Host Service pid {}: {err}",
                    service.pid
                )));
            }
        }

        let gone_deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < gone_deadline {
            // SAFETY: existence probe for the exact pid we killed.
            let still_alive = unsafe { libc::kill(service.pid as i32, 0) } == 0;
            if !still_alive && !endpoint.exists() {
                break;
            }
            if endpoint.exists() {
                let _ = std::fs::remove_file(endpoint);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        require(
            unsafe { libc::kill(service.pid as i32, 0) } != 0,
            "Host Service pid remained alive after SIGKILL",
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

    async fn connect(inputs: &Inputs) -> Result<LocalExecutionManager, AnyError> {
        let config =
            LinuxKvmOciMigrationConfig::new(inputs.runtime_root.clone(), inputs.endpoint.clone())?;
        Ok(LocalExecutionManager::with_linux_kvm_oci_qualification(
            &inputs.state_path,
            &inputs.home_dir,
            config,
        )
        .await?)
    }

    fn qualification_request(image: &str, long_running: bool) -> CreateExecutionRequest {
        let cmd = if long_running {
            format!(
                "printf '{STDOUT_MARKER}\\n'; printf '{STDERR_MARKER}\\n' >&2; sleep 120; exit {EXPECTED_EXIT_CODE}"
            )
        } else {
            format!(
                "printf '{STDOUT_MARKER}\\n'; printf '{STDERR_MARKER}\\n' >&2; sleep 10; exit {EXPECTED_EXIT_CODE}"
            )
        };
        CreateExecutionRequest {
            external_sandbox_id: if long_running {
                "linux-kvm-oci-runtime-restart".to_string()
            } else {
                "linux-kvm-oci-qualification".to_string()
            },
            config: BoxConfig {
                isolation: ExecutionIsolation::Microvm,
                image: image.to_string(),
                resources: ResourceConfig {
                    vcpus: 1,
                    memory_mb: 512,
                    ..Default::default()
                },
                cmd: vec!["/bin/sh".to_string(), "-c".to_string(), cmd],
                network: NetworkMode::None,
                persistent: false,
                ..Default::default()
            },
            labels: BTreeMap::from([(
                "purpose".to_string(),
                if long_running {
                    "linux-kvm-oci-runtime-restart".to_string()
                } else {
                    "linux-kvm-oci-qualification".to_string()
                },
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

    fn same_resources(left: &ResourceConfig, right: &ResourceConfig) -> bool {
        left.vcpus == right.vcpus
            && left.memory_mb == right.memory_mb
            && left.disk_mb == right.disk_mb
            && left.timeout == right.timeout
    }

    fn directory_absent_or_empty(path: &Path) -> Result<bool, AnyError> {
        if !path.exists() {
            return Ok(true);
        }
        require(
            path.is_dir(),
            format!("{} is not a directory", path.display()),
        )?;
        Ok(std::fs::read_dir(path)?.next().is_none())
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
