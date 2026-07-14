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
        ExecutionManager, ExecutionManagerError, ExecutionState, IsolationClass, KillOutcome,
        NetworkMode, OperationId,
    };
    use a3s_box_runtime::{LocalExecutionManager, ManagedExecutionStore};

    type AnyError = Box<dyn Error + Send + Sync>;

    pub(super) async fn run() -> Result<(), AnyError> {
        let home_dir = validated_home()?;
        let state_path = home_dir.join("managed-executions.json");
        let operation_id =
            OperationId::new(format!("managed-sandbox-smoke-{}", uuid::Uuid::new_v4()))?;

        let result = exercise(&home_dir, &state_path, &operation_id).await;
        if let Err(cleanup_error) = cleanup(&home_dir, &state_path, &operation_id).await {
            if result.is_ok() {
                return Err(cleanup_error);
            }
            eprintln!("managed Sandbox cleanup also failed: {cleanup_error}");
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
        Ok(home_dir)
    }

    async fn exercise(
        home_dir: &Path,
        state_path: &Path,
        operation_id: &OperationId,
    ) -> Result<(), AnyError> {
        let image =
            std::env::var("A3S_BOX_SMOKE_IMAGE").unwrap_or_else(|_| "alpine:3.20".to_string());
        let request = CreateExecutionRequest {
            external_sandbox_id: "managed-sandbox-smoke-external-id".to_string(),
            config: BoxConfig {
                isolation: ExecutionIsolation::Sandbox,
                image,
                cmd: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "while :; do sleep 60; done".to_string(),
                ],
                network: NetworkMode::None,
                ..Default::default()
            },
            labels: BTreeMap::from([("purpose".to_string(), "managed-sandbox-smoke".to_string())]),
        };

        let manager = LocalExecutionManager::with_vm_backend(state_path, home_dir);
        let lease = manager.create_and_start(request, operation_id).await?;
        require(
            lease.plan.backend == ExecutionBackend::Crun
                && lease.plan.isolation_class == IsolationClass::SharedKernel,
            "Sandbox request did not resolve exclusively to the crun shared-kernel backend",
        )?;
        let execution_id = lease.execution_id.clone();
        let status = manager.inspect(&execution_id).await?;
        require(
            status.state == ExecutionState::Running,
            "new managed Sandbox is not running",
        )?;

        let box_dir = home_dir.join("boxes").join(execution_id.as_str());
        validate_runtime_record(home_dir, &box_dir, &execution_id)?;
        println!(
            "created execution={} backend=crun state=running",
            execution_id
        );

        drop(manager);
        let recovered = LocalExecutionManager::with_vm_backend(state_path, home_dir);
        let recovered_status = recovered.inspect(&execution_id).await?;
        require(
            recovered_status.state == ExecutionState::Running,
            "restarted manager did not recover the running Sandbox",
        )?;
        validate_runtime_record(home_dir, &box_dir, &execution_id)?;
        println!("recovered execution={} state=running", execution_id);

        let pause_error = match recovered
            .pause(&execution_id, recovered_status.generation, true)
            .await
        {
            Ok(_) => return Err(failure("Sandbox pause unexpectedly succeeded")),
            Err(error) => error,
        };
        require(
            matches!(pause_error, ExecutionManagerError::Conflict { .. }),
            "Sandbox pause did not return a conflict",
        )?;
        let rolled_back = recovered.inspect(&execution_id).await?;
        require(
            rolled_back.state == ExecutionState::Running
                && rolled_back.generation == recovered_status.generation,
            "failed Sandbox pause did not roll back to the running generation",
        )?;
        println!("pause-rejected execution={} state=running", execution_id);

        let outcome = recovered
            .kill(&execution_id, rolled_back.generation)
            .await?;
        require(
            outcome == KillOutcome::Killed,
            "managed Sandbox kill did not own runtime cleanup",
        )?;
        let stopped = recovered.inspect(&execution_id).await?;
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
        println!("killed execution={} state=stopped cleanup=ok", execution_id);
        Ok(())
    }

    fn validate_runtime_record(
        home_dir: &Path,
        box_dir: &Path,
        execution_id: &ExecutionId,
    ) -> Result<(), AnyError> {
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
        )
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
}
