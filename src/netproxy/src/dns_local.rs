//! Minimal NetworkStore-local DNS answers for netproxy / passt_bridge.
//!
//! Guests already send UDP/53 to configured upstream AnyIP addresses. Before
//! forwarding upstream, answer queries for names registered in Box
//! `networks.json` so late-joining peers are resolvable without rewriting
//! guest `/etc/hosts`:
//! - QTYPE A → A record with the endpoint IPv4
//! - QTYPE AAAA for a known NetworkStore name → authoritative NODATA (no IPv6
//!   in NetworkStore; avoid upstream NXDOMAIN/false negatives for dual-stack
//!   resolvers that query AAAA first)

use std::net::Ipv4Addr;
use std::path::Path;

const DNS_TYPE_A: u16 = 1;
const DNS_TYPE_AAAA: u16 = 28;
const DNS_CLASS_IN: u16 = 1;
const DNS_TTL_SECS: u32 = 30;

/// Split one DNS-over-TCP message (`u16be` length + payload).
///
/// Returns `(message, bytes_consumed)`. `None` while the length prefix or
/// payload is still incomplete. A zero length is rejected.
pub(crate) fn split_dns_tcp_message(buf: &[u8]) -> Option<(&[u8], usize)> {
    if buf.len() < 2 {
        return None;
    }
    let len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
    if len == 0 || buf.len() < 2 + len {
        return None;
    }
    Some((&buf[2..2 + len], 2 + len))
}

/// If `query` is a single-question A/AAAA lookup for a NetworkStore name/alias,
/// return a synthesized response. Otherwise `None` (caller forwards upstream).
pub(crate) fn try_network_a_response(
    query: &[u8],
    networks_json: &Path,
    network_name: &str,
) -> Option<Vec<u8>> {
    let (qname, qtype) = parse_query_name_and_type(query)?;
    // Name must exist in NetworkStore; unknown names always forward.
    let ip = a3s_box_core::network::lookup_network_a(networks_json, network_name, &qname)?;
    match qtype {
        DNS_TYPE_A => build_a_response(query, ip),
        DNS_TYPE_AAAA => build_nodata_response(query),
        _ => None,
    }
}

/// Parse a standard DNS query with one question; returns (qname, qtype).
fn parse_query_name_and_type(query: &[u8]) -> Option<(String, u16)> {
    if query.len() < 12 {
        return None;
    }
    let qdcount = u16::from_be_bytes([query[4], query[5]]);
    if qdcount != 1 {
        return None;
    }
    // Reject responses / truncated nonsense.
    if query[2] & 0x80 != 0 {
        return None;
    }
    let mut offset = 12usize;
    let mut labels = Vec::new();
    loop {
        if offset >= query.len() {
            return None;
        }
        let len = query[offset] as usize;
        offset += 1;
        if len == 0 {
            break;
        }
        if len > 63 || offset + len > query.len() {
            return None;
        }
        let label = std::str::from_utf8(&query[offset..offset + len]).ok()?;
        labels.push(label.to_ascii_lowercase());
        offset += len;
    }
    if offset + 4 > query.len() {
        return None;
    }
    let qtype = u16::from_be_bytes([query[offset], query[offset + 1]]);
    let qclass = u16::from_be_bytes([query[offset + 2], query[offset + 3]]);
    if qclass != DNS_CLASS_IN {
        return None;
    }
    if qtype != DNS_TYPE_A && qtype != DNS_TYPE_AAAA {
        return None;
    }
    Some((labels.join("."), qtype))
}

fn build_response_header(query: &[u8], ancount: u16) -> Option<Vec<u8>> {
    if query.len() < 12 {
        return None;
    }
    let mut out = query.to_vec();
    let flags = u16::from_be_bytes([out[2], out[3]]);
    // QR | AA | copy RD | RA
    let flags = (flags | 0x8400 | 0x0080) & !0x0200;
    out[2] = (flags >> 8) as u8;
    out[3] = (flags & 0xff) as u8;
    out[6] = (ancount >> 8) as u8;
    out[7] = (ancount & 0xff) as u8;
    out[8] = 0;
    out[9] = 0; // NSCOUNT
    out[10] = 0;
    out[11] = 0; // ARCOUNT
    Some(out)
}

fn build_a_response(query: &[u8], ip: Ipv4Addr) -> Option<Vec<u8>> {
    let mut out = build_response_header(query, 1)?;
    // Answer: compression pointer to QNAME at offset 12.
    out.extend_from_slice(&[0xC0, 0x0C]);
    out.extend_from_slice(&DNS_TYPE_A.to_be_bytes());
    out.extend_from_slice(&DNS_CLASS_IN.to_be_bytes());
    out.extend_from_slice(&DNS_TTL_SECS.to_be_bytes());
    out.extend_from_slice(&4u16.to_be_bytes());
    out.extend_from_slice(&ip.octets());
    Some(out)
}

/// Authoritative empty answer for AAAA on an IPv4-only NetworkStore name.
fn build_nodata_response(query: &[u8]) -> Option<Vec<u8>> {
    build_response_header(query, 0)
}

/// Config for answering NetworkStore DNS queries on a raw Ethernet path
/// (Linux passt_bridge). Guests query configured upstream IPs on UDP/53.
#[derive(Clone)]
pub struct NetworkDnsConfig {
    pub networks_json: std::path::PathBuf,
    pub network_name: String,
    pub dns_servers: Vec<Ipv4Addr>,
}

/// If `frame` is IPv4 UDP/53 to a configured DNS server for a NetworkStore name,
/// return a full Ethernet reply frame. Otherwise `None` (forward upstream).
pub(crate) fn try_ethernet_network_a_reply(
    frame: &[u8],
    config: &NetworkDnsConfig,
) -> Option<Vec<u8>> {
    // Ethernet: dst(6) src(6) type(2) + IPv4
    if frame.len() < 14 + 20 + 8 {
        return None;
    }
    if frame[12..14] != [0x08, 0x00] {
        return None;
    }
    let ip = &frame[14..];
    let ihl = (ip[0] & 0x0f) as usize * 4;
    if ihl < 20 || frame.len() < 14 + ihl + 8 {
        return None;
    }
    if ip[9] != 17 {
        return None; // UDP
    }
    let dst_ip = Ipv4Addr::new(ip[16], ip[17], ip[18], ip[19]);
    if !config.dns_servers.contains(&dst_ip) {
        return None;
    }
    let udp = &ip[ihl..];
    let dst_port = u16::from_be_bytes([udp[2], udp[3]]);
    if dst_port != 53 {
        return None;
    }
    let udp_len = u16::from_be_bytes([udp[4], udp[5]]) as usize;
    if udp_len < 8 || udp.len() < udp_len {
        return None;
    }
    let query = &udp[8..udp_len];
    let dns_payload = try_network_a_response(query, &config.networks_json, &config.network_name)?;

    let src_mac = [frame[6], frame[7], frame[8], frame[9], frame[10], frame[11]];
    let dst_mac = [frame[0], frame[1], frame[2], frame[3], frame[4], frame[5]];
    let src_ip = Ipv4Addr::new(ip[12], ip[13], ip[14], ip[15]);
    let src_port = u16::from_be_bytes([udp[0], udp[1]]);

    Some(build_ipv4_udp_ethernet_frame(
        dst_mac, // reply src = original dst (gateway)
        src_mac, // reply dst = guest
        dst_ip,  // reply src IP = DNS server
        src_ip,  // reply dst IP = guest
        53,
        src_port,
        &dns_payload,
    ))
}

fn build_ipv4_udp_ethernet_frame(
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> Vec<u8> {
    let udp_len = 8 + payload.len();
    let ip_len = 20 + udp_len;
    let mut frame = Vec::with_capacity(14 + ip_len);
    frame.extend_from_slice(&dst_mac);
    frame.extend_from_slice(&src_mac);
    frame.extend_from_slice(&[0x08, 0x00]);

    let mut ip_header = [0u8; 20];
    ip_header[0] = 0x45; // v4, IHL=5
    ip_header[1] = 0;
    ip_header[2] = (ip_len >> 8) as u8;
    ip_header[3] = (ip_len & 0xff) as u8;
    ip_header[6] = 0x40; // DF
    ip_header[8] = 64; // TTL
    ip_header[9] = 17; // UDP
    ip_header[12..16].copy_from_slice(&src_ip.octets());
    ip_header[16..20].copy_from_slice(&dst_ip.octets());
    let checksum = ipv4_header_checksum(&ip_header);
    ip_header[10] = (checksum >> 8) as u8;
    ip_header[11] = (checksum & 0xff) as u8;
    frame.extend_from_slice(&ip_header);

    frame.extend_from_slice(&src_port.to_be_bytes());
    frame.extend_from_slice(&dst_port.to_be_bytes());
    frame.extend_from_slice(&(udp_len as u16).to_be_bytes());
    frame.extend_from_slice(&0u16.to_be_bytes()); // UDP checksum optional (0)
    frame.extend_from_slice(payload);
    frame
}

fn ipv4_header_checksum(header: &[u8; 20]) -> u16 {
    let mut sum = 0u32;
    for chunk in header.as_chunks::<2>().0 {
        sum += u16::from_be_bytes(*chunk) as u32;
    }
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::network::NetworkConfig;
    use std::io::Write;

    fn encode_query_typed(name: &str, qtype: u16) -> Vec<u8> {
        let mut out = vec![
            0x12, 0x34, // ID
            0x01, 0x00, // RD
            0x00, 0x01, // QDCOUNT
            0x00, 0x00, // ANCOUNT
            0x00, 0x00, // NSCOUNT
            0x00, 0x00, // ARCOUNT
        ];
        for label in name.split('.') {
            out.push(label.len() as u8);
            out.extend_from_slice(label.as_bytes());
        }
        out.push(0);
        out.extend_from_slice(&qtype.to_be_bytes());
        out.extend_from_slice(&DNS_CLASS_IN.to_be_bytes());
        out
    }

    fn encode_query(name: &str) -> Vec<u8> {
        encode_query_typed(name, DNS_TYPE_A)
    }

    #[test]
    fn split_dns_tcp_message_waits_for_complete_payload() {
        let query = encode_query("db");
        let mut framed = (query.len() as u16).to_be_bytes().to_vec();
        assert!(split_dns_tcp_message(&framed).is_none());
        framed.extend_from_slice(&query[..query.len() - 1]);
        assert!(split_dns_tcp_message(&framed).is_none());
        framed.push(query[query.len() - 1]);
        let (message, consumed) = split_dns_tcp_message(&framed).unwrap();
        assert_eq!(message, query.as_slice());
        assert_eq!(consumed, framed.len());
        assert!(split_dns_tcp_message(&[0, 0]).is_none());
    }

    #[test]
    fn local_a_answer_for_registered_alias() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("networks.json");
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        let ep = net
            .connect_with_aliases("box-db", "proj-db", &["db".to_string()])
            .unwrap();
        let body = serde_json::json!({ "networks": { "mynet": net } });
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(serde_json::to_string(&body).unwrap().as_bytes())
            .unwrap();

        let query = encode_query("db");
        let response = try_network_a_response(&query, &path, "mynet").unwrap();
        assert_eq!(&response[0..2], &query[0..2]);
        assert_eq!(response[2] & 0x80, 0x80); // QR
        assert_eq!(response[7], 1); // ANCOUNT
        assert_eq!(&response[response.len() - 4..], &ep.ip_address.octets());
    }

    #[test]
    fn local_aaaa_nodata_for_registered_alias() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("networks.json");
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        let _ = net
            .connect_with_aliases("box-db", "proj-db", &["db".to_string()])
            .unwrap();
        let body = serde_json::json!({ "networks": { "mynet": net } });
        std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();

        let query = encode_query_typed("db", DNS_TYPE_AAAA);
        let response = try_network_a_response(&query, &path, "mynet").unwrap();
        assert_eq!(response[2] & 0x80, 0x80); // QR
        assert_eq!(response[2] & 0x04, 0x04); // AA
        assert_eq!(response[7], 0); // ANCOUNT NODATA
        assert_eq!(response.len(), query.len());
    }

    #[test]
    fn unknown_aaaa_returns_none_for_upstream_forward() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("networks.json");
        let net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        let body = serde_json::json!({ "networks": { "mynet": net } });
        std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();
        assert!(try_network_a_response(
            &encode_query_typed("example.com", DNS_TYPE_AAAA),
            &path,
            "mynet"
        )
        .is_none());
    }

    #[test]
    fn unknown_name_returns_none_for_upstream_forward() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("networks.json");
        let net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        let body = serde_json::json!({ "networks": { "mynet": net } });
        std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();
        assert!(try_network_a_response(&encode_query("example.com"), &path, "mynet").is_none());
    }

    #[test]
    fn ethernet_udp_dns_a_reply_for_registered_alias() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("networks.json");
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        let ep = net
            .connect_with_aliases("box-db", "proj-db", &["db".to_string()])
            .unwrap();
        let body = serde_json::json!({ "networks": { "mynet": net } });
        std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();

        let dns_server = Ipv4Addr::new(8, 8, 8, 8);
        let guest_ip = Ipv4Addr::new(10, 88, 0, 2);
        let query = encode_query("db");
        let frame = build_ipv4_udp_ethernet_frame(
            [0x02, 0x42, 0x0a, 0x58, 0x00, 0x02],
            [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
            guest_ip,
            dns_server,
            53000,
            53,
            &query,
        );
        let config = NetworkDnsConfig {
            networks_json: path,
            network_name: "mynet".into(),
            dns_servers: vec![dns_server],
        };
        let reply = try_ethernet_network_a_reply(&frame, &config).unwrap();
        assert_eq!(&reply[0..6], &frame[6..12]); // dst = guest
        assert_eq!(&reply[6..12], &frame[0..6]); // src = gateway
        assert_eq!(&reply[reply.len() - 4..], &ep.ip_address.octets());
    }
}
