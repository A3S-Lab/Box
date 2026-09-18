//! Default MicroVM egress posture for untrusted workloads (Axis C).
//!
//! First-match product intent (not a CNI claim):
//! - allow the attached NetworkStore / bridge CIDR (peers + gateway);
//! - allow public unicast IPv4;
//! - deny loopback, link-local (incl. cloud metadata `169.254.169.254`),
//!   RFC1918 outside the attached CIDR, and CGNAT `100.64/10`.
//!
//! Does not invent DNS policy, TLS MITM, or Sandbox bridge GA.

use std::net::Ipv4Addr;

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
    if dest.is_loopback() || dest.is_unspecified() || dest.is_broadcast() {
        return true;
    }
    if is_link_local(dest) {
        return true;
    }
    if let Some((network_addr, prefix_len)) = attached_cidr {
        if ipv4_in_prefix(network_addr, prefix_len, dest) {
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

/// IPv4 destination of an Ethernet frame, if the payload is IPv4.
pub(crate) fn ethernet_ipv4_destination(frame: &[u8]) -> Option<Ipv4Addr> {
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
    Some(Ipv4Addr::new(ip[16], ip[17], ip[18], ip[19]))
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
}
