#![cfg(any(target_os = "macos", all(test, unix)))]

//! Pure-Rust userspace network proxy for libkrun on macOS.
//!
//! Uses a Unix datagram `socketpair()` to connect libkrun's virtio-net backend
//! to the userspace gateway and provides the gateway services needed by the guest:
//!
//! - **ARP**: handled automatically by smoltcp's interface layer.
//! - **DNS**: UDP/53 queries forwarded to the host's configured DNS servers.
//! - **Inbound TCP port-forwarding**: `host_port → guest_ip:guest_port` pairs
//!   parsed from the box's `port_map` config (e.g. `"8088:80"`).
//! - **Outbound TCP proxying**: guest connections addressed through the gateway
//!   are terminated by smoltcp and connected through the host TCP stack.

mod device;
mod manager;
#[cfg(test)]
mod tests;

use std::collections::{HashSet, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc::{self, Receiver, TryRecvError},
    Arc,
};
use std::time::Duration;

use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetFrame, EthernetProtocol, IpAddress, IpCidr, IpEndpoint, IpProtocol, Ipv4Address,
    Ipv4Packet, TcpPacket,
};

use device::{BridgePort, NetStats, UnixgramDevice, GATEWAY_MAC};
use manager::write_stats_file;

pub use manager::{spawn_inherited_netproxy, InheritedNetProxyConfig, NetProxyManager};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Returns the current time as a smoltcp `Instant` (microseconds since Unix epoch).
fn smoltcp_now() -> Instant {
    use std::time::{SystemTime, UNIX_EPOCH};
    let us = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64;
    Instant::from_micros(us)
}

/// Convert `std::net::Ipv4Addr` to `smoltcp::wire::Ipv4Address`.
fn to_smoltcp_ipv4(ip: Ipv4Addr) -> Ipv4Address {
    Ipv4Address::from(ip)
}

// ── Constants ─────────────────────────────────────────────────────────────────

/// Ephemeral port range start for outbound TCP connections from the gateway.
const EPHEMERAL_BASE: u16 = 49152;
/// Bound per-box memory and host resources consumed by transparent TCP flows.
const MAX_OUTBOUND_CONNECTIONS: usize = 256;
/// Do not let a host-side connect stall the guest indefinitely.
const OUTBOUND_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Idle TCP state is eventually reclaimed even if one endpoint disappears.
const TCP_IDLE_TIMEOUT: smoltcp::time::Duration = smoltcp::time::Duration::from_secs(300);
/// How often the proxy refreshes its stats file.
const STATS_WRITE_INTERVAL: Duration = Duration::from_secs(1);

// ── Port-forward state ────────────────────────────────────────────────────────

/// Parsed port-forward rule: `host_port → guest_ip:guest_port`.
struct PortForward {
    listener: TcpListener,
    guest_ip: Ipv4Addr,
    guest_port: u16,
    /// TCP handshake in progress from the gateway to the guest.
    pending: Vec<PendingGuestConnection>,
    /// Fully established connections ready for data proxying.
    active: Vec<TcpProxyConnection>,
}

struct PendingGuestConnection {
    handle: smoltcp::iface::SocketHandle,
    host_stream: TcpStream,
    started_at: std::time::Instant,
}

struct TcpProxyConnection {
    handle: smoltcp::iface::SocketHandle,
    host_stream: TcpStream,
    host_read_closed: bool,
    guest_read_closed: bool,
    /// An abort raised after the most recent interface poll must survive until
    /// the next poll so smoltcp can emit its reset packet.
    abort_pending: bool,
}

impl TcpProxyConnection {
    fn new(handle: smoltcp::iface::SocketHandle, host_stream: TcpStream) -> Self {
        Self {
            handle,
            host_stream,
            host_read_closed: false,
            guest_read_closed: false,
            abort_pending: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct OutboundFlow {
    guest_ip: Ipv4Addr,
    guest_port: u16,
    remote_ip: Ipv4Addr,
    remote_port: u16,
}

struct PendingOutboundConnection {
    flow: OutboundFlow,
    handle: smoltcp::iface::SocketHandle,
    connect_result: Receiver<io::Result<TcpStream>>,
    host_stream: Option<TcpStream>,
    started_at: std::time::Instant,
    failed: bool,
}

struct ActiveOutboundConnection {
    flow: OutboundFlow,
    proxy: TcpProxyConnection,
}

// ── Proxy engine ──────────────────────────────────────────────────────────────

struct ProxyEngineConfig {
    socket: UnixDatagram,
    guest_ip: Ipv4Addr,
    gateway_ip: Ipv4Addr,
    prefix_len: u8,
    dns_servers: Vec<Ipv4Addr>,
    port_forwards: Vec<PortForward>,
    shutdown: Arc<AtomicBool>,
    stats: Arc<NetStats>,
    stats_path: Option<PathBuf>,
    bridge: Option<BridgePort>,
}

struct ProxyEngine {
    device: UnixgramDevice,
    iface: Interface,
    sockets: SocketSet<'static>,
    dns_sockets: Vec<(smoltcp::iface::SocketHandle, Ipv4Addr)>,
    guest_ip: Ipv4Addr,
    gateway_ip: Ipv4Addr,
    port_forwards: Vec<PortForward>,
    pending_outbound: Vec<PendingOutboundConnection>,
    active_outbound: Vec<ActiveOutboundConnection>,
    outbound_connectors: Arc<AtomicUsize>,
    next_ephemeral: u16,
    shutdown: Arc<AtomicBool>,
    stats: Arc<NetStats>,
    stats_path: Option<PathBuf>,
    last_stats_write: std::time::Instant,
}

impl ProxyEngine {
    fn new(config: ProxyEngineConfig) -> Self {
        let ProxyEngineConfig {
            socket,
            guest_ip,
            gateway_ip,
            prefix_len,
            dns_servers,
            port_forwards,
            shutdown,
            stats,
            stats_path,
            bridge,
        } = config;

        let mut device = UnixgramDevice {
            socket,
            bridge,
            rx_queue: VecDeque::new(),
            stats: Arc::clone(&stats),
        };

        // Configure smoltcp interface as the gateway.
        let config = Config::new(GATEWAY_MAC.into());
        let mut iface = Interface::new(config, &mut device, smoltcp_now());
        iface.update_ip_addrs(|addrs| {
            let cidr = IpCidr::new(IpAddress::Ipv4(to_smoltcp_ipv4(gateway_ip)), prefix_len);
            addrs.push(cidr).ok();
        });
        // The guest keeps the real destination IP in its packets and uses the
        // gateway only as the Ethernet next hop. AnyIP plus a default route via
        // our own gateway address lets smoltcp terminate those transparent TCP
        // connections while preserving their original destination endpoints.
        iface.set_any_ip(true);
        let _ = iface
            .routes_mut()
            .add_default_ipv4_route(to_smoltcp_ipv4(gateway_ip));

        let mut sockets = SocketSet::new(vec![]);

        // The guest's resolv.conf contains the configured upstream addresses
        // (for example 8.8.8.8), not the gateway address. Bind one AnyIP UDP
        // socket per upstream so replies preserve the queried source IP; a
        // wildcard :53 socket would reply from gateway_ip and resolvers would
        // reject the mismatched response.
        let dns_sockets = dns_servers
            .iter()
            .copied()
            .filter_map(|server| {
                let dns_rx =
                    udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0u8; 8192]);
                let dns_tx =
                    udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0u8; 8192]);
                let mut dns_socket = udp::Socket::new(dns_rx, dns_tx);
                let endpoint = IpEndpoint::new(IpAddress::Ipv4(to_smoltcp_ipv4(server)), 53);
                dns_socket
                    .bind(endpoint)
                    .ok()
                    .map(|_| (sockets.add(dns_socket), server))
            })
            .collect();

        Self {
            device,
            iface,
            sockets,
            dns_sockets,
            guest_ip,
            gateway_ip,
            port_forwards,
            pending_outbound: Vec::new(),
            active_outbound: Vec::new(),
            outbound_connectors: Arc::new(AtomicUsize::new(0)),
            next_ephemeral: EPHEMERAL_BASE,
            shutdown,
            stats,
            stats_path,
            last_stats_write: std::time::Instant::now(),
        }
    }

    fn run(&mut self) {
        self.write_stats_snapshot();
        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }

            let now = smoltcp_now();

            // 1. Drain UnixGram socket into rx_queue.
            self.device.drain();

            // 2. Accept published-port clients and discover new guest outbound
            // TCP flows before smoltcp consumes their SYN packets.
            self.accept_connections();
            self.accept_outbound_flows();

            // 3. Collect non-blocking host connect results. Failed established
            // flows are aborted before the interface poll so the reset is sent.
            self.poll_outbound_connectors();

            // 4. Poll smoltcp (processes ARP, TCP, UDP frames).
            self.iface.poll(now, &mut self.device, &mut self.sockets);

            // Connections aborted after the previous poll have now had one
            // dispatch opportunity and can release their socket handles.
            self.finish_aborted_connections();

            // 5. Promote pending TCP connections to active once established.
            self.promote_established();
            self.promote_outbound_established();

            // 6. Proxy data for active TCP connections.
            self.proxy_data();

            // 7. Forward DNS queries to real DNS servers.
            self.forward_dns();

            // 8. Remove closed connections and release their smoltcp sockets.
            self.cleanup();

            // 9. Publish resource counters for `a3s-box stats`.
            self.maybe_write_stats_snapshot();

            // 10. Sleep until the next smoltcp event or at most 5 ms.
            let delay = self
                .iface
                .poll_delay(now, &self.sockets)
                .unwrap_or(smoltcp::time::Duration::from_millis(1));
            std::thread::sleep(Duration::from_micros(delay.micros().min(5_000)));
        }
        self.write_stats_snapshot();
    }

    fn maybe_write_stats_snapshot(&mut self) {
        if self.last_stats_write.elapsed() < STATS_WRITE_INTERVAL {
            return;
        }
        self.last_stats_write = std::time::Instant::now();
        self.write_stats_snapshot();
    }

    fn write_stats_snapshot(&self) {
        let Some(path) = self.stats_path.as_deref() else {
            return;
        };
        if let Err(e) = write_stats_file(path, self.stats.snapshot()) {
            tracing::debug!(error = %e, path = %path.display(), "NetProxy: failed to write stats file");
        }
    }

    // ── Accept new host connections ───────────────────────────────────────────

    fn accept_connections(&mut self) {
        // First pass: accept connections, collect (forward_index, stream, guest_ip, guest_port).
        // We can't call open_guest_tcp while mutably borrowing port_forwards.
        let mut new_conns: Vec<(usize, TcpStream, Ipv4Addr, u16)> = Vec::new();
        for (i, pf) in self.port_forwards.iter_mut().enumerate() {
            loop {
                match pf.listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(true).ok();
                        stream.set_nodelay(true).ok();
                        new_conns.push((i, stream, pf.guest_ip, pf.guest_port));
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        tracing::warn!(error = %e, "Port-forward listener error");
                        break;
                    }
                }
            }
        }

        // Second pass: open smoltcp TCP sockets and push to pending.
        for (i, stream, guest_ip, guest_port) in new_conns {
            let handle = self.open_guest_tcp(guest_ip, guest_port);
            self.port_forwards[i].pending.push(PendingGuestConnection {
                handle,
                host_stream: stream,
                started_at: std::time::Instant::now(),
            });
            tracing::debug!(
                guest = %guest_ip,
                port = guest_port,
                handle = ?handle,
                "NetProxy accepted host connection and initiated guest TCP connect"
            );
        }
    }

    /// Create a smoltcp TCP socket and initiate a connection to the guest.
    fn open_guest_tcp(
        &mut self,
        guest_ip: Ipv4Addr,
        guest_port: u16,
    ) -> smoltcp::iface::SocketHandle {
        let rx = tcp::SocketBuffer::new(vec![0u8; 65536]);
        let tx = tcp::SocketBuffer::new(vec![0u8; 65536]);
        let mut socket = tcp::Socket::new(rx, tx);

        let local_port = self.next_ephemeral;
        self.next_ephemeral = self.next_ephemeral.wrapping_add(1);
        if self.next_ephemeral < EPHEMERAL_BASE {
            self.next_ephemeral = EPHEMERAL_BASE;
        }

        let remote = IpEndpoint::new(IpAddress::Ipv4(to_smoltcp_ipv4(guest_ip)), guest_port);
        socket
            .connect(self.iface.context(), remote, local_port)
            .ok();
        socket.set_keep_alive(Some(smoltcp::time::Duration::from_secs(30)));
        socket.set_timeout(Some(TCP_IDLE_TIMEOUT));

        self.sockets.add(socket)
    }

    // ── Discover guest outbound TCP connections ──────────────────────────────

    fn accept_outbound_flows(&mut self) {
        let queued_flows: HashSet<_> = self
            .device
            .rx_queue
            .iter()
            .filter_map(|frame| outbound_syn_flow(frame, self.guest_ip, self.gateway_ip))
            .collect();

        for flow in queued_flows {
            if self.outbound_flow_exists(flow) {
                continue;
            }
            if self.pending_outbound.len() + self.active_outbound.len() >= MAX_OUTBOUND_CONNECTIONS
                || self.outbound_connectors.load(Ordering::Relaxed) >= MAX_OUTBOUND_CONNECTIONS
            {
                tracing::warn!(
                    limit = MAX_OUTBOUND_CONNECTIONS,
                    "NetProxy outbound connection limit reached"
                );
                continue;
            }

            let connect_result = match spawn_outbound_connect(
                flow,
                Arc::clone(&self.outbound_connectors),
            ) {
                Ok(receiver) => receiver,
                Err(error) => {
                    tracing::warn!(%error, ?flow, "NetProxy failed to spawn outbound connector");
                    continue;
                }
            };

            let rx = tcp::SocketBuffer::new(vec![0u8; 65536]);
            let tx = tcp::SocketBuffer::new(vec![0u8; 65536]);
            let mut socket = tcp::Socket::new(rx, tx);
            let endpoint = IpEndpoint::new(
                IpAddress::Ipv4(to_smoltcp_ipv4(flow.remote_ip)),
                flow.remote_port,
            );
            if let Err(error) = socket.listen(endpoint) {
                tracing::warn!(?error, ?flow, "NetProxy failed to listen for outbound flow");
                continue;
            }
            socket.set_keep_alive(Some(smoltcp::time::Duration::from_secs(30)));
            socket.set_timeout(Some(TCP_IDLE_TIMEOUT));
            let handle = self.sockets.add(socket);

            self.pending_outbound.push(PendingOutboundConnection {
                flow,
                handle,
                connect_result,
                host_stream: None,
                started_at: std::time::Instant::now(),
                failed: false,
            });
            tracing::debug!(
                ?flow,
                ?handle,
                "NetProxy discovered guest outbound TCP flow"
            );
        }
    }

    fn outbound_flow_exists(&self, flow: OutboundFlow) -> bool {
        self.pending_outbound
            .iter()
            .any(|pending| pending.flow == flow)
            || self
                .active_outbound
                .iter()
                .any(|active| active.flow == flow)
    }

    /// Collect host connect results without blocking the netproxy packet loop.
    /// This runs before `iface.poll`, so an aborted smoltcp socket gets a chance
    /// to emit its reset before cleanup removes it.
    fn poll_outbound_connectors(&mut self) {
        for pending in &mut self.pending_outbound {
            if pending.failed || pending.host_stream.is_some() {
                continue;
            }

            match pending.connect_result.try_recv() {
                Ok(Ok(stream)) => {
                    pending.host_stream = Some(stream);
                    tracing::debug!(flow = ?pending.flow, "NetProxy host TCP connection established");
                }
                Ok(Err(error)) => {
                    tracing::debug!(%error, flow = ?pending.flow, "NetProxy host TCP connection failed");
                    self.sockets.get_mut::<tcp::Socket>(pending.handle).abort();
                    pending.failed = true;
                }
                Err(TryRecvError::Empty) => {
                    if pending.started_at.elapsed()
                        > OUTBOUND_CONNECT_TIMEOUT + Duration::from_secs(1)
                    {
                        tracing::debug!(flow = ?pending.flow, "NetProxy host TCP connection timed out");
                        self.sockets.get_mut::<tcp::Socket>(pending.handle).abort();
                        pending.failed = true;
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    self.sockets.get_mut::<tcp::Socket>(pending.handle).abort();
                    pending.failed = true;
                }
            }
        }
    }

    // ── Promote pending → active ──────────────────────────────────────────────

    fn promote_established(&mut self) {
        for pf in &mut self.port_forwards {
            let mut still_pending = Vec::new();
            let mut to_remove = Vec::new();
            for pending in pf.pending.drain(..) {
                let socket = self.sockets.get::<tcp::Socket>(pending.handle);
                use smoltcp::socket::tcp::State;
                match socket.state() {
                    State::Established => {
                        tracing::debug!(handle = ?pending.handle, "NetProxy guest TCP connection established");
                        pf.active
                            .push(TcpProxyConnection::new(pending.handle, pending.host_stream));
                    }
                    State::Closed | State::TimeWait => {
                        tracing::debug!(handle = ?pending.handle, state = ?socket.state(), "NetProxy guest TCP connection closed before establishment");
                        to_remove.push(pending.handle);
                    }
                    _ if pending.started_at.elapsed() > OUTBOUND_CONNECT_TIMEOUT => {
                        tracing::debug!(handle = ?pending.handle, "NetProxy guest TCP connection timed out");
                        to_remove.push(pending.handle);
                    }
                    _ => {
                        still_pending.push(pending);
                    }
                }
            }
            pf.pending = still_pending;
            for handle in to_remove {
                self.sockets.remove(handle);
            }
        }
    }

    fn promote_outbound_established(&mut self) {
        use smoltcp::socket::tcp::State;

        let mut still_pending = Vec::new();
        let mut to_remove = Vec::new();
        for mut pending in self.pending_outbound.drain(..) {
            let state = self.sockets.get::<tcp::Socket>(pending.handle).state();
            if pending.failed || matches!(state, State::Closed | State::TimeWait) {
                to_remove.push(pending.handle);
                continue;
            }

            if matches!(state, State::Established | State::CloseWait) {
                if let Some(stream) = pending.host_stream.take() {
                    tracing::debug!(flow = ?pending.flow, handle = ?pending.handle, "NetProxy outbound TCP proxy active");
                    self.active_outbound.push(ActiveOutboundConnection {
                        flow: pending.flow,
                        proxy: TcpProxyConnection::new(pending.handle, stream),
                    });
                    continue;
                }
            }

            still_pending.push(pending);
        }
        self.pending_outbound = still_pending;
        for handle in to_remove {
            self.sockets.remove(handle);
        }
    }

    fn finish_aborted_connections(&mut self) {
        for pf in &mut self.port_forwards {
            let mut to_remove = Vec::new();
            pf.active.retain(|connection| {
                if connection.abort_pending {
                    to_remove.push(connection.handle);
                    false
                } else {
                    true
                }
            });
            for handle in to_remove {
                self.sockets.remove(handle);
            }
        }

        let mut to_remove = Vec::new();
        self.active_outbound.retain(|connection| {
            if connection.proxy.abort_pending {
                to_remove.push(connection.proxy.handle);
                false
            } else {
                true
            }
        });
        for handle in to_remove {
            self.sockets.remove(handle);
        }
    }

    // ── Bidirectional data proxy ──────────────────────────────────────────────

    fn proxy_data(&mut self) {
        for pf in &mut self.port_forwards {
            for connection in &mut pf.active {
                proxy_tcp_connection(&mut self.sockets, connection);
            }
        }
        for connection in &mut self.active_outbound {
            proxy_tcp_connection(&mut self.sockets, &mut connection.proxy);
        }
    }

    // ── DNS forwarding ────────────────────────────────────────────────────────

    fn forward_dns(&mut self) {
        let next_query = self.dns_sockets.iter().find_map(|(handle, server)| {
            let socket = self.sockets.get_mut::<udp::Socket>(*handle);
            if !socket.can_recv() {
                return None;
            }
            let (query, source) = socket.recv().ok()?;
            Some((*handle, *server, query.to_vec(), source))
        });
        let Some((handle, dns_server, query, source)) = next_query else {
            return;
        };

        // Forward query to the real DNS server via a host UDP socket.
        match UdpSocket::bind("0.0.0.0:0") {
            Ok(udp) => {
                udp.set_read_timeout(Some(Duration::from_secs(2))).ok();
                let dest = SocketAddrV4::new(dns_server, 53);
                if udp.send_to(&query, dest).is_ok() {
                    let mut resp = vec![0u8; 4096];
                    if let Ok((n, _)) = udp.recv_from(&mut resp) {
                        let socket = self.sockets.get_mut::<udp::Socket>(handle);
                        socket.send_slice(&resp[..n], source).ok();
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "DNS forward: failed to bind host UDP socket");
            }
        }
    }

    // ── Cleanup ───────────────────────────────────────────────────────────────

    fn cleanup(&mut self) {
        use smoltcp::socket::tcp::State;
        for pf in &mut self.port_forwards {
            // Collect handles that need to be removed first, then remove outside retain.
            let mut to_remove = Vec::new();
            pf.active.retain(|connection| {
                let state = self.sockets.get::<tcp::Socket>(connection.handle).state();
                if matches!(state, State::Closed | State::TimeWait) && !connection.abort_pending {
                    to_remove.push(connection.handle);
                    false
                } else {
                    true
                }
            });
            for handle in to_remove {
                self.sockets.remove(handle);
            }
        }

        let mut to_remove = Vec::new();
        self.active_outbound.retain(|connection| {
            let state = self
                .sockets
                .get::<tcp::Socket>(connection.proxy.handle)
                .state();
            if matches!(state, State::Closed | State::TimeWait) && !connection.proxy.abort_pending {
                to_remove.push(connection.proxy.handle);
                false
            } else {
                true
            }
        });
        for handle in to_remove {
            self.sockets.remove(handle);
        }
    }
}

/// Extract the original four-tuple from a guest TCP SYN routed through the
/// gateway. Peer-to-peer bridge frames use the peer MAC and are deliberately
/// excluded so the local Ethernet switch keeps owning those connections.
fn outbound_syn_flow(
    frame: &[u8],
    expected_guest_ip: Ipv4Addr,
    gateway_ip: Ipv4Addr,
) -> Option<OutboundFlow> {
    let ethernet = EthernetFrame::new_checked(frame).ok()?;
    if ethernet.dst_addr() != GATEWAY_MAC || ethernet.ethertype() != EthernetProtocol::Ipv4 {
        return None;
    }

    let ipv4 = Ipv4Packet::new_checked(ethernet.payload()).ok()?;
    if ipv4.next_header() != IpProtocol::Tcp {
        return None;
    }
    let guest_ip = Ipv4Addr::from(ipv4.src_addr().0);
    let remote_ip = Ipv4Addr::from(ipv4.dst_addr().0);
    if guest_ip != expected_guest_ip
        || remote_ip == gateway_ip
        || remote_ip.is_unspecified()
        || remote_ip.is_multicast()
        || remote_ip == Ipv4Addr::BROADCAST
    {
        return None;
    }

    let tcp = TcpPacket::new_checked(ipv4.payload()).ok()?;
    if !tcp.syn() || tcp.ack() || tcp.src_port() == 0 || tcp.dst_port() == 0 {
        return None;
    }

    Some(OutboundFlow {
        guest_ip,
        guest_port: tcp.src_port(),
        remote_ip,
        remote_port: tcp.dst_port(),
    })
}

fn spawn_outbound_connect(
    flow: OutboundFlow,
    connector_count: Arc<AtomicUsize>,
) -> io::Result<Receiver<io::Result<TcpStream>>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    connector_count.fetch_add(1, Ordering::Relaxed);
    let thread_count = Arc::clone(&connector_count);
    let spawn = std::thread::Builder::new()
        .name("a3s-netproxy-connect".to_string())
        .spawn(move || {
            let address = SocketAddr::V4(SocketAddrV4::new(flow.remote_ip, flow.remote_port));
            let result =
                TcpStream::connect_timeout(&address, OUTBOUND_CONNECT_TIMEOUT).and_then(|stream| {
                    stream.set_nonblocking(true)?;
                    let _ = stream.set_nodelay(true);
                    Ok(stream)
                });
            let _ = sender.send(result);
            thread_count.fetch_sub(1, Ordering::Relaxed);
        });

    match spawn {
        Ok(_) => Ok(receiver),
        Err(error) => {
            connector_count.fetch_sub(1, Ordering::Relaxed);
            Err(error)
        }
    }
}

/// Move as much data as each non-blocking endpoint can currently accept.
/// Consuming only the byte count returned by `write` and reading directly into
/// smoltcp's available transmit slice prevents partial writes from dropping
/// bytes under backpressure.
fn proxy_tcp_connection(sockets: &mut SocketSet<'static>, connection: &mut TcpProxyConnection) {
    let handle = connection.handle;
    let socket = sockets.get_mut::<tcp::Socket>(handle);

    let mut guest_to_host_bytes = 0usize;
    let mut host_write_error = None;
    if socket.can_recv() {
        let _ = socket.recv(|data| match connection.host_stream.write(data) {
            Ok(0) if !data.is_empty() => {
                host_write_error = Some(io::Error::from(io::ErrorKind::WriteZero));
                (0, ())
            }
            Ok(written) => {
                guest_to_host_bytes = written;
                (written, ())
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                (0, ())
            }
            Err(error) => {
                host_write_error = Some(error);
                (0, ())
            }
        });
    }
    if guest_to_host_bytes > 0 {
        tracing::trace!(
            ?handle,
            bytes = guest_to_host_bytes,
            "NetProxy forwarded guest -> host bytes"
        );
    }
    if let Some(error) = host_write_error {
        tracing::debug!(%error, ?handle, "NetProxy host write failed");
        let _ = connection.host_stream.shutdown(Shutdown::Both);
        socket.abort();
        connection.abort_pending = true;
        return;
    }

    if !connection.guest_read_closed && !socket.may_recv() {
        let _ = connection.host_stream.shutdown(Shutdown::Write);
        connection.guest_read_closed = true;
    }

    let mut host_to_guest_bytes = 0usize;
    let mut host_eof = false;
    let mut host_read_error = None;
    if !connection.host_read_closed && socket.can_send() {
        let _ = socket.send(|buffer| match connection.host_stream.read(buffer) {
            Ok(0) => {
                host_eof = true;
                (0, ())
            }
            Ok(read) => {
                host_to_guest_bytes = read;
                (read, ())
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                (0, ())
            }
            Err(error) => {
                host_read_error = Some(error);
                (0, ())
            }
        });
    }
    if host_to_guest_bytes > 0 {
        tracing::trace!(
            ?handle,
            bytes = host_to_guest_bytes,
            "NetProxy forwarded host -> guest bytes"
        );
    }
    if let Some(error) = host_read_error {
        tracing::debug!(%error, ?handle, "NetProxy host read failed");
        let _ = connection.host_stream.shutdown(Shutdown::Both);
        socket.abort();
        connection.abort_pending = true;
    } else if host_eof {
        tracing::debug!(?handle, "NetProxy host side closed its write half");
        connection.host_read_closed = true;
        socket.close();
    }
}
