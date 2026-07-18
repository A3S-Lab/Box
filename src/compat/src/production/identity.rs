use a3s_box_core::OperationId;
use uuid::Uuid;

use crate::control::{
    IdentityProviderError, IdentityProviderResult, SandboxId, SandboxIdentity,
    SandboxIdentityProvider,
};

/// Generates externally safe sandbox IDs and independent lifecycle operation IDs.
#[derive(Debug, Default)]
pub struct UuidSandboxIdentityProvider;

impl SandboxIdentityProvider for UuidSandboxIdentityProvider {
    fn next_identity(&self) -> IdentityProviderResult<SandboxIdentity> {
        let sandbox_uuid = Uuid::new_v4();
        let operation_uuid = Uuid::new_v4();
        let sandbox_id = SandboxId::new(format!("sandbox-{sandbox_uuid}"))
            .map_err(|error| unavailable(error.to_string()))?;
        let operation_id = OperationId::new(format!("e2b-create-{operation_uuid}"))
            .map_err(|error| unavailable(error.to_string()))?;
        Ok(SandboxIdentity {
            sandbox_id,
            operation_id,
        })
    }
}

fn unavailable(message: String) -> IdentityProviderError {
    IdentityProviderError::Unavailable(message)
}
