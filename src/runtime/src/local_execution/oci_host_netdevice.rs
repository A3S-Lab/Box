//! Stage one host veth for SandboxViaOci named-bridge under keep-authority.
//!
//! IPAM stays in [`NetworkStore`]. The container end stays unbridged so OCI
//! Create can move it; the peer is attached to a Box-owned Linux bridge for L2.
//! Egress uses host `ip_forward` plus per-subnet iptables MASQUERADE. Optional
//! static TCP and UDP published ports install per-box DNAT (+ localhost OUTPUT).
//! Networks that store `--egress` rules are refused: this path does not evaluate
//! them, and attaching would ignore operator deny.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Command;

use a3s_box_core::{
    parse_port_mapping, ExecutionManagerError, ExecutionManagerResult, NetworkEndpoint,
    NetworkMode, PortProtocol, OCI_NATIVE_KEEP_NETWORK_DEVICE_AUTHORITY_ENV,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::NetworkStore;

const LEASE_SCHEMA: &str = "a3s.box.sandbox-host-netdevice.v5";
const GUEST_IFACE_NAME: &str = "eth0";

/// One static TCP host→container publication persisted on the lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublishedTcpPort {
    pub host_port: u16,
    pub guest_port: u16,
}

/// Durable lease for a staged host netdevice pair on a Box bridge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HostNetDeviceLease {
    pub schema: String,
    pub network: String,
    pub subnet: String,
    pub bridge_iface: String,
    pub container_iface: String,
    pub peer_iface: String,
    pub guest_name: String,
    pub container_ip: String,
    #[serde(default)]
    pub published_tcp: Vec<PublishedTcpPort>,
    /// Static UDP publications. Absent on v4 leases.
    #[serde(default)]
    pub published_udp: Vec<PublishedTcpPort>,
}

impl HostNetDeviceLease {
    #[allow(clippy::too_many_arguments)] // lease fields are the persisted wire shape
    fn new(
        network: &str,
        subnet: String,
        bridge_iface: String,
        container_iface: String,
        peer_iface: String,
        container_ip: Ipv4Addr,
        published_tcp: Vec<PublishedTcpPort>,
        published_udp: Vec<PublishedTcpPort>,
    ) -> Self {
        Self {
            schema: LEASE_SCHEMA.to_string(),
            network: network.to_string(),
            subnet,
            bridge_iface,
            container_iface,
            peer_iface,
            guest_name: GUEST_IFACE_NAME.to_string(),
            container_ip: container_ip.to_string(),
            published_tcp,
            published_udp,
        }
    }
}

/// Fail closed when Bridge is requested without keep-authority.
pub(crate) fn require_keep_authority_for_bridge(
    network: &NetworkMode,
) -> ExecutionManagerResult<()> {
    if !matches!(network, NetworkMode::Bridge { .. }) {
        return Ok(());
    }
    if super::oci_owner::keep_network_device_authority_active()? {
        return Ok(());
    }
    Err(ExecutionManagerError::Unavailable(format!(
        "SandboxViaOci named bridge requires {OCI_NATIVE_KEEP_NETWORK_DEVICE_AUTHORITY_ENV}=1 with matched root (euid==uid==0); GA default remains loopback-only"
    )))
}

pub(crate) fn lease_path(home_dir: &Path, box_id: &str) -> PathBuf {
    home_dir
        .join("boxes")
        .join(box_id)
        .join("sandbox")
        .join("host-netdevice.json")
}

/// IFNAMSIZ-safe names derived from the box id (`bv` + 8 hex + `c`/`p`).
pub(crate) fn interface_names(box_id: &str) -> ExecutionManagerResult<(String, String)> {
    let hex: String = box_id
        .chars()
        .filter(|ch| ch.is_ascii_hexdigit())
        .take(8)
        .map(|ch| ch.to_ascii_lowercase())
        .collect();
    if hex.len() != 8 {
        return Err(ExecutionManagerError::InvalidRequest(format!(
            "box id '{box_id}' lacks 8 hex digits for host netdevice interface names"
        )));
    }
    Ok((format!("bv{hex}c"), format!("bv{hex}p")))
}

/// Keep-authority Sandbox does not filter `--egress`. A non-empty rule list
/// must fail closed so operator allow/deny is not silently ignored.
pub(crate) fn refuse_unenforced_sandbox_egress(
    network_name: &str,
    rule_count: usize,
) -> ExecutionManagerResult<()> {
    if rule_count == 0 {
        return Ok(());
    }
    Err(ExecutionManagerError::InvalidRequest(format!(
        "network '{network_name}' has {rule_count} --egress rule(s) that Sandbox keep-authority does not enforce; refusing Bridge attach. MicroVM netproxy and Linux passt_bridge enforce those rules"
    )))
}

/// Deterministic IFNAMSIZ-safe Linux bridge name for a NetworkStore network.
pub(crate) fn bridge_iface_name(network_name: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(network_name.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!("a3sb{}", &digest[..8])
}

/// Stage one veth pair for Bridge+keep-authority; return lease when staged.
///
/// Non-Bridge modes return `None` without host mutation. The container end
/// stays unbridged (Create-movable); the peer is enslaved to the Box bridge.
/// Optional static TCP and UDP `port_map` entries install DNAT to the endpoint IP.
pub(crate) fn stage_for_sandbox_bundle(
    home_dir: &Path,
    box_id: &str,
    network: &NetworkMode,
    port_map: &[String],
) -> ExecutionManagerResult<Option<HostNetDeviceLease>> {
    let NetworkMode::Bridge {
        network: network_name,
    } = network
    else {
        if !port_map.is_empty() {
            return Err(ExecutionManagerError::InvalidRequest(
                "SandboxViaOci published ports require NetworkMode::Bridge under keep-authority"
                    .to_string(),
            ));
        }
        return Ok(None);
    };
    require_keep_authority_for_bridge(network)?;

    let store = NetworkStore::new(home_dir.join("networks.json"));
    let config = store
        .get(network_name)
        .map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to load network '{network_name}' for host netdevice staging: {error}"
            ))
        })?
        .ok_or_else(|| {
            ExecutionManagerError::Unavailable(format!(
                "network '{network_name}' not found for SandboxViaOci host netdevice staging"
            ))
        })?;
    // Operator rules live on the network object. Keep-authority MASQUERADE does
    // not evaluate them; attaching would ignore deny. MicroVM netproxy and
    // passt_bridge do evaluate them. Refuse before any host mutation.
    refuse_unenforced_sandbox_egress(network_name, config.egress.len())?;
    let endpoint = config.endpoints.get(box_id).ok_or_else(|| {
        ExecutionManagerError::Unavailable(format!(
            "box '{box_id}' is not connected to network '{network_name}'; resource guard must connect before prepare"
        ))
    })?;
    let (published_tcp, published_udp) = parse_static_published_ports(port_map)?;
    let prefix_len = prefix_len_from_subnet(&config.subnet)?;
    let (container_iface, peer_iface) = interface_names(box_id)?;
    let bridge_iface = bridge_iface_name(network_name);
    let lease = HostNetDeviceLease::new(
        network_name,
        config.subnet.clone(),
        bridge_iface,
        container_iface,
        peer_iface,
        endpoint.ip_address,
        published_tcp,
        published_udp,
    );

    // Replace any stale lease/ifaces from a previous failed prepare. Fail closed
    // if an existing publish rule or lease cannot be torn down — soft-skipping
    // here can leave duplicate DNAT beside a new lease.
    teardown_lease(home_dir, box_id)?;

    stage_veth_pair(&lease, endpoint, prefix_len, config.gateway)?;
    ensure_published_dnat(&lease)?;
    persist_lease(home_dir, box_id, &lease)?;
    Ok(Some(lease))
}

pub(crate) fn teardown_lease(home_dir: &Path, box_id: &str) -> ExecutionManagerResult<()> {
    let path = lease_path(home_dir, box_id);
    let lease = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str::<HostNetDeviceLease>(&raw).map_err(|error| {
            ExecutionManagerError::Internal(format!(
                "failed to decode host netdevice lease {}: {error}",
                path.display()
            ))
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(ExecutionManagerError::Internal(format!(
                "failed to read host netdevice lease {}: {error}",
                path.display()
            )));
        }
    };

    remove_published_dnat(&lease)?;
    delete_link_if_present(&lease.container_iface)?;
    delete_link_if_present(&lease.peer_iface)?;
    // Network-scoped bridge is shared across endpoints; delete only when empty.
    try_delete_bridge_if_idle(&lease.bridge_iface, &lease.subnet)?;

    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ExecutionManagerError::Internal(format!(
            "failed to remove host netdevice lease {}: {error}",
            path.display()
        ))),
    }
}

fn persist_lease(
    home_dir: &Path,
    box_id: &str,
    lease: &HostNetDeviceLease,
) -> ExecutionManagerResult<()> {
    let path = lease_path(home_dir, box_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            ExecutionManagerError::Internal(format!(
                "failed to create host netdevice lease directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    let json = serde_json::to_string_pretty(lease).map_err(|error| {
        ExecutionManagerError::Internal(format!(
            "failed to serialize host netdevice lease: {error}"
        ))
    })?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json).map_err(|error| {
        ExecutionManagerError::Internal(format!(
            "failed to write host netdevice lease {}: {error}",
            tmp.display()
        ))
    })?;
    std::fs::rename(&tmp, &path).map_err(|error| {
        ExecutionManagerError::Internal(format!(
            "failed to publish host netdevice lease {}: {error}",
            path.display()
        ))
    })
}

fn prefix_len_from_subnet(subnet: &str) -> ExecutionManagerResult<u8> {
    subnet
        .split('/')
        .nth(1)
        .and_then(|value| value.parse().ok())
        .filter(|value| (1..=32).contains(value))
        .ok_or_else(|| {
            ExecutionManagerError::InvalidRequest(format!(
                "network subnet '{subnet}' is missing a valid prefix length"
            ))
        })
}

#[cfg(target_os = "linux")]
fn stage_veth_pair(
    lease: &HostNetDeviceLease,
    endpoint: &NetworkEndpoint,
    prefix_len: u8,
    gateway: std::net::Ipv4Addr,
) -> ExecutionManagerResult<()> {
    ensure_bridge(&lease.bridge_iface, gateway, prefix_len)?;
    ensure_bridge_egress_nat(&lease.subnet, &lease.bridge_iface)?;
    run_ip(&[
        "link",
        "add",
        &lease.container_iface,
        "type",
        "veth",
        "peer",
        "name",
        &lease.peer_iface,
    ])?;
    let staged = (|| -> ExecutionManagerResult<()> {
        // Container end must remain free of a bridge master so OCI Create can move it.
        run_ip(&[
            "link",
            "set",
            &lease.container_iface,
            "address",
            &endpoint.mac_address,
        ])?;
        run_ip(&[
            "addr",
            "add",
            &format!("{}/{}", endpoint.ip_address, prefix_len),
            "dev",
            &lease.container_iface,
        ])?;
        run_ip(&["link", "set", &lease.container_iface, "up"])?;
        run_ip(&[
            "route",
            "replace",
            "default",
            "via",
            &gateway.to_string(),
            "dev",
            &lease.container_iface,
        ])?;
        run_ip(&[
            "link",
            "set",
            &lease.peer_iface,
            "master",
            &lease.bridge_iface,
        ])?;
        run_ip(&["link", "set", &lease.peer_iface, "up"])?;
        Ok(())
    })();
    if let Err(error) = staged {
        // No lease file yet — surface rollback failures so orphan fabric is not
        // hidden behind the primary staging error.
        let mut message = error.to_string();
        if let Err(cleanup) = delete_link_if_present(&lease.container_iface) {
            message = format!("{message}; rollback also failed: {cleanup}");
        }
        if let Err(cleanup) = delete_link_if_present(&lease.peer_iface) {
            message = format!("{message}; rollback also failed: {cleanup}");
        }
        return Err(ExecutionManagerError::Unavailable(message));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn stage_veth_pair(
    _lease: &HostNetDeviceLease,
    _endpoint: &NetworkEndpoint,
    _prefix_len: u8,
    _gateway: std::net::Ipv4Addr,
) -> ExecutionManagerResult<()> {
    Err(ExecutionManagerError::Unavailable(
        "SandboxViaOci host netdevice staging requires Linux".to_string(),
    ))
}

#[cfg(target_os = "linux")]
fn ensure_bridge(
    bridge_iface: &str,
    gateway: std::net::Ipv4Addr,
    prefix_len: u8,
) -> ExecutionManagerResult<()> {
    if !link_exists(bridge_iface)? {
        run_ip(&["link", "add", bridge_iface, "type", "bridge"])?;
    }
    if let Err(error) = run_ip(&["link", "set", bridge_iface, "up"]) {
        // Rollback a bridge we may have just created with no slaves and no
        // MASQUERADE yet (empty subnet skips NAT delete). Surface rollback
        // failure so orphan bridges are not hidden behind the primary up error.
        return Err(match try_delete_bridge_if_idle(bridge_iface, "") {
            Ok(()) => error,
            Err(cleanup) => ExecutionManagerError::Unavailable(format!(
                "{error}; rollback also failed: {cleanup}"
            )),
        });
    }
    // Gateway lives on the bridge so peers ARP a real L2 next hop. Idempotent.
    let cidr = format!("{gateway}/{prefix_len}");
    match run_ip(&["addr", "add", &cidr, "dev", bridge_iface]) {
        Ok(()) => Ok(()),
        Err(error) if error.to_string().contains("File exists") => Ok(()),
        Err(error) => Err(error),
    }
}

/// Enable host IPv4 forwarding and install subnet MASQUERADE for bridge egress.
#[cfg(target_os = "linux")]
fn ensure_bridge_egress_nat(subnet: &str, bridge_iface: &str) -> ExecutionManagerResult<()> {
    enable_ipv4_forwarding()?;
    if iptables_nat_rule_present(subnet, bridge_iface)? {
        return Ok(());
    }
    run_iptables(&[
        "-t",
        "nat",
        "-A",
        "POSTROUTING",
        "-s",
        subnet,
        "!",
        "-o",
        bridge_iface,
        "-j",
        "MASQUERADE",
    ])
}

fn parse_static_published_ports(
    port_map: &[String],
) -> ExecutionManagerResult<(Vec<PublishedTcpPort>, Vec<PublishedTcpPort>)> {
    let mut tcp = Vec::new();
    let mut udp = Vec::new();
    for entry in port_map {
        let mapping = parse_port_mapping(entry).map_err(ExecutionManagerError::InvalidRequest)?;
        if mapping.host_port == 0 {
            return Err(ExecutionManagerError::InvalidRequest(format!(
                "SandboxViaOci published ports reject host_port=0 auto-assign in '{entry}'"
            )));
        }
        let published = PublishedTcpPort {
            host_port: mapping.host_port,
            guest_port: mapping.guest_port,
        };
        match mapping.protocol {
            PortProtocol::Tcp => tcp.push(published),
            PortProtocol::Udp => udp.push(published),
        }
    }
    Ok((tcp, udp))
}

#[cfg(target_os = "linux")]
fn ensure_published_dnat(lease: &HostNetDeviceLease) -> ExecutionManagerResult<()> {
    if lease.published_tcp.is_empty() && lease.published_udp.is_empty() {
        return Ok(());
    }
    enable_ipv4_forwarding()?;
    for mapping in &lease.published_tcp {
        ensure_one_published_dnat(&lease.container_ip, "tcp", mapping)?;
    }
    for mapping in &lease.published_udp {
        ensure_one_published_dnat(&lease.container_ip, "udp", mapping)?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn ensure_published_dnat(_lease: &HostNetDeviceLease) -> ExecutionManagerResult<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn ensure_one_published_dnat(
    container_ip: &str,
    protocol: &str,
    mapping: &PublishedTcpPort,
) -> ExecutionManagerResult<()> {
    let host = mapping.host_port.to_string();
    let dest = format!("{container_ip}:{}", mapping.guest_port);
    let guest = mapping.guest_port.to_string();

    // Host LAN / non-local arrivals.
    if !iptables_check(&[
        "-t",
        "nat",
        "-C",
        "PREROUTING",
        "-p",
        protocol,
        "--dport",
        &host,
        "-j",
        "DNAT",
        "--to-destination",
        &dest,
    ])? {
        run_iptables(&[
            "-t",
            "nat",
            "-A",
            "PREROUTING",
            "-p",
            protocol,
            "--dport",
            &host,
            "-j",
            "DNAT",
            "--to-destination",
            &dest,
        ])?;
    }

    // localhost:HOST on the host.
    if !iptables_check(&[
        "-t",
        "nat",
        "-C",
        "OUTPUT",
        "-d",
        "127.0.0.1",
        "-p",
        protocol,
        "--dport",
        &host,
        "-j",
        "DNAT",
        "--to-destination",
        &dest,
    ])? {
        run_iptables(&[
            "-t",
            "nat",
            "-A",
            "OUTPUT",
            "-d",
            "127.0.0.1",
            "-p",
            protocol,
            "--dport",
            &host,
            "-j",
            "DNAT",
            "--to-destination",
            &dest,
        ])?;
    }

    // Don't assume filter FORWARD is ACCEPT.
    if !iptables_check(&[
        "-C",
        "FORWARD",
        "-d",
        container_ip,
        "-p",
        protocol,
        "--dport",
        &guest,
        "-j",
        "ACCEPT",
    ])? {
        run_iptables(&[
            "-A",
            "FORWARD",
            "-d",
            container_ip,
            "-p",
            protocol,
            "--dport",
            &guest,
            "-j",
            "ACCEPT",
        ])?;
    }
    Ok(())
}

fn remove_published_dnat(lease: &HostNetDeviceLease) -> ExecutionManagerResult<()> {
    #[cfg(target_os = "linux")]
    {
        let mut failures = Vec::new();
        for (protocol, mappings) in [("tcp", &lease.published_tcp), ("udp", &lease.published_udp)] {
            for mapping in mappings {
                let host = mapping.host_port.to_string();
                let dest = format!("{}:{}", lease.container_ip, mapping.guest_port);
                let guest = mapping.guest_port.to_string();
                if let Err(error) = iptables_delete_if_present(&[
                    "-t",
                    "nat",
                    "-D",
                    "PREROUTING",
                    "-p",
                    protocol,
                    "--dport",
                    &host,
                    "-j",
                    "DNAT",
                    "--to-destination",
                    &dest,
                ]) {
                    failures.push(error.to_string());
                }
                if let Err(error) = iptables_delete_if_present(&[
                    "-t",
                    "nat",
                    "-D",
                    "OUTPUT",
                    "-d",
                    "127.0.0.1",
                    "-p",
                    protocol,
                    "--dport",
                    &host,
                    "-j",
                    "DNAT",
                    "--to-destination",
                    &dest,
                ]) {
                    failures.push(error.to_string());
                }
                if let Err(error) = iptables_delete_if_present(&[
                    "-D",
                    "FORWARD",
                    "-d",
                    &lease.container_ip,
                    "-p",
                    protocol,
                    "--dport",
                    &guest,
                    "-j",
                    "ACCEPT",
                ]) {
                    failures.push(error.to_string());
                }
            }
        }
        if !failures.is_empty() {
            return Err(ExecutionManagerError::Unavailable(format!(
                "failed to remove keep-authority published DNAT for container {}: {}",
                lease.container_ip,
                failures.join("; ")
            )));
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = lease;
        Ok(())
    }
}

/// Delete an iptables rule when present; treat "missing rule" as success.
#[cfg(target_os = "linux")]
fn iptables_delete_if_present(delete_args: &[&str]) -> ExecutionManagerResult<()> {
    // `-D` args share the same match as `-C` after replacing the action token.
    let mut check_args = delete_args.to_vec();
    if let Some(action) = check_args.iter_mut().find(|arg| **arg == "-D") {
        *action = "-C";
    } else {
        return Err(ExecutionManagerError::Internal(
            "iptables delete args missing -D action".into(),
        ));
    }
    if !iptables_check(&check_args)? {
        return Ok(());
    }
    run_iptables(delete_args)
}

#[cfg(target_os = "linux")]
fn iptables_check(args: &[&str]) -> ExecutionManagerResult<bool> {
    let output = Command::new("iptables")
        .args(args)
        .output()
        .map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to query `iptables {}`: {error}",
                args.join(" ")
            ))
        })?;
    Ok(output.status.success())
}

#[cfg(target_os = "linux")]
fn enable_ipv4_forwarding() -> ExecutionManagerResult<()> {
    std::fs::write("/proc/sys/net/ipv4/ip_forward", b"1").map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to enable net.ipv4.ip_forward for SandboxViaOci Bridge NAT: {error}"
        ))
    })
}

#[cfg(target_os = "linux")]
fn iptables_nat_rule_present(subnet: &str, bridge_iface: &str) -> ExecutionManagerResult<bool> {
    let output = Command::new("iptables")
        .args([
            "-t",
            "nat",
            "-C",
            "POSTROUTING",
            "-s",
            subnet,
            "!",
            "-o",
            bridge_iface,
            "-j",
            "MASQUERADE",
        ])
        .output()
        .map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to query iptables MASQUERADE for {subnet}: {error}"
            ))
        })?;
    Ok(output.status.success())
}

#[cfg(target_os = "linux")]
fn remove_bridge_egress_nat(subnet: &str, bridge_iface: &str) -> ExecutionManagerResult<()> {
    if subnet.is_empty() || bridge_iface.is_empty() {
        return Ok(());
    }
    iptables_delete_if_present(&[
        "-t",
        "nat",
        "-D",
        "POSTROUTING",
        "-s",
        subnet,
        "!",
        "-o",
        bridge_iface,
        "-j",
        "MASQUERADE",
    ])
}

#[cfg(target_os = "linux")]
fn run_iptables(args: &[&str]) -> ExecutionManagerResult<()> {
    let output = Command::new("iptables")
        .args(args)
        .output()
        .map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to execute `iptables {}`: {error}",
                args.join(" ")
            ))
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(ExecutionManagerError::Unavailable(format!(
        "`iptables {}` failed: {}",
        args.join(" "),
        stderr.trim()
    )))
}

#[cfg(target_os = "linux")]
fn link_exists(name: &str) -> ExecutionManagerResult<bool> {
    let output = Command::new("ip")
        .args(["link", "show", "dev", name])
        .output()
        .map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to query `ip link show dev {name}`: {error}"
            ))
        })?;
    if output.status.success() {
        return Ok(true);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Missing device is success-absent; other ip failures must not look absent.
    if stderr.contains("does not exist")
        || stderr.contains("Cannot find device")
        || stderr.contains("No such device")
    {
        return Ok(false);
    }
    Err(ExecutionManagerError::Unavailable(format!(
        "`ip link show dev {name}` failed: {}",
        stderr.trim()
    )))
}

#[cfg(target_os = "linux")]
fn try_delete_bridge_if_idle(bridge_iface: &str, subnet: &str) -> ExecutionManagerResult<()> {
    if !link_exists(bridge_iface)? {
        return remove_bridge_egress_nat(subnet, bridge_iface);
    }
    // `ip -o link show master <br>` lists slaves; empty means idle fabric.
    let output = Command::new("ip")
        .args(["-o", "link", "show", "master", bridge_iface])
        .output()
        .map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to inspect slaves for bridge {bridge_iface}: {error}"
            ))
        })?;
    if !output.status.success() {
        return Err(ExecutionManagerError::Unavailable(format!(
            "failed to list slaves for bridge {bridge_iface}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    if !String::from_utf8_lossy(&output.stdout).trim().is_empty() {
        return Ok(());
    }
    remove_bridge_egress_nat(subnet, bridge_iface)?;
    run_ip(&["link", "del", bridge_iface])
}

#[cfg(not(target_os = "linux"))]
fn try_delete_bridge_if_idle(_bridge_iface: &str, _subnet: &str) -> ExecutionManagerResult<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn run_ip(args: &[&str]) -> ExecutionManagerResult<()> {
    let output = Command::new("ip").args(args).output().map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to execute `ip {}`: {error}",
            args.join(" ")
        ))
    })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(ExecutionManagerError::Unavailable(format!(
        "`ip {}` failed: {}",
        args.join(" "),
        stderr.trim()
    )))
}

fn delete_link_if_present(name: &str) -> ExecutionManagerResult<()> {
    #[cfg(target_os = "linux")]
    {
        if !link_exists(name)? {
            return Ok(());
        }
        run_ip(&["link", "del", name])
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = name;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_names_are_ifnamsiz_safe() {
        let (c, p) = interface_names("11111111-1111-4111-8111-111111111121").unwrap();
        assert_eq!(c, "bv11111111c");
        assert_eq!(p, "bv11111111p");
        assert!(c.len() <= 15);
        assert!(p.len() <= 15);
    }

    #[test]
    fn sandbox_bridge_refuses_networks_that_carry_egress_rules() {
        assert!(refuse_unenforced_sandbox_egress("dev", 0).is_ok());
        let error = refuse_unenforced_sandbox_egress("dev", 2).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("dev") && message.contains("--egress") && message.contains("2"),
            "expected a fail-closed egress refusal, got {message}"
        );
    }

    #[test]
    fn bridge_iface_name_is_stable_and_ifnamsiz_safe() {
        let a = bridge_iface_name("dev");
        let b = bridge_iface_name("dev");
        assert_eq!(a, b);
        assert!(a.starts_with("a3sb"));
        assert_eq!(a.len(), 12);
        assert_ne!(bridge_iface_name("dev"), bridge_iface_name("prod"));
    }

    #[test]
    fn bridge_without_keep_authority_fails_closed() {
        std::env::remove_var(OCI_NATIVE_KEEP_NETWORK_DEVICE_AUTHORITY_ENV);
        let error = require_keep_authority_for_bridge(&NetworkMode::Bridge {
            network: "dev".into(),
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(OCI_NATIVE_KEEP_NETWORK_DEVICE_AUTHORITY_ENV),
            "{error}"
        );
    }

    #[test]
    fn none_network_skips_authority_gate() {
        std::env::remove_var(OCI_NATIVE_KEEP_NETWORK_DEVICE_AUTHORITY_ENV);
        require_keep_authority_for_bridge(&NetworkMode::None).unwrap();
    }

    #[test]
    fn lease_round_trip() {
        let home = tempfile::tempdir().unwrap();
        let id = "22222222-2222-4222-8222-222222222222";
        let lease = HostNetDeviceLease::new(
            "dev",
            "10.88.0.0/24".into(),
            bridge_iface_name("dev"),
            "bv22222222c".into(),
            "bv22222222p".into(),
            "10.88.0.2".parse().unwrap(),
            vec![PublishedTcpPort {
                host_port: 18080,
                guest_port: 80,
            }],
            vec![PublishedTcpPort {
                host_port: 5300,
                guest_port: 53,
            }],
        );
        persist_lease(home.path(), id, &lease).unwrap();
        let loaded: HostNetDeviceLease =
            serde_json::from_str(&std::fs::read_to_string(lease_path(home.path(), id)).unwrap())
                .unwrap();
        assert_eq!(loaded, lease);
        assert_eq!(loaded.schema, LEASE_SCHEMA);
        assert_eq!(loaded.subnet, "10.88.0.0/24");
        assert_eq!(loaded.container_ip, "10.88.0.2");
        assert_eq!(loaded.published_tcp.len(), 1);
        assert_eq!(loaded.published_udp.len(), 1);
        // Teardown without real ifaces still removes the lease file.
        teardown_lease(home.path(), id).unwrap();
        assert!(!lease_path(home.path(), id).exists());
    }

    #[test]
    fn remove_published_tcp_dnat_is_ok_when_no_rules_present() {
        let lease = HostNetDeviceLease::new(
            "dev",
            "10.88.0.0/24".into(),
            bridge_iface_name("dev"),
            "bv33333333c".into(),
            "bv33333333p".into(),
            "10.88.0.3".parse().unwrap(),
            vec![PublishedTcpPort {
                host_port: 18081,
                guest_port: 8080,
            }],
            vec![PublishedTcpPort {
                host_port: 5300,
                guest_port: 53,
            }],
        );
        // Without matching iptables rules (or without iptables on this host),
        // missing-rule deletes must not invent soft success by swallowing errors
        // after a present rule failed to delete — absence is Ok.
        remove_published_dnat(&lease).unwrap();
    }

    #[test]
    fn parse_static_published_ports_keeps_udp_and_rejects_host_port_zero() {
        let (tcp, udp) =
            parse_static_published_ports(&["8080:80".into(), "5300:53/udp".into()]).unwrap();
        assert_eq!(tcp.len(), 1);
        assert_eq!(udp[0].guest_port, 53);
        let error = parse_static_published_ports(&["0:80".into()]).unwrap_err();
        assert!(error.to_string().contains("host_port=0"), "{error}");
        let error = parse_static_published_ports(&["0:53/udp".into()]).unwrap_err();
        assert!(error.to_string().contains("host_port=0"), "{error}");
    }

    #[test]
    fn v4_lease_without_udp_field_still_loads() {
        let raw = r#"{"schema":"a3s.box.sandbox-host-netdevice.v4","network":"dev","subnet":"10.88.0.0/24","bridge_iface":"a3sb","container_iface":"bv22222222c","peer_iface":"bv22222222p","guest_name":"eth0","container_ip":"10.88.0.2","published_tcp":[]}"#;
        let loaded: HostNetDeviceLease = serde_json::from_str(raw).unwrap();
        assert!(loaded.published_udp.is_empty());
    }

    #[test]
    fn prefix_len_parses_subnet() {
        assert_eq!(prefix_len_from_subnet("10.88.0.0/24").unwrap(), 24);
        assert!(prefix_len_from_subnet("10.88.0.0").is_err());
    }
}
