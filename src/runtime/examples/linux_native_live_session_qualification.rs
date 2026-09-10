//! Destructive live-session qualification for Box over Native Linux OCI.
//!
//! Exercises the public Box Sandbox path with
//! `A3S_OCI_NATIVE_SESSION_SUPERVISOR=1`, SIGKILLs the out-of-process Native
//! Linux Host owner while a generation is running, rebinds through a
//! replacement owner, and continues authentic Live process-session ops
//! (keyed captured exec, state, inventory, stats, kill) without inventing an
//! exit status.
//!
//! Schema `a3s.box.linux-native-live-session.v2`.
//!
//! Honest scope (anti-overfit):
//! - Live Host-reopen is Native-Linux-driver-only today.
//! - This harness **drops** the Box manager before owner SIGKILL, so it does
//!   **not** prove retained streaming process-handle continuity (that remains
//!   the fixture-only contract in `oci_backend_tests::process_restart`).
//! - It does **not** claim KVM MicroVM Live continuity (KVM Host reopen remains
//!   stopped-only / recreate) and does **not** close Box B2 / OCI R6.

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
fn main() {
    eprintln!("linux-native-live-session-qualification requires Linux x86_64 or aarch64");
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
    use std::time::Duration;

    use a3s_box_core::{
        BoxConfig, CreateExecutionRequest, ExecRequest, ExecutionBackend, ExecutionGeneration,
        ExecutionId, ExecutionIsolation, ExecutionManager, ExecutionManagerError,
        ExecutionSessionManager, ExecutionState, IsolationClass as BoxIsolationClass, NetworkMode,
        OperationId, ReconcileOutcome, ResourceConfig,
    };
    use a3s_box_runtime::{
        LocalExecutionManager, ManagedExecutionStore, ManagedRuntimeRoute,
        NativeLinuxOciMigrationConfig, OciRuntimeBinding,
    };
    use a3s_oci_sdk::{DriverKind, IsolationClass as OciIsolationClass};
    use serde::Serialize;
    use serde_json::Value;

    const ENABLE_ENV: &str = "A3S_BOX_NATIVE_LIVE_SESSION_QUALIFICATION";
    const HOME_ENV: &str = "A3S_HOME";
    const HOST_ROOT_ENV: &str = "A3S_BOX_OCI_HOST_ROOT";
    const RUNTIME_PATH_ENV: &str = "A3S_BOX_OCI_RUNTIME_PATH";
    const AGENT_PATH_ENV: &str = "A3S_BOX_OCI_AGENT_PATH";
    const IMAGE_ENV: &str = "A3S_BOX_NATIVE_LIVE_SESSION_IMAGE";
    const REPORT_ENV: &str = "A3S_BOX_NATIVE_LIVE_SESSION_REPORT";
    const BOX_SHA_ENV: &str = "A3S_BOX_NATIVE_LIVE_SESSION_BOX_SHA";
    const OCI_SHA_ENV: &str = "A3S_BOX_NATIVE_LIVE_SESSION_OCI_SHA";
    const SUPERVISOR_ENV: &str = "A3S_OCI_NATIVE_SESSION_SUPERVISOR";
    const SCHEMA_VERSION: &str = "a3s.box.linux-native-live-session.v2";
    const OWNER_SCHEMA: &str = "a3s.box.native-linux-oci-owner.v1";
    const KEYED_EXEC_BEFORE: &str = "a3s.box.live-session.keyed-exec.before-owner-kill";
    const KEYED_EXEC_AFTER: &str = "a3s.box.live-session.keyed-exec.after-reopen";
    const KEYED_EXEC_MARKER: &[u8] = b"live-session-keyed-ok\n";

    type AnyError = Box<dyn Error + Send + Sync>;

    #[derive(Debug, Clone)]
    struct Inputs {
        home_dir: PathBuf,
        state_path: PathBuf,
        host_root: PathBuf,
        runtime_path: PathBuf,
        agent_path: PathBuf,
        image: String,
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
        host_root: Option<PathBuf>,
        image: Option<String>,
        driver_target: &'static str,
        kvm_microvm_live_claimed: bool,
        utility_vm_claimed: bool,
        /// Always false: this harness drops the Box manager before owner death.
        retained_stream_handle_proven: bool,
        /// Always false: fixture `process_restart` continuity is not this gate.
        fixture_stream_continuity_claimed: bool,
        /// Always false: B2 still requires real-driver retained-stream + utility-VM.
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
                driver_target: "native-linux-shared-host-kernel",
                kvm_microvm_live_claimed: false,
                utility_vm_claimed: false,
                retained_stream_handle_proven: false,
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
                eprintln!("native live-session qualification cannot select its report: {error}");
                std::process::exit(2);
            }
        };
        let mut report = QualificationReport::new();

        let inputs = load_inputs(&mut report);
        let outcome = match inputs {
            Ok(inputs) => {
                let operation_id = OperationId::new(format!(
                    "linux-native-live-session-{}",
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
                "native live-session qualification could not write {}: {error}",
                report_path.display()
            );
            std::process::exit(1);
        }
        if report.status != "passed" {
            eprintln!(
                "native live-session qualification failed: {}",
                report.error.as_deref().unwrap_or("unknown failure")
            );
            std::process::exit(1);
        }
        println!(
            "native live-session qualification passed: {}",
            report_path.display()
        );
    }

    fn load_inputs(report: &mut QualificationReport) -> Result<Inputs, AnyError> {
        require(
            std::env::var(ENABLE_ENV).as_deref() == Ok("1"),
            format!("set {ENABLE_ENV}=1 to acknowledge the destructive qualification"),
        )?;
        require(
            std::env::var(SUPERVISOR_ENV).as_deref() == Ok("1"),
            format!(
                "set {SUPERVISOR_ENV}=1; Live supervised create is required (fail-closed without it)"
            ),
        )?;
        report.supervised_create_enabled = true;

        let home_dir = absolute_environment_path(HOME_ENV)?;
        require(
            home_dir
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("native-live-session")),
            "A3S_HOME must name a dedicated native-live-session directory",
        )?;
        let host_root = absolute_environment_path(HOST_ROOT_ENV)?;
        let runtime_path = absolute_environment_path(RUNTIME_PATH_ENV)?;
        let agent_path = absolute_environment_path(AGENT_PATH_ENV)?;
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

        report.home_dir = Some(home_dir.clone());
        report.state_path = Some(state_path.clone());
        report.host_root = Some(host_root.clone());
        report.image = Some(image.clone());
        report.box_commit_sha = Some(box_sha.clone());
        report.oci_runtime_commit_sha = Some(oci_sha.clone());

        let _ = (box_sha, oci_sha);
        Ok(Inputs {
            home_dir,
            state_path,
            host_root,
            runtime_path,
            agent_path,
            image,
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
            reservation.plan.backend == ExecutionBackend::A3sOci
                && reservation.plan.isolation_class == BoxIsolationClass::SharedKernel,
            "the Box request did not retain its Native Linux Sandbox product plan",
        )?;

        let lease = tokio::time::timeout(
            Duration::from_secs(30 * 60),
            manager.start(&reservation.execution_id, reservation.generation),
        )
        .await
        .map_err(|_| failure("timed out preparing or starting the Native Linux execution"))??;
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
            binding.driver == DriverKind::NativeLinux
                && binding.isolation == OciIsolationClass::SharedHostKernel,
            "runtime did not return the Native Linux shared-host-kernel binding",
        )?;
        report.runtime_binding = Some(binding.clone());

        wait_until_running(&manager, &reservation.execution_id, report, true).await?;

        let inventory = manager
            .list_processes(&reservation.execution_id, reservation.generation)
            .await?;
        require(
            !inventory.processes.is_empty(),
            "process inventory was empty before owner SIGKILL",
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

        let owner = load_owner_record(&inputs.host_root)?;
        report.owner_before_kill = Some(owner.clone());
        let recovery = load_live_recovery(&inputs.host_root, &owner, &binding)?;
        report.recovery_schema_version = Some(recovery.schema_version.clone());
        let supervisor = recovery.session_supervisor.clone().ok_or_else(|| {
            failure("recovery record lacks sessionSupervisor; Live path unavailable (fail-closed)")
        })?;
        report.session_supervisor_recorded = true;
        report.supervisor_before_kill = Some(supervisor.clone());
        report.launcher_before_kill = Some(recovery.launcher.clone());
        report.init_before_kill = Some(recovery.init.clone());
        require_live_identity("session supervisor", &supervisor)?;
        require_live_identity("launcher", &recovery.launcher)?;
        require_live_identity("init", &recovery.init)?;

        drop(manager);
        sigkill_identity(&owner)?;
        report.owner_sigkilled = true;
        wait_identity_gone("OCI owner", &owner, Duration::from_secs(30))?;
        report.owner_gone = true;

        // Fail-closed: without Live supervisor parentage these die via PDEATHSIG.
        require_live_identity("session supervisor after owner SIGKILL", &supervisor)?;
        report.supervisor_survived_owner_kill = true;
        require_live_identity("launcher after owner SIGKILL", &recovery.launcher)?;
        report.launcher_survived_owner_kill = true;
        require_live_identity("init after owner SIGKILL", &recovery.init)?;
        report.init_survived_owner_kill = true;

        let reconnected = connect(inputs).await?;
        let mut outcome = None;
        let reconcile_deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        while outcome.is_none() {
            match reconnected.reconcile(operation_id).await {
                Ok(value) => outcome = Some(value),
                Err(ExecutionManagerError::Unavailable(_)) => {
                    if tokio::time::Instant::now() >= reconcile_deadline {
                        return Err(failure(
                            "Box remained Unavailable for 120s after Native Linux owner SIGKILL",
                        ));
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(error) => {
                    return Err(failure(format!(
                        "Box reconcile failed after Native Linux owner SIGKILL: {error}"
                    )));
                }
            }
        }
        match outcome.expect("reconcile outcome assigned before loop exit") {
            ReconcileOutcome::Ready(recovered) => {
                require(
                    recovered.execution_id == reservation.execution_id
                        && recovered.generation == reservation.generation,
                    "owner reopen recovered a different Ready Box generation",
                )?;
                report.reconciled_ready_after_reopen = true;
            }
            ReconcileOutcome::Failed => {
                return Err(failure(
                    "owner reopen reconciled Failed/stopped instead of Live Ready (Live path unavailable)",
                ));
            }
            ReconcileOutcome::Created(_) => {
                return Err(failure(
                    "owner reopen left the generation Created instead of Live Ready",
                ));
            }
            ReconcileOutcome::Creating => {
                return Err(failure(
                    "owner reopen left the Box generation stuck creating",
                ));
            }
            ReconcileOutcome::Absent => {
                return Err(failure(
                    "owner reopen lost the Box operation before Live reconciliation",
                ));
            }
        }

        let replacement_owner = load_owner_record(&inputs.host_root)?;
        require(
            replacement_owner.pid != owner.pid
                || replacement_owner.start_time_ticks != owner.start_time_ticks,
            "replacement Native Linux owner reused the killed identity",
        )?;
        report.owner_rebound = true;
        report.owner_after_reopen = Some(replacement_owner);

        wait_until_running(&reconnected, &reservation.execution_id, report, false).await?;
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

        let inventory_after = reconnected
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
            .any(|process| process.process_id == "init" && process.pid == Some(recovery.init.pid));
        require(
            init_continuous,
            format!(
                "Live reopen inventory lost continuous init PID {}",
                recovery.init.pid
            ),
        )?;
        report.init_pid_continuous_after_reopen = true;

        let stats = reconnected
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
            &reconnected,
            &reservation.execution_id,
            reservation.generation,
            KEYED_EXEC_AFTER,
        )
        .await?;
        report.keyed_captured_exec_after_reopen = true;

        // Authentic Live kill of the still-running generation (not an invented stop).
        reconnected
            .kill(&reservation.execution_id, reservation.generation)
            .await?;
        report.live_kill_after_reopen = true;

        let stop_deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            let status = reconnected.inspect(&reservation.execution_id).await?;
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

        report.removed = reconnected
            .remove(&reservation.execution_id, reservation.generation)
            .await?;
        require(report.removed, "terminal Box generation was not removed")?;
        require(
            matches!(
                reconnected.reconcile(operation_id).await?,
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
                        "expected running Native Linux generation, found {:?}",
                        status.state
                    )));
                }
                ExecutionState::Created | ExecutionState::Creating | ExecutionState::Paused => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(failure(
                    "timed out waiting for the Native Linux execution to run",
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn connect(inputs: &Inputs) -> Result<LocalExecutionManager, AnyError> {
        // Re-assert opt-in so replacement owners inherit supervised create policy.
        require(
            std::env::var(SUPERVISOR_ENV).as_deref() == Ok("1"),
            format!("{SUPERVISOR_ENV}=1 must remain set for Live reconnect"),
        )?;
        let config = NativeLinuxOciMigrationConfig::new(inputs.host_root.clone())?
            .with_artifacts(inputs.runtime_path.clone(), inputs.agent_path.clone())?;
        Ok(LocalExecutionManager::with_native_linux_oci_migration(
            &inputs.state_path,
            &inputs.home_dir,
            config,
        )
        .await?)
    }

    fn qualification_request(image: &str) -> CreateExecutionRequest {
        CreateExecutionRequest {
            external_sandbox_id: "linux-native-live-session".to_string(),
            config: BoxConfig {
                isolation: ExecutionIsolation::Sandbox,
                image: image.to_string(),
                resources: ResourceConfig {
                    vcpus: 1,
                    memory_mb: 512,
                    ..Default::default()
                },
                // Long-lived init so owner SIGKILL can observe Live survival.
                cmd: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "printf 'a3s-box-native-live-session\\n'; while :; do sleep 60; done"
                        .to_string(),
                ],
                network: NetworkMode::None,
                persistent: false,
                ..Default::default()
            },
            labels: BTreeMap::from([(
                "purpose".to_string(),
                "linux-native-live-session".to_string(),
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

    fn load_owner_record(host_root: &Path) -> Result<ProcessIdentityReport, AnyError> {
        let path = host_root.join("box-owner.json");
        let value: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        require(
            value.get("schema").and_then(Value::as_str) == Some(OWNER_SCHEMA),
            format!("unexpected owner schema in {}", path.display()),
        )?;
        let pid = value
            .get("pid")
            .and_then(Value::as_u64)
            .ok_or_else(|| failure("owner record lacks pid"))? as u32;
        let start_time_ticks = value
            .get("pid_start_time")
            .and_then(Value::as_u64)
            .ok_or_else(|| failure("owner record lacks pid_start_time"))?;
        let identity = ProcessIdentityReport {
            pid,
            start_time_ticks,
        };
        require_live_identity("OCI owner", &identity)?;
        Ok(identity)
    }

    struct RecoverySnapshot {
        schema_version: String,
        session_supervisor: Option<ProcessIdentityReport>,
        launcher: ProcessIdentityReport,
        init: ProcessIdentityReport,
    }

    fn load_live_recovery(
        host_root: &Path,
        owner: &ProcessIdentityReport,
        binding: &OciRuntimeBinding,
    ) -> Result<RecoverySnapshot, AnyError> {
        let executor_root = host_root.join("executor").join(format!(
            "a3s-oci-agent-{}-{:016x}",
            owner.pid, owner.start_time_ticks
        ));
        let mut candidates = Vec::new();
        for entry in std::fs::read_dir(&executor_root)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("c-") {
                continue;
            }
            let recovery_path = entry.path().join("recovery.json");
            if recovery_path.is_file() {
                candidates.push(recovery_path);
            }
        }
        require(
            candidates.len() == 1,
            format!(
                "expected one live recovery record under {}, found {}",
                executor_root.display(),
                candidates.len()
            ),
        )?;
        let recovery: Value = serde_json::from_slice(&std::fs::read(&candidates[0])?)?;
        let schema_version = recovery
            .get("schemaVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| failure("recovery record lacks schemaVersion"))?
            .to_string();
        require(
            schema_version.starts_with("a3s.oci.native-linux-recovery.v"),
            format!("unexpected recovery schema {schema_version}"),
        )?;
        let target_id = recovery
            .pointer("/target/id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        require(
            target_id == binding.target.id.as_str(),
            format!(
                "recovery target {target_id} does not match Box binding {}",
                binding.target.id
            ),
        )?;
        Ok(RecoverySnapshot {
            schema_version,
            session_supervisor: identity_from_recovery(&recovery, "sessionSupervisor")?,
            launcher: identity_from_recovery(&recovery, "launcher")?
                .ok_or_else(|| failure("recovery record lacks launcher identity"))?,
            init: identity_from_recovery(&recovery, "init")?
                .ok_or_else(|| failure("recovery record lacks init identity"))?,
        })
    }

    fn identity_from_recovery(
        recovery: &Value,
        field: &str,
    ) -> Result<Option<ProcessIdentityReport>, AnyError> {
        let Some(value) = recovery.get(field) else {
            return Ok(None);
        };
        if value.is_null() {
            return Ok(None);
        }
        let pid = value
            .get("pid")
            .and_then(Value::as_u64)
            .ok_or_else(|| failure(format!("recovery.{field} lacks pid")))?
            as u32;
        let start_time_ticks = value
            .get("startTimeTicks")
            .and_then(Value::as_u64)
            .ok_or_else(|| failure(format!("recovery.{field} lacks startTimeTicks")))?;
        Ok(Some(ProcessIdentityReport {
            pid,
            start_time_ticks,
        }))
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

    fn wait_identity_gone(
        label: &str,
        identity: &ProcessIdentityReport,
        timeout: Duration,
    ) -> Result<(), AnyError> {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            let observed = process_start_time(identity.pid)?;
            if observed.is_none() || observed != Some(identity.start_time_ticks) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Err(failure(format!(
            "{label} remained alive after expected termination"
        )))
    }

    fn sigkill_identity(identity: &ProcessIdentityReport) -> Result<(), AnyError> {
        // SAFETY: qualification-only SIGKILL of the authenticated owner identity.
        let kill_rc = unsafe { libc::kill(identity.pid as i32, libc::SIGKILL) };
        if kill_rc != 0 {
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::NotFound {
                return Err(failure(format!(
                    "failed to SIGKILL OCI owner pid {}: {err}",
                    identity.pid
                )));
            }
        }
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
        message.into().into()
    }
}
