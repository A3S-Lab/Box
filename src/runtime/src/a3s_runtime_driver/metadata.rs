//! Stable provider ownership metadata and fail-closed record discovery.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use a3s_box_core::{ExecutionGeneration, ExecutionId, OperationId};
use a3s_runtime::contract::{RuntimeObservation, RuntimeUnitSpec, RuntimeUnitState};
use a3s_runtime::{RuntimeError, RuntimeResult};
use sha2::{Digest, Sha256};

use crate::{BoxRecord, ManagedExecutionState};

use super::mapping::{creation_request, labels_as_hash_map};
use super::BoxRuntimeDriver;

pub(super) const MANAGED_LABEL: &str = "a3s.runtime.managed";
pub(super) const PROVIDER_LABEL: &str = "a3s.runtime.provider";
pub(super) const UNIT_LABEL: &str = "a3s.runtime.unit-id";
pub(super) const GENERATION_LABEL: &str = "a3s.runtime.generation";
pub(super) const SPEC_DIGEST_LABEL: &str = "a3s.runtime.spec-digest";
const PROVIDER_VALUE: &str = "a3s-box";

pub(super) fn managed_labels(
    spec: &RuntimeUnitSpec,
    spec_digest: &str,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        (MANAGED_LABEL.into(), "true".into()),
        (PROVIDER_LABEL.into(), PROVIDER_VALUE.into()),
        (UNIT_LABEL.into(), spec.unit_id.clone()),
        (GENERATION_LABEL.into(), spec.generation.to_string()),
        (SPEC_DIGEST_LABEL.into(), spec_digest.into()),
    ])
}

pub(super) fn operation_id(
    unit_id: &str,
    generation: u64,
    spec_digest: &str,
) -> RuntimeResult<OperationId> {
    let mut digest = Sha256::new();
    digest.update(b"a3s-box-runtime-operation-v1\0");
    digest.update((unit_id.len() as u64).to_be_bytes());
    digest.update(unit_id.as_bytes());
    digest.update(generation.to_be_bytes());
    digest.update(spec_digest.as_bytes());
    OperationId::new(format!("a3s-runtime-box-v1:{:x}", digest.finalize()))
        .map_err(|error| RuntimeError::InvalidRequest(error.to_string()))
}

impl BoxRuntimeDriver {
    pub(super) async fn find_generation(
        &self,
        spec: &RuntimeUnitSpec,
    ) -> RuntimeResult<Option<BoxRecord>> {
        let records = self
            .manager
            .managed_records()
            .await
            .map_err(|error| map_execution_error(&spec.unit_id, error))?;
        let matches = records
            .into_iter()
            .filter(|record| {
                record.labels.get(UNIT_LABEL) == Some(&spec.unit_id)
                    && record.labels.get(GENERATION_LABEL) == Some(&spec.generation.to_string())
            })
            .collect::<Vec<_>>();
        if matches.len() > 1 {
            return Err(RuntimeError::Protocol(format!(
                "Box has multiple managed executions for unit {:?} generation {}",
                spec.unit_id, spec.generation
            )));
        }
        let Some(record) = matches.into_iter().next() else {
            return Ok(None);
        };
        validate_record_for_spec(&record, spec)?;
        Ok(Some(record))
    }

    pub(super) async fn unit_records(&self, unit_id: &str) -> RuntimeResult<Vec<BoxRecord>> {
        let records = self
            .manager
            .managed_records()
            .await
            .map_err(|error| map_execution_error(unit_id, error))?;
        let mut matches = records
            .into_iter()
            .filter(|record| {
                record
                    .labels
                    .get(UNIT_LABEL)
                    .is_some_and(|value| value == unit_id)
            })
            .collect::<Vec<_>>();
        for record in &matches {
            validate_owned_record(record, unit_id)?;
        }
        matches.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(matches)
    }
}

pub(super) fn validate_record_for_spec(
    record: &BoxRecord,
    spec: &RuntimeUnitSpec,
) -> RuntimeResult<()> {
    validate_owned_record(record, &spec.unit_id)?;
    let expected_request = creation_request(spec)?;
    let metadata = record.managed_execution.as_ref().ok_or_else(|| {
        RuntimeError::Protocol(format!("Box execution {} lost managed metadata", record.id))
    })?;
    let actual_request = serde_json::to_value(&metadata.request)
        .map_err(|error| RuntimeError::Protocol(error.to_string()))?;
    let expected_request = serde_json::to_value(&expected_request)
        .map_err(|error| RuntimeError::Protocol(error.to_string()))?;
    if actual_request != expected_request {
        return Err(RuntimeError::Protocol(format!(
            "Box execution {} creation intent does not match the Runtime specification",
            record.id
        )));
    }
    Ok(())
}

pub(super) fn validate_owned_record(record: &BoxRecord, unit_id: &str) -> RuntimeResult<()> {
    let metadata = record.managed_execution.as_ref().ok_or_else(|| {
        RuntimeError::Protocol(format!("Box execution {} is not managed", record.id))
    })?;
    metadata
        .validate()
        .map_err(|error| RuntimeError::Protocol(error.to_string()))?;
    let expected = [
        (MANAGED_LABEL, "true"),
        (PROVIDER_LABEL, PROVIDER_VALUE),
        (UNIT_LABEL, unit_id),
    ];
    for (key, value) in expected {
        if record.labels.get(key).map(String::as_str) != Some(value)
            || metadata.request.labels.get(key).map(String::as_str) != Some(value)
        {
            return Err(RuntimeError::Protocol(format!(
                "Box execution {} metadata {key:?} does not match Runtime ownership",
                record.id
            )));
        }
    }
    let generation = label_generation(record)?;
    let spec_digest = record.labels.get(SPEC_DIGEST_LABEL).ok_or_else(|| {
        RuntimeError::Protocol(format!(
            "Box execution {} has no Runtime specification digest",
            record.id
        ))
    })?;
    let expected_operation = operation_id(unit_id, generation, spec_digest)?;
    if metadata.operation_id != expected_operation
        || labels_as_hash_map(&metadata.request.labels) != record.labels
    {
        return Err(RuntimeError::Protocol(format!(
            "Box execution {} has inconsistent managed identity",
            record.id
        )));
    }
    Ok(())
}

pub(super) fn label_generation(record: &BoxRecord) -> RuntimeResult<u64> {
    record
        .labels
        .get(GENERATION_LABEL)
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            RuntimeError::Protocol(format!(
                "Box execution {} has an invalid Runtime generation",
                record.id
            ))
        })
}

pub(super) fn local_identity(
    record: &BoxRecord,
) -> RuntimeResult<(ExecutionId, ExecutionGeneration, ManagedExecutionState)> {
    let execution_id = ExecutionId::new(record.id.clone())
        .map_err(|error| RuntimeError::Protocol(error.to_string()))?;
    let metadata = record.managed_execution.as_ref().ok_or_else(|| {
        RuntimeError::Protocol(format!("Box execution {} lost managed metadata", record.id))
    })?;
    let state = record
        .managed_state()
        .map_err(|error| RuntimeError::Protocol(error.to_string()))?
        .ok_or_else(|| {
            RuntimeError::Protocol(format!("Box execution {} is unmanaged", record.id))
        })?;
    Ok((execution_id, metadata.generation, state))
}

pub(super) fn provider_identity_matches(
    current: &RuntimeObservation,
    record: &BoxRecord,
) -> RuntimeResult<()> {
    if current.state != RuntimeUnitState::Unknown
        && current
            .provider_resource_id
            .as_deref()
            .is_some_and(|provider_id| provider_id != record.id)
    {
        return Err(RuntimeError::Protocol(format!(
            "Runtime observation for {:?} is bound to a different Box execution",
            current.unit_id
        )));
    }
    Ok(())
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

pub(super) fn timestamp_ms(value: chrono::DateTime<chrono::Utc>) -> RuntimeResult<u64> {
    u64::try_from(value.timestamp_millis()).map_err(|_| {
        RuntimeError::Protocol("Box execution timestamp precedes the Unix epoch".into())
    })
}

pub(super) fn map_execution_error(
    unit_id: &str,
    error: a3s_box_core::ExecutionManagerError,
) -> RuntimeError {
    match error {
        a3s_box_core::ExecutionManagerError::InvalidRequest(message) => {
            RuntimeError::InvalidRequest(message)
        }
        a3s_box_core::ExecutionManagerError::NotFound(_) => RuntimeError::NotFound {
            unit_id: unit_id.into(),
        },
        a3s_box_core::ExecutionManagerError::Conflict { message, .. } => {
            RuntimeError::ProviderUnavailable(format!("Box execution conflict: {message}"))
        }
        a3s_box_core::ExecutionManagerError::Unavailable(message) => {
            RuntimeError::ProviderUnavailable(message)
        }
        a3s_box_core::ExecutionManagerError::Internal(message) => RuntimeError::Protocol(message),
    }
}
