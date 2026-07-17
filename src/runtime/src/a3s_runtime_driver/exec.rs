//! Generation-bound, ambiguity-safe Runtime exec mapping.

use std::time::Duration;

use a3s_box_core::{ExecRequest, ExecutionManager, ExecutionSessionManager};
use a3s_runtime::contract::{
    RuntimeExecRequest, RuntimeExecResult, RuntimeUnitState,
};
use a3s_runtime::{RuntimeError, RuntimeResult, RuntimeUnitRecord};

use crate::ManagedExecutionState;

use super::metadata::{
    local_identity, map_execution_error, now_ms, provider_identity_matches,
    validate_record_for_spec,
};
use super::BoxRuntimeDriver;

const NANOS_PER_MILLISECOND: u64 = 1_000_000;

impl BoxRuntimeDriver {
    pub(super) async fn execute_runtime_command(
        &self,
        unit: &RuntimeUnitRecord,
        request: &RuntimeExecRequest,
    ) -> RuntimeResult<RuntimeExecResult> {
        unit.validate().map_err(RuntimeError::Protocol)?;
        request.validate().map_err(RuntimeError::InvalidRequest)?;
        validate_exec_identity(unit, request)?;
        if unit.observation.state != RuntimeUnitState::Running {
            return Err(RuntimeError::InvalidRequest(format!(
                "Box Runtime exec requires a running unit; {:?} is {:?}",
                unit.spec.unit_id, unit.observation.state
            )));
        }

        let record = self
            .find_generation(&unit.spec)
            .await?
            .ok_or_else(|| RuntimeError::NotFound {
                unit_id: unit.spec.unit_id.clone(),
            })?;
        provider_identity_matches(&unit.observation, &record)?;
        validate_record_for_spec(&record, &unit.spec)?;
        let (execution_id, generation, _) = local_identity(&record)?;

        // Refresh provider state before crossing the exec boundary. The
        // session manager repeats the generation and process-identity checks
        // after opening the runtime endpoint, preventing a concurrent local
        // restart from redirecting this command to another generation.
        self.bounded("exec inspection", async {
            self.manager
                .inspect(&execution_id)
                .await
                .map_err(|error| map_execution_error(&unit.spec.unit_id, error))
        })
        .await?;
        let record = self
            .manager
            .managed_record(&execution_id)
            .await
            .map_err(|error| map_execution_error(&unit.spec.unit_id, error))?
            .ok_or_else(|| RuntimeError::NotFound {
                unit_id: unit.spec.unit_id.clone(),
            })?;
        validate_record_for_spec(&record, &unit.spec)?;
        let (_, refreshed_generation, state) = local_identity(&record)?;
        if state != ManagedExecutionState::Running {
            return Err(RuntimeError::InvalidRequest(format!(
                "Box Runtime exec requires a running provider resource; execution {} is {state}",
                record.id
            )));
        }
        if refreshed_generation != generation {
            return Err(RuntimeError::ProviderUnavailable(format!(
                "Box execution {} restarted while binding Runtime exec",
                record.id
            )));
        }

        let timeout = effective_timeout(request)?;
        let timeout_ns = u64::try_from(timeout.as_nanos())
            .map_err(|_| RuntimeError::InvalidRequest("Runtime exec timeout overflows u64".into()))?;
        let output = self
            .manager
            .execute(
                &execution_id,
                refreshed_generation,
                ExecRequest {
                    request_id: Some(request.request_id.clone()),
                    cmd: request.command.clone(),
                    timeout_ns,
                    env: Vec::new(),
                    working_dir: None,
                    rootfs: None,
                    stdin: None,
                    stdin_streaming: false,
                    user: None,
                    streaming: false,
                },
            )
            .await
            .map_err(|error| map_execution_error(&unit.spec.unit_id, error))?;

        // Read the durable record without an additional provider mutation. The
        // per-unit Runtime operation lease excludes stop/remove/restart while
        // this call is active, and the session boundary already fenced local
        // generation changes immediately before dispatch.
        let record = self
            .manager
            .managed_record(&execution_id)
            .await
            .map_err(|error| map_execution_error(&unit.spec.unit_id, error))?
            .ok_or_else(|| RuntimeError::NotFound {
                unit_id: unit.spec.unit_id.clone(),
            })?;
        validate_record_for_spec(&record, &unit.spec)?;
        let observation = self.observation(&unit.spec, &record, None, None).await?;
        let result = RuntimeExecResult {
            schema: RuntimeExecResult::SCHEMA.into(),
            request_id: request.request_id.clone(),
            observation,
            exit_code: output.exit_code,
            stdout: decode_output("stdout", output.stdout)?,
            stderr: decode_output("stderr", output.stderr)?,
            truncated: output.truncated,
        };
        result.validate().map_err(RuntimeError::Protocol)?;
        Ok(result)
    }
}

fn validate_exec_identity(
    unit: &RuntimeUnitRecord,
    request: &RuntimeExecRequest,
) -> RuntimeResult<()> {
    if request.unit_id != unit.spec.unit_id || request.generation != unit.spec.generation {
        return Err(RuntimeError::InvalidRequest(
            "Runtime exec identity does not match its unit record".into(),
        ));
    }
    Ok(())
}

fn effective_timeout(request: &RuntimeExecRequest) -> RuntimeResult<Duration> {
    let mut timeout_ms = request.timeout_ms;
    if let Some(deadline_at_ms) = request.deadline_at_ms {
        let remaining_ms = deadline_at_ms.checked_sub(now_ms()).ok_or_else(|| {
            RuntimeError::DeadlineExceeded("exec request expired before provider dispatch".into())
        })?;
        if remaining_ms == 0 {
            return Err(RuntimeError::DeadlineExceeded(
                "exec request expired before provider dispatch".into(),
            ));
        }
        timeout_ms = timeout_ms.min(remaining_ms);
    }
    timeout_ms
        .checked_mul(NANOS_PER_MILLISECOND)
        .ok_or_else(|| RuntimeError::InvalidRequest("Runtime exec timeout overflows u64".into()))?;
    Ok(Duration::from_millis(timeout_ms))
}

fn decode_output(stream: &'static str, bytes: Vec<u8>) -> RuntimeResult<String> {
    String::from_utf8(bytes).map_err(|error| {
        RuntimeError::Protocol(format!(
            "Box Runtime exec {stream} is not valid UTF-8 at byte {}",
            error.utf8_error().valid_up_to()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_timeout_honors_relative_bound_and_rejects_overflow() {
        let request = RuntimeExecRequest {
            schema: RuntimeExecRequest::SCHEMA.into(),
            request_id: "request-1".into(),
            unit_id: "unit-1".into(),
            generation: 1,
            command: vec!["true".into()],
            timeout_ms: 5_000,
            deadline_at_ms: None,
        };
        assert_eq!(effective_timeout(&request).unwrap(), Duration::from_secs(5));

        let mut overflow = request;
        overflow.timeout_ms = u64::MAX;
        assert!(matches!(
            effective_timeout(&overflow),
            Err(RuntimeError::InvalidRequest(message)) if message.contains("overflows")
        ));
    }

    #[test]
    fn invalid_utf8_fails_closed() {
        assert!(matches!(
            decode_output("stdout", vec![0xff]),
            Err(RuntimeError::Protocol(message)) if message.contains("not valid UTF-8")
        ));
    }
}
