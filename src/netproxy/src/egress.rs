//! Default MicroVM egress posture for untrusted workloads (Axis C).
//!
//! First-match operator rules (CIDR / protocol / port) on the network object
//! are evaluated before this default. Domain rules are rejected at parse time.
//! Does not invent DNS policy, TLS MITM, CNI, or Sandbox bridge GA.
//! IPv6 is not matched.

use std::net::Ipv4Addr;

use a3s_box_core::{EgressMatchRule, PolicyAction};

/// Whether an outbound IPv4 destination is denied by the default untrusted
/// MicroVM egress profile.
///
/// `attached_cidr` is `(any_address_in_subnet, prefix_len)` for the guest's
/// attached product network. When `None` (no bridge CIDR), all RFC1918 /
/// CGNAT destinations are denied.
pub fn default_untrusted_egress_denied(
    dest: Ipv4Addr,
    attached_cidr: Option<(Ipv4Addr, u8)>,
) -> bool {
    untrusted_egress_denied(dest, 0, None, attached_cidr, &[])
}

/// First-match operator rules, then the default untrusted profile.
///
/// `protocol` is the IPv4 protocol number (`6` TCP, `17` UDP). `dest_port` is
/// set only for TCP/UDP. A rule with a port does not match ICMP or other
/// protocols that have no port.
pub fn untrusted_egress_denied(
    dest: Ipv4Addr,
    protocol: u8,
    dest_port: Option<u16>,
    attached_cidr: Option<(Ipv4Addr, u8)>,
    rules: &[EgressMatchRule],
) -> bool {
    for rule in rules {
        if rule.matches(dest, protocol, dest_port) {
            return rule.action == PolicyAction::Deny;
        }
    }
    default_profile_denied(dest, attached_cidr)
}

fn default_profile_denied(dest: Ipv4Addr, attached_cidr: Option<(Ipv4Addr, u8)>) -> bool {
    if dest.is_loopback() || dest.is_unspecified() || dest.is_broadcast() {
        return true;
    }
    if is_link_local(dest) {
        return true;
    }
    if let Some((network_addr, prefix_len)) = attached_cidr {
        if a3s_box_core::ipv4_in_prefix(network_addr, prefix_len, dest) {
            return false;
        }
    }
    is_rfc1918(dest) || is_cgnat(dest) || dest.is_multicast()
}

fn is_link_local(addr: Ipv4Addr) -> bool {
    // 169.254.0.0/16 — includes AWS/GCP/Azure metadata at 169.254.169.254.
    ipv4_in_prefix(Ipv4Addr::new(169, 254, 0, 0), 16, addr)
}

fn is_rfc1918(addr: Ipv4Addr) -> bool {
    ipv4_in_prefix(Ipv4Addr::new(10, 0, 0, 0), 8, addr)
        || ipv4_in_prefix(Ipv4Addr::new(172, 16, 0, 0), 12, addr)
        || ipv4_in_prefix(Ipv4Addr::new(192, 168, 0, 0), 16, addr)
}

fn is_cgnat(addr: Ipv4Addr) -> bool {
    ipv4_in_prefix(Ipv4Addr::new(100, 64, 0, 0), 10, addr)
}

pub(crate) fn ipv4_in_prefix(network: Ipv4Addr, prefix_len: u8, addr: Ipv4Addr) -> bool {
    let prefix_len = prefix_len.min(32);
    let mask = if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - prefix_len)
    };
    (u32::from(network) & mask) == (u32::from(addr) & mask)
}

/// IPv4 view of an Ethernet payload, if the ethertype is IPv4.
pub(crate) struct Ipv4View {
    pub dest: Ipv4Addr,
    pub protocol: u8,
    pub dest_port: Option<u16>,
}

pub(crate) fn ethernet_ipv4_view(frame: &[u8]) -> Option<Ipv4View> {
    if frame.len() < 14 + 20 {
        return None;
    }
    if frame[12..14] != [0x08, 0x00] {
        return None;
    }
    let ip = &frame[14..];
    let ihl = (ip[0] & 0x0f) as usize * 4;
    if ihl < 20 || frame.len() < 14 + ihl {
        return None;
    }
    let protocol = ip[9];
    let dest_port = if protocol == 6 || protocol == 17 {
        let payload = &frame[14 + ihl..];
        if payload.len() >= 4 {
            Some(u16::from_be_bytes([payload[2], payload[3]]))
        } else {
            None
        }
    } else {
        None
    };
    Some(Ipv4View {
        dest: Ipv4Addr::new(ip[16], ip[17], ip[18], ip[19]),
        protocol,
        dest_port,
    })
}

/// IPv4 destination of an Ethernet frame, if the payload is IPv4.
pub(crate) fn ethernet_ipv4_destination(frame: &[u8]) -> Option<Ipv4Addr> {
    ethernet_ipv4_view(frame).map(|view| view.dest)
}

pub(crate) fn load_egress_rules(
    networks_json: Option<&std::path::Path>,
    network_name: Option<&str>,
) -> Vec<EgressMatchRule> {
    let (Some(path), Some(name)) = (networks_json, network_name) else {
        return Vec::new();
    };
    match a3s_box_core::load_network_egress_rules(path, name) {
        Ok(rules) => rules,
        Err(error) => {
            tracing::error!(
                %error,
                "MicroVM egress rules failed to load; denying IPv4 egress"
            );
            vec![EgressMatchRule::deny_all()]
        }
    }
}
/// How one guest Ethernet frame should be treated.
pub(crate) enum EgressLeg {
    /// An operator rule denied this IPv4 destination. Drop peers and gateway.
    Drop,
    /// An operator rule allowed it. Deliver peers and the gateway.
    Allow,
    /// No operator rule matched. Peers stay on the switch; the gateway uses
    /// the default untrusted profile.
    Default,
}

pub(crate) fn classify_ethernet_egress(frame: &[u8], rules: &[EgressMatchRule]) -> EgressLeg {
    let Some(view) = ethernet_ipv4_view(frame) else {
        return EgressLeg::Default;
    };
    for rule in rules {
        if rule.matches(view.dest, view.protocol, view.dest_port) {
            return if rule.action == PolicyAction::Deny {
                EgressLeg::Drop
            } else {
                EgressLeg::Allow
            };
        }
    }
    EgressLeg::Default
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_loopback_link_local_and_metadata() {
        assert!(default_untrusted_egress_denied(
            Ipv4Addr::new(127, 0, 0, 1),
            None
        ));
        assert!(default_untrusted_egress_denied(
            Ipv4Addr::new(169, 254, 169, 254),
            None
        ));
        assert!(default_untrusted_egress_denied(
            Ipv4Addr::new(169, 254, 1, 1),
            Some((Ipv4Addr::new(10, 88, 0, 1), 24))
        ));
    }

    #[test]
    fn denies_foreign_private_allows_attached_cidr_and_public() {
        let cidr = Some((Ipv4Addr::new(10, 88, 0, 2), 24));
        assert!(!default_untrusted_egress_denied(
            Ipv4Addr::new(10, 88, 0, 1),
            cidr
        ));
        assert!(!default_untrusted_egress_denied(
            Ipv4Addr::new(10, 88, 0, 9),
            cidr
        ));
        assert!(default_untrusted_egress_denied(
            Ipv4Addr::new(10, 0, 0, 1),
            cidr
        ));
        assert!(default_untrusted_egress_denied(
            Ipv4Addr::new(192, 168, 1, 1),
            cidr
        ));
        assert!(default_untrusted_egress_denied(
            Ipv4Addr::new(100, 64, 0, 1),
            cidr
        ));
        assert!(!default_untrusted_egress_denied(
            Ipv4Addr::new(8, 8, 8, 8),
            cidr
        ));
        assert!(!default_untrusted_egress_denied(
            Ipv4Addr::new(1, 1, 1, 1),
            None
        ));
        assert!(default_untrusted_egress_denied(
            Ipv4Addr::new(10, 0, 0, 1),
            None
        ));
    }

    #[test]
    fn ethernet_ipv4_destination_reads_dst() {
        let mut frame = vec![0u8; 34];
        frame[12] = 0x08;
        frame[13] = 0x00;
        frame[14] = 0x45;
        frame[30] = 8;
        frame[31] = 8;
        frame[32] = 8;
        frame[33] = 8;
        assert_eq!(
            ethernet_ipv4_destination(&frame),
            Some(Ipv4Addr::new(8, 8, 8, 8))
        );
    }

    #[test]
    fn first_match_overrides_default_profile() {
        let cidr = Some((Ipv4Addr::new(10, 88, 0, 2), 24));
        let allow_private = EgressMatchRule::parse("allow:10.0.0.0/8").unwrap();
        assert!(!untrusted_egress_denied(
            Ipv4Addr::new(10, 0, 0, 1),
            6,
            Some(443),
            cidr,
            &[allow_private]
        ));
        let deny_public = EgressMatchRule::parse("deny:1.1.1.1/32:tcp:443").unwrap();
        assert!(untrusted_egress_denied(
            Ipv4Addr::new(1, 1, 1, 1),
            6,
            Some(443),
            cidr,
            std::slice::from_ref(&deny_public)
        ));
        assert!(!untrusted_egress_denied(
            Ipv4Addr::new(1, 1, 1, 1),
            6,
            Some(80),
            cidr,
            std::slice::from_ref(&deny_public)
        ));
        let deny_then_allow = [
            EgressMatchRule::parse("deny:8.8.8.8/32").unwrap(),
            EgressMatchRule::parse("allow:8.8.8.8/32").unwrap(),
        ];
        assert!(untrusted_egress_denied(
            Ipv4Addr::new(8, 8, 8, 8),
            17,
            Some(53),
            cidr,
            &deny_then_allow
        ));
    }
}
