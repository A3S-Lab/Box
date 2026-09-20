//! DNS configuration helpers for guest rootfs.
//!
//! Generates /etc/resolv.conf content from user-specified DNS servers,
//! host configuration, or sensible defaults.

use std::net::IpAddr;

/// Default DNS servers (Google Public DNS).
const DEFAULT_DNS: &[&str] = &["8.8.8.8", "8.8.4.4"];

/// Host resolv.conf path normally consulted for inheritance.
const HOST_RESOLV_CONF: &str = "/etc/resolv.conf";

/// systemd-resolved upstream list (not the stub listener).
///
/// On hosts where `/etc/resolv.conf` points at the stub (`127.0.0.53`), this
/// file still holds the real recursive nameservers. Prefer it when present so
/// guests do not inherit a loopback address that is unreachable under TSI for
/// glibc/c-ares connected UDP (see #455).
const SYSTEMD_RESOLVED_UPSTREAM: &str = "/run/systemd/resolve/resolv.conf";

/// A static host-to-IP mapping for `/etc/hosts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostEntry {
    /// Hostname or DNS name.
    pub host: String,
    /// IP address string.
    pub ip: String,
}

/// Generate resolv.conf content for the guest rootfs.
///
/// Resolution order:
/// 1. If `custom_dns` is non-empty, use those servers (explicit `--dns`)
/// 2. Otherwise, inherit usable host nameservers (loopback filtered; prefer
///    systemd-resolved upstream when the stub is detected)
/// 3. Fall back to Google Public DNS (8.8.8.8, 8.8.4.4)
pub fn generate_resolv_conf(custom_dns: &[String]) -> String {
    if !custom_dns.is_empty() {
        return custom_dns
            .iter()
            .map(|s| format!("nameserver {s}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
    }

    if let Some(host_resolv) = read_host_resolv_conf() {
        return host_resolv;
    }

    // Fallback to default DNS
    DEFAULT_DNS
        .iter()
        .map(|s| format!("nameserver {s}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// Render `/etc/resolv.conf` content from explicit DNS settings.
///
/// Emits one `nameserver` line per server, a single `search` line (when any
/// search domains are given), and a single `options` line (when any options are
/// given) — the layout Kubernetes' `DNSConfig` expects. Returns an empty string
/// when nothing is configured, so callers can fall back to a default.
pub fn render_resolv_conf(servers: &[String], searches: &[String], options: &[String]) -> String {
    let mut out = String::new();
    for server in servers {
        out.push_str("nameserver ");
        out.push_str(server);
        out.push('\n');
    }
    if !searches.is_empty() {
        out.push_str("search ");
        out.push_str(&searches.join(" "));
        out.push('\n');
    }
    if !options.is_empty() {
        out.push_str("options ");
        out.push_str(&options.join(" "));
        out.push('\n');
    }
    out
}

/// Try to read usable host DNS for guest inheritance.
///
/// Returns None if no non-loopback nameserver can be found (caller falls back
/// to built-in defaults). Mirrors Docker's `FilterResolvDNS` posture: never
/// copy `127.0.0.0/8` / `::1` into the guest.
fn read_host_resolv_conf() -> Option<String> {
    let primary = std::fs::read_to_string(HOST_RESOLV_CONF).ok();
    let upstream = if primary
        .as_deref()
        .is_some_and(resolv_content_has_loopback_nameserver)
    {
        std::fs::read_to_string(SYSTEMD_RESOLVED_UPSTREAM).ok()
    } else {
        None
    };

    select_host_nameserver_lines(primary.as_deref(), upstream.as_deref())
}

/// Pure selection used by [`read_host_resolv_conf`] (and unit tests).
///
/// When the primary file contains a loopback nameserver and an upstream file
/// yields at least one non-loopback nameserver, prefer the upstream. Otherwise
/// filter the primary. Empty after filtering → `None`.
fn select_host_nameserver_lines(
    primary_content: Option<&str>,
    upstream_content: Option<&str>,
) -> Option<String> {
    let primary = primary_content?;

    if resolv_content_has_loopback_nameserver(primary) {
        if let Some(upstream) = upstream_content {
            let filtered = filter_resolv_nameserver_lines(upstream);
            if !filtered.is_empty() {
                return Some(filtered.join("\n") + "\n");
            }
        }
    }

    let filtered = filter_resolv_nameserver_lines(primary);
    if filtered.is_empty() {
        None
    } else {
        Some(filtered.join("\n") + "\n")
    }
}

/// Drop loopback nameservers from resolv.conf content; keep only `nameserver` lines.
fn filter_resolv_nameserver_lines(content: &str) -> Vec<String> {
    content
        .lines()
        .filter_map(|line| {
            let addr = nameserver_address(line)?;
            if is_loopback_nameserver(addr) {
                None
            } else {
                Some(format!("nameserver {addr}"))
            }
        })
        .collect()
}

fn resolv_content_has_loopback_nameserver(content: &str) -> bool {
    content
        .lines()
        .filter_map(nameserver_address)
        .any(is_loopback_nameserver)
}

fn nameserver_address(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    // resolv.conf is case-sensitive for the keyword in practice; match Docker/glibc.
    let rest = trimmed.strip_prefix("nameserver")?.trim();
    if rest.is_empty() {
        return None;
    }
    rest.split_whitespace().next()
}

fn is_loopback_nameserver(addr: &str) -> bool {
    match addr.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => v4.is_loopback(),
        Ok(IpAddr::V6(v6)) => v6.is_loopback(),
        // Unparseable tokens are not treated as loopback; they also will not
        // be useful in-guest, but filtering them here would change inheritance
        // beyond the #455 contract (drop 127/8 and ::1 only).
        Err(_) => false,
    }
}

/// Generate /etc/hosts content for DNS service discovery.
///
/// Produces a hosts file with:
/// - localhost entry (127.0.0.1)
/// - the box's own IP and name
/// - peer entries for all other boxes on the same network
pub fn generate_hosts_file(
    own_ip: &str,
    own_name: &str,
    peers: &[(String, String)], // (ip, name)
) -> String {
    generate_hosts_file_with_entries(Some(own_ip), &[own_name.to_string()], peers, &[])
}

/// Generate `/etc/hosts` content with optional own aliases and static entries.
pub fn generate_hosts_file_with_entries(
    own_ip: Option<&str>,
    own_names: &[String],
    peers: &[(String, String)], // (ip, name)
    extra_hosts: &[HostEntry],
) -> String {
    let mut lines = Vec::new();
    lines.push("127.0.0.1 localhost".to_string());
    if !own_names.is_empty() {
        let own_names = own_names.join(" ");
        let own_ip = own_ip.unwrap_or("127.0.1.1");
        lines.push(format!("{} {}", own_ip, own_names));
    }
    for (ip, name) in peers {
        lines.push(format!("{} {}", ip, name));
    }
    for entry in extra_hosts {
        lines.push(format!("{} {}", entry.ip, entry.host));
    }
    lines.join("\n") + "\n"
}

/// Validate a hostname or DNS name accepted by a3s-box runtime options.
pub fn validate_hostname(hostname: &str) -> Result<(), String> {
    if hostname.is_empty() {
        return Err("hostname must not be empty".to_string());
    }
    if hostname.len() > 253 {
        return Err("hostname must be at most 253 characters".to_string());
    }
    if hostname.contains('\0') || hostname.chars().any(char::is_whitespace) {
        return Err("hostname must not contain whitespace or NUL bytes".to_string());
    }

    for label in hostname.trim_end_matches('.').split('.') {
        if label.is_empty() {
            return Err(format!("hostname '{hostname}' contains an empty label"));
        }
        if label.len() > 63 {
            return Err(format!(
                "hostname label '{label}' is longer than 63 characters"
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!(
                "hostname label '{label}' must not start or end with '-'"
            ));
        }
        if !label
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        {
            return Err(format!(
                "hostname label '{label}' contains unsupported characters"
            ));
        }
    }

    Ok(())
}

/// Parse a CLI `--add-host HOST:IP` value.
pub fn parse_add_host_entry(entry: &str) -> Result<HostEntry, String> {
    let (host, ip) = entry
        .split_once(':')
        .ok_or_else(|| format!("expected HOST:IP, got '{entry}'"))?;
    validate_hostname(host).map_err(|e| format!("invalid host '{host}': {e}"))?;
    let ip = ip.trim();
    if ip.is_empty() {
        return Err(format!("missing IP address in '{entry}'"));
    }
    ip.parse::<IpAddr>()
        .map_err(|_| format!("invalid IP address '{ip}' in '{entry}'"))?;

    Ok(HostEntry {
        host: host.to_string(),
        ip: ip.to_string(),
    })
}

/// Parse repeated CLI `--add-host` values.
pub fn parse_add_host_entries(entries: &[String]) -> Result<Vec<HostEntry>, String> {
    entries
        .iter()
        .map(|entry| parse_add_host_entry(entry))
        .collect()
}

/// Parse one DNS server as any IP address.
///
/// Default TSI hijacks AF_INET6, so IPv6 nameservers remain valid there.
/// Bridge setup uses [`parse_ipv4_dns_server`] instead.
pub fn parse_dns_server(server: &str) -> Result<IpAddr, String> {
    let trimmed = server.trim();
    if trimmed.is_empty() {
        return Err("DNS server must not be empty".to_string());
    }
    trimmed
        .parse::<IpAddr>()
        .map_err(|_| format!("invalid DNS server address '{trimmed}'"))
}

/// Parse every configured DNS server as an IP address, preserving order.
pub fn parse_dns_servers(servers: &[String]) -> Result<Vec<IpAddr>, String> {
    servers
        .iter()
        .map(|server| parse_dns_server(server))
        .collect()
}

/// Parse one DNS server as IPv4 for Bridge / passt / netproxy.
///
/// Those paths and the untrusted egress profile are IPv4-only. Silently
/// dropping an IPv6 value would leave guest `resolv.conf` disagreeing with the
/// host proxy.
pub fn parse_ipv4_dns_server(server: &str) -> Result<std::net::Ipv4Addr, String> {
    match parse_dns_server(server)? {
        IpAddr::V4(v4) => Ok(v4),
        IpAddr::V6(_) => Err(format!(
            "DNS server '{}' is IPv6; Bridge networking requires IPv4 DNS",
            server.trim()
        )),
    }
}

/// Parse every configured DNS server as IPv4, preserving order.
pub fn parse_ipv4_dns_servers(servers: &[String]) -> Result<Vec<std::net::Ipv4Addr>, String> {
    servers
        .iter()
        .map(|server| parse_ipv4_dns_server(server))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_custom_dns() {
        let result = generate_resolv_conf(&["1.1.1.1".to_string(), "1.0.0.1".to_string()]);
        assert_eq!(result, "nameserver 1.1.1.1\nnameserver 1.0.0.1\n");
    }

    #[test]
    fn test_render_resolv_conf() {
        let servers = vec!["10.10.10.10".to_string(), "10.10.10.11".to_string()];
        let searches = vec!["a.com".to_string(), "b.com".to_string()];
        let options = vec!["ndots:5".to_string(), "timeout:2".to_string()];
        assert_eq!(
            render_resolv_conf(&servers, &searches, &options),
            "nameserver 10.10.10.10\nnameserver 10.10.10.11\nsearch a.com b.com\noptions ndots:5 timeout:2\n"
        );
        // Servers only — no search/options lines.
        assert_eq!(
            render_resolv_conf(&["1.1.1.1".to_string()], &[], &[]),
            "nameserver 1.1.1.1\n"
        );
        // Nothing configured -> empty (caller falls back to a default).
        assert_eq!(render_resolv_conf(&[], &[], &[]), "");
    }

    #[test]
    fn test_empty_dns_uses_host_or_default() {
        let result = generate_resolv_conf(&[]);
        // Should contain at least one nameserver line
        assert!(result.contains("nameserver"));
        // Guest inheritance must never surface the systemd-resolved stub.
        assert!(
            !result.contains("127.0.0.53"),
            "inherited resolv.conf must not keep loopback stub: {result}"
        );
        for line in result.lines() {
            if let Some(addr) = nameserver_address(line) {
                assert!(
                    !is_loopback_nameserver(addr),
                    "loopback nameserver leaked into guest resolv.conf: {result}"
                );
            }
        }
    }

    #[test]
    fn test_single_dns() {
        let result = generate_resolv_conf(&["9.9.9.9".to_string()]);
        assert_eq!(result, "nameserver 9.9.9.9\n");
    }

    #[test]
    fn filter_drops_ipv4_and_ipv6_loopback_nameservers() {
        let content = "\
# stub
nameserver 127.0.0.53
nameserver 127.0.0.1
nameserver ::1
nameserver 1.1.1.1
search example.com
";
        assert_eq!(
            filter_resolv_nameserver_lines(content),
            vec!["nameserver 1.1.1.1".to_string()]
        );
    }

    #[test]
    fn stub_primary_prefers_systemd_upstream_nameservers() {
        let primary = "nameserver 127.0.0.53\noptions edns0\n";
        let upstream = "nameserver 10.1.7.5\nnameserver 10.1.7.6\n";
        assert_eq!(
            select_host_nameserver_lines(Some(primary), Some(upstream)).as_deref(),
            Some("nameserver 10.1.7.5\nnameserver 10.1.7.6\n")
        );
    }

    #[test]
    fn stub_primary_without_upstream_falls_through_to_none() {
        let primary = "nameserver 127.0.0.53\n";
        assert_eq!(select_host_nameserver_lines(Some(primary), None), None);
        assert_eq!(
            select_host_nameserver_lines(Some(primary), Some("# empty upstream\n")),
            None
        );
    }

    #[test]
    fn non_stub_primary_keeps_public_nameservers() {
        let primary = "nameserver 8.8.8.8\nnameserver 8.8.4.4\n";
        assert_eq!(
            select_host_nameserver_lines(Some(primary), Some("nameserver 10.0.0.1\n")).as_deref(),
            Some("nameserver 8.8.8.8\nnameserver 8.8.4.4\n")
        );
    }

    #[test]
    fn mixed_primary_without_usable_upstream_keeps_non_loopback() {
        let primary = "nameserver 127.0.0.53\nnameserver 9.9.9.9\n";
        assert_eq!(
            select_host_nameserver_lines(Some(primary), None).as_deref(),
            Some("nameserver 9.9.9.9\n")
        );
    }

    #[test]
    fn loopback_detection_covers_entire_127_slash_8() {
        assert!(is_loopback_nameserver("127.0.0.53"));
        assert!(is_loopback_nameserver("127.1.2.3"));
        assert!(is_loopback_nameserver("::1"));
        assert!(!is_loopback_nameserver("10.1.7.5"));
        assert!(!is_loopback_nameserver("8.8.8.8"));
    }

    #[test]
    fn path_constants_match_linux_layout() {
        // Guard against accidental drift of the documented paths.
        assert_eq!(HOST_RESOLV_CONF, "/etc/resolv.conf");
        assert_eq!(
            SYSTEMD_RESOLVED_UPSTREAM,
            "/run/systemd/resolve/resolv.conf"
        );
    }

    // --- generate_hosts_file tests ---

    #[test]
    fn test_hosts_file_no_peers() {
        let result = generate_hosts_file("10.88.0.2", "web", &[]);
        assert_eq!(result, "127.0.0.1 localhost\n10.88.0.2 web\n");
    }

    #[test]
    fn test_hosts_file_with_peers() {
        let peers = vec![
            ("10.88.0.3".to_string(), "api".to_string()),
            ("10.88.0.4".to_string(), "db".to_string()),
        ];
        let result = generate_hosts_file("10.88.0.2", "web", &peers);
        assert_eq!(
            result,
            "127.0.0.1 localhost\n10.88.0.2 web\n10.88.0.3 api\n10.88.0.4 db\n"
        );
    }

    #[test]
    fn test_hosts_file_own_entry_present() {
        let result = generate_hosts_file("192.168.1.5", "mybox", &[]);
        assert!(result.contains("192.168.1.5 mybox"));
        assert!(result.contains("127.0.0.1 localhost"));
    }

    #[test]
    fn test_hosts_file_deterministic_output() {
        let peers = vec![
            ("10.0.0.2".to_string(), "a".to_string()),
            ("10.0.0.3".to_string(), "b".to_string()),
        ];
        let r1 = generate_hosts_file("10.0.0.1", "self", &peers);
        let r2 = generate_hosts_file("10.0.0.1", "self", &peers);
        assert_eq!(r1, r2);
    }

    #[test]
    fn test_hosts_file_with_hostname_without_ip() {
        let result = generate_hosts_file_with_entries(None, &["box1".to_string()], &[], &[]);
        assert_eq!(result, "127.0.0.1 localhost\n127.0.1.1 box1\n");
    }

    #[test]
    fn test_hosts_file_with_extra_hosts() {
        let result = generate_hosts_file_with_entries(
            Some("10.88.0.2"),
            &["web".to_string(), "custom".to_string()],
            &[],
            &[HostEntry {
                host: "db.local".to_string(),
                ip: "10.88.0.10".to_string(),
            }],
        );
        assert_eq!(
            result,
            "127.0.0.1 localhost\n10.88.0.2 web custom\n10.88.0.10 db.local\n"
        );
    }

    #[test]
    fn test_validate_hostname() {
        validate_hostname("web").unwrap();
        validate_hostname("web-1.example").unwrap();
        assert!(validate_hostname("").is_err());
        assert!(validate_hostname("-web").is_err());
        assert!(validate_hostname("web_1").is_err());
        assert!(validate_hostname("bad host").is_err());
    }

    #[test]
    fn test_parse_add_host_entry() {
        let entry = parse_add_host_entry("db.local:10.88.0.10").unwrap();
        assert_eq!(entry.host, "db.local");
        assert_eq!(entry.ip, "10.88.0.10");

        let entry = parse_add_host_entry("v6:2001:db8::1").unwrap();
        assert_eq!(entry.host, "v6");
        assert_eq!(entry.ip, "2001:db8::1");

        assert!(parse_add_host_entry("missing-ip:").is_err());
        assert!(parse_add_host_entry("bad_host:10.0.0.1").is_err());
        assert!(parse_add_host_entry("host:not-an-ip").is_err());
    }

    #[test]
    fn parse_dns_servers_accepts_v4_and_v6_rejects_garbage() {
        assert_eq!(
            parse_dns_servers(&["8.8.8.8".into(), "2001:db8::1".into()]).unwrap(),
            vec![
                IpAddr::V4(std::net::Ipv4Addr::new(8, 8, 8, 8)),
                IpAddr::V6("2001:db8::1".parse().unwrap())
            ]
        );
        assert!(parse_dns_server("not-an-ip").is_err());
        assert!(parse_dns_server("").is_err());
    }

    #[test]
    fn parse_ipv4_dns_servers_accepts_v4_rejects_v6_and_garbage() {
        assert_eq!(
            parse_ipv4_dns_servers(&["8.8.8.8".into(), "1.1.1.1".into()]).unwrap(),
            vec![
                std::net::Ipv4Addr::new(8, 8, 8, 8),
                std::net::Ipv4Addr::new(1, 1, 1, 1)
            ]
        );
        let v6 = parse_ipv4_dns_server("2001:db8::1").unwrap_err();
        assert!(v6.contains("IPv6"), "{v6}");
        assert!(v6.contains("Bridge"), "{v6}");
        assert!(parse_ipv4_dns_server("not-an-ip").is_err());
        assert!(parse_ipv4_dns_server("").is_err());
        assert!(parse_ipv4_dns_server("   ").is_err());
    }
}
