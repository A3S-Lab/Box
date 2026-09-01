//! VM Manager - Lifecycle management for MicroVM instances.

mod boot;
mod execution;
mod layout;
mod lifecycle;
mod maintenance;
mod network;
mod oci_microvm;
mod ready;
pub mod reap;
mod sandbox;
mod spec;
#[cfg(windows)]
mod windows_stop;

pub(crate) use layout::{
    legacy_sandbox_runtime_root, persistent_rootfs_generation_exists, runtime_socket_dir,
    sandbox_runtime_root,
};
pub use maintenance::archive_stopped_guest_native_rootfs;

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Callback type for image pull progress: `(current, total, digest, size_bytes)`.
pub type PullProgressFn = Arc<dyn Fn(usize, usize, &str, i64) + Send + Sync>;

use a3s_box_core::config::BoxConfig;
#[cfg(unix)]
use a3s_box_core::config::TeeConfig;
use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::event::{BoxEvent, EventEmitter};
use a3s_box_core::execution::ResolvedExecutionPlan;
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum VmBootMode {
    #[default]
    Workload,
    RootfsMaintenance,
}

/// Layout of directories for a box instance.
pub(crate) struct BoxLayout {
    /// Host staging path for a fresh directory-derived generation.
    ///
    /// This path is intentionally non-authoritative when `resumed_rootfs` is
    /// present; directly assembled and resumed guest-owned disks have no host
    /// directory view.
    pub(crate) rootfs_path: PathBuf,
    /// Guest-owned generation finalized before the directory staging boundary.
    /// This covers direct OCI assembly and persistent restart.
    pub(crate) resumed_rootfs: Option<crate::rootfs::ResumedRootfs>,
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
    /// Exact resolved OCI manifest behind a fresh image-derived generation.
    /// Snapshot and externally prebuilt roots intentionally have no reusable
    /// base identity until their own generation protocol is implemented.
    #[cfg(target_os = "macos")]
    pub(crate) oci_manifest_digest: Option<String>,
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

pub(crate) const TERMINAL_EXIT_POLL_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(25);
pub(crate) const TERMINAL_EXIT_POLL_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(5);

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

/// Read the durable workload status without treating it as provider completion.
///
/// The WHPX guest writes this file before libkrun necessarily returns to the
/// shim. Readiness and boot cleanup use its presence only to distinguish a
/// completed one-shot from a live guest that never became ready; normal wait
/// paths still wait for the shim to finish relaying logs before collecting it.
#[cfg(target_os = "windows")]
fn windows_guest_persisted_exit_code(box_dir: &Path) -> Option<i32> {
    use std::io::Read;

    let exit_path = box_dir.join("rootfs").join(WINDOWS_GUEST_EXIT_CODE);
    let (file, _) = a3s_box_core::windows_file::open_regular_file(&exit_path, None).ok()?;
    let mut contents = String::new();
    file.take(64).read_to_string(&mut contents).ok()?;
    contents.trim().parse::<i32>().ok()
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

    /// Internal boot contract. Maintenance never becomes persisted box state.
    boot_mode: VmBootMode,

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

    /// Exact rootfs cache entry paired with a snapshot-fork template.
    ///
    /// The template's guest memory and filesystem must come from the same
    /// resolved image even when its configured tag moves later.
    pub(crate) restore_rootfs_cache_key: Option<String>,

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

    /// Rootfs preparation and transport provider.
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

    /// Runtime-owned tmpfs root whose regular files may be prepared for the
    /// Sandbox user namespace. Arbitrary external bind mounts remain immutable.
    pub(crate) managed_secret_root: Option<PathBuf>,

    /// One in-memory registry authorization selected for this boot attempt.
    /// It is consumed and zeroized while preparing the image layout.
    pub(crate) transient_registry_auth: Option<crate::oci::RegistryAuth>,
}

impl VmManager {
    /// Create a new VM manager.
    pub fn new(config: BoxConfig, event_emitter: EventEmitter) -> Self {
        let box_id = uuid::Uuid::new_v4().to_string();
        let home_dir = a3s_box_core::dirs_home();
        let rootfs_provider =
            crate::rootfs::default_provider_for_boot(rootfs_snapshot_requested(&config));

        Self {
            config,
            box_id,
            boot_mode: VmBootMode::Workload,
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
            restore_rootfs_cache_key: None,
            healthcheck_disabled: false,
            preserve_rootfs_on_boot_failure: false,
            #[cfg(unix)]
            tee: None,
            rootfs_provider,
            exec_socket_path: None,
            pty_socket_path: None,
            port_forward_socket_path: None,
            prom: None,
            shim_exit_code: None,
            pull_progress_fn: None,
            log_config: a3s_box_core::log::LogConfig::default(),
            resolved_execution_plan: None,
            managed_secret_root: None,
            transient_registry_auth: None,
        }
    }

    /// Create a new VM manager with a specific box ID.
    pub fn with_box_id(config: BoxConfig, event_emitter: EventEmitter, box_id: String) -> Self {
        let home_dir = a3s_box_core::dirs_home();
        let rootfs_provider = crate::rootfs::default_provider_for_box_boot(
            &home_dir.join("boxes").join(&box_id),
            rootfs_snapshot_requested(&config),
        );

        Self {
            config,
            box_id,
            boot_mode: VmBootMode::Workload,
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
            restore_rootfs_cache_key: None,
            healthcheck_disabled: false,
            preserve_rootfs_on_boot_failure: false,
            #[cfg(unix)]
            tee: None,
            rootfs_provider,
            exec_socket_path: None,
            pty_socket_path: None,
            port_forward_socket_path: None,
            prom: None,
            shim_exit_code: None,
            pull_progress_fn: None,
            log_config: a3s_box_core::log::LogConfig::default(),
            resolved_execution_plan: None,
            managed_secret_root: None,
            transient_registry_auth: None,
        }
    }

    /// Remove host-side boot artifacts after a failed boot attempt.
    async fn cleanup_boot_failure(&mut self) {
        let box_dir = self.home_dir.join("boxes").join(&self.box_id);

        #[cfg(target_os = "windows")]
        let guest_exit_before_cleanup = windows_guest_persisted_exit_code(&box_dir);

        if let Some(mut handler) = self.handler.write().await.take() {
            // A short-lived workload can finish before the runtime publishes
            // its readiness endpoint. That is a normal terminal completion,
            // not a failed rootfs build. Preserve its writable generation so a
            // managed restart observes the same persistent filesystem while
            // ephemeral mounts are recreated by the next runtime generation.
            // Process termination and publication of the exact wait result are
            // separate events. Try once before cleanup, let `stop` collect its
            // owned child, then wait within the common terminal bound only when
            // the workload was already known to have exited naturally. A live
            // boot failure stopped by Box must not be reclassified as success.
            let exited_before_cleanup = handler.has_exited();
            let collected_before_cleanup = match handler.try_wait_exit() {
                Ok(Some(exit_code)) => {
                    self.shim_exit_code = Some(exit_code);
                    true
                }
                Ok(None) => false,
                Err(error) => {
                    tracing::debug!(
                        box_id = %self.box_id,
                        error = %error,
                        "Failed to collect a terminal status before boot cleanup"
                    );
                    false
                }
            };
            if let Err(error) = handler.stop(default_stop_signal(), DEFAULT_SHUTDOWN_TIMEOUT_MS) {
                tracing::warn!(
                    box_id = %self.box_id,
                    error = %error,
                    "Failed to stop VM handler after boot failure"
                );
            }
            let provider_exit_code = handler.exit_code().or(self.shim_exit_code);
            #[cfg(not(target_os = "windows"))]
            {
                self.shim_exit_code =
                    crate::rootfs::resolve_workload_exit_code(&box_dir, provider_exit_code);
            }
            #[cfg(target_os = "windows")]
            {
                let completed_before_cleanup = collected_before_cleanup
                    || exited_before_cleanup
                    || guest_exit_before_cleanup.is_some();
                if completed_before_cleanup {
                    let fallback_exit_code = guest_exit_before_cleanup.or(provider_exit_code);
                    if let Some(fallback_exit_code) = fallback_exit_code {
                        match collect_windows_guest_result(
                            &box_dir,
                            &self.log_config,
                            fallback_exit_code,
                        ) {
                            Ok(exit_code) => self.shim_exit_code = Some(exit_code),
                            Err(error) => {
                                tracing::warn!(
                                    box_id = %self.box_id,
                                    error = %error,
                                    "Failed to collect the completed Windows guest during boot cleanup"
                                );
                                self.shim_exit_code =
                                    guest_exit_before_cleanup.or(provider_exit_code);
                            }
                        }
                    } else {
                        // A provider can report process exit before its owned
                        // child status becomes collectable. Keep the status
                        // pending so the delayed terminal poll below can reap it.
                        self.shim_exit_code = None;
                    }
                } else {
                    self.shim_exit_code = provider_exit_code;
                }
            }
            if exited_before_cleanup && self.shim_exit_code.is_none() {
                self.shim_exit_code =
                    wait_for_delayed_terminal_exit(handler.as_mut(), &box_dir, &self.box_id).await;
            }
            let completed_before_cleanup = collected_before_cleanup || exited_before_cleanup;
            #[cfg(target_os = "windows")]
            let completed_before_cleanup =
                completed_before_cleanup || guest_exit_before_cleanup.is_some();
            if self.config.persistent && self.shim_exit_code.is_some() && completed_before_cleanup {
                self.preserve_rootfs_on_boot_failure = true;
            }
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
            if let Err(error) = store.remove_anonymous(volume_name, &self.box_id) {
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
        let mount_aliases_clean = match self.cleanup_sandbox_mount_aliases() {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(
                    box_id = %self.box_id,
                    %error,
                    "Failed to cleanup Sandbox attachment aliases after boot failure"
                );
                false
            }
        };

        // Reap the box's passt daemon (Linux bridge mode) BEFORE removing its
        // socket dir. A boot that fails after passt spawned but before
        // `self.net_manager` was assigned leaves `net_manager.stop()` a no-op, so
        // passt would otherwise survive holding the published port — the
        // "Address already in use" on the next start. terminate_passt reads
        // `socket_dir/passt.pid` and is a no-op when there is no passt.
        #[cfg(target_os = "linux")]
        crate::network::terminate_passt(&self.socket_dir());

        let preserve_rootfs_on_boot_failure = self.preserve_rootfs_on_boot_failure
            || self.rootfs_provider.preserve_on_boot_failure(&box_dir);
        if let Err(error) = self
            .rootfs_provider
            .cleanup(&box_dir, preserve_rootfs_on_boot_failure)
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
        if !self.config.persistent && mount_aliases_clean {
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

    fn cleanup_sandbox_mount_aliases(&self) -> Result<()> {
        if self.config.isolation.is_sandbox() {
            crate::sandbox::cleanup_sandbox_mount_aliases(&self.home_dir, &self.box_id)
        } else {
            Ok(())
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
        let rootfs_provider =
            crate::rootfs::default_provider_for_boot(rootfs_snapshot_requested(&config));
        Self {
            config,
            box_id,
            boot_mode: VmBootMode::Workload,
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
            restore_rootfs_cache_key: None,
            healthcheck_disabled: false,
            preserve_rootfs_on_boot_failure: false,
            #[cfg(unix)]
            tee: None,
            rootfs_provider,
            exec_socket_path: None,
            pty_socket_path: None,
            port_forward_socket_path: None,
            prom: None,
            shim_exit_code: None,
            pull_progress_fn: None,
            log_config: a3s_box_core::log::LogConfig::default(),
            resolved_execution_plan: None,
            managed_secret_root: None,
            transient_registry_auth: None,
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
}

/// Whether this launch couples the rootfs to a VMM memory snapshot.
///
/// The provider must be selected before layout preparation, so keep every
/// snapshot entry point in one predicate. `KRUN_RESTORE_FROM` remains the
/// compatibility input for the single-VM restore path.
fn rootfs_snapshot_requested(config: &BoxConfig) -> bool {
    if config.snapshot_mem_file.is_some()
        || config.snapshot_sock.is_some()
        || config.restore_from.is_some()
    {
        return true;
    }

    #[cfg(unix)]
    {
        std::env::var_os("KRUN_RESTORE_FROM").is_some_and(|value| !value.as_os_str().is_empty())
    }

    #[cfg(not(unix))]
    {
        false
    }
}

/// Whether the vendored libkrun snapshot state contract exists for this build.
///
/// Its serialized VM/vCPU/device state is intentionally compiled only for
/// Linux KVM on x86_64. Detect this before layout preparation so unsupported
/// hosts do not pull an image, allocate file-backed RAM, or create a temporary
/// rootfs transport for an operation that cannot produce a restorable state.
pub(crate) const fn native_snapshot_fork_supported() -> bool {
    cfg!(all(target_os = "linux", target_arch = "x86_64"))
}

fn validate_snapshot_launch(config: &BoxConfig) -> Result<()> {
    let memory = config
        .snapshot_mem_file
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| snapshot_env_nonempty("KRUN_SNAPSHOT_MEM_FILE"));
    let trigger = config
        .snapshot_sock
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| snapshot_env_nonempty("KRUN_SNAPSHOT_SOCK"));
    let restore = config
        .restore_from
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| snapshot_env_nonempty("KRUN_RESTORE_FROM"));

    validate_snapshot_launch_shape(memory.is_some(), trigger.is_some(), restore.is_some())
}

fn validate_snapshot_launch_shape(memory: bool, trigger: bool, restore: bool) -> Result<()> {
    let requested = memory || trigger || restore;
    if !requested {
        return Ok(());
    }
    if !native_snapshot_fork_supported() {
        return Err(BoxError::ConfigError(
            "native VM snapshot-fork is supported only by the Linux x86_64 KVM build".to_string(),
        ));
    }
    match (memory, trigger, restore) {
        (true, true, false) | (true, false, true) => Ok(()),
        _ => Err(BoxError::ConfigError(
            "invalid native VM snapshot configuration: template mode requires memory + trigger socket, while restore mode requires memory + state file"
                .to_string(),
        )),
    }
}

fn snapshot_env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
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

async fn wait_for_delayed_terminal_exit(
    handler: &mut dyn VmHandler,
    box_dir: &Path,
    box_id: &str,
) -> Option<i32> {
    let deadline = tokio::time::Instant::now() + TERMINAL_EXIT_POLL_TIMEOUT;
    let mut reported_wait_error = false;
    loop {
        if let Some(exit_code) = handler.exit_code() {
            return Some(exit_code);
        }
        if let Some(exit_code) = boot_failure_persisted_exit_code(box_dir) {
            return Some(exit_code);
        }
        match handler.try_wait_exit() {
            Ok(Some(exit_code)) => return Some(exit_code),
            Ok(None) => {}
            Err(error) => {
                if !reported_wait_error {
                    tracing::debug!(
                        %box_id,
                        %error,
                        "Terminal status remained unavailable after boot cleanup"
                    );
                    reported_wait_error = true;
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(TERMINAL_EXIT_POLL_INTERVAL).await;
    }
}

#[cfg(not(target_os = "windows"))]
fn boot_failure_persisted_exit_code(box_dir: &Path) -> Option<i32> {
    crate::rootfs::read_persisted_exit_code(box_dir)
}

#[cfg(target_os = "windows")]
fn boot_failure_persisted_exit_code(box_dir: &Path) -> Option<i32> {
    windows_guest_persisted_exit_code(box_dir)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
