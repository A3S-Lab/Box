use super::{LifecycleError, LifecycleState, SandboxRecord};

pub(crate) fn validate_persisted_record(record: &SandboxRecord) -> Result<(), LifecycleError> {
    if record.owner_id().trim().is_empty()
        || record.template_id().trim().is_empty()
        || record.envd_version().trim().is_empty()
    {
        return Err(LifecycleError::InvalidPersistedState(
            "owner ID, template ID, and envd version must be non-empty".to_string(),
        ));
    }
    if record.expires_at() < record.created_at() {
        return Err(LifecycleError::InvalidExpiry);
    }
    record.routing().validate().map_err(|error| {
        LifecycleError::InvalidPersistedState(format!("invalid route policy: {error}"))
    })?;
    if record.execution_id().is_some() != record.execution_generation().is_some() {
        return Err(LifecycleError::InvalidPersistedState(
            "execution ID and generation must be present together".to_string(),
        ));
    }
    let requires_execution = matches!(
        record.state(),
        LifecycleState::Running
            | LifecycleState::Pausing
            | LifecycleState::Paused
            | LifecycleState::Resuming
    );
    if requires_execution && record.execution_id().is_none() {
        return Err(LifecycleError::InvalidPersistedState(format!(
            "state {:?} requires a runtime execution",
            record.state()
        )));
    }
    if record.execution_id().is_some() && record.started_at().is_none() {
        return Err(LifecycleError::InvalidPersistedState(
            "a runtime execution requires a start timestamp".to_string(),
        ));
    }
    if record
        .started_at()
        .is_some_and(|started_at| started_at < record.created_at())
    {
        return Err(LifecycleError::InvalidPersistedState(
            "sandbox start timestamp precedes creation".to_string(),
        ));
    }
    if record
        .started_at()
        .is_some_and(|started_at| record.expires_at() < started_at)
    {
        return Err(LifecycleError::InvalidPersistedState(
            "sandbox expiry precedes readiness".to_string(),
        ));
    }
    if record.state() == LifecycleState::Failed && record.failure().is_none() {
        return Err(LifecycleError::InvalidPersistedState(
            "failed sandbox is missing its failure category".to_string(),
        ));
    }
    if record.failure().is_some()
        && !matches!(
            record.state(),
            LifecycleState::Failed | LifecycleState::Killing | LifecycleState::Killed
        )
    {
        return Err(LifecycleError::InvalidPersistedState(
            "non-failed sandbox retains a failure category".to_string(),
        ));
    }
    Ok(())
}
