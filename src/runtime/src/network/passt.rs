//! Passt process management for virtio-net networking.
//!
//! Manages the lifecycle of `passt` daemon instances that provide
//! the virtio-net backend for bridge-mode networking. Each box gets
//! its own passt process with a dedicated Unix socket.

use a3s_box_core::error::{BoxError, Result};
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const PASST_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const PASST_STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(50);
const PASST_STARTUP_STABILITY_WINDOW: Duration = Duration::from_millis(250);

/// Manages a passt daemon instance for a single box.
#[derive(Debug)]
pub struct PasstManager {
    /// Path to the passt Unix socket.
    socket_path: PathBuf,
    /// Path to passt's guest-side packet capture.
    pcap_path: PathBuf,
    /// Child process handle (None if not started).
    child: Option<Child>,
    /// PID file path for the passt process.
    pid_file: PathBuf,
    /// libkrun endpoint inherited by the shim.
    net_socket_fd: Option<OwnedFd>,
    /// Multiplexer endpoint inherited by the shim.
    net_proxy_fd: Option<OwnedFd>,
}

impl PasstManager {
    /// Create a new PasstManager.
    ///
    /// The socket and PID file are placed directly in the provided runtime
    /// socket directory (the same directory that holds the exec/PTY control
    /// sockets, e.g. `/tmp/a3s-box-sockets/<box_id>`).
    ///
    /// This directory MUST be reachable by the user passt runs as. When passt
    /// is started as root it drops privileges to `nobody`, so the socket
    /// directory has to be world-traversable — the box's `~/.a3s/boxes/<id>`
    /// home is mode 0700 for root and would leave passt unable to bind its
    /// socket, silently breaking all bridge networking and Compose.
    pub fn new(socket_dir: &Path) -> Self {
        Self {
            socket_path: socket_dir.join("passt.sock"),
            pcap_path: socket_dir.join("passt.pcap"),
            pid_file: socket_dir.join("passt.pid"),
            child: None,
            net_socket_fd: None,
            net_proxy_fd: None,
        }
    }

    /// Get the passt socket path.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Get the passt packet capture path.
    pub fn pcap_path(&self) -> &Path {
        &self.pcap_path
    }

    /// Insert a shim-hosted peer switch between libkrun and this passt process.
    pub fn enable_peer_bridge(&mut self) -> Result<()> {
        if self.net_socket_fd.is_some() || self.net_proxy_fd.is_some() {
            return Ok(());
        }
        let mut descriptors = [-1; 2];
        #[cfg(target_os = "linux")]
        let socket_type = libc::SOCK_STREAM | libc::SOCK_CLOEXEC;
        #[cfg(not(target_os = "linux"))]
        let socket_type = libc::SOCK_STREAM;
        let result =
            unsafe { libc::socketpair(libc::AF_UNIX, socket_type, 0, descriptors.as_mut_ptr()) };
        if result != 0 {
            return Err(BoxError::NetworkError(format!(
                "failed to create passt bridge socketpair: {}",
                std::io::Error::last_os_error()
            )));
        }
        // SAFETY: socketpair returned two new, uniquely owned descriptors.
        self.net_socket_fd = Some(unsafe { OwnedFd::from_raw_fd(descriptors[0]) });
        self.net_proxy_fd = Some(unsafe { OwnedFd::from_raw_fd(descriptors[1]) });
        Ok(())
    }

    pub fn net_socket_fd(&self) -> Option<RawFd> {
        self.net_socket_fd.as_ref().map(AsRawFd::as_raw_fd)
    }

    pub fn net_proxy_fd(&self) -> Option<RawFd> {
        self.net_proxy_fd.as_ref().map(AsRawFd::as_raw_fd)
    }

    /// Spawn the passt daemon.
    ///
    /// Configures passt with:
    /// - Unix socket mode (no PID namespace)
    /// - The assigned IP, gateway, prefix length
    /// - DNS forwarding
    /// - No DHCP (static IP assignment)
    /// - Inbound TCP port forwarding for any published ports (`port_map`)
    pub fn spawn(
        &mut self,
        ip: Ipv4Addr,
        gateway: Ipv4Addr,
        prefix_len: u8,
        dns_servers: &[Ipv4Addr],
        port_map: &[String],
    ) -> Result<()> {
        // Ensure parent directory exists.
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                BoxError::NetworkError(format!(
                    "failed to create socket directory {}: {}",
                    parent.display(),
                    e
                ))
            })?;

            // passt drops privileges to `nobody` when launched as root, so the
            // directory it binds its socket (and writes its PID file) in must be
            // writable by that user. Widen the directory permissions; the path
            // is an ephemeral, per-box runtime directory under a world-traversable
            // base, so this only affects this box's control sockets.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Err(e) =
                    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o777))
                {
                    tracing::warn!(
                        dir = %parent.display(),
                        error = %e,
                        "Failed to widen passt socket directory permissions; \
                         passt may be unable to bind its socket after dropping privileges"
                    );
                }
            }
        }

        self.remove_launch_artifacts();

        match self.spawn_attempt(ip, gateway, prefix_len, dns_servers, port_map, false) {
            Ok(()) => Ok(()),
            Err(initial_error) => {
                let stderr = self.read_stderr();
                if !should_retry_passt_as_root(&stderr) {
                    return Err(initial_error);
                }

                tracing::warn!(
                    error = %initial_error,
                    "passt could not create its unprivileged user namespace; retrying with root as the sandbox identity"
                );
                self.cleanup_failed_launch();
                self.spawn_attempt(ip, gateway, prefix_len, dns_servers, port_map, true)
            }
        }
    }

    fn spawn_attempt(
        &mut self,
        ip: Ipv4Addr,
        gateway: Ipv4Addr,
        prefix_len: u8,
        dns_servers: &[Ipv4Addr],
        port_map: &[String],
        run_as_root: bool,
    ) -> Result<()> {
        self.remove_launch_artifacts();

        let mut cmd = Command::new("passt");
        if run_as_root {
            cmd.arg("--runas").arg("0:0");
        }
        cmd.arg("--socket")
            .arg(&self.socket_path)
            .arg("--pid")
            .arg(&self.pid_file)
            .arg("--pcap")
            .arg(&self.pcap_path)
            // Run in foreground (we manage the process)
            .arg("--foreground")
            // Configure the network
            .arg("--address")
            .arg(ip.to_string())
            .arg("--gateway")
            .arg(gateway.to_string())
            .arg("--netmask")
            .arg(format!("{}", prefix_to_netmask(prefix_len)));

        // Add DNS servers
        for dns in dns_servers {
            cmd.arg("--dns").arg(dns.to_string());
        }

        // Forward published TCP ports into the guest. libkrun discards the
        // TSI host_port_map once a virtio-net device is attached, so passt is
        // what actually publishes `-p host:guest` in bridge mode. Auto-assigned
        // host ports (host_port == 0) cannot be forwarded by passt and are
        // skipped. passt accepts a comma-separated `host:guest,...` spec.
        let tcp_specs = passt_tcp_port_specs(port_map);
        if !tcp_specs.is_empty() {
            let spec = tcp_specs.join(",");
            tracing::info!(tcp_ports = %spec, "Configuring passt inbound TCP port forwarding");
            cmd.arg("--tcp-ports").arg(spec);
        }

        // Capture passt's stderr to a log file so spawn failures (bad args,
        // unsupported flags, permission errors after dropping privileges) are
        // diagnosable instead of silently discarded to /dev/null.
        cmd.stdout(std::process::Stdio::null());
        match self
            .stderr_path()
            .and_then(|p| std::fs::File::create(p).ok())
        {
            Some(file) => {
                cmd.stderr(std::process::Stdio::from(file));
            }
            None => {
                cmd.stderr(std::process::Stdio::null());
            }
        }

        let child = cmd.spawn().map_err(|e| {
            BoxError::NetworkError(format!(
                "failed to spawn passt: {} (is passt installed?)",
                e
            ))
        })?;

        tracing::info!(
            pid = child.id(),
            socket = %self.socket_path.display(),
            ip = %ip,
            gateway = %gateway,
            run_as_root,
            "Passt daemon started"
        );

        self.child = Some(child);

        // A socket can appear before passt finishes creating its namespaces and
        // seccomp sandbox. Require the process to remain alive briefly after
        // the socket is visible so a late sandbox failure cannot be mistaken
        // for a usable network backend.
        self.wait_for_socket()?;

        Ok(())
    }

    /// Wait for the passt socket to become available.
    ///
    /// Also detects immediate passt exit (e.g. bad args or a permission failure
    /// after dropping privileges) so the real cause is surfaced instead of a
    /// misleading 5-second timeout.
    fn wait_for_socket(&mut self) -> Result<()> {
        let started_at = Instant::now();
        let mut socket_seen_at = None;
        while started_at.elapsed() < PASST_STARTUP_TIMEOUT {
            if let Some(child) = self.child.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    // Early exit — try_wait reaped it, so nothing lingers.
                    return Err(BoxError::NetworkError(format!(
                        "passt exited early with {status} during startup{}",
                        self.stderr_tail()
                    )));
                }
            }

            if self.socket_path.exists() {
                let seen_at = socket_seen_at.get_or_insert_with(Instant::now);
                if seen_at.elapsed() >= PASST_STARTUP_STABILITY_WINDOW {
                    return Ok(());
                }
            }
            std::thread::sleep(PASST_STARTUP_POLL_INTERVAL);
        }

        // Timed out with passt still alive: kill the child we spawned so it does
        // not linger holding the published port. `Drop` deliberately leaves a
        // healthy passt running (detached use), and the pid-file fallback used by
        // boot-failure cleanup can miss it if passt hasn't written the file yet —
        // so reap it here directly via the handle we hold.
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }

        Err(BoxError::NetworkError(format!(
            "passt socket {} did not appear within 5 seconds{}",
            self.socket_path.display(),
            self.stderr_tail()
        )))
    }

    fn stderr_path(&self) -> Option<PathBuf> {
        self.socket_path
            .parent()
            .map(|parent| parent.join("passt.stderr.log"))
    }

    fn read_stderr(&self) -> String {
        self.stderr_path()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .unwrap_or_default()
    }

    fn stderr_tail(&self) -> String {
        let stderr = self.read_stderr();
        let mut tail: Vec<&str> = stderr.lines().rev().take(4).collect();
        tail.reverse();
        if tail.is_empty() {
            String::new()
        } else {
            format!(" (passt stderr: {})", tail.join("; "))
        }
    }

    fn cleanup_failed_launch(&mut self) {
        if let Some(mut child) = self.child.take() {
            match child.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
        self.remove_launch_artifacts();
    }

    fn remove_launch_artifacts(&self) {
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_file(&self.pcap_path);
        let _ = std::fs::remove_file(&self.pid_file);
        if let Some(stderr_path) = self.stderr_path() {
            let _ = std::fs::remove_file(stderr_path);
        }
    }

    /// Stop the passt daemon.
    pub fn stop(&mut self) {
        if let Some(ref mut child) = self.child {
            let pid = child.id();
            if let Err(e) = child.kill() {
                tracing::warn!(pid, error = %e, "Failed to kill passt process");
            } else {
                // Reap the child to avoid zombies
                let _ = child.wait();
                tracing::info!(pid, "Passt daemon stopped");
            }
        }
        self.child = None;
        self.net_socket_fd = None;
        self.net_proxy_fd = None;

        // Clean up socket and PID file
        std::fs::remove_file(&self.socket_path).ok();
        std::fs::remove_file(&self.pcap_path).ok();
        std::fs::remove_file(&self.pid_file).ok();
    }

    /// Check if the passt process is still running.
    pub fn is_running(&mut self) -> bool {
        match self.child {
            Some(ref mut child) => child.try_wait().ok().flatten().is_none(),
            None => false,
        }
    }
}

fn passt_sandbox_was_denied(stderr: &str) -> bool {
    stderr.contains("Failed to sandbox process")
        && (stderr.contains("unshare: Operation not permitted")
            || stderr.contains("Operation not permitted"))
}

#[cfg(target_os = "linux")]
fn should_retry_passt_as_root(stderr: &str) -> bool {
    let launcher_is_root = unsafe { libc::geteuid() == 0 };
    launcher_is_root && passt_sandbox_was_denied(stderr)
}

#[cfg(not(target_os = "linux"))]
fn should_retry_passt_as_root(_stderr: &str) -> bool {
    false
}

impl Drop for PasstManager {
    fn drop(&mut self) {
        // Intentionally does NOT kill passt. passt must outlive the process that
        // spawned it: a detached `run -d` returns while the box keeps running,
        // and the VM (driven by the shim) outlives the CLI. Killing on drop here
        // is exactly what previously left detached bridge boxes with dead
        // networking. passt is reaped on box stop/rm via `terminate_passt`, which
        // uses the PID file as the source of truth.
    }
}

/// Terminate a passt daemon by its PID file and remove its socket/PID files.
///
/// passt outlives the `PasstManager` that launched it (so detached boxes keep
/// working after the CLI exits), so box teardown cannot rely on a live handle —
/// the PID file written into the box's runtime socket directory is authoritative.
pub fn terminate_passt(socket_dir: &Path) {
    let pid_file = socket_dir.join("passt.pid");
    if let Ok(contents) = std::fs::read_to_string(&pid_file) {
        if let Ok(pid) = contents.trim().parse::<i32>() {
            // Verify the pid is still passt before signalling: the pid file is a
            // stale snapshot, so if passt already exited and the kernel recycled
            // its pid, a bare kill would SIGTERM an unrelated process.
            if pid > 1 && pid_is_passt(pid) {
                // SIGTERM; passt exits and is reaped by its (re)parent.
                #[cfg(unix)]
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
                tracing::info!(pid, "Terminated passt daemon");
            }
        }
    }
    let _ = std::fs::remove_file(&pid_file);
    let _ = std::fs::remove_file(socket_dir.join("passt.sock"));
    let _ = std::fs::remove_file(socket_dir.join("passt.pcap"));
}

/// Best-effort check that `pid` is actually a passt process, to avoid SIGTERM-ing
/// an unrelated process that recycled the pid after passt exited. Reads
/// `/proc/<pid>/comm`; on any error (pid gone, no `/proc`) returns false so the
/// stale pid is left alone — a genuinely-dead passt was already reaped by its
/// reparent.
#[cfg(target_os = "linux")]
fn pid_is_passt(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|comm| passt_process_name_is_known(comm.trim()))
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn passt_process_name_is_known(name: &str) -> bool {
    // Debian/Ubuntu's passt executable selects its optimized implementation at
    // startup and re-execs the packaged passt.avx2 binary. Keep this allowlist
    // explicit so a recycled PID with a merely similar process name is not
    // signalled during box teardown.
    matches!(name, "passt" | "passt.avx2")
}

#[cfg(not(target_os = "linux"))]
fn pid_is_passt(_pid: i32) -> bool {
    // No procfs to consult; passt is Linux-only, so this path is unreachable in
    // practice — preserve the prior kill-by-pid-file behavior.
    true
}

impl super::NetworkBackend for PasstManager {
    fn socket_path(&self) -> &std::path::Path {
        self.socket_path()
    }

    fn stop(&mut self) {
        self.stop();
    }
}

/// Convert published ports to passt's inbound TCP forwarding spec entries.
fn passt_tcp_port_specs(port_map: &[String]) -> Vec<String> {
    port_map
        .iter()
        .filter_map(|m| a3s_box_core::parse_port_mapping(m).ok())
        .filter(|m| m.host_port != 0)
        .map(|m| format!("{}:{}", m.host_port, m.guest_port))
        .collect()
}

/// Convert a prefix length to a dotted-decimal netmask string.
fn prefix_to_netmask(prefix: u8) -> Ipv4Addr {
    if prefix == 0 {
        return Ipv4Addr::new(0, 0, 0, 0);
    }
    let mask = !((1u32 << (32 - prefix)) - 1);
    Ipv4Addr::from(mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prefix_to_netmask() {
        assert_eq!(prefix_to_netmask(24), Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(prefix_to_netmask(16), Ipv4Addr::new(255, 255, 0, 0));
        assert_eq!(prefix_to_netmask(8), Ipv4Addr::new(255, 0, 0, 0));
        assert_eq!(prefix_to_netmask(32), Ipv4Addr::new(255, 255, 255, 255));
        assert_eq!(prefix_to_netmask(0), Ipv4Addr::new(0, 0, 0, 0));
        assert_eq!(prefix_to_netmask(28), Ipv4Addr::new(255, 255, 255, 240));
    }

    #[test]
    fn test_passt_tcp_port_specs_skips_invalid_and_auto_assigned_ports() {
        let specs = passt_tcp_port_specs(&[
            "8080:80".to_string(),
            "0:443".to_string(),
            "not-a-port-map".to_string(),
            "9000:90/tcp".to_string(),
        ]);

        assert_eq!(specs, vec!["8080:80", "9000:90"]);
    }

    #[test]
    fn passt_root_retry_requires_an_explicit_sandbox_permission_failure() {
        assert!(passt_sandbox_was_denied(
            "unshare: Operation not permitted\nFailed to sandbox process, exiting"
        ));
        assert!(!passt_sandbox_was_denied(
            "Failed to bind UNIX domain socket"
        ));
        assert!(!passt_sandbox_was_denied(
            "Failed to sandbox process, exiting"
        ));
    }

    #[test]
    fn test_passt_manager_new() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = PasstManager::new(dir.path());
        assert_eq!(mgr.socket_path(), dir.path().join("passt.sock"));
        assert_eq!(mgr.pcap_path(), dir.path().join("passt.pcap"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn peer_bridge_descriptors_are_close_on_exec_until_the_shim_claims_them() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = PasstManager::new(dir.path());

        manager.enable_peer_bridge().unwrap();

        for descriptor in [manager.net_socket_fd(), manager.net_proxy_fd()] {
            let descriptor = descriptor.unwrap();
            let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
            assert_ne!(flags, -1);
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }
    }

    #[test]
    fn test_passt_manager_implements_network_backend() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = PasstManager::new(dir.path());
        let socket_path = dir.path().join("passt.sock");
        let backend: &mut dyn crate::network::NetworkBackend = &mut mgr;

        assert_eq!(backend.socket_path(), socket_path.as_path());
        backend.stop();
    }

    #[test]
    fn test_passt_manager_not_running_initially() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = PasstManager::new(dir.path());
        assert!(!mgr.is_running());
    }

    #[test]
    fn test_passt_manager_stop_when_not_started() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = PasstManager::new(dir.path());
        // Should not panic
        mgr.stop();
        assert!(!mgr.is_running());
    }

    #[test]
    fn test_passt_manager_stop_removes_artifacts_without_child() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = PasstManager::new(dir.path());
        std::fs::write(&mgr.socket_path, "socket").unwrap();
        std::fs::write(&mgr.pcap_path, "pcap").unwrap();
        std::fs::write(&mgr.pid_file, "123").unwrap();

        mgr.stop();

        assert!(!mgr.socket_path.exists());
        assert!(!mgr.pcap_path.exists());
        assert!(!mgr.pid_file.exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_passt_manager_stop_kills_child_and_removes_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = PasstManager::new(dir.path());
        mgr.child = Some(
            Command::new("sh")
                .arg("-c")
                .arg("sleep 30")
                .spawn()
                .unwrap(),
        );
        std::fs::write(&mgr.socket_path, "socket").unwrap();
        std::fs::write(&mgr.pcap_path, "pcap").unwrap();
        std::fs::write(&mgr.pid_file, "123").unwrap();

        assert!(mgr.is_running());
        mgr.stop();

        assert!(!mgr.is_running());
        assert!(!mgr.socket_path.exists());
        assert!(!mgr.pcap_path.exists());
        assert!(!mgr.pid_file.exists());
    }

    #[test]
    fn test_passt_manager_socket_path() {
        let dir = tempfile::tempdir().unwrap();
        let box_dir = dir.path().join("boxes").join("test-box-id");
        let mgr = PasstManager::new(&box_dir);
        assert_eq!(mgr.socket_path(), box_dir.join("passt.sock"));
        assert_eq!(mgr.pcap_path(), box_dir.join("passt.pcap"));
    }

    #[test]
    fn test_spawn_returns_directory_creation_error_before_running_passt() {
        let dir = tempfile::tempdir().unwrap();
        let socket_dir = dir.path().join("socket-dir-is-file");
        std::fs::write(&socket_dir, "not a directory").unwrap();
        let mut mgr = PasstManager::new(&socket_dir);

        let err = mgr
            .spawn(
                Ipv4Addr::new(10, 0, 2, 15),
                Ipv4Addr::new(10, 0, 2, 2),
                24,
                &[Ipv4Addr::new(1, 1, 1, 1)],
                &["8080:80".to_string()],
            )
            .unwrap_err();

        assert!(err
            .to_string()
            .contains("failed to create socket directory"));
        assert!(!mgr.is_running());
    }

    #[test]
    fn test_wait_for_socket_succeeds_when_socket_exists() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = PasstManager::new(dir.path());
        std::fs::write(&mgr.socket_path, "socket").unwrap();

        mgr.wait_for_socket().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn test_wait_for_socket_reports_early_exit_with_stderr_tail() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = PasstManager::new(dir.path());
        std::fs::write(
            dir.path().join("passt.stderr.log"),
            "line1\nline2\nline3\nline4\nline5\n",
        )
        .unwrap();
        mgr.child = Some(Command::new("sh").arg("-c").arg("exit 7").spawn().unwrap());

        let err = mgr.wait_for_socket().unwrap_err();
        let message = err.to_string();

        assert!(message.contains("passt exited early"));
        assert!(message.contains("line2; line3; line4; line5"));
        assert!(!mgr.is_running());
    }

    #[test]
    fn test_terminate_passt_removes_socket_and_pid_files() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("passt.sock");
        let pid_path = dir.path().join("passt.pid");
        let pcap_path = dir.path().join("passt.pcap");

        // A non-existent PID so the SIGTERM is a harmless no-op (ESRCH).
        std::fs::write(&socket_path, "fake").unwrap();
        std::fs::write(&pid_path, "2147483647").unwrap();
        std::fs::write(&pcap_path, "fake pcap").unwrap();

        terminate_passt(dir.path());

        assert!(!socket_path.exists());
        assert!(!pid_path.exists());
        assert!(!pcap_path.exists());
    }

    #[test]
    fn test_terminate_passt_removes_artifacts_with_invalid_pid_file() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("passt.sock");
        let pid_path = dir.path().join("passt.pid");
        let pcap_path = dir.path().join("passt.pcap");

        std::fs::write(&socket_path, "fake").unwrap();
        std::fs::write(&pid_path, "not a pid").unwrap();
        std::fs::write(&pcap_path, "fake pcap").unwrap();

        terminate_passt(dir.path());

        assert!(!socket_path.exists());
        assert!(!pid_path.exists());
        assert!(!pcap_path.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_pid_is_passt_rejects_non_passt_processes() {
        assert!(!pid_is_passt(std::process::id() as i32));
        assert!(!pid_is_passt(2_147_483_647));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn passt_process_name_accepts_the_packaged_simd_variant() {
        assert!(passt_process_name_is_known("passt"));
        assert!(passt_process_name_is_known("passt.avx2"));
        assert!(!passt_process_name_is_known("passt-helper"));
        assert!(!passt_process_name_is_known("passt.avx2.old"));
    }
}
