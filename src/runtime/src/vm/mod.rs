//! VM Manager - Lifecycle management for MicroVM instances.

mod layout;
mod network;
mod ready;
pub mod reap;
mod sandbox;
mod spec;
#[cfg(windows)]
mod windows_stop;

pub(crate) use layout::{persistent_rootfs_generation_exists, runtime_socket_dir};

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Callback type for image pull progress: `(current, total, digest, size_bytes)`.
pub type PullProgressFn = Arc<dyn Fn(usize, usize, &str, i64) + Send + Sync>;

use a3s_box_core::config::BoxConfig;
#[cfg(unix)]
use a3s_box_core::config::TeeConfig;
use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::event::{BoxEvent, EventEmitter};
use a3s_box_core::execution::{ExecutionBackend, ResolvedExecutionPlan};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::Instrument;

#[cfg(unix)]
use libc;

#[cfg(unix)]
use crate::grpc::ExecClient;
#[cfg(unix)]
use crate::tee::TeeExtension;
use crate::vmm::{VmController, VmHandler, VmmProvider, DEFAULT_SHUTDOWN_TIMEOUT_MS};

/// Box state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoxState {
    /// Config captured, no VM started
    Created,

    /// VM booted, container initialized, gRPC healthy
    Ready,

    /// A session is actively processing a prompt
    Busy,

    /// A session is compressing its context
    Compacting,

    /// VM terminated, resources freed
    Stopped,
}

/// Layout of directories for a box instance.
pub(crate) struct BoxLayout {
    /// Path to the root filesystem
    pub(crate) rootfs_path: PathBuf,
    /// Path to the exec Unix socket
    pub(crate) exec_socket_path: PathBuf,
    /// Path to the PTY Unix socket
    pub(crate) pty_socket_path: PathBuf,
    /// Path to the attestation Unix socket
    pub(crate) attest_socket_path: PathBuf,
    /// Path to the CRI port-forward Unix socket
    pub(crate) port_forward_socket_path: PathBuf,
    /// Path to the workspace directory
    pub(crate) workspace_path: PathBuf,
    /// Path to console output file (optional)
    pub(crate) console_output: Option<PathBuf>,
    /// OCI image config (entrypoint, env, working dir, volumes)
    pub(crate) oci_config: Option<crate::oci::OciImageConfig>,
    /// Fresh image/cache rootfs generations must ignore any terminal manifest
    /// baked into an older malicious image. Persistent and Snapshot generations
    /// instead prefer the terminal manifest captured after guest writes.
    pub(crate) prefer_image_rootfs_metadata: bool,
    /// TEE instance configuration (if TEE is enabled)
    pub(crate) tee_instance_config: Option<crate::vmm::TeeInstanceConfig>,
}

#[cfg(target_os = "windows")]
const WINDOWS_GUEST_EXIT_CODE: &str = ".a3s_exit_code";
#[cfg(target_os = "windows")]
const WINDOWS_GUEST_STDOUT: &str = "guest-init.stdout.log";
#[cfg(target_os = "windows")]
const WINDOWS_GUEST_STDERR: &str = "guest-init.stderr.log";
#[cfg(target_os = "windows")]
const WINDOWS_STOP_DELIVERY_TIMEOUT_MS: u64 = 1_000;
#[cfg(target_os = "windows")]
const WINDOWS_GUEST_FINALIZATION_TIMEOUT_MS: u64 = 30_000;
#[cfg(target_os = "windows")]
const WINDOWS_GUEST_RESULT_MARKER: &str = ".a3s_host_result_collected";
#[cfg(target_os = "windows")]
const WINDOWS_LIVE_LOGS_DRAINED_MARKER: &str = ".a3s_host_live_logs_drained";

/// Append a completed Windows guest stream to its raw host console, filtering
/// libkrun's pre-guest C-init diagnostics while preserving arbitrary bytes.
#[cfg(target_os = "windows")]
fn append_windows_guest_stream(
    source: &Path,
    destination: &Path,
    runtime_filter: &a3s_box_core::log::RuntimeConsoleFilter,
) -> std::io::Result<()> {
    use std::io::{BufRead, Write};

    let input = match a3s_box_core::windows_file::open_regular_file(source, None) {
        Ok((input, _)) => input,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut reader = std::io::BufReader::new(input);
    let mut output = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(destination)?;
    let mut line = Vec::new();

    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let keep = !line.ends_with(b"\n")
            || std::str::from_utf8(&line).map_or(true, |line| runtime_filter.keep_line(line));
        if keep {
            output.write_all(&line)?;
        }
    }

    output.flush()
}

#[cfg(target_os = "windows")]
fn windows_marker_matches(path: &Path, expected: &[u8]) -> bool {
    use std::io::Read;

    let Ok((file, _)) = a3s_box_core::windows_file::open_regular_file(path, None) else {
        return false;
    };
    let mut contents = Vec::with_capacity(expected.len().saturating_add(1));
    if file
        .take(expected.len().saturating_add(1) as u64)
        .read_to_end(&mut contents)
        .is_err()
    {
        return false;
    }
    contents == expected
}

/// Collect the completed WHPX guest result after the shim process has exited.
///
/// Current shims drain structured logs before exiting. The runtime still owns
/// raw-console collection and provides a completed-stream fallback for older
/// libkrun bundles that terminate the shim with `_exit`.
#[cfg(target_os = "windows")]
pub fn collect_windows_guest_result(
    box_dir: &Path,
    log_config: &a3s_box_core::log::LogConfig,
    shim_exit_code: i32,
) -> Result<i32> {
    let rootfs = box_dir.join("rootfs");
    let logs = box_dir.join("logs");
    let marker = rootfs.join(WINDOWS_GUEST_RESULT_MARKER);
    let live_logs_drained = rootfs.join(WINDOWS_LIVE_LOGS_DRAINED_MARKER);
    let stdout_source = rootfs.join(WINDOWS_GUEST_STDOUT);
    let stderr_source = rootfs.join(WINDOWS_GUEST_STDERR);

    if !windows_marker_matches(&marker, b"collected\n") {
        std::fs::create_dir_all(&logs)?;
        let runtime_filter = a3s_box_core::log::RuntimeConsoleFilter::new();

        for (source, destination) in [
            (&stdout_source, logs.join("console.log")),
            (&stderr_source, logs.join("console.err.log")),
        ] {
            append_windows_guest_stream(source, &destination, &runtime_filter).map_err(
                |error| BoxError::BoxBootError {
                    message: format!(
                        "Failed to collect Windows guest output {} into {}: {error}",
                        source.display(),
                        destination.display()
                    ),
                    hint: None,
                },
            )?;
        }

        // New Windows shims tail these sources live and drain them before exit.
        // Older libkrun bundles still terminate the shim with `_exit`, so keep
        // the completed-stream fallback when no drained marker exists. Process
        // the sources rather than the retained raw console to avoid replaying a
        // previous restart.
        if !windows_marker_matches(&live_logs_drained, b"drained\n") {
            let stopped = std::sync::atomic::AtomicBool::new(true);
            a3s_box_core::log::run_log_processor_streams(
                &stdout_source,
                &stderr_source,
                &logs,
                log_config,
                &stopped,
            );
        }

        a3s_box_core::windows_file::replace_regular_file(&marker, b"collected\n").map_err(
            |error| BoxError::BoxBootError {
                message: format!(
                    "Failed to mark the Windows guest result collected at {}: {error}",
                    marker.display()
                ),
                hint: None,
            },
        )?;
    }

    let exit_path = rootfs.join(WINDOWS_GUEST_EXIT_CODE);
    let contents = match a3s_box_core::windows_file::open_regular_file(&exit_path, None) {
        Ok((file, _)) => {
            use std::io::Read;
            let mut contents = String::new();
            file.take(64)
                .read_to_string(&mut contents)
                .map_err(|error| BoxError::BoxBootError {
                    message: format!(
                        "Failed to read the Windows guest exit code {}: {error}",
                        exit_path.display()
                    ),
                    hint: None,
                })?;
            contents
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && shim_exit_code != 0 => {
            return Ok(shim_exit_code);
        }
        Err(error) => {
            return Err(BoxError::BoxBootError {
                message: if error.kind() == std::io::ErrorKind::NotFound {
                    format!(
                        "WHPX stopped before the guest persisted its exit code ({})",
                        exit_path.display()
                    )
                } else {
                    format!(
                        "Failed to read the Windows guest exit code {}: {error}",
                        exit_path.display()
                    )
                },
                hint: Some(
                    "Inspect logs/init-rust.log and the shim log for the guest boot failure"
                        .to_string(),
                ),
            });
        }
    };

    contents
        .trim()
        .parse::<i32>()
        .map_err(|error| BoxError::BoxBootError {
            message: format!(
                "Invalid Windows guest exit code in {}: {error}",
                exit_path.display()
            ),
            hint: None,
        })
}

/// VM manager - orchestrates VM lifecycle.
pub struct VmManager {
    /// Box configuration
    pub(crate) config: BoxConfig,

    /// Unique box identifier
    pub(crate) box_id: String,

    /// Current state
    pub(crate) state: Arc<RwLock<BoxState>>,

    /// Event emitter
    pub(crate) event_emitter: EventEmitter,

    /// VMM provider (spawns VMs via pluggable backend)
    pub(crate) provider: Option<Box<dyn VmmProvider>>,

    /// VM handler (runtime operations on running VM)
    pub(crate) handler: Arc<RwLock<Option<Box<dyn VmHandler>>>>,

    /// Exec client for executing commands in the guest
    #[cfg(unix)]
    pub(crate) exec_client: Option<ExecClient>,

    /// Network backend manager for bridge networking (None if TSI mode).
    /// Platform-specific: passt on Linux, gvproxy on macOS.
    pub(crate) net_manager: Option<Box<dyn crate::network::NetworkBackend>>,

    /// A3S home directory (~/.a3s)
    pub(crate) home_dir: PathBuf,

    /// Anonymous volume names created during boot (from OCI VOLUME directives)
    pub(crate) anonymous_volumes: Vec<String>,

    /// Anonymous volumes newly created by the current boot attempt.
    ///
    /// Reused anonymous volumes must survive failed restarts because they may
    /// contain data from an existing stopped box.
    pub(crate) created_anonymous_volumes: Vec<String>,

    /// OCI image config resolved during the last successful boot.
    pub(crate) image_config: Option<crate::oci::OciImageConfig>,

    /// Suppress an image-defined health check for callers that explicitly
    /// requested Docker-compatible `--no-healthcheck` semantics.
    pub(crate) healthcheck_disabled: bool,

    /// Whether this boot attempt started with an existing persistent rootfs
    /// generation. Failed first boots may discard a partial extraction, while
    /// failed restarts must retain the pre-existing guest data.
    pub(crate) preserve_rootfs_on_boot_failure: bool,

    /// TEE extension (attestation, sealing, secret injection)
    #[cfg(unix)]
    pub(crate) tee: Option<Box<dyn TeeExtension>>,

    /// Rootfs provider (overlay or copy)
    pub(crate) rootfs_provider: Box<dyn crate::rootfs::RootfsProvider>,

    /// Path to the exec Unix socket (set after boot)
    pub(crate) exec_socket_path: Option<PathBuf>,

    /// Path to the PTY Unix socket (set after boot)
    pub(crate) pty_socket_path: Option<PathBuf>,

    /// Path to the CRI port-forward Unix socket (set after boot)
    pub(crate) port_forward_socket_path: Option<PathBuf>,

    /// Prometheus metrics (optional, for instrumented deployments).
    pub(crate) prom: Option<crate::prom::RuntimeMetrics>,

    /// Exit code captured from the shim process after it exits.
    pub(crate) shim_exit_code: Option<i32>,

    /// Optional progress callback for image pulls: `(current, total, digest, size_bytes)`.
    pub(crate) pull_progress_fn: Option<PullProgressFn>,

    /// Logging driver config, threaded into the InstanceSpec so the shim runs
    /// the log processor for the box's lifetime (set by the CLI via
    /// [`VmManager::set_log_config`]).
    pub(crate) log_config: a3s_box_core::log::LogConfig,

    /// Backend-neutral resolution captured before any boot side effects.
    pub(crate) resolved_execution_plan: Option<ResolvedExecutionPlan>,
}

impl VmManager {
    /// Create a new VM manager.
    pub fn new(config: BoxConfig, event_emitter: EventEmitter) -> Self {
        let box_id = uuid::Uuid::new_v4().to_string();
        let home_dir = a3s_box_core::dirs_home();

        Self {
            config,
            box_id,
            state: Arc::new(RwLock::new(BoxState::Created)),
            event_emitter,
            provider: None,
            handler: Arc::new(RwLock::new(None)),
            #[cfg(unix)]
            exec_client: None,
            net_manager: None,
            home_dir,
            anonymous_volumes: Vec::new(),
            created_anonymous_volumes: Vec::new(),
            image_config: None,
            healthcheck_disabled: false,
            preserve_rootfs_on_boot_failure: false,
            #[cfg(unix)]
            tee: None,
            rootfs_provider: crate::rootfs::default_provider(),
            exec_socket_path: None,
            pty_socket_path: None,
            port_forward_socket_path: None,
            prom: None,
            shim_exit_code: None,
            pull_progress_fn: None,
            log_config: a3s_box_core::log::LogConfig::default(),
            resolved_execution_plan: None,
        }
    }

    /// Create a new VM manager with a specific box ID.
    pub fn with_box_id(config: BoxConfig, event_emitter: EventEmitter, box_id: String) -> Self {
        let home_dir = a3s_box_core::dirs_home();

        Self {
            config,
            box_id,
            state: Arc::new(RwLock::new(BoxState::Created)),
            event_emitter,
            provider: None,
            handler: Arc::new(RwLock::new(None)),
            #[cfg(unix)]
            exec_client: None,
            net_manager: None,
            home_dir,
            anonymous_volumes: Vec::new(),
            created_anonymous_volumes: Vec::new(),
            image_config: None,
            healthcheck_disabled: false,
            preserve_rootfs_on_boot_failure: false,
            #[cfg(unix)]
            tee: None,
            rootfs_provider: crate::rootfs::default_provider(),
            exec_socket_path: None,
            pty_socket_path: None,
            port_forward_socket_path: None,
            prom: None,
            shim_exit_code: None,
            pull_progress_fn: None,
            log_config: a3s_box_core::log::LogConfig::default(),
            resolved_execution_plan: None,
        }
    }

    /// Remove host-side boot artifacts after a failed boot attempt.
    async fn cleanup_boot_failure(&mut self) {
        if let Some(mut handler) = self.handler.write().await.take() {
            if let Err(error) = handler.stop(default_stop_signal(), DEFAULT_SHUTDOWN_TIMEOUT_MS) {
                tracing::warn!(
                    box_id = %self.box_id,
                    error = %error,
                    "Failed to stop VM handler after boot failure"
                );
            }
            self.shim_exit_code = handler.exit_code();
        }

        if let Some(mut net_manager) = self.net_manager.take() {
            net_manager.stop();
        }

        self.cleanup_created_anonymous_volumes();
        self.cleanup_box_dir();
    }

    fn cleanup_created_anonymous_volumes(&mut self) {
        if self.created_anonymous_volumes.is_empty() {
            return;
        }

        let created = std::mem::take(&mut self.created_anonymous_volumes);
        let created_set: std::collections::HashSet<_> = created.iter().cloned().collect();
        let store = crate::volume::VolumeStore::new(
            self.home_dir.join("volumes.json"),
            self.home_dir.join("volumes"),
        );

        for volume_name in &created {
            if let Err(error) = store.remove(volume_name, true) {
                tracing::debug!(
                    box_id = %self.box_id,
                    volume = volume_name,
                    error = %error,
                    "Failed to remove anonymous volume after boot failure"
                );
            }
        }

        self.anonymous_volumes
            .retain(|name| !created_set.contains(name));
    }

    /// Remove transient host boot artifacts, retaining persistent guest data.
    fn cleanup_box_dir(&self) {
        let box_dir = self.home_dir.join("boxes").join(&self.box_id);
        let socket_dir = self.socket_dir();

        // Reap the box's passt daemon (Linux bridge mode) BEFORE removing its
        // socket dir. A boot that fails after passt spawned but before
        // `self.net_manager` was assigned leaves `net_manager.stop()` a no-op, so
        // passt would otherwise survive holding the published port — the
        // "Address already in use" on the next start. terminate_passt reads
        // `socket_dir/passt.pid` and is a no-op when there is no passt.
        #[cfg(target_os = "linux")]
        crate::network::terminate_passt(&self.socket_dir());

        if let Err(error) = self
            .rootfs_provider
            .cleanup(&box_dir, self.preserve_rootfs_on_boot_failure)
        {
            tracing::warn!(
                box_id = %self.box_id,
                path = %box_dir.display(),
                error = %error,
                "Failed to cleanup rootfs provider after boot failure"
            );
        }

        match std::fs::remove_dir_all(&socket_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::debug!(
                    box_id = %self.box_id,
                    path = %socket_dir.display(),
                    error = %error,
                    "Failed to cleanup socket directory after boot failure"
                );
            }
        }

        // A failed restart must never erase a persistent writable rootfs. The
        // provider cleanup above detaches transient mounts while retaining the
        // persistent generation; only ephemeral boxes are removed wholesale.
        if !self.config.persistent {
            match std::fs::remove_dir_all(&box_dir) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(
                        box_id = %self.box_id,
                        path = %box_dir.display(),
                        error = %error,
                        "Failed to cleanup box directory after boot failure"
                    );
                }
            }
        }
    }

    /// Create a new VM manager with a custom VMM provider.
    pub fn with_provider(
        config: BoxConfig,
        event_emitter: EventEmitter,
        provider: Box<dyn VmmProvider>,
    ) -> Self {
        let box_id = uuid::Uuid::new_v4().to_string();
        let home_dir = a3s_box_core::dirs_home();
        Self {
            config,
            box_id,
            state: Arc::new(RwLock::new(BoxState::Created)),
            event_emitter,
            provider: Some(provider),
            handler: Arc::new(RwLock::new(None)),
            #[cfg(unix)]
            exec_client: None,
            net_manager: None,
            home_dir,
            anonymous_volumes: Vec::new(),
            created_anonymous_volumes: Vec::new(),
            image_config: None,
            healthcheck_disabled: false,
            preserve_rootfs_on_boot_failure: false,
            #[cfg(unix)]
            tee: None,
            rootfs_provider: crate::rootfs::default_provider(),
            exec_socket_path: None,
            pty_socket_path: None,
            port_forward_socket_path: None,
            prom: None,
            shim_exit_code: None,
            pull_progress_fn: None,
            log_config: a3s_box_core::log::LogConfig::default(),
            resolved_execution_plan: None,
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

    /// Get the exec client, if connected.
    #[cfg(unix)]
    pub fn exec_client(&self) -> Option<&ExecClient> {
        self.exec_client.as_ref()
    }

    #[cfg(unix)]
    async fn connect_exec_client_for_request(socket_path: &Path) -> Result<ExecClient> {
        const ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

        let client = ExecClient::connect(socket_path).await?;
        match tokio::time::timeout(ATTEMPT_TIMEOUT, client.heartbeat()).await {
            Ok(Ok(true)) => Ok(client),
            Ok(Ok(false)) => Err(BoxError::ExecError(format!(
                "Exec client not connected: heartbeat failed at {}",
                socket_path.display()
            ))),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(BoxError::ExecError(format!(
                "Exec client not connected: heartbeat timed out at {}",
                socket_path.display()
            ))),
        }
    }

    /// Wait until the guest exec server can complete a heartbeat.
    ///
    /// Cold foreground boots may proceed after the short diagnostic readiness
    /// cap so logs remain visible. A warm pool has a stronger contract: an idle
    /// VM must actually be executable before it is published to callers.
    #[cfg(unix)]
    pub async fn wait_for_exec_available(&mut self, timeout: std::time::Duration) -> Result<()> {
        let socket_path = self
            .exec_socket_path
            .clone()
            .ok_or_else(|| BoxError::ExecError("Exec socket path is unavailable".to_string()))?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match Self::connect_exec_client_for_request(&socket_path).await {
                Ok(client) => {
                    self.exec_client = Some(client);
                    return Ok(());
                }
                Err(error) if tokio::time::Instant::now() < deadline => {
                    tracing::debug!(%error, "Waiting for pooled VM exec readiness");
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    #[cfg(not(unix))]
    pub async fn wait_for_exec_available(&mut self, _timeout: std::time::Duration) -> Result<()> {
        Ok(())
    }

    /// Attach this manager to an already-running shim process.
    ///
    /// This is useful for crash recovery or control-plane restart flows where
    /// the workload VM is still alive and only the host-side manager state
    /// needs to be reconstructed.
    #[cfg(unix)]
    pub async fn attach_running_process(
        &mut self,
        pid: u32,
        exec_socket_path: PathBuf,
        pty_socket_path: Option<PathBuf>,
    ) -> Result<()> {
        let port_forward_socket_path = exec_socket_path.with_file_name("portfwd.sock");
        let handler = crate::vmm::ShimHandler::from_pid(pid, self.box_id.clone());
        if !handler.is_running() {
            return Err(BoxError::StateError(format!(
                "Cannot attach to non-running VM process {pid}"
            )));
        }

        self.exec_client = match ExecClient::connect(&exec_socket_path).await {
            Ok(client) => Some(client),
            Err(error) => {
                tracing::debug!(
                    box_id = %self.box_id,
                    socket_path = %exec_socket_path.display(),
                    error = %error,
                    "Failed to reconnect exec client while attaching to running VM"
                );
                None
            }
        };
        self.exec_socket_path = Some(exec_socket_path);
        self.pty_socket_path = pty_socket_path;
        self.port_forward_socket_path = Some(port_forward_socket_path);
        *self.handler.write().await = Some(Box::new(handler));
        *self.state.write().await = BoxState::Ready;
        Ok(())
    }

    /// Attach this manager to an already-running Windows shim process.
    #[cfg(windows)]
    pub async fn attach_running_process(
        &mut self,
        pid: u32,
        exec_socket_path: PathBuf,
        pty_socket_path: Option<PathBuf>,
    ) -> Result<()> {
        let handler = crate::vmm::ShimHandler::from_pid(pid, self.box_id.clone());
        if !handler.is_running() {
            return Err(BoxError::StateError(format!(
                "Cannot attach to non-running VM process {pid}"
            )));
        }

        self.exec_socket_path = Some(exec_socket_path);
        self.pty_socket_path = pty_socket_path;
        self.port_forward_socket_path = None;
        *self.handler.write().await = Some(Box::new(handler));
        *self.state.write().await = BoxState::Ready;
        Ok(())
    }

    /// Get the exec socket path, if the VM has been booted.
    pub fn exec_socket_path(&self) -> Option<&Path> {
        self.exec_socket_path.as_deref()
    }

    /// Get the PTY socket path, if the VM has been booted.
    pub fn pty_socket_path(&self) -> Option<&Path> {
        self.pty_socket_path.as_deref()
    }

    /// Get the CRI port-forward socket path, if the VM has been booted.
    pub fn port_forward_socket_path(&self) -> Option<&Path> {
        self.port_forward_socket_path.as_deref()
    }

    /// Inject a custom VMM provider (e.g., a VmController with a known shim path).
    ///
    /// If set before `boot()`, the injected provider is used instead of the
    /// default `VmController::find_shim()` fallback.
    pub fn set_provider(&mut self, provider: Box<dyn VmmProvider>) {
        self.provider = Some(provider);
    }

    /// Override the rootfs provider (overlay or copy).
    ///
    /// By default, `default_provider()` auto-detects the best available provider.
    /// Call this before `boot()` to force a specific provider.
    pub fn set_rootfs_provider(&mut self, provider: Box<dyn crate::rootfs::RootfsProvider>) {
        self.rootfs_provider = provider;
    }

    /// Get the name of the active rootfs provider.
    pub fn rootfs_provider_name(&self) -> &str {
        self.rootfs_provider.name()
    }

    /// Set a progress callback for image pulls: `(current, total, digest, size_bytes)`.
    /// Called once per layer when `run` pulls an image that is not yet cached.
    pub fn set_pull_progress_fn(&mut self, f: PullProgressFn) {
        self.pull_progress_fn = Some(f);
    }

    /// Attach Prometheus metrics to this VM manager.
    pub fn set_metrics(&mut self, metrics: crate::prom::RuntimeMetrics) {
        self.prom = Some(metrics);
    }

    /// Set the logging driver config. Threaded into the InstanceSpec so the shim
    /// runs the log processor for the box's lifetime.
    pub fn set_log_config(&mut self, log_config: a3s_box_core::log::LogConfig) {
        self.log_config = log_config;
    }

    /// Set whether an image-defined health check is explicitly disabled.
    pub fn set_healthcheck_disabled(&mut self, disabled: bool) {
        self.healthcheck_disabled = disabled;
    }

    /// Get the attached Prometheus metrics (if any).
    pub fn metrics_prom(&self) -> Option<&crate::prom::RuntimeMetrics> {
        self.prom.as_ref()
    }

    /// Get the names of anonymous volumes created during boot.
    ///
    /// These are auto-created from OCI VOLUME directives and should be tracked
    /// for cleanup when the box is removed.
    pub fn anonymous_volumes(&self) -> &[String] {
        &self.anonymous_volumes
    }

    /// Get the OCI image config resolved during boot.
    pub fn image_config(&self) -> Option<&crate::oci::OciImageConfig> {
        self.image_config.as_ref()
    }

    /// Return the immutable execution resolution captured for this boot.
    pub fn resolved_execution_plan(&self) -> Option<&ResolvedExecutionPlan> {
        self.resolved_execution_plan.as_ref()
    }

    /// Get the exit code of the container, if it has exited.
    ///
    /// Returns `Some(code)` after `destroy()` has been called and the shim
    /// process exited naturally (not killed). Returns `None` if the VM has not
    /// yet stopped or the exit code could not be determined.
    pub fn exit_code(&self) -> Option<i32> {
        self.shim_exit_code
    }

    #[cfg(not(target_os = "windows"))]
    fn persisted_exit_code(&self) -> Option<i32> {
        crate::rootfs::read_persisted_exit_code(&self.home_dir.join("boxes").join(&self.box_id))
    }

    /// Poll the owned VM process for natural exit without sending a signal.
    ///
    /// This is used by foreground CLI flows where the container command may
    /// finish on its own and the CLI should clean up instead of waiting for
    /// a Ctrl-C.
    pub async fn try_wait_exit(&mut self) -> Result<Option<i32>> {
        if let Some(code) = self.shim_exit_code {
            return Ok(Some(code));
        }

        // Unix guests expose the overlay exit file only once their shutdown is
        // effectively complete. On Windows the shared-rootfs file appears while
        // the shim still has to relay guest stdout/stderr into the host logs, so
        // treating it as completion here races teardown against that relay.
        #[cfg(not(target_os = "windows"))]
        if let Some(code) = self.persisted_exit_code() {
            self.shim_exit_code = Some(code);
            return Ok(Some(code));
        }

        let mut handler = self.handler.write().await;
        let Some(handler) = handler.as_mut() else {
            return Ok(self.shim_exit_code);
        };

        if let Some(code) = handler.try_wait_exit()? {
            #[cfg(target_os = "windows")]
            let code = collect_windows_guest_result(
                &self.home_dir.join("boxes").join(&self.box_id),
                &self.log_config,
                code,
            )?;
            self.shim_exit_code = Some(code);
            return Ok(Some(code));
        }

        Ok(None)
    }

    /// Return true once the foreground container is known to have finished, even
    /// if the shim exit status has not been reaped yet.
    pub async fn has_exited(&self) -> bool {
        if self.shim_exit_code.is_some() {
            return true;
        }

        #[cfg(not(target_os = "windows"))]
        if self.persisted_exit_code().is_some() {
            return true;
        }

        self.handler
            .read()
            .await
            .as_ref()
            .map(|handler| handler.has_exited())
            .unwrap_or(false)
    }

    /// Run a command as the container MAIN in an IDLE-booted (deferred-main) VM.
    ///
    /// Sends the `spawn-main` control frame carrying `spec_json` (the command),
    /// waits for the main to exit (which halts the VM), and returns its real exit
    /// code + the box's json-file console logs split by stream. This is the full-
    /// box-semantics counterpart to [`Self::exec_command`] (whose output is piped
    /// over the exec stream, not the json-file logs).
    #[cfg(unix)]
    pub async fn run_deferred_main(
        &mut self,
        spec_json: &[u8],
        timeout: std::time::Duration,
    ) -> Result<a3s_box_core::exec::ExecOutput> {
        let log_dir = self.home_dir.join("boxes").join(&self.box_id).join("logs");
        let console_out_path = log_dir.join("console.log");
        let console_err_path = a3s_box_core::log::stderr_console_path(&console_out_path);
        let console_out_start = std::fs::metadata(&console_out_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let console_err_start = std::fs::metadata(&console_err_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);

        let acked = {
            let owned_client;
            let client = if let Some(client) = self.exec_client.as_ref() {
                client
            } else {
                let socket_path = self
                    .exec_socket_path
                    .as_deref()
                    .ok_or_else(|| BoxError::ExecError("Exec client not connected".to_string()))?;
                owned_client = Self::connect_exec_client_for_request(socket_path).await?;
                &owned_client
            };
            client.spawn_main(Some(spec_json)).await?
        };
        let exit_wait_timeout = if acked {
            timeout
        } else {
            // Very short deferred mains can exit and halt the VM before the
            // guest's ACK frame makes it back to the host. Treat a missing ACK as
            // provisional: if the VM exits promptly, the spawn succeeded and the
            // real exit code/logs are authoritative; otherwise fail quickly
            // instead of waiting the full command timeout for an IDLE VM.
            tracing::debug!(
                box_id = %self.box_id,
                "spawn-main was not acknowledged; waiting briefly for main exit"
            );
            timeout.min(std::time::Duration::from_secs(2))
        };

        // Wait for the main to exit — guest-init persists the code and halts the VM.
        let start = std::time::Instant::now();
        let exit_code = loop {
            if let Some(code) = self.try_wait_exit().await? {
                break code;
            }
            if start.elapsed() >= exit_wait_timeout {
                let message = if acked {
                    "deferred main did not exit within the timeout"
                } else {
                    "spawn-main was not acknowledged by the guest"
                };
                return Err(BoxError::ExecError(message.to_string()));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };

        // Let the shim's log processor finish draining console.log into the json
        // file (it flushes as the VM halts). A single short "stable length"
        // sample is not enough here: deferred-main can persist its exit code
        // before the final stdout/stderr bytes have reached the host tailer,
        // especially with pre-warmed pools. Require a small quiet window before
        // reading logs, bounded so no-output commands still return promptly.
        let json_path = log_dir.join("container.json");
        let drain_start = std::time::Instant::now();
        let max_wait = std::time::Duration::from_secs(2);
        let min_wait = std::time::Duration::from_millis(500);
        let quiet_window = std::time::Duration::from_millis(200);
        let mut last_len: Option<u64> = None;
        let mut last_change = drain_start;
        loop {
            let len = std::fs::metadata(&json_path).map(|m| m.len()).unwrap_or(0);
            if last_len != Some(len) {
                last_len = Some(len);
                last_change = std::time::Instant::now();
            }
            let elapsed = drain_start.elapsed();
            if elapsed >= max_wait || (elapsed >= min_wait && last_change.elapsed() >= quiet_window)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let (mut stdout, mut stderr) = self.read_container_logs();
        if stdout.is_empty() {
            stdout = Self::read_file_from_offset(&console_out_path, console_out_start);
        }
        if stderr.is_empty() {
            stderr = Self::read_file_from_offset(&console_err_path, console_err_start);
        }
        let truncated = stdout.len() > a3s_box_core::exec::MAX_OUTPUT_BYTES
            || stderr.len() > a3s_box_core::exec::MAX_OUTPUT_BYTES;
        stdout.truncate(a3s_box_core::exec::MAX_OUTPUT_BYTES);
        stderr.truncate(a3s_box_core::exec::MAX_OUTPUT_BYTES);
        Ok(a3s_box_core::exec::ExecOutput {
            stdout,
            stderr,
            exit_code,
            truncated,
        })
    }

    fn read_file_from_offset(path: &Path, offset: u64) -> Vec<u8> {
        use std::io::{Read, Seek, SeekFrom};

        let mut file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(_) => return vec![],
        };
        if file.seek(SeekFrom::Start(offset)).is_err() {
            return vec![];
        }

        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_err() {
            return vec![];
        }
        bytes
    }

    /// Read the box's json-file console logs, split into stdout/stderr by stream.
    #[cfg(unix)]
    fn read_container_logs(&self) -> (Vec<u8>, Vec<u8>) {
        let path = self
            .home_dir
            .join("boxes")
            .join(&self.box_id)
            .join("logs")
            .join("container.json");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        if let Ok(content) = std::fs::read_to_string(&path) {
            for line in content.lines() {
                if let Ok(entry) = serde_json::from_str::<a3s_box_core::log::LogEntry>(line) {
                    if entry.stream == "stderr" {
                        err.extend_from_slice(entry.log.as_bytes());
                    } else {
                        out.extend_from_slice(entry.log.as_bytes());
                    }
                }
            }
        }
        (out, err)
    }

    /// Execute a command in the guest VM.
    ///
    /// Requires the VM to be in Ready, Busy, or Compacting state.
    #[cfg(unix)]
    #[tracing::instrument(skip(self, request), fields(box_id = %self.box_id))]
    pub async fn exec_request(
        &self,
        request: &a3s_box_core::exec::ExecRequest,
    ) -> Result<a3s_box_core::exec::ExecOutput> {
        if request.cmd.is_empty() {
            return Err(BoxError::ExecError(
                "Exec request requires a non-empty command".to_string(),
            ));
        }

        let state = self.state.read().await;
        match *state {
            BoxState::Ready | BoxState::Busy | BoxState::Compacting => {}
            BoxState::Created => {
                return Err(BoxError::ExecError("VM not yet booted".to_string()));
            }
            BoxState::Stopped => {
                return Err(BoxError::ExecError("VM is stopped".to_string()));
            }
        }
        drop(state);

        let owned_client;
        let client = if let Some(client) = self.exec_client.as_ref() {
            client
        } else {
            let socket_path = self
                .exec_socket_path
                .as_deref()
                .ok_or_else(|| BoxError::ExecError("Exec client not connected".to_string()))?;
            owned_client = Self::connect_exec_client_for_request(socket_path).await?;
            &owned_client
        };

        let exec_start = std::time::Instant::now();
        let result = client.exec_command(request).await;

        // Record Prometheus metrics
        if let Some(ref prom) = self.prom {
            prom.exec_total.inc();
            prom.exec_duration
                .observe(exec_start.elapsed().as_secs_f64());
            if result.is_err() || result.as_ref().is_ok_and(|o| o.exit_code != 0) {
                prom.exec_errors_total.inc();
            }
        }

        result
    }

    /// Execute a command in the guest VM.
    ///
    /// Requires the VM to be in Ready, Busy, or Compacting state.
    #[cfg(unix)]
    #[tracing::instrument(skip(self, cmd), fields(box_id = %self.box_id))]
    pub async fn exec_command(
        &self,
        cmd: Vec<String>,
        timeout_ns: u64,
    ) -> Result<a3s_box_core::exec::ExecOutput> {
        let request = a3s_box_core::exec::ExecRequest {
            request_id: None,
            cmd,
            timeout_ns,
            env: vec![],
            working_dir: None,
            rootfs: None,
            stdin: None,
            stdin_streaming: false,
            user: None,
            streaming: false,
        };

        self.exec_request(&request).await
    }

    /// Boot the VM.
    pub async fn boot(&mut self) -> Result<()> {
        let boot_span = tracing::info_span!("vm_boot", box_id = %self.box_id);
        // Check and transition state: Created → booting
        {
            let state = self.state.read().await;
            if *state != BoxState::Created {
                return Err(BoxError::StateError("VM already booted".to_string()));
            }
        }

        let box_dir = self.home_dir.join("boxes").join(&self.box_id);
        self.preserve_rootfs_on_boot_failure =
            self.config.persistent && layout::persistent_rootfs_generation_exists(&box_dir)?;

        let execution_plan = a3s_box_core::resolve_execution(&self.config)?;
        self.resolved_execution_plan = Some(execution_plan.clone());
        if execution_plan.backend == ExecutionBackend::Crun {
            let boot_start = std::time::Instant::now();
            return self
                .boot_sandbox(execution_plan, &boot_span, boot_start)
                .await;
        }

        let boot_start = std::time::Instant::now();

        tracing::info!(parent: &boot_span, box_id = %self.box_id, "Booting VM");

        // 1. Prepare filesystem layout
        let layout = match self
            .prepare_layout()
            .instrument(tracing::info_span!(parent: &boot_span, "prepare_layout"))
            .await
        {
            Ok(layout) => layout,
            Err(error) => {
                self.cleanup_boot_failure().await;
                return Err(error);
            }
        };
        self.image_config = layout.oci_config.clone();

        // `prepare_layout` may only now have mounted a Snapshot lower through
        // this box's overlay. Stage via the exact guest-visible root so rename
        // copy-ups into the per-box upper before any guest process can launch.
        if let Err(error) = a3s_box_core::rootfs_metadata::stage_terminal_rootfs_metadata_for_boot(
            &layout.rootfs_path,
        ) {
            self.cleanup_boot_failure().await;
            return Err(BoxError::IoError(error));
        }

        // 1.5. Override /etc/resolv.conf with configured DNS
        let resolv_content = a3s_box_core::dns::generate_resolv_conf(&self.config.dns);
        if let Err(e) = crate::oci::rootfs::write_guest_file(
            &layout.rootfs_path,
            "etc/resolv.conf",
            &resolv_content,
        ) {
            self.cleanup_boot_failure().await;
            return Err(e);
        }
        tracing::debug!(parent: &boot_span, dns = %resolv_content.trim(), "Configured guest DNS");

        // 1.6. Apply hostname and static hosts entries before the VM starts.
        if let Err(e) = self.write_hostname_file(&layout) {
            self.cleanup_boot_failure().await;
            return Err(e);
        }
        if let Err(e) = self.write_standalone_hosts_file(&layout) {
            self.cleanup_boot_failure().await;
            return Err(e);
        }

        // 2. Build InstanceSpec
        let mut spec = match self.build_instance_spec(&layout) {
            Ok(s) => s,
            Err(e) => {
                self.cleanup_boot_failure().await;
                return Err(e);
            }
        };

        // 2.5. Configure bridge networking if requested
        let bridge_network = match &self.config.network {
            a3s_box_core::NetworkMode::Bridge { network } => Some(network.clone()),
            _ => None,
        };
        if let Some(network_name) = bridge_network {
            let net_config = match self.setup_bridge_network(&network_name) {
                Ok(n) => n,
                Err(e) => {
                    self.cleanup_boot_failure().await;
                    return Err(e);
                }
            };

            // Write /etc/hosts for DNS service discovery
            match self.write_hosts_file(&layout, &network_name) {
                Ok(()) => (),
                Err(e) => {
                    self.cleanup_boot_failure().await;
                    return Err(e);
                }
            };

            // Inject network env vars into entrypoint so they are passed via
            // krun_set_exec's envp (not krun_set_env which overwrites all vars).
            let ip_cidr = format!("{}/{}", net_config.ip_address, net_config.prefix_len);
            spec.entrypoint
                .env
                .push(("A3S_NET_IP".to_string(), ip_cidr));
            spec.entrypoint.env.push((
                "A3S_NET_GATEWAY".to_string(),
                net_config.gateway.to_string(),
            ));
            spec.entrypoint.env.push((
                "A3S_NET_DNS".to_string(),
                net_config
                    .dns_servers
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            ));

            spec.network = Some(net_config);
        }

        #[cfg(target_os = "macos")]
        if spec.network.is_none()
            && matches!(self.config.network, a3s_box_core::NetworkMode::Tsi)
            && !self.config.port_map.is_empty()
        {
            let net_config = match self.setup_published_default_network() {
                Ok(network) => network,
                Err(error) => {
                    self.cleanup_boot_failure().await;
                    return Err(error);
                }
            };
            let ip_cidr = format!("{}/{}", net_config.ip_address, net_config.prefix_len);
            spec.entrypoint
                .env
                .push(("A3S_NET_IP".to_string(), ip_cidr));
            spec.entrypoint.env.push((
                "A3S_NET_GATEWAY".to_string(),
                net_config.gateway.to_string(),
            ));
            spec.entrypoint.env.push((
                "A3S_NET_DNS".to_string(),
                net_config
                    .dns_servers
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            ));
            spec.network = Some(net_config);
        }

        // 3. Initialize VMM provider (use injected provider or default to VmController)
        if self.provider.is_none() {
            let shim_path = match VmController::find_shim() {
                Ok(p) => p,
                Err(e) => {
                    self.cleanup_boot_failure().await;
                    return Err(e);
                }
            };
            let controller = match VmController::new(shim_path) {
                Ok(c) => c,
                Err(e) => {
                    self.cleanup_boot_failure().await;
                    return Err(e);
                }
            };
            self.provider = Some(Box::new(controller));
        }

        // 4. Start VM via provider
        let handler = {
            let provider = self
                .provider
                .as_ref()
                .ok_or_else(|| BoxError::BoxBootError {
                    message: "VMM provider not initialized".to_string(),
                    hint: Some("Ensure VmManager has a provider set before boot".to_string()),
                })?;
            let vm_start_span = tracing::info_span!(parent: &boot_span, "vm_start");
            match async { provider.start(&spec).await }
                .instrument(vm_start_span)
                .await
            {
                Ok(h) => h,
                Err(e) => {
                    self.cleanup_boot_failure().await;
                    return Err(e);
                }
            }
        };

        // Store handler
        *self.handler.write().await = Some(handler);

        // 5. Wait for guest ready
        {
            let wait_span = tracing::info_span!(parent: &boot_span, "wait_for_ready");
            if let Err(e) = async {
                self.wait_for_vm_running().await?;

                // 5b. Become ready. A snapshot-restore boot resumes an already-booted
                // guest whose exec server won't re-signal readiness, so the cold-boot
                // wait would stall registration on its safety cap — do one best-effort
                // probe instead. A normal boot waits for the Heartbeat health check.
                #[cfg(unix)]
                if is_restore_mode(&self.config) {
                    self.probe_exec_ready_once(&layout.exec_socket_path).await;
                } else {
                    self.wait_for_exec_ready(&layout.exec_socket_path).await?;
                }
                Ok::<(), BoxError>(())
            }
            .instrument(wait_span)
            .await
            {
                self.cleanup_boot_failure().await;
                return Err(e);
            }
        }

        // Prototype: deferred-main-spawn. The guest booted IDLE (BOX_DEFERRED_MAIN);
        // now that the exec server is ready, tell it to spawn the container command
        // (already passed via BOX_EXEC_*) as the MAIN process — full box semantics
        // (exit code + json-file console logs) without a cold boot.
        // Auto-trigger spawn-main only for the env-driven `run` path, where the
        // command is known at boot. The pool sets config.deferred_main to boot the
        // VM IDLE but drives spawn-main EXPLICITLY per request (the per-request
        // command isn't known at pre-warm), so a pool VM must NOT auto-trigger here.
        // A restored guest's main is ALREADY running (captured in the snapshot), so
        // it must never re-spawn — doing so would start a duplicate main.
        #[cfg(unix)]
        if !is_restore_mode(&self.config)
            && std::env::var("BOX_DEFERRED_MAIN")
                .map(|v| v == "1")
                .unwrap_or(false)
        {
            if let Some(client) = self.exec_client.as_ref() {
                match client.spawn_main(None).await {
                    Ok(true) => tracing::info!("deferred container main spawned"),
                    Ok(false) => tracing::warn!("deferred spawn-main not acknowledged"),
                    Err(e) => tracing::warn!(error = %e, "deferred spawn-main failed"),
                }
            }
        }

        // 5b2. Store socket paths for CRI streaming access
        self.exec_socket_path = Some(layout.exec_socket_path.clone());
        self.pty_socket_path = Some(layout.pty_socket_path.clone());
        self.port_forward_socket_path = Some(layout.port_forward_socket_path.clone());

        // 5c. Initialize TEE extension for TEE environments
        #[cfg(unix)]
        if !matches!(self.config.tee, TeeConfig::None) {
            self.tee = Some(Box::new(crate::tee::SnpTeeExtension::new(
                self.box_id.clone(),
                layout.attest_socket_path.clone(),
            )));
        }

        // 6. Update state to Ready
        *self.state.write().await = BoxState::Ready;

        // Record Prometheus metrics
        if let Some(ref prom) = self.prom {
            let boot_duration = boot_start.elapsed().as_secs_f64();
            prom.vm_boot_duration.observe(boot_duration);
            prom.vm_created_total.inc();
            prom.vm_count.with_label_values(&["ready"]).inc();
        }

        // Emit ready event
        self.event_emitter.emit(BoxEvent::empty("box.ready"));

        tracing::info!(parent: &boot_span, box_id = %self.box_id, "VM ready");

        Ok(())
    }

    /// Destroy the VM with the default shutdown timeout and SIGTERM.
    pub async fn destroy(&mut self) -> Result<()> {
        self.destroy_with_options(default_stop_signal(), DEFAULT_SHUTDOWN_TIMEOUT_MS)
            .await
    }

    /// Destroy the VM with a custom shutdown timeout and SIGTERM.
    pub async fn destroy_with_timeout(&mut self, timeout_ms: u64) -> Result<()> {
        self.destroy_with_options(default_stop_signal(), timeout_ms)
            .await
    }

    /// Destroy the VM with a specific stop signal and timeout.
    ///
    /// Sends `signal` to the shim process and waits up to `timeout_ms` for it
    /// to exit gracefully before sending SIGKILL.
    #[tracing::instrument(skip(self), fields(box_id = %self.box_id))]
    pub async fn destroy_with_options(&mut self, signal: i32, timeout_ms: u64) -> Result<()> {
        let preserve_rootfs = self.config.persistent;
        self.destroy_with_rootfs_policy(signal, timeout_ms, preserve_rootfs)
            .await
    }

    /// Stop the runtime while retaining its writable rootfs for a managed
    /// restart or filesystem-only pause.
    pub(crate) async fn destroy_preserving_rootfs_with_options(
        &mut self,
        signal: i32,
        timeout_ms: u64,
    ) -> Result<()> {
        self.destroy_with_rootfs_policy(signal, timeout_ms, true)
            .await
    }

    pub(crate) async fn destroy_preserving_rootfs(&mut self) -> Result<()> {
        self.destroy_with_rootfs_policy(default_stop_signal(), DEFAULT_SHUTDOWN_TIMEOUT_MS, true)
            .await
    }

    async fn destroy_with_rootfs_policy(
        &mut self,
        signal: i32,
        timeout_ms: u64,
        preserve_rootfs: bool,
    ) -> Result<()> {
        let mut state = self.state.write().await;

        if *state == BoxState::Stopped {
            return Ok(());
        }

        tracing::info!(box_id = %self.box_id, signal, timeout_ms, "Destroying VM");

        // Mark as stopped first — ensures state is correct even if handler.stop() fails.
        *state = BoxState::Stopped;

        // Stop the VM handler and capture its exit code before it's dropped.
        // A stop failure must NOT skip the host-resource teardown below (network
        // backend, overlay unmount, socket + box dirs) — those are already
        // best-effort and would otherwise leak on every wedged stop. Capture the
        // error and surface it after teardown instead of returning early.
        let mut stop_error = None;
        if let Some(mut handler) = self.handler.write().await.take() {
            #[cfg(windows)]
            let stop_request = match windows_stop::stage(&self.socket_dir(), signal) {
                Ok(path) => {
                    tracing::debug!(
                        box_id = %self.box_id,
                        signal,
                        path = %path.display(),
                        "Staged Windows guest stop request"
                    );
                    Some(path)
                }
                Err(error) => {
                    tracing::warn!(
                        box_id = %self.box_id,
                        signal,
                        error = %error,
                        "Failed to stage Windows guest stop request; force-stop fallback remains active"
                    );
                    None
                }
            };

            #[cfg(windows)]
            let handler_timeout_ms = if timeout_ms == 0 {
                0
            } else if let Some(request) = stop_request.as_deref() {
                let delivery_started = std::time::Instant::now();
                let delivery_timeout = std::time::Duration::from_millis(
                    timeout_ms.min(WINDOWS_STOP_DELIVERY_TIMEOUT_MS),
                );
                let delivered =
                    match windows_stop::wait_until_delivered(request, delivery_timeout).await {
                        Ok(delivered) => delivered,
                        Err(error) => {
                            tracing::warn!(
                                box_id = %self.box_id,
                                error = %error,
                                "Failed while waiting for Windows guest stop request delivery"
                            );
                            false
                        }
                    };
                let delivery_elapsed_ms =
                    u64::try_from(delivery_started.elapsed().as_millis()).unwrap_or(u64::MAX);
                let remaining_timeout_ms = timeout_ms.saturating_sub(delivery_elapsed_ms);
                if delivered {
                    let finalization_timeout_ms = if self.config.persistent {
                        WINDOWS_GUEST_FINALIZATION_TIMEOUT_MS
                    } else {
                        0
                    };
                    let handler_timeout_ms =
                        remaining_timeout_ms.saturating_add(finalization_timeout_ms);
                    tracing::debug!(
                        box_id = %self.box_id,
                        delivery_elapsed_ms,
                        handler_timeout_ms,
                        "Delivered Windows stop request to the guest"
                    );
                    handler_timeout_ms
                } else {
                    tracing::warn!(
                        box_id = %self.box_id,
                        delivery_elapsed_ms,
                        "Windows guest stop request was not delivered before the forwarding deadline"
                    );
                    remaining_timeout_ms
                }
            } else {
                timeout_ms
            };
            #[cfg(not(windows))]
            let handler_timeout_ms = timeout_ms;

            let _handler_stopped = match handler.stop(signal, handler_timeout_ms) {
                Ok(()) => true,
                Err(e) => {
                    tracing::error!(box_id = %self.box_id, error = %e, "Failed to stop VM handler; continuing teardown");
                    stop_error = Some(e);
                    false
                }
            };
            self.shim_exit_code = handler.exit_code();

            #[cfg(windows)]
            if stop_request.is_some() {
                if let Err(error) = windows_stop::clear(&self.socket_dir()) {
                    tracing::warn!(
                        box_id = %self.box_id,
                        error = %error,
                        "Failed to clear Windows guest stop request"
                    );
                }
            }

            #[cfg(windows)]
            if _handler_stopped && self.config.persistent {
                let rootfs = self
                    .home_dir
                    .join("boxes")
                    .join(&self.box_id)
                    .join("rootfs");
                match a3s_box_core::rootfs_metadata::finalize_terminal_rootfs_metadata(&rootfs) {
                    Ok(true) => tracing::info!(
                        box_id = %self.box_id,
                        path = %rootfs.display(),
                        "Published terminal rootfs metadata after Windows guest exit"
                    ),
                    Ok(false) => tracing::debug!(
                        box_id = %self.box_id,
                        path = %rootfs.display(),
                        "No Windows terminal rootfs metadata required host finalization"
                    ),
                    Err(error) => tracing::warn!(
                        box_id = %self.box_id,
                        path = %rootfs.display(),
                        error = %error,
                        "Refused to publish invalid Windows terminal rootfs metadata"
                    ),
                }
            }
        }

        // Stop network backend if running
        if let Some(ref mut net) = self.net_manager {
            net.stop();
        }
        self.net_manager = None;

        // Cleanup rootfs provider (unmount overlay if applicable)
        let box_dir = self.home_dir.join("boxes").join(&self.box_id);
        if let Err(e) = self.rootfs_provider.cleanup(&box_dir, preserve_rootfs) {
            tracing::warn!(
                box_id = %self.box_id,
                error = %e,
                "Failed to cleanup rootfs provider"
            );
        }

        let socket_dir = self.socket_dir();
        if let Err(e) = std::fs::remove_dir_all(&socket_dir) {
            tracing::debug!(
                box_id = %self.box_id,
                path = %socket_dir.display(),
                error = %e,
                "Failed to cleanup VM socket directory"
            );
        }

        // Remove the box working directory itself (overlay upper/work, logs,
        // leftover metadata) for non-persistent boxes. Without this, ephemeral
        // CRI pods leak their `boxes/<id>` directory on every destroy; the
        // accumulation slows later RunPodSandbox calls until they time out
        // (observed: pod #21 after churning 20). Persistent boxes keep their
        // dir intentionally.
        if !preserve_rootfs {
            match std::fs::remove_dir_all(&box_dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(
                        box_id = %self.box_id,
                        path = %box_dir.display(),
                        error = %e,
                        "Failed to remove box directory on destroy"
                    );
                }
            }
        }

        // Record Prometheus metrics
        if let Some(ref prom) = self.prom {
            prom.vm_destroyed_total.inc();
            prom.vm_count.with_label_values(&["ready"]).dec();
        }

        // Emit stopped event
        self.event_emitter.emit(BoxEvent::empty("box.stopped"));

        // Host teardown above is complete; surface a handler-stop failure now so
        // the caller still learns the stop was imperfect.
        match stop_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Transition to busy state.
    pub async fn set_busy(&self) -> Result<()> {
        let mut state = self.state.write().await;

        if *state != BoxState::Ready {
            return Err(BoxError::StateError("VM not ready".to_string()));
        }

        *state = BoxState::Busy;
        Ok(())
    }

    /// Transition back to ready state.
    pub async fn set_ready(&self) -> Result<()> {
        let mut state = self.state.write().await;

        if *state != BoxState::Busy && *state != BoxState::Compacting {
            return Err(BoxError::StateError("Invalid state transition".to_string()));
        }

        *state = BoxState::Ready;
        Ok(())
    }

    /// Transition to compacting state.
    pub async fn set_compacting(&self) -> Result<()> {
        let mut state = self.state.write().await;

        if *state != BoxState::Busy {
            return Err(BoxError::StateError("VM not busy".to_string()));
        }

        *state = BoxState::Compacting;
        Ok(())
    }

    /// Pause the VM by sending SIGSTOP to the shim process.
    ///
    /// The VM must be in Ready, Busy, or Compacting state.
    #[cfg(unix)]
    pub async fn pause(&self) -> Result<()> {
        let state = self.state.read().await;
        match *state {
            BoxState::Ready | BoxState::Busy | BoxState::Compacting => {}
            BoxState::Created => {
                return Err(BoxError::StateError("VM not yet booted".to_string()));
            }
            BoxState::Stopped => {
                return Err(BoxError::StateError("VM is stopped".to_string()));
            }
        }
        drop(state);

        if self
            .resolved_execution_plan
            .as_ref()
            .is_some_and(|plan| plan.backend == ExecutionBackend::Crun)
            || self.config.isolation.is_sandbox()
        {
            return Err(BoxError::StateError(
                "Pause is not supported by the Sandbox backend yet".to_string(),
            ));
        }

        if let Some(pid) = self.pid().await {
            // Safety: sending SIGSTOP to pause the process
            let ret = unsafe { libc::kill(pid as i32, libc::SIGSTOP) };
            if ret != 0 {
                let err = std::io::Error::last_os_error();
                return Err(BoxError::ExecError(format!(
                    "Failed to send SIGSTOP to pid {}: {}",
                    pid, err
                )));
            }
            tracing::info!(box_id = %self.box_id, pid, "VM paused");
            Ok(())
        } else {
            Err(BoxError::StateError(
                "VM has no running process".to_string(),
            ))
        }
    }

    /// Resume the VM by sending SIGCONT to the shim process.
    ///
    /// Can be called on a paused VM to resume execution.
    #[cfg(unix)]
    pub async fn resume(&self) -> Result<()> {
        if self
            .resolved_execution_plan
            .as_ref()
            .is_some_and(|plan| plan.backend == ExecutionBackend::Crun)
            || self.config.isolation.is_sandbox()
        {
            return Err(BoxError::StateError(
                "Resume is not supported by the Sandbox backend yet".to_string(),
            ));
        }
        if let Some(pid) = self.pid().await {
            // Safety: sending SIGCONT to resume the process
            let ret = unsafe { libc::kill(pid as i32, libc::SIGCONT) };
            if ret != 0 {
                let err = std::io::Error::last_os_error();
                return Err(BoxError::ExecError(format!(
                    "Failed to send SIGCONT to pid {}: {}",
                    pid, err
                )));
            }
            tracing::info!(box_id = %self.box_id, pid, "VM resumed");
            Ok(())
        } else {
            Err(BoxError::StateError(
                "VM has no running process".to_string(),
            ))
        }
    }

    /// Pause the VM (Windows stub - not yet implemented).
    #[cfg(windows)]
    pub async fn pause(&self) -> Result<()> {
        Err(BoxError::StateError(
            "VM pause is not yet supported on Windows".to_string(),
        ))
    }

    /// Resume the VM (Windows stub - not yet implemented).
    #[cfg(windows)]
    pub async fn resume(&self) -> Result<()> {
        Err(BoxError::StateError(
            "VM resume is not yet supported on Windows".to_string(),
        ))
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
        let vm_metrics = self
            .handler
            .read()
            .await
            .as_ref()
            .map(|handler| handler.metrics())?;

        // Update per-VM Prometheus gauges if metrics are attached
        if let Some(ref prom) = self.prom {
            prom.vm_cpu_percent
                .with_label_values(&[&self.box_id])
                .set(vm_metrics.cpu_percent.unwrap_or(0.0) as f64);
            prom.vm_memory_bytes
                .with_label_values(&[&self.box_id])
                .set(vm_metrics.memory_bytes.unwrap_or(0) as f64);
        }

        Some(vm_metrics)
    }

    /// Get the PID of the VM shim process.
    pub async fn pid(&self) -> Option<u32> {
        self.handler
            .read()
            .await
            .as_ref()
            .map(|handler| handler.pid())
    }

    /// Get the TEE extension, if TEE is configured and VM is booted.
    #[cfg(unix)]
    pub fn tee(&self) -> Option<&dyn TeeExtension> {
        self.tee.as_deref()
    }

    /// Get the TEE extension or return an error.
    #[cfg(unix)]
    pub fn require_tee(&self) -> Result<&dyn TeeExtension> {
        self.tee.as_deref().ok_or_else(|| {
            BoxError::AttestationError("TEE is not configured for this box".to_string())
        })
    }

    /// Apply a live resource update to the running VM.
    ///
    /// Tier 1 changes (vCPU count, memory size) are rejected with a clear error
    /// because libkrun does not expose a hot-resize API.
    ///
    /// Tier 2 changes (cgroup-based limits) are applied by executing shell
    /// commands inside the guest that write to cgroup v2 control files.
    #[cfg(unix)]
    pub async fn update_resources(
        &self,
        update: &crate::resize::ResourceUpdate,
    ) -> Result<crate::resize::ResizeResult> {
        if self
            .resolved_execution_plan
            .as_ref()
            .is_some_and(|plan| plan.backend == ExecutionBackend::Crun)
            || self.config.isolation.is_sandbox()
        {
            return Err(BoxError::StateError(
                "Live resource updates are not supported by the Sandbox backend yet".to_string(),
            ));
        }
        // Reject Tier 1 changes upfront
        crate::resize::validate_update(update)?;

        let mut result = crate::resize::ResizeResult {
            applied: Vec::new(),
            rejected: Vec::new(),
        };

        if !update.has_tier2_changes() {
            return Ok(result);
        }

        // Build cgroup commands and execute them inside the guest
        let commands = update.build_cgroup_commands();
        for cmd_str in &commands {
            let shell_cmd = vec!["sh".to_string(), "-c".to_string(), cmd_str.clone()];

            match self.exec_command(shell_cmd, 5_000_000_000).await {
                Ok(output) if output.exit_code == 0 => {
                    result.applied.push(cmd_str.clone());
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let reason = if stderr.trim().is_empty() {
                        format!("exit code {}", output.exit_code)
                    } else {
                        stderr.trim().to_string()
                    };
                    tracing::warn!(
                        box_id = %self.box_id,
                        cmd = %cmd_str,
                        exit_code = output.exit_code,
                        stderr = %stderr,
                        "Cgroup update failed inside guest"
                    );
                    result.rejected.push((cmd_str.clone(), reason));
                }
                Err(e) => {
                    tracing::warn!(
                        box_id = %self.box_id,
                        cmd = %cmd_str,
                        error = %e,
                        "Failed to exec cgroup update in guest"
                    );
                    result.rejected.push((cmd_str.clone(), e.to_string()));
                }
            }
        }

        Ok(result)
    }
}

/// Whether this boot is a snapshot-fork restore (the guest is resumed already-booted
/// rather than cold-booted). PER-VM: a pool / fork daemon sets `config.restore_from`
/// so one process can restore different VMs; the single-VM `run` path uses the
/// `KRUN_RESTORE_FROM` env. Either source means restore mode.
#[cfg(unix)]
fn is_restore_mode(config: &BoxConfig) -> bool {
    config
        .restore_from
        .as_deref()
        .is_some_and(|s| !s.is_empty())
        || std::env::var("KRUN_RESTORE_FROM")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
}

/// Simple FNV-1a hash for generating short deterministic hashes from strings.
pub(crate) fn fnv1a_hash(input: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in input.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(unix)]
fn default_stop_signal() -> i32 {
    libc::SIGTERM
}

#[cfg(windows)]
fn default_stop_signal() -> i32 {
    15
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::event::EventEmitter;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    struct RecordingHandler {
        stopped: Arc<AtomicBool>,
    }

    impl VmHandler for RecordingHandler {
        fn stop(&mut self, _signal: i32, _timeout_ms: u64) -> Result<()> {
            self.stopped.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn metrics(&self) -> crate::vmm::VmMetrics {
            crate::vmm::VmMetrics::default()
        }

        fn is_running(&self) -> bool {
            true
        }

        fn pid(&self) -> u32 {
            42
        }
    }

    struct ExitStateHandler {
        exited: bool,
    }

    impl VmHandler for ExitStateHandler {
        fn stop(&mut self, _signal: i32, _timeout_ms: u64) -> Result<()> {
            Ok(())
        }

        fn metrics(&self) -> crate::vmm::VmMetrics {
            crate::vmm::VmMetrics::default()
        }

        fn is_running(&self) -> bool {
            !self.exited
        }

        fn has_exited(&self) -> bool {
            self.exited
        }

        fn pid(&self) -> u32 {
            42
        }
    }

    struct CompletedHandler {
        code: i32,
    }

    impl VmHandler for CompletedHandler {
        fn stop(&mut self, _signal: i32, _timeout_ms: u64) -> Result<()> {
            Ok(())
        }

        fn metrics(&self) -> crate::vmm::VmMetrics {
            crate::vmm::VmMetrics::default()
        }

        fn is_running(&self) -> bool {
            false
        }

        fn has_exited(&self) -> bool {
            true
        }

        fn pid(&self) -> u32 {
            42
        }

        fn try_wait_exit(&mut self) -> Result<Option<i32>> {
            Ok(Some(self.code))
        }
    }

    /// A handler whose `stop` always fails — models a wedged VM that won't halt.
    struct FailingHandler;

    impl VmHandler for FailingHandler {
        fn stop(&mut self, _signal: i32, _timeout_ms: u64) -> Result<()> {
            Err(BoxError::StateError("simulated stop failure".to_string()))
        }

        fn metrics(&self) -> crate::vmm::VmMetrics {
            crate::vmm::VmMetrics::default()
        }

        fn is_running(&self) -> bool {
            true
        }

        fn pid(&self) -> u32 {
            42
        }
    }

    // Regression: a handler-stop failure must still run the host teardown
    // (overlay unmount, socket + box dirs). Pre-fix, destroy_with_options
    // returned early on the stop error and leaked the box directory on every
    // wedged stop.
    #[tokio::test]
    async fn test_destroy_runs_host_teardown_even_when_handler_stop_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let box_id = "box-stopfail".to_string();
        // persistent=false (the default) → the box dir is removed on destroy.
        let mut vm =
            VmManager::with_box_id(BoxConfig::default(), EventEmitter::new(16), box_id.clone());
        vm.home_dir = tmp.path().to_path_buf();
        *vm.handler.write().await = Some(Box::new(FailingHandler));

        let box_dir = tmp.path().join("boxes").join(&box_id);
        std::fs::create_dir_all(box_dir.join("logs")).unwrap();

        let result = vm.destroy_with_options(default_stop_signal(), 100).await;

        // The stop failure is still surfaced to the caller...
        assert!(
            result.is_err(),
            "a handler-stop failure must still be reported"
        );
        // ...but the host teardown ran anyway: handler taken + box dir removed.
        assert!(vm.handler.read().await.is_none());
        assert!(
            !box_dir.exists(),
            "non-persistent box dir must be removed even when the stop failed"
        );
    }

    #[tokio::test]
    async fn test_cleanup_boot_failure_stops_handler_and_removes_created_volumes() {
        let tmp = tempfile::tempdir().unwrap();
        let box_id = "box-test".to_string();
        let mut vm =
            VmManager::with_box_id(BoxConfig::default(), EventEmitter::new(16), box_id.clone());
        vm.home_dir = tmp.path().to_path_buf();
        vm.anonymous_volumes = vec!["created-volume".to_string(), "reused-volume".to_string()];
        vm.created_anonymous_volumes = vec!["created-volume".to_string()];

        let stopped = Arc::new(AtomicBool::new(false));
        *vm.handler.write().await = Some(Box::new(RecordingHandler {
            stopped: stopped.clone(),
        }));

        let box_dir = tmp.path().join("boxes").join(&box_id);
        std::fs::create_dir_all(box_dir.join("logs")).unwrap();

        let store = crate::volume::VolumeStore::new(
            tmp.path().join("volumes.json"),
            tmp.path().join("volumes"),
        );
        store
            .create(a3s_box_core::volume::VolumeConfig::new(
                "created-volume",
                "",
            ))
            .unwrap();
        store
            .create(a3s_box_core::volume::VolumeConfig::new("reused-volume", ""))
            .unwrap();

        vm.cleanup_boot_failure().await;

        assert!(stopped.load(Ordering::SeqCst));
        assert!(vm.handler.read().await.is_none());
        assert!(vm.created_anonymous_volumes.is_empty());
        assert_eq!(vm.anonymous_volumes, vec!["reused-volume".to_string()]);
        assert!(store.get("created-volume").unwrap().is_none());
        assert!(store.get("reused-volume").unwrap().is_some());
        assert!(!box_dir.exists());
    }

    #[tokio::test]
    async fn test_cleanup_boot_failure_preserves_persistent_rootfs() {
        let tmp = tempfile::tempdir().unwrap();
        let box_id = "box-persistent-boot-failure".to_string();
        let config = BoxConfig {
            persistent: true,
            ..BoxConfig::default()
        };
        let mut vm = VmManager::with_box_id(config, EventEmitter::new(16), box_id.clone());
        vm.home_dir = tmp.path().to_path_buf();
        vm.set_rootfs_provider(Box::new(crate::rootfs::CopyProvider));

        let box_dir = tmp.path().join("boxes").join(&box_id);
        let sentinel = box_dir.join("rootfs/var/lib/application/data.db");
        std::fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
        std::fs::write(&sentinel, b"persistent guest data").unwrap();
        std::fs::create_dir_all(vm.socket_dir()).unwrap();
        std::fs::write(vm.socket_dir().join("stale.sock"), b"stale").unwrap();
        vm.preserve_rootfs_on_boot_failure = true;

        vm.cleanup_boot_failure().await;

        assert_eq!(
            std::fs::read(&sentinel).unwrap(),
            b"persistent guest data",
            "a failed restart must not erase the persistent writable rootfs"
        );
        assert!(box_dir.exists());
        assert!(
            !vm.socket_dir().exists(),
            "transient sockets should still be removed after a failed restart"
        );
    }

    #[tokio::test]
    async fn test_cleanup_boot_failure_discards_partial_first_persistent_rootfs() {
        let tmp = tempfile::tempdir().unwrap();
        let box_id = "box-partial-first-boot".to_string();
        let config = BoxConfig {
            persistent: true,
            ..BoxConfig::default()
        };
        let mut vm = VmManager::with_box_id(config, EventEmitter::new(16), box_id.clone());
        vm.home_dir = tmp.path().to_path_buf();
        vm.set_rootfs_provider(Box::new(crate::rootfs::CopyProvider));

        let box_dir = tmp.path().join("boxes").join(&box_id);
        let partial = box_dir.join("rootfs/partially-extracted");
        std::fs::create_dir_all(partial.parent().unwrap()).unwrap();
        std::fs::write(&partial, b"incomplete").unwrap();

        vm.cleanup_boot_failure().await;

        assert!(box_dir.exists(), "a retained box keeps its host directory");
        assert!(
            !box_dir.join("rootfs").exists(),
            "a failed first boot must discard its incomplete rootfs generation"
        );
    }

    #[tokio::test]
    async fn test_wait_for_vm_running_returns_error_when_handler_exited() {
        let vm = VmManager::with_box_id(
            BoxConfig::default(),
            EventEmitter::new(16),
            "box-exited".to_string(),
        );
        *vm.handler.write().await = Some(Box::new(ExitStateHandler { exited: true }));

        let err = vm.wait_for_vm_running().await.unwrap_err();

        assert!(err
            .to_string()
            .contains("VM process exited immediately after start"));
    }

    #[tokio::test]
    async fn test_wait_for_vm_running_succeeds_when_handler_stays_running() {
        let config = BoxConfig {
            restore_from: Some("snapshot-path".to_string()),
            ..BoxConfig::default()
        };
        let vm = VmManager::with_box_id(config, EventEmitter::new(16), "box-running".to_string());
        *vm.handler.write().await = Some(Box::new(ExitStateHandler { exited: false }));

        vm.wait_for_vm_running().await.unwrap();
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn test_try_wait_exit_reads_guest_persisted_exit_code() {
        let tmp = tempfile::tempdir().unwrap();
        let box_id = "box-exit-code".to_string();
        let mut vm =
            VmManager::with_box_id(BoxConfig::default(), EventEmitter::new(16), box_id.clone());
        vm.home_dir = tmp.path().to_path_buf();

        let exit_path = tmp
            .path()
            .join("boxes")
            .join(&box_id)
            .join("upper")
            .join(".a3s_exit_code");
        std::fs::create_dir_all(exit_path.parent().unwrap()).unwrap();
        std::fs::write(&exit_path, "42\n").unwrap();

        assert_eq!(vm.try_wait_exit().await.unwrap(), Some(42));
        assert_eq!(vm.exit_code(), Some(42));
        assert!(vm.has_exited().await);
    }

    #[tokio::test]
    async fn test_try_wait_exit_reads_windows_rootfs_exit_code() {
        let tmp = tempfile::tempdir().unwrap();
        let box_id = "box-windows-exit-code".to_string();
        let mut vm =
            VmManager::with_box_id(BoxConfig::default(), EventEmitter::new(16), box_id.clone());
        vm.home_dir = tmp.path().to_path_buf();
        *vm.handler.write().await = Some(Box::new(CompletedHandler { code: 23 }));

        let exit_path = tmp
            .path()
            .join("boxes")
            .join(&box_id)
            .join("rootfs")
            .join(".a3s_exit_code");
        std::fs::create_dir_all(exit_path.parent().unwrap()).unwrap();
        std::fs::write(&exit_path, "23\n").unwrap();
        #[cfg(target_os = "windows")]
        {
            std::fs::write(
                exit_path.parent().unwrap().join(WINDOWS_GUEST_STDOUT),
                "guest stdout\n",
            )
            .unwrap();
            std::fs::write(
                exit_path.parent().unwrap().join(WINDOWS_GUEST_STDERR),
                concat!(
                    "init.krun: mount_filesystems ok\n",
                    "init.krun: execvp(/bin/app) starting\n",
                    "guest stderr\n",
                ),
            )
            .unwrap();
        }

        assert_eq!(vm.try_wait_exit().await.unwrap(), Some(23));
        assert_eq!(vm.exit_code(), Some(23));
        assert!(vm.has_exited().await);
        #[cfg(target_os = "windows")]
        {
            let logs = tmp.path().join("boxes").join(&box_id).join("logs");
            assert_eq!(
                std::fs::read_to_string(logs.join("console.log")).unwrap(),
                "guest stdout\n"
            );
            assert_eq!(
                std::fs::read_to_string(logs.join("console.err.log")).unwrap(),
                "guest stderr\n"
            );
            let json = std::fs::read_to_string(logs.join("container.json")).unwrap();
            assert!(json.contains("\"log\":\"guest stdout\\n\""));
            assert!(json.contains("\"log\":\"guest stderr\\n\""));
            assert!(!json.contains("init.krun"));
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn test_append_windows_guest_stream_uses_shared_phase_and_keeps_partial_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let stdout_source = tmp.path().join("stdout.source");
        let stderr_source = tmp.path().join("stderr.source");
        let stdout_destination = tmp.path().join("stdout.destination");
        let stderr_destination = tmp.path().join("stderr.destination");
        std::fs::write(
            &stdout_source,
            concat!(
                "init.krun: mount_filesystems ok\n",
                "init.krun: business\n",
                "init.krun: config parsed",
            ),
        )
        .unwrap();
        std::fs::write(
            &stderr_source,
            concat!(
                "init.krun: execvp(/bin/app) starting\n",
                "init.krun: mount_filesystems ok\n",
            ),
        )
        .unwrap();

        let filter = a3s_box_core::log::RuntimeConsoleFilter::new();
        append_windows_guest_stream(&stdout_source, &stdout_destination, &filter).unwrap();
        append_windows_guest_stream(&stderr_source, &stderr_destination, &filter).unwrap();

        assert_eq!(
            std::fs::read_to_string(stdout_destination).unwrap(),
            "init.krun: business\ninit.krun: config parsed"
        );
        assert_eq!(
            std::fs::read_to_string(stderr_destination).unwrap(),
            "init.krun: mount_filesystems ok\n"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn test_collect_windows_guest_result_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let box_dir = tmp.path().join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::write(rootfs.join(WINDOWS_GUEST_STDOUT), "once\n").unwrap();
        std::fs::write(rootfs.join(WINDOWS_GUEST_STDERR), "error once\n").unwrap();
        std::fs::write(rootfs.join(WINDOWS_GUEST_EXIT_CODE), "7\n").unwrap();

        let config = a3s_box_core::log::LogConfig::default();
        assert_eq!(
            collect_windows_guest_result(&box_dir, &config, 0).unwrap(),
            7
        );
        assert_eq!(
            collect_windows_guest_result(&box_dir, &config, 0).unwrap(),
            7
        );

        let logs = box_dir.join("logs");
        assert_eq!(
            std::fs::read_to_string(logs.join("console.log")).unwrap(),
            "once\n"
        );
        assert_eq!(
            std::fs::read_to_string(logs.join("console.err.log")).unwrap(),
            "error once\n"
        );
        let json = std::fs::read_to_string(logs.join("container.json")).unwrap();
        assert_eq!(json.matches("\"log\":\"once\\n\"").count(), 1);
        assert_eq!(json.matches("\"log\":\"error once\\n\"").count(), 1);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn test_collect_windows_guest_result_does_not_replay_drained_live_logs() {
        let tmp = tempfile::tempdir().unwrap();
        let box_dir = tmp.path().join("box");
        let rootfs = box_dir.join("rootfs");
        let logs = box_dir.join("logs");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(rootfs.join(WINDOWS_GUEST_STDOUT), "live once\n").unwrap();
        std::fs::write(rootfs.join(WINDOWS_GUEST_STDERR), "live error once\n").unwrap();
        std::fs::write(rootfs.join(WINDOWS_GUEST_EXIT_CODE), "4\n").unwrap();
        std::fs::write(rootfs.join(WINDOWS_LIVE_LOGS_DRAINED_MARKER), "drained\n").unwrap();
        let live_json =
            "{\"log\":\"live once\\n\",\"stream\":\"stdout\",\"time\":\"2026-01-01T00:00:00Z\"}\n";
        std::fs::write(logs.join("container.json"), live_json).unwrap();

        let config = a3s_box_core::log::LogConfig::default();
        assert_eq!(
            collect_windows_guest_result(&box_dir, &config, 0).unwrap(),
            4
        );

        assert_eq!(
            std::fs::read_to_string(logs.join("container.json")).unwrap(),
            live_json
        );
        assert_eq!(
            std::fs::read_to_string(logs.join("console.log")).unwrap(),
            "live once\n"
        );
        assert_eq!(
            std::fs::read_to_string(logs.join("console.err.log")).unwrap(),
            "live error once\n"
        );
        assert!(rootfs.join(WINDOWS_GUEST_RESULT_MARKER).exists());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn test_collect_windows_guest_result_replaces_marker_symlink_without_touching_target() {
        let tmp = tempfile::tempdir().unwrap();
        let box_dir = tmp.path().join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::write(rootfs.join(WINDOWS_GUEST_STDOUT), "safe output\n").unwrap();
        std::fs::write(rootfs.join(WINDOWS_GUEST_STDERR), "").unwrap();
        std::fs::write(rootfs.join(WINDOWS_GUEST_EXIT_CODE), "0\n").unwrap();

        let host_target = tmp.path().join("host-target.txt");
        std::fs::write(&host_target, "host secret").unwrap();
        let marker = rootfs.join(WINDOWS_GUEST_RESULT_MARKER);
        match std::os::windows::fs::symlink_file(&host_target, &marker) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(1314) => return,
            Err(error) => panic!("failed to create marker symlink: {error}"),
        }

        let config = a3s_box_core::log::LogConfig::default();
        assert_eq!(
            collect_windows_guest_result(&box_dir, &config, 0).unwrap(),
            0
        );
        assert_eq!(
            std::fs::read_to_string(&host_target).unwrap(),
            "host secret"
        );
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "collected\n");
        assert!(!std::fs::symlink_metadata(marker)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn test_collect_windows_guest_result_refuses_stream_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let box_dir = tmp.path().join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let host_secret = tmp.path().join("host-secret.txt");
        std::fs::write(&host_secret, "must not be logged\n").unwrap();
        match std::os::windows::fs::symlink_file(&host_secret, rootfs.join(WINDOWS_GUEST_STDOUT)) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(1314) => return,
            Err(error) => panic!("failed to create stream symlink: {error}"),
        }
        std::fs::write(rootfs.join(WINDOWS_GUEST_STDERR), "").unwrap();
        std::fs::write(rootfs.join(WINDOWS_GUEST_EXIT_CODE), "0\n").unwrap();

        let config = a3s_box_core::log::LogConfig::default();
        let error = collect_windows_guest_result(&box_dir, &config, 0)
            .unwrap_err()
            .to_string();

        assert!(error.contains("Failed to collect Windows guest output"));
        let console = box_dir.join("logs").join("console.log");
        assert!(
            !console.exists()
                || !std::fs::read_to_string(console)
                    .unwrap()
                    .contains("must not")
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn test_collect_windows_guest_result_rejects_false_success() {
        let tmp = tempfile::tempdir().unwrap();
        let box_dir = tmp.path().join("box");
        std::fs::create_dir_all(box_dir.join("rootfs")).unwrap();
        let config = a3s_box_core::log::LogConfig::default();

        let error = collect_windows_guest_result(&box_dir, &config, 0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("before the guest persisted its exit code"));
        assert_eq!(
            collect_windows_guest_result(&box_dir, &config, 9).unwrap(),
            9
        );
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn test_windows_exit_file_waits_for_shim_log_relay() {
        let tmp = tempfile::tempdir().unwrap();
        let box_id = "box-windows-pending-relay".to_string();
        let mut vm =
            VmManager::with_box_id(BoxConfig::default(), EventEmitter::new(16), box_id.clone());
        vm.home_dir = tmp.path().to_path_buf();
        *vm.handler.write().await = Some(Box::new(RecordingHandler {
            stopped: Arc::new(AtomicBool::new(false)),
        }));

        let exit_path = tmp
            .path()
            .join("boxes")
            .join(&box_id)
            .join("rootfs")
            .join(".a3s_exit_code");
        std::fs::create_dir_all(exit_path.parent().unwrap()).unwrap();
        std::fs::write(exit_path, "0\n").unwrap();

        assert_eq!(vm.try_wait_exit().await.unwrap(), None);
        assert_eq!(vm.exit_code(), None);
        assert!(!vm.has_exited().await);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_wait_for_exec_ready_returns_when_handler_already_exited() {
        let mut vm = VmManager::with_box_id(
            BoxConfig::default(),
            EventEmitter::new(16),
            "box-exec-exited".to_string(),
        );
        *vm.handler.write().await = Some(Box::new(ExitStateHandler { exited: true }));
        let tmp = tempfile::tempdir().unwrap();

        vm.wait_for_exec_ready(&tmp.path().join("missing-exec.sock"))
            .await
            .unwrap();

        assert!(vm.exec_client.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_wait_for_exec_ready_returns_when_guest_exit_code_persisted() {
        let tmp = tempfile::tempdir().unwrap();
        let box_id = "box-exec-finished".to_string();
        let mut vm =
            VmManager::with_box_id(BoxConfig::default(), EventEmitter::new(16), box_id.clone());
        vm.home_dir = tmp.path().to_path_buf();

        let exit_path = tmp
            .path()
            .join("boxes")
            .join(&box_id)
            .join("upper")
            .join(".a3s_exit_code");
        std::fs::create_dir_all(exit_path.parent().unwrap()).unwrap();
        std::fs::write(&exit_path, "17\n").unwrap();

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            vm.wait_for_exec_ready(&tmp.path().join("missing-exec.sock")),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(vm.exit_code(), Some(17));
        assert!(vm.exec_client.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_probe_exec_ready_once_ignores_missing_socket() {
        let mut vm = VmManager::with_box_id(
            BoxConfig::default(),
            EventEmitter::new(16),
            "box-probe".to_string(),
        );
        let tmp = tempfile::tempdir().unwrap();

        vm.probe_exec_ready_once(&tmp.path().join("missing-exec.sock"))
            .await;

        assert!(vm.exec_client.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_attach_running_process_infers_port_forward_socket_path() {
        let mut vm = VmManager::with_box_id(
            BoxConfig::default(),
            EventEmitter::new(16),
            "box-test".to_string(),
        );
        let tmp = tempfile::tempdir().unwrap();
        let exec_socket_path = tmp.path().join("exec.sock");
        let pty_socket_path = Some(tmp.path().join("pty.sock"));

        vm.attach_running_process(
            std::process::id(),
            exec_socket_path.clone(),
            pty_socket_path.clone(),
        )
        .await
        .unwrap();

        assert_eq!(vm.exec_socket_path(), Some(exec_socket_path.as_path()));
        assert_eq!(vm.pty_socket_path(), pty_socket_path.as_deref());
        assert_eq!(
            vm.port_forward_socket_path(),
            Some(exec_socket_path.with_file_name("portfwd.sock").as_path())
        );
        assert_eq!(vm.state().await, BoxState::Ready);
    }
}
