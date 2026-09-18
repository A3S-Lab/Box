//! Port publishing validation.
//!
//! a3s-box supports Docker-style TCP and UDP port publishing in the
//! `host_port:guest_port[/tcp|/udp]` form. Bind-specific host IPs, ranges,
//! and other protocols are rejected before a box record is persisted or a VM
//! boots. Unresolved `host_port=0` is allocated here when the caller uses
//! [`normalize_and_resolve_port_maps`]; backends that still see `0` fail closed.

/// Supported published-port protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortProtocol {
    /// TCP port publishing.
    Tcp,
    /// UDP port publishing.
    Udp,
}

impl PortProtocol {
    /// String representation used by Docker-compatible output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

/// Parsed published-port mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortMapping {
    /// Host-side port. `0` means host auto-assignment where supported.
    pub host_port: u16,
    /// Guest/container port.
    pub guest_port: u16,
    /// Published protocol.
    pub protocol: PortProtocol,
}

impl PortMapping {
    /// Create and validate one TCP port publication.
    pub fn tcp(host_port: u16, guest_port: u16) -> Result<Self, String> {
        parse_port_mapping(&format!("{host_port}:{guest_port}"))
    }

    /// Convert to the normalized runtime format.
    ///
    /// TCP stays `host:guest` so existing records keep their spelling. UDP
    /// keeps `/udp` so passt and keep-authority DNAT cannot treat it as TCP.
    pub fn runtime_entry(&self) -> String {
        match self.protocol {
            PortProtocol::Tcp => format!("{}:{}", self.host_port, self.guest_port),
            PortProtocol::Udp => format!("{}:{}/udp", self.host_port, self.guest_port),
        }
    }
}

/// Validate and normalize multiple port mappings to runtime format.
///
/// Preserves `host_port=0` (auto-assign). Prefer
/// [`normalize_and_resolve_port_maps`] before persisting a bootable box config
/// so MicroVM/passt/TSI and keep-authority DNAT see a concrete host port.
pub fn normalize_port_maps(entries: &[String]) -> Result<Vec<String>, String> {
    entries
        .iter()
        .map(|entry| parse_port_mapping(entry).map(|mapping| mapping.runtime_entry()))
        .collect()
}

/// Normalize published ports and resolve `host_port=0` to a free ephemeral port.
///
/// Matches Docker CLI behavior: briefly claim `0.0.0.0:0`, then release so the
/// runtime can bind the chosen port (small TOCTOU window). Non-zero host ports
/// are unchanged. Call this at product admission (CLI/Compose/SDK) before
/// persisting `port_map` for boot.
pub fn normalize_and_resolve_port_maps(entries: &[String]) -> Result<Vec<String>, String> {
    normalize_port_maps(entries)?
        .into_iter()
        .map(resolve_auto_host_port)
        .collect()
}

/// Resolve an auto-assign host port (`0:guest`) to a concrete free ephemeral
/// port. A non-zero host port is returned unchanged.
pub fn resolve_auto_host_port(entry: String) -> Result<String, String> {
    let mut mapping = parse_port_mapping(&entry)?;
    if mapping.host_port != 0 {
        return Ok(mapping.runtime_entry());
    }
    let port = match mapping.protocol {
        PortProtocol::Tcp => {
            let listener = std::net::TcpListener::bind("0.0.0.0:0")
                .map_err(|e| format!("failed to allocate a host port for '{entry}': {e}"))?;
            listener
                .local_addr()
                .map_err(|e| format!("failed to read allocated host port: {e}"))?
                .port()
        }
        PortProtocol::Udp => {
            let socket = std::net::UdpSocket::bind("0.0.0.0:0")
                .map_err(|e| format!("failed to allocate a UDP host port for '{entry}': {e}"))?;
            socket
                .local_addr()
                .map_err(|e| format!("failed to read allocated UDP host port: {e}"))?
                .port()
        }
    };
    mapping.host_port = port;
    Ok(mapping.runtime_entry())
}

/// Parse a published-port mapping.
pub fn parse_port_mapping(input: &str) -> Result<PortMapping, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("Invalid port mapping: value must not be empty".to_string());
    }

    let mut protocol_split = input.split('/');
    let port_part = protocol_split.next().unwrap_or_default();
    let protocol = match protocol_split.next() {
        None => PortProtocol::Tcp,
        Some(value) if value.eq_ignore_ascii_case("tcp") => PortProtocol::Tcp,
        Some(value) if value.eq_ignore_ascii_case("udp") => PortProtocol::Udp,
        Some("") => {
            return Err(format!(
                "Invalid port mapping '{input}': protocol must not be empty"
            ));
        }
        Some(value) => {
            return Err(format!(
                "Unsupported port mapping protocol '{value}' in '{input}'; only TCP and UDP are supported"
            ));
        }
    };
    if protocol_split.next().is_some() {
        return Err(format!(
            "Invalid port mapping '{input}': expected host_port:guest_port[/tcp|/udp]"
        ));
    }

    let parts: Vec<&str> = port_part.split(':').collect();
    if parts.len() != 2 {
        return Err(format!(
            "Invalid port mapping '{input}': expected host_port:guest_port[/tcp]; bind-specific host IPs, single-port shorthand, and port ranges are not supported"
        ));
    }

    let host_port = parse_port(input, parts[0], "host", true)?;
    let guest_port = parse_port(input, parts[1], "guest", false)?;

    Ok(PortMapping {
        host_port,
        guest_port,
        protocol,
    })
}

fn parse_port(input: &str, value: &str, label: &str, allow_zero: bool) -> Result<u16, String> {
    if value.is_empty() {
        return Err(format!(
            "Invalid port mapping '{input}': {label} port must not be empty"
        ));
    }
    if value.contains('-') {
        return Err(format!(
            "Invalid port mapping '{input}': {label} port ranges are not supported"
        ));
    }

    let port = value.parse::<u16>().map_err(|_| {
        format!("Invalid port mapping '{input}': {label} port '{value}' must be 0..=65535")
    })?;
    if port == 0 && !allow_zero {
        return Err(format!(
            "Invalid port mapping '{input}': guest port must be 1..=65535"
        ));
    }
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_port_mapping_host_guest() {
        let mapping = parse_port_mapping("8080:80").unwrap();

        assert_eq!(mapping.host_port, 8080);
        assert_eq!(mapping.guest_port, 80);
        assert_eq!(mapping.protocol, PortProtocol::Tcp);
        assert_eq!(mapping.protocol.as_str(), "tcp");
        assert_eq!(mapping.runtime_entry(), "8080:80");
    }

    #[test]
    fn test_typed_tcp_mapping_validates_guest_port() {
        assert_eq!(PortMapping::tcp(0, 8080).unwrap().runtime_entry(), "0:8080");
        assert!(PortMapping::tcp(8080, 0).is_err());
    }

    #[test]
    fn test_parse_port_mapping_tcp_suffix_is_normalized() {
        let mapping = parse_port_mapping("8080:80/tcp").unwrap();

        assert_eq!(mapping.runtime_entry(), "8080:80");
    }

    #[test]
    fn test_parse_port_mapping_allows_host_port_zero() {
        let mapping = parse_port_mapping("0:8080").unwrap();

        assert_eq!(mapping.host_port, 0);
        assert_eq!(mapping.guest_port, 8080);
    }

    #[test]
    fn test_resolve_auto_host_port_leaves_static_ports() {
        assert_eq!(
            resolve_auto_host_port("8080:80".to_string()).unwrap(),
            "8080:80"
        );
    }

    #[test]
    fn test_resolve_auto_host_port_allocates_ephemeral() {
        let resolved = resolve_auto_host_port("0:80".to_string()).unwrap();
        let (host, guest) = resolved.split_once(':').unwrap();
        assert_eq!(guest, "80");
        assert_ne!(host, "0");
        assert!(host.parse::<u16>().unwrap() > 0);
    }

    #[test]
    fn test_normalize_and_resolve_port_maps() {
        let resolved = normalize_and_resolve_port_maps(&["0:443/tcp".to_string()]).unwrap();
        assert_eq!(resolved.len(), 1);
        let (host, guest) = resolved[0].split_once(':').unwrap();
        assert_eq!(guest, "443");
        assert_ne!(host, "0");
    }

    #[test]
    fn test_normalize_port_maps() {
        let entries = vec!["8080:80/tcp".to_string(), "8443:443".to_string()];

        let normalized = normalize_port_maps(&entries).unwrap();

        assert_eq!(normalized, vec!["8080:80", "8443:443"]);
    }

    #[test]
    fn test_parse_port_mapping_keeps_udp_suffix() {
        let mapping = parse_port_mapping("8080:80/udp").unwrap();
        assert_eq!(mapping.protocol, PortProtocol::Udp);
        assert_eq!(mapping.runtime_entry(), "8080:80/udp");
        let normalized = normalize_port_maps(&["8080:80/UDP".to_string()]).unwrap();
        assert_eq!(normalized, vec!["8080:80/udp"]);
    }

    #[test]
    fn test_resolve_udp_auto_host_port() {
        let resolved = normalize_and_resolve_port_maps(&["0:53/udp".to_string()]).unwrap();
        assert_eq!(resolved.len(), 1);
        assert!(resolved[0].ends_with(":53/udp"), "{}", resolved[0]);
        assert!(!resolved[0].starts_with("0:"));
    }

    #[test]
    fn test_parse_port_mapping_rejects_host_ip() {
        let error = parse_port_mapping("127.0.0.1:8080:80").unwrap_err();

        assert!(error.contains("bind-specific host IPs"));
    }

    #[test]
    fn test_parse_port_mapping_rejects_single_port() {
        let error = parse_port_mapping("80").unwrap_err();

        assert!(error.contains("single-port shorthand"));
    }

    #[test]
    fn test_parse_port_mapping_rejects_guest_zero() {
        let error = parse_port_mapping("8080:0").unwrap_err();

        assert!(error.contains("guest port"));
    }

    #[test]
    fn test_parse_port_mapping_rejects_ranges() {
        let error = parse_port_mapping("8000-8010:80").unwrap_err();

        assert!(error.contains("ranges"));
    }
}
