//! Canonical record mapping for managed local executions.

use std::collections::HashMap;
use std::path::Path;

use a3s_box_core::log::LogConfig;
use a3s_box_core::{
    CreateExecutionRequest, ExecutionGeneration, ExecutionId, ExecutionLease,
    ExecutionManagerError, ExecutionManagerResult, ExecutionReservation, ExecutionState,
    ExecutionStatus, NetworkMode, OperationId,
};
use chrono::{DateTime, Utc};

use super::LocalExecutionHandle;
use crate::{BoxRecord, ManagedExecutionMetadata, ManagedExecutionState};

pub(crate) fn build_managed_record(
    home_dir: &Path,
    execution_id: &ExecutionId,
    operation_id: OperationId,
    request: CreateExecutionRequest,
    now: DateTime<Utc>,
) -> ExecutionManagerResult<BoxRecord> {
    let metadata =
        ManagedExecutionMetadata::new(operation_id, ExecutionGeneration::INITIAL, request.clone())
            .map_err(|error| ExecutionManagerError::InvalidRequest(error.to_string()))?;
    let config = &request.config;
    let short_id = BoxRecord::make_short_id(execution_id.as_str());
    let box_dir = home_dir.join("boxes").join(execution_id.as_str());
    let network_name = match &config.network {
        NetworkMode::Bridge { network } => Some(network.clone()),
        NetworkMode::Tsi | NetworkMode::None => None,
    };
    let env = config.extra_env.iter().cloned().collect::<HashMap<_, _>>();
    let labels = request
        .labels
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();

    Ok(BoxRecord {
        id: execution_id.to_string(),
        short_id: short_id.clone(),
        name: format!("managed-{short_id}"),
        image: config.image.clone(),
        isolation: config.isolation,
        managed_execution: Some(metadata),
        status: ManagedExecutionState::Created.as_status().to_string(),
        pid: None,
        pid_start_time: None,
        cpus: config.resources.vcpus,
        memory_mb: config.resources.memory_mb,
        volumes: config.volumes.clone(),
        virtiofs_cache: config.virtiofs_cache.clone(),
        env,
        cmd: config.cmd.clone(),
        entrypoint: config.entrypoint_override.clone(),
        box_dir: box_dir.clone(),
        exec_socket_path: box_dir.join("sockets/exec.sock"),
        console_log: box_dir.join("logs/console.log"),
        created_at: now,
        started_at: None,
        auto_remove: false,
        hostname: config.hostname.clone(),
        user: config.user.clone(),
        workdir: config.workdir.clone(),
        restart_policy: "no".to_string(),
        port_map: config.port_map.clone(),
        labels,
        stopped_by_user: false,
        restart_count: 0,
        max_restart_count: 0,
        exit_code: None,
        health_check: None,
        healthcheck_disabled: false,
        health_status: "none".to_string(),
        health_retries: 0,
        health_last_check: None,
        network_mode: config.network.clone(),
        network_name,
        volume_names: Vec::new(),
        tmpfs: config.tmpfs.clone(),
        anonymous_volumes: Vec::new(),
        resource_limits: config.resource_limits.clone(),
        log_config: LogConfig::default(),
        add_host: config.add_hosts.clone(),
        platform: None,
        init: false,
        read_only: config.read_only,
        cap_add: config.cap_add.clone(),
        cap_drop: config.cap_drop.clone(),
        security_opt: config.security_opt.clone(),
        privileged: config.privileged,
        devices: Vec::new(),
        gpus: None,
        shm_size: None,
        stop_signal: None,
        stop_timeout: None,
        oom_kill_disable: false,
        oom_score_adj: None,
    })
}

pub(crate) fn apply_handle(record: &mut BoxRecord, handle: &LocalExecutionHandle) {
    record.pid = handle.pid;
    record.pid_start_time = handle.pid_start_time;
    record.exec_socket_path = handle.exec_socket_path.clone();
    record.console_log = handle.console_log.clone();
    record.started_at = Some(handle.started_at);
    record.anonymous_volumes = handle.anonymous_volumes.clone();
    record.exit_code = None;
}

pub(crate) fn clear_live_runtime(record: &mut BoxRecord, exit_code: Option<i32>) {
    record.pid = None;
    record.pid_start_time = None;
    record.exit_code = exit_code;
    record.health_status = "none".to_string();
    record.health_retries = 0;
}

pub(crate) fn reservation_from_record(
    record: &BoxRecord,
) -> ExecutionManagerResult<ExecutionReservation> {
    let execution_id = execution_id(record)?;
    let metadata = metadata(record, &execution_id)?;
    Ok(ExecutionReservation {
        execution_id,
        generation: metadata.generation,
        plan: metadata.plan.clone(),
        resources: metadata.request.config.resources.clone(),
        created_at: record.created_at,
    })
}

pub(crate) fn lease_from_record(record: &BoxRecord) -> ExecutionManagerResult<ExecutionLease> {
    let execution_id = execution_id(record)?;
    let metadata = metadata(record, &execution_id)?;
    let started_at = record.started_at.ok_or_else(|| {
        ExecutionManagerError::Internal(format!(
            "managed execution {execution_id} is ready without a start timestamp"
        ))
    })?;
    Ok(ExecutionLease {
        execution_id,
        generation: metadata.generation,
        plan: metadata.plan.clone(),
        resources: metadata.request.config.resources.clone(),
        started_at,
    })
}

pub(crate) fn status_from_record(
    record: &BoxRecord,
    state: ExecutionState,
) -> ExecutionManagerResult<ExecutionStatus> {
    let execution_id = execution_id(record)?;
    let metadata = metadata(record, &execution_id)?;
    Ok(ExecutionStatus {
        execution_id,
        generation: metadata.generation,
        state,
        plan: metadata.plan.clone(),
    })
}

pub(crate) fn execution_id(record: &BoxRecord) -> ExecutionManagerResult<ExecutionId> {
    ExecutionId::new(record.id.clone())
        .map_err(|error| ExecutionManagerError::Internal(error.to_string()))
}

fn metadata<'a>(
    record: &'a BoxRecord,
    execution_id: &ExecutionId,
) -> ExecutionManagerResult<&'a ManagedExecutionMetadata> {
    record.managed_execution.as_ref().ok_or_else(|| {
        ExecutionManagerError::Internal(format!(
            "execution {execution_id} lost managed lifecycle metadata"
        ))
    })
}
