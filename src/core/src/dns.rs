//! DNS configuration helpers for guest rootfs.
//!
//! Generates /etc/resolv.conf content from user-specified DNS servers,
//! host configuration, or sensible defaults.

/// Default DNS servers (Google Public DNS).
const DEFAULT_DNS: &[&str] = &["8.8.8.8", "8.8.4.4"];

/// Generate resolv.conf content for the guest rootfs.
///
/// Resolution order:
/// 1. If `custom_dns` is non-empty, use those servers
/// 2. Otherwise, try to read the host's /etc/resolv.conf
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

/// Try to read the host's /etc/resolv.conf.
///
/// Returns None if the file doesn't exist, is unreadable, or contains
/// no nameserver entries (e.g., only comments).
fn read_host_resolv_conf() -> Option<String> {
    let content = std::fs::read_to_string("/etc/resolv.conf").ok()?;

    // Filter to only nameserver lines (skip comments, search, domain, etc.)
    let nameservers: Vec<&str> = content
        .lines()
        .filter(|line| line.trim_start().starts_with("nameserver"))
        .collect();

    if nameservers.is_empty() {
        return None;
    }

    Some(nameservers.join("\n") + "\n")
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
    let mut lines = Vec::new();
    lines.push("127.0.0.1 localhost".to_string());
    lines.push(format!("{} {}", own_ip, own_name));
    for (ip, name) in peers {
        lines.push(format!("{} {}", ip, name));
    }
    lines.join("\n") + "\n"
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
    fn test_empty_dns_uses_host_or_default() {
        let result = generate_resolv_conf(&[]);
        // Should contain at least one nameserver line
        assert!(result.contains("nameserver"));
    }

    #[test]
    fn test_single_dns() {
        let result = generate_resolv_conf(&["9.9.9.9".to_string()]);
        assert_eq!(result, "nameserver 9.9.9.9\n");
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
}
