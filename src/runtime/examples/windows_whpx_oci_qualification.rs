//! Destructive product-lifecycle qualification for Box over A3S OCI Runtime/WHPX.
//!
//! The PowerShell qualification runner owns the isolated home, runtime service,
//! image import, artifact verification, and process-leak checks. This executable
//! exercises only the public Box lifecycle boundary and always emits a versioned
//! JSON report when `A3S_BOX_WHPX_OCI_REPORT` names an absolute output path.

#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
fn main() {
    eprintln!("windows-whpx-oci-qualification requires Windows x86_64");
    std::process::exit(2);
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[tokio::main]
async fn main() {
    qualification::main().await;
}

#[cfg_attr(
    not(all(target_os = "windows", target_arch = "x86_64")),
    allow(dead_code)
)]
mod qualification {
    use std::collections::BTreeMap;
    use std::error::Error;
    use std::fs::OpenOptions;
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use a3s_box_core::{
        BoxConfig, CreateExecutionRequest, ExecutionBackend, ExecutionGeneration, ExecutionId,
        ExecutionIsolation, ExecutionManager, ExecutionState, IsolationClass as BoxIsolationClass,
        NetworkMode, OperationId, ReconcileOutcome, ResourceConfig,
    };
    use a3s_box_runtime::{
        LocalExecutionManager, ManagedExecutionStore, ManagedRuntimeRoute, OciRuntimeBinding,
        WindowsWhpxOciMigrationConfig,
    };
    use a3s_oci_sdk::{DriverKind, IsolationClass as OciIsolationClass};
    use serde::Serialize;

    const ENABLE_ENV: &str = "A3S_BOX_WHPX_OCI_QUALIFICATION";
    const HOME_ENV: &str = "A3S_HOME";
    const RUNTIME_ROOT_ENV: &str = "A3S_BOX_OCI_HOST_ROOT";
    const ENDPOINT_ENV: &str = "A3S_BOX_OCI_WHPX_ENDPOINT";
    const IMAGE_ENV: &str = "A3S_BOX_WHPX_OCI_IMAGE";
    const REPORT_ENV: &str = "A3S_BOX_WHPX_OCI_REPORT";
    const SCHEMA_VERSION: &str = "a3s.box.windows-whpx-oci-qualification.v1";
    const STDOUT_MARKER: &str = "a3s-box-whpx-oci-stdout";
    const STDERR_MARKER: &str = "a3s-box-whpx-oci-stderr";
    const EXPECTED_EXIT_CODE: i32 = 23;

    type AnyError = Box<dyn Error + Send + Sync>;

    #[derive(Debug, Clone)]
    struct Inputs {
        home_dir: PathBuf,
        state_path: PathBuf,
        runtime_root: PathBuf,
        endpoint: String,
        image: String,
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
        endpoint: Option<String>,
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
            }
        }
    }

    pub(super) async fn main() {
        let report_path = match absolute_environment_path(REPORT_ENV) {
            Ok(path) => path,
            Err(error) => {
                eprintln!("WHPX OCI qualification cannot select its report: {error}");
                std::process::exit(2);
            }
        };
        let mut report = QualificationReport::new();

        let inputs = load_inputs(&mut report);
        let outcome = match inputs {
            Ok(inputs) => {
                let operation_id = OperationId::new(format!(
                    "windows-whpx-oci-qualification-{}",
                    uuid::Uuid::new_v4()
                ));
                match operation_id {
                    Ok(operation_id) => {
                        report.operation_id = Some(operation_id.to_string());
                        let outcome = exercise(&inputs, &operation_id, &mut report).await;
                        if outcome.is_err() {
                            if let Err(error) = cleanup(&inputs, &operation_id).await {
                                report.cleanup_error = Some(error.to_string());
                            }
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
                "WHPX OCI qualification could not write {}: {error}",
                report_path.display()
            );
            std::process::exit(1);
        }
        if report.status != "passed" {
            eprintln!(
                "WHPX OCI qualification failed: {}",
                report.error.as_deref().unwrap_or("unknown failure")
            );
            std::process::exit(1);
        }
        println!("WHPX OCI qualification passed: {}", report_path.display());
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
                .is_some_and(|name| name.contains("whpx-oci-qualification")),
            "A3S_HOME must name a dedicated whpx-oci-qualification directory",
        )?;
        let runtime_root = absolute_environment_path(RUNTIME_ROOT_ENV)?;
        require(
            runtime_root.starts_with(&home_dir),
            "A3S_BOX_OCI_HOST_ROOT must be inside the dedicated A3S_HOME",
        )?;
        let endpoint = required_environment_string(ENDPOINT_ENV)?;
        let image = required_environment_string(IMAGE_ENV)?;
        let state_path = home_dir.join("managed-executions.json");

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
        })
    }

    async fn exercise(
        inputs: &Inputs,
        operation_id: &OperationId,
        report: &mut QualificationReport,
    ) -> Result<(), AnyError> {
        let request = qualification_request(&inputs.image);
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
        .map_err(|_| failure("timed out preparing or starting the WHPX execution"))??;
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
            binding.driver == DriverKind::LibkrunWhpx
                && binding.isolation == OciIsolationClass::DedicatedVm,
            "runtime did not return the dedicated-VM libkrun/WHPX binding",
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
                    return Err(failure("WHPX execution entered failed state"));
                }
                ExecutionState::Created | ExecutionState::Creating => {}
                ExecutionState::Paused => {
                    return Err(failure("WHPX execution unexpectedly entered paused state"));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(failure("timed out waiting for the WHPX execution to stop"));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        require(
            report.observed_running,
            "Box never observed the WHPX execution in running state",
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

    async fn connect(inputs: &Inputs) -> Result<LocalExecutionManager, AnyError> {
        let config = WindowsWhpxOciMigrationConfig::new(
            inputs.runtime_root.clone(),
            inputs.endpoint.clone(),
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
            external_sandbox_id: "windows-whpx-oci-qualification".to_string(),
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
                    format!(
                        "printf '{STDOUT_MARKER}\\n'; printf '{STDERR_MARKER}\\n' >&2; sleep 10; exit {EXPECTED_EXIT_CODE}"
                    ),
                ],
                network: NetworkMode::None,
                persistent: false,
                ..Default::default()
            },
            labels: BTreeMap::from([(
                "purpose".to_string(),
                "windows-whpx-oci-qualification".to_string(),
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
