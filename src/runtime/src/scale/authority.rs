//! Durable desired-state authority for the standalone Gateway scale contract.

use std::path::{Path, PathBuf};

use a3s_box_core::scale::{
    ScaleObservation, ScaleOperationConflict, ScaleOperationRequest, ScaleOperationResponse,
};
use thiserror::Error;

use super::manager::ScaleAuthorityState;
use super::ScaleManager;

#[derive(Debug, Error)]
pub enum ScaleAuthorityError {
    #[error("scale operation conflict: {0}")]
    Conflict(String, ScaleOperationConflict),
    #[error("scale authority state error: {0}")]
    State(String),
}

impl ScaleAuthorityError {
    pub fn conflict(&self) -> Option<&ScaleOperationConflict> {
        match self {
            Self::Conflict(_, conflict) => Some(conflict),
            Self::State(_) => None,
        }
    }
}

pub struct DurableScaleAuthority {
    path: PathBuf,
    manager: ScaleManager,
}

impl DurableScaleAuthority {
    pub fn open(path: impl Into<PathBuf>, max_instances: u32) -> Result<Self, ScaleAuthorityError> {
        let path = path.into();
        let mut manager = ScaleManager::new(max_instances);
        if path.exists() {
            let bytes = std::fs::read(&path).map_err(|error| {
                ScaleAuthorityError::State(format!("failed to read {}: {error}", path.display()))
            })?;
            let state: ScaleAuthorityState = serde_json::from_slice(&bytes).map_err(|error| {
                ScaleAuthorityError::State(format!("failed to parse {}: {error}", path.display()))
            })?;
            manager
                .restore_authority_state(state)
                .map_err(ScaleAuthorityError::State)?;
        }
        Ok(Self { path, manager })
    }

    pub fn observation(&self, service: &str) -> ScaleObservation {
        self.manager.scale_observation(service)
    }

    pub fn apply(
        &mut self,
        request: &ScaleOperationRequest,
    ) -> Result<ScaleOperationResponse, ScaleAuthorityError> {
        let previous = self.manager.authority_state();
        let response = self.manager.apply_operation(request).map_err(|conflict| {
            ScaleAuthorityError::Conflict(conflict.message.clone(), conflict)
        })?;
        if let Err(error) = persist(&self.path, &self.manager.authority_state()) {
            if let Err(rollback) = self.manager.restore_authority_state(previous) {
                return Err(ScaleAuthorityError::State(format!(
                    "{error}; failed to restore in-memory state: {rollback}"
                )));
            }
            return Err(error);
        }
        Ok(response)
    }

    pub fn finalize(
        &mut self,
        request: &ScaleOperationRequest,
        response: ScaleOperationResponse,
    ) -> Result<ScaleOperationResponse, ScaleAuthorityError> {
        let previous = self.manager.authority_state();
        self.manager
            .finalize_operation_response(request, response.clone())
            .map_err(ScaleAuthorityError::State)?;
        if let Err(error) = persist(&self.path, &self.manager.authority_state()) {
            if let Err(rollback) = self.manager.restore_authority_state(previous) {
                return Err(ScaleAuthorityError::State(format!(
                    "{error}; failed to restore in-memory state: {rollback}"
                )));
            }
            return Err(error);
        }
        Ok(response)
    }
}

fn persist(path: &Path, state: &ScaleAuthorityState) -> Result<(), ScaleAuthorityError> {
    let parent = path.parent().ok_or_else(|| {
        ScaleAuthorityError::State(format!("{} has no parent directory", path.display()))
    })?;
    std::fs::create_dir_all(parent).map_err(|error| {
        ScaleAuthorityError::State(format!("failed to create {}: {error}", parent.display()))
    })?;
    let bytes = serde_json::to_vec(state).map_err(|error| {
        ScaleAuthorityError::State(format!("failed to encode scale authority: {error}"))
    })?;
    let temporary = path.with_extension("tmp");
    a3s_box_core::fs_atomic::write_durable(&temporary, path, &bytes).map_err(|error| {
        ScaleAuthorityError::State(format!("failed to persist {}: {error}", path.display()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::scale::{ScaleDirection, SCALE_OPERATION_SCHEMA_VERSION};

    fn request(id: &str, revision: &str, current: u32, desired: u32) -> ScaleOperationRequest {
        ScaleOperationRequest {
            schema_version: SCALE_OPERATION_SCHEMA_VERSION,
            operation_id: id.to_string(),
            service: "api".to_string(),
            expected_revision: Some(revision.to_string()),
            direction: if desired > current {
                ScaleDirection::Up
            } else {
                ScaleDirection::Down
            },
            current_replicas: current,
            desired_replicas: desired,
            reason: "fixture load".to_string(),
        }
    }

    #[test]
    fn restart_retains_revision_and_exact_operation_replay() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("scale-authority.json");
        let operation = request("scale-v1-restart", "0", 0, 2);
        let accepted = {
            let mut authority = DurableScaleAuthority::open(&path, 10).unwrap();
            authority.apply(&operation).unwrap()
        };

        let mut reopened = DurableScaleAuthority::open(&path, 10).unwrap();
        assert_eq!(reopened.observation("api").replicas, 2);
        assert_eq!(reopened.observation("api").revision.as_deref(), Some("1"));
        assert_eq!(reopened.apply(&operation).unwrap(), accepted);

        let conflict = reopened
            .apply(&request("scale-v1-stale", "0", 0, 3))
            .unwrap_err();
        assert_eq!(conflict.conflict().unwrap().code, "stale_revision");
    }

    #[test]
    fn restart_replays_the_durable_reconciled_response() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("scale-authority.json");
        let operation = request("scale-v1-finalized", "0", 0, 2);
        let finalized = {
            let mut authority = DurableScaleAuthority::open(&path, 10).unwrap();
            let accepted = authority.apply(&operation).unwrap();
            authority
                .finalize(
                    &operation,
                    ScaleOperationResponse {
                        actual_replicas: 1,
                        message: "Box reconciled one ready replica".to_string(),
                        ..accepted
                    },
                )
                .unwrap()
        };

        let mut reopened = DurableScaleAuthority::open(&path, 10).unwrap();
        assert_eq!(reopened.apply(&operation).unwrap(), finalized);
        assert_eq!(finalized.actual_replicas, 1);
        assert_eq!(finalized.revision.as_deref(), Some("1"));
    }

    #[test]
    fn corrupt_journal_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("scale-authority.json");
        std::fs::write(&path, b"not-json").unwrap();

        let error = match DurableScaleAuthority::open(path, 10) {
            Ok(_) => panic!("corrupt journal must fail closed"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("failed to parse"));
    }

    #[test]
    fn persistence_failure_rolls_back_in_memory_transition() {
        let directory = tempfile::tempdir().unwrap();
        let blocked_parent = directory.path().join("not-a-directory");
        std::fs::write(&blocked_parent, b"file").unwrap();
        let mut authority =
            DurableScaleAuthority::open(blocked_parent.join("state.json"), 10).unwrap();

        let error = authority
            .apply(&request("scale-v1-fail", "0", 0, 2))
            .unwrap_err();

        assert!(error.conflict().is_none());
        assert_eq!(authority.observation("api").replicas, 0);
        assert_eq!(authority.observation("api").revision.as_deref(), Some("0"));
    }
}
