use super::device::{NetStatsSnapshot, MAX_FRAME};
use super::manager::{parse_port_forwards, write_stats_file};
use super::*;
use std::collections::VecDeque;

use smoltcp::wire::EthernetAddress;

const TEST_GUEST_IP: Ipv4Addr = Ipv4Addr::new(10, 88, 0, 2);
const TEST_GATEWAY_IP: Ipv4Addr = Ipv4Addr::new(10, 88, 0, 1);
const TEST_GUEST_MAC: EthernetAddress = EthernetAddress([0x02, 0x42, 10, 88, 0, 2]);

struct TestGuest {
    device: UnixgramDevice,
    iface: Interface,
    sockets: SocketSet<'static>,
}

fn test_guest_and_proxy(dns_servers: Vec<Ipv4Addr>) -> (TestGuest, ProxyEngine) {
    let (guest_socket, proxy_socket) = UnixDatagram::pair().unwrap();
    guest_socket.set_nonblocking(true).unwrap();
    proxy_socket.set_nonblocking(true).unwrap();

    let stats = Arc::new(NetStats::default());
    let mut guest_device = UnixgramDevice {
        socket: guest_socket,
        bridge: None,
        rx_queue: VecDeque::new(),
        stats: Arc::clone(&stats),
    };
    let mut guest_iface = Interface::new(
        Config::new(TEST_GUEST_MAC.into()),
        &mut guest_device,
        smoltcp_now(),
    );
    guest_iface.update_ip_addrs(|addrs| {
        addrs
            .push(IpCidr::new(
                IpAddress::Ipv4(to_smoltcp_ipv4(TEST_GUEST_IP)),
                24,
            ))
            .unwrap();
    });
    guest_iface
        .routes_mut()
        .add_default_ipv4_route(to_smoltcp_ipv4(TEST_GATEWAY_IP))
        .unwrap();

    let guest = TestGuest {
        device: guest_device,
        iface: guest_iface,
        sockets: SocketSet::new(vec![]),
    };
    let proxy = ProxyEngine::new(ProxyEngineConfig {
        socket: proxy_socket,
        guest_ip: TEST_GUEST_IP,
        gateway_ip: TEST_GATEWAY_IP,
        prefix_len: 24,
        dns_servers,
        port_forwards: Vec::new(),
        shutdown: Arc::new(AtomicBool::new(false)),
        stats,
        stats_path: None,
        bridge: None,
    });
    (guest, proxy)
}

fn poll_test_guest(guest: &mut TestGuest) {
    guest.device.drain();
    guest
        .iface
        .poll(smoltcp_now(), &mut guest.device, &mut guest.sockets);
}

fn poll_test_proxy_tcp(proxy: &mut ProxyEngine) {
    let now = smoltcp_now();
    proxy.device.drain();
    proxy.accept_connections();
    proxy.accept_outbound_flows();
    proxy.poll_outbound_connectors();
    proxy.iface.poll(now, &mut proxy.device, &mut proxy.sockets);
    proxy.finish_aborted_connections();
    proxy.promote_established();
    proxy.promote_outbound_established();
    proxy.proxy_data();
    proxy.cleanup();
}

fn port_is_bindable(port: u16) -> bool {
    TcpListener::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)).is_ok()
}

fn ports_are_bindable(ports: &[u16]) -> bool {
    ports.iter().copied().all(port_is_bindable)
}

#[test]
fn test_smoltcp_now_returns_reasonable_value() {
    let now = smoltcp_now();
    // Should return microseconds since epoch
    assert!(now.micros() > 0);
}

#[test]
fn test_to_smoltcp_ipv4_conversion() {
    let ip = Ipv4Addr::new(10, 88, 0, 1);
    let smol_ip = to_smoltcp_ipv4(ip);
    assert_eq!(smol_ip.as_bytes(), &[10, 88, 0, 1]);
}

#[test]
fn test_to_smoltcp_ipv4_loopback() {
    let ip = Ipv4Addr::new(127, 0, 0, 1);
    let smol_ip = to_smoltcp_ipv4(ip);
    assert_eq!(smol_ip.as_bytes(), &[127, 0, 0, 1]);
}

fn ethernet_frame(destination: [u8; 6], source: [u8; 6]) -> Vec<u8> {
    let mut frame = vec![0u8; 64];
    frame[..6].copy_from_slice(&destination);
    frame[6..12].copy_from_slice(&source);
    frame[12..14].copy_from_slice(&[0x08, 0x00]);
    frame[14..].fill(0x5a);
    frame
}

fn outbound_syn_frame(
    destination_mac: [u8; 6],
    source_ip: Ipv4Addr,
    source_port: u16,
    destination_ip: Ipv4Addr,
    destination_port: u16,
    ack: bool,
) -> Vec<u8> {
    let mut frame = vec![0u8; 14 + 20 + 20];
    frame[..6].copy_from_slice(&destination_mac);
    frame[6..12].copy_from_slice(&[0x02, 0x42, 10, 88, 0, 2]);
    frame[12..14].copy_from_slice(&[0x08, 0x00]);

    let ipv4 = &mut frame[14..34];
    ipv4[0] = 0x45;
    ipv4[2..4].copy_from_slice(&40u16.to_be_bytes());
    ipv4[8] = 64;
    ipv4[9] = 6;
    ipv4[12..16].copy_from_slice(&source_ip.octets());
    ipv4[16..20].copy_from_slice(&destination_ip.octets());

    let tcp = &mut frame[34..];
    tcp[..2].copy_from_slice(&source_port.to_be_bytes());
    tcp[2..4].copy_from_slice(&destination_port.to_be_bytes());
    tcp[12] = 0x50;
    tcp[13] = if ack { 0x12 } else { 0x02 };
    tcp[14..16].copy_from_slice(&65535u16.to_be_bytes());
    frame
}

#[test]
fn outbound_syn_flow_preserves_original_destination() {
    let guest = Ipv4Addr::new(10, 88, 0, 2);
    let gateway = Ipv4Addr::new(10, 88, 0, 1);
    let remote = Ipv4Addr::new(93, 184, 216, 34);
    let frame = outbound_syn_frame(GATEWAY_MAC.0, guest, 50123, remote, 443, false);

    assert_eq!(
        outbound_syn_flow(&frame, guest, gateway),
        Some(OutboundFlow {
            guest_ip: guest,
            guest_port: 50123,
            remote_ip: remote,
            remote_port: 443,
        })
    );
}

#[test]
fn outbound_syn_flow_ignores_retransmitted_handshake_ack_and_peer_mac() {
    let guest = Ipv4Addr::new(10, 88, 0, 2);
    let gateway = Ipv4Addr::new(10, 88, 0, 1);
    let remote = Ipv4Addr::new(93, 184, 216, 34);
    let ack = outbound_syn_frame(GATEWAY_MAC.0, guest, 50123, remote, 443, true);
    assert_eq!(outbound_syn_flow(&ack, guest, gateway), None);

    let peer_mac = [0x02, 0x42, 10, 88, 0, 3];
    let peer = outbound_syn_frame(peer_mac, guest, 50123, remote, 443, false);
    assert_eq!(outbound_syn_flow(&peer, guest, gateway), None);
}

#[test]
fn proxy_engine_enables_any_ip_for_transparent_outbound_tcp() {
    let (guest_socket, proxy_socket) = UnixDatagram::pair().unwrap();
    guest_socket.set_nonblocking(true).unwrap();
    proxy_socket.set_nonblocking(true).unwrap();
    let engine = ProxyEngine::new(ProxyEngineConfig {
        socket: proxy_socket,
        guest_ip: Ipv4Addr::new(10, 88, 0, 2),
        gateway_ip: Ipv4Addr::new(10, 88, 0, 1),
        prefix_len: 24,
        dns_servers: vec![Ipv4Addr::new(8, 8, 8, 8)],
        port_forwards: Vec::new(),
        shutdown: Arc::new(AtomicBool::new(false)),
        stats: Arc::new(NetStats::default()),
        stats_path: None,
        bridge: None,
    });

    assert!(engine.iface.any_ip());
    let (dns_handle, dns_server) = engine.dns_sockets[0];
    let dns_socket = engine.sockets.get::<udp::Socket>(dns_handle);
    assert_eq!(dns_server, Ipv4Addr::new(8, 8, 8, 8));
    assert_eq!(
        dns_socket.endpoint().addr,
        Some(IpAddress::Ipv4(to_smoltcp_ipv4(dns_server)))
    );
}

#[test]
fn outbound_tcp_proxy_transfers_bytes_end_to_end() {
    const GUEST_REQUEST: &[u8] = b"guest-request";
    const HOST_RESPONSE: &[u8] = b"host-response";

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let host_port = listener.local_addr().unwrap().port();
    let (mut guest, mut proxy) = test_guest_and_proxy(Vec::new());

    let rx = tcp::SocketBuffer::new(vec![0u8; 4096]);
    let tx = tcp::SocketBuffer::new(vec![0u8; 4096]);
    let mut guest_tcp = tcp::Socket::new(rx, tx);
    guest_tcp
        .connect(
            guest.iface.context(),
            (
                IpAddress::Ipv4(to_smoltcp_ipv4(Ipv4Addr::LOCALHOST)),
                host_port,
            ),
            50123,
        )
        .unwrap();
    let guest_handle = guest.sockets.add(guest_tcp);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut host_stream = None;
    let mut guest_sent = false;
    let mut host_received = Vec::new();
    let mut host_response_offset = 0;
    let mut guest_received = Vec::new();

    while std::time::Instant::now() < deadline && guest_received != HOST_RESPONSE {
        poll_test_guest(&mut guest);
        poll_test_proxy_tcp(&mut proxy);

        if host_stream.is_none() {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(true).unwrap();
                    host_stream = Some(stream);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("host listener accept failed: {error}"),
            }
        }

        {
            let socket = guest.sockets.get_mut::<tcp::Socket>(guest_handle);
            if !guest_sent && socket.can_send() {
                assert_eq!(
                    socket.send_slice(GUEST_REQUEST).unwrap(),
                    GUEST_REQUEST.len()
                );
                guest_sent = true;
            }
            if socket.can_recv() {
                socket
                    .recv(|data| {
                        guest_received.extend_from_slice(data);
                        (data.len(), ())
                    })
                    .unwrap();
            }
        }

        if let Some(stream) = host_stream.as_mut() {
            let mut buffer = [0u8; 128];
            match stream.read(&mut buffer) {
                Ok(0) => {}
                Ok(size) => host_received.extend_from_slice(&buffer[..size]),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => panic!("host stream read failed: {error}"),
            }

            if host_received == GUEST_REQUEST && host_response_offset < HOST_RESPONSE.len() {
                match stream.write(&HOST_RESPONSE[host_response_offset..]) {
                    Ok(0) => panic!("host stream returned write zero"),
                    Ok(size) => host_response_offset += size,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(error) => panic!("host stream write failed: {error}"),
                }
            }
        }

        std::thread::sleep(Duration::from_millis(1));
    }

    assert!(guest_sent, "guest TCP connection never became writable");
    assert_eq!(host_received, GUEST_REQUEST);
    assert_eq!(guest_received, HOST_RESPONSE);
    assert_eq!(proxy.pending_outbound.len(), 0);
    assert_eq!(proxy.active_outbound.len(), 1);
}

#[test]
fn dns_response_preserves_queried_server_endpoint_end_to_end() {
    const QUERY: &[u8] = b"dns-query";
    const RESPONSE: &[u8] = b"dns-response";

    let dns_server = Ipv4Addr::new(8, 8, 8, 8);
    let (mut guest, mut proxy) = test_guest_and_proxy(vec![dns_server]);
    let rx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0u8; 1024]);
    let tx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0u8; 1024]);
    let mut guest_udp = udp::Socket::new(rx, tx);
    guest_udp.bind(53000).unwrap();
    guest_udp
        .send_slice(
            QUERY,
            IpEndpoint::new(IpAddress::Ipv4(to_smoltcp_ipv4(dns_server)), 53),
        )
        .unwrap();
    let guest_handle = guest.sockets.add(guest_udp);
    let (proxy_handle, _) = proxy.dns_sockets[0];

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut injected_response = false;
    let mut received = None;

    while std::time::Instant::now() < deadline && received.is_none() {
        poll_test_guest(&mut guest);
        proxy.device.drain();
        proxy
            .iface
            .poll(smoltcp_now(), &mut proxy.device, &mut proxy.sockets);

        if !injected_response {
            let socket = proxy.sockets.get_mut::<udp::Socket>(proxy_handle);
            if socket.can_recv() {
                let (query, source) = socket.recv().unwrap();
                assert_eq!(query, QUERY);
                assert_eq!(
                    source.endpoint,
                    IpEndpoint::new(IpAddress::Ipv4(to_smoltcp_ipv4(TEST_GUEST_IP)), 53000,)
                );
                socket.send_slice(RESPONSE, source).unwrap();
                injected_response = true;
            }
        }

        proxy
            .iface
            .poll(smoltcp_now(), &mut proxy.device, &mut proxy.sockets);
        poll_test_guest(&mut guest);

        let socket = guest.sockets.get_mut::<udp::Socket>(guest_handle);
        if socket.can_recv() {
            let (payload, source) = socket.recv().unwrap();
            received = Some((payload.to_vec(), source.endpoint));
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    assert!(
        injected_response,
        "proxy never received the guest DNS query"
    );
    assert_eq!(
        received,
        Some((
            RESPONSE.to_vec(),
            IpEndpoint::new(IpAddress::Ipv4(to_smoltcp_ipv4(dns_server)), 53),
        ))
    );
}

#[test]
fn bridge_port_unicasts_frame_to_matching_peer_mac() {
    let dir = tempfile::tempdir().unwrap();
    let mac_a = [0x02, 0x42, 10, 88, 0, 2];
    let mac_b = [0x02, 0x42, 10, 88, 0, 3];
    let bridge_a = BridgePort::bind(dir.path(), mac_a).unwrap();
    let bridge_b = BridgePort::bind(dir.path(), mac_b).unwrap();
    let (guest_b, proxy_b) = UnixDatagram::pair().unwrap();
    guest_b.set_nonblocking(true).unwrap();
    let frame = ethernet_frame(mac_b, mac_a);

    assert!(!bridge_a.forward_from_guest(&frame));
    bridge_b.drain_to_guest(&proxy_b, &NetStats::default());

    let mut received = [0u8; MAX_FRAME];
    let size = guest_b.recv(&mut received).unwrap();
    assert_eq!(&received[..size], frame.as_slice());
}

#[test]
fn bridge_port_floods_broadcast_and_keeps_gateway_delivery() {
    let dir = tempfile::tempdir().unwrap();
    let mac_a = [0x02, 0x42, 10, 88, 0, 2];
    let mac_b = [0x02, 0x42, 10, 88, 0, 3];
    let bridge_a = BridgePort::bind(dir.path(), mac_a).unwrap();
    let bridge_b = BridgePort::bind(dir.path(), mac_b).unwrap();
    let (guest_b, proxy_b) = UnixDatagram::pair().unwrap();
    guest_b.set_nonblocking(true).unwrap();
    let frame = ethernet_frame([0xff; 6], mac_a);

    assert!(bridge_a.forward_from_guest(&frame));
    bridge_b.drain_to_guest(&proxy_b, &NetStats::default());

    let mut received = [0u8; MAX_FRAME];
    let size = guest_b.recv(&mut received).unwrap();
    assert_eq!(&received[..size], frame.as_slice());
}

#[test]
fn test_net_stats_records_bytes_and_packets() {
    let stats = NetStats::default();

    stats.record_rx(64);
    stats.record_rx(128);
    stats.record_tx(512);

    let snapshot = stats.snapshot();
    assert_eq!(snapshot.rx_bytes, 192);
    assert_eq!(snapshot.rx_packets, 2);
    assert_eq!(snapshot.tx_bytes, 512);
    assert_eq!(snapshot.tx_packets, 1);
}

#[test]
fn test_parse_port_forwards_empty_rules() {
    let guest = Ipv4Addr::new(10, 89, 0, 2);
    let fwds = parse_port_forwards(&[], guest).unwrap();
    assert!(fwds.is_empty());
}

#[test]
fn test_parse_port_forwards_rejects_udp_suffix() {
    let guest = Ipv4Addr::new(10, 89, 0, 2);
    let rules = vec!["19990:80/udp".to_string()];
    let error = match parse_port_forwards(&rules, guest) {
        Ok(_) => panic!("UDP port mapping unexpectedly succeeded"),
        Err(error) => error,
    };

    assert!(error.contains("only TCP is supported"));
}

#[test]
fn test_parse_port_forwards_multiple_rules() {
    let guest = Ipv4Addr::new(10, 89, 0, 2);
    if !ports_are_bindable(&[19991, 19992, 19993]) {
        eprintln!("skipping test: one or more host ports are not bindable");
        return;
    }
    let rules = vec![
        "19991:80".to_string(),
        "19992:443".to_string(),
        "19993:8080".to_string(),
    ];
    let fwds = parse_port_forwards(&rules, guest).unwrap();
    assert_eq!(fwds.len(), 3);
    assert_eq!(fwds[0].guest_port, 80);
    assert_eq!(fwds[1].guest_port, 443);
    assert_eq!(fwds[2].guest_port, 8080);
}

#[test]
fn test_parse_port_forwards_empty_string() {
    let guest = Ipv4Addr::new(10, 89, 0, 2);
    // Empty entry should fail parsing
    let rules = vec!["".to_string()];
    let result = parse_port_forwards(&rules, guest);
    assert!(result.is_err());
}

#[test]
fn test_netproxy_manager_new() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = NetProxyManager::new(dir.path());
    assert_eq!(
        mgr.socket_path(),
        dir.path().join("sockets").join("net.sock")
    );
    assert_eq!(
        mgr.stats_path(),
        dir.path().join("sockets").join("net.stats.json")
    );
    assert_eq!(mgr.net_socket_fd(), None);
}

#[test]
fn test_netproxy_manager_not_running_initially() {
    let dir = tempfile::tempdir().unwrap();
    let mut mgr = NetProxyManager::new(dir.path());
    assert!(!mgr.is_running());
}

#[test]
fn test_netproxy_manager_stop_when_not_started() {
    let dir = tempfile::tempdir().unwrap();
    let mut mgr = NetProxyManager::new(dir.path());
    mgr.stop(); // must not panic
    assert!(!mgr.is_running());
}

#[test]
fn test_netproxy_manager_spawn_creates_socketpair_fds_and_stop_closes_them() {
    let dir = tempfile::tempdir().unwrap();
    let mut mgr = NetProxyManager::new(dir.path());

    mgr.spawn(
        Ipv4Addr::new(10, 89, 0, 2),
        Ipv4Addr::new(10, 89, 0, 1),
        24,
        &[Ipv4Addr::new(8, 8, 8, 8)],
        &[],
    )
    .unwrap();

    assert!(mgr.is_running());
    assert!(mgr.net_socket_fd().is_some());
    assert!(mgr.net_proxy_fd().is_some());

    mgr.stop();
    assert!(!mgr.is_running());
    assert!(mgr.net_socket_fd().is_none());
    assert!(mgr.net_proxy_fd().is_none());
}

#[test]
fn test_netproxy_manager_drop_cleans_up() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("sockets").join("net.sock");
    std::fs::create_dir_all(dir.path().join("sockets")).unwrap();
    std::fs::write(&socket_path, "fake").unwrap();
    {
        let _mgr = NetProxyManager::new(dir.path());
        // Drop triggers cleanup
    }
    assert!(!socket_path.exists());
}

#[test]
fn test_write_stats_file_writes_json_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sockets").join("net.stats.json");

    write_stats_file(
        &path,
        NetStatsSnapshot {
            rx_bytes: 1024,
            tx_bytes: 2048,
            rx_packets: 3,
            tx_packets: 4,
        },
    )
    .unwrap();

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(json["schema"], "a3s-box.netproxy.stats.v1");
    assert_eq!(json["rx_bytes"], 1024);
    assert_eq!(json["tx_bytes"], 2048);
    assert_eq!(json["rx_packets"], 3);
    assert_eq!(json["tx_packets"], 4);
}

#[test]
fn test_write_stats_file_overwrites_existing_file_and_removes_temp() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("net.stats.json");
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&path, "old").unwrap();
    std::fs::write(&tmp, "stale temp").unwrap();

    write_stats_file(
        &path,
        NetStatsSnapshot {
            rx_bytes: 1,
            tx_bytes: 2,
            rx_packets: 3,
            tx_packets: 4,
        },
    )
    .unwrap();

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(json["rx_bytes"], 1);
    assert_eq!(json["tx_bytes"], 2);
    assert!(!tmp.exists());
}

#[test]
fn test_parse_port_forwards_valid() {
    let guest = Ipv4Addr::new(10, 89, 0, 2);
    if !ports_are_bindable(&[19988, 19443]) {
        eprintln!("skipping test: one or more host ports are not bindable");
        return;
    }
    // Use a random high port to avoid conflicts
    let rules = vec!["19988:80".to_string(), "19443:443".to_string()];
    let fwds = parse_port_forwards(&rules, guest).unwrap();
    assert_eq!(fwds.len(), 2);
    assert_eq!(fwds[0].guest_port, 80);
    assert_eq!(fwds[1].guest_port, 443);
}

#[test]
fn test_parse_port_forwards_with_protocol_suffix() {
    let guest = Ipv4Addr::new(10, 89, 0, 2);
    if !port_is_bindable(19989) {
        eprintln!("skipping test: host port 19989 is not bindable");
        return;
    }
    let rules = vec!["19989:80/tcp".to_string()];
    let fwds = parse_port_forwards(&rules, guest).unwrap();
    assert_eq!(fwds[0].guest_port, 80);
}

#[test]
fn test_parse_port_forwards_invalid_format() {
    let guest = Ipv4Addr::new(10, 89, 0, 2);
    assert!(parse_port_forwards(&["notaport".to_string()], guest).is_err());
    assert!(parse_port_forwards(&["abc:80".to_string()], guest).is_err());
    assert!(parse_port_forwards(&["80:xyz".to_string()], guest).is_err());
}

#[test]
fn test_parse_port_forwards_reports_bind_conflict() {
    let guest = Ipv4Addr::new(10, 89, 0, 2);
    let held = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).unwrap();
    let port = held.local_addr().unwrap().port();

    let error = match parse_port_forwards(&[format!("{port}:80")], guest) {
        Ok(_) => panic!("port-forward bind conflict should return an error"),
        Err(error) => error,
    };

    assert!(error.contains(&format!("cannot bind 0.0.0.0:{port}")));
}

// Note: test_netproxy_manager_spawn_binds_and_releases_host_ports was removed
// because spawn() no longer spawns a thread or binds ports. Port binding
// now happens in spawn_inherited_netproxy() called from the shim.
