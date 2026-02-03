//! VM Manager - Lifecycle management for MicroVM instances.

use std::path::PathBuf;
use std::sync::Arc;

use a3s_box_core::config::BoxConfig;
use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::event::{BoxEvent, EventEmitter};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::rootfs::{GUEST_AGENT_PATH, GUEST_WORKDIR};
use crate::vmm::{Entrypoint, FsMount, InstanceSpec, VmController, VmHandler};
use crate::AGENT_VSOCK_PORT;

/// Box state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoxState {
    /// Config captured, no VM started
    Created,

    /// VM booted, agent initialized, gRPC healthy
    Ready,

    /// A session is actively processing a prompt
    Busy,

    /// A session is compressing its context
    Compacting,

    /// VM terminated, resources freed
    Stopped,
}

/// Layout of directories for a box instance.
struct BoxLayout {
    /// Path to the root filesystem
    rootfs_path: PathBuf,
    /// Path to the gRPC Unix socket
    socket_path: PathBuf,
    /// Path to the workspace directory
    workspace_path: PathBuf,
    /// Path to the skills directory
    skills_path: PathBuf,
    /// Path to console output file (optional)
    console_output: Option<PathBuf>,
}

/// VM manager - orchestrates VM lifecycle.
pub struct VmManager {
    /// Box configuration
    config: BoxConfig,

    /// Unique box identifier
    box_id: String,

    /// Current state
    state: Arc<RwLock<BoxState>>,

    /// Event emitter
    event_emitter: EventEmitter,

    /// VM controller (spawns shim subprocess)
    controller: Option<VmController>,

    /// VM handler (runtime operations on running VM)
    handler: Arc<RwLock<Option<Box<dyn VmHandler>>>>,

    /// A3S home directory (~/.a3s)
    home_dir: PathBuf,
}

impl VmManager {
    /// Create a new VM manager.
    pub fn new(config: BoxConfig, event_emitter: EventEmitter) -> Self {
        let box_id = uuid::Uuid::new_v4().to_string();
        let home_dir = dirs_home().unwrap_or_else(|| PathBuf::from(".a3s"));

        Self {
            config,
            box_id,
            state: Arc::new(RwLock::new(BoxState::Created)),
            event_emitter,
            controller: None,
            handler: Arc::new(RwLock::new(None)),
            home_dir,
        }
    }

    /// Create a new VM manager with a specific box ID.
    pub fn with_box_id(config: BoxConfig, event_emitter: EventEmitter, box_id: String) -> Self {
        let home_dir = dirs_home().unwrap_or_else(|| PathBuf::from(".a3s"));

        Self {
            config,
            box_id,
            state: Arc::new(RwLock::new(BoxState::Created)),
            event_emitter,
            controller: None,
            handler: Arc::new(RwLock::new(None)),
            home_dir,
        }
    }

    /// Get the box ID.
    pub fn box_id(&self) -> &str {
        &self.box_id
    }

    /// Get current state.
    pub async fn state(&self) -> BoxState {
        *self.state.read().await
    }

    /// Boot the VM.
    pub async fn boot(&mut self) -> Result<()> {
        let mut state = self.state.write().await;

        if *state != BoxState::Created {
            return Err(BoxError::Other("VM already booted".to_string()));
        }

        tracing::info!(box_id = %self.box_id, "Booting VM");

        // 1. Prepare filesystem layout
        let layout = self.prepare_layout()?;

        // 2. Build InstanceSpec
        let spec = self.build_instance_spec(&layout)?;

        // 3. Initialize controller
        let shim_path = VmController::find_shim()?;
        let controller = VmController::new(shim_path)?;
        self.controller = Some(controller);

        // 4. Start VM via controller
        let handler = self.controller.as_ref().unwrap().start(&spec).await?;

        // Store handler
        *self.handler.write().await = Some(handler);

        // 5. Wait for guest ready (gRPC health check)
        self.wait_for_guest_ready(&layout.socket_path).await?;

        // 6. Update state
        *state = BoxState::Ready;

        // Emit ready event
        self.event_emitter.emit(BoxEvent::empty("box.ready"));

        tracing::info!(box_id = %self.box_id, "VM ready");

        Ok(())
    }

    /// Destroy the VM.
    pub async fn destroy(&mut self) -> Result<()> {
        let mut state = self.state.write().await;

        if *state == BoxState::Stopped {
            return Ok(());
        }

        tracing::info!(box_id = %self.box_id, "Destroying VM");

        // Stop the VM handler
        if let Some(mut handler) = self.handler.write().await.take() {
            handler.stop()?;
        }

        *state = BoxState::Stopped;

        // Emit stopped event
        self.event_emitter.emit(BoxEvent::empty("box.stopped"));

        Ok(())
    }

    /// Transition to busy state.
    pub async fn set_busy(&self) -> Result<()> {
        let mut state = self.state.write().await;

        if *state != BoxState::Ready {
            return Err(BoxError::Other("VM not ready".to_string()));
        }

        *state = BoxState::Busy;
        Ok(())
    }

    /// Transition back to ready state.
    pub async fn set_ready(&self) -> Result<()> {
        let mut state = self.state.write().await;

        if *state != BoxState::Busy && *state != BoxState::Compacting {
            return Err(BoxError::Other("Invalid state transition".to_string()));
        }

        *state = BoxState::Ready;
        Ok(())
    }

    /// Transition to compacting state.
    pub async fn set_compacting(&self) -> Result<()> {
        let mut state = self.state.write().await;

        if *state != BoxState::Busy {
            return Err(BoxError::Other("VM not busy".to_string()));
        }

        *state = BoxState::Compacting;
        Ok(())
    }

    /// Check if VM is healthy.
    pub async fn health_check(&self) -> Result<bool> {
        let state = self.state.read().await;

        match *state {
            BoxState::Ready | BoxState::Busy | BoxState::Compacting => {
                // Check if handler reports VM is running
                if let Some(ref handler) = *self.handler.read().await {
                    Ok(handler.is_running())
                } else {
                    Ok(false)
                }
            }
            _ => Ok(false),
        }
    }

    /// Get VM metrics.
    pub async fn metrics(&self) -> Option<crate::vmm::VmMetrics> {
        self.handler
            .read()
            .await
            .as_ref()
            .map(|handler| handler.metrics())
    }

    /// Prepare the filesystem layout for the VM.
    fn prepare_layout(&self) -> Result<BoxLayout> {
        // Create box-specific directories
        let box_dir = self.home_dir.join("boxes").join(&self.box_id);
        let socket_dir = box_dir.join("sockets");
        let logs_dir = box_dir.join("logs");

        std::fs::create_dir_all(&socket_dir).map_err(|e| BoxError::BoxBootError {
            message: format!("Failed to create socket directory: {}", e),
            hint: None,
        })?;

        std::fs::create_dir_all(&logs_dir).map_err(|e| BoxError::BoxBootError {
            message: format!("Failed to create logs directory: {}", e),
            hint: None,
        })?;

        // Resolve paths from config
        let workspace_path = PathBuf::from(&self.config.workspace);

        // Use first skills directory, or create a default one
        let skills_path = self
            .config
            .skills
            .first()
            .cloned()
            .unwrap_or_else(|| self.home_dir.join("skills"));

        // Ensure workspace exists
        if !workspace_path.exists() {
            std::fs::create_dir_all(&workspace_path).map_err(|e| BoxError::BoxBootError {
                message: format!("Failed to create workspace directory: {}", e),
                hint: None,
            })?;
        }

        // Ensure skills directory exists
        if !skills_path.exists() {
            std::fs::create_dir_all(&skills_path).map_err(|e| BoxError::BoxBootError {
                message: format!("Failed to create skills directory: {}", e),
                hint: None,
            })?;
        }

        // Guest rootfs path (must be set up separately)
        let rootfs_path = self.home_dir.join("guest-rootfs");

        Ok(BoxLayout {
            rootfs_path,
            socket_path: socket_dir.join("grpc.sock"),
            workspace_path,
            skills_path,
            console_output: Some(logs_dir.join("console.log")),
        })
    }

    /// Build InstanceSpec from config and layout.
    fn build_instance_spec(&self, layout: &BoxLayout) -> Result<InstanceSpec> {
        // Build filesystem mounts
        let fs_mounts = vec![
            FsMount {
                tag: "workspace".to_string(),
                host_path: layout.workspace_path.clone(),
                read_only: false,
            },
            FsMount {
                tag: "skills".to_string(),
                host_path: layout.skills_path.clone(),
                read_only: true,
            },
        ];

        // Build entrypoint
        // The guest agent listens on vsock for gRPC commands
        let entrypoint = Entrypoint {
            executable: GUEST_AGENT_PATH.to_string(),
            args: vec![
                "--listen".to_string(),
                format!("vsock://{}", AGENT_VSOCK_PORT),
            ],
            env: vec![],
        };

        Ok(InstanceSpec {
            box_id: self.box_id.clone(),
            vcpus: self.config.resources.vcpus as u8,
            memory_mib: self.config.resources.memory_mb,
            rootfs_path: layout.rootfs_path.clone(),
            grpc_socket_path: layout.socket_path.clone(),
            fs_mounts,
            entrypoint,
            console_output: layout.console_output.clone(),
            workdir: GUEST_WORKDIR.to_string(),
        })
    }

    /// Wait for the guest agent to become ready.
    async fn wait_for_guest_ready(&self, socket_path: &std::path::Path) -> Result<()> {
        const MAX_WAIT_MS: u64 = 30000;
        const POLL_INTERVAL_MS: u64 = 100;

        tracing::debug!(
            socket_path = %socket_path.display(),
            "Waiting for guest agent to become ready"
        );

        let start = std::time::Instant::now();

        while start.elapsed().as_millis() < MAX_WAIT_MS as u128 {
            // Check if socket exists
            if socket_path.exists() {
                // TODO: Perform actual gRPC health check
                // For now, just check socket existence
                tracing::debug!("gRPC socket created, guest agent ready");
                return Ok(());
            }

            // Check if VM is still running
            if let Some(ref handler) = *self.handler.read().await {
                if !handler.is_running() {
                    return Err(BoxError::BoxBootError {
                        message: "VM process exited unexpectedly".to_string(),
                        hint: Some("Check console output for errors".to_string()),
                    });
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
        }

        Err(BoxError::TimeoutError(
            "Timed out waiting for guest agent to become ready".to_string(),
        ))
    }
}

/// Get the A3S home directory (~/.a3s).
fn dirs_home() -> Option<PathBuf> {
    // Check A3S_HOME environment variable first
    if let Ok(home) = std::env::var("A3S_HOME") {
        return Some(PathBuf::from(home));
    }

    // Fall back to ~/.a3s
    dirs::home_dir().map(|h| h.join(".a3s"))
}

/// VM configuration for libkrun (legacy, kept for compatibility).
#[derive(Debug, Clone)]
pub struct VmConfig {
    /// Number of vCPUs
    pub vcpus: u32,

    /// Memory in MB
    pub memory_mb: u32,

    /// Kernel image path
    pub kernel_path: String,

    /// Init command
    pub init_cmd: Vec<String>,
}

impl From<&BoxConfig> for VmConfig {
    fn from(config: &BoxConfig) -> Self {
        Self {
            vcpus: config.resources.vcpus,
            memory_mb: config.resources.memory_mb,
            kernel_path: "/path/to/kernel".to_string(),
            init_cmd: vec![GUEST_AGENT_PATH.to_string()],
        }
    }
}
