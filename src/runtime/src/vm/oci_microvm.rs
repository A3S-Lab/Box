//! Box-owned preparation for a portable dedicated-VM OCI bundle.

use std::path::{Path, PathBuf};

use a3s_box_core::{BoxError, ExecutionBackend, ResolvedExecutionPlan, Result};

use super::VmManager;
use crate::sandbox::{compile_portable_microvm_oci_spec, SandboxRuntimeProcess};

/// Portable handoff produced without starting a Box-owned VM.
#[derive(Debug, Clone)]
pub(crate) struct RuntimeOwnedMicrovmBundle {
    pub bundle_dir: PathBuf,
    pub console_output: PathBuf,
    pub anonymous_volumes: Vec<String>,
}

impl VmManager {
    /// Resolve the image process, copy the fresh rootfs, and publish one exact handoff bundle.
    pub(crate) async fn prepare_runtime_owned_microvm_bundle(
        &mut self,
        execution_plan: &ResolvedExecutionPlan,
        bundle_directory: &Path,
    ) -> Result<RuntimeOwnedMicrovmBundle> {
        if execution_plan.backend != ExecutionBackend::Krun {
            return Err(BoxError::BoxBootError {
                message: "portable MicroVM OCI preparation requires the Krun execution plan"
                    .to_string(),
                hint: None,
            });
        }

        let original_anonymous_volumes = self.anonymous_volumes.clone();
        let box_dir = self.home_dir.join("boxes").join(&self.box_id);
        let layout = match self.prepare_layout().await {
            Ok(layout) => layout,
            Err(error) => {
                self.cleanup_boot_failure().await;
                return Err(error);
            }
        };
        self.image_config = layout.oci_config.clone();

        let prepare = (|| -> Result<RuntimeOwnedMicrovmBundle> {
            let instance_spec = self.build_runtime_owned_instance_spec(&layout)?;
            if self.anonymous_volumes != original_anonymous_volumes {
                return Err(BoxError::ConfigError(
                    "portable MicroVM OCI qualification does not support image-declared volumes"
                        .to_string(),
                ));
            }
            let runtime_process: SandboxRuntimeProcess =
                crate::vm::sandbox::resolve_runtime_owned_process(
                    &layout.rootfs_path,
                    &instance_spec,
                    &self.config.cap_drop,
                )?;
            let hostname = self
                .config
                .hostname
                .clone()
                .unwrap_or_else(|| self.box_id.clone());
            let spec =
                compile_portable_microvm_oci_spec(&self.box_id, &hostname, &runtime_process)?;
            crate::local_execution::oci_portable_rootfs::publish_portable_bundle(
                &layout.rootfs_path,
                &spec,
                bundle_directory,
            )?;

            Ok(RuntimeOwnedMicrovmBundle {
                bundle_dir: bundle_directory.to_path_buf(),
                console_output: instance_spec
                    .console_output
                    .unwrap_or_else(|| box_dir.join("logs").join("console.log")),
                anonymous_volumes: self.anonymous_volumes.clone(),
            })
        })();

        match prepare {
            Ok(prepared) => Ok(prepared),
            Err(error) => {
                self.cleanup_boot_failure().await;
                Err(error)
            }
        }
    }

    /// Remove only Box-owned rootfs and socket preparation after runtime deletion.
    pub(crate) fn cleanup_runtime_owned_microvm_bundle(&self) -> Result<()> {
        let box_dir = self.home_dir.join("boxes").join(&self.box_id);
        self.rootfs_provider.cleanup(&box_dir, false)?;
        match std::fs::remove_dir_all(self.socket_dir()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(BoxError::IoError(error)),
        }
    }
}
