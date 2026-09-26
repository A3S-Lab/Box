//! Keep-authority Sandbox bridge FORWARD egress (Axis C).
//!
//! Installs a per-bridge iptables filter chain that mirrors MicroVM
//! [`a3s_box_netproxy::egress::untrusted_egress_denied`]: first-match operator
//! CIDR/protocol/port rules, then the default untrusted profile (metadata /
//! foreign RFC1918 / CGNAT deny; attached CIDR + public allow).
//!
//! Domain match, CNI, full AAAA, and Enterprise GA are out of scope.

use std::process::Command;

use a3s_box_core::{EgressMatchRule, ExecutionManagerError, ExecutionManagerResult, PolicyAction};

/// IFNAMSIZ-safe custom chain for one Box bridge (`A3SSE` + 8 hex).
pub(crate) fn egress_filter_chain_name(bridge_iface: &str) -> String {
    let digest = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bridge_iface.as_bytes());
        format!("{:x}", hasher.finalize())
    };
    format!("A3SSE{}", &digest[..8])
}

/// Compile filter-table chain body rules (no `-A CHAIN` prefix).
///
/// Order matches netproxy: operator first-match, then ACCEPT attached CIDR,
/// then default denies, then ACCEPT remainder (public).
pub(crate) fn compile_sandbox_egress_chain_body(
    attached_subnet: &str,
    rules: &[EgressMatchRule],
) -> ExecutionManagerResult<Vec<Vec<String>>> {
    let mut body = Vec::new();
    for rule in rules {
        body.push(compile_operator_rule(rule)?);
    }
    body.push(vec![
        "-d".into(),
        attached_subnet.into(),
        "-j".into(),
        "ACCEPT".into(),
    ]);
    for cidr in [
        "127.0.0.0/8",
        "0.0.0.0/8",
        "255.255.255.255/32",
        "169.254.0.0/16",
        "10.0.0.0/8",
        "172.16.0.0/12",
        "192.168.0.0/16",
        "100.64.0.0/10",
        "224.0.0.0/4",
    ] {
        body.push(vec!["-d".into(), cidr.into(), "-j".into(), "DROP".into()]);
    }
    body.push(vec!["-j".into(), "ACCEPT".into()]);
    Ok(body)
}

fn compile_operator_rule(rule: &EgressMatchRule) -> ExecutionManagerResult<Vec<String>> {
    let mut args = Vec::new();
    if let Some(cidr) = &rule.cidr {
        args.push("-d".into());
        args.push(cidr.clone());
    }
    match rule.protocol.as_str() {
        "any" => {}
        "tcp" | "udp" => {
            args.push("-p".into());
            args.push(rule.protocol.clone());
            if let Some(port) = rule.port {
                args.push("--dport".into());
                args.push(port.to_string());
            }
        }
        other => {
            return Err(ExecutionManagerError::InvalidRequest(format!(
                "sandbox egress filter refuses protocol '{other}'"
            )));
        }
    }
    args.push("-j".into());
    args.push(match rule.action {
        PolicyAction::Allow => "ACCEPT".into(),
        PolicyAction::Deny => "DROP".into(),
    });
    Ok(args)
}

/// FORWARD jump that selects traffic leaving the product subnet via the host.
pub(crate) fn compile_forward_jump_args(
    subnet: &str,
    bridge_iface: &str,
    chain: &str,
) -> Vec<String> {
    vec![
        "-s".into(),
        subnet.into(),
        "!".into(),
        "-o".into(),
        bridge_iface.into(),
        "-j".into(),
        chain.into(),
    ]
}

#[cfg(target_os = "linux")]
pub(crate) fn ensure_bridge_egress_filter(
    subnet: &str,
    bridge_iface: &str,
    rules: &[EgressMatchRule],
) -> ExecutionManagerResult<()> {
    if subnet.is_empty() || bridge_iface.is_empty() {
        return Err(ExecutionManagerError::InvalidRequest(
            "sandbox egress filter requires subnet and bridge iface".into(),
        ));
    }
    let chain = egress_filter_chain_name(bridge_iface);
    ensure_chain_exists(&chain)?;
    flush_chain(&chain)?;
    let body = compile_sandbox_egress_chain_body(subnet, rules)?;
    for rule in &body {
        let mut args = vec!["-A".to_string(), chain.clone()];
        args.extend(rule.iter().cloned());
        run_iptables_owned(&args)?;
    }
    let jump = compile_forward_jump_args(subnet, bridge_iface, &chain);
    if !iptables_check_owned(
        &std::iter::once("-C".into())
            .chain(std::iter::once("FORWARD".into()))
            .chain(jump.iter().cloned())
            .collect::<Vec<_>>(),
    )? {
        let mut args = vec!["-I".to_string(), "FORWARD".into()];
        args.extend(jump);
        run_iptables_owned(&args)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn remove_bridge_egress_filter(
    subnet: &str,
    bridge_iface: &str,
) -> ExecutionManagerResult<()> {
    if subnet.is_empty() || bridge_iface.is_empty() {
        return Ok(());
    }
    let chain = egress_filter_chain_name(bridge_iface);
    let jump = compile_forward_jump_args(subnet, bridge_iface, &chain);
    let mut del = vec!["-D".to_string(), "FORWARD".into()];
    del.extend(jump);
    let _ = iptables_delete_if_present_owned(&del);
    let _ = flush_chain(&chain);
    let _ = run_iptables_owned(&["-X".into(), chain]);
    Ok(())
}

#[cfg(target_os = "linux")]
fn ensure_chain_exists(chain: &str) -> ExecutionManagerResult<()> {
    if iptables_check_owned(&["-L".into(), chain.into(), "-n".into()])? {
        return Ok(());
    }
    run_iptables_owned(&["-N".into(), chain.into()])
}

#[cfg(target_os = "linux")]
fn flush_chain(chain: &str) -> ExecutionManagerResult<()> {
    run_iptables_owned(&["-F".into(), chain.into()])
}

#[cfg(target_os = "linux")]
fn run_iptables_owned(args: &[String]) -> ExecutionManagerResult<()> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = Command::new("iptables")
        .args(&refs)
        .output()
        .map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to execute `iptables {}`: {error}",
                refs.join(" ")
            ))
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(ExecutionManagerError::Unavailable(format!(
        "`iptables {}` failed: {}",
        refs.join(" "),
        stderr.trim()
    )))
}

#[cfg(target_os = "linux")]
fn iptables_check_owned(args: &[String]) -> ExecutionManagerResult<bool> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = Command::new("iptables")
        .args(&refs)
        .output()
        .map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to query `iptables {}`: {error}",
                refs.join(" ")
            ))
        })?;
    Ok(output.status.success())
}

#[cfg(target_os = "linux")]
fn iptables_delete_if_present_owned(args: &[String]) -> ExecutionManagerResult<()> {
    let mut check_args = args.to_vec();
    if let Some(action) = check_args.iter_mut().find(|arg| arg.as_str() == "-D") {
        *action = "-C".into();
    } else {
        return Err(ExecutionManagerError::Internal(
            "iptables delete args missing -D action".into(),
        ));
    }
    if !iptables_check_owned(&check_args)? {
        return Ok(());
    }
    run_iptables_owned(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_name_is_stable_and_short() {
        let a = egress_filter_chain_name("a3sbdeadbeef");
        let b = egress_filter_chain_name("a3sbdeadbeef");
        assert_eq!(a, b);
        assert!(a.starts_with("A3SSE"));
        assert_eq!(a.len(), 13);
        assert_ne!(
            egress_filter_chain_name("a3sbdeadbeef"),
            egress_filter_chain_name("a3sbcafebabe")
        );
    }

    #[test]
    fn compile_places_operator_before_default_profile() {
        let deny = EgressMatchRule::parse("deny:1.1.1.1/32").unwrap();
        let allow_tcp = EgressMatchRule::parse("allow:10.1.0.0/16:tcp:443").unwrap();
        let body = compile_sandbox_egress_chain_body("10.88.0.0/24", &[deny, allow_tcp]).unwrap();
        assert_eq!(
            body[0],
            vec!["-d", "1.1.1.1/32", "-j", "DROP"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            body[1],
            vec![
                "-d",
                "10.1.0.0/16",
                "-p",
                "tcp",
                "--dport",
                "443",
                "-j",
                "ACCEPT"
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
        );
        assert_eq!(
            body[2],
            vec!["-d", "10.88.0.0/24", "-j", "ACCEPT"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
        assert!(body.iter().any(|r| {
            r.as_slice()
                == [
                    "-d".to_string(),
                    "169.254.0.0/16".to_string(),
                    "-j".to_string(),
                    "DROP".to_string(),
                ]
        }));
        assert_eq!(
            body.last().unwrap().as_slice(),
            ["-j".to_string(), "ACCEPT".to_string()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn compile_verdicts_align_with_netproxy_untrusted_profile() {
        use a3s_box_netproxy::untrusted_egress_denied;
        use std::net::Ipv4Addr;

        let deny = EgressMatchRule::parse("deny:1.1.1.1/32").unwrap();
        let allow = EgressMatchRule::parse("allow:8.8.8.8/32").unwrap();
        let rules = vec![deny, allow];
        let attached = (Ipv4Addr::new(10, 88, 0, 0), 24);
        let body = compile_sandbox_egress_chain_body("10.88.0.0/24", &rules).unwrap();

        // Operator deny must precede default ACCEPT remainder.
        assert_eq!(body[0][body[0].len() - 1], "DROP");
        assert!(body[0].contains(&"1.1.1.1/32".to_string()));
        assert_eq!(body[1][body[1].len() - 1], "ACCEPT");
        assert!(body[1].contains(&"8.8.8.8/32".to_string()));

        let samples = [
            (Ipv4Addr::new(1, 1, 1, 1), 6, Some(443u16)),
            (Ipv4Addr::new(8, 8, 8, 8), 6, Some(443)),
            (Ipv4Addr::new(10, 88, 0, 5), 6, Some(80)),
            (Ipv4Addr::new(169, 254, 169, 254), 6, Some(80)),
            (Ipv4Addr::new(10, 0, 0, 1), 6, Some(80)),
            (Ipv4Addr::new(93, 184, 216, 34), 6, Some(80)),
        ];
        for (dest, proto, port) in samples {
            let denied = untrusted_egress_denied(dest, proto, port, Some(attached), &rules);
            let chain_denied = simulate_chain_deny(&body, dest, proto, port);
            assert_eq!(
                denied, chain_denied,
                "iptables compile must match netproxy for {dest}"
            );
        }
    }

    /// First-match walk of compiled `-d` / `-p` / `--dport` / `-j` rows.
    #[cfg(unix)]
    fn simulate_chain_deny(
        body: &[Vec<String>],
        dest: std::net::Ipv4Addr,
        protocol: u8,
        port: Option<u16>,
    ) -> bool {
        for rule in body {
            let mut di = 0usize;
            let mut matched = true;
            let mut action_drop = false;
            while di < rule.len() {
                match rule[di].as_str() {
                    "-d" => {
                        let cidr = &rule[di + 1];
                        di += 2;
                        let (net, plen) = a3s_box_core::parse_ipv4_cidr(cidr).unwrap();
                        if !a3s_box_core::ipv4_in_prefix(net, plen, dest) {
                            matched = false;
                            break;
                        }
                    }
                    "-p" => {
                        let p = rule[di + 1].as_str();
                        di += 2;
                        let want = match p {
                            "tcp" => 6u8,
                            "udp" => 17u8,
                            _ => {
                                matched = false;
                                break;
                            }
                        };
                        if protocol != want {
                            matched = false;
                            break;
                        }
                    }
                    "--dport" => {
                        let want: u16 = rule[di + 1].parse().unwrap();
                        di += 2;
                        if port != Some(want) {
                            matched = false;
                            break;
                        }
                    }
                    "-j" => {
                        action_drop = rule[di + 1] == "DROP";
                        di += 2;
                    }
                    other => panic!("unexpected iptables token {other}"),
                }
            }
            if matched {
                return action_drop;
            }
        }
        false
    }

    #[test]
    fn forward_jump_matches_masquerade_selector() {
        let jump = compile_forward_jump_args("10.88.0.0/24", "a3sbdeadbeef", "A3SSEabcdef01");
        assert_eq!(
            jump,
            vec![
                "-s",
                "10.88.0.0/24",
                "!",
                "-o",
                "a3sbdeadbeef",
                "-j",
                "A3SSEabcdef01"
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
        );
    }
}
