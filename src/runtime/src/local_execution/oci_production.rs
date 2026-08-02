//! Production Box-owned bundle preparation for the native Linux OCI service.

use std::path::{Path, PathBuf};

use a3s_box_core::{
    BoxError, ExecutionBackend, ExecutionIsolation, ExecutionManagerError, ExecutionManagerResult,
};
use a3s_oci_sdk::{CreateAttachments, IoMode, OciBundle, ProcessIo};
use async_trait::async_trait;

use super::{OciBundleProvider, OciPreparedExecution, VmLocalExecutionBackend};
use crate::sandbox::probe_sandbox_capabilities_for;
use crate::BoxRecord;

/// Prepares immutable bundles from Box image/rootfs policy while the separate
/// A3S OCI Runtime service owns container lifecycle and I/O.
#[derive(Clone)]
pub struct NativeLinuxOciBundleProvider {
    preparer: VmLocalExecutionBackend,
    runtime_path: PathBuf,
    agent_path: PathBuf,
}

impl NativeLinuxOciBundleProvider {
    pub fn new(
        home_dir: impl Into<PathBuf>,
        runtime_path: impl Into<PathBuf>,
        agent_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            preparer: VmLocalExecutionBackend::new(home_dir),
            runtime_path: runtime_path.into(),
            agent_path: agent_path.into(),
        }
    }

    pub fn runtime_path(&self) -> &Path {
        &self.runtime_path
    }

    pub fn agent_path(&self) -> &Path {
        &self.agent_path
    }

    pub fn with_pull_progress_fn(mut self, pull_progress_fn: crate::PullProgressFn) -> Self {
        self.preparer = self.preparer.with_pull_progress_fn(pull_progress_fn);
        self
    }
}

#[async_trait]
impl OciBundleProvider for NativeLinuxOciBundleProvider {
    async fn prepare(&self, record: &BoxRecord) -> ExecutionManagerResult<OciPreparedExecution> {
        if record.isolation != ExecutionIsolation::Sandbox {
            return Err(ExecutionManagerError::InvalidRequest(format!(
                "native Linux OCI migration only prepares Sandbox executions, got {:?}",
                record.isolation
            )));
        }
        let metadata = record.managed_execution.as_ref().ok_or_else(|| {
            ExecutionManagerError::Internal(format!(
                "execution {} has no managed lifecycle metadata",
                record.id
            ))
        })?;
        metadata
            .validate()
            .map_err(|error| ExecutionManagerError::Internal(error.to_string()))?;
        if metadata.plan.backend != ExecutionBackend::A3sOci {
            return Err(ExecutionManagerError::InvalidRequest(format!(
                "execution {} did not resolve to the A3S OCI Sandbox backend",
                record.id
            )));
        }

        // Re-hash the exact configured artifacts immediately before rootfs or
        // bundle mutations. Owner startup and bundle evidence use this same pair.
        let capabilities = probe_sandbox_capabilities_for(
            ExecutionBackend::A3sOci,
            Some(&self.runtime_path),
            Some(&self.agent_path),
        );
        capabilities
            .require_ready()
            .map_err(|error| preparation_error("capability preflight", error))?;

        let mut manager = self.preparer.new_oci_preparation_manager(record)?;
        let prepared = manager
            .prepare_runtime_owned_sandbox_bundle(&metadata.plan, &capabilities)
            .await
            .map_err(|error| preparation_error("prepare bundle", error))?;
        let bundle = match OciBundle::load(&prepared.bundle_dir).await {
            Ok(bundle) => bundle,
            Err(error) => {
                let cleanup = manager.cleanup_runtime_owned_sandbox_bundle();
                return Err(match cleanup {
                    Ok(()) => ExecutionManagerError::Internal(format!(
                        "failed to load the generated OCI bundle: {error}"
                    )),
                    Err(cleanup) => ExecutionManagerError::Internal(format!(
                        "failed to load the generated OCI bundle: {error}; cleanup also failed: {cleanup}"
                    )),
                });
            }
        };
        let io = ProcessIo {
            stdin: if metadata.request.config.stdin_open {
                IoMode::Pipe
            } else {
                IoMode::Null
            },
            stdout: IoMode::Capture,
            stderr: IoMode::Capture,
            terminal_size: None,
        };
        let attachments = match CreateAttachments::from_bundle(&bundle, io) {
            Ok(attachments) => attachments,
            Err(error) => {
                return Err(cleanup_after_prepare_failure(
                    &manager,
                    format!("failed to derive generated OCI bundle attachments: {error}"),
                ));
            }
        };
        let mut result = match OciPreparedExecution::with_attachments(
            bundle,
            attachments,
            prepared.console_output,
        ) {
            Ok(result) => result,
            Err(error) => {
                return Err(cleanup_after_prepare_failure(
                    &manager,
                    format!("failed to validate generated OCI bundle attachments: {error}"),
                ));
            }
        };
        result.anonymous_volumes = prepared.anonymous_volumes;
        Ok(result)
    }

    async fn cleanup(&self, record: &BoxRecord) -> ExecutionManagerResult<()> {
        let manager = self.preparer.new_oci_preparation_manager(record)?;
        manager
            .cleanup_runtime_owned_sandbox_bundle()
            .map_err(|error| preparation_error("cleanup bundle", error))
    }
}

fn preparation_error(action: &str, error: BoxError) -> ExecutionManagerError {
    match error {
        BoxError::ConfigError(message) => ExecutionManagerError::InvalidRequest(message),
        error => {
            ExecutionManagerError::Unavailable(format!("native Linux OCI {action} failed: {error}"))
        }
    }
}

fn cleanup_after_prepare_failure(
    manager: &crate::VmManager,
    message: String,
) -> ExecutionManagerError {
    match manager.cleanup_runtime_owned_sandbox_bundle() {
        Ok(()) => ExecutionManagerError::Internal(message),
        Err(cleanup) => {
            ExecutionManagerError::Internal(format!("{message}; cleanup also failed: {cleanup}"))
        }
    }
}
