//! Destructive lifecycle smoke test for a dedicated A3S OS Sandbox test home.
//!
//! The caller must set `A3S_BOX_MANAGED_SMOKE=1`, point `A3S_HOME` at a
//! dedicated directory whose name contains `managed-smoke`, and set
//! `A3S_BOX_CRUN_PATH` to that directory's certified `bin/crun` artifact.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("managed-sandbox-smoke requires Linux");
    std::process::exit(2);
}

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() {
    if let Err(error) = linux::run().await {
        eprintln!("managed Sandbox smoke test failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::BTreeMap;
    use std::error::Error;
    use std::io;
    use std::path::{Path, PathBuf};

    use a3s_box_core::{
        BoxConfig, CreateExecutionRequest, ExecutionBackend, ExecutionId, ExecutionIsolation,
        ExecutionManager, ExecutionManagerError, ExecutionSnapshotId, ExecutionState,
        IsolationClass, KillOutcome, NetworkMode, OperationId, ReconcileOutcome, ResourceConfig,
    };
    use a3s_box_runtime::{LocalExecutionManager, ManagedExecutionStore};

    type AnyError = Box<dyn Error + Send + Sync>;

    pub(super) async fn run() -> Result<(), AnyError> {
        // Production services commonly use UMask=0077. Keep the real crun
        // smoke from accidentally relying on world-searchable runtime paths.
        let _umask = RestrictiveUmask::install();
        let home_dir = validated_home()?;
        let state_path = home_dir.join("managed-executions.json");
        let source_operation_id =
            OperationId::new(format!("managed-sandbox-smoke-{}", uuid::Uuid::new_v4()))?;
        let restored_operation_id = OperationId::new(format!(
            "managed-sandbox-restore-{}",
            uuid::Uuid::new_v4()
        ))?;
        let snapshot_id = ExecutionSnapshotId::new(format!(
            "managed-smoke-{}",
            uuid::Uuid::new_v4().simple()
        ))?;

        let result = exercise(
            &home_dir,
            &state_path,
            &source_operation_id,
            &restored_operation_id,
            &snapshot_id,
        )
        .await;
        for operation_id in [&source_operation_id, &restored_operation_id] {
            if let Err(cleanup_error) = cleanup(&home_dir, &state_path, operation_id).await {
                if result.is_ok() {
                    return Err(cleanup_error);
                }
                eprintln!("managed Sandbox cleanup also failed: {cleanup_error}");
            }
        }
        let cleanup_manager = LocalExecutionManager::with_vm_backend(&state_path, &home_dir);
        if let Err(cleanup_error) = cleanup_manager
            .delete_filesystem_snapshot(&snapshot_id)
            .await
        {
            if result.is_ok() {
                return Err(cleanup_error.into());
            }
            eprintln!("managed snapshot cleanup also failed: {cleanup_error}");
        }
        result
    }

    fn validated_home() -> Result<PathBuf, AnyError> {
        require(
            std::env::var("A3S_BOX_MANAGED_SMOKE").as_deref() == Ok("1"),
            "set A3S_BOX_MANAGED_SMOKE=1 to acknowledge the destructive smoke test",
        )?;
        let home_dir = std::env::var_os("A3S_HOME")
            .map(PathBuf::from)
            .ok_or_else(|| failure("A3S_HOME must point to a dedicated smoke-test directory"))?;
        require(home_dir.is_absolute(), "A3S_HOME must be absolute")?;
        require(
            home_dir
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("managed-smoke")),
            "A3S_HOME must name a dedicated managed-smoke directory",
        )?;

        let expected_crun = home_dir.join("bin/crun").canonicalize()?;
        let configured_crun = std::env::var_os("A3S_BOX_CRUN_PATH")
            .map(PathBuf::from)
            .ok_or_else(|| failure("A3S_BOX_CRUN_PATH must select the isolated crun artifact"))?
            .canonicalize()?;
        require(
            configured_crun == expected_crun,
            "A3S_BOX_CRUN_PATH must equal A3S_HOME/bin/crun",
        )?;
        require(
            home_dir.join("bin/a3s-box-guest-init").is_file(),
            "A3S_HOME/bin/a3s-box-guest-init is missing",
        )?;
        require(
            home_dir.join("bin/a3s-box-shim").is_file(),
            "A3S_HOME/bin/a3s-box-shim is missing",
        )?;
        Ok(home_dir)
    }

    async fn exercise(
        home_dir: &Path,
        state_path: &Path,
        source_operation_id: &OperationId,
        restored_operation_id: &OperationId,
        snapshot_id: &ExecutionSnapshotId,
    ) -> Result<(), AnyError> {
        let image =
            std::env::var("A3S_BOX_SMOKE_IMAGE").unwrap_or_else(|_| "alpine:3.20".to_string());
        let config = BoxConfig {
            isolation: ExecutionIsolation::Sandbox,
            image,
            cmd: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "mkdir -p /state; if [ ! -f /state/value ]; then printf 'captured' > /state/value; fi; printf 'sandbox-state=%s\\n' \"$(cat /state/value)\"; printf 'sandbox-stderr\\n' >&2; while :; do sleep 60; done"
                    .to_string(),
            ],
            network: NetworkMode::None,
            ..Default::default()
        };
        let request = CreateExecutionRequest {
            external_sandbox_id: "managed-sandbox-smoke-external-id".to_string(),
            config: config.clone(),
            labels: BTreeMap::from([("purpose".to_string(), "managed-sandbox-smoke".to_string())]),
            policy: Default::default(),
            rootfs_snapshot_id: None,
        };

        let manager = LocalExecutionManager::with_vm_backend(state_path, home_dir);
        let reservation = manager.create(request, source_operation_id).await?;
        require(
            reservation.plan.backend == ExecutionBackend::Crun
                && reservation.plan.isolation_class == IsolationClass::SharedKernel,
            "Sandbox request did not resolve exclusively to the crun shared-kernel backend",
        )?;
        let execution_id = reservation.execution_id.clone();
        let status = manager.inspect(&execution_id).await?;
        require(
            status.state == ExecutionState::Created,
            "new managed Sandbox reservation is not created",
        )?;
        require(
            status.generation == reservation.generation && status.plan == reservation.plan,
            "created Sandbox inspection disagrees with its reservation",
        )?;

        let box_dir = home_dir.join("boxes").join(execution_id.as_str());
        validate_runtime_absent(home_dir, &execution_id)?;
        println!(
            "created execution={} backend=crun state=created",
            execution_id
        );

        drop(manager);
        let restarted = LocalExecutionManager::with_vm_backend(state_path, home_dir);
        let recovered_reservation = match restarted.reconcile(source_operation_id).await? {
            ReconcileOutcome::Created(reservation) => reservation,
            _ => {
                return Err(failure(
                    "restarted manager did not recover a created reservation",
                ))
            }
        };
        require(
            recovered_reservation.execution_id == reservation.execution_id
                && recovered_reservation.generation == reservation.generation
                && recovered_reservation.plan == reservation.plan
                && same_resources(&recovered_reservation.resources, &reservation.resources),
            "recovered Sandbox reservation differs from the durable reservation",
        )?;
        let recovered_status = restarted.inspect(&execution_id).await?;
        require(
            recovered_status.state == ExecutionState::Created,
            "restarted manager did not preserve the created Sandbox state",
        )?;
        validate_runtime_absent(home_dir, &execution_id)?;
        println!("recovered execution={} state=created", execution_id);

        let lease = restarted
            .start(&execution_id, recovered_reservation.generation)
            .await?;
        require(
            lease.execution_id == reservation.execution_id
                && lease.generation == reservation.generation
                && lease.plan == reservation.plan
                && same_resources(&lease.resources, &reservation.resources),
            "started Sandbox lease differs from its reservation",
        )?;
        let running_status = restarted.inspect(&execution_id).await?;
        require(
            running_status.state == ExecutionState::Running,
            "started managed Sandbox is not running",
        )?;
        let log_worker_identity = validate_runtime_record(home_dir, &box_dir, &execution_id)?;
        validate_structured_logs(&box_dir).await?;
        println!("started execution={} state=running", execution_id);

        let paused = restarted
            .pause(&execution_id, running_status.generation, true)
            .await?;
        require(
            restarted.inspect(&execution_id).await?.state == ExecutionState::Paused,
            "managed Sandbox did not enter the paused state",
        )?;
        let resumed = restarted
            .resume(&execution_id, paused.generation)
            .await?;
        require(
            resumed.generation.get() == paused.generation.get() + 1
                && restarted.inspect(&execution_id).await?.state == ExecutionState::Running,
            "managed Sandbox did not resume at the next generation",
        )?;
        println!(
            "pause-resume execution={} generation={}",
            execution_id,
            resumed.generation.get()
        );

        let snapshot_started = std::time::Instant::now();
        let snapshot = restarted
            .create_filesystem_snapshot(&execution_id, resumed.generation, snapshot_id)
            .await?;
        let snapshot_elapsed = snapshot_started.elapsed();
        require(
            snapshot.state == ExecutionState::Running
                && snapshot.lease.generation == resumed.generation
                && snapshot.size_bytes > 0,
            "managed Sandbox snapshot returned inconsistent evidence",
        )?;
        require(
            snapshot_elapsed <= std::time::Duration::from_secs(30),
            "managed Sandbox snapshot exceeded the 30-second smoke gate",
        )?;
        println!(
            "snapshot execution={} id={} bytes={} elapsed_ms={}",
            execution_id,
            snapshot_id,
            snapshot.size_bytes,
            snapshot_elapsed.as_millis()
        );

        let outcome = restarted
            .kill(&execution_id, resumed.generation)
            .await?;
        require(
            outcome == KillOutcome::Killed,
            "managed Sandbox kill did not own runtime cleanup",
        )?;
        let stopped = restarted.inspect(&execution_id).await?;
        require(
            stopped.state == ExecutionState::Stopped,
            "managed Sandbox did not persist a stopped state",
        )?;
        require(!box_dir.exists(), "managed Sandbox box directory leaked")?;
        require(
            !home_dir
                .join("run/crun")
                .join(execution_id.as_str())
                .exists(),
            "managed Sandbox crun state directory leaked",
        )?;
        require(
            !Path::new("/tmp/a3s-box-sockets")
                .join(execution_id.as_str())
                .exists(),
            "managed Sandbox socket directory leaked",
        )?;
        wait_for_process_exit(log_worker_identity).await?;
        println!("killed execution={} state=stopped cleanup=ok", execution_id);

        let restored_request = CreateExecutionRequest {
            external_sandbox_id: "managed-sandbox-restored-external-id".to_string(),
            config,
            labels: BTreeMap::from([(
                "purpose".to_string(),
                "managed-sandbox-restore-smoke".to_string(),
            )]),
            policy: Default::default(),
            rootfs_snapshot_id: Some(snapshot_id.clone()),
        };
        let restore_started = std::time::Instant::now();
        let restored_lease = restarted
            .create_and_start(restored_request, restored_operation_id)
            .await?;
        let restore_elapsed = restore_started.elapsed();
        require(
            restored_lease.plan.backend == ExecutionBackend::Crun
                && restored_lease.plan.isolation_class == IsolationClass::SharedKernel,
            "restored Sandbox did not stay on the crun backend",
        )?;
        require(
            restore_elapsed <= std::time::Duration::from_secs(30),
            "managed Sandbox restore exceeded the 30-second smoke gate",
        )?;
        let restored_id = restored_lease.execution_id.clone();
        let restored_box_dir = home_dir.join("boxes").join(restored_id.as_str());
        let restored_log_worker =
            validate_runtime_record(home_dir, &restored_box_dir, &restored_id)?;
        validate_snapshot_marker(home_dir, &restored_box_dir, snapshot_id)?;
        validate_structured_logs(&restored_box_dir).await?;
        require(
            matches!(
                restarted.delete_filesystem_snapshot(snapshot_id).await,
                Err(ExecutionManagerError::Conflict { .. })
            ),
            "active restored Sandbox did not protect its snapshot lower",
        )?;
        println!(
            "restored execution={} snapshot={} elapsed_ms={}",
            restored_id,
            snapshot_id,
            restore_elapsed.as_millis()
        );

        require(
            restarted
                .kill(&restored_id, restored_lease.generation)
                .await?
                == KillOutcome::Killed,
            "restored Sandbox kill did not own runtime cleanup",
        )?;
        require(
            restarted.delete_filesystem_snapshot(snapshot_id).await?,
            "managed snapshot was not deleted after restored Sandbox cleanup",
        )?;
        require(
            !restored_box_dir.exists(),
            "restored managed Sandbox box directory leaked",
        )?;
        wait_for_process_exit(restored_log_worker).await?;
        println!(
            "killed restored_execution={} snapshot_delete=ok cleanup=ok",
            restored_id
        );
        Ok(())
    }

    fn validate_runtime_absent(
        home_dir: &Path,
        execution_id: &ExecutionId,
    ) -> Result<(), AnyError> {
        require(
            !home_dir.join("boxes").join(execution_id.as_str()).exists(),
            "created Sandbox unexpectedly allocated a box directory",
        )?;
        require(
            !home_dir
                .join("run/crun")
                .join(execution_id.as_str())
                .exists(),
            "created Sandbox unexpectedly allocated a crun state directory",
        )?;
        require(
            !Path::new("/tmp/a3s-box-sockets")
                .join(execution_id.as_str())
                .exists(),
            "created Sandbox unexpectedly allocated a socket directory",
        )
    }

    fn same_resources(left: &ResourceConfig, right: &ResourceConfig) -> bool {
        left.vcpus == right.vcpus
            && left.memory_mb == right.memory_mb
            && left.disk_mb == right.disk_mb
            && left.timeout == right.timeout
    }

    fn validate_snapshot_marker(
        home_dir: &Path,
        box_dir: &Path,
        snapshot_id: &ExecutionSnapshotId,
    ) -> Result<(), AnyError> {
        let expected = home_dir
            .join("snapshots")
            .join(snapshot_id.as_str())
            .join("rootfs")
            .canonicalize()?;
        let marker = box_dir.join(".snapshot-lower");
        let actual = PathBuf::from(std::fs::read_to_string(&marker)?.trim());
        require(actual == expected, "restored Sandbox has the wrong CoW lower")?;
        require(
            std::fs::symlink_metadata(marker)?.file_type().is_file(),
            "restored Sandbox snapshot marker is not a regular file",
        )
    }

    fn validate_runtime_record(
        home_dir: &Path,
        box_dir: &Path,
        execution_id: &ExecutionId,
    ) -> Result<(u32, u64), AnyError> {
        let runtime_record = box_dir.join("sandbox/runtime.json");
        let record: serde_json::Value = serde_json::from_slice(&std::fs::read(&runtime_record)?)?;
        require(
            record.get("schema").and_then(serde_json::Value::as_str)
                == Some("a3s.box.sandbox-runtime.v1"),
            "Sandbox runtime record has an unexpected schema",
        )?;
        require(
            record
                .get("container_id")
                .and_then(serde_json::Value::as_str)
                == Some(execution_id.as_str()),
            "Sandbox runtime record does not use the internal execution ID",
        )?;
        let runtime_path = record
            .get("runtime_path")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
            .ok_or_else(|| failure("Sandbox runtime record has no runtime path"))?;
        require(
            runtime_path.canonicalize()? == home_dir.join("bin/crun").canonicalize()?,
            "Sandbox runtime record does not reference the certified crun artifact",
        )?;
        let log_worker_pid = record
            .get("log_worker_pid")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| failure("Sandbox runtime record has no log worker PID"))?;
        let log_worker_pid_start_time = record
            .get("log_worker_pid_start_time")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| failure("Sandbox runtime record has no log worker PID identity"))?;
        require(
            a3s_box_runtime::is_process_alive_with_identity(
                log_worker_pid,
                Some(log_worker_pid_start_time),
            ),
            "Sandbox log worker is not alive with its recorded identity",
        )?;
        Ok((log_worker_pid, log_worker_pid_start_time))
    }

    async fn validate_structured_logs(box_dir: &Path) -> Result<(), AnyError> {
        let path = box_dir.join("logs/container.json");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Ok(contents) = tokio::fs::read_to_string(&path).await {
                let entries: Vec<a3s_box_core::log::LogEntry> = contents
                    .lines()
                    .filter_map(|line| serde_json::from_str(line).ok())
                    .collect();
                let stdout = entries
                    .iter()
                    .any(|entry| {
                        entry.stream == "stdout" && entry.log == "sandbox-state=captured\n"
                    });
                let stderr = entries
                    .iter()
                    .any(|entry| entry.stream == "stderr" && entry.log == "sandbox-stderr\n");
                if stdout && stderr {
                    println!("logs stdout=ok stderr=ok format=json-file");
                    return Ok(());
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(failure(format!(
                    "Sandbox structured logs did not capture both streams at {}",
                    path.display()
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    async fn wait_for_process_exit(identity: (u32, u64)) -> Result<(), AnyError> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while a3s_box_runtime::is_process_alive_with_identity(identity.0, Some(identity.1)) {
            if tokio::time::Instant::now() >= deadline {
                return Err(failure("Sandbox log worker leaked after terminal cleanup"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        Ok(())
    }

    async fn cleanup(
        home_dir: &Path,
        state_path: &Path,
        operation_id: &OperationId,
    ) -> Result<(), AnyError> {
        if !state_path.exists() {
            return Ok(());
        }
        let store = ManagedExecutionStore::new(state_path);
        let Some(record) = store.get_by_operation_id(operation_id)? else {
            return Ok(());
        };
        let execution_id = ExecutionId::new(record.id.clone())?;
        let generation = record
            .managed_execution
            .as_ref()
            .ok_or_else(|| failure("smoke-test execution lost managed metadata"))?
            .generation;
        let manager = LocalExecutionManager::with_vm_backend(state_path, home_dir);
        let _ = manager.kill(&execution_id, generation).await?;
        Ok(())
    }

    fn require(condition: bool, message: &str) -> Result<(), AnyError> {
        if condition {
            Ok(())
        } else {
            Err(failure(message))
        }
    }

    fn failure(message: impl Into<String>) -> AnyError {
        Box::new(io::Error::other(message.into()))
    }

    struct RestrictiveUmask(libc::mode_t);

    impl RestrictiveUmask {
        fn install() -> Self {
            // SAFETY: umask has no memory-safety preconditions. This smoke is a
            // single-purpose process, and the guard restores the caller value.
            Self(unsafe { libc::umask(0o077) })
        }
    }

    impl Drop for RestrictiveUmask {
        fn drop(&mut self) {
            // SAFETY: see `install`; restoring the process umask is infallible.
            unsafe {
                libc::umask(self.0);
            }
        }
    }
}
