//! Default MicroVM egress posture for untrusted workloads (Axis C).
//!
//! First-match operator rules (CIDR / protocol / port) on the network object
//! are evaluated before this default. Domain rules are rejected at parse time.
//! IPv6 Ethernet frames are dropped: NetworkStore and this profile are IPv4-only,
//! so leaving IPv6 through would bypass link-local and metadata denial. A single
//! 802.1Q or 802.1ad tag does not hide that header. The bridge gateway address
//! is denied even inside the attached CIDR: passt rewrites it to host loopback.
//! This is not an IPv6 policy, VLAN policy, DNS policy, TLS MITM, CNI, or
//! Sandbox bridge GA.

use std::net::Ipv4Addr;

use a3s_box_core::{EgressMatchRule, PolicyAction};

/// Attached network used by the default untrusted profile.
///
/// `gateway` is denied even when it is inside `attached_cidr`, because passt
/// rewrites that address to host loopback. `None` gateway keeps the CIDR allow.
#[derive(Clone, Copy, Debug, Default)]
pub struct UntrustedEgressScope {
    pub attached_cidr: Option<(Ipv4Addr, u8)>,
    pub gateway: Option<Ipv4Addr>,
}
///
/// `attached_cidr` is `(any_address_in_subnet, prefix_len)` for the guest's
/// attached product network. When `None` (no bridge CIDR), all RFC1918 /
/// CGNAT destinations are denied. The attached gateway is not part of this
/// two-argument form; use [`default_untrusted_egress_denied_with_gateway`].
#[cfg(test)]
pub fn default_untrusted_egress_denied(
    dest: Ipv4Addr,
    attached_cidr: Option<(Ipv4Addr, u8)>,
) -> bool {
    default_untrusted_egress_denied_with_gateway(dest, attached_cidr, None)
}

/// Same profile as [`default_untrusted_egress_denied`], plus the bridge gateway.
///
/// passt's default `--map-host-loopback` is the guest gateway, and it rewrites
/// that destination to host `127.0.0.1`. The address is inside the attached
/// CIDR, so the CIDR allow would otherwise open host loopback. Packets that
/// only use the gateway as the Ethernet next hop keep their real destination
/// and are unchanged. `None` keeps the previous CIDR allow, including `.1`.
pub(crate) fn default_untrusted_egress_denied_with_gateway(
    dest: Ipv4Addr,
    attached_cidr: Option<(Ipv4Addr, u8)>,
    gateway: Option<Ipv4Addr>,
) -> bool {
    untrusted_egress_denied_with_gateway(dest, 0, None, attached_cidr, &[], gateway)
}

/// First-match operator rules, then the default untrusted profile.
///
/// `protocol` is the IPv4 protocol number (`6` TCP, `17` UDP). `dest_port` is
/// set only for TCP/UDP. A rule with a port does not match ICMP or other
/// protocols that have no port. Gateway denial is off in this test helper;
/// production calls [`untrusted_egress_denied_with_gateway`].
#[cfg(test)]
pub fn untrusted_egress_denied(
    dest: Ipv4Addr,
    protocol: u8,
    dest_port: Option<u16>,
    attached_cidr: Option<(Ipv4Addr, u8)>,
    rules: &[EgressMatchRule],
) -> bool {
    untrusted_egress_denied_with_gateway(dest, protocol, dest_port, attached_cidr, rules, None)
}

pub(crate) fn untrusted_egress_denied_with_gateway(
    dest: Ipv4Addr,
    protocol: u8,
    dest_port: Option<u16>,
    attached_cidr: Option<(Ipv4Addr, u8)>,
    rules: &[EgressMatchRule],
    gateway: Option<Ipv4Addr>,
) -> bool {
    for rule in rules {
        if rule.matches(dest, protocol, dest_port) {
            return rule.action == PolicyAction::Deny;
        }
    }
    default_profile_denied(dest, attached_cidr, gateway)
}

fn default_profile_denied(
    dest: Ipv4Addr,
    attached_cidr: Option<(Ipv4Addr, u8)>,
    gateway: Option<Ipv4Addr>,
) -> bool {
    if gateway == Some(dest) {
        return true;
    }
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

/// Offset of the payload ethertype after at most two 802.1Q (`0x8100`) or
/// 802.1ad (`0x88a8`) tags. A guest can shift IPv4 or IPv6 past byte 12;
/// the profile still has to see the inner header. This is not a VLAN policy.
fn payload_ethertype_offset(frame: &[u8]) -> Option<usize> {
    if frame.len() < 14 {
        return None;
    }
    let mut offset = 12usize;
    for _ in 0..2 {
        if frame.len() < offset + 4 {
            return Some(offset);
        }
        let tpid = u16::from_be_bytes([frame[offset], frame[offset + 1]]);
        if tpid == 0x8100 || tpid == 0x88a8 {
            offset += 4;
            continue;
        }
        return Some(offset);
    }
    if frame.len() < offset + 2 {
        return None;
    }
    Some(offset)
}

pub(crate) fn ethernet_ipv4_view(frame: &[u8]) -> Option<Ipv4View> {
    let ethertype_at = payload_ethertype_offset(frame)?;
    if frame.len() < ethertype_at + 2 + 20 {
        return None;
    }
    if frame[ethertype_at..ethertype_at + 2] != [0x08, 0x00] {
        return None;
    }
    let ip_at = ethertype_at + 2;
    let ip = &frame[ip_at..];
    let ihl = (ip[0] & 0x0f) as usize * 4;
    if ihl < 20 || frame.len() < ip_at + ihl {
        return None;
    }
    let protocol = ip[9];
    let dest_port = if protocol == 6 || protocol == 17 {
        let payload = &frame[ip_at + ihl..];
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

/// IPv6 is not a product path. NetworkStore, DNS answers, and the untrusted
/// profile are IPv4-only, so an IPv6 frame (including link-local and metadata)
/// is dropped before peer switch and host egress. One 802.1Q or 802.1ad tag,
/// or QinQ, does not hide that ethertype. A third VLAN tag is also dropped:
/// it is not IPv4 or ARP, and forwarding it would hide the inner header.
/// ARP is not IPv6.
pub(crate) fn ipv6_egress_denied(frame: &[u8]) -> bool {
    let Some(ethertype_at) = payload_ethertype_offset(frame) else {
        return false;
    };
    if frame.len() < ethertype_at + 2 {
        return false;
    }
    let ethertype = u16::from_be_bytes([frame[ethertype_at], frame[ethertype_at + 1]]);
    ethertype == 0x86dd || ethertype == 0x8100 || ethertype == 0x88a8
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
    fn denies_gateway_inside_attached_cidr_unless_operator_allows_it() {
        let cidr = Some((Ipv4Addr::new(10, 88, 0, 2), 24));
        let gateway = Some(Ipv4Addr::new(10, 88, 0, 1));
        assert!(default_untrusted_egress_denied_with_gateway(
            Ipv4Addr::new(10, 88, 0, 1),
            cidr,
            gateway
        ));
        assert!(!default_untrusted_egress_denied_with_gateway(
            Ipv4Addr::new(10, 88, 0, 9),
            cidr,
            gateway
        ));
        assert!(!default_untrusted_egress_denied_with_gateway(
            Ipv4Addr::new(8, 8, 8, 8),
            cidr,
            gateway
        ));
        let allow_gateway = EgressMatchRule::parse("allow:10.88.0.1/32").unwrap();
        assert!(!untrusted_egress_denied_with_gateway(
            Ipv4Addr::new(10, 88, 0, 1),
            6,
            Some(80),
            cidr,
            std::slice::from_ref(&allow_gateway),
            gateway
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

    #[test]
    fn ipv6_frames_are_denied_and_arp_is_not() {
        let mut ipv6 = vec![0u8; 14];
        ipv6[12] = 0x86;
        ipv6[13] = 0xdd;
        assert!(ipv6_egress_denied(&ipv6));
        let mut arp = vec![0u8; 14];
        arp[12] = 0x08;
        arp[13] = 0x06;
        assert!(!ipv6_egress_denied(&arp));
        let mut ipv4 = vec![0u8; 14];
        ipv4[12] = 0x08;
        ipv4[13] = 0x00;
        assert!(!ipv6_egress_denied(&ipv4));
        assert!(!ipv6_egress_denied(&[0u8; 13]));
    }

    #[test]
    fn vlan_tag_does_not_hide_ipv6_or_metadata() {
        let mut tagged_ipv6 = vec![0u8; 18];
        tagged_ipv6[12] = 0x81;
        tagged_ipv6[13] = 0x00;
        tagged_ipv6[16] = 0x86;
        tagged_ipv6[17] = 0xdd;
        assert!(ipv6_egress_denied(&tagged_ipv6));

        let mut qinq_ipv6 = vec![0u8; 22];
        qinq_ipv6[12] = 0x88;
        qinq_ipv6[13] = 0xa8;
        qinq_ipv6[16] = 0x81;
        qinq_ipv6[17] = 0x00;
        qinq_ipv6[20] = 0x86;
        qinq_ipv6[21] = 0xdd;
        assert!(ipv6_egress_denied(&qinq_ipv6));

        // A third tag is not IPv4 or ARP. Forwarding it would hide the inner header.
        let mut triple = vec![0u8; 26];
        triple[12] = 0x88;
        triple[13] = 0xa8;
        triple[16] = 0x81;
        triple[17] = 0x00;
        triple[20] = 0x81;
        triple[21] = 0x00;
        triple[24] = 0x08;
        triple[25] = 0x00;
        assert!(ipv6_egress_denied(&triple));

        let mut tagged_ipv4 = vec![0u8; 38];
        tagged_ipv4[12] = 0x81;
        tagged_ipv4[13] = 0x00;
        tagged_ipv4[16] = 0x08;
        tagged_ipv4[17] = 0x00;
        tagged_ipv4[18] = 0x45;
        tagged_ipv4[34] = 169;
        tagged_ipv4[35] = 254;
        tagged_ipv4[36] = 169;
        tagged_ipv4[37] = 254;
        assert_eq!(
            ethernet_ipv4_destination(&tagged_ipv4),
            Some(Ipv4Addr::new(169, 254, 169, 254))
        );
        assert!(default_untrusted_egress_denied(
            ethernet_ipv4_destination(&tagged_ipv4).unwrap(),
            Some((Ipv4Addr::new(10, 88, 0, 1), 24))
        ));

        let mut tagged_arp = vec![0u8; 18];
        tagged_arp[12] = 0x81;
        tagged_arp[13] = 0x00;
        tagged_arp[16] = 0x08;
        tagged_arp[17] = 0x06;
        assert!(!ipv6_egress_denied(&tagged_arp));
    }

    #[test]
    fn vlan_tag_does_not_hide_tcp_port_from_operator_rules() {
        let mut frame = vec![0u8; 42];
        frame[12] = 0x81;
        frame[13] = 0x00;
        frame[16] = 0x08;
        frame[17] = 0x00;
        frame[18] = 0x45;
        frame[27] = 6;
        frame[34] = 1;
        frame[35] = 1;
        frame[36] = 1;
        frame[37] = 1;
        frame[40] = 0x01;
        frame[41] = 0xbb;
        let view = ethernet_ipv4_view(&frame).expect("vlan tcp view");
        assert_eq!(view.dest, Ipv4Addr::new(1, 1, 1, 1));
        assert_eq!(view.protocol, 6);
        assert_eq!(view.dest_port, Some(443));
        assert!(matches!(
            classify_ethernet_egress(
                &frame,
                &[EgressMatchRule::parse("deny:1.1.1.1/32:tcp:443").unwrap()]
            ),
            EgressLeg::Drop
        ));
        assert!(matches!(
            classify_ethernet_egress(
                &frame,
                &[EgressMatchRule::parse("deny:1.1.1.1/32:tcp:80").unwrap()]
            ),
            EgressLeg::Default
        ));
    }
}
