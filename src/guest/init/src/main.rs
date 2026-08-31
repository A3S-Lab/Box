//! Guest init process for a3s-box VM.
//!
//! This process runs as PID 1 inside the MicroVM and is responsible for:
//! - Mounting essential filesystems (/proc, /sys, /dev)
//! - Mounting virtio-fs shares (workspace, user volumes)
//! - Mounting tmpfs volumes
//! - Configuring the guest network
//! - Launching the container entrypoint process
//! - Reaping zombie processes and handling SIGTERM for graceful shutdown

#[cfg(target_os = "linux")]
mod linux {

    use a3s_box_core::guest_exec::{
        GuestBootConfig, GuestBootMode, GuestExecConfig, GuestHostConfig, GUEST_BOOT_CONFIG_ENV,
        GUEST_BOOT_CONFIG_PATH, GUEST_BOOT_CONTROL_MOUNT_PATH, GUEST_BOOT_CONTROL_TAG,
        GUEST_ROOTFS_MAINTENANCE_ENV, GUEST_TERMINAL_CONTROL_MOUNT_PATH,
        GUEST_TERMINAL_CONTROL_TAG, GUEST_TERMINAL_STATUS_PATH, MAX_GUEST_BOOT_CONFIG_BYTES,
        MAX_RUNTIME_EXEC_CONFIG_BYTES, RUNTIME_EXEC_CONFIG_PATH,
    };
    use a3s_box_core::rootfs_baseline::GUEST_DIFF_BASELINE_PATH;
    use a3s_box_core::secret::{SecretEnvironmentBinding, SECRET_ENVIRONMENT_MANIFEST};
    use a3s_box_guest_init::{
        attest_server, diff_baseline, exec_server, host_config, namespace, network, port_forward,
        pty_server, root_transport, terminal_status, volume,
    };
    use std::process;
    use tracing::{error, info, warn};
    use zeroize::{Zeroize, Zeroizing};

    /// Bootstrap environment selected by the host execution backend.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum BootstrapMode {
        Microvm,
        HostSandbox,
    }

    impl BootstrapMode {
        fn from_value(value: Option<&str>) -> Result<Self, Box<dyn std::error::Error>> {
            match value.map(str::trim).filter(|value| !value.is_empty()) {
                None | Some("microvm") => Ok(Self::Microvm),
                Some("host-sandbox") => Ok(Self::HostSandbox),
                Some(value) => Err(format!("unsupported A3S_BOOTSTRAP_MODE {value:?}").into()),
            }
        }

        fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
            Self::from_value(std::env::var("A3S_BOOTSTRAP_MODE").ok().as_deref())
        }

        fn is_host_sandbox(self) -> bool {
            matches!(self, Self::HostSandbox)
        }
    }

    mod stdio;
    use stdio::*;

    mod exec_config;
    use exec_config::*;

    mod shutdown;
    use shutdown::*;

    /// Check if this VM is running in a TEE environment.
    ///
    /// Delegates to `a3s_box_core::tee::is_tee_available()` which checks
    /// `A3S_TEE_SIMULATE` env var and `/dev/sev-guest` or `/dev/sev` devices.
    fn is_tee_environment() -> bool {
        a3s_box_core::tee::is_tee_available()
    }

    /// Raw fd of `/dev/kmsg`, opened ONCE before any chroot/pivot and kept open for
    /// the process lifetime. An open file description survives `pivot_root`/`chroot`
    /// (it is independent of the path), so reusing this fd avoids the gap where the
    /// new root has no `/dev/kmsg` yet — which would otherwise leak a few lines back
    /// to the console mid-boot.
    static KMSG_FD: std::sync::OnceLock<Option<std::os::unix::io::RawFd>> =
        std::sync::OnceLock::new();

    /// Writer for guest-init's OWN tracing. Routes it to the kernel log
    /// (`/dev/kmsg`) instead of the VM console so it never pollutes container logs:
    /// the container inherits the console for its stdout/stderr, and Docker-style
    /// `logs` must show only that, not runtime internals (init/exec/pty chatter).
    /// A `<7>` (debug) priority prefix keeps these lines below the guest kernel's
    /// console loglevel (4), so they never echo back to the console. Falls back to
    /// stderr when `/dev/kmsg` is unavailable. The OCI Sandbox controller keeps
    /// runtime stderr separate from container stdout, so bootstrap diagnostics can
    /// never contaminate command output returned to SDK clients.
    enum InitLogWriter {
        Kmsg(std::os::unix::io::RawFd),
        Inherited(std::os::unix::io::RawFd),
        Stderr(std::io::Stderr),
    }

    impl std::io::Write for InitLogWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            match self {
                InitLogWriter::Kmsg(fd) => {
                    // /dev/kmsg treats each write() as one record: prefix the
                    // priority and flatten embedded newlines so a formatted event
                    // stays a single kernel-log record.
                    let mut record = Vec::with_capacity(buf.len() + 13);
                    record.extend_from_slice(b"<7>a3s-init: ");
                    record.extend(buf.iter().map(|&b| if b == b'\n' { b' ' } else { b }));
                    // SAFETY: *fd is a valid, process-lifetime fd to /dev/kmsg; a
                    // failed write is intentionally ignored (logging must never panic).
                    unsafe {
                        libc::write(*fd, record.as_ptr() as *const libc::c_void, record.len());
                    }
                    Ok(buf.len())
                }
                InitLogWriter::Inherited(fd) => write_inherited_log(*fd, buf),
                InitLogWriter::Stderr(out) => out.write(buf),
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            match self {
                InitLogWriter::Kmsg(_) | InitLogWriter::Inherited(_) => Ok(()),
                InitLogWriter::Stderr(out) => out.flush(),
            }
        }
    }

    fn make_init_log_writer() -> InitLogWriter {
        if let Some(fd) = inherited_init_log_fd() {
            return InitLogWriter::Inherited(fd);
        }
        match KMSG_FD.get().copied().flatten() {
            Some(fd) => InitLogWriter::Kmsg(fd),
            None => InitLogWriter::Stderr(std::io::stderr()),
        }
    }

    fn inherited_init_log_fd() -> Option<std::os::unix::io::RawFd> {
        let value = std::env::var("A3S_INIT_LOG_FD").ok()?;
        let fd = value.parse::<std::os::unix::io::RawFd>().ok()?;
        if fd < 3 || unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            return None;
        }
        // The descriptor belongs to guest-init but must not leak into main, exec,
        // or PTY workloads.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } != 0 {
            return None;
        }
        Some(fd)
    }

    fn write_inherited_log(
        fd: std::os::unix::io::RawFd,
        mut bytes: &[u8],
    ) -> std::io::Result<usize> {
        let original_len = bytes.len();
        while !bytes.is_empty() {
            let written =
                unsafe { libc::write(fd, bytes.as_ptr() as *const libc::c_void, bytes.len()) };
            if written < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if written == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "inherited init log descriptor returned a zero-length write",
                ));
            }
            bytes = &bytes[written as usize..];
        }
        Ok(original_len)
    }

    pub(super) fn run() {
        // Open /dev/kmsg once (before any chroot) and keep it open for the whole
        // process via into_raw_fd, so guest-init's logs reach the kernel log
        // reliably across the pivot. Container logs stay clean (see InitLogWriter).
        use std::os::unix::io::IntoRawFd;
        let kmsg_fd = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/kmsg")
            .ok()
            .map(|file| file.into_raw_fd());
        let _ = KMSG_FD.set(kmsg_fd);

        // Initialize logging. guest-init's own logs go to the kernel log, NOT the
        // console, to keep container logs clean.
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .with_ansi(false)
            .with_writer(make_init_log_writer)
            .init();

        info!("a3s-box guest init starting (PID {})", process::id());

        // Run init process
        if let Err(e) = run_init() {
            error!("Init process failed: {}", e);
            persist_exit_code(1);
            quiesce_rootfs_for_handoff();
            eprintln!("a3s-box guest init failed: {e}");
            process::exit(1);
        }

        info!("Init process completed successfully");
    }

    fn run_init() -> Result<(), Box<dyn std::error::Error>> {
        let bootstrap_mode = BootstrapMode::from_env()?;
        let rootfs_maintenance = rootfs_maintenance_requested()?;
        if bootstrap_mode.is_host_sandbox() && rootfs_maintenance {
            return Err("rootfs maintenance is supported only by the MicroVM bootstrap".into());
        }
        info!(?bootstrap_mode, "Selected guest-init bootstrap mode");

        // Restore Linux uid/gid/mode before mounting procfs, workspace, or user
        // volumes so metadata replay can never mutate an attached host path.
        #[cfg(target_os = "linux")]
        if rootfs_maintenance {
            info!("Skipping workload metadata replay for the trusted maintenance root");
        } else if bootstrap_mode.is_host_sandbox() {
            a3s_box_guest_init::rootfs_archive::restore_rootfs_metadata_around_mounts(
                std::path::Path::new("/"),
            )?;
        } else {
            a3s_box_guest_init::rootfs_archive::restore_rootfs_metadata(std::path::Path::new("/"))?;
        }

        let (mut exec_config, boot_host_config, boot_mode): (
            ExecConfig,
            Option<GuestHostConfig>,
            GuestBootMode,
        ) = if bootstrap_mode.is_host_sandbox() {
            // An OCI runtime may mount a Sandbox root read-only before PID 1
            // starts, so retain the staged process file there.
            (
                ExecConfig::from_env_without_consuming_staged_file()?,
                None,
                GuestBootMode::Workload,
            )
        } else if std::env::var_os(GUEST_BOOT_CONFIG_ENV).is_some() {
            // The MicroVM boot bundle lives on a private virtio-fs share, so
            // mount backend-owned shares before reading it. User mount targets
            // cannot overlap /run/a3s-box.
            mount_essential_filesystems()
                .map_err(|error| format!("failed to mount essential filesystems: {error}"))?;
            mount_default_shm()
                .map_err(|error| format!("failed to mount default shared memory: {error}"))?;
            mount_virtio_fs_shares()
                .map_err(|error| format!("failed to mount private boot transport: {error}"))?;
            let boot = read_guest_boot_config_from_env()?
                .ok_or_else(|| format!("{GUEST_BOOT_CONFIG_ENV} disappeared during boot"))?;
            unmount_guest_boot_control()
                .map_err(|error| format!("failed to unmount private boot transport: {error}"))?;
            let GuestBootConfig {
                mode,
                exec,
                environment,
                host,
                ..
            } = boot;
            (
                ExecConfig::from_guest_boot_config(exec, environment),
                Some(host),
                mode,
            )
        } else {
            // Legacy MicroVM runtimes stage fixed files in the rootfs. Read
            // them before user mounts can cover their paths.
            let exec = ExecConfig::from_env()?;
            mount_essential_filesystems()?;
            mount_default_shm()?;
            mount_virtio_fs_shares()?;
            (exec, None, GuestBootMode::Workload)
        };

        if rootfs_maintenance != boot_mode.is_rootfs_maintenance() {
            return Err(format!(
                "rootfs maintenance selector disagrees with typed boot mode {boot_mode:?}"
            )
            .into());
        }
        if rootfs_maintenance {
            return run_rootfs_maintenance();
        }

        info!(
            executable = %exec_config.executable,
            args = ?exec_config.args,
            workdir = %exec_config.workdir,
            env_count = exec_config.env.len(),
            "Container entrypoint configuration loaded"
        );

        // Host Sandbox mounts are already installed by the OCI runtime before
        // PID 1 starts, so its Secret bindings can be resolved immediately.
        if bootstrap_mode.is_host_sandbox() {
            exec_config.materialize_secret_environment()?;
        }

        // Step 2.6: Bind the exec (vsock 4089) and PTY (vsock 4090) listening sockets
        // NOW, before the slower network bring-up and container spawn below. These are
        // pure socket/bind/listen syscalls on this (still single-threaded) main thread,
        // so the later container fork stays fork-safe; the accept loops are spawned as
        // threads only after the fork (Step 8). Binding this early fills the listen
        // backlog from the start of boot, so a host connect QUEUES instead of being
        // refused while network setup and the container spawn finish — closing the
        // exec/PTY startup race of issue #3. CLOEXEC on the fds keeps the forked
        // container from inheriting the listeners.
        let (exec_listener, pty_listener) = if bootstrap_mode.is_host_sandbox() {
            let exec_fd = inherited_listener_fd("A3S_EXEC_LISTENER_FD")?;
            let pty_fd = inherited_listener_fd("A3S_PTY_LISTENER_FD")?;
            if exec_fd == pty_fd {
                return Err("Sandbox exec and PTY listeners must use distinct descriptors".into());
            }
            (
                exec_server::adopt_inherited_exec_listener(exec_fd)?,
                pty_server::adopt_inherited_pty_listener(pty_fd)?,
            )
        } else {
            (
                exec_server::bind_exec_server()?,
                pty_server::bind_pty_server()?,
            )
        };

        // Step 3: Configure guest network (if passt mode is active).
        if bootstrap_mode.is_host_sandbox() {
            network::configure_sandbox_loopback()?;
        } else {
            network::configure_guest_network()?;
        }

        // Step 3.25: Materialize host-owned files while the rootfs is writable.
        if !bootstrap_mode.is_host_sandbox() {
            match boot_host_config.as_ref() {
                Some(config) => host_config::apply_from_boot_config(config)?,
                None => host_config::apply_from_env()?,
            }

            // Only now expose host workspace and user volumes. This prevents an
            // image symlink such as `/etc -> /workspace` from redirecting the
            // runtime-owned hostname/DNS writes into a host share.
            mount_workload_virtio_fs_shares()?;
            mount_devpts()?;
            mount_tmpfs_volumes()?;

            // Make the unified hierarchy visible for nested runtimes in a VM.
            #[cfg(target_os = "linux")]
            let _ = a3s_box_guest_init::cgroup::ensure_cgroup2_ready();

            // Secret files are user-volume mounts and therefore resolve only
            // after the workload shares are present.
            exec_config.materialize_secret_environment()?;

            // A guest-native block provider has no host-visible tree after
            // ownership handoff. Capture its pristine diff baseline now, after
            // host files and mounts are finalized but before any workload or
            // sidecar process can mutate the root disk.
            if diff_baseline::persist(std::path::Path::new("/"))? {
                info!("Published guest-owned pristine rootfs diff baseline");
            }
        }

        // Step 3.5: Remount rootfs read-only if BOX_READONLY=1.
        // All writes to / must complete first.
        if !bootstrap_mode.is_host_sandbox() {
            remount_rootfs_readonly()?;
        }

        // Step 4: Register SIGTERM handler before spawning any children
        register_sigterm_handler()?;

        // Step 6: Create namespace config (isolation disabled inside the MicroVM —
        // the VM boundary itself provides isolation, and unshare can interfere with
        // the lightweight kernel's limited namespace support)
        let namespace_config = namespace::NamespaceConfig {
            mount: false,
            pid: false,
            ipc: false,
            uts: false,
            net: false,
            user: false,
            cgroup: false,
        };

        // Step 6.5: Launch sidecar process (if configured)
        // The sidecar runs before the main container so it is ready to intercept
        // traffic when the agent starts. It is not waited on — it runs for the
        // lifetime of the VM and is reaped by the zombie-reaper loop.
        if !bootstrap_mode.is_host_sandbox() {
            if let Some(sidecar) = SidecarConfig::from_env() {
                info!(
                    image = %sidecar.image,
                    vsock_port = sidecar.vsock_port,
                    "Launching sidecar process"
                );
                launch_sidecar(&sidecar)?;
            }
        }

        // Step 7: Launch container entrypoint
        info!("Launching container entrypoint");

        // Ensure the working directory exists — Docker creates a missing WORKDIR /
        // `-w` path before chdir. Best-effort: a pre-existing dir is fine, and a
        // read-only rootfs (where creation fails) matches Docker's inability to
        // create it there.
        if !exec_config.workdir.is_empty() && exec_config.workdir != "/" {
            if let Err(e) = std::fs::create_dir_all(&exec_config.workdir) {
                warn!(
                    workdir = %exec_config.workdir,
                    error = %e,
                    "Could not pre-create working directory (continuing)"
                );
            }
        }

        // Convert args to &str for spawn_isolated
        let args_refs: Vec<&str> = exec_config.args.iter().map(|s| s.as_str()).collect();
        let env_refs: Vec<(&str, &str)> = exec_config
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        // Deferred-main (BOX_DEFERRED_MAIN=1): boot IDLE — skip the boot spawn and let
        // the container main be spawned later by a `spawn-main` control frame (for a
        // pre-warmed/pooled sandbox). CONTAINER_PID stays the -1 sentinel; the exec
        // server + supervision loop start as usual, so host readiness still passes
        // (the heartbeat handshake has no container-pid dependency).
        // Standard /dev/std{in,out,err} symlinks (-> the container's own fds), created
        // before the container fork so it inherits them. Pairs with setup_main_stdio_pipes.
        #[cfg(target_os = "linux")]
        ensure_dev_std_symlinks();

        // MicroVMs create their workload cgroup here. A host Sandbox instead
        // adopts the runtime-prepared control/workload layout through the fixed
        // SDK descriptors.
        // Those files are owned by host root outside the Sandbox user namespace,
        // and cgroupfs is deliberately mounted read-only. Retaining the already-
        // open workload descriptor prevents a path substitution race and lets
        // every main/exec/PTY process join the exact runtime-owned leaf before
        // exec without introducing a second cgroup management mechanism.
        #[cfg(target_os = "linux")]
        let container_cgroup = if bootstrap_mode.is_host_sandbox() {
            let control_descriptor =
                inherited_listener_fd(a3s_box_guest_init::cgroup::CONTROL_CGROUP_PROCS_FD_ENV)?;
            let workload_descriptor =
                inherited_listener_fd(a3s_box_guest_init::cgroup::WORKLOAD_CGROUP_PROCS_FD_ENV)?;
            let cgroup = a3s_box_guest_init::cgroup::ContainerCgroup::adopt_runtime_delegation(
                control_descriptor,
                workload_descriptor,
            )?;
            std::env::remove_var(a3s_box_guest_init::cgroup::CONTROL_CGROUP_PROCS_FD_ENV);
            std::env::remove_var(a3s_box_guest_init::cgroup::WORKLOAD_CGROUP_PROCS_FD_ENV);
            Some(cgroup)
        } else {
            a3s_box_guest_init::cgroup::ContainerCgroup::create_for_main(
                std::env::var("A3S_SEC_MEM_LIMIT")
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok()),
                std::env::var("A3S_SEC_MEM_LOW")
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok()),
                std::env::var("A3S_SEC_MEM_SWAP")
                    .ok()
                    .and_then(|value| value.parse::<i64>().ok()),
                std::env::var("A3S_SEC_CPU_QUOTA")
                    .ok()
                    .and_then(|value| value.parse::<i64>().ok()),
                std::env::var("A3S_SEC_CPU_PERIOD")
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok()),
                std::env::var("A3S_SEC_CPU_SHARES")
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok()),
                std::env::var("A3S_SEC_PIDS_LIMIT")
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok()),
            )
        };
        #[cfg(target_os = "linux")]
        let cgroup_procs_path = container_cgroup.as_ref().map(|cgroup| cgroup.procs_path());
        #[cfg(target_os = "linux")]
        let cgroup_procs = container_cgroup
            .as_ref()
            .and_then(|cgroup| cgroup.procs_descriptor());
        #[cfg(not(target_os = "linux"))]
        let cgroup_procs: Option<std::os::fd::RawFd> = None;
        #[cfg(target_os = "linux")]
        exec_server::set_container_cgroup(cgroup_procs_path, cgroup_procs)?;

        let deferred_main = std::env::var("BOX_DEFERRED_MAIN")
            .map(|v| v == "1")
            .unwrap_or(false);

        let container_pid = if deferred_main {
            info!("BOX_DEFERRED_MAIN=1 — booting IDLE; container main deferred to a spawn-main control frame");
            // Stash the parsed command so a later spawn-main trigger runs it as main.
            #[cfg(target_os = "linux")]
            exec_server::set_deferred_main_spec(
                exec_config.executable.clone(),
                exec_config.args.clone(),
                exec_config
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                if exec_config.workdir.is_empty() {
                    None
                } else {
                    Some(exec_config.workdir.clone())
                },
                exec_config.user.clone(),
                exec_config.stdin_null,
            );
            nix::unistd::Pid::from_raw(-1)
        } else {
            // Hand the main process re-openable pipe write-ends as fd 1/2 (see
            // setup_main_stdio_pipes) so it can re-open its own stdout/stderr by path.
            #[cfg(target_os = "linux")]
            let relay = setup_main_stdio_pipes();
            #[cfg(target_os = "linux")]
            let main_stdio = relay.as_ref().map(|r| (r.out_w, r.err_w));
            #[cfg(not(target_os = "linux"))]
            let main_stdio = None;

            let container_pid_raw = namespace::spawn_isolated(
                &namespace_config,
                &exec_config.executable,
                &args_refs,
                &env_refs,
                &exec_config.workdir,
                exec_config.user.as_deref(),
                exec_config.stdin_null,
                main_stdio,
                cgroup_procs,
            )?;
            info!("Container process started with PID {}", container_pid_raw);

            // Close our copies of the write-ends (the container is now the sole writer),
            // then start the relay threads. Starting them AFTER the fork keeps guest-init
            // single-threaded across the container `fork()` (fork-safety).
            #[cfg(target_os = "linux")]
            if let Some(r) = relay {
                unsafe {
                    libc::close(r.out_w);
                    libc::close(r.err_w);
                }
                start_stdio_relays(r.out_r, r.console_out, r.err_r, r.console_err);
            }

            // Make the main container PID available to the exec server so a host
            // graceful-stop request (signal-main control frame) can deliver the
            // STOPSIGNAL to it. Must be set before the exec server thread starts.
            exec_server::set_container_pid(container_pid_raw as i32);
            nix::unistd::Pid::from_raw(container_pid_raw as i32)
        };

        expose_container_env_to_exec(&exec_config);

        // Step 8: Start the exec server accept loop on the socket bound in Step 2.6.
        // (set_container_pid above ran first, so a host signal-main frame still finds
        // the PID once the loop is serving.)
        std::thread::spawn(move || {
            if let Err(e) = exec_server::serve_exec_server(exec_listener) {
                error!("Exec server failed: {}", e);
            }
        });

        // Step 8.25: Start Windows host-port forward control client when enabled.
        if !bootstrap_mode.is_host_sandbox() {
            std::thread::spawn(|| {
                if let Err(e) = port_forward::run_port_forward_client(request_shutdown) {
                    error!("Port-forward client failed: {}", e);
                }
            });
        }

        // Step 8.5: Start the PTY server accept loop on the socket bound in Step 2.6.
        std::thread::spawn(move || {
            if let Err(e) = pty_server::serve_pty_server(pty_listener) {
                error!("PTY server failed: {}", e);
            }
        });

        // Step 8.6: Start attestation server in background thread (TEE environments only)
        // Only start if TEE simulation is enabled or real SEV-SNP hardware is present.
        if !bootstrap_mode.is_host_sandbox() && is_tee_environment() {
            std::thread::spawn(|| {
                if let Err(e) = attest_server::run_attest_server() {
                    error!("Attestation server failed: {}", e);
                }
            });
        }

        // Step 9: Wait for agent process (reap zombies, handle SIGTERM)
        wait_for_children(container_pid, bootstrap_mode)?;

        persist_terminal_rootfs_metadata();

        // Drain the stdio relays on the graceful-shutdown / no-children return paths
        // (the container-exit path flushes before its own process::exit).
        flush_stdio_relays();
        quiesce_rootfs_for_handoff();

        Ok(())
    }

    fn rootfs_maintenance_requested() -> Result<bool, Box<dyn std::error::Error>> {
        match std::env::var(GUEST_ROOTFS_MAINTENANCE_ENV) {
            Err(std::env::VarError::NotPresent) => Ok(false),
            Ok(value) if value == "1" => Ok(true),
            Ok(value) => {
                Err(format!("unsupported {GUEST_ROOTFS_MAINTENANCE_ENV} value {value:?}").into())
            }
            Err(error) => Err(error.into()),
        }
    }

    fn inherited_listener_fd(name: &str) -> Result<std::os::fd::RawFd, Box<dyn std::error::Error>> {
        let raw = std::env::var(name).map_err(|_| format!("missing required {name}"))?;
        let fd = raw
            .parse::<std::os::fd::RawFd>()
            .map_err(|_| format!("invalid {name} value {raw:?}"))?;
        if !(3..=1024).contains(&fd) {
            return Err(format!("{name} must be a descriptor between 3 and 1024").into());
        }
        Ok(fd)
    }

    fn expose_container_env_to_exec(config: &ExecConfig) {
        for (key, value) in &config.env {
            if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') {
                warn!(key, "Skipping invalid container environment entry for exec");
                continue;
            }
            std::env::set_var(key, value);
        }
    }

    /// Launch the sidecar process as a background co-process.
    ///
    /// The sidecar binary is expected to be present in the rootfs at a well-known
    /// path. It is spawned with its configured environment variables and runs
    /// independently of the main container process.
    ///
    /// The sidecar is NOT waited on — it runs for the lifetime of the VM and is
    /// reaped by the zombie-reaper loop in `wait_for_children`.
    fn launch_sidecar(config: &SidecarConfig) -> Result<(), Box<dyn std::error::Error>> {
        // The sidecar binary path: conventionally /usr/bin/sidecar or derived from image name.
        // Inside the VM the sidecar image is already extracted into the rootfs by the runtime.
        // We look for the binary at /usr/bin/<basename> where basename is the last component
        // of the image reference (e.g., "safeclaw" from "ghcr.io/a3s-lab/safeclaw:latest").
        let binary_name = config
            .image
            .split('/')
            .next_back()
            .and_then(|s| s.split(':').next())
            .unwrap_or("sidecar");

        let binary_path = format!("/usr/bin/{}", binary_name);

        let mut cmd = std::process::Command::new(&binary_path);

        // Inject sidecar-specific env vars
        for (key, value) in &config.env {
            cmd.env(key, value);
        }

        // Pass vsock port so the sidecar knows where to listen
        cmd.env("SIDECAR_VSOCK_PORT", config.vsock_port.to_string());

        match cmd.spawn() {
            Ok(child) => {
                info!(
                    binary = %binary_path,
                    pid = child.id(),
                    vsock_port = config.vsock_port,
                    "Sidecar process launched"
                );
                // Intentionally leak the Child handle — the zombie-reaper loop
                // in wait_for_children will reap it when it exits.
                std::mem::forget(child);
                Ok(())
            }
            Err(e) => {
                // Non-fatal: log and continue. The main container should still start
                // even if the sidecar binary is missing (e.g., in development).
                warn!(
                    binary = %binary_path,
                    error = %e,
                    "Failed to launch sidecar — continuing without it"
                );
                Ok(())
            }
        }
    }

    mod mounts;
    use mounts::*;

    /// Supervise children as PID 1: propagate the container's exit, and reap orphans.
    ///
    /// Exec and PTY request handlers reap their OWN children (each `waitpid`s a
    /// specific pid) to read the real exit status, so this loop must not steal them
    /// with a blind `waitpid(-1)`. It peeks exited children non-destructively with
    /// `waitid(WNOWAIT)` and, via the [`reaper`](a3s_box_guest_init::reaper)
    /// registry, reaps only the container (→ VM lifecycle / exit code) and UNMANAGED
    /// children — reparented grandchildren and the sidecar — leaving handler-managed
    /// children for their handler. This propagates the container exit code AND fixes
    /// the zombie leak (orphans were previously never reaped until shutdown).
    #[cfg(target_os = "linux")]
    fn wait_for_children(
        container_pid: nix::unistd::Pid,
        bootstrap_mode: BootstrapMode,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use a3s_box_guest_init::reaper;
        use nix::sys::wait::{waitid, waitpid, Id, WaitPidFlag, WaitStatus};

        /// Maximum time to wait for children after forwarding SIGTERM (5 seconds).
        const CHILD_SHUTDOWN_TIMEOUT_MS: u64 = 5000;

        info!(
            "Supervising children as PID 1; container PID {}",
            container_pid
        );

        loop {
            let shutdown_signal = shutdown_signal();
            if shutdown_signal != 0 {
                info!(
                    shutdown_signal,
                    "Shutdown requested, initiating graceful shutdown"
                );
                graceful_shutdown(CHILD_SHUTDOWN_TIMEOUT_MS, shutdown_signal);
                return Ok(());
            }

            // Drain currently-exited children. `WNOWAIT` peeks without reaping, so a
            // handler-managed child stays reapable by its handler; we break on it and
            // revisit next tick (the handler clears it within its own poll interval).
            loop {
                let (pid, code, signaled) = match waitid(
                    Id::All,
                    WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT | WaitPidFlag::WNOHANG,
                ) {
                    Ok(WaitStatus::Exited(pid, status)) => (pid, status, false),
                    Ok(WaitStatus::Signaled(pid, signal, _)) => (pid, 128 + signal as i32, true),
                    // No exited child right now: stop draining and poll again later.
                    Ok(_) => break,
                    // No children right now. In deferred-main mode (IDLE boot) the
                    // container main has not been spawned yet — keep waiting for the
                    // spawn-main frame rather than exiting (which would halt the VM
                    // before the main ever runs). Otherwise the container is gone: done.
                    Err(nix::errno::Errno::ECHILD) => {
                        if exec_server::container_pid() < 0 {
                            break;
                        }
                        return Ok(());
                    }
                    // Transient error: retry on the next tick.
                    Err(_) => break,
                };

                // Read the container pid fresh each iteration: a deferred main (IDLE
                // boot) publishes it late via spawn-main; the eager path set it at boot.
                // The -1/-2 sentinels (unset/pending) never match a real pid.
                let cpid = exec_server::container_pid();
                if cpid >= 0 && pid.as_raw() == cpid {
                    // The container drives the VM lifecycle: reap it and exit with its
                    // status so the host (and detached `run -d wait`) sees the real code.
                    let _ = waitpid(pid, None);
                    if signaled {
                        error!("Container process {} terminated (exit code {})", pid, code);
                    } else {
                        info!("Container process {} exited with status {}", pid, code);
                    }
                    persist_terminal_rootfs_metadata();
                    persist_exit_code(code);
                    // Flush the stdout/stderr relays so the container's last output
                    // reaches the console before this process::exit halts the VM.
                    flush_stdio_relays();
                    quiesce_rootfs_for_handoff();
                    // MicroVM logging still needs a bounded handoff before PID 1
                    // halts the VMM. Host Sandbox logging has a generation-owned
                    // worker that waits for the runtime owner to close both writers and drains
                    // through EOF, so retaining this legacy delay there only adds
                    // fixed latency to every short command.
                    let handoff = console_handoff_delay(bootstrap_mode);
                    if !handoff.is_zero() {
                        std::thread::sleep(handoff);
                    }
                    process::exit(code);
                } else if reaper::is_managed(pid.as_raw()) {
                    // Owned by an exec/PTY handler, which reaps it for the real status.
                    // Stop draining; it clears shortly and we revisit on the next tick.
                    break;
                } else {
                    // Orphan (reparented grandchild) or the sidecar: reap it here so it
                    // does not linger as a zombie. Keep draining for more.
                    let _ = waitpid(pid, Some(WaitPidFlag::WNOHANG));
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    #[cfg(any(target_os = "linux", test))]
    fn console_handoff_delay(bootstrap_mode: BootstrapMode) -> std::time::Duration {
        if bootstrap_mode.is_host_sandbox() {
            std::time::Duration::ZERO
        } else {
            std::time::Duration::from_millis(250)
        }
    }

    #[cfg(target_os = "linux")]
    fn persist_terminal_rootfs_metadata() {
        if std::env::var("BOX_PERSIST_ROOTFS_METADATA").as_deref() != Ok("1") {
            return;
        }
        if let Err(error) =
            a3s_box_guest_init::rootfs_archive::persist_rootfs_metadata(std::path::Path::new("/"))
        {
            warn!(%error, "Failed to persist terminal rootfs metadata");
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn persist_terminal_rootfs_metadata() {}

    /// Non-Linux development stub: just wait for the container process to exit.
    #[cfg(not(target_os = "linux"))]
    fn wait_for_children(
        container_pid: nix::unistd::Pid,
        _bootstrap_mode: BootstrapMode,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};

        loop {
            if shutdown_signal() != 0 {
                return Ok(());
            }
            match waitpid(container_pid, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(_, status)) => {
                    persist_exit_code(status);
                    process::exit(status);
                }
                Ok(WaitStatus::Signaled(_, signal, _)) => {
                    persist_exit_code(128 + signal as i32);
                    process::exit(128 + signal as i32);
                }
                Ok(WaitStatus::StillAlive) => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        Ok(())
    }

    /// Publish the exact container exit code before PID 1 halts the VM.
    ///
    /// New MicroVMs use the private pre-opened terminal channel. The rootfs
    /// marker remains as a compatibility fallback for Sandbox and directory-root
    /// providers whose writable tree is safely host-visible.
    fn persist_exit_code(code: i32) {
        use std::io::Write;
        if let Err(error) = terminal_status::persist(code) {
            warn!(%error, "Failed to persist guest terminal status");
        }
        if let Ok(mut file) = std::fs::File::create(a3s_box_core::rootfs_metadata::EXIT_CODE_PATH) {
            let _ = write!(file, "{code}");
            let _ = file.sync_all();
        }
    }

    fn quiesce_rootfs_for_handoff() {
        match root_transport::quiesce_for_handoff() {
            Ok(true) => match terminal_status::mark_rootfs_quiesced() {
                Ok(true) => info!("Published guest-owned rootfs handoff acknowledgement"),
                Ok(false) => error!(
                    "Guest-owned block root has no terminal channel for its handoff acknowledgement"
                ),
                Err(error) => {
                    error!(%error, "Failed to publish guest-owned rootfs handoff acknowledgement")
                }
            },
            Ok(false) => {}
            Err(error) => {
                error!(%error, "Failed to quiesce guest-owned rootfs before VM exit")
            }
        }
    }

    /// Perform graceful shutdown: forward the requested signal to children,
    /// wait, then force-kill.
    /// Only the Linux supervision loop drives this (the non-Linux dev stub exits the
    /// process directly), so it is gated to avoid a dead-code warning on macOS.
    #[cfg(target_os = "linux")]
    fn graceful_shutdown(timeout_ms: u64, signal: i32) {
        // Step 1: Send the requested signal to all processes except PID 1.
        #[cfg(target_os = "linux")]
        {
            info!(signal, "Forwarding stop signal to all child processes");
            unsafe {
                libc::kill(-1, signal);
            }
        }

        // Step 2: Wait for children to exit with timeout
        let start = std::time::Instant::now();
        loop {
            use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
            use nix::unistd::Pid;

            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(pid, status)) => {
                    info!(
                        "Child {} exited with status {} during shutdown",
                        pid, status
                    );
                }
                Ok(WaitStatus::Signaled(pid, signal, _)) => {
                    info!("Child {} terminated by {:?} during shutdown", pid, signal);
                }
                Ok(WaitStatus::StillAlive) => {
                    if start.elapsed().as_millis() > timeout_ms as u128 {
                        warn!("Shutdown timeout reached, sending SIGKILL to remaining children");
                        #[cfg(target_os = "linux")]
                        unsafe {
                            libc::kill(-1, libc::SIGKILL);
                        }
                        // Reap any remaining
                        loop {
                            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                                Ok(WaitStatus::StillAlive) | Err(nix::errno::Errno::ECHILD) => {
                                    break
                                }
                                _ => continue,
                            }
                        }
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Ok(_) => {
                    // Other status, continue
                }
                Err(nix::errno::Errno::ECHILD) => {
                    info!("All children exited during shutdown");
                    break;
                }
                Err(e) => {
                    warn!("waitpid error during shutdown: {}", e);
                    break;
                }
            }
        }

        // Step 3: Sync filesystem buffers
        info!("Syncing filesystem buffers");
        #[cfg(target_os = "linux")]
        unsafe {
            libc::sync();
        }

        info!("Graceful shutdown complete");
    }

    #[cfg(test)]
    mod tests;
}

#[cfg(target_os = "linux")]
fn main() {
    linux::run();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("a3s-box-guest-init is a Linux guest binary");
    std::process::exit(1);
}
