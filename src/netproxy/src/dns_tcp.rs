//! NetworkStore DNS-over-TCP owner for Linux passt_bridge (Axis C P1).
//!
//! Terminates guest TCP/53 to configured DNS servers with smoltcp (same honesty
//! bar as macOS netproxy `#577`): known NetworkStore names are answered locally;
//! unknown names connect via host `TcpStream` with bytes already read prefetched.
//! Diverted flows are never forwarded to passt mid-stream.

use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpStream};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc::{self, Receiver, TryRecvError},
    Arc,
};
use std::time::Duration;

use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::socket::tcp;
use smoltcp::wire::{
    EthernetFrame, EthernetProtocol, IpAddress, IpCidr, IpEndpoint, IpProtocol, Ipv4Packet,
    TcpPacket,
};

use crate::device::{FrameDevice, GATEWAY_MAC};
use crate::dns_local::{self, NetworkDnsConfig};
use crate::{smoltcp_now, to_smoltcp_ipv4};

const OUTBOUND_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TCP_IDLE_TIMEOUT: smoltcp::time::Duration = smoltcp::time::Duration::from_secs(300);
const MAX_OUTBOUND_CONNECTIONS: usize = 64;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct OutboundFlow {
    guest_ip: Ipv4Addr,
    guest_port: u16,
    remote_ip: Ipv4Addr,
    remote_port: u16,
}

struct DnsTcpSession {
    flow: OutboundFlow,
    handle: smoltcp::iface::SocketHandle,
    buffer: Vec<u8>,
    reply: Vec<u8>,
    started_at: std::time::Instant,
    abort_pending: bool,
}

struct PendingUpstream {
    flow: OutboundFlow,
    handle: smoltcp::iface::SocketHandle,
    connect_result: Receiver<io::Result<TcpStream>>,
    host_stream: Option<TcpStream>,
    started_at: std::time::Instant,
    failed: bool,
    prefetch: Vec<u8>,
}

struct ActiveUpstream {
    flow: OutboundFlow,
    handle: smoltcp::iface::SocketHandle,
    host_stream: TcpStream,
    prefetch: Vec<u8>,
    guest_read_closed: bool,
    host_read_closed: bool,
    abort_pending: bool,
}

/// smoltcp TCP owner for NetworkStore DNS-over-TCP on the passt_bridge L2 path.
pub(crate) struct DnsTcpOwner {
    device: FrameDevice,
    iface: Interface,
    sockets: SocketSet<'static>,
    guest_ip: Ipv4Addr,
    gateway_ip: Ipv4Addr,
    config: NetworkDnsConfig,
    dns_tcp: Vec<DnsTcpSession>,
    pending: Vec<PendingUpstream>,
    active: Vec<ActiveUpstream>,
    outbound_connectors: Arc<AtomicUsize>,
    _shutdown: Arc<AtomicBool>,
}

impl DnsTcpOwner {
    pub(crate) fn new(config: NetworkDnsConfig) -> Self {
        let guest_ip = config.guest_ip;
        let gateway_ip = config.gateway_ip;
        let prefix_len = config.prefix_len;
        let mut device = FrameDevice::new();
        let mut iface = Interface::new(Config::new(GATEWAY_MAC.into()), &mut device, smoltcp_now());
        iface.set_any_ip(true);
        iface.update_ip_addrs(|addrs| {
            let cidr = IpCidr::new(IpAddress::Ipv4(to_smoltcp_ipv4(gateway_ip)), prefix_len);
            addrs.push(cidr).unwrap();
        });
        iface
            .routes_mut()
            .add_default_ipv4_route(to_smoltcp_ipv4(gateway_ip))
            .unwrap();

        Self {
            device,
            iface,
            sockets: SocketSet::new(vec![]),
            guest_ip,
            gateway_ip,
            config,
            dns_tcp: Vec::new(),
            pending: Vec::new(),
            active: Vec::new(),
            outbound_connectors: Arc::new(AtomicUsize::new(0)),
            _shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn should_divert(&self, frame: &[u8]) -> bool {
        if let Some(flow) = outbound_syn_flow(frame, self.guest_ip, self.gateway_ip) {
            if self.is_holdable_dns(&flow) {
                return true;
            }
        }
        if let Some(flow) = ethernet_tcp_flow(frame, self.guest_ip) {
            return self.owns_flow(flow);
        }
        false
    }

    pub(crate) fn push_guest_frame(&mut self, frame: Vec<u8>) {
        self.device.rx_queue.push_back(frame);
    }

    pub(crate) fn poll_and_drain_tx(&mut self) -> Vec<Vec<u8>> {
        self.accept_dns_syns();
        self.poll_upstream_connectors();
        let now = smoltcp_now();
        self.iface.poll(now, &mut self.device, &mut self.sockets);
        self.serve_local_answers();
        self.promote_upstream();
        self.proxy_upstream();
        self.cleanup();
        self.device.drain_tx()
    }

    fn is_holdable_dns(&self, flow: &OutboundFlow) -> bool {
        flow.remote_port == 53 && self.config.dns_servers.contains(&flow.remote_ip)
    }

    fn owns_flow(&self, flow: OutboundFlow) -> bool {
        self.dns_tcp.iter().any(|s| s.flow == flow)
            || self.pending.iter().any(|p| p.flow == flow)
            || self.active.iter().any(|a| a.flow == flow)
    }

    fn accept_dns_syns(&mut self) {
        let queued: HashSet<_> = self
            .device
            .rx_queue
            .iter()
            .filter_map(|frame| outbound_syn_flow(frame, self.guest_ip, self.gateway_ip))
            .filter(|flow| self.is_holdable_dns(flow))
            .collect();

        for flow in queued {
            if self.owns_flow(flow) {
                continue;
            }
            if self.dns_tcp.len() + self.pending.len() + self.active.len()
                >= MAX_OUTBOUND_CONNECTIONS
            {
                tracing::warn!(
                    limit = MAX_OUTBOUND_CONNECTIONS,
                    "DnsTcpOwner connection limit reached"
                );
                continue;
            }
            let rx = tcp::SocketBuffer::new(vec![0u8; 65536]);
            let tx = tcp::SocketBuffer::new(vec![0u8; 65536]);
            let mut socket = tcp::Socket::new(rx, tx);
            let endpoint = IpEndpoint::new(
                IpAddress::Ipv4(to_smoltcp_ipv4(flow.remote_ip)),
                flow.remote_port,
            );
            if let Err(error) = socket.listen(endpoint) {
                tracing::warn!(?error, ?flow, "DnsTcpOwner failed to listen");
                continue;
            }
            socket.set_keep_alive(Some(smoltcp::time::Duration::from_secs(30)));
            socket.set_timeout(Some(TCP_IDLE_TIMEOUT));
            let handle = self.sockets.add(socket);
            self.dns_tcp.push(DnsTcpSession {
                flow,
                handle,
                buffer: Vec::new(),
                reply: Vec::new(),
                started_at: std::time::Instant::now(),
                abort_pending: false,
            });
            tracing::debug!(?flow, "DnsTcpOwner holding TCP/53 for NetworkStore answer");
        }
    }

    fn serve_local_answers(&mut self) {
        use smoltcp::socket::tcp::State;

        let networks_json = self.config.networks_json.clone();
        let network_name = self.config.network_name.clone();
        let mut still = Vec::new();
        let mut fallbacks = Vec::new();
        for mut session in self.dns_tcp.drain(..) {
            if session.abort_pending {
                still.push(session);
                continue;
            }
            let state = self.sockets.get::<tcp::Socket>(session.handle).state();
            if matches!(state, State::Closed | State::TimeWait) {
                self.sockets.remove(session.handle);
                continue;
            }

            if session.reply.is_empty() {
                {
                    let socket = self.sockets.get_mut::<tcp::Socket>(session.handle);
                    if socket.can_recv() {
                        let _ = socket.recv(|data| {
                            session.buffer.extend_from_slice(data);
                            (data.len(), ())
                        });
                    }
                }
                if session.buffer.len() > 2 + 65535 {
                    self.sockets.get_mut::<tcp::Socket>(session.handle).abort();
                    session.abort_pending = true;
                    still.push(session);
                    continue;
                }
                if let Some((query, consumed)) = dns_local::split_dns_tcp_message(&session.buffer) {
                    let query = query.to_vec();
                    let response =
                        dns_local::try_network_a_response(&query, &networks_json, &network_name);
                    if let Some(response) = response.filter(|response| response.len() <= 65535) {
                        let mut reply = (response.len() as u16).to_be_bytes().to_vec();
                        reply.extend_from_slice(&response);
                        session.reply = reply;
                        session.buffer.drain(..consumed);
                    } else {
                        fallbacks.push(session);
                        continue;
                    }
                } else if session.started_at.elapsed() > OUTBOUND_CONNECT_TIMEOUT {
                    self.sockets.get_mut::<tcp::Socket>(session.handle).abort();
                    session.abort_pending = true;
                    still.push(session);
                    continue;
                } else {
                    still.push(session);
                    continue;
                }
            }

            {
                let socket = self.sockets.get_mut::<tcp::Socket>(session.handle);
                if socket.can_send() && !session.reply.is_empty() {
                    if let Ok(sent) = socket.send_slice(&session.reply) {
                        session.reply.drain(..sent);
                    }
                }
                if session.reply.is_empty() {
                    socket.close();
                }
            }
            still.push(session);
        }
        self.dns_tcp = still;
        for session in fallbacks {
            self.fallback_upstream(session);
        }
    }

    fn fallback_upstream(&mut self, session: DnsTcpSession) {
        let connect_result = match spawn_outbound_connect(
            session.flow,
            Arc::clone(&self.outbound_connectors),
        ) {
            Ok(receiver) => receiver,
            Err(error) => {
                tracing::warn!(%error, flow = ?session.flow, "DnsTcpOwner failed to spawn TCP/53 upstream");
                self.sockets.get_mut::<tcp::Socket>(session.handle).abort();
                self.dns_tcp.push(DnsTcpSession {
                    abort_pending: true,
                    ..session
                });
                return;
            }
        };
        self.pending.push(PendingUpstream {
            flow: session.flow,
            handle: session.handle,
            connect_result,
            host_stream: None,
            started_at: session.started_at,
            failed: false,
            prefetch: session.buffer,
        });
    }

    fn poll_upstream_connectors(&mut self) {
        for pending in &mut self.pending {
            if pending.failed || pending.host_stream.is_some() {
                continue;
            }
            match pending.connect_result.try_recv() {
                Ok(Ok(stream)) => pending.host_stream = Some(stream),
                Ok(Err(error)) => {
                    tracing::debug!(%error, flow = ?pending.flow, "DnsTcpOwner upstream connect failed");
                    pending.failed = true;
                }
                Err(TryRecvError::Empty)
                    if pending.started_at.elapsed() > OUTBOUND_CONNECT_TIMEOUT =>
                {
                    pending.failed = true;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => pending.failed = true,
            }
        }
    }

    fn promote_upstream(&mut self) {
        use smoltcp::socket::tcp::State;

        let mut still = Vec::new();
        for mut pending in self.pending.drain(..) {
            if pending.failed {
                self.sockets.get_mut::<tcp::Socket>(pending.handle).abort();
                continue;
            }
            let state = self.sockets.get::<tcp::Socket>(pending.handle).state();
            if matches!(state, State::Closed | State::TimeWait) {
                self.sockets.remove(pending.handle);
                continue;
            }
            if let Some(host_stream) = pending.host_stream.take() {
                if matches!(state, State::Established) {
                    self.active.push(ActiveUpstream {
                        flow: pending.flow,
                        handle: pending.handle,
                        host_stream,
                        prefetch: pending.prefetch,
                        guest_read_closed: false,
                        host_read_closed: false,
                        abort_pending: false,
                    });
                    continue;
                }
                pending.host_stream = Some(host_stream);
            }
            still.push(pending);
        }
        self.pending = still;
    }

    fn proxy_upstream(&mut self) {
        for connection in &mut self.active {
            proxy_one(
                &mut self.sockets,
                connection.handle,
                &mut connection.host_stream,
                &mut connection.prefetch,
                &mut connection.guest_read_closed,
                &mut connection.host_read_closed,
                &mut connection.abort_pending,
            );
        }
    }

    fn cleanup(&mut self) {
        use smoltcp::socket::tcp::State;
        let mut to_remove = Vec::new();
        self.active.retain(|connection| {
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
}

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

fn ethernet_tcp_flow(frame: &[u8], expected_guest_ip: Ipv4Addr) -> Option<OutboundFlow> {
    let ethernet = EthernetFrame::new_checked(frame).ok()?;
    if ethernet.ethertype() != EthernetProtocol::Ipv4 {
        return None;
    }
    let ipv4 = Ipv4Packet::new_checked(ethernet.payload()).ok()?;
    if ipv4.next_header() != IpProtocol::Tcp {
        return None;
    }
    let src = Ipv4Addr::from(ipv4.src_addr().0);
    let dst = Ipv4Addr::from(ipv4.dst_addr().0);
    let tcp = TcpPacket::new_checked(ipv4.payload()).ok()?;
    if tcp.src_port() == 0 || tcp.dst_port() == 0 {
        return None;
    }
    // Guest → DNS
    if src == expected_guest_ip {
        return Some(OutboundFlow {
            guest_ip: src,
            guest_port: tcp.src_port(),
            remote_ip: dst,
            remote_port: tcp.dst_port(),
        });
    }
    // DNS → guest (reply path still owned)
    if dst == expected_guest_ip {
        return Some(OutboundFlow {
            guest_ip: dst,
            guest_port: tcp.dst_port(),
            remote_ip: src,
            remote_port: tcp.src_port(),
        });
    }
    None
}

fn spawn_outbound_connect(
    flow: OutboundFlow,
    connector_count: Arc<AtomicUsize>,
) -> io::Result<Receiver<io::Result<TcpStream>>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    connector_count.fetch_add(1, Ordering::Relaxed);
    let thread_count = Arc::clone(&connector_count);
    let spawn = std::thread::Builder::new()
        .name("a3s-dns-tcp-connect".to_string())
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

fn proxy_one(
    sockets: &mut SocketSet<'static>,
    handle: smoltcp::iface::SocketHandle,
    host_stream: &mut TcpStream,
    prefetch: &mut Vec<u8>,
    guest_read_closed: &mut bool,
    host_read_closed: &mut bool,
    abort_pending: &mut bool,
) {
    if !prefetch.is_empty() {
        let socket = sockets.get_mut::<tcp::Socket>(handle);
        match host_stream.write(prefetch) {
            Ok(0) => {
                let _ = host_stream.shutdown(Shutdown::Both);
                socket.abort();
                *abort_pending = true;
                return;
            }
            Ok(written) => {
                prefetch.drain(..written);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                return;
            }
            Err(_) => {
                let _ = host_stream.shutdown(Shutdown::Both);
                socket.abort();
                *abort_pending = true;
                return;
            }
        }
        if !prefetch.is_empty() {
            return;
        }
    }

    let socket = sockets.get_mut::<tcp::Socket>(handle);
    let mut host_write_error = None;
    if socket.can_recv() {
        let _ = socket.recv(|data| match host_stream.write(data) {
            Ok(0) if !data.is_empty() => {
                host_write_error = Some(io::Error::from(io::ErrorKind::WriteZero));
                (0, ())
            }
            Ok(written) => (written, ()),
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
    if host_write_error.is_some() {
        let _ = host_stream.shutdown(Shutdown::Both);
        socket.abort();
        *abort_pending = true;
        return;
    }

    if !*guest_read_closed && !socket.may_recv() {
        let _ = host_stream.shutdown(Shutdown::Write);
        *guest_read_closed = true;
    }

    let mut host_eof = false;
    let mut host_read_error = None;
    if !*host_read_closed && socket.can_send() {
        let _ = socket.send(|buffer| match host_stream.read(buffer) {
            Ok(0) => {
                host_eof = true;
                (0, ())
            }
            Ok(read) => (read, ()),
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
    if host_read_error.is_some() {
        let _ = host_stream.shutdown(Shutdown::Both);
        socket.abort();
        *abort_pending = true;
    } else if host_eof {
        *host_read_closed = true;
        socket.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::network::NetworkConfig;
    use smoltcp::wire::EthernetAddress;

    const GUEST_IP: Ipv4Addr = Ipv4Addr::new(10, 88, 0, 2);
    const GATEWAY_IP: Ipv4Addr = Ipv4Addr::new(10, 88, 0, 1);
    const DNS_SERVER: Ipv4Addr = Ipv4Addr::new(8, 8, 8, 8);
    const GUEST_MAC: EthernetAddress = EthernetAddress([0x02, 0x42, 10, 88, 0, 2]);

    fn exchange(guest_dev: &mut FrameDevice, owner: &mut DnsTcpOwner) {
        for frame in guest_dev.drain_tx() {
            owner.push_guest_frame(frame);
        }
        for frame in owner.poll_and_drain_tx() {
            guest_dev.rx_queue.push_back(frame);
        }
    }

    #[test]
    fn owner_answers_known_alias_with_real_tcp_handshake() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("networks.json");
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        let endpoint = net
            .connect_with_aliases("box-db", "proj-db", &["db".to_string()])
            .unwrap();
        let body = serde_json::json!({ "networks": { "mynet": net } });
        std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();

        let config = NetworkDnsConfig {
            networks_json: path,
            network_name: "mynet".into(),
            dns_servers: vec![DNS_SERVER],
            guest_ip: GUEST_IP,
            gateway_ip: GATEWAY_IP,
            prefix_len: 24,
        };
        let mut owner = DnsTcpOwner::new(config);

        let mut guest_dev = FrameDevice::new();
        let mut guest_iface =
            Interface::new(Config::new(GUEST_MAC.into()), &mut guest_dev, smoltcp_now());
        guest_iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(IpAddress::Ipv4(to_smoltcp_ipv4(GUEST_IP)), 24))
                .unwrap();
        });
        guest_iface
            .routes_mut()
            .add_default_ipv4_route(to_smoltcp_ipv4(GATEWAY_IP))
            .unwrap();
        let mut guest_sockets = SocketSet::new(vec![]);

        let mut query = vec![
            0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 2, b'd', b'b',
            0, 0x00, 0x01, 0x00, 0x01,
        ];
        let mut framed = (query.len() as u16).to_be_bytes().to_vec();
        framed.append(&mut query);

        let rx = tcp::SocketBuffer::new(vec![0u8; 4096]);
        let tx = tcp::SocketBuffer::new(vec![0u8; 4096]);
        let mut guest_tcp = tcp::Socket::new(rx, tx);
        guest_tcp
            .connect(
                guest_iface.context(),
                (IpAddress::Ipv4(to_smoltcp_ipv4(DNS_SERVER)), 53),
                53053,
            )
            .unwrap();
        let guest_handle = guest_sockets.add(guest_tcp);

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut sent = false;
        let mut received = Vec::new();
        while std::time::Instant::now() < deadline && received.len() < 2 {
            guest_iface.poll(smoltcp_now(), &mut guest_dev, &mut guest_sockets);
            exchange(&mut guest_dev, &mut owner);
            guest_iface.poll(smoltcp_now(), &mut guest_dev, &mut guest_sockets);

            let socket = guest_sockets.get_mut::<tcp::Socket>(guest_handle);
            if !sent && socket.can_send() {
                assert_eq!(socket.send_slice(&framed).unwrap(), framed.len());
                sent = true;
            }
            if socket.can_recv() {
                socket
                    .recv(|data| {
                        received.extend_from_slice(data);
                        (data.len(), ())
                    })
                    .unwrap();
            }
            std::thread::sleep(Duration::from_millis(1));
        }

        assert!(
            sent,
            "guest TCP/53 never became writable (handshake failed)"
        );
        assert!(
            received.len() >= 2,
            "NetworkStore TCP/53 answer missing after real TCP termination"
        );
        let len = u16::from_be_bytes([received[0], received[1]]) as usize;
        assert!(received.len() >= 2 + len);
        let message = &received[2..2 + len];
        assert_eq!(message[2] & 0x80, 0x80);
        assert_eq!(&message[message.len() - 4..], &endpoint.ip_address.octets());
    }

    #[test]
    fn owner_waits_for_complete_length_prefixed_query() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("networks.json");
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        let _ = net
            .connect_with_aliases("box-db", "proj-db", &["db".to_string()])
            .unwrap();
        let body = serde_json::json!({ "networks": { "mynet": net } });
        std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();

        let config = NetworkDnsConfig {
            networks_json: path,
            network_name: "mynet".into(),
            dns_servers: vec![DNS_SERVER],
            guest_ip: GUEST_IP,
            gateway_ip: GATEWAY_IP,
            prefix_len: 24,
        };
        let mut owner = DnsTcpOwner::new(config);
        let mut guest_dev = FrameDevice::new();
        let mut guest_iface =
            Interface::new(Config::new(GUEST_MAC.into()), &mut guest_dev, smoltcp_now());
        guest_iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(IpAddress::Ipv4(to_smoltcp_ipv4(GUEST_IP)), 24))
                .unwrap();
        });
        guest_iface
            .routes_mut()
            .add_default_ipv4_route(to_smoltcp_ipv4(GATEWAY_IP))
            .unwrap();
        let mut guest_sockets = SocketSet::new(vec![]);

        let query = vec![
            0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 2, b'd', b'b',
            0, 0x00, 0x01, 0x00, 0x01,
        ];
        let mut framed = (query.len() as u16).to_be_bytes().to_vec();
        framed.extend_from_slice(&query);

        let rx = tcp::SocketBuffer::new(vec![0u8; 4096]);
        let tx = tcp::SocketBuffer::new(vec![0u8; 4096]);
        let mut guest_tcp = tcp::Socket::new(rx, tx);
        guest_tcp
            .connect(
                guest_iface.context(),
                (IpAddress::Ipv4(to_smoltcp_ipv4(DNS_SERVER)), 53),
                53054,
            )
            .unwrap();
        let guest_handle = guest_sockets.add(guest_tcp);

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut established = false;
        while std::time::Instant::now() < deadline && !established {
            guest_iface.poll(smoltcp_now(), &mut guest_dev, &mut guest_sockets);
            exchange(&mut guest_dev, &mut owner);
            guest_iface.poll(smoltcp_now(), &mut guest_dev, &mut guest_sockets);
            if guest_sockets.get::<tcp::Socket>(guest_handle).can_send() {
                established = true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(established);

        // Only the first length byte — must not produce a DNS answer yet.
        {
            let socket = guest_sockets.get_mut::<tcp::Socket>(guest_handle);
            assert_eq!(socket.send_slice(&framed[..1]).unwrap(), 1);
        }
        let partial_deadline = std::time::Instant::now() + Duration::from_millis(200);
        while std::time::Instant::now() < partial_deadline {
            guest_iface.poll(smoltcp_now(), &mut guest_dev, &mut guest_sockets);
            exchange(&mut guest_dev, &mut owner);
            guest_iface.poll(smoltcp_now(), &mut guest_dev, &mut guest_sockets);
            assert!(
                !guest_sockets.get::<tcp::Socket>(guest_handle).can_recv(),
                "partial length prefix must not yield a DNS answer"
            );
            std::thread::sleep(Duration::from_millis(1));
        }

        {
            let socket = guest_sockets.get_mut::<tcp::Socket>(guest_handle);
            assert_eq!(socket.send_slice(&framed[1..]).unwrap(), framed.len() - 1);
        }
        let mut received = Vec::new();
        let full_deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < full_deadline && received.len() < 2 {
            guest_iface.poll(smoltcp_now(), &mut guest_dev, &mut guest_sockets);
            exchange(&mut guest_dev, &mut owner);
            guest_iface.poll(smoltcp_now(), &mut guest_dev, &mut guest_sockets);
            let socket = guest_sockets.get_mut::<tcp::Socket>(guest_handle);
            if socket.can_recv() {
                socket
                    .recv(|data| {
                        received.extend_from_slice(data);
                        (data.len(), ())
                    })
                    .unwrap();
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            received.len() >= 2,
            "complete length-prefixed query should produce an answer"
        );
    }
}
