//! VmController - Spawns VM subprocesses.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use a3s_box_core::error::{BoxError, Result};

use super::handler::ShimHandler;
use super::spec::InstanceSpec;
use super::VmHandler;

/// Controller for spawning VM subprocesses.
///
/// Spawns the `a3s-box-shim` binary in a subprocess and returns a ShimHandler
/// for runtime operations. The subprocess isolation ensures that VM process
/// takeover doesn't affect the host application.
pub struct VmController {
    /// Path to the a3s-box-shim binary
    shim_path: PathBuf,
}

impl VmController {
    /// Create a new VmController.
    ///
    /// # Arguments
    /// * `shim_path` - Path to the a3s-box-shim binary
    ///
    /// # Returns
    /// * `Ok(VmController)` - Successfully created controller
    /// * `Err(...)` - Failed to create controller (e.g., binary not found)
    pub fn new(shim_path: PathBuf) -> Result<Self> {
        // Verify that the shim binary exists
        if !shim_path.exists() {
            return Err(BoxError::BoxBootError {
                message: format!("Shim binary not found: {}", shim_path.display()),
                hint: Some("Build the shim with: cargo build -p a3s-box-shim".to_string()),
            });
        }

        Ok(Self { shim_path })
    }

    /// Find the shim binary in common locations.
    ///
    /// Searches in order:
    /// 1. Same directory as current executable
    /// 2. target/debug or target/release (for development)
    /// 3. PATH
    pub fn find_shim() -> Result<PathBuf> {
        // Try same directory as current executable
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                let shim_path = exe_dir.join("a3s-box-shim");
                if shim_path.exists() {
                    return Ok(shim_path);
                }
            }
        }

        // Try target directories (for development)
        let target_dirs = ["target/debug", "target/release"];
        for dir in target_dirs {
            let shim_path = PathBuf::from(dir).join("a3s-box-shim");
            if shim_path.exists() {
                return Ok(shim_path);
            }
        }

        // Try PATH
        if let Ok(output) = Command::new("which").arg("a3s-box-shim").output() {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !path.is_empty() {
                    return Ok(PathBuf::from(path));
                }
            }
        }

        Err(BoxError::BoxBootError {
            message: "Could not find a3s-box-shim binary".to_string(),
            hint: Some("Build the shim with: cargo build -p a3s-box-shim".to_string()),
        })
    }

    /// Start a VM with the given configuration.
    ///
    /// Spawns the shim subprocess with the serialized InstanceSpec.
    /// Returns a handler for runtime operations on the VM.
    pub async fn start(&self, spec: &InstanceSpec) -> Result<Box<dyn VmHandler>> {
        tracing::debug!(
            box_id = %spec.box_id,
            vcpus = spec.vcpus,
            memory_mib = spec.memory_mib,
            "Starting VM subprocess"
        );

        // Serialize the config for passing to subprocess
        let config_json = serde_json::to_string(spec).map_err(|e| BoxError::BoxBootError {
            message: format!("Failed to serialize config: {}", e),
            hint: None,
        })?;

        tracing::trace!(config = %config_json, "VM configuration");

        // Clean up stale socket file if it exists
        if spec.grpc_socket_path.exists() {
            tracing::warn!(
                path = %spec.grpc_socket_path.display(),
                "Removing stale Unix socket"
            );
            let _ = std::fs::remove_file(&spec.grpc_socket_path);
        }

        // Ensure socket directory exists
        if let Some(socket_dir) = spec.grpc_socket_path.parent() {
            std::fs::create_dir_all(socket_dir).map_err(|e| BoxError::BoxBootError {
                message: format!(
                    "Failed to create socket directory {}: {}",
                    socket_dir.display(),
                    e
                ),
                hint: None,
            })?;
        }

        // Spawn shim subprocess
        tracing::info!(
            shim = %self.shim_path.display(),
            box_id = %spec.box_id,
            "Spawning shim subprocess"
        );

        let child = Command::new(&self.shim_path)
            .arg("--config")
            .arg(&config_json)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit()) // Inherit for debugging
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| BoxError::BoxBootError {
                message: format!("Failed to spawn shim: {}", e),
                hint: Some(format!("Shim path: {}", self.shim_path.display())),
            })?;

        let pid = child.id();
        tracing::info!(
            box_id = %spec.box_id,
            pid = pid,
            "Shim subprocess spawned"
        );

        // Create handler for the running VM
        let handler = ShimHandler::from_child(child, spec.box_id.clone());

        Ok(Box::new(handler))
    }
}
