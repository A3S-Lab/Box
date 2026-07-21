//! CLI command definitions and dispatch.

mod attach;
mod attest;
mod audit;
mod build;
mod commit;
pub(crate) mod common;
mod compose;
mod container_update;
mod cp;
mod create;
mod df;
pub(crate) mod diff;
mod events;
pub(crate) mod exec;
mod export;
mod history;
mod image_inspect;
mod image_prune;
mod image_tag;
mod images;
mod import;
mod info;
mod inject_secret;
mod inspect;
mod kill;
mod load;
mod login;
mod logout;
mod logs;
mod monitor;
mod monitor_metrics;
mod monitor_service;
pub(crate) mod network;
mod pause;
mod pool;
mod port;
mod prune;
mod ps;
mod pull;
mod push;
mod rename;
mod restart;
mod rm;
mod rmi;
mod run;
mod save;
mod seal;
mod shell;
mod snapshot;
mod start;
mod stats;
mod stop;
mod system_prune;
mod top;
mod unpause;
mod unseal;
mod version;
pub mod volume;
mod wait;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Environment variable to override the image cache size limit.
///
/// Accepts human-readable sizes: `500m`, `10g`, `1t`, etc.
const IMAGE_CACHE_SIZE_ENV: &str = "A3S_IMAGE_CACHE_SIZE";

/// A3S Box — Docker-like MicroVM runtime.
#[derive(Parser)]
#[command(name = "a3s-box", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// Available commands.
#[derive(Subcommand)]
pub enum Command {
    /// Create and start a new box from an image
    Run(run::RunArgs),
    /// Create a new box without starting it
    Create(create::CreateArgs),
    /// Start one or more eligible boxes
    Start(start::StartArgs),
    /// Gracefully stop one or more running boxes
    Stop(stop::StopArgs),
    /// Restart one or more boxes
    Restart(restart::RestartArgs),
    /// Remove one or more boxes
    Rm(rm::RmArgs),
    /// Force-kill one or more running boxes
    Kill(kill::KillArgs),
    /// Pause one or more running boxes
    Pause(pause::PauseArgs),
    /// Unpause one or more paused boxes
    Unpause(unpause::UnpauseArgs),
    /// List boxes
    Ps(ps::PsArgs),
    /// Display resource usage statistics
    Stats(stats::StatsArgs),
    /// View box logs
    Logs(logs::LogsArgs),
    /// Execute a command in a running box
    Exec(exec::ExecArgs),
    /// Display running processes in a box
    Top(top::TopArgs),
    /// Display detailed box information
    Inspect(inspect::InspectArgs),
    /// Attach to a running box's console output
    Attach(attach::AttachArgs),
    /// Request and verify a TEE attestation report from a running box
    Attest(attest::AttestArgs),
    /// View the audit log
    Audit(audit::AuditArgs),
    /// Seal (encrypt) data bound to a TEE's identity
    Seal(seal::SealArgs),
    /// Unseal (decrypt) data inside a TEE
    Unseal(unseal::UnsealArgs),
    /// Inject secrets into a running TEE box via RA-TLS
    InjectSecret(inject_secret::InjectSecretArgs),
    /// Block until one or more boxes stop
    Wait(wait::WaitArgs),
    /// Rename a box
    Rename(rename::RenameArgs),
    /// List port mappings for a box
    Port(port::PortArgs),
    /// Export a box's filesystem to a tar archive
    Export(export::ExportArgs),
    /// Create an image from a box's changes
    Commit(commit::CommitArgs),
    /// Show filesystem changes in a box
    Diff(diff::DiffArgs),
    /// Stream real-time system events
    Events(events::EventsArgs),
    /// Update resource limits of a box
    ContainerUpdate(container_update::ContainerUpdateArgs),
    /// Manage multi-container workloads with a compose file
    Compose(compose::ComposeArgs),
    /// Manage VM snapshots (create, restore, list, remove)
    Snapshot(snapshot::SnapshotArgs),
    /// Build an image from a Dockerfile or Containerfile
    Build(build::BuildArgs),
    /// List cached images
    Images(images::ImagesArgs),
    /// Pull an image from a registry
    Pull(pull::PullArgs),
    /// Push an image to a registry
    Push(push::PushArgs),
    /// Log in to a container registry
    Login(login::LoginArgs),
    /// Log out from a container registry
    Logout(logout::LogoutArgs),
    /// Remove one or more cached images
    Rmi(rmi::RmiArgs),
    /// Display detailed image information as JSON
    ImageInspect(image_inspect::ImageInspectArgs),
    /// Show image layer history
    History(history::HistoryArgs),
    /// Remove unused images
    ImagePrune(image_prune::ImagePruneArgs),
    /// Create a tag that refers to an existing image
    Tag(image_tag::ImageTagArgs),
    /// Save an image to a tar archive
    Save(save::SaveArgs),
    /// Load an image from a tar archive
    Load(load::LoadArgs),
    /// Import a rootfs tarball as a single-layer image
    Import(import::ImportArgs),
    /// Copy files between host and a running box
    Cp(cp::CpArgs),
    /// Manage networks
    Network(network::NetworkArgs),
    /// Manage volumes
    Volume(volume::VolumeArgs),
    /// Show disk usage
    Df(df::DfArgs),
    /// Remove all stopped boxes (Docker `container prune`)
    #[command(visible_alias = "container-prune")]
    Prune(prune::PruneArgs),
    /// Remove all unused data (stopped boxes and unused images)
    SystemPrune(system_prune::SystemPruneArgs),
    /// Show version information
    Version(version::VersionArgs),
    /// Show system information
    Info(info::InfoArgs),
    /// Background daemon that monitors and restarts dead boxes
    Monitor(monitor::MonitorArgs),
    /// Manage the warm VM pool (pre-boot VMs for instant start)
    Pool(pool::PoolArgs),
    /// Open an interactive shell in a running box
    Shell(shell::ShellArgs),
}

/// Return the path to the image store directory (~/.a3s/images).
pub(crate) fn images_dir() -> PathBuf {
    a3s_box_core::dirs_home().join("images")
}

/// Open the shared image store.
///
/// The cache size limit can be configured via the `A3S_IMAGE_CACHE_SIZE`
/// environment variable (e.g., `500m`, `20g`). Defaults to 10 GB.
pub(crate) fn open_image_store() -> Result<a3s_box_runtime::ImageStore, Box<dyn std::error::Error>>
{
    let dir = images_dir();
    let max_size = match std::env::var(IMAGE_CACHE_SIZE_ENV) {
        Ok(val) => crate::output::parse_size_bytes(&val).map_err(|e| {
            format!("Invalid {IMAGE_CACHE_SIZE_ENV}={val:?}: {e} (examples: 500m, 10g, 1t)")
        })?,
        Err(_) => a3s_box_runtime::DEFAULT_IMAGE_CACHE_SIZE,
    };
    let store = a3s_box_runtime::ImageStore::new(&dir, max_size)?;
    Ok(store)
}

/// Resolve a box's on-disk full root filesystem directory.
///
/// The overlay provider (default on Linux) materializes the rootfs at
/// `<box_dir>/merged`, while the plain/copy provider uses `<box_dir>/rootfs`.
/// Returns the first that exists and is non-empty so that `export`/`commit`
/// work regardless of provider. Returns `None` if neither is available (e.g.
/// the overlay is unmounted because the box is stopped).
pub(crate) fn resolve_box_rootfs(box_dir: &std::path::Path) -> Option<PathBuf> {
    let is_populated = |p: &std::path::Path| -> bool {
        p.is_dir()
            && std::fs::read_dir(p)
                .map(|mut it| it.next().is_some())
                .unwrap_or(false)
    };
    let merged = box_dir.join("merged");
    if is_populated(&merged) {
        return Some(merged);
    }
    let rootfs = box_dir.join("rootfs");
    let apfs_data = rootfs.join(".a3s-rootfs");
    if apfs_data.is_dir() {
        return Some(apfs_data);
    }
    if rootfs.is_dir() {
        return Some(rootfs);
    }
    None
}

/// Wait for a stream file to exist, then continuously print new data.
/// `to_stderr` preserves the workload's stdout/stderr identity.
pub(crate) async fn tail_file_stream_positioned(
    path: &std::path::Path,
    to_stderr: bool,
    position: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    runtime_filter: Option<std::sync::Arc<a3s_box_core::log::RuntimeConsoleFilter>>,
) {
    use std::sync::atomic::Ordering;
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    // Foreground `run` supplies a position tracker so it can wait until the
    // terminal has consumed the final console bytes. Poll that latency-sensitive
    // path promptly; long-lived `logs -f` / `attach` tails retain the lower-rate
    // idle polling cadence.
    let eof_poll = if position.is_some() {
        tokio::time::Duration::from_millis(20)
    } else {
        tokio::time::Duration::from_millis(200)
    };

    // Wait for file to exist
    loop {
        if path.exists() {
            break;
        }
        if stop
            .as_ref()
            .is_some_and(|stop| stop.load(Ordering::Acquire))
        {
            return;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    #[cfg(target_os = "windows")]
    let (mut file, windows_identity) = {
        let Ok((file, identity)) = a3s_box_core::windows_file::open_regular_file(path, None) else {
            return;
        };
        (tokio::fs::File::from_std(file), identity)
    };
    #[cfg(not(target_os = "windows"))]
    let mut file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(_) => return,
    };

    let mut buf = vec![0u8; 4096];
    let mut pos: u64 = 0;
    let mut noise_filter = RuntimeNoiseLineFilter::new(runtime_filter);
    loop {
        match file.read(&mut buf).await {
            Ok(0) => {
                if stop
                    .as_ref()
                    .is_some_and(|stop| stop.load(Ordering::Acquire))
                {
                    noise_filter.finish(|bytes| write_terminal_bytes(bytes, to_stderr));
                    return;
                }

                #[cfg(target_os = "windows")]
                {
                    // Shared-rootfs writers can append through a handle whose
                    // updates stay invisible to a reader already at EOF. Open
                    // a fresh handle and resume from the consumed byte offset.
                    tokio::time::sleep(eof_poll).await;
                    if let Ok((replacement, _)) =
                        a3s_box_core::windows_file::open_regular_file(path, Some(windows_identity))
                    {
                        let mut replacement = tokio::fs::File::from_std(replacement);
                        let visible_len = replacement
                            .metadata()
                            .await
                            .map(|metadata| metadata.len())
                            .unwrap_or(pos);
                        let replacement_pos = if visible_len < pos { 0 } else { pos };
                        if replacement
                            .seek(std::io::SeekFrom::Start(replacement_pos))
                            .await
                            .is_ok()
                        {
                            file = replacement;
                            pos = replacement_pos;
                            if let Some(position) = &position {
                                position.store(pos, Ordering::Relaxed);
                            }
                        }
                    }
                    continue;
                }

                // Detect truncation/rotation: if our read offset is past the
                // file's current end, the shim bounded the raw console under us
                // (see core::log::console_truncate_if_over). Re-read from the
                // start so we keep streaming instead of freezing forever at a
                // stale offset.
                #[cfg(not(target_os = "windows"))]
                if let Ok(meta) = file.metadata().await {
                    if pos > meta.len() {
                        pos = file.seek(std::io::SeekFrom::Start(0)).await.unwrap_or(0);
                        if let Some(position) = &position {
                            position.store(pos, Ordering::Relaxed);
                        }
                    }
                }
                #[cfg(not(target_os = "windows"))]
                tokio::time::sleep(eof_poll).await;
            }
            Ok(n) => {
                pos += n as u64;
                noise_filter.push(&buf[..n], |bytes| write_terminal_bytes(bytes, to_stderr));
                if let Some(position) = &position {
                    position.store(pos, Ordering::Relaxed);
                }
            }
            Err(_) => {
                noise_filter.finish(|bytes| write_terminal_bytes(bytes, to_stderr));
                break;
            }
        }
    }
}

fn write_terminal_bytes(bytes: &[u8], to_stderr: bool) {
    use std::io::Write as _;

    if to_stderr {
        let mut err = std::io::stderr();
        let _ = err.write_all(bytes);
        let _ = err.flush();
    } else {
        let mut out = std::io::stdout();
        let _ = out.write_all(bytes);
        let _ = out.flush();
    }
}

struct RuntimeNoiseLineFilter {
    runtime_filter: Option<std::sync::Arc<a3s_box_core::log::RuntimeConsoleFilter>>,
    state: RuntimeNoiseLineState,
}

enum RuntimeNoiseLineState {
    /// Buffer only while the beginning of a line could still be the runtime
    /// noise prefix. This is bounded by `RUNTIME_NOISE_PREFIX.len()`.
    Probe(Vec<u8>),
    /// The line cannot be runtime noise, so stream it without further delay.
    Passthrough,
    /// The runtime prefix matched. Buffer the bounded candidate until its
    /// newline so the strict core grammar can classify the complete line.
    Candidate(Vec<u8>),
}

impl RuntimeNoiseLineFilter {
    const RUNTIME_NOISE_PREFIX: &'static [u8] = b"init.krun:";
    const MAX_CANDIDATE_LEN: usize = 64 * 1024;

    fn new(
        runtime_filter: Option<std::sync::Arc<a3s_box_core::log::RuntimeConsoleFilter>>,
    ) -> Self {
        Self {
            runtime_filter,
            state: RuntimeNoiseLineState::Probe(Vec::new()),
        }
    }

    fn push(&mut self, bytes: &[u8], mut emit: impl FnMut(&[u8])) {
        if self.runtime_filter.is_none() {
            emit(bytes);
            return;
        }

        let mut offset = 0;
        while offset < bytes.len() {
            if self
                .runtime_filter
                .as_ref()
                .is_some_and(|filter| !filter.preamble_active())
            {
                self.disable_and_flush(&mut emit);
                emit(&bytes[offset..]);
                return;
            }

            let state =
                std::mem::replace(&mut self.state, RuntimeNoiseLineState::Probe(Vec::new()));
            match state {
                RuntimeNoiseLineState::Probe(mut pending) => {
                    pending.push(bytes[offset]);
                    offset += 1;

                    if !Self::RUNTIME_NOISE_PREFIX.starts_with(&pending) {
                        emit(&pending);
                        let ended_line = pending.last() == Some(&b'\n');
                        self.state = if ended_line {
                            RuntimeNoiseLineState::Probe(Vec::new())
                        } else {
                            RuntimeNoiseLineState::Passthrough
                        };
                    } else if pending.len() == Self::RUNTIME_NOISE_PREFIX.len() {
                        self.state = RuntimeNoiseLineState::Candidate(pending);
                    } else {
                        self.state = RuntimeNoiseLineState::Probe(pending);
                    }
                }
                RuntimeNoiseLineState::Passthrough => {
                    if let Some(relative_newline) =
                        bytes[offset..].iter().position(|byte| *byte == b'\n')
                    {
                        let end = offset + relative_newline + 1;
                        emit(&bytes[offset..end]);
                        offset = end;
                        self.state = RuntimeNoiseLineState::Probe(Vec::new());
                    } else {
                        emit(&bytes[offset..]);
                        self.state = RuntimeNoiseLineState::Passthrough;
                        return;
                    }
                }
                RuntimeNoiseLineState::Candidate(mut candidate) => {
                    if let Some(relative_newline) =
                        bytes[offset..].iter().position(|byte| *byte == b'\n')
                    {
                        let end = offset + relative_newline + 1;
                        candidate.extend_from_slice(&bytes[offset..end]);
                        offset = end;

                        if candidate.len() > Self::MAX_CANDIDATE_LEN {
                            emit(&candidate);
                        } else {
                            let keep = std::str::from_utf8(&candidate).map_or(true, |line| {
                                self.runtime_filter
                                    .as_ref()
                                    .is_none_or(|filter| filter.keep_line(line))
                            });
                            if keep {
                                emit(&candidate);
                            }
                        }
                        self.state = RuntimeNoiseLineState::Probe(Vec::new());
                    } else {
                        candidate.extend_from_slice(&bytes[offset..]);
                        if candidate.len() > Self::MAX_CANDIDATE_LEN {
                            emit(&candidate);
                            self.state = RuntimeNoiseLineState::Passthrough;
                        } else {
                            self.state = RuntimeNoiseLineState::Candidate(candidate);
                        }
                        return;
                    }
                }
            }
        }
    }

    fn finish(&mut self, mut emit: impl FnMut(&[u8])) {
        let state = std::mem::replace(&mut self.state, RuntimeNoiseLineState::Passthrough);
        if let RuntimeNoiseLineState::Probe(pending) | RuntimeNoiseLineState::Candidate(pending) =
            state
        {
            // An unterminated fragment is not a complete C-init record.
            emit(&pending);
        }
    }

    fn disable_and_flush(&mut self, mut emit: impl FnMut(&[u8])) {
        self.runtime_filter = None;
        let state = std::mem::replace(&mut self.state, RuntimeNoiseLineState::Passthrough);
        if let RuntimeNoiseLineState::Probe(pending) | RuntimeNoiseLineState::Candidate(pending) =
            state
        {
            emit(&pending);
        }
    }
}

#[cfg(test)]
mod console_tail_tests {
    use super::RuntimeNoiseLineFilter;
    use a3s_box_core::log::RuntimeConsoleFilter;
    use std::sync::Arc;

    fn runtime_filter() -> Arc<RuntimeConsoleFilter> {
        Arc::new(RuntimeConsoleFilter::new())
    }

    #[test]
    fn runtime_noise_filter_handles_cross_chunk_sentinel_and_final_partial() {
        let mut filter = RuntimeNoiseLineFilter::new(Some(runtime_filter()));
        let mut visible = Vec::new();

        filter.push(b"init.kr", |bytes| visible.extend_from_slice(bytes));
        filter.push(
            b"un: execvp(/bin/app) starting\ninit.krun: business\npart",
            |bytes| visible.extend_from_slice(bytes),
        );
        filter.push(b"ial", |bytes| visible.extend_from_slice(bytes));
        filter.finish(|bytes| visible.extend_from_slice(bytes));

        assert_eq!(visible, b"init.krun: business\npartial");
    }

    #[test]
    fn runtime_noise_filter_preserves_generic_prefix_and_unterminated_candidate() {
        let mut filtered = RuntimeNoiseLineFilter::new(Some(runtime_filter()));
        let mut visible = Vec::new();
        filtered.push(b"ok\r\ninit.krun: business\n", |bytes| {
            visible.extend_from_slice(bytes)
        });
        filtered.push(b"init.krun: mount_filesystems ok", |bytes| {
            visible.extend_from_slice(bytes)
        });
        filtered.finish(|bytes| visible.extend_from_slice(bytes));
        assert_eq!(
            visible,
            b"ok\r\ninit.krun: business\ninit.krun: mount_filesystems ok"
        );

        let mut raw = RuntimeNoiseLineFilter::new(None);
        raw.push(b"init.kr", |bytes| visible.extend_from_slice(bytes));
        raw.push(b"un: retained", |bytes| visible.extend_from_slice(bytes));
        raw.finish(|bytes| visible.extend_from_slice(bytes));
        assert!(visible.ends_with(b"init.krun: retained"));
    }

    #[test]
    fn runtime_noise_filter_shares_sentinel_across_streams() {
        let shared = runtime_filter();
        let mut stdout = RuntimeNoiseLineFilter::new(Some(Arc::clone(&shared)));
        let mut stderr = RuntimeNoiseLineFilter::new(Some(shared));
        let mut visible_stdout = Vec::new();
        let mut visible_stderr = Vec::new();

        stdout.push(b"init.krun: mount_filesystems ok\n", |bytes| {
            visible_stdout.extend_from_slice(bytes)
        });
        stderr.push(b"init.krun: execvp(/bin/app) starting\n", |bytes| {
            visible_stderr.extend_from_slice(bytes)
        });
        stdout.push(b"init.krun: mount_filesystems ok\n", |bytes| {
            visible_stdout.extend_from_slice(bytes)
        });

        assert_eq!(visible_stdout, b"init.krun: mount_filesystems ok\n");
        assert!(visible_stderr.is_empty());
    }

    #[test]
    fn runtime_noise_filter_streams_visible_partial_line_during_push() {
        let mut filter = RuntimeNoiseLineFilter::new(Some(runtime_filter()));
        let mut visible = Vec::new();

        filter.push(b"progress without newline", |bytes| {
            visible.extend_from_slice(bytes)
        });

        assert_eq!(visible, b"progress without newline");
        filter.finish(|bytes| visible.extend_from_slice(bytes));
        assert_eq!(visible, b"progress without newline");
    }
}

/// Dispatch a parsed CLI to the appropriate command handler.
pub async fn dispatch(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Command::Run(args) => run::execute(args).await,
        Command::Create(args) => create::execute(args).await,
        Command::Start(args) => start::execute(args).await,
        Command::Stop(args) => stop::execute(args).await,
        Command::Restart(args) => restart::execute(args).await,
        Command::Rm(args) => rm::execute(args).await,
        Command::Kill(args) => kill::execute(args).await,
        Command::Pause(args) => pause::execute(args).await,
        Command::Unpause(args) => unpause::execute(args).await,
        Command::Ps(args) => ps::execute(args).await,
        Command::Stats(args) => stats::execute(args).await,
        Command::Logs(args) => logs::execute(args).await,
        Command::Exec(args) => exec::execute(args).await,
        Command::Top(args) => top::execute(args).await,
        Command::Inspect(args) => inspect::execute(args).await,
        Command::Attach(args) => attach::execute(args).await,
        Command::Attest(args) => attest::execute(args).await,
        Command::Audit(args) => audit::execute(args).await,
        Command::Seal(args) => seal::execute(args).await,
        Command::Unseal(args) => unseal::execute(args).await,
        Command::InjectSecret(args) => inject_secret::execute(args).await,
        Command::Wait(args) => wait::execute(args).await,
        Command::Rename(args) => rename::execute(args).await,
        Command::Port(args) => port::execute(args).await,
        Command::Export(args) => export::execute(args).await,
        Command::Commit(args) => commit::execute(args).await,
        Command::Diff(args) => diff::execute(args).await,
        Command::Events(args) => events::execute(args).await,
        Command::ContainerUpdate(args) => container_update::execute(args).await,
        Command::Compose(args) => compose::execute(args).await,
        Command::Snapshot(args) => snapshot::execute(args).await,
        Command::Build(args) => build::execute(args).await,
        Command::Images(args) => images::execute(args).await,
        Command::Pull(args) => pull::execute(args).await,
        Command::Push(args) => push::execute(args).await,
        Command::Login(args) => login::execute(args).await,
        Command::Logout(args) => logout::execute(args).await,
        Command::Rmi(args) => rmi::execute(args).await,
        Command::ImageInspect(args) => image_inspect::execute(args).await,
        Command::History(args) => history::execute(args).await,
        Command::ImagePrune(args) => image_prune::execute(args).await,
        Command::Tag(args) => image_tag::execute(args).await,
        Command::Save(args) => save::execute(args).await,
        Command::Load(args) => load::execute(args).await,
        Command::Import(args) => import::execute(args).await,
        Command::Cp(args) => cp::execute(args).await,
        Command::Network(args) => network::execute(args).await,
        Command::Volume(args) => volume::execute(args).await,
        Command::Df(args) => df::execute(args).await,
        Command::Prune(args) => prune::execute(args).await,
        Command::SystemPrune(args) => system_prune::execute(args).await,
        Command::Version(args) => version::execute(args).await,
        Command::Info(args) => info::execute(args).await,
        Command::Monitor(args) => monitor::execute(args).await,
        Command::Pool(args) => pool::execute(args).await,
        Command::Shell(args) => shell::execute(args).await,
    }
}

#[cfg(test)]
mod isolation_cli_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn run_accepts_explicit_sandbox_isolation() {
        let cli =
            Cli::try_parse_from(["a3s-box", "run", "--isolation", "sandbox", "alpine:latest"])
                .unwrap();

        let Command::Run(args) = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(args.common.isolation, Some(common::IsolationArg::Sandbox));
    }

    #[test]
    fn run_omission_preserves_microvm_default() {
        let cli = Cli::try_parse_from(["a3s-box", "run", "alpine:latest"]).unwrap();

        let Command::Run(args) = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(
            common::execution_isolation(&args.common),
            a3s_box_core::ExecutionIsolation::Microvm
        );
    }

    #[test]
    fn cli_rejects_explicit_microvm_spelling() {
        let error =
            Cli::try_parse_from(["a3s-box", "run", "--isolation", "microvm", "alpine:latest"])
                .err()
                .expect("explicit microvm spelling must be rejected");

        assert!(error.to_string().contains("invalid value 'microvm'"));
    }

    #[test]
    fn compose_up_accepts_sandbox_isolation() {
        let cli =
            Cli::try_parse_from(["a3s-box", "compose", "up", "--isolation", "sandbox"]).unwrap();

        let Command::Compose(args) = cli.command else {
            panic!("expected compose command");
        };
        let compose::ComposeCommand::Up(args) = args.command else {
            panic!("expected compose up command");
        };
        assert_eq!(args.isolation, Some(common::IsolationArg::Sandbox));
        assert!(args.services.is_empty());
    }
}
