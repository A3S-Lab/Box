//! VM Manager - Lifecycle management for MicroVM instances.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use a3s_box_core::config::{AgentType, BoxConfig, BusinessType, TeeConfig};
use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::event::{BoxEvent, EventEmitter};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::grpc::AgentClient;
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
    /// Whether the OCI image is extracted at rootfs root (true) or under /agent (false).
    /// Images from OCI registries are extracted at root so absolute symlinks work.
    image_at_root: bool,
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

    /// gRPC client for communicating with the guest agent
    agent_client: Option<AgentClient>,

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
            agent_client: None,
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
            agent_client: None,
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

    /// Get the agent client, if connected.
    pub fn agent_client(&self) -> Option<&AgentClient> {
        self.agent_client.as_ref()
    }

    /// Boot the VM.
    pub async fn boot(&mut self) -> Result<()> {
        // Check and transition state: Created → booting
        {
            let state = self.state.read().await;
            if *state != BoxState::Created {
                return Err(BoxError::Other("VM already booted".to_string()));
            }
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

        // 5. Wait for guest ready
        if layout.image_at_root {
            // Generic OCI image (no a3s agent) - just wait for the VM process to stabilize
            self.wait_for_vm_running().await?;
        } else {
            // A3S agent image - wait for gRPC health check
            self.wait_for_guest_ready(&layout.socket_path).await?;
        }

        // 6. Update state to Ready
        *self.state.write().await = BoxState::Ready;

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

    /// Get the PID of the VM shim process.
    pub async fn pid(&self) -> Option<u32> {
        self.handler
            .read()
            .await
            .as_ref()
            .map(|handler| handler.pid())
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

        // Canonicalize paths to absolute (libkrun requires absolute paths for virtiofs)
        let workspace_path =
            workspace_path
                .canonicalize()
                .map_err(|e| BoxError::BoxBootError {
                    message: format!(
                        "Failed to resolve workspace path {}: {}",
                        workspace_path.display(),
                        e
                    ),
                    hint: None,
                })?;
        let skills_path = skills_path
            .canonicalize()
            .map_err(|e| BoxError::BoxBootError {
                message: format!(
                    "Failed to resolve skills path {}: {}",
                    skills_path.display(),
                    e
                ),
                hint: None,
            })?;

        // Prepare rootfs based on agent type
        let (rootfs_path, agent_oci_config, has_guest_init, image_at_root) = match &self.config.agent {
            AgentType::OciImage { path: agent_path } => {
                // Use OCI image for agent (extracted under /agent)
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

                (rootfs_path, Some(agent_config), has_guest_init, false)
            }
            AgentType::OciRegistry { reference } => {
                // Pull image from registry and extract at rootfs root.
                // This preserves absolute symlinks and dynamic linker paths.
                let images_dir = self.home_dir.join("images");
                let store = crate::oci::ImageStore::new(&images_dir, 10 * 1024 * 1024 * 1024)?;
                let puller = crate::oci::ImagePuller::new(
                    std::sync::Arc::new(store),
                    crate::oci::RegistryAuth::from_env(),
                );

                tracing::info!(
                    reference = %reference,
                    "Pulling OCI image from registry"
                );

                let oci_image = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(puller.pull(reference))
                })?;

                let agent_path = oci_image.root_dir().to_path_buf();
                let rootfs_path = box_dir.join("rootfs");

                tracing::info!(
                    agent_image = %agent_path.display(),
                    rootfs = %rootfs_path.display(),
                    "Building rootfs from pulled OCI image"
                );

                // Extract at root ("/") so absolute symlinks and library paths work
                let mut builder = OciRootfsBuilder::new(&rootfs_path)
                    .with_agent_image(&agent_path)
                    .with_agent_target("/")
                    .with_business_target("/workspace");

                if let BusinessType::OciImage {
                    path: business_path,
                } = &self.config.business
                {
                    builder = builder.with_business_image(business_path);
                }

                let has_guest_init = if let Ok(guest_init_path) = Self::find_guest_init() {
                    tracing::info!(
                        guest_init = %guest_init_path.display(),
                        "Using guest init for namespace isolation"
                    );
                    builder = builder.with_guest_init(guest_init_path);

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

                builder.build()?;
                let agent_config = builder.agent_config()?;

                (rootfs_path, Some(agent_config), has_guest_init, true)
            }
            AgentType::A3sCode | AgentType::LocalBinary { .. } | AgentType::RemoteBinary { .. } => {
                // Use default guest-rootfs (must be set up separately)
                let rootfs_path = self.home_dir.join("guest-rootfs");
                (rootfs_path, None, false, false)
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
            image_at_root,
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
    ///
    /// The binary must be a Linux ELF executable since it runs inside the VM.
    fn find_guest_init() -> Result<PathBuf> {
        let candidates = Self::find_binary_candidates("a3s-box-guest-init");
        for path in candidates {
            if Self::is_linux_elf(&path) {
                return Ok(path);
            }
            tracing::debug!(
                path = %path.display(),
                "Skipping guest init (not a Linux ELF binary)"
            );
        }

        Err(BoxError::BoxBootError {
            message: "Linux guest init binary not found".to_string(),
            hint: Some(
                "Cross-compile with: cargo build -p a3s-box-guest-init --target aarch64-unknown-linux-musl"
                    .to_string(),
            ),
        })
    }

    /// Find the nsexec binary in common locations.
    ///
    /// Searches in order:
    /// 1. Same directory as current executable
    /// 2. target/debug or target/release (for development)
    /// 3. PATH
    ///
    /// The binary must be a Linux ELF executable since it runs inside the VM.
    fn find_nsexec() -> Result<PathBuf> {
        let candidates = Self::find_binary_candidates("a3s-box-nsexec");
        for path in candidates {
            if Self::is_linux_elf(&path) {
                return Ok(path);
            }
            tracing::debug!(
                path = %path.display(),
                "Skipping nsexec (not a Linux ELF binary)"
            );
        }

        Err(BoxError::BoxBootError {
            message: "Linux nsexec binary not found".to_string(),
            hint: Some(
                "Cross-compile with: cargo build -p a3s-box-guest-init --bin a3s-box-nsexec --target aarch64-unknown-linux-musl"
                    .to_string(),
            ),
        })
    }

    /// Search common locations for a binary by name.
    fn find_binary_candidates(name: &str) -> Vec<PathBuf> {
        let mut candidates = Vec::new();

        // Try same directory as current executable
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                let path = exe_dir.join(name);
                if path.exists() {
                    candidates.push(path);
                }
            }
        }

        // Try cross-compilation target directories (for development)
        let target_dirs = [
            "target/aarch64-unknown-linux-musl/debug",
            "target/aarch64-unknown-linux-musl/release",
            "target/x86_64-unknown-linux-musl/debug",
            "target/x86_64-unknown-linux-musl/release",
            "target/debug",
            "target/release",
        ];
        for dir in &target_dirs {
            let path = PathBuf::from(dir).join(name);
            if path.exists() {
                candidates.push(path);
            }
        }

        // Try PATH
        if let Ok(path_var) = std::env::var("PATH") {
            for dir in std::env::split_paths(&path_var) {
                let path = dir.join(name);
                if path.exists() {
                    candidates.push(path);
                }
            }
        }

        candidates
    }

    /// Check if a file is a Linux ELF binary by reading its magic bytes.
    fn is_linux_elf(path: &std::path::Path) -> bool {
        let Ok(file) = std::fs::File::open(path) else {
            return false;
        };
        use std::io::Read;
        let mut header = [0u8; 18];
        let Ok(_) = (&file).read_exact(&mut header) else {
            return false;
        };
        // ELF magic: 0x7f 'E' 'L' 'F'
        if header[0..4] != [0x7f, b'E', b'L', b'F'] {
            return false;
        }
        // EI_OSABI (byte 7): 0x00 = ELFOSABI_NONE (System V / Linux)
        // or 0x03 = ELFOSABI_LINUX
        matches!(header[7], 0x00 | 0x03)
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
                    let (exec, args) = Self::resolve_oci_entrypoint(oci_config, layout.image_at_root, &self.config.cmd);
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
                    let (executable, args) = Self::resolve_oci_entrypoint(oci_config, layout.image_at_root, &self.config.cmd);
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

    /// Resolve the executable and args from an OCI image config.
    ///
    /// Follows Docker semantics:
    /// - If ENTRYPOINT is set: executable = ENTRYPOINT[0], args = ENTRYPOINT[1:] + CMD
    /// - If only CMD is set: executable = CMD[0], args = CMD[1:]
    /// - If neither: fall back to default agent path
    /// - If `cmd_override` is non-empty, it replaces the OCI CMD
    ///
    /// When `image_at_root` is false, paths are prefixed with `/agent` since the
    /// image is extracted under that directory. When true, paths are used as-is.
    fn resolve_oci_entrypoint(
        oci_config: &OciImageConfig,
        image_at_root: bool,
        cmd_override: &[String],
    ) -> (String, Vec<String>) {
        let oci_entrypoint = oci_config
            .entrypoint
            .as_ref()
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let oci_cmd = if cmd_override.is_empty() {
            oci_config.cmd.as_ref().map(|v| v.as_slice()).unwrap_or(&[])
        } else {
            cmd_override
        };

        let maybe_prefix = |path: &str| -> String {
            if image_at_root {
                path.to_string()
            } else {
                Self::prefix_agent_path(path)
            }
        };

        if !oci_entrypoint.is_empty() {
            // ENTRYPOINT is set: use it as executable, CMD as additional args
            let exec = maybe_prefix(&oci_entrypoint[0]);
            let mut args: Vec<String> = oci_entrypoint.iter().skip(1).cloned().collect();
            args.extend(oci_cmd.iter().cloned());
            (exec, args)
        } else if !oci_cmd.is_empty() {
            // Only CMD is set: use CMD[0] as executable, CMD[1:] as args
            let exec = maybe_prefix(&oci_cmd[0]);
            let args: Vec<String> = oci_cmd.iter().skip(1).cloned().collect();
            (exec, args)
        } else {
            // Neither set: fall back to default agent path
            (GUEST_AGENT_PATH.to_string(), vec![])
        }
    }

    /// Prefix a path with /agent to make it relative to the agent rootfs.
    fn prefix_agent_path(path: &str) -> String {
        if path.starts_with('/') {
            format!("/agent{}", path)
        } else {
            format!("/agent/{}", path)
        }
    }

    /// Wait for the VM process to be running (for generic OCI images without an agent).
    ///
    /// Gives the VM a brief moment to start, then verifies the process hasn't exited.
    async fn wait_for_vm_running(&self) -> Result<()> {
        const STABILIZE_MS: u64 = 1000;

        tracing::debug!("Waiting for VM process to stabilize");
        tokio::time::sleep(tokio::time::Duration::from_millis(STABILIZE_MS)).await;

        if let Some(ref handler) = *self.handler.read().await {
            if !handler.is_running() {
                return Err(BoxError::BoxBootError {
                    message: "VM process exited immediately after start".to_string(),
                    hint: Some("Check console output for errors".to_string()),
                });
            }
        }

        tracing::debug!("VM process is running");
        Ok(())
    }

    /// Wait for the guest agent to become ready.
    ///
    /// Phase 1: Wait for the Unix socket file to appear on disk.
    /// Phase 2: Connect via gRPC and perform a health check with retries.
    async fn wait_for_guest_ready(&mut self, socket_path: &std::path::Path) -> Result<()> {
        const MAX_WAIT_MS: u64 = 30000;
        const POLL_INTERVAL_MS: u64 = 100;
        const HEALTH_CHECK_INTERVAL_MS: u64 = 250;

        tracing::debug!(
            socket_path = %socket_path.display(),
            "Waiting for guest agent to become ready"
        );

        let start = std::time::Instant::now();

        // Phase 1: Wait for socket file to appear
        loop {
            if start.elapsed().as_millis() >= MAX_WAIT_MS as u128 {
                return Err(BoxError::TimeoutError(
                    "Timed out waiting for gRPC socket to appear".to_string(),
                ));
            }

            if socket_path.exists() {
                tracing::debug!("gRPC socket file detected");
                break;
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

        // Phase 2: Connect and perform gRPC health check with retries
        let mut last_err = None;
        while start.elapsed().as_millis() < MAX_WAIT_MS as u128 {
            match AgentClient::connect(socket_path).await {
                Ok(client) => {
                    match client.health_check().await {
                        Ok(true) => {
                            tracing::debug!("Guest agent health check passed");
                            self.agent_client = Some(client);
                            return Ok(());
                        }
                        Ok(false) => {
                            tracing::debug!("Guest agent reported unhealthy, retrying");
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, "Health check RPC failed, retrying");
                            last_err = Some(e);
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, "Failed to connect to agent, retrying");
                    last_err = Some(e);
                }
            }

            // Check if VM is still running
            if let Some(ref handler) = *self.handler.read().await {
                if !handler.is_running() {
                    return Err(BoxError::BoxBootError {
                        message: "VM process exited during health check".to_string(),
                        hint: Some("Check console output for errors".to_string()),
                    });
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(HEALTH_CHECK_INTERVAL_MS))
                .await;
        }

        Err(BoxError::TimeoutError(format!(
            "Timed out waiting for guest agent health check (last error: {})",
            last_err
                .map(|e| e.to_string())
                .unwrap_or_else(|| "none".to_string()),
        )))
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
