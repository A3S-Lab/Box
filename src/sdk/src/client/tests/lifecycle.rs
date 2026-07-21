#[derive(Debug)]
enum LifecycleCall {
    Create {
        request: serde_json::Value,
        operation_id: String,
    },
    Start {
        execution_id: String,
        generation: u64,
    },
    Run {
        request: serde_json::Value,
        operation_id: String,
    },
}

struct RecordingExecutionManager {
    calls: std::sync::Mutex<Vec<LifecycleCall>>,
    reservation: a3s_box_core::ExecutionReservation,
    lease: a3s_box_core::ExecutionLease,
}

#[async_trait::async_trait]
impl a3s_box_core::ExecutionManager for RecordingExecutionManager {
    async fn create(
        &self,
        request: a3s_box_core::CreateExecutionRequest,
        operation_id: &a3s_box_core::OperationId,
    ) -> a3s_box_core::ExecutionManagerResult<a3s_box_core::ExecutionReservation> {
        self.calls.lock().unwrap().push(LifecycleCall::Create {
            request: serde_json::to_value(request).unwrap(),
            operation_id: operation_id.to_string(),
        });
        Ok(self.reservation.clone())
    }

    async fn start(
        &self,
        execution_id: &a3s_box_core::ExecutionId,
        generation: a3s_box_core::ExecutionGeneration,
    ) -> a3s_box_core::ExecutionManagerResult<a3s_box_core::ExecutionLease> {
        self.calls.lock().unwrap().push(LifecycleCall::Start {
            execution_id: execution_id.to_string(),
            generation: generation.get(),
        });
        Ok(self.lease.clone())
    }

    async fn create_and_start(
        &self,
        request: a3s_box_core::CreateExecutionRequest,
        operation_id: &a3s_box_core::OperationId,
    ) -> a3s_box_core::ExecutionManagerResult<a3s_box_core::ExecutionLease> {
        self.calls.lock().unwrap().push(LifecycleCall::Run {
            request: serde_json::to_value(request).unwrap(),
            operation_id: operation_id.to_string(),
        });
        Ok(self.lease.clone())
    }

    async fn inspect(
        &self,
        execution_id: &a3s_box_core::ExecutionId,
    ) -> a3s_box_core::ExecutionManagerResult<a3s_box_core::ExecutionStatus> {
        Err(a3s_box_core::ExecutionManagerError::NotFound(
            execution_id.clone(),
        ))
    }

    async fn pause(
        &self,
        execution_id: &a3s_box_core::ExecutionId,
        _generation: a3s_box_core::ExecutionGeneration,
        _keep_memory: bool,
    ) -> a3s_box_core::ExecutionManagerResult<a3s_box_core::ExecutionLease> {
        Err(a3s_box_core::ExecutionManagerError::NotFound(
            execution_id.clone(),
        ))
    }

    async fn resume(
        &self,
        execution_id: &a3s_box_core::ExecutionId,
        _generation: a3s_box_core::ExecutionGeneration,
    ) -> a3s_box_core::ExecutionManagerResult<a3s_box_core::ExecutionLease> {
        Err(a3s_box_core::ExecutionManagerError::NotFound(
            execution_id.clone(),
        ))
    }

    async fn kill(
        &self,
        execution_id: &a3s_box_core::ExecutionId,
        _generation: a3s_box_core::ExecutionGeneration,
    ) -> a3s_box_core::ExecutionManagerResult<a3s_box_core::KillOutcome> {
        Err(a3s_box_core::ExecutionManagerError::NotFound(
            execution_id.clone(),
        ))
    }

    async fn reconcile(
        &self,
        _operation_id: &a3s_box_core::OperationId,
    ) -> a3s_box_core::ExecutionManagerResult<a3s_box_core::ReconcileOutcome> {
        Ok(a3s_box_core::ReconcileOutcome::Absent)
    }
}

#[tokio::test]
async fn lifecycle_calls_preserve_complete_request_and_fencing_identity() {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use a3s_box_core::{
        resolve_execution, BoxConfig, CreateExecutionRequest, ExecutionGeneration,
        ExecutionHealthCheck, ExecutionId, ExecutionLease, ExecutionRecordPolicy,
        ExecutionReservation, ExecutionRestartPolicy, OperationId, ResourceLimits,
    };
    use chrono::Utc;

    let temp = tempfile::tempdir().unwrap();
    let config = BoxConfig {
        image: "registry.example/sdk:latest".to_string(),
        isolation: a3s_box_core::ExecutionIsolation::Sandbox,
        extra_env: vec![("SDK_CALLER".to_string(), "preserved".to_string())],
        dns: vec!["1.1.1.1".to_string()],
        read_only: true,
        resource_limits: ResourceLimits {
            pids_limit: Some(64),
            ..ResourceLimits::default()
        },
        ..BoxConfig::default()
    };
    let request = CreateExecutionRequest {
        external_sandbox_id: "sdk-external-id".to_string(),
        config: config.clone(),
        labels: BTreeMap::from([("caller".to_string(), "rust-sdk".to_string())]),
        policy: ExecutionRecordPolicy {
            name: Some("sdk-box".to_string()),
            auto_remove: true,
            restart_policy: ExecutionRestartPolicy::OnFailure,
            max_restart_count: 4,
            health_check: Some(ExecutionHealthCheck {
                cmd: vec!["true".to_string()],
                interval_secs: 7,
                timeout_secs: 3,
                retries: 2,
                start_period_secs: 1,
            }),
            healthcheck_disabled: false,
            log_config: a3s_box_core::log::LogConfig::default(),
            volume_names: vec!["sdk-data".to_string()],
            platform: Some("linux/amd64".to_string()),
            init: true,
            devices: vec!["/dev/null".to_string()],
            gpus: Some("none".to_string()),
            shm_size: Some(16 * 1024 * 1024),
            stop_signal: Some("SIGTERM".to_string()),
            stop_timeout: Some(9),
            oom_kill_disable: true,
            oom_score_adj: Some(100),
        },
        rootfs_snapshot_id: None,
    };
    let request_json = serde_json::to_value(&request).unwrap();
    let execution_id = ExecutionId::new("sdk-execution-id").unwrap();
    let operation_id = OperationId::new("sdk-operation-id").unwrap();
    let plan = resolve_execution(&config).unwrap();
    let reservation = ExecutionReservation {
        execution_id: execution_id.clone(),
        generation: ExecutionGeneration::INITIAL,
        plan: plan.clone(),
        resources: config.resources.clone(),
        created_at: Utc::now(),
    };
    let lease = ExecutionLease {
        execution_id: execution_id.clone(),
        generation: ExecutionGeneration::INITIAL,
        plan,
        resources: config.resources,
        started_at: Utc::now(),
    };
    let manager = Arc::new(RecordingExecutionManager {
        calls: std::sync::Mutex::new(Vec::new()),
        reservation,
        lease,
    });
    let client =
        A3sBoxClient::with_execution_manager(A3sBoxPaths::from_home(temp.path()), manager.clone());

    let created = client
        .create_box(request.clone(), &operation_id)
        .await
        .unwrap();
    assert_eq!(created.execution_id, execution_id);
    let started = client
        .start_box(&created.execution_id, created.generation)
        .await
        .unwrap();
    assert_eq!(started.execution_id, execution_id);
    let running = client.run_box(request, &operation_id).await.unwrap();
    assert_eq!(running.execution_id, execution_id);

    let calls = manager.calls.lock().unwrap();
    assert!(matches!(
        &calls[0],
        LifecycleCall::Create {
            request,
            operation_id
        } if request == &request_json && operation_id == "sdk-operation-id"
    ));
    assert!(matches!(
        &calls[1],
        LifecycleCall::Start {
            execution_id,
            generation: 1
        } if execution_id == "sdk-execution-id"
    ));
    assert!(matches!(
        &calls[2],
        LifecycleCall::Run {
            request,
            operation_id
        } if request == &request_json && operation_id == "sdk-operation-id"
    ));
}

#[tokio::test]
async fn sdk_create_rejects_unsupported_microvm_security_before_persistence() {
    let temp = tempfile::tempdir().unwrap();
    let client = A3sBoxClient::from_home(temp.path());

    for (index, (security_option, expected)) in [
        ("apparmor=runtime/default", "AppArmor"),
        ("label=type:container_t", "SELinux"),
        ("seccomp=/profiles/restricted.json", "custom seccomp"),
    ]
    .into_iter()
    .enumerate()
    {
        let request = a3s_box_core::CreateExecutionRequest {
            external_sandbox_id: format!("sdk-unsupported-security-{index}"),
            config: a3s_box_core::BoxConfig {
                image: "alpine:3.20".to_string(),
                isolation: a3s_box_core::ExecutionIsolation::Microvm,
                security_opt: vec![security_option.to_string()],
                ..Default::default()
            },
            labels: Default::default(),
            policy: Default::default(),
            rootfs_snapshot_id: None,
        };
        let operation_id =
            a3s_box_core::OperationId::new(format!("sdk-unsupported-security-operation-{index}"))
                .unwrap();

        let error = client.create_box(request, &operation_id).await.unwrap_err();

        assert!(matches!(
            &error,
            ClientError::Execution(a3s_box_core::ExecutionManagerError::InvalidRequest(message))
                if message.contains(expected)
        ));
        assert!(!client.paths().boxes_file.exists());
    }
}

#[cfg(windows)]
#[tokio::test]
async fn sdk_create_rejects_windows_health_check_before_persistence() {
    let temp = tempfile::tempdir().unwrap();
    let client = A3sBoxClient::from_home(temp.path());
    let mut request = a3s_box_core::CreateExecutionRequest {
        external_sandbox_id: "sdk-windows-health".to_string(),
        config: a3s_box_core::BoxConfig {
            image: "alpine:3.20".to_string(),
            ..Default::default()
        },
        labels: Default::default(),
        policy: Default::default(),
        rootfs_snapshot_id: None,
    };
    request.policy.health_check = Some(a3s_box_core::ExecutionHealthCheck {
        cmd: vec!["true".to_string()],
        interval_secs: 30,
        timeout_secs: 5,
        retries: 3,
        start_period_secs: 0,
    });
    let operation_id = a3s_box_core::OperationId::new("sdk-windows-health-operation").unwrap();

    let error = client.create_box(request, &operation_id).await.unwrap_err();

    assert!(matches!(
        &error,
        ClientError::Execution(a3s_box_core::ExecutionManagerError::InvalidRequest(message))
            if message.contains("health checks are not supported on Windows")
    ));
    assert!(!client.paths().boxes_file.exists());
}

#[cfg(windows)]
#[tokio::test]
async fn sdk_start_rejects_persisted_windows_health_check_without_claiming() {
    let temp = tempfile::tempdir().unwrap();
    let client = A3sBoxClient::from_home(temp.path());
    let mut request = a3s_box_core::CreateExecutionRequest {
        external_sandbox_id: "sdk-persisted-windows-health".to_string(),
        config: a3s_box_core::BoxConfig {
            image: "alpine:3.20".to_string(),
            ..Default::default()
        },
        labels: Default::default(),
        policy: Default::default(),
        rootfs_snapshot_id: None,
    };
    request.policy.health_check = Some(a3s_box_core::ExecutionHealthCheck {
        cmd: vec!["true".to_string()],
        interval_secs: 30,
        timeout_secs: 5,
        retries: 3,
        start_period_secs: 0,
    });
    request.policy.healthcheck_disabled = true;
    let operation_id =
        a3s_box_core::OperationId::new("sdk-persisted-windows-health-operation").unwrap();
    let reservation = client.create_box(request, &operation_id).await.unwrap();

    let state_path = &client.paths().boxes_file;
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(state_path).unwrap()).unwrap();
    let record = state.as_array_mut().unwrap().first_mut().unwrap();
    record["healthcheck_disabled"] = serde_json::json!(false);
    record["managed_execution"]["request"]["policy"]["healthcheck_disabled"] =
        serde_json::json!(false);
    std::fs::write(state_path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();

    let error = client
        .start_box(&reservation.execution_id, reservation.generation)
        .await
        .unwrap_err();

    assert!(matches!(
        &error,
        ClientError::Execution(a3s_box_core::ExecutionManagerError::InvalidRequest(message))
            if message.contains("health checks are not supported on Windows")
    ));
    let persisted: serde_json::Value =
        serde_json::from_slice(&std::fs::read(state_path).unwrap()).unwrap();
    assert_eq!(persisted[0]["status"], "created");
    assert!(persisted[0]["managed_execution"]["pending_operation"].is_null());
}
