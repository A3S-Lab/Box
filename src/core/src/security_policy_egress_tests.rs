use super::*;

#[test]
fn allow_domains_is_a_bounded_https_convenience() {
    let resolved = SandboxSecurityPolicy::new()
        .egress(EgressPolicy::allow_domains(["api.example.com"]))
        .resolve()
        .unwrap();
    let EgressPolicy::Allowlist(rules) = resolved.egress.unwrap() else {
        panic!("expected allowlist")
    };

    assert_eq!(
        rules.http_rules,
        vec![EgressHttpRule::https("api.example.com")]
    );
    assert!(rules.ip_rules.is_empty());
    assert_eq!(rules.limits, EgressPolicyLimits::default());
}

#[test]
fn hostname_rules_normalize_idna_and_match_only_the_declared_http_boundary() {
    let policy = EgressPolicy::allowlist(
        [
            EgressHttpRule::https("BÜCHER.Example."),
            EgressHttpRule::http_on("*.svc.example.com", 8080),
        ],
        std::iter::empty::<EgressIpRule>(),
    );

    assert!(policy
        .allows_http(EgressHttpScheme::Https, "xn--bcher-kva.example", 443)
        .unwrap());
    assert!(policy
        .allows_http(EgressHttpScheme::Https, "BÜCHER.EXAMPLE.", 443)
        .unwrap());
    assert!(policy
        .allows_http(EgressHttpScheme::Http, "api.svc.example.com", 8080)
        .unwrap());
    assert!(!policy
        .allows_http(EgressHttpScheme::Https, "bücher.example", 80)
        .unwrap());
    assert!(!policy
        .allows_http(EgressHttpScheme::Http, "svc.example.com", 8080)
        .unwrap());
    assert!(!policy
        .allows_http(EgressHttpScheme::Http, "a.b.svc.example.com", 8080)
        .unwrap());
    assert!(!policy
        .allows_http(EgressHttpScheme::Https, "unlisted.example", 443)
        .unwrap());
}

#[test]
fn allowlist_denies_ip_literals_and_raw_transports_without_an_ip_rule() {
    let hostname_only = EgressPolicy::allow_domains(["api.example.com"]);
    assert!(!hostname_only
        .allows_http(EgressHttpScheme::Https, "127.0.0.1", 443)
        .unwrap());
    assert!(!hostname_only
        .allows_http(EgressHttpScheme::Https, "127.1", 443)
        .unwrap());
    assert!(!hostname_only
        .allows_ip(EgressProtocol::Tcp, "127.0.0.1".parse().unwrap(), 443)
        .unwrap());

    let with_ip = EgressPolicy::allowlist(
        [EgressHttpRule::https("api.example.com")],
        [
            EgressIpRule::tcp("10.0.0.7/24", 443),
            EgressIpRule::udp("2001:db8::/32", 53),
        ],
    );
    assert!(with_ip
        .allows_ip(EgressProtocol::Tcp, "10.0.0.200".parse().unwrap(), 443)
        .unwrap());
    assert!(with_ip
        .allows_http(EgressHttpScheme::Https, "10.0.0.200", 443)
        .unwrap());
    assert!(!with_ip
        .allows_ip(EgressProtocol::Udp, "10.0.0.200".parse().unwrap(), 443)
        .unwrap());
    assert!(!with_ip
        .allows_ip(EgressProtocol::Udp, "2001:db8::1".parse().unwrap(), 443)
        .unwrap());
    assert!(with_ip
        .allows_ip(EgressProtocol::Udp, "2001:db8::1".parse().unwrap(), 53)
        .unwrap());
}

#[test]
fn deny_all_and_unrestricted_have_explicit_decision_semantics() {
    let destination = "203.0.113.8".parse().unwrap();
    assert!(!EgressPolicy::DenyAll
        .allows_ip(EgressProtocol::Tcp, destination, 443)
        .unwrap());
    assert!(EgressPolicy::Unrestricted
        .allows_ip(EgressProtocol::Tcp, destination, 443)
        .unwrap());
}

#[test]
fn egress_resource_limits_are_normalized_and_bounded() {
    let defaults = EgressPolicyLimits::default();
    assert_eq!(defaults.max_concurrent_connections, 256);
    assert_eq!(defaults.max_pending_connections, 64);
    assert_eq!(defaults.max_dns_cache_entries, 1024);
    assert_eq!(defaults.max_dns_answers_per_query, 32);
    assert_eq!(defaults.max_dns_queries_per_minute, 1024);
    assert_eq!(defaults.min_dns_ttl_seconds, 1);
    assert_eq!(defaults.max_dns_ttl_seconds, 300);
    assert_eq!(defaults.max_dns_negative_ttl_seconds, 30);
    assert_eq!(defaults.dns_timeout_ms, 5_000);
    assert_eq!(defaults.connect_timeout_ms, 10_000);
    assert_eq!(defaults.idle_timeout_ms, 300_000);
    assert_eq!(defaults.max_decision_records, 10_000);
    assert_eq!(defaults.max_decision_record_bytes, 4_096);
    assert_eq!(defaults.max_decision_log_bytes, 8 * 1024 * 1024);

    let mut limits = defaults;
    limits.max_pending_connections = limits.max_concurrent_connections + 1;
    let error = SandboxSecurityPolicy::new()
        .egress(EgressPolicy::Allowlist(
            EgressAllowlist::new(
                [EgressHttpRule::https("example.com")],
                std::iter::empty::<EgressIpRule>(),
            )
            .with_limits(limits),
        ))
        .resolve()
        .unwrap_err();
    assert!(error.to_string().contains("pending connections"));

    let mut limits = defaults;
    limits.max_decision_log_bytes = u64::MAX;
    let error = SandboxSecurityPolicy::new()
        .egress(EgressPolicy::Allowlist(
            EgressAllowlist::new(
                [EgressHttpRule::https("example.com")],
                std::iter::empty::<EgressIpRule>(),
            )
            .with_limits(limits),
        ))
        .resolve()
        .unwrap_err();
    assert!(error.to_string().contains("decision log"));
}
