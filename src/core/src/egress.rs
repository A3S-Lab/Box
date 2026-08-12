//! Immutable egress-policy compilation and side-effect-free decisions.
//!
//! This module defines authorization semantics only. It never resolves DNS or
//! creates a socket; runtime proxies and packet boundaries consume its typed,
//! generation-independent decisions before performing those side effects.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use serde::{Deserialize, Serialize};
use url::Host;

use crate::error::{BoxError, Result};
use crate::security_policy::{
    normalize_host, EgressHttpScheme, EgressPolicy, EgressPolicyLimits, EgressProtocol,
};

/// Stable schema recorded with a runtime egress decision.
pub const EGRESS_DECISION_SCHEMA_V1: &str = "a3s.box.egress-decision.v1";

/// Protocol represented by one policy decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EgressDecisionProtocol {
    Http,
    Https,
    Tcp,
    Udp,
}

impl From<EgressHttpScheme> for EgressDecisionProtocol {
    fn from(value: EgressHttpScheme) -> Self {
        match value {
            EgressHttpScheme::Http => Self::Http,
            EgressHttpScheme::Https => Self::Https,
        }
    }
}

impl From<EgressProtocol> for EgressDecisionProtocol {
    fn from(value: EgressProtocol) -> Self {
        match value {
            EgressProtocol::Tcp => Self::Tcp,
            EgressProtocol::Udp => Self::Udp,
        }
    }
}

/// Redacted destination identity. No URL path, query, user information, or
/// request header can be represented by this type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EgressDecisionDestination {
    Hostname { hostname: String },
    Ip { address: IpAddr },
    ResolvedHostname { hostname: String, address: IpAddr },
    Invalid,
}

/// Stable reason category emitted by the policy evaluator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressDecisionReason {
    Unrestricted,
    DenyAll,
    HttpRuleMatched,
    IpRuleMatched,
    HttpAndIpRuleMatched,
    HostnameNotAllowed,
    IpNotAllowed,
    InvalidDestination,
    ResolutionMismatch,
    ResolvedAddressRequiresIpRule,
}

/// Authorization result returned before any connection is created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressEvaluation {
    pub schema: String,
    pub allowed: bool,
    pub reason: EgressDecisionReason,
    pub protocol: EgressDecisionProtocol,
    pub destination: EgressDecisionDestination,
    pub port: u16,
}

impl EgressEvaluation {
    pub const fn is_allowed(&self) -> bool {
        self.allowed
    }

    fn allowed(
        reason: EgressDecisionReason,
        protocol: EgressDecisionProtocol,
        destination: EgressDecisionDestination,
        port: u16,
    ) -> Self {
        Self::new(true, reason, protocol, destination, port)
    }

    fn denied(
        reason: EgressDecisionReason,
        protocol: EgressDecisionProtocol,
        destination: EgressDecisionDestination,
        port: u16,
    ) -> Self {
        Self::new(false, reason, protocol, destination, port)
    }

    fn new(
        allowed: bool,
        reason: EgressDecisionReason,
        protocol: EgressDecisionProtocol,
        destination: EgressDecisionDestination,
        port: u16,
    ) -> Self {
        Self {
            schema: EGRESS_DECISION_SCHEMA_V1.to_string(),
            allowed,
            reason,
            protocol,
            destination,
            port,
        }
    }
}

#[derive(Debug, Clone)]
struct CompiledHttpRule {
    scheme: EgressHttpScheme,
    host: String,
    port: u16,
}

#[derive(Debug, Clone)]
struct CompiledIpRule {
    network: IpAddr,
    prefix: u8,
    protocol: EgressProtocol,
    port: u16,
}

#[derive(Debug, Clone)]
enum CompiledEgressKind {
    Unrestricted,
    DenyAll,
    Allowlist {
        http_rules: Vec<CompiledHttpRule>,
        ip_rules: Vec<CompiledIpRule>,
    },
}

/// Canonical, immutable egress rules suitable for sharing across proxy tasks.
#[derive(Debug, Clone)]
pub struct CompiledEgressPolicy {
    kind: CompiledEgressKind,
    limits: EgressPolicyLimits,
}

impl CompiledEgressPolicy {
    /// Normalize and compile every rule before a proxy listener is created.
    pub fn compile(policy: &EgressPolicy) -> Result<Self> {
        match policy.normalized()? {
            EgressPolicy::Unrestricted => Ok(Self {
                kind: CompiledEgressKind::Unrestricted,
                limits: EgressPolicyLimits::default(),
            }),
            EgressPolicy::DenyAll => Ok(Self {
                kind: CompiledEgressKind::DenyAll,
                limits: EgressPolicyLimits::default(),
            }),
            EgressPolicy::Allowlist(rules) => {
                let http_rules = rules
                    .http_rules
                    .into_iter()
                    .map(|rule| CompiledHttpRule {
                        scheme: rule.scheme,
                        host: rule.host,
                        port: rule.port,
                    })
                    .collect();
                let ip_rules = rules
                    .ip_rules
                    .into_iter()
                    .map(|rule| {
                        let (network, prefix) = parse_normalized_ip_range(&rule.range)?;
                        Ok(CompiledIpRule {
                            network,
                            prefix,
                            protocol: rule.protocol,
                            port: rule.port,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(Self {
                    kind: CompiledEgressKind::Allowlist {
                        http_rules,
                        ip_rules,
                    },
                    limits: rules.limits,
                })
            }
        }
    }

    pub const fn limits(&self) -> EgressPolicyLimits {
        self.limits
    }

    /// Evaluate an HTTP proxy authority without resolving or connecting it.
    pub fn evaluate_http(
        &self,
        scheme: EgressHttpScheme,
        host: &str,
        port: u16,
    ) -> EgressEvaluation {
        let protocol = EgressDecisionProtocol::from(scheme);
        if port == 0 || host.contains('*') {
            return EgressEvaluation::denied(
                EgressDecisionReason::InvalidDestination,
                protocol,
                EgressDecisionDestination::Invalid,
                port,
            );
        }

        match normalize_host(host, host) {
            Ok(Host::Domain(hostname)) => {
                let destination = EgressDecisionDestination::Hostname {
                    hostname: hostname.clone(),
                };
                match &self.kind {
                    CompiledEgressKind::Unrestricted => EgressEvaluation::allowed(
                        EgressDecisionReason::Unrestricted,
                        protocol,
                        destination,
                        port,
                    ),
                    CompiledEgressKind::DenyAll => EgressEvaluation::denied(
                        EgressDecisionReason::DenyAll,
                        protocol,
                        destination,
                        port,
                    ),
                    CompiledEgressKind::Allowlist { http_rules, .. }
                        if http_rules.iter().any(|rule| {
                            rule.scheme == scheme
                                && rule.port == port
                                && domain_rule_matches(&rule.host, &hostname)
                        }) =>
                    {
                        EgressEvaluation::allowed(
                            EgressDecisionReason::HttpRuleMatched,
                            protocol,
                            destination,
                            port,
                        )
                    }
                    CompiledEgressKind::Allowlist { .. } => EgressEvaluation::denied(
                        EgressDecisionReason::HostnameNotAllowed,
                        protocol,
                        destination,
                        port,
                    ),
                }
            }
            Ok(Host::Ipv4(address)) => self.evaluate_http_ip(protocol, IpAddr::V4(address), port),
            Ok(Host::Ipv6(address)) => self.evaluate_http_ip(protocol, IpAddr::V6(address), port),
            Err(_) => EgressEvaluation::denied(
                EgressDecisionReason::InvalidDestination,
                protocol,
                EgressDecisionDestination::Invalid,
                port,
            ),
        }
    }

    /// Revalidate an authorized hostname against one concrete DNS answer.
    ///
    /// Public unicast addresses remain authorized by the hostname rule.
    /// Loopback, private, link-local, multicast, documentation, and otherwise
    /// special-use addresses additionally require a matching TCP IP/CIDR rule.
    pub fn evaluate_resolved_http(
        &self,
        scheme: EgressHttpScheme,
        host: &str,
        port: u16,
        address: IpAddr,
    ) -> EgressEvaluation {
        let initial = self.evaluate_http(scheme, host, port);
        if !initial.allowed {
            return initial;
        }

        let protocol = EgressDecisionProtocol::from(scheme);
        let hostname = match initial.destination {
            EgressDecisionDestination::Hostname { hostname } => hostname,
            EgressDecisionDestination::Ip { address: requested } => {
                if requested == address {
                    return initial;
                }
                return EgressEvaluation::denied(
                    EgressDecisionReason::ResolutionMismatch,
                    protocol,
                    EgressDecisionDestination::Ip { address },
                    port,
                );
            }
            EgressDecisionDestination::ResolvedHostname { hostname, .. } => hostname,
            EgressDecisionDestination::Invalid => return initial,
        };
        let destination = EgressDecisionDestination::ResolvedHostname { hostname, address };

        match &self.kind {
            CompiledEgressKind::Unrestricted => EgressEvaluation::allowed(
                EgressDecisionReason::Unrestricted,
                protocol,
                destination,
                port,
            ),
            CompiledEgressKind::DenyAll => {
                EgressEvaluation::denied(EgressDecisionReason::DenyAll, protocol, destination, port)
            }
            CompiledEgressKind::Allowlist { ip_rules, .. } if is_public_egress_address(address) => {
                EgressEvaluation::allowed(
                    EgressDecisionReason::HttpRuleMatched,
                    protocol,
                    destination,
                    port,
                )
            }
            CompiledEgressKind::Allowlist { ip_rules, .. }
                if ip_rules_match(ip_rules, EgressProtocol::Tcp, address, port) =>
            {
                EgressEvaluation::allowed(
                    EgressDecisionReason::HttpAndIpRuleMatched,
                    protocol,
                    destination,
                    port,
                )
            }
            CompiledEgressKind::Allowlist { .. } => EgressEvaluation::denied(
                EgressDecisionReason::ResolvedAddressRequiresIpRule,
                protocol,
                destination,
                port,
            ),
        }
    }

    /// Evaluate raw TCP or UDP against the compiled IP/CIDR rules.
    pub fn evaluate_ip(
        &self,
        protocol: EgressProtocol,
        address: IpAddr,
        port: u16,
    ) -> EgressEvaluation {
        let decision_protocol = EgressDecisionProtocol::from(protocol);
        let destination = EgressDecisionDestination::Ip { address };
        if port == 0 {
            return EgressEvaluation::denied(
                EgressDecisionReason::InvalidDestination,
                decision_protocol,
                destination,
                port,
            );
        }

        match &self.kind {
            CompiledEgressKind::Unrestricted => EgressEvaluation::allowed(
                EgressDecisionReason::Unrestricted,
                decision_protocol,
                destination,
                port,
            ),
            CompiledEgressKind::DenyAll => EgressEvaluation::denied(
                EgressDecisionReason::DenyAll,
                decision_protocol,
                destination,
                port,
            ),
            CompiledEgressKind::Allowlist { ip_rules, .. }
                if ip_rules_match(ip_rules, protocol, address, port) =>
            {
                EgressEvaluation::allowed(
                    EgressDecisionReason::IpRuleMatched,
                    decision_protocol,
                    destination,
                    port,
                )
            }
            CompiledEgressKind::Allowlist { .. } => EgressEvaluation::denied(
                EgressDecisionReason::IpNotAllowed,
                decision_protocol,
                destination,
                port,
            ),
        }
    }

    fn evaluate_http_ip(
        &self,
        protocol: EgressDecisionProtocol,
        address: IpAddr,
        port: u16,
    ) -> EgressEvaluation {
        let destination = EgressDecisionDestination::Ip { address };
        match &self.kind {
            CompiledEgressKind::Unrestricted => EgressEvaluation::allowed(
                EgressDecisionReason::Unrestricted,
                protocol,
                destination,
                port,
            ),
            CompiledEgressKind::DenyAll => {
                EgressEvaluation::denied(EgressDecisionReason::DenyAll, protocol, destination, port)
            }
            CompiledEgressKind::Allowlist { ip_rules, .. }
                if ip_rules_match(ip_rules, EgressProtocol::Tcp, address, port) =>
            {
                EgressEvaluation::allowed(
                    EgressDecisionReason::IpRuleMatched,
                    protocol,
                    destination,
                    port,
                )
            }
            CompiledEgressKind::Allowlist { .. } => EgressEvaluation::denied(
                EgressDecisionReason::IpNotAllowed,
                protocol,
                destination,
                port,
            ),
        }
    }
}

fn parse_normalized_ip_range(value: &str) -> Result<(IpAddr, u8)> {
    let (address, prefix) = value.split_once('/').ok_or_else(|| {
        BoxError::ConfigError(format!(
            "security policy: invalid normalized egress IP range {value:?}"
        ))
    })?;
    let address = address.parse::<IpAddr>().map_err(|_| {
        BoxError::ConfigError(format!(
            "security policy: invalid normalized egress IP range {value:?}"
        ))
    })?;
    let prefix = prefix.parse::<u8>().map_err(|_| {
        BoxError::ConfigError(format!(
            "security policy: invalid normalized egress IP range {value:?}"
        ))
    })?;
    let maximum = if address.is_ipv4() { 32 } else { 128 };
    if prefix > maximum {
        return Err(BoxError::ConfigError(format!(
            "security policy: invalid normalized egress IP range {value:?}"
        )));
    }
    Ok((address, prefix))
}

fn domain_rule_matches(pattern: &str, host: &str) -> bool {
    let Some(suffix) = pattern.strip_prefix("*.") else {
        return pattern == host;
    };
    host.strip_suffix(suffix)
        .and_then(|prefix| prefix.strip_suffix('.'))
        .is_some_and(|label| !label.is_empty() && !label.contains('.'))
}

fn ip_rules_match(
    rules: &[CompiledIpRule],
    protocol: EgressProtocol,
    address: IpAddr,
    port: u16,
) -> bool {
    rules.iter().any(|rule| {
        rule.protocol == protocol
            && rule.port == port
            && ip_range_contains(rule.network, rule.prefix, address)
    })
}

fn ip_range_contains(network: IpAddr, prefix: u8, address: IpAddr) -> bool {
    match (network, address) {
        (IpAddr::V4(network), IpAddr::V4(address)) => {
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - u32::from(prefix))
            };
            u32::from(network) & mask == u32::from(address) & mask
        }
        (IpAddr::V6(network), IpAddr::V6(address)) => {
            let mask = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - u32::from(prefix))
            };
            u128::from(network) & mask == u128::from(address) & mask
        }
        _ => false,
    }
}

fn is_public_egress_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let [first, second, third, _] = address.octets();
    if address.is_unspecified()
        || address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_multicast()
        || address.is_broadcast()
        || address.is_documentation()
    {
        return false;
    }
    if first == 0
        || (first == 100 && (64..=127).contains(&second))
        || (first == 192 && second == 0 && third == 0)
        || (first == 192 && second == 88 && third == 99)
        || (first == 198 && (18..=19).contains(&second))
        || first >= 240
    {
        return false;
    }
    true
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    if address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || address.is_unique_local()
        || address.is_unicast_link_local()
    {
        return false;
    }

    let segments = address.segments();
    if segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2..6] == [0, 0, 0, 0] {
        let embedded = Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            segments[6] as u8,
            (segments[7] >> 8) as u8,
            segments[7] as u8,
        );
        return is_public_ipv4(embedded);
    }
    if segments[0] == 0x2002 {
        let embedded = Ipv4Addr::new(
            (segments[1] >> 8) as u8,
            segments[1] as u8,
            (segments[2] >> 8) as u8,
            segments[2] as u8,
        );
        return is_public_ipv4(embedded);
    }

    let in_global_unicast = (segments[0] & 0xe000) == 0x2000;
    let documentation = segments[0] == 0x2001 && segments[1] == 0x0db8;
    let discard_only = segments[0] == 0x0100 && segments[1..4] == [0, 0, 0];
    let special_2001 = segments[0] == 0x2001
        && (segments[1] == 0 || segments[1] == 2 || (0x0010..=0x002f).contains(&segments[1]));
    in_global_unicast && !documentation && !discard_only && !special_2001
}

#[cfg(test)]
#[path = "egress_tests.rs"]
mod tests;
