//! E2B-style local Sandbox API backed directly by the A3S Box runtime.
//!
//! This module never reads endpoint or API-key environment variables. The
//! default constructor opens the installed local runtime state directly.

mod builder;
mod commands;
mod filesystem;
mod lifecycle;
mod options;
mod script;

use std::sync::{Arc, RwLock};

use a3s_box_core::{
    ExecutionGeneration, ExecutionId, ExecutionIsolation, ExecutionSnapshot, ExecutionSnapshotId,
    ExecutionState, ExecutionStatus,
};

pub use builder::SandboxBuilder;
pub use commands::{CommandResult, CommandRunOptions, Commands, SandboxCommand};
pub use filesystem::{Filesystem, FilesystemOptions, WriteInfo};
pub use lifecycle::{SandboxLogOptions, SandboxRestartOptions};
pub use options::{
    SandboxCreateOptions, SandboxNetwork, TmpfsMount, VolumeMount, VolumeSource,
    DEFAULT_SANDBOX_IMAGE, DEFAULT_SANDBOX_TIMEOUT_SECONDS,
};
pub use script::ScriptBuilder;

use crate::{A3sBoxClient, ClientError, Result};

#[derive(Debug, Clone, Copy)]
struct SandboxState {
    generation: ExecutionGeneration,
    state: ExecutionState,
    closed: bool,
}

pub(crate) struct SandboxInner {
    client: A3sBoxClient,
    execution_id: ExecutionId,
    isolation: ExecutionIsolation,
    state: RwLock<SandboxState>,
}

impl SandboxInner {
    fn state(&self) -> SandboxState {
        *self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn update(&self, generation: ExecutionGeneration, state: ExecutionState) {
        let mut current = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        current.generation = generation;
        current.state = state;
    }

    fn close(&self, generation: ExecutionGeneration) {
        let mut current = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        current.generation = generation;
        current.state = ExecutionState::Stopped;
        current.closed = true;
    }

    pub(crate) fn active_execution(&self) -> Result<(ExecutionId, ExecutionGeneration)> {
        let state = self.state();
        if state.closed || state.state != ExecutionState::Running {
            return Err(ClientError::Validation(format!(
                "sandbox {} is not running",
                self.execution_id
            )));
        }
        Ok((self.execution_id.clone(), state.generation))
    }
}

/// Current local Sandbox identity and runtime state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxInfo {
    pub sandbox_id: String,
    pub generation: u64,
    pub state: ExecutionState,
    pub isolation: ExecutionIsolation,
}

/// A zero-configuration local A3S Box handle with an E2B-style surface.
///
/// `Sandbox::create` uses MicroVM isolation by default. Pass
/// [`ExecutionIsolation::Sandbox`] through [`SandboxCreateOptions`] to opt into
/// the lower-overhead shared-kernel backend on a certified Linux host.
#[derive(Clone)]
pub struct Sandbox {
    inner: Arc<SandboxInner>,
    pub commands: Commands,
    pub files: Filesystem,
}

impl std::fmt::Debug for Sandbox {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Sandbox")
            .field("info", &self.info())
            .finish_non_exhaustive()
    }
}

impl Sandbox {
    /// Create a local MicroVM Sandbox from an OCI image.
    pub async fn create(image: impl Into<String>) -> Result<Self> {
        Self::create_with_options(SandboxCreateOptions::new(image)).await
    }

    /// Create a local Sandbox with explicit runtime and isolation options.
    pub async fn create_with_options(options: SandboxCreateOptions) -> Result<Self> {
        Self::create_with_client(A3sBoxClient::new(), options).await
    }

    /// Create a local Sandbox through an explicitly supplied typed client.
    pub async fn create_with_client(
        client: A3sBoxClient,
        options: SandboxCreateOptions,
    ) -> Result<Self> {
        let isolation = options.isolation;
        let (request, operation) = options.into_runtime_request(&client)?;
        let lease = client.run_box(request, &operation).await?;
        Ok(Self::from_known_state(
            client,
            lease.execution_id,
            lease.generation,
            ExecutionState::Running,
            isolation,
        ))
    }

    /// Reconnect to an existing local Sandbox without credentials.
    pub async fn connect(sandbox_id: impl Into<String>) -> Result<Self> {
        Self::connect_with_client(A3sBoxClient::new(), sandbox_id).await
    }

    /// Reconnect through an explicitly supplied typed client.
    pub async fn connect_with_client(
        client: A3sBoxClient,
        sandbox_id: impl Into<String>,
    ) -> Result<Self> {
        let execution_id = ExecutionId::new(sandbox_id.into())
            .map_err(|error| ClientError::Validation(error.to_string()))?;
        let status = client.inspect_execution(&execution_id).await?;
        Ok(Self::from_status(client, status))
    }

    fn from_status(client: A3sBoxClient, status: ExecutionStatus) -> Self {
        Self::from_known_state(
            client,
            status.execution_id,
            status.generation,
            status.state,
            status.plan.requested_isolation,
        )
    }

    pub(crate) fn from_known_state(
        client: A3sBoxClient,
        execution_id: ExecutionId,
        generation: ExecutionGeneration,
        state: ExecutionState,
        isolation: ExecutionIsolation,
    ) -> Self {
        let inner = Arc::new(SandboxInner {
            client,
            execution_id,
            isolation,
            state: RwLock::new(SandboxState {
                generation,
                state,
                closed: false,
            }),
        });
        Self {
            commands: Commands {
                inner: Arc::clone(&inner),
            },
            files: Filesystem {
                inner: Arc::clone(&inner),
            },
            inner,
        }
    }

    pub fn id(&self) -> &str {
        self.inner.execution_id.as_str()
    }

    pub fn info(&self) -> SandboxInfo {
        let state = self.inner.state();
        SandboxInfo {
            sandbox_id: self.id().to_string(),
            generation: state.generation.get(),
            state: state.state,
            isolation: self.inner.isolation,
        }
    }

    pub fn isolation(&self) -> ExecutionIsolation {
        self.inner.isolation
    }

    /// Build an explicitly interpreted script execution.
    pub fn script(&self, source: impl AsRef<[u8]>) -> ScriptBuilder {
        self.commands.script(source)
    }

    /// Capture this running or paused Sandbox filesystem without changing its
    /// final lifecycle state.
    pub async fn create_filesystem_snapshot(
        &self,
        snapshot_id: ExecutionSnapshotId,
    ) -> Result<ExecutionSnapshot> {
        let state = self.inner.state();
        if state.closed
            || !matches!(
                state.state,
                ExecutionState::Running | ExecutionState::Paused
            )
        {
            return Err(ClientError::Validation(format!(
                "sandbox {} cannot be snapshotted while it is {:?}",
                self.id(),
                state.state
            )));
        }
        let snapshot = self
            .inner
            .client
            .create_execution_snapshot(&self.inner.execution_id, state.generation, &snapshot_id)
            .await?;
        self.inner.update(snapshot.lease.generation, snapshot.state);
        Ok(snapshot)
    }

    pub async fn pause(&self, keep_memory: bool) -> Result<()> {
        let (_, generation) = self.inner.active_execution()?;
        let lease = self
            .inner
            .client
            .pause_execution(&self.inner.execution_id, generation, keep_memory)
            .await?;
        self.inner.update(lease.generation, ExecutionState::Paused);
        Ok(())
    }

    pub async fn resume(&self) -> Result<()> {
        let state = self.inner.state();
        if state.closed {
            return Err(ClientError::Validation(format!(
                "sandbox {} has been killed",
                self.id()
            )));
        }
        let lease = self
            .inner
            .client
            .resume_execution(&self.inner.execution_id, state.generation)
            .await?;
        self.inner.update(lease.generation, ExecutionState::Running);
        Ok(())
    }

    pub async fn is_running(&self) -> Result<bool> {
        if self.inner.state().closed {
            return Ok(false);
        }
        match self
            .inner
            .client
            .inspect_execution(&self.inner.execution_id)
            .await
        {
            Ok(status) => {
                self.inner.update(status.generation, status.state);
                Ok(status.state == ExecutionState::Running)
            }
            Err(ClientError::Execution(a3s_box_core::ExecutionManagerError::NotFound(_)))
            | Err(ClientError::BoxNotFound(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Kill the local execution and remove its runtime-owned record/resources.
    pub async fn kill(&self) -> Result<()> {
        self.stop().await?;
        self.remove().await
    }
}

#[cfg(test)]
mod tests;
