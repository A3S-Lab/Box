//! Runtime lifecycle projection over Box's durable local execution manager.

use std::time::Duration;

use a3s_box_core::{ExecutionManager, KillOutcome, ReconcileOutcome};
use a3s_runtime::contract::{
    RestartPolicy, RuntimeActionRequest, RuntimeFailure, RuntimeInspection, RuntimeObservation,
    RuntimeRemoval, RuntimeUnitClass, RuntimeUnitSpec, RuntimeUnitState,
};
use a3s_runtime::{RuntimeError, RuntimeResult, RuntimeUnitRecord};
use sha2::{Digest, Sha256};

use crate::{BoxRecord, ManagedExecutionState};

use super::mapping::{creation_request, operation};
use super::metadata::{
    local_identity, map_execution_error, now_ms, provider_identity_matches, timestamp_ms,
    validate_record_for_spec,
};
use super::BoxRuntimeDriver;

impl BoxRuntimeDriver {
    pub(super) async fn apply_unit(
        &self,
        spec: &RuntimeUnitSpec,
        current: &RuntimeObservation,
    ) -> RuntimeResult<RuntimeObservation> {
        // Complete validation and conversion before any Box state or provider
        // mutation. The returned request is reused for identity comparison.
        let request = creation_request(spec)?;
        let mut record = self.find_generation(spec).await?;

        if let Some(existing) = record.as_ref() {
            provider_identity_matches(current, existing)?;
            if should_replace_confirmed_loss(existing, current)? {
                self.retire_record(existing.clone(), &spec.unit_id).await?;
                record = None;
            }
        }

        let record = match record {
            Some(record) => self.ensure_started(spec, record).await?,
            None => {
                let operation_id = operation(spec)?;
                let reservation = self
                    .bounded("reservation", async {
                        self.manager
                            .create(request, &operation_id)
                            .await
                            .map_err(|error| map_execution_error(&spec.unit_id, error))
                    })
                    .await?;
                let record = self
                    .manager
                    .managed_record(&reservation.execution_id)
                    .await
                    .map_err(|error| map_execution_error(&spec.unit_id, error))?
                    .ok_or_else(|| {
                        RuntimeError::Protocol(
                            "Box lost a durable execution immediately after reservation".into(),
                        )
                    })?;
                validate_record_for_spec(&record, spec)?;
                self.ensure_started(spec, record).await?
            }
        };

        // Retire older Runtime generations as soon as the current generation
        // has a durable provider identity. A retry resumes any partial cleanup.
        self.retire_stale_generations(spec, &record.id).await?;

        match spec.class {
            RuntimeUnitClass::Task => self.wait_for_task(spec, record).await,
            RuntimeUnitClass::Service => self.observation(spec, &record, None, None).await,
        }
    }

    pub(super) async fn inspect_unit(
        &self,
        unit: &RuntimeUnitRecord,
    ) -> RuntimeResult<RuntimeInspection> {
        unit.validate().map_err(RuntimeError::Protocol)?;
        let Some(record) = self.find_generation(&unit.spec).await? else {
            return Ok(not_found(&unit.spec));
        };
        provider_identity_matches(&unit.observation, &record)?;
        let before = local_identity(&record)?.2;
        let record = self.refresh_record(&unit.spec, record).await?;
        if provider_was_lost(before, &record)? {
            return Ok(not_found(&unit.spec));
        }

        let record = if should_restart(&unit.spec, &record)? {
            self.restart_record(&unit.spec, record).await?
        } else {
            record
        };
        let observation = self.observation(&unit.spec, &record, None, None).await?;
        Ok(RuntimeInspection::Found {
            schema: RuntimeInspection::SCHEMA.into(),
            observation: Box::new(observation),
        })
    }

    pub(super) async fn stop_unit(
        &self,
        unit: &RuntimeUnitRecord,
        request: &RuntimeActionRequest,
    ) -> RuntimeResult<RuntimeObservation> {
        request.validate().map_err(RuntimeError::InvalidRequest)?;
        validate_action_identity(unit, request)?;
        if unit.observation.state.is_terminal() {
            return Ok(unit.observation.clone());
        }

        let Some(record) = self.find_generation(&unit.spec).await? else {
            return unknown_observation(&unit.observation);
        };
        provider_identity_matches(&unit.observation, &record)?;
        let before = local_identity(&record)?.2;
        let mut record = self.refresh_record(&unit.spec, record).await?;
        if provider_was_lost(before, &record)? {
            return unknown_observation(&unit.observation);
        }

        let (_, generation, state) = local_identity(&record)?;
        if !state.is_terminal() {
            let execution_id = a3s_box_core::ExecutionId::new(record.id.clone())
                .map_err(|error| RuntimeError::Protocol(error.to_string()))?;
            self.manager
                .kill(&execution_id, generation)
                .await
                .map_err(|error| map_execution_error(&unit.spec.unit_id, error))?;
            record = self.load_record(&unit.spec, &execution_id).await?;
        }

        self.observation(
            &unit.spec,
            &record,
            Some(RuntimeUnitState::Stopped),
            None,
        )
        .await
    }

    pub(super) async fn remove_unit(
        &self,
        unit: &RuntimeUnitRecord,
        request: &RuntimeActionRequest,
    ) -> RuntimeResult<RuntimeRemoval> {
        request.validate().map_err(RuntimeError::InvalidRequest)?;
        validate_action_identity(unit, request)?;
        let record = self.find_generation(&unit.spec).await?;
        let already_absent = record.is_none();
        if let Some(record) = record {
            provider_identity_matches(&unit.observation, &record)?;
            self.retire_record(record, &unit.spec.unit_id).await?;
        }
        let removal = RuntimeRemoval {
            schema: RuntimeRemoval::SCHEMA.into(),
            request_id: request.request_id.clone(),
            unit_id: request.unit_id.clone(),
            generation: request.generation,
            removed_at_ms: now_ms(),
            already_absent,
        };
        removal.validate().map_err(RuntimeError::Protocol)?;
        Ok(removal)
    }

    async fn ensure_started(
        &self,
        spec: &RuntimeUnitSpec,
        record: BoxRecord,
    ) -> RuntimeResult<BoxRecord> {
        validate_record_for_spec(&record, spec)?;
        let (execution_id, generation, state) = local_identity(&record)?;
        match state {
            ManagedExecutionState::Creating
            | ManagedExecutionState::Created
            | ManagedExecutionState::Starting => {
                let result = self
                    .bounded("startup", async {
                        self.manager
                            .start(&execution_id, generation)
                            .await
                            .map_err(|error| map_execution_error(&spec.unit_id, error))
                    })
                    .await;
                if let Err(error) = result {
                    let current = self.load_record(spec, &execution_id).await?;
                    if local_identity(&current)?.2.is_terminal() {
                        return Ok(current);
                    }
                    return Err(error);
                }
                self.load_record(spec, &execution_id).await
            }
            ManagedExecutionState::Running => Ok(record),
            ManagedExecutionState::Stopped | ManagedExecutionState::Failed => {
                if should_restart(spec, &record)? {
                    self.restart_record(spec, record).await
                } else {
                    Ok(record)
                }
            }
            ManagedExecutionState::Removing => Err(RuntimeError::ProviderUnavailable(format!(
                "Box execution {} is still being removed",
                record.id
            ))),
            state => Err(RuntimeError::ProviderUnavailable(format!(
                "Box execution {} cannot be applied while in state {state}",
                record.id
            ))),
        }
    }

    async fn wait_for_task(
        &self,
        spec: &RuntimeUnitSpec,
        mut record: BoxRecord,
    ) -> RuntimeResult<RuntimeObservation> {
        let timeout_ms = spec.resources.execution_timeout_ms.ok_or_else(|| {
            RuntimeError::InvalidRequest("Runtime Task has no execution timeout".into())
        })?;
        let timeout = Duration::from_millis(timeout_ms);
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let before = local_identity(&record)?.2;
            record = self.refresh_record(spec, record).await?;
            let state = local_identity(&record)?.2;
            if state.is_terminal() {
                if provider_was_lost(before, &record)? {
                    return self
                        .observation(
                            spec,
                            &record,
                            Some(RuntimeUnitState::Failed),
                            Some(RuntimeFailure {
                                code: "provider_lost".into(),
                                message: "Box lost the Sandbox provider resource".into(),
                                retryable: true,
                            }),
                        )
                        .await;
                }
                if should_restart(spec, &record)? {
                    record = self.restart_record(spec, record).await?;
                    continue;
                }
                return self.observation(spec, &record, None, None).await;
            }

            if tokio::time::Instant::now() >= deadline {
                let (execution_id, generation, _) = local_identity(&record)?;
                self.bounded("Task timeout cleanup", async {
                    self.manager
                        .kill(&execution_id, generation)
                        .await
                        .map_err(|error| map_execution_error(&spec.unit_id, error))
                })
                .await?;
                record = self.load_record(spec, &execution_id).await?;
                return self
                    .observation(
                        spec,
                        &record,
                        Some(RuntimeUnitState::Failed),
                        Some(RuntimeFailure {
                            code: "execution_timeout".into(),
                            message: format!("Box Task exceeded its {timeout_ms} ms timeout"),
                            retryable: false,
                        }),
                    )
                    .await;
            }

            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            tokio::time::sleep(self.config.task_poll_interval.min(remaining)).await;
        }
    }

    async fn refresh_record(
        &self,
        spec: &RuntimeUnitSpec,
        record: BoxRecord,
    ) -> RuntimeResult<BoxRecord> {
        let (execution_id, _, _) = local_identity(&record)?;
        self.bounded("inspection", async {
            self.manager
                .inspect(&execution_id)
                .await
                .map_err(|error| map_execution_error(&spec.unit_id, error))
        })
        .await?;
        self.load_record(spec, &execution_id).await
    }

    async fn load_record(
        &self,
        spec: &RuntimeUnitSpec,
        execution_id: &a3s_box_core::ExecutionId,
    ) -> RuntimeResult<BoxRecord> {
        let record = self
            .manager
            .managed_record(execution_id)
            .await
            .map_err(|error| map_execution_error(&spec.unit_id, error))?
            .ok_or_else(|| RuntimeError::NotFound {
                unit_id: spec.unit_id.clone(),
            })?;
        validate_record_for_spec(&record, spec)?;
        Ok(record)
    }

    async fn restart_record(
        &self,
        spec: &RuntimeUnitSpec,
        record: BoxRecord,
    ) -> RuntimeResult<BoxRecord> {
        let (execution_id, generation, _) = local_identity(&record)?;
        let operation_id = restart_operation(spec, generation)?;
        self.bounded("restart", async {
            self.manager
                .restart(&execution_id, generation, &operation_id)
                .await
                .map_err(|error| map_execution_error(&spec.unit_id, error))
        })
        .await?;
        self.load_record(spec, &execution_id).await
    }

    async fn retire_stale_generations(
        &self,
        spec: &RuntimeUnitSpec,
        current_id: &str,
    ) -> RuntimeResult<()> {
        let records = self.unit_records(&spec.unit_id).await?;
        if !records.iter().any(|record| record.id == current_id) {
            return Err(RuntimeError::ProviderUnavailable(
                "Box lost the current execution during generation reconciliation".into(),
            ));
        }
        for record in records {
            if record.id != current_id {
                self.retire_record(record, &spec.unit_id).await?;
            }
        }

        let remaining = self.unit_records(&spec.unit_id).await?;
        if remaining.len() != 1 || remaining[0].id != current_id {
            return Err(RuntimeError::Protocol(format!(
                "Box generation reconciliation for unit {:?} left {} executions",
                spec.unit_id,
                remaining.len()
            )));
        }
        validate_record_for_spec(&remaining[0], spec)
    }

    async fn retire_record(
        &self,
        mut record: BoxRecord,
        unit_id: &str,
    ) -> RuntimeResult<()> {
        let (execution_id, mut generation, mut state) = local_identity(&record)?;
        if matches!(
            state,
            ManagedExecutionState::RestartStopping | ManagedExecutionState::RestartStarting
        ) {
            let create_operation = record
                .managed_execution
                .as_ref()
                .ok_or_else(|| RuntimeError::Protocol("Box execution lost metadata".into()))?
                .operation_id
                .clone();
            match self
                .bounded("restart reconciliation", async {
                    self.manager
                        .reconcile(&create_operation)
                        .await
                        .map_err(|error| map_execution_error(unit_id, error))
                })
                .await
            {
                Ok(ReconcileOutcome::Absent) => return Ok(()),
                Ok(_) => {}
                Err(error) => return Err(error),
            }
            record = self
                .manager
                .managed_record(&execution_id)
                .await
                .map_err(|error| map_execution_error(unit_id, error))?
                .ok_or_else(|| RuntimeError::NotFound {
                    unit_id: unit_id.into(),
                })?;
            (_, generation, state) = local_identity(&record)?;
        }

        if !matches!(
            state,
            ManagedExecutionState::Created
                | ManagedExecutionState::Stopped
                | ManagedExecutionState::Failed
                | ManagedExecutionState::Removing
        ) {
            match self
                .bounded("generation retirement stop", async {
                    self.manager
                        .kill(&execution_id, generation)
                        .await
                        .map_err(|error| map_execution_error(unit_id, error))
                })
                .await
            {
                Ok(KillOutcome::Killed | KillOutcome::AlreadyStopped) => {}
                Err(error) => return Err(error),
            }
            record = self
                .manager
                .managed_record(&execution_id)
                .await
                .map_err(|error| map_execution_error(unit_id, error))?
                .ok_or_else(|| RuntimeError::NotFound {
                    unit_id: unit_id.into(),
                })?;
            generation = local_identity(&record)?.1;
        }

        self.bounded("generation retirement removal", async {
            self.manager
                .remove_execution(&execution_id, generation)
                .await
                .map_err(|error| map_execution_error(unit_id, error))
        })
        .await?;
        if self
            .manager
            .managed_record(&execution_id)
            .await
            .map_err(|error| map_execution_error(unit_id, error))?
            .is_some()
        {
            return Err(RuntimeError::Protocol(format!(
                "Box removal left execution {} in durable inventory",
                execution_id
            )));
        }
        Ok(())
    }

    pub(super) async fn observation(
        &self,
        spec: &RuntimeUnitSpec,
        record: &BoxRecord,
        state_override: Option<RuntimeUnitState>,
        failure_override: Option<RuntimeFailure>,
    ) -> RuntimeResult<RuntimeObservation> {
        validate_record_for_spec(record, spec)?;
        let state = state_override.unwrap_or(runtime_state(spec, record)?);
        let terminal = state.is_terminal();
        let metadata = record.managed_execution.as_ref().ok_or_else(|| {
            RuntimeError::Protocol(format!("Box execution {} lost metadata", record.id))
        })?;
        let started_at_ms = record.started_at.map(timestamp_ms).transpose()?;
        let finished_at_ms = if terminal {
            Some(
                metadata
                    .finished_at
                    .map(timestamp_ms)
                    .transpose()?
                    .unwrap_or_else(now_ms),
            )
        } else {
            None
        };
        let mut observed_at_ms = now_ms();
        if let Some(started) = started_at_ms {
            observed_at_ms = observed_at_ms.max(started);
        }
        if let Some(finished) = finished_at_ms {
            observed_at_ms = observed_at_ms.max(finished);
        }
        let failure = if state == RuntimeUnitState::Failed {
            Some(failure_override.unwrap_or_else(|| exit_failure(record)))
        } else {
            None
        };
        let observation = RuntimeObservation {
            schema: RuntimeObservation::SCHEMA.into(),
            unit_id: spec.unit_id.clone(),
            generation: spec.generation,
            spec_digest: spec.digest().map_err(RuntimeError::Protocol)?,
            class: spec.class,
            state,
            provider_resource_id: Some(record.id.clone()),
            provider_build: Some(self.provider_build().await?),
            observed_at_ms,
            started_at_ms,
            finished_at_ms,
            health: None,
            outputs: Vec::new(),
            usage: None,
            evidence: None,
            provider_attestation: None,
            failure,
        };
        observation
            .validate_against(spec)
            .map_err(RuntimeError::Protocol)?;
        Ok(observation)
    }
}

fn runtime_state(spec: &RuntimeUnitSpec, record: &BoxRecord) -> RuntimeResult<RuntimeUnitState> {
    let state = local_identity(record)?.2;
    match state {
        ManagedExecutionState::Creating => Ok(RuntimeUnitState::Preparing),
        ManagedExecutionState::Created
        | ManagedExecutionState::Starting
        | ManagedExecutionState::RestartStarting => Ok(RuntimeUnitState::Starting),
        ManagedExecutionState::Running
        | ManagedExecutionState::Pausing
        | ManagedExecutionState::Paused
        | ManagedExecutionState::Resuming
        | ManagedExecutionState::Snapshotting
        | ManagedExecutionState::RestartStopping => Ok(RuntimeUnitState::Running),
        ManagedExecutionState::Killing | ManagedExecutionState::Removing => {
            Ok(RuntimeUnitState::Stopping)
        }
        ManagedExecutionState::Stopped | ManagedExecutionState::Failed => match spec.class {
            RuntimeUnitClass::Task if record.exit_code == Some(0) => {
                Ok(RuntimeUnitState::Succeeded)
            }
            RuntimeUnitClass::Task => Ok(RuntimeUnitState::Failed),
            RuntimeUnitClass::Service if record.exit_code.unwrap_or_default() == 0 => {
                Ok(RuntimeUnitState::Stopped)
            }
            RuntimeUnitClass::Service => Ok(RuntimeUnitState::Failed),
        },
    }
}

fn exit_failure(record: &BoxRecord) -> RuntimeFailure {
    let message = match record.exit_code {
        Some(code) => format!("Box Sandbox exited with code {code}"),
        None => "Box Sandbox exited without a recoverable exit code".into(),
    };
    RuntimeFailure {
        code: "sandbox_exit".into(),
        message,
        retryable: false,
    }
}

fn should_replace_confirmed_loss(
    record: &BoxRecord,
    current: &RuntimeObservation,
) -> RuntimeResult<bool> {
    Ok(current.state == RuntimeUnitState::Unknown
        && local_identity(record)?.2.is_terminal()
        && record.exit_code.is_none())
}

fn provider_was_lost(before: ManagedExecutionState, after: &BoxRecord) -> RuntimeResult<bool> {
    Ok(before.keeps_resources()
        && local_identity(after)?.2 == ManagedExecutionState::Failed
        && after.exit_code.is_none())
}

fn should_restart(spec: &RuntimeUnitSpec, record: &BoxRecord) -> RuntimeResult<bool> {
    if !local_identity(record)?.2.is_terminal() || record.exit_code.is_none() {
        return Ok(false);
    }
    let attempts = local_identity(record)?.1.get().saturating_sub(1);
    Ok(match &spec.restart {
        RestartPolicy::Never => false,
        RestartPolicy::Always => spec.class == RuntimeUnitClass::Service,
        RestartPolicy::OnFailure { max_retries } => {
            record.exit_code != Some(0) && attempts < u64::from(*max_retries)
        }
    })
}

fn restart_operation(
    spec: &RuntimeUnitSpec,
    generation: a3s_box_core::ExecutionGeneration,
) -> RuntimeResult<a3s_box_core::OperationId> {
    let mut digest = Sha256::new();
    digest.update(b"a3s-box-runtime-restart-v1\0");
    digest.update(spec.unit_id.as_bytes());
    digest.update(spec.generation.to_be_bytes());
    digest.update(spec.digest().map_err(RuntimeError::Protocol)?.as_bytes());
    digest.update(generation.get().to_be_bytes());
    a3s_box_core::OperationId::new(format!("a3s-runtime-restart-v1:{:x}", digest.finalize()))
        .map_err(|error| RuntimeError::InvalidRequest(error.to_string()))
}

fn validate_action_identity(
    unit: &RuntimeUnitRecord,
    request: &RuntimeActionRequest,
) -> RuntimeResult<()> {
    if request.unit_id != unit.spec.unit_id || request.generation != unit.spec.generation {
        return Err(RuntimeError::InvalidRequest(
            "Runtime action identity does not match its unit record".into(),
        ));
    }
    Ok(())
}

fn unknown_observation(current: &RuntimeObservation) -> RuntimeResult<RuntimeObservation> {
    let mut unknown = current.clone();
    unknown.state = RuntimeUnitState::Unknown;
    unknown.observed_at_ms = unknown.observed_at_ms.max(now_ms());
    unknown.finished_at_ms = None;
    unknown.health = None;
    unknown.outputs.clear();
    unknown.failure = None;
    unknown.validate().map_err(RuntimeError::Protocol)?;
    Ok(unknown)
}

fn not_found(spec: &RuntimeUnitSpec) -> RuntimeInspection {
    RuntimeInspection::NotFound {
        schema: RuntimeInspection::SCHEMA.into(),
        unit_id: spec.unit_id.clone(),
        last_generation: Some(spec.generation),
    }
}
