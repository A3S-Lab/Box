//! VM Manager - Lifecycle management for MicroVM instances.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use a3s_box_core::config::{AgentType, BoxConfig, BusinessType, TeeConfig};
use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::event::{BoxEvent, EventEmitter};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::oci::{OciImageConfig, OciRootfsBuilder};
use crate::rootfs::{GUEST_AGENT_PATH, GUEST_WORKDIR};
use crate::vmm::{Entrypoint, FsMount, InstanceSpec, TeeInstanceConfig, VmController, VmHandler};
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
    /// OCI image config for agent (if using OCI image)
    agent_oci_config: Option<OciImageConfig>,
    /// Whether guest init is installed for namespace isolation
    has_guest_init: bool,
    /// TEE instance configuration (if TEE is enabled)
    tee_instance_config: Option<TeeInstanceConfig>,
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

        // Prepare rootfs based on agent type
        let (rootfs_path, agent_oci_config, has_guest_init) = match &self.config.agent {
            AgentType::OciImage { path: agent_path } => {
                // Use OCI image for agent
                let rootfs_path = box_dir.join("rootfs");

                tracing::info!(
                    agent_image = %agent_path.display(),
                    rootfs = %rootfs_path.display(),
                    "Building rootfs from OCI images"
                );

                // Build rootfs using OciRootfsBuilder
                let mut builder = OciRootfsBuilder::new(&rootfs_path)
                    .with_agent_image(agent_path)
                    .with_agent_target("/agent")
                    .with_business_target("/workspace");

                // Add business image if specified
                if let BusinessType::OciImage {
                    path: business_path,
                } = &self.config.business
                {
                    builder = builder.with_business_image(business_path);
                }

                // Add guest init if available
                let has_guest_init = if let Ok(guest_init_path) = Self::find_guest_init() {
                    tracing::info!(
                        guest_init = %guest_init_path.display(),
                        "Using guest init for namespace isolation"
                    );
                    builder = builder.with_guest_init(guest_init_path);

                    // Also add nsexec if available
                    if let Ok(nsexec_path) = Self::find_nsexec() {
                        tracing::info!(
                            nsexec = %nsexec_path.display(),
                            "Installing nsexec for business code execution"
                        );
                        builder = builder.with_nsexec(nsexec_path);
                    }

                    true
                } else {
                    false
                };

                // Build the rootfs
                builder.build()?;

                // Get agent OCI config for entrypoint/env extraction
                let agent_config = builder.agent_config()?;

                (rootfs_path, Some(agent_config), has_guest_init)
            }
            AgentType::A3sCode | AgentType::LocalBinary { .. } | AgentType::RemoteBinary { .. } => {
                // Use default guest-rootfs (must be set up separately)
                let rootfs_path = self.home_dir.join("guest-rootfs");
                (rootfs_path, None, false)
            }
        };

        // Generate TEE configuration if enabled
        let tee_instance_config = self.generate_tee_config(&box_dir)?;

        Ok(BoxLayout {
            rootfs_path,
            socket_path: socket_dir.join("grpc.sock"),
            workspace_path,
            skills_path,
            console_output: Some(logs_dir.join("console.log")),
            agent_oci_config,
            has_guest_init,
            tee_instance_config,
        })
    }

    /// Generate TEE configuration file if TEE is enabled.
    fn generate_tee_config(&self, box_dir: &Path) -> Result<Option<TeeInstanceConfig>> {
        match &self.config.tee {
            TeeConfig::None => Ok(None),
            TeeConfig::SevSnp {
                workload_id,
                generation,
            } => {
                // Verify hardware support
                crate::tee::require_sev_snp_support()?;

                // Generate TEE config JSON
                let config = serde_json::json!({
                    "workload_id": workload_id,
                    "cpus": self.config.resources.vcpus,
                    "ram_mib": self.config.resources.memory_mb,
                    "tee": "snp",
                    "tee_data": format!(r#"{{"gen":"{}"}}"#, generation.as_str()),
                    "attestation_url": ""  // Phase 2: Remote attestation
                });

                let config_path = box_dir.join("tee-config.json");
                std::fs::write(&config_path, serde_json::to_string_pretty(&config)?).map_err(
                    |e| {
                        BoxError::TeeConfig(format!(
                            "Failed to write TEE config to {}: {}",
                            config_path.display(),
                            e
                        ))
                    },
                )?;

                tracing::info!(
                    workload_id = %workload_id,
                    generation = %generation.as_str(),
                    config_path = %config_path.display(),
                    "Generated TEE configuration"
                );

                Ok(Some(TeeInstanceConfig {
                    config_path,
                    tee_type: "snp".to_string(),
                }))
            }
        }
    }

    /// Find the guest init binary in common locations.
    ///
    /// Searches in order:
    /// 1. Same directory as current executable
    /// 2. target/debug or target/release (for development)
    /// 3. PATH
    fn find_guest_init() -> Result<PathBuf> {
        // Try same directory as current executable
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                let guest_init = exe_dir.join("a3s-box-guest-init");
                if guest_init.exists() {
                    return Ok(guest_init);
                }
            }
        }

        // Try target/debug or target/release (for development)
        let target_dirs = ["target/debug", "target/release"];
        for dir in &target_dirs {
            let guest_init = PathBuf::from(dir).join("a3s-box-guest-init");
            if guest_init.exists() {
                return Ok(guest_init);
            }
        }

        // Try PATH
        if let Ok(path_var) = std::env::var("PATH") {
            for path in std::env::split_paths(&path_var) {
                let guest_init = path.join("a3s-box-guest-init");
                if guest_init.exists() {
                    return Ok(guest_init);
                }
            }
        }

        Err(BoxError::BoxBootError {
            message: "Guest init binary not found".to_string(),
            hint: Some("Build with: cargo build -p a3s-box-guest-init".to_string()),
        })
    }

    /// Find the nsexec binary in common locations.
    ///
    /// Searches in order:
    /// 1. Same directory as current executable
    /// 2. target/debug or target/release (for development)
    /// 3. PATH
    fn find_nsexec() -> Result<PathBuf> {
        // Try same directory as current executable
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                let nsexec = exe_dir.join("a3s-box-nsexec");
                if nsexec.exists() {
                    return Ok(nsexec);
                }
            }
        }

        // Try target/debug or target/release (for development)
        let target_dirs = ["target/debug", "target/release"];
        for dir in &target_dirs {
            let nsexec = PathBuf::from(dir).join("a3s-box-nsexec");
            if nsexec.exists() {
                return Ok(nsexec);
            }
        }

        // Try PATH
        if let Ok(path_var) = std::env::var("PATH") {
            for path in std::env::split_paths(&path_var) {
                let nsexec = path.join("a3s-box-nsexec");
                if nsexec.exists() {
                    return Ok(nsexec);
                }
            }
        }

        Err(BoxError::BoxBootError {
            message: "Nsexec binary not found".to_string(),
            hint: Some("Build with: cargo build -p a3s-box-guest-init --bin a3s-box-nsexec".to_string()),
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

        // Build entrypoint based on agent type and OCI config
        let entrypoint = if layout.has_guest_init {
            // Use guest init as entrypoint for namespace isolation
            // Pass agent configuration via environment variables
            let (agent_exec, agent_args, agent_env) = match &layout.agent_oci_config {
                Some(oci_config) => {
                    let oci_entrypoint = oci_config
                        .entrypoint
                        .as_ref()
                        .map(|v| v.as_slice())
                        .unwrap_or(&[]);
                    let oci_cmd = oci_config.cmd.as_ref().map(|v| v.as_slice()).unwrap_or(&[]);

                    let exec = if !oci_entrypoint.is_empty() {
                        let e = &oci_entrypoint[0];
                        if e.starts_with('/') {
                            format!("/agent{}", e)
                        } else {
                            format!("/agent/{}", e)
                        }
                    } else {
                        "/agent/bin/agent".to_string()
                    };

                    let mut args: Vec<String> = oci_entrypoint.iter().skip(1).cloned().collect();
                    args.extend(oci_cmd.iter().cloned());

                    (exec, args, oci_config.env.clone())
                }
                None => (
                    GUEST_AGENT_PATH.to_string(),
                    vec![
                        "--listen".to_string(),
                        format!("vsock://{}", AGENT_VSOCK_PORT),
                    ],
                    vec![],
                ),
            };

            // Build environment for guest init
            let mut env: Vec<(String, String)> = vec![
                ("A3S_AGENT_EXEC".to_string(), agent_exec),
                ("A3S_AGENT_ARGS".to_string(), agent_args.join(" ")),
            ];

            // Add agent environment variables with A3S_AGENT_ENV_ prefix
            for (key, value) in agent_env {
                env.push((format!("A3S_AGENT_ENV_{}", key), value));
            }

            tracing::debug!(
                env = ?env,
                "Using guest init with agent configuration"
            );

            Entrypoint {
                executable: "/sbin/init".to_string(),
                args: vec![],
                env,
            }
        } else {
            // Direct agent execution (no namespace isolation)
            match &layout.agent_oci_config {
                Some(oci_config) => {
                    // Use OCI image config for entrypoint
                    let oci_entrypoint = oci_config
                        .entrypoint
                        .as_ref()
                        .map(|v| v.as_slice())
                        .unwrap_or(&[]);
                    let oci_cmd = oci_config.cmd.as_ref().map(|v| v.as_slice()).unwrap_or(&[]);

                    // Combine entrypoint and cmd
                    let executable = if !oci_entrypoint.is_empty() {
                        // Prepend /agent to make path relative to agent's root
                        let exec = &oci_entrypoint[0];
                        if exec.starts_with('/') {
                            format!("/agent{}", exec)
                        } else {
                            format!("/agent/{}", exec)
                        }
                    } else {
                        // Default agent path
                        "/agent/bin/agent".to_string()
                    };

                    // Build args from entrypoint[1:] + cmd
                    let mut args: Vec<String> = oci_entrypoint.iter().skip(1).cloned().collect();
                    args.extend(oci_cmd.iter().cloned());

                    // Use environment variables from OCI config
                    let env = oci_config.env.clone();

                    tracing::debug!(
                        executable = %executable,
                        args = ?args,
                        env_count = env.len(),
                        workdir = ?oci_config.working_dir,
                        "Using OCI image entrypoint"
                    );

                    Entrypoint {
                        executable,
                        args,
                        env,
                    }
                }
                None => {
                    // Use default A3S agent entrypoint
                    // The guest agent listens on vsock for gRPC commands
                    Entrypoint {
                        executable: GUEST_AGENT_PATH.to_string(),
                        args: vec![
                            "--listen".to_string(),
                            format!("vsock://{}", AGENT_VSOCK_PORT),
                        ],
                        env: vec![],
                    }
                }
            }
        };

        // Determine workdir
        let workdir = match &layout.agent_oci_config {
            Some(oci_config) => oci_config
                .working_dir
                .clone()
                .unwrap_or_else(|| GUEST_WORKDIR.to_string()),
            None => GUEST_WORKDIR.to_string(),
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
            workdir,
            tee_config: layout.tee_instance_config.clone(),
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
