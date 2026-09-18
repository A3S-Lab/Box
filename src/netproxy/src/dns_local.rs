//! Minimal NetworkStore-local DNS A answers for netproxy.
//!
//! Guests already send UDP/53 to configured upstream AnyIP addresses. Before
//! forwarding upstream, answer A queries for names registered in Box
//! `networks.json` so late-joining peers are resolvable without rewriting
//! guest `/etc/hosts`.

use std::net::Ipv4Addr;
use std::path::Path;

const DNS_TYPE_A: u16 = 1;
const DNS_CLASS_IN: u16 = 1;
const DNS_TTL_SECS: u32 = 30;

/// If `query` is a single-question A lookup for a NetworkStore name/alias,
/// return a synthesized response. Otherwise `None` (caller forwards upstream).
pub(crate) fn try_network_a_response(
    query: &[u8],
    networks_json: &Path,
    network_name: &str,
) -> Option<Vec<u8>> {
    let qname = parse_query_a_name(query)?;
    let ip = a3s_box_core::network::lookup_network_a(networks_json, network_name, &qname)?;
    build_a_response(query, ip)
}

/// Parse a standard DNS query with one question; only QTYPE A is accepted.
fn parse_query_a_name(query: &[u8]) -> Option<String> {
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
    if qtype != DNS_TYPE_A || qclass != DNS_CLASS_IN {
        return None;
    }
    Some(labels.join("."))
}

fn build_a_response(query: &[u8], ip: Ipv4Addr) -> Option<Vec<u8>> {
    if query.len() < 12 {
        return None;
    }
    let mut out = query.to_vec();
    let flags = u16::from_be_bytes([out[2], out[3]]);
    // QR | AA | copy RD | RA
    let flags = (flags | 0x8400 | 0x0080) & !0x0200;
    out[2] = (flags >> 8) as u8;
    out[3] = (flags & 0xff) as u8;
    out[6] = 0;
    out[7] = 1; // ANCOUNT
    out[8] = 0;
    out[9] = 0; // NSCOUNT
    out[10] = 0;
    out[11] = 0; // ARCOUNT
    // Answer: compression pointer to QNAME at offset 12.
    out.extend_from_slice(&[0xC0, 0x0C]);
    out.extend_from_slice(&DNS_TYPE_A.to_be_bytes());
    out.extend_from_slice(&DNS_CLASS_IN.to_be_bytes());
    out.extend_from_slice(&DNS_TTL_SECS.to_be_bytes());
    out.extend_from_slice(&4u16.to_be_bytes());
    out.extend_from_slice(&ip.octets());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::network::NetworkConfig;
    use std::io::Write;

    fn encode_query(name: &str) -> Vec<u8> {
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
        out.extend_from_slice(&DNS_TYPE_A.to_be_bytes());
        out.extend_from_slice(&DNS_CLASS_IN.to_be_bytes());
        out
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
    fn unknown_name_returns_none_for_upstream_forward() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("networks.json");
        let net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        let body = serde_json::json!({ "networks": { "mynet": net } });
        std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();
        assert!(try_network_a_response(&encode_query("example.com"), &path, "mynet").is_none());
    }
}
