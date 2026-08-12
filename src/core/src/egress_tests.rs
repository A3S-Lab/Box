use super::*;
use crate::security_policy::{EgressHttpRule, EgressIpRule};

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn compiled_policy_is_send_sync_and_immutable() {
    assert_send_sync::<CompiledEgressPolicy>();
    let mut source = EgressPolicy::allow_domains(["api.example.com"]);
    let compiled = CompiledEgressPolicy::compile(&source).unwrap();
    source = EgressPolicy::DenyAll;

    assert!(compiled
        .evaluate_http(EgressHttpScheme::Https, "api.example.com", 443)
        .is_allowed());
    assert!(!CompiledEgressPolicy::compile(&source)
        .unwrap()
        .evaluate_http(EgressHttpScheme::Https, "api.example.com", 443)
        .is_allowed());
}

#[test]
fn decisions_are_default_deny_and_structurally_redacted() {
    let compiled =
        CompiledEgressPolicy::compile(&EgressPolicy::allow_domains(["api.example.com"])).unwrap();
    let decision = compiled.evaluate_http(
        EgressHttpScheme::Https,
        "user:secret@api.example.com/path?token=secret",
        443,
    );
    assert!(!decision.is_allowed());
    assert_eq!(decision.reason, EgressDecisionReason::InvalidDestination);

    let encoded = serde_json::to_string(&decision).unwrap();
    assert!(!encoded.contains("secret"));
    assert!(!encoded.contains("token"));
    assert!(!encoded.contains("path"));
    assert_eq!(decision.destination, EgressDecisionDestination::Invalid);
}

#[test]
fn hostname_and_ip_rules_keep_distinct_protocol_boundaries() {
    let compiled = CompiledEgressPolicy::compile(&EgressPolicy::allowlist(
        [EgressHttpRule::https("*.example.com")],
        [EgressIpRule::udp("2001:db8::/32", 53)],
    ))
    .unwrap();

    assert!(compiled
        .evaluate_http(EgressHttpScheme::Https, "api.example.com", 443)
        .is_allowed());
    assert!(!compiled
        .evaluate_http(EgressHttpScheme::Https, "a.b.example.com", 443)
        .is_allowed());
    assert!(compiled
        .evaluate_ip(EgressProtocol::Udp, "2001:db8::53".parse().unwrap(), 53,)
        .is_allowed());
    assert!(!compiled
        .evaluate_ip(EgressProtocol::Tcp, "2001:db8::53".parse().unwrap(), 53,)
        .is_allowed());
}

#[test]
fn hostname_resolution_rejects_special_addresses_without_an_ip_rule() {
    let hostname_only =
        CompiledEgressPolicy::compile(&EgressPolicy::allow_domains(["api.example.com"])).unwrap();
    assert!(hostname_only
        .evaluate_resolved_http(
            EgressHttpScheme::Https,
            "api.example.com",
            443,
            "93.184.216.34".parse().unwrap(),
        )
        .is_allowed());
    for address in [
        "127.0.0.1",
        "10.0.0.1",
        "169.254.169.254",
        "::1",
        "fd00::1",
        "fe80::1",
    ] {
        let decision = hostname_only.evaluate_resolved_http(
            EgressHttpScheme::Https,
            "api.example.com",
            443,
            address.parse().unwrap(),
        );
        assert_eq!(
            decision.reason,
            EgressDecisionReason::ResolvedAddressRequiresIpRule,
            "expected {address} to require an explicit IP rule"
        );
    }

    let with_private_ip = CompiledEgressPolicy::compile(&EgressPolicy::allowlist(
        [EgressHttpRule::https("api.example.com")],
        [EgressIpRule::tcp("10.0.0.0/8", 443)],
    ))
    .unwrap();
    let decision = with_private_ip.evaluate_resolved_http(
        EgressHttpScheme::Https,
        "api.example.com",
        443,
        "10.1.2.3".parse().unwrap(),
    );
    assert!(decision.is_allowed());
    assert_eq!(decision.reason, EgressDecisionReason::HttpAndIpRuleMatched);
}

#[test]
fn noncanonical_ip_literals_cannot_match_hostname_rules() {
    let compiled =
        CompiledEgressPolicy::compile(&EgressPolicy::allow_domains(["127.1.example.com"])).unwrap();
    assert!(!compiled
        .evaluate_http(EgressHttpScheme::Https, "127.1", 443)
        .is_allowed());
}
