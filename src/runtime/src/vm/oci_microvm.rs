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
        let socket_dir = self.socket_dir();
        #[cfg(windows)]
        super::sandbox::refuse_directory_reparse(&socket_dir)?;
        match std::fs::remove_dir_all(&socket_dir) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(BoxError::IoError(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn cleanup_runtime_owned_microvm_bundle_does_not_delete_through_a_directory_junction() {
        use std::os::windows::fs::MetadataExt;
        use std::os::windows::process::CommandExt;

        let home = tempfile::tempdir().unwrap();
        let outside = home.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
        let box_id = "junction-microvm";
        let mut manager = VmManager::with_box_id(
            a3s_box_core::BoxConfig::default(),
            a3s_box_core::EventEmitter::new(10),
            box_id.to_string(),
        );
        manager.home_dir = home.path().to_path_buf();
        let socket_dir = manager.socket_dir();
        std::fs::create_dir_all(socket_dir.parent().unwrap()).unwrap();
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            socket_dir.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());

        let removed = manager.cleanup_runtime_owned_microvm_bundle();
        assert!(
            removed.is_err(),
            "microvm cleanup deleted through a directory junction: {removed:?}"
        );
        assert_eq!(
            std::fs::read(outside.join("secret.txt")).unwrap(),
            b"secret"
        );
        let metadata = std::fs::symlink_metadata(&socket_dir).unwrap();
        assert!(
            metadata.file_attributes() & 0x400 != 0,
            "microvm cleanup removed the directory junction"
        );
    }
}
