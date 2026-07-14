use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::os::fd::{FromRawFd, IntoRawFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::{atomic::AtomicBool, Arc};

use a3s_box_core::error::{BoxError, Result};

use super::device::{BridgePort, NetStats, NetStatsSnapshot};
use super::{PortForward, ProxyEngine, ProxyEngineConfig};

// ── NetProxyManager lifecycle ─────────────────────────────────────────────────

/// Manages the lifecycle of the pure-Rust vfkit network proxy thread.
///
/// Drop calls `stop()` automatically.
pub struct NetProxyManager {
    socket_path: PathBuf,
    stats_path: PathBuf,
    net_socket_fd: Option<RawFd>,
    net_proxy_fd: Option<RawFd>,
}

impl NetProxyManager {
    /// Create a new manager. Socket will be placed at
    /// `~/.a3s/boxes/<box_id>/sockets/net.sock`.
    pub fn new(box_dir: &Path) -> Self {
        let socket_dir = box_dir.join("sockets");
        Self {
            socket_path: socket_dir.join("net.sock"),
            stats_path: socket_dir.join("net.stats.json"),
            net_socket_fd: None,
            net_proxy_fd: None,
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn stats_path(&self) -> &Path {
        &self.stats_path
    }

    pub fn net_socket_fd(&self) -> Option<RawFd> {
        self.net_socket_fd
    }

    pub fn net_proxy_fd(&self) -> Option<RawFd> {
        self.net_proxy_fd
    }

    /// Create socketpair for NetProxy.
    ///
    /// Unlike the name suggests, this does NOT spawn a thread. Thread spawning
    /// happens in `spawn_inherited_netproxy()` called from the shim.
    pub fn spawn(
        &mut self,
        _ip: Ipv4Addr,
        _gateway: Ipv4Addr,
        _prefix_len: u8,
        _dns_servers: &[Ipv4Addr],
        _port_map: &[String],
    ) -> Result<()> {
        let (proxy_socket, krun_fd) = socketpair_unixgram()?;
        self.net_socket_fd = Some(krun_fd);
        self.net_proxy_fd = Some(proxy_socket.into_raw_fd());
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(fd) = self.net_socket_fd.take() {
            unsafe {
                libc::close(fd);
            }
        }
        if let Some(fd) = self.net_proxy_fd.take() {
            unsafe {
                libc::close(fd);
            }
        }
        std::fs::remove_file(&self.socket_path).ok();
        std::fs::remove_file(&self.stats_path).ok();
    }

    pub fn is_running(&mut self) -> bool {
        self.net_socket_fd.is_some() || self.net_proxy_fd.is_some()
    }
}

impl Drop for NetProxyManager {
    fn drop(&mut self) {
        self.stop();
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub struct InheritedNetProxyConfig<'a> {
    pub guest_ip: Ipv4Addr,
    pub gateway: Ipv4Addr,
    pub prefix_len: u8,
    pub dns_servers: &'a [Ipv4Addr],
    pub port_map: &'a [String],
    pub stats_path: Option<PathBuf>,
    pub bridge_socket_dir: Option<PathBuf>,
    pub own_mac: [u8; 6],
}

pub fn spawn_inherited_netproxy(fd: RawFd, config: InheritedNetProxyConfig<'_>) -> Result<()> {
    let InheritedNetProxyConfig {
        guest_ip,
        gateway,
        prefix_len,
        dns_servers,
        port_map,
        stats_path,
        bridge_socket_dir,
        own_mac,
    } = config;
    let socket = unsafe { UnixDatagram::from_raw_fd(fd) };
    let port_forwards = parse_port_forwards(port_map, guest_ip)
        .map_err(|e| BoxError::NetworkError(format!("invalid port_map: {e}")))?;
    let dns_servers = dns_servers.to_vec();
    let shutdown = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(NetStats::default());
    let bridge = bridge_socket_dir
        .as_deref()
        .map(|directory| BridgePort::bind(directory, own_mac))
        .transpose()
        .map_err(|error| {
            BoxError::NetworkError(format!("failed to join bridge Ethernet switch: {error}"))
        })?;

    std::thread::Builder::new()
        .name("a3s-netproxy".to_string())
        .spawn(move || {
            tracing::info!(fd, gateway = %gateway, guest_ip = %guest_ip, stats = ?stats_path, "NetProxy thread started");
            if let Err(e) = socket.set_nonblocking(true) {
                tracing::error!(error = %e, "NetProxy: set_nonblocking failed");
                return;
            }

            let mut engine = ProxyEngine::new(ProxyEngineConfig {
                socket,
                guest_ip,
                gateway_ip: gateway,
                prefix_len,
                dns_servers,
                port_forwards,
                shutdown,
                stats,
                stats_path,
                bridge,
            });
            engine.run();
            tracing::info!("NetProxy thread exiting");
        })
        .map_err(|e| BoxError::NetworkError(format!("failed to spawn netproxy thread: {e}")))?;

    Ok(())
}

fn socketpair_unixgram() -> Result<(UnixDatagram, RawFd)> {
    let mut fds = [-1; 2];
    let ret = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) };
    if ret != 0 {
        return Err(BoxError::NetworkError(format!(
            "failed to create unix datagram socketpair: {}",
            io::Error::last_os_error()
        )));
    }

    let proxy_socket = unsafe { UnixDatagram::from_raw_fd(fds[0]) };
    Ok((proxy_socket, fds[1]))
}

pub(super) fn write_stats_file(path: &Path, stats: NetStatsSnapshot) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let updated_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let body = format!(
        "{{\"schema\":\"a3s-box.netproxy.stats.v1\",\"rx_bytes\":{},\"tx_bytes\":{},\"rx_packets\":{},\"tx_packets\":{},\"updated_at_ms\":{}}}\n",
        stats.rx_bytes, stats.tx_bytes, stats.rx_packets, stats.tx_packets, updated_at_ms
    );
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(tmp, path)
}

/// Parse `["8088:80", "443:443"]` into `Vec<PortForward>`.
///
/// Each rule maps `host_port → guest_ip:guest_port`. Guest IP is always the
/// IPAM-assigned `guest_ip`.
pub(super) fn parse_port_forwards(
    port_map: &[String],
    guest_ip: Ipv4Addr,
) -> std::result::Result<Vec<PortForward>, String> {
    let mut forwards = Vec::new();
    for entry in port_map {
        let mapping = a3s_box_core::parse_port_mapping(entry)?;
        let host_port = mapping.host_port;
        let guest_port = mapping.guest_port;

        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, host_port))
            .map_err(|e| format!("cannot bind 0.0.0.0:{host_port}: {e}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("set_nonblocking on listener: {e}"))?;

        tracing::info!(
            host_port,
            guest_port,
            guest_ip = %guest_ip,
            "Port-forward listener ready"
        );
        forwards.push(PortForward {
            listener,
            guest_ip,
            guest_port,
            pending: Vec::new(),
            active: Vec::new(),
        });
    }
    Ok(forwards)
}
