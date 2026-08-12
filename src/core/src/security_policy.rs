//! Optional request-scoped security policy types.
//!
//! These values describe policy intent. Backend resolution must reject an
//! enabled policy until the selected backend can prove that it enforces every
//! requested control.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Host;

use crate::error::{BoxError, Result};

/// Schema version included in normalized policies and their digests.
pub const SECURITY_POLICY_SCHEMA_VERSION: u8 = 1;

/// Optional security controls attached to one execution request.
///
/// An absent policy preserves the legacy execution behavior. An empty policy
/// is rejected so callers cannot mistake a no-op object for an enabled
/// security posture.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxSecurityPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host_mounts: Option<HostMountPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    egress: Option<EgressPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    receipt: Option<ReceiptPolicy>,
}

impl SandboxSecurityPolicy {
    pub const fn new() -> Self {
        Self {
            host_mounts: None,
            egress: None,
            receipt: None,
        }
    }

    pub fn host_mounts(mut self, policy: HostMountPolicy) -> Self {
        self.host_mounts = Some(policy);
        self
    }

    pub fn egress(mut self, policy: EgressPolicy) -> Self {
        self.egress = Some(policy);
        self
    }

    pub const fn receipt(mut self, policy: ReceiptPolicy) -> Self {
        self.receipt = Some(policy);
        self
    }

    pub fn host_mount_policy(&self) -> Option<&HostMountPolicy> {
        self.host_mounts.as_ref()
    }

    pub fn egress_policy(&self) -> Option<&EgressPolicy> {
        self.egress.as_ref()
    }

    pub const fn receipt_policy(&self) -> Option<ReceiptPolicy> {
        self.receipt
    }

    /// Validate and normalize the policy without probing or mutating the host.
    pub fn resolve(&self) -> Result<ResolvedSandboxSecurityPolicy> {
        if self.host_mounts.is_none() && self.egress.is_none() && self.receipt.is_none() {
            return Err(policy_error(
                "empty security policy; omit the policy instead of enabling a no-op object",
            ));
        }

        Ok(ResolvedSandboxSecurityPolicy {
            schema_version: SECURITY_POLICY_SCHEMA_VERSION,
            host_mounts: self
                .host_mounts
                .as_ref()
                .map(HostMountPolicy::normalized)
                .transpose()?,
            egress: self
                .egress
                .as_ref()
                .map(EgressPolicy::normalized)
                .transpose()?,
            receipt: self.receipt,
        })
    }
}

/// Normalized policy persisted with the resolved execution plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedSandboxSecurityPolicy {
    pub schema_version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_mounts: Option<HostMountPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress: Option<EgressPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<ReceiptPolicy>,
}

impl ResolvedSandboxSecurityPolicy {
    /// Digest the canonical normalized JSON representation.
    pub fn digest(&self) -> Result<String> {
        if self.schema_version != SECURITY_POLICY_SCHEMA_VERSION {
            return Err(policy_error(format!(
                "unsupported resolved policy schema version {}",
                self.schema_version
            )));
        }
        let encoded = serde_json::to_vec(self).map_err(|error| {
            BoxError::SerializationError(format!(
                "failed to encode normalized security policy: {error}"
            ))
        })?;
        Ok(format!("sha256:{}", hex::encode(Sha256::digest(encoded))))
    }
}

/// Whether host mount findings are observed or enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostMountPolicyMode {
    Audit,
    Enforce,
}

/// Built-in host mount risk profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostMountProfile {
    AgentSafe,
}

/// Agent-oriented host bind mount admission policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostMountPolicy {
    pub mode: HostMountPolicyMode,
    pub profile: HostMountProfile,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_paths: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_host_control_sockets: Vec<PathBuf>,
}

impl HostMountPolicy {
    pub const fn agent_safe() -> Self {
        Self {
            mode: HostMountPolicyMode::Enforce,
            profile: HostMountProfile::AgentSafe,
            allowed_paths: Vec::new(),
            allowed_host_control_sockets: Vec::new(),
        }
    }

    pub const fn audit_only(mut self) -> Self {
        self.mode = HostMountPolicyMode::Audit;
        self
    }

    pub fn allow_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.allowed_paths.push(path.into());
        self
    }

    pub fn allow_host_control_socket(mut self, path: impl Into<PathBuf>) -> Self {
        self.allowed_host_control_sockets.push(path.into());
        self
    }

    pub(crate) fn normalized(&self) -> Result<Self> {
        let mut allowed_paths = self
            .allowed_paths
            .iter()
            .map(|path| normalize_policy_path(path, "host mount exception"))
            .collect::<Result<Vec<_>>>()?;
        let mut allowed_host_control_sockets = self
            .allowed_host_control_sockets
            .iter()
            .map(|path| normalize_policy_path(path, "host-control socket exception"))
            .collect::<Result<Vec<_>>>()?;
        allowed_paths.sort();
        allowed_paths.dedup();
        allowed_host_control_sockets.sort();
        allowed_host_control_sockets.dedup();

        Ok(Self {
            mode: self.mode,
            profile: self.profile,
            allowed_paths,
            allowed_host_control_sockets,
        })
    }
}

/// Maximum raw rule counts accepted by a single request.
const MAX_EGRESS_HTTP_RULES: usize = 256;
const MAX_EGRESS_IP_RULES: usize = 256;

/// Hard ceilings for per-generation egress engine resources.
const MAX_EGRESS_CONCURRENT_CONNECTIONS: u32 = 4_096;
const MAX_EGRESS_DNS_CACHE_ENTRIES: u32 = 16_384;
const MAX_EGRESS_DNS_ANSWERS_PER_QUERY: u32 = 64;
const MAX_EGRESS_DNS_QUERIES_PER_MINUTE: u32 = 16_384;
const MAX_EGRESS_DNS_TTL_SECONDS: u32 = 3_600;
const MAX_EGRESS_DNS_NEGATIVE_TTL_SECONDS: u32 = 300;
const MAX_EGRESS_DNS_TIMEOUT_MS: u32 = 30_000;
const MAX_EGRESS_CONNECT_TIMEOUT_MS: u32 = 60_000;
const MAX_EGRESS_IDLE_TIMEOUT_MS: u32 = 3_600_000;
const MAX_EGRESS_DECISION_RECORDS: u32 = 1_000_000;
const MAX_EGRESS_DECISION_RECORD_BYTES: u32 = 16_384;
const MAX_EGRESS_DECISION_LOG_BYTES: u64 = 64 * 1024 * 1024;

/// Outbound transport protocol used by an IP/CIDR rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EgressProtocol {
    Tcp,
    Udp,
}

/// Application protocol supported by a hostname rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EgressHttpScheme {
    Http,
    Https,
}

/// One HTTP or HTTPS hostname authorized through the mandatory host proxy.
///
/// `host` is either an exact hostname or one leading `*.` pattern. A wildcard
/// matches exactly one non-empty label, never the suffix itself or a deeper
/// descendant. Hostname rules never authorize raw TCP, UDP, or IP literals.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressHttpRule {
    pub scheme: EgressHttpScheme,
    pub host: String,
    pub port: u16,
}

impl EgressHttpRule {
    pub fn http(host: impl Into<String>) -> Self {
        Self::http_on(host, 80)
    }

    pub fn https(host: impl Into<String>) -> Self {
        Self::https_on(host, 443)
    }

    pub fn http_on(host: impl Into<String>, port: u16) -> Self {
        Self {
            scheme: EgressHttpScheme::Http,
            host: host.into(),
            port,
        }
    }

    pub fn https_on(host: impl Into<String>, port: u16) -> Self {
        Self {
            scheme: EgressHttpScheme::Https,
            host: host.into(),
            port,
        }
    }
}

/// One IP/CIDR, transport protocol, and port authorized for raw egress.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressIpRule {
    pub range: String,
    pub protocol: EgressProtocol,
    pub port: u16,
}

impl EgressIpRule {
    pub fn tcp(range: impl Into<String>, port: u16) -> Self {
        Self {
            range: range.into(),
            protocol: EgressProtocol::Tcp,
            port,
        }
    }

    pub fn udp(range: impl Into<String>, port: u16) -> Self {
        Self {
            range: range.into(),
            protocol: EgressProtocol::Udp,
            port,
        }
    }
}

/// Bounded resources attached to one immutable, generation-scoped allowlist.
///
/// These values are part of the normalized policy and its digest. Runtime
/// enforcement may use less capacity, but must never exceed these limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EgressPolicyLimits {
    pub max_concurrent_connections: u32,
    pub max_pending_connections: u32,
    pub max_dns_cache_entries: u32,
    pub max_dns_answers_per_query: u32,
    pub max_dns_queries_per_minute: u32,
    pub min_dns_ttl_seconds: u32,
    pub max_dns_ttl_seconds: u32,
    pub max_dns_negative_ttl_seconds: u32,
    pub dns_timeout_ms: u32,
    pub connect_timeout_ms: u32,
    pub idle_timeout_ms: u32,
    pub max_decision_records: u32,
    pub max_decision_record_bytes: u32,
    pub max_decision_log_bytes: u64,
}

impl Default for EgressPolicyLimits {
    fn default() -> Self {
        Self {
            max_concurrent_connections: 256,
            max_pending_connections: 64,
            max_dns_cache_entries: 1_024,
            max_dns_answers_per_query: 32,
            max_dns_queries_per_minute: 1_024,
            min_dns_ttl_seconds: 1,
            max_dns_ttl_seconds: 300,
            max_dns_negative_ttl_seconds: 30,
            dns_timeout_ms: 5_000,
            connect_timeout_ms: 10_000,
            idle_timeout_ms: 300_000,
            max_decision_records: 10_000,
            max_decision_record_bytes: 4_096,
            max_decision_log_bytes: 8 * 1024 * 1024,
        }
    }
}

impl EgressPolicyLimits {
    fn normalized(&self) -> Result<Self> {
        validate_egress_limit(
            self.max_concurrent_connections,
            1,
            MAX_EGRESS_CONCURRENT_CONNECTIONS,
            "concurrent connections",
        )?;
        validate_egress_limit(
            self.max_pending_connections,
            1,
            self.max_concurrent_connections,
            "pending connections",
        )?;
        validate_egress_limit(
            self.max_dns_cache_entries,
            1,
            MAX_EGRESS_DNS_CACHE_ENTRIES,
            "DNS cache entries",
        )?;
        validate_egress_limit(
            self.max_dns_answers_per_query,
            1,
            MAX_EGRESS_DNS_ANSWERS_PER_QUERY,
            "DNS answers per query",
        )?;
        validate_egress_limit(
            self.max_dns_queries_per_minute,
            1,
            MAX_EGRESS_DNS_QUERIES_PER_MINUTE,
            "DNS queries per minute",
        )?;
        validate_egress_limit(
            self.min_dns_ttl_seconds,
            1,
            MAX_EGRESS_DNS_TTL_SECONDS,
            "minimum DNS TTL seconds",
        )?;
        validate_egress_limit(
            self.max_dns_ttl_seconds,
            self.min_dns_ttl_seconds,
            MAX_EGRESS_DNS_TTL_SECONDS,
            "maximum DNS TTL seconds",
        )?;
        validate_egress_limit(
            self.max_dns_negative_ttl_seconds,
            1,
            MAX_EGRESS_DNS_NEGATIVE_TTL_SECONDS,
            "negative DNS TTL seconds",
        )?;
        validate_egress_limit(
            self.dns_timeout_ms,
            100,
            MAX_EGRESS_DNS_TIMEOUT_MS,
            "DNS timeout milliseconds",
        )?;
        validate_egress_limit(
            self.connect_timeout_ms,
            100,
            MAX_EGRESS_CONNECT_TIMEOUT_MS,
            "connection timeout milliseconds",
        )?;
        validate_egress_limit(
            self.idle_timeout_ms,
            1_000,
            MAX_EGRESS_IDLE_TIMEOUT_MS,
            "idle timeout milliseconds",
        )?;
        validate_egress_limit(
            self.max_decision_records,
            2,
            MAX_EGRESS_DECISION_RECORDS,
            "decision records",
        )?;
        validate_egress_limit(
            self.max_decision_record_bytes,
            256,
            MAX_EGRESS_DECISION_RECORD_BYTES,
            "decision record bytes",
        )?;
        validate_egress_limit_u64(
            self.max_decision_log_bytes,
            u64::from(self.max_decision_record_bytes),
            MAX_EGRESS_DECISION_LOG_BYTES,
            "decision log bytes",
        )?;
        Ok(*self)
    }
}

/// Normalizable destination rules for outbound access.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressAllowlist {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub http_rules: Vec<EgressHttpRule>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ip_rules: Vec<EgressIpRule>,
    #[serde(default)]
    pub limits: EgressPolicyLimits,
}

impl EgressAllowlist {
    pub fn new<HttpRules, IpRules>(http_rules: HttpRules, ip_rules: IpRules) -> Self
    where
        HttpRules: IntoIterator<Item = EgressHttpRule>,
        IpRules: IntoIterator<Item = EgressIpRule>,
    {
        Self {
            http_rules: http_rules.into_iter().collect(),
            ip_rules: ip_rules.into_iter().collect(),
            limits: EgressPolicyLimits::default(),
        }
    }

    pub const fn with_limits(mut self, limits: EgressPolicyLimits) -> Self {
        self.limits = limits;
        self
    }
}

/// Optional outbound access policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "rules", rename_all = "snake_case")]
pub enum EgressPolicy {
    Unrestricted,
    DenyAll,
    Allowlist(EgressAllowlist),
}

impl EgressPolicy {
    pub fn allowlist<HttpRules, IpRules>(http_rules: HttpRules, ip_rules: IpRules) -> Self
    where
        HttpRules: IntoIterator<Item = EgressHttpRule>,
        IpRules: IntoIterator<Item = EgressIpRule>,
    {
        Self::Allowlist(EgressAllowlist::new(http_rules, ip_rules))
    }

    /// Allow HTTPS to the supplied domains and deny direct IP literals.
    pub fn allow_domains<Domains, Domain>(domains: Domains) -> Self
    where
        Domains: IntoIterator<Item = Domain>,
        Domain: Into<String>,
    {
        Self::allowlist(
            domains.into_iter().map(EgressHttpRule::https),
            std::iter::empty::<EgressIpRule>(),
        )
    }

    /// Evaluate the normalized first-release HTTP/HTTPS policy semantics.
    ///
    /// This method does not establish a connection. The runtime must still
    /// enforce hostname rules through its mandatory proxy. An IP literal is
    /// evaluated as raw TCP and requires an explicit IP/CIDR rule.
    pub fn allows_http(&self, scheme: EgressHttpScheme, host: &str, port: u16) -> Result<bool> {
        Ok(crate::egress::CompiledEgressPolicy::compile(self)?
            .evaluate_http(scheme, host, port)
            .is_allowed())
    }

    /// Evaluate raw TCP or UDP against explicit IP/CIDR rules.
    ///
    /// Hostname rules are deliberately ignored because raw transports cannot
    /// prove that a destination IP still represents a claimed hostname.
    pub fn allows_ip(&self, protocol: EgressProtocol, address: IpAddr, port: u16) -> Result<bool> {
        Ok(crate::egress::CompiledEgressPolicy::compile(self)?
            .evaluate_ip(protocol, address, port)
            .is_allowed())
    }

    /// Validate and return the canonical policy used by runtime compilers.
    pub fn normalized(&self) -> Result<Self> {
        match self {
            Self::Unrestricted => Ok(Self::Unrestricted),
            Self::DenyAll => Ok(Self::DenyAll),
            Self::Allowlist(rules) => {
                if rules.http_rules.len() > MAX_EGRESS_HTTP_RULES {
                    return Err(policy_error(format!(
                        "egress allowlist exceeds {MAX_EGRESS_HTTP_RULES} HTTP rules"
                    )));
                }
                if rules.ip_rules.len() > MAX_EGRESS_IP_RULES {
                    return Err(policy_error(format!(
                        "egress allowlist exceeds {MAX_EGRESS_IP_RULES} IP rules"
                    )));
                }

                let mut http_rules = rules
                    .http_rules
                    .iter()
                    .map(|rule| {
                        validate_egress_port(rule.port)?;
                        Ok(EgressHttpRule {
                            scheme: rule.scheme,
                            host: normalize_domain_pattern(&rule.host)?,
                            port: rule.port,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let mut ip_rules = rules
                    .ip_rules
                    .iter()
                    .map(|rule| {
                        validate_egress_port(rule.port)?;
                        Ok(EgressIpRule {
                            range: normalize_ip_range(&rule.range)?,
                            protocol: rule.protocol,
                            port: rule.port,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;

                http_rules.sort();
                http_rules.dedup();
                ip_rules.sort();
                ip_rules.dedup();

                if http_rules.is_empty() && ip_rules.is_empty() {
                    return Err(policy_error(
                        "egress allowlist requires at least one destination",
                    ));
                }

                Ok(Self::Allowlist(EgressAllowlist {
                    http_rules,
                    ip_rules,
                    limits: rules.limits.normalized()?,
                }))
            }
        }
    }
}

/// Required durable execution evidence policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptPolicy {
    Required,
}

fn normalize_policy_path(path: &Path, label: &str) -> Result<PathBuf> {
    let is_normalized = path.is_absolute()
        && path.to_str().is_some()
        && !path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir));
    if !is_normalized {
        return Err(policy_error(format!(
            "{label} must be an absolute normalized path: {}",
            path.display()
        )));
    }
    Ok(path.components().collect())
}

fn normalize_domain_pattern(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(policy_error(format!("invalid egress domain {value:?}")));
    }
    let (wildcard, domain) = match trimmed.strip_prefix("*.") {
        Some(domain) => (true, domain),
        None => (false, trimmed),
    };
    if domain.contains('*') {
        return Err(policy_error(format!("invalid egress domain {value:?}")));
    }

    let Host::Domain(domain) = normalize_host(domain, value)? else {
        return Err(policy_error(format!(
            "invalid egress domain {value:?}: IP literals require an IP/CIDR rule"
        )));
    };
    if wildcard && domain.split('.').count() < 2 {
        return Err(policy_error(format!(
            "invalid egress domain {value:?}: wildcard suffix requires at least two labels"
        )));
    }

    Ok(if wildcard {
        format!("*.{domain}")
    } else {
        domain.to_string()
    })
}

pub(crate) fn normalize_host(value: &str, original: &str) -> Result<Host<String>> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.contains('%') {
        return Err(policy_error(format!("invalid egress domain {original:?}")));
    }

    let parsed = Host::parse(trimmed)
        .map_err(|_| policy_error(format!("invalid egress domain {original:?}")))?;
    match parsed {
        Host::Domain(mut domain) => {
            if domain.ends_with('.') {
                domain.pop();
            }
            validate_ascii_domain(&domain, original)?;
            Ok(Host::Domain(domain))
        }
        Host::Ipv4(address) => Ok(Host::Ipv4(address)),
        Host::Ipv6(address) => Ok(Host::Ipv6(address)),
    }
}

fn validate_ascii_domain(domain: &str, original: &str) -> Result<()> {
    if domain.is_empty()
        || domain.len() > 253
        || !domain.is_ascii()
        || domain.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return Err(policy_error(format!("invalid egress domain {original:?}")));
    }
    Ok(())
}

pub(crate) fn validate_egress_port(port: u16) -> Result<()> {
    if port == 0 {
        return Err(policy_error(
            "egress allowlist port must be greater than zero",
        ));
    }
    Ok(())
}

fn validate_egress_limit(value: u32, minimum: u32, maximum: u32, label: &str) -> Result<()> {
    if !(minimum..=maximum).contains(&value) {
        return Err(policy_error(format!(
            "egress {label} must be between {minimum} and {maximum}, got {value}"
        )));
    }
    Ok(())
}

fn validate_egress_limit_u64(value: u64, minimum: u64, maximum: u64, label: &str) -> Result<()> {
    if !(minimum..=maximum).contains(&value) {
        return Err(policy_error(format!(
            "egress {label} must be between {minimum} and {maximum}, got {value}"
        )));
    }
    Ok(())
}

fn normalize_ip_range(value: &str) -> Result<String> {
    let trimmed = value.trim();
    let (address, prefix) = match trimmed.split_once('/') {
        Some((address, prefix)) => (address, Some(prefix)),
        None => (trimmed, None),
    };
    let address = address
        .parse::<IpAddr>()
        .map_err(|_| policy_error(format!("invalid egress IP range {value:?}")))?;

    match address {
        IpAddr::V4(address) => {
            let prefix = parse_prefix(prefix, 32, value)?;
            let bits = u32::from(address);
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - u32::from(prefix))
            };
            Ok(format!("{}/{}", Ipv4Addr::from(bits & mask), prefix))
        }
        IpAddr::V6(address) => {
            let prefix = parse_prefix(prefix, 128, value)?;
            let bits = u128::from(address);
            let mask = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - u32::from(prefix))
            };
            Ok(format!("{}/{}", Ipv6Addr::from(bits & mask), prefix))
        }
    }
}

fn parse_prefix(prefix: Option<&str>, maximum: u8, original: &str) -> Result<u8> {
    let prefix = match prefix {
        Some(prefix) => prefix
            .parse::<u8>()
            .map_err(|_| policy_error(format!("invalid egress IP range {original:?}")))?,
        None => maximum,
    };
    if prefix > maximum {
        return Err(policy_error(format!(
            "invalid egress IP range {original:?}: prefix exceeds {maximum}"
        )));
    }
    Ok(prefix)
}

fn policy_error(message: impl Into<String>) -> BoxError {
    BoxError::ConfigError(format!("security policy: {}", message.into()))
}

#[cfg(test)]
#[path = "security_policy_egress_tests.rs"]
mod egress_tests;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[cfg(unix)]
    fn absolute_test_path(value: &str) -> PathBuf {
        PathBuf::from(format!("/{value}"))
    }

    #[cfg(windows)]
    fn absolute_test_path(value: &str) -> PathBuf {
        PathBuf::from(format!(r"C:\{}", value.replace('/', r"\")))
    }

    #[test]
    fn equivalent_policies_have_the_same_normalized_value_and_digest() {
        let first = SandboxSecurityPolicy::new()
            .host_mounts(
                HostMountPolicy::agent_safe()
                    .allow_path(absolute_test_path("srv//workspace/"))
                    .allow_path(absolute_test_path("opt/toolchain"))
                    .allow_path(absolute_test_path("srv/workspace"))
                    .allow_host_control_socket(absolute_test_path("run/podman/podman.sock")),
            )
            .egress(EgressPolicy::allowlist(
                [
                    EgressHttpRule::https("API.EXAMPLE.COM."),
                    EgressHttpRule::https("*.Example.org"),
                    EgressHttpRule::https("BÜCHER.Example"),
                    EgressHttpRule::https("api.example.com"),
                ],
                [
                    EgressIpRule::tcp("10.0.0.7/24", 443),
                    EgressIpRule::udp("2001:db8::1", 53),
                    EgressIpRule::tcp("10.0.0.7/24", 443),
                ],
            ))
            .receipt(ReceiptPolicy::Required);
        let second = SandboxSecurityPolicy::new()
            .receipt(ReceiptPolicy::Required)
            .egress(EgressPolicy::allowlist(
                [
                    EgressHttpRule::https("*.example.org"),
                    EgressHttpRule::https("api.example.com"),
                    EgressHttpRule::https("xn--bcher-kva.example"),
                ],
                [
                    EgressIpRule::udp("2001:db8::1/128", 53),
                    EgressIpRule::tcp("10.0.0.0/24", 443),
                ],
            ))
            .host_mounts(
                HostMountPolicy::agent_safe()
                    .allow_host_control_socket(absolute_test_path("run/podman/podman.sock"))
                    .allow_path(absolute_test_path("opt/toolchain"))
                    .allow_path(absolute_test_path("srv/workspace")),
            );

        let first = first.resolve().unwrap();
        let second = second.resolve().unwrap();
        assert_eq!(first, second);
        assert_eq!(first.digest().unwrap(), second.digest().unwrap());
    }

    #[test]
    fn changed_policy_changes_the_digest() {
        let audit = SandboxSecurityPolicy::new()
            .host_mounts(HostMountPolicy::agent_safe().audit_only())
            .resolve()
            .unwrap();
        let enforce = SandboxSecurityPolicy::new()
            .host_mounts(HostMountPolicy::agent_safe())
            .resolve()
            .unwrap();

        assert_ne!(audit.digest().unwrap(), enforce.digest().unwrap());
    }

    #[test]
    fn empty_and_ambiguous_policies_are_rejected() {
        let empty = SandboxSecurityPolicy::new().resolve().unwrap_err();
        assert!(empty.to_string().contains("empty"));

        let empty_allowlist = SandboxSecurityPolicy::new()
            .egress(EgressPolicy::allowlist(
                std::iter::empty::<EgressHttpRule>(),
                std::iter::empty::<EgressIpRule>(),
            ))
            .resolve()
            .unwrap_err();
        assert!(empty_allowlist
            .to_string()
            .contains("at least one destination"));

        for domain in [
            "*",
            "*.com",
            "api.*.example.com",
            "https://example.com",
            "127.0.0.1",
            "127.1",
            "[::1]",
            "api_example.com",
        ] {
            let error = SandboxSecurityPolicy::new()
                .egress(EgressPolicy::allowlist(
                    [EgressHttpRule::https(domain)],
                    std::iter::empty::<EgressIpRule>(),
                ))
                .resolve()
                .unwrap_err();
            assert!(
                error.to_string().contains("domain"),
                "expected {domain:?} to be rejected as a domain, got {error}"
            );
        }
    }

    #[test]
    fn unknown_policy_fields_and_variants_are_rejected() {
        let unknown_field = serde_json::from_value::<SandboxSecurityPolicy>(
            serde_json::json!({ "receipt": "required", "fallback": true }),
        )
        .unwrap_err();
        assert!(unknown_field.to_string().contains("unknown field"));

        let unknown_variant = serde_json::from_value::<SandboxSecurityPolicy>(
            serde_json::json!({ "egress": { "mode": "best_effort" } }),
        )
        .unwrap_err();
        assert!(unknown_variant.to_string().contains("unknown variant"));

        let unknown_allowlist_field =
            serde_json::from_value::<SandboxSecurityPolicy>(serde_json::json!({
                "egress": {
                    "mode": "allowlist",
                        "rules": {
                        "http_rules": [{
                            "scheme": "https",
                            "host": "example.com",
                            "port": 443
                        }],
                        "fallback": "unrestricted"
                    }
                }
            }))
            .unwrap_err();
        assert!(unknown_allowlist_field
            .to_string()
            .contains("unknown field"));
    }

    #[test]
    fn invalid_cidr_port_and_mount_exceptions_are_rejected() {
        for range in ["10.0.0.1/33", "2001:db8::/129", "not-an-address"] {
            let error = SandboxSecurityPolicy::new()
                .egress(EgressPolicy::allowlist(
                    std::iter::empty::<EgressHttpRule>(),
                    [EgressIpRule::tcp(range, 443)],
                ))
                .resolve()
                .unwrap_err();
            assert!(error.to_string().contains("IP range"));
        }

        let invalid_port = SandboxSecurityPolicy::new()
            .egress(EgressPolicy::allowlist(
                [EgressHttpRule::https_on("example.com", 0)],
                std::iter::empty::<EgressIpRule>(),
            ))
            .resolve()
            .unwrap_err();
        assert!(invalid_port.to_string().contains("port"));

        for path in [
            PathBuf::from("relative"),
            absolute_test_path("srv/../secret"),
        ] {
            let error = SandboxSecurityPolicy::new()
                .host_mounts(HostMountPolicy::agent_safe().allow_path(path))
                .resolve()
                .unwrap_err();
            assert!(error.to_string().contains("absolute normalized path"));
        }
    }

    #[test]
    fn resolved_policy_uses_a_versioned_stable_shape() {
        let resolved = SandboxSecurityPolicy::new()
            .receipt(ReceiptPolicy::Required)
            .resolve()
            .unwrap();
        let value = serde_json::to_value(&resolved).unwrap();

        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["receipt"], "required");
        assert!(value.get("host_mounts").is_none());
        assert!(value.get("egress").is_none());
        assert!(resolved.digest().unwrap().starts_with("sha256:"));
        assert_eq!(resolved.digest().unwrap().len(), 71);
    }
}
