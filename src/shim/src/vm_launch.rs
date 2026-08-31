//! Cross-platform libkrun launch with Windows WHPX guest preparation.

use super::*;

/// Apply the currently supported WHPX boot contract and clear result files from
/// an earlier boot before libkrun opens the rootfs.
#[cfg(target_os = "windows")]
pub(super) fn prepare_windows_guest(spec: &InstanceSpec) -> Result<()> {
    let rootfs = windows_rootfs_path(spec)?;
    validate_vcpu_count(u32::from(spec.vcpus)).map_err(|message| BoxError::BoxBootError {
        message,
        hint: Some("Use --cpus 1 on Windows".to_string()),
    })?;

    // The checked-in libkrunfw kernel boots reliably through WHPX's legacy PIC
    // path. An explicitly present value (including an empty one) remains an
    // expert override for kernel debugging.
    if std::env::var_os("LIBKRUN_WINDOWS_KERNEL_CMDLINE_APPEND").is_none() {
        std::env::set_var("LIBKRUN_WINDOWS_KERNEL_CMDLINE_APPEND", "noapic");
        tracing::debug!("Enabled the Windows WHPX noapic kernel boot path");
    }

    // A3S owns this dedicated shim process and must regain control after the
    // workload exits so its live log processor can drain the complete streams.
    // The bundled libkrun keeps its normal process-takeover contract unless this
    // internal opt-in is set.
    std::env::set_var(WINDOWS_RETURN_ON_EXIT_ENV, "1");

    for name in [
        WINDOWS_GUEST_EXIT_CODE,
        WINDOWS_GUEST_STDOUT,
        WINDOWS_GUEST_STDERR,
        WINDOWS_GUEST_RESULT_MARKER,
        WINDOWS_LIVE_LOGS_DRAINED_MARKER,
    ] {
        let path = rootfs.join(name);
        if let Err(error) = a3s_box_core::windows_file::remove_path_no_follow(&path) {
            return Err(BoxError::BoxBootError {
                message: format!(
                    "Failed to remove stale Windows guest result {}: {error}",
                    path.display()
                ),
                hint: Some(
                    "Ensure the extracted rootfs is writable by the current user".to_string(),
                ),
            });
        }
    }

    // The live log processor must open both streams before start_enter so a
    // short guest cannot exit before its tailers are ready. The guest wrapper
    // append-opens the same files after boot; create them empty on the host now
    // to make that readiness barrier possible and discard any stale output.
    for name in [WINDOWS_GUEST_STDOUT, WINDOWS_GUEST_STDERR] {
        let path = rootfs.join(name);
        a3s_box_core::windows_file::replace_regular_file(&path, b"").map_err(|error| {
            BoxError::BoxBootError {
                message: format!(
                    "Failed to create empty Windows guest stream {}: {error}",
                    path.display()
                ),
                hint: Some(
                    "Ensure the extracted rootfs is writable by the current user".to_string(),
                ),
            }
        })?;
    }

    Ok(())
}

/// Configure libkrun context and start the VM.
///
/// # Safety
/// This function calls unsafe libkrun FFI functions.
/// It performs process takeover on Unix. The bundled Windows backend returns
/// after guest shutdown so the shim can finish host-side log processing.
pub(super) unsafe fn configure_and_start_vm(spec: &InstanceSpec) -> Result<()> {
    // Initialize libkrun logging
    tracing::debug!("Initializing libkrun logging");
    if let Err(e) = KrunContext::init_logging() {
        tracing::warn!(error = %e, "Failed to initialize libkrun logging");
    }

    // Create libkrun context
    tracing::debug!("Creating libkrun context");
    let ctx = KrunContext::create()?;

    // Configure VM resources
    tracing::debug!(
        vcpus = spec.vcpus,
        memory_mib = spec.memory_mib,
        "Setting VM config"
    );
    ctx.set_vm_config(spec.vcpus, spec.memory_mib)?;

    #[cfg(target_os = "windows")]
    configure_windows_kernel(&ctx)?;

    // Raise RLIMIT_NOFILE to maximum - CRITICAL for virtio-fs
    #[cfg(unix)]
    {
        use libc::{getrlimit, rlimit, setrlimit, RLIMIT_NOFILE};
        let mut rlim = rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if getrlimit(RLIMIT_NOFILE, &mut rlim) == 0 {
            rlim.rlim_cur = rlim.rlim_max;
            if setrlimit(RLIMIT_NOFILE, &rlim) != 0 {
                tracing::warn!("Failed to raise RLIMIT_NOFILE");
            } else {
                tracing::debug!(limit = rlim.rlim_cur, "RLIMIT_NOFILE raised");
            }
        }
    }

    // Configure guest rlimits
    let mut rlimits = vec![
        "7=1048576:1048576".to_string(), // RLIMIT_NOFILE = 7
    ];

    // Apply pids_limit as RLIMIT_NPROC (resource 6)
    if let Some(pids_limit) = spec.resource_limits.pids_limit {
        rlimits.push(format!("6={}:{}", pids_limit, pids_limit));
        tracing::info!(pids_limit, "Applying PID limit via RLIMIT_NPROC");
    } else {
        rlimits.push("6=4096:8192".to_string()); // Default RLIMIT_NPROC
    }

    // Apply custom ulimits (--ulimit RESOURCE=SOFT:HARD)
    for ulimit in &spec.resource_limits.ulimits {
        if let Some(rlimit_str) = parse_ulimit(ulimit) {
            rlimits.push(rlimit_str);
            tracing::info!(ulimit, "Applying custom ulimit");
        } else {
            tracing::warn!(ulimit, "Ignoring unrecognized ulimit format");
        }
    }

    tracing::debug!(rlimits = ?rlimits, "Configuring guest rlimits");
    ctx.set_rlimits(&rlimits)?;

    // Add filesystem mounts via virtiofs
    // For file mounts, we need to create a temporary directory and copy the file into it
    // because virtio-fs only supports directory mounts.
    tracing::info!("Adding filesystem mounts via virtiofs:");
    for mount in &spec.fs_mounts {
        let host_path = &mount.host_path;

        let mount_path: std::path::PathBuf = if host_path.is_file() {
            // Create a temporary directory to hold the file
            // Per-mount temp dir (keyed by tag) so two file mounts sharing a
            // basename (e.g. two app.conf to different targets) don't collide.
            let temp_dir =
                std::env::temp_dir().join(format!("a3s-fs-mount-{}-{}", spec.box_id, mount.tag));
            let file_name = host_path.file_name().unwrap();
            let temp_file_path = temp_dir.join(file_name);

            std::fs::create_dir_all(&temp_dir).map_err(|e| BoxError::BoxBootError {
                message: format!("Failed to create temp directory for file mount: {}", e),
                hint: None,
            })?;
            std::fs::copy(host_path, &temp_file_path).map_err(|e| BoxError::BoxBootError {
                message: format!("Failed to copy file for mount: {}", e),
                hint: None,
            })?;

            tracing::debug!(
                tag = %mount.tag,
                original = %host_path.display(),
                temp = %temp_dir.display(),
                "File mount converted to directory mount"
            );
            temp_dir
        } else {
            host_path.clone()
        };

        let path_str = mount_path.to_str().ok_or_else(|| BoxError::BoxBootError {
            message: format!("Invalid path: {}", mount_path.display()),
            hint: None,
        })?;

        tracing::info!(
            "  {} → {} ({})",
            mount.tag,
            host_path.display(),
            if mount.read_only { "ro" } else { "rw" }
        );
        ctx.add_virtiofs(&mount.tag, path_str)?;
    }

    // Configure either the legacy virtio-fs directory root or the guest-native
    // raw ext4 root disk. The latter never enters macOS DiskImages.
    ctx.set_root(&spec.rootfs)?;

    // Root block disks must be registered first so they remain `/dev/vda`.
    // Auxiliary devices then receive stable names in declared order; the
    // maintenance guest relies on its sole read-only disk being `/dev/vda`.
    for disk in &spec.block_devices {
        ctx.add_raw_block_device(disk)?;
    }

    // Set working directory
    tracing::debug!(workdir = %spec.workdir, "Setting working directory");
    ctx.set_workdir(&spec.workdir)?;

    // Set entrypoint
    tracing::debug!(
        executable = %spec.entrypoint.executable,
        args = ?spec.entrypoint.args,
        "Setting entrypoint"
    );
    ctx.set_exec(
        &spec.entrypoint.executable,
        &spec.entrypoint.args,
        &spec.entrypoint.env,
    )?;

    // TSI port mapping for inbound connections (host -> guest)
    // This allows external connections to reach services inside the guest.
    // Must be called before add_vsock_port to avoid EINVAL from libkrun.
    // Skip entries handled by bridge-native forwarding or host_port=0
    // auto-assignment, which would fail with EINVAL in libkrun's TSI.
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        let valid_port_map = tsi_port_map_for_spec(spec);

        if !valid_port_map.is_empty() {
            tracing::info!(
                port_map = ?valid_port_map,
                "Configuring TSI port mapping for inbound connections"
            );
            ctx.set_port_map(&valid_port_map)?;
        } else if !spec.port_map.is_empty() {
            tracing::debug!(
                port_map = ?spec.port_map,
                "Skipping TSI port mapping; native bridge port forwarding or auto-assigned host ports handle these entries"
            );
        }
    }

    if spec.disable_tsi {
        tracing::info!("Disabling TSI socket interception for isolated networking");
        ctx.disable_tsi()?;
    }

    // Configure exec communication channel
    #[cfg(not(target_os = "windows"))]
    {
        let exec_socket_str =
            spec.exec_socket_path
                .to_str()
                .ok_or_else(|| BoxError::BoxBootError {
                    message: format!(
                        "Invalid exec socket path: {}",
                        spec.exec_socket_path.display()
                    ),
                    hint: None,
                })?;
        tracing::debug!(
            socket_path = exec_socket_str,
            guest_port = EXEC_VSOCK_PORT,
            "Configuring vsock bridge for exec (Unix socket)"
        );
        ctx.add_vsock_port(EXEC_VSOCK_PORT, exec_socket_str, true)?;

        // Configure PTY communication channel (Unix socket bridged to vsock port 4090)
        if !spec.pty_socket_path.as_os_str().is_empty() {
            let pty_socket_str =
                spec.pty_socket_path
                    .to_str()
                    .ok_or_else(|| BoxError::BoxBootError {
                        message: format!(
                            "Invalid PTY socket path: {}",
                            spec.pty_socket_path.display()
                        ),
                        hint: None,
                    })?;
            tracing::debug!(
                socket_path = pty_socket_str,
                guest_port = PTY_VSOCK_PORT,
                "Configuring vsock bridge for PTY"
            );
            ctx.add_vsock_port(PTY_VSOCK_PORT, pty_socket_str, true)?;
        }

        // Configure attestation communication channel (Unix socket bridged to vsock port 4091)
        if !spec.attest_socket_path.as_os_str().is_empty() {
            let attest_socket_str =
                spec.attest_socket_path
                    .to_str()
                    .ok_or_else(|| BoxError::BoxBootError {
                        message: format!(
                            "Invalid attestation socket path: {}",
                            spec.attest_socket_path.display()
                        ),
                        hint: None,
                    })?;
            tracing::debug!(
                socket_path = attest_socket_str,
                guest_port = ATTEST_VSOCK_PORT,
                "Configuring vsock bridge for attestation"
            );
            ctx.add_vsock_port(ATTEST_VSOCK_PORT, attest_socket_str, true)?;
        }

        if !spec.port_forward_socket_path.as_os_str().is_empty() {
            let port_forward_socket_str =
                spec.port_forward_socket_path
                    .to_str()
                    .ok_or_else(|| BoxError::BoxBootError {
                        message: format!(
                            "Invalid port-forward socket path: {}",
                            spec.port_forward_socket_path.display()
                        ),
                        hint: None,
                    })?;
            tracing::debug!(
                socket_path = port_forward_socket_str,
                guest_port = PORT_FWD_VSOCK_PORT,
                "Configuring vsock bridge for CRI port-forward control"
            );
            ctx.add_vsock_port(PORT_FWD_VSOCK_PORT, port_forward_socket_str, true)?;
        }
    }

    // Configure the Windows host-control worker. WHPX named-pipe mappings are
    // guest-initiated, so host exec cannot directly bridge to the guest's 4089
    // listener. The worker exposes a local exec pipe and tunnels the unchanged
    // exec protocol over the long-lived guest-initiated 4093 channel.
    #[cfg(target_os = "windows")]
    let windows_port_forward_manager = {
        // Note: PTY and attestation channels are not yet implemented on Windows.
        // The 4093 channel also carries lifecycle signals, so it must exist even
        // when the box has no published ports.
        let socket_dir = spec
            .exec_socket_path
            .parent()
            .ok_or_else(|| BoxError::BoxBootError {
                message: format!(
                    "Windows exec socket path has no parent: {}",
                    spec.exec_socket_path.display()
                ),
                hint: None,
            })?;
        let stop_request = socket_dir.join(WINDOWS_STOP_REQUEST_FILE);
        let port_forward_manager = windows_port_forward::spawn_port_forward_manager(
            &spec.box_id,
            &spec.port_map,
            &stop_request,
        )?;
        tracing::info!(
            port_map = ?spec.port_map,
            pipe_name = %port_forward_manager.pipe_name(),
            guest_port = PORT_FWD_VSOCK_PORT,
            stop_request = %stop_request.display(),
            "Configuring Windows host-control channel"
        );
        ctx.add_vsock_port_windows(PORT_FWD_VSOCK_PORT, port_forward_manager.pipe_name())?;
        port_forward_manager
    };

    // Note: A3S_TEE_SIMULATE is already included in spec.entrypoint.env
    // (added by vm.rs when simulate mode is on) and passed to the guest init
    // via krun_set_exec's envp parameter. Do NOT call set_env here — libkrun's
    // krun_set_env overwrites (not appends) the environment, which would erase
    // all BOX_EXEC_* vars set by set_exec.
    if spec
        .entrypoint
        .env
        .iter()
        .any(|(k, _)| k == "A3S_TEE_SIMULATE")
    {
        tracing::info!("TEE simulation mode: A3S_TEE_SIMULATE=1 included in entrypoint env");
    }

    // Configure networking: virtio-net (passt on Linux, gvproxy on macOS) or TSI (default)
    #[cfg(not(target_os = "windows"))]
    if let Some(ref net_config) = spec.network {
        #[cfg(unix)]
        tracing::info!(
            ip = %net_config.ip_address,
            gateway = %net_config.gateway,
            mac = ?net_config.mac_address,
            socket = %net_config.net_socket_path.display(),
            net_socket_fd = net_config.net_socket_fd,
            net_proxy_fd = net_config.net_proxy_fd,
            "Configuring virtio-net networking"
        );
        #[cfg(not(unix))]
        tracing::info!(
            ip = %net_config.ip_address,
            gateway = %net_config.gateway,
            mac = ?net_config.mac_address,
            socket = %net_config.net_socket_path.display(),
            "Configuring virtio-net networking"
        );

        #[cfg(target_os = "linux")]
        if let Some(fd) = net_config.net_socket_fd {
            let proxy_fd = net_config
                .net_proxy_fd
                .ok_or_else(|| BoxError::BoxBootError {
                    message: "Linux bridge networking is missing its inherited proxy descriptor"
                        .to_string(),
                    hint: None,
                })?;
            let bridge_socket_dir =
                net_config
                    .bridge_socket_dir
                    .clone()
                    .ok_or_else(|| BoxError::BoxBootError {
                        message: "Linux bridge networking is missing its peer switch directory"
                            .to_string(),
                        hint: None,
                    })?;
            spawn_inherited_passt_bridge(
                proxy_fd,
                net_config.net_socket_path.clone(),
                bridge_socket_dir,
                net_config.mac_address,
            )?;
            log_inherited_net_fd(fd);
            ctx.add_net_unixstream_fd(fd, &net_config.mac_address)?;
        } else {
            let socket_str =
                net_config
                    .net_socket_path
                    .to_str()
                    .ok_or_else(|| BoxError::BoxBootError {
                        message: format!(
                            "Invalid network socket path: {}",
                            net_config.net_socket_path.display()
                        ),
                        hint: None,
                    })?;
            ctx.add_net_unixstream(socket_str, &net_config.mac_address)?;
        }
        #[cfg(target_os = "macos")]
        if let Some(fd) = net_config.net_socket_fd {
            if let Some(proxy_fd) = net_config.net_proxy_fd {
                spawn_inherited_netproxy(
                    proxy_fd,
                    InheritedNetProxyConfig {
                        guest_ip: net_config.ip_address,
                        gateway: net_config.gateway,
                        prefix_len: net_config.prefix_len,
                        dns_servers: &net_config.dns_servers,
                        port_map: &spec.port_map,
                        stats_path: net_config.net_stats_path.clone(),
                        bridge_socket_dir: net_config.bridge_socket_dir.clone(),
                        own_mac: net_config.mac_address,
                    },
                )?;
            }
            log_inherited_net_fd(fd);
            ctx.add_net_unixgram_fd(fd, &net_config.mac_address)?;
        } else {
            let socket_str =
                net_config
                    .net_socket_path
                    .to_str()
                    .ok_or_else(|| BoxError::BoxBootError {
                        message: format!(
                            "Invalid network socket path: {}",
                            net_config.net_socket_path.display()
                        ),
                        hint: None,
                    })?;
            ctx.add_net_unixgram(socket_str, &net_config.mac_address)?;
        }

        // Network env vars (A3S_NET_IP, A3S_NET_GATEWAY, A3S_NET_DNS) are now
        // injected into spec.entrypoint.env by vm.rs, so they are passed via
        // krun_set_exec's envp alongside all BOX_EXEC_* vars. Do NOT call
        // ctx.set_env here — libkrun's krun_set_env overwrites (not appends)
        // the environment, which would erase all vars set by set_exec.
    }

    // Configure user/group from OCI USER directive
    if let Some(ref user) = spec.user {
        apply_user_config(&ctx, user)?;
    }

    // Keep split-console descriptors owned by the shim until start_enter
    // returns. Closing them before the final log drain establishes a real EOF
    // boundary; leaking them with mem::forget made short detached output race
    // the processor indefinitely.
    #[cfg(unix)]
    let mut split_console_files: Option<(std::fs::File, std::fs::File)> = None;

    // Configure console output if specified
    if let Some(console_path) = &spec.console_output {
        let console_str = console_path
            .to_str()
            .ok_or_else(|| BoxError::BoxBootError {
                message: format!("Invalid console output path: {}", console_path.display()),
                hint: None,
            })?;
        // Split console: guest stdout -> console.log, stderr -> console.err.log
        // (libkrun's 3-fd virtio-console separates the streams), so the log
        // processor can tag each line's stream like Docker's json-file driver.
        // Unix only (uses raw fds); Windows always uses the merged single-file
        // console. Falls back to merged if the err file can't be opened, and
        // BOX_NO_SPLIT_STDERR forces the legacy merged behavior.
        #[allow(unused_mut)]
        let mut split_done = false;
        #[cfg(unix)]
        if std::env::var_os("BOX_NO_SPLIT_STDERR").is_none() {
            use std::os::unix::io::AsRawFd;
            let err_path = console_path.with_file_name("console.err.log");
            let open = |p: &std::path::Path| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)
            };
            if let (Ok(out_f), Ok(err_f)) = (open(console_path), open(&err_path)) {
                ctx.add_split_console(-1, out_f.as_raw_fd(), err_f.as_raw_fd())?;
                split_console_files = Some((out_f, err_f));
                tracing::debug!("split console enabled (stdout/stderr separated)");
                split_done = true;
            }
        }
        if !split_done {
            tracing::debug!(
                console_path = console_str,
                "Redirecting console output (merged)"
            );
            ctx.set_console_output(console_str)?;
        }
    }

    // Configure TEE if specified (only available on Linux with SEV support)
    #[cfg(target_os = "linux")]
    if let Some(ref tee_config) = spec.tee_config {
        tracing::info!(
            tee_type = %tee_config.tee_type,
            config_path = %tee_config.config_path.display(),
            "Configuring TEE"
        );

        // Enable split IRQ chip (required for TEE)
        ctx.enable_split_irqchip()?;

        // Set TEE configuration file
        let tee_config_str = tee_config.config_path.to_str().ok_or_else(|| {
            BoxError::TeeConfig(format!(
                "Invalid TEE config path: {}",
                tee_config.config_path.display()
            ))
        })?;
        ctx.set_tee_config(tee_config_str)?;

        tracing::info!("TEE configured successfully");
    }

    #[cfg(not(target_os = "linux"))]
    if spec.tee_config.is_some() {
        tracing::warn!("TEE configuration is only supported on Linux; ignoring");
    }

    // Apply CPU pinning via sched_setaffinity (Linux only)
    #[cfg(target_os = "linux")]
    if let Some(ref cpuset) = spec.resource_limits.cpuset_cpus {
        if let Err(e) = apply_cpuset(cpuset) {
            tracing::warn!(cpuset = cpuset, error = %e, "Failed to apply CPU pinning");
        }
    }

    // CPU/memory cgroup limits (--cpu-shares/--cpu-quota/--memory-reservation/
    // --memory-swap) are NOT applied to the host VM process: they are enforced
    // INSIDE the guest by guest-init's per-container cgroup (the workload runs in
    // the microVM, so the in-guest cgroup is what bounds it — real-VM measured
    // cpu.max actively throttling a 0.5-core cap). The old host-side
    // apply_cgroup_limits created /sys/fs/cgroup/a3s-box/<id> and wrote
    // cpu.weight/cpu.max/memory.* there, but the controllers were never delegated
    // to that subtree (writes ENOENT'd, swallowed) so it never enforced anything —
    // it only leaked an empty host cgroup. Removed. (cpuset above stays: CPU
    // pinning of the VM threads has no in-guest equivalent.)

    // Spawn the log processor on a dedicated thread for the box's lifetime. The
    // shim owns console.log and lives exactly as long as the VM, so this is the
    // daemonless home for log processing — a detached `run -d` box keeps logging
    // after the launching CLI exits (the processor used to die with that CLI).
    let log_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let log_ready = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    #[cfg(target_os = "windows")]
    let windows_guest_rootfs = windows_rootfs_path(spec)?.to_path_buf();
    let log_thread = spec.console_output.as_ref().map(|console| {
        let console = console.clone();
        let log_dir = console
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let config = spec.log_config.clone();
        let stop = log_stop.clone();
        let ready = log_ready.clone();
        std::thread::spawn(move || {
            #[cfg(target_os = "windows")]
            {
                // The Windows init wrapper persists the workload streams in the
                // shared rootfs because the WHPX console contains only boot
                // diagnostics. Tail those files directly so `logs` is live.
                let stdout = windows_guest_rootfs.join(WINDOWS_GUEST_STDOUT);
                let stderr = windows_guest_rootfs.join(WINDOWS_GUEST_STDERR);
                a3s_box_core::log::run_log_processor_streams_with_ready(
                    &stdout,
                    &stderr,
                    &log_dir,
                    &config,
                    &stop,
                    Some(&ready),
                );
            }
            #[cfg(not(target_os = "windows"))]
            a3s_box_core::log::run_log_processor_with_ready(
                &console,
                &log_dir,
                &config,
                &stop,
                Some(&ready),
            );
        })
    });

    if log_thread.is_some() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while log_ready.load(std::sync::atomic::Ordering::Acquire) < 2 {
            if std::time::Instant::now() >= deadline {
                log_stop.store(true, std::sync::atomic::Ordering::SeqCst);
                if let Some(handle) = log_thread {
                    let _ = handle.join();
                }
                return Err(BoxError::BoxBootError {
                    message: "log processor did not become ready before VM start".to_string(),
                    hint: None,
                });
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    // Start VM. start_enter RETURNS with the guest exit status once the guest
    // exits (status >= 0) or on a start failure (status < 0).
    tracing::info!(box_id = %spec.box_id, "Starting VM (process takeover)");
    let status = ctx.start_enter();

    // No guest writes are valid after start_enter returns. Close the shim's
    // console descriptors before signaling the readers so their final EOF is
    // authoritative and all kernel-buffered output is visible.
    #[cfg(unix)]
    drop(split_console_files);

    // Guest has exited and console.log is fully flushed: signal the processor to
    // drain the remainder and stop, then join so the final lines reach
    // container.json before this process exits (no teardown race).
    log_stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _log_thread_drained = log_thread.is_some_and(|handle| handle.join().is_ok());
    if let Some(console) = spec.console_output.as_ref() {
        let structured = a3s_box_core::log::json_log_path(
            console
                .parent()
                .unwrap_or_else(|| std::path::Path::new(".")),
        );
        #[cfg(not(target_os = "windows"))]
        {
            let stderr_console = a3s_box_core::log::stderr_console_path(console);
            let structured_empty = structured.metadata().map(|m| m.len() == 0).unwrap_or(true);
            let raw_has_output = console.metadata().map(|m| m.len() > 0).unwrap_or(false)
                || stderr_console
                    .metadata()
                    .map(|m| m.len() > 0)
                    .unwrap_or(false);
            if structured_empty
                && raw_has_output
                && spec.log_config.driver == a3s_box_core::log::LogDriver::JsonFile
            {
                // A very short VM can finish while the first processor is sitting
                // on a provisional console EOF. Its raw files are authoritative at
                // this point (start_enter returned and the write fds are closed), so
                // repair the empty projection synchronously. The empty guard makes
                // this idempotent and prevents duplicate records.
                a3s_box_core::log::run_log_processor(
                    console,
                    console
                        .parent()
                        .unwrap_or_else(|| std::path::Path::new(".")),
                    &spec.log_config,
                    &log_stop,
                );
            }
        }

        #[cfg(target_os = "windows")]
        {
            let rootfs = windows_rootfs_path(spec)?;
            let stdout = rootfs.join(WINDOWS_GUEST_STDOUT);
            let stderr = rootfs.join(WINDOWS_GUEST_STDERR);
            let structured_empty = structured.metadata().map(|m| m.len() == 0).unwrap_or(true);
            let raw_has_output = [&stdout, &stderr].iter().any(|path| {
                a3s_box_core::windows_file::open_regular_file(path, None)
                    .and_then(|(file, _)| file.metadata())
                    .is_ok_and(|metadata| metadata.len() > 0)
            });
            if structured_empty
                && raw_has_output
                && spec.log_config.driver == a3s_box_core::log::LogDriver::JsonFile
            {
                // A short WHPX workload can finish before its live tailers emit
                // the first record. The completed rootfs streams are authoritative
                // after start_enter returns, so project them synchronously once.
                a3s_box_core::log::run_log_processor_streams(
                    &stdout,
                    &stderr,
                    console
                        .parent()
                        .unwrap_or_else(|| std::path::Path::new(".")),
                    &spec.log_config,
                    &log_stop,
                );
            }
        }
    }

    #[cfg(target_os = "windows")]
    if status >= 0 && _log_thread_drained {
        let marker = windows_rootfs_path(spec)?.join(WINDOWS_LIVE_LOGS_DRAINED_MARKER);
        a3s_box_core::windows_file::replace_regular_file(&marker, b"drained\n").map_err(
            |error| BoxError::BoxBootError {
                message: format!(
                    "Failed to mark Windows live logs drained at {}: {error}",
                    marker.display()
                ),
                hint: None,
            },
        )?;
    }

    #[cfg(target_os = "windows")]
    drop(windows_port_forward_manager);

    // `std::process::exit` below skips Rust destructors. On Windows,
    // `start_enter` returns after guest shutdown and the KrunContext owns the
    // WHPX partition until `krun_free_ctx` runs in Drop. Free it explicitly so
    // a following restart/new VM cannot race Windows' asynchronous process-handle
    // cleanup and boot into a live shim with no functioning guest channel.
    #[cfg(target_os = "windows")]
    drop(ctx);
    // If we reach here, either:
    // 1. VM failed to start (negative status)
    // 2. VM started and guest exited (non-negative status)
    if status < 0 {
        if status == -22 {
            return Err(BoxError::BoxBootError {
                message: "libkrun returned EINVAL - invalid configuration".to_string(),
                hint: Some("Check VM configuration (rootfs, entrypoint, etc.)".to_string()),
            });
        }
        Err(BoxError::BoxBootError {
            message: format!("VM failed to start with status {}", status),
            hint: None,
        })
    } else {
        // VM started and guest exited — propagate the guest exit code to the host.
        tracing::info!(exit_status = status, "VM exited");
        std::process::exit(status);
    }
}

#[cfg(target_os = "windows")]
pub(super) fn configure_windows_kernel(ctx: &KrunContext) -> Result<()> {
    let Some(kernel_path) = std::env::var_os("A3S_BOX_KERNEL").map(PathBuf::from) else {
        return Ok(());
    };

    if !kernel_path.is_file() {
        return Err(BoxError::BoxBootError {
            message: format!(
                "A3S_BOX_KERNEL does not point to a file: {}",
                kernel_path.display()
            ),
            hint: Some(
                "Provide an x86_64 ELF vmlinux or the kernel file from an official WSL package"
                    .to_string(),
            ),
        });
    }

    let kernel_format = detect_windows_kernel_format(&kernel_path)?;
    let kernel_path_str = kernel_path.to_str().ok_or_else(|| BoxError::BoxBootError {
        message: format!(
            "A3S_BOX_KERNEL is not valid UTF-8: {}",
            kernel_path.display()
        ),
        hint: None,
    })?;

    tracing::info!(
        kernel = %kernel_path.display(),
        kernel_format,
        "Using external Windows guest kernel"
    );
    unsafe { ctx.set_kernel(kernel_path_str, kernel_format, None, None) }
}

#[cfg(target_os = "windows")]
pub(super) fn detect_windows_kernel_format(path: &Path) -> Result<u32> {
    let mut file = File::open(path).map_err(|e| BoxError::BoxBootError {
        message: format!(
            "Failed to open Windows guest kernel {}: {e}",
            path.display()
        ),
        hint: None,
    })?;
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic)
        .map_err(|e| BoxError::BoxBootError {
            message: format!(
                "Failed to read Windows guest kernel {}: {e}",
                path.display()
            ),
            hint: None,
        })?;

    kernel_format_from_magic(magic).ok_or_else(|| BoxError::BoxBootError {
        message: format!(
            "Unsupported Windows guest kernel format in {}",
            path.display()
        ),
        hint: Some(
            "Expected an ELF vmlinux or the PE/COFF kernel file from an official WSL package"
                .to_string(),
        ),
    })
}

#[cfg(target_os = "windows")]
pub(super) fn kernel_format_from_magic(magic: [u8; 4]) -> Option<u32> {
    match magic {
        [0x7f, b'E', b'L', b'F'] => Some(KRUN_KERNEL_FORMAT_ELF),
        [b'M', b'Z', _, _] => Some(KRUN_KERNEL_FORMAT_IMAGE_GZ),
        _ => None,
    }
}
