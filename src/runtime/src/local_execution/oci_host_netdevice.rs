//! Stage one host veth for SandboxViaOci named-bridge under keep-authority.
//!
//! IPAM stays in [`NetworkStore`]. The container end stays unbridged so OCI
//! Create can move it; the peer is attached to a Box-owned Linux bridge for L2.

use std::path::{Path, PathBuf};
use std::process::Command;

use a3s_box_core::{
    ExecutionManagerError, ExecutionManagerResult, NetworkEndpoint, NetworkMode,
    OCI_NATIVE_KEEP_NETWORK_DEVICE_AUTHORITY_ENV,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::NetworkStore;

const LEASE_SCHEMA: &str = "a3s.box.sandbox-host-netdevice.v2";
const GUEST_IFACE_NAME: &str = "eth0";

/// Durable lease for a staged host netdevice pair on a Box bridge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HostNetDeviceLease {
    pub schema: String,
    pub network: String,
    pub bridge_iface: String,
    pub container_iface: String,
    pub peer_iface: String,
    pub guest_name: String,
}

impl HostNetDeviceLease {
    fn new(
        network: &str,
        bridge_iface: String,
        container_iface: String,
        peer_iface: String,
    ) -> Self {
        Self {
            schema: LEASE_SCHEMA.to_string(),
            network: network.to_string(),
            bridge_iface,
            container_iface,
            peer_iface,
            guest_name: GUEST_IFACE_NAME.to_string(),
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
pub(crate) fn stage_for_sandbox_bundle(
    home_dir: &Path,
    box_id: &str,
    network: &NetworkMode,
) -> ExecutionManagerResult<Option<HostNetDeviceLease>> {
    let NetworkMode::Bridge {
        network: network_name,
    } = network
    else {
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
    let endpoint = config.endpoints.get(box_id).ok_or_else(|| {
        ExecutionManagerError::Unavailable(format!(
            "box '{box_id}' is not connected to network '{network_name}'; resource guard must connect before prepare"
        ))
    })?;
    let prefix_len = prefix_len_from_subnet(&config.subnet)?;
    let (container_iface, peer_iface) = interface_names(box_id)?;
    let bridge_iface = bridge_iface_name(network_name);
    let lease = HostNetDeviceLease::new(network_name, bridge_iface, container_iface, peer_iface);

    // Replace any stale lease/ifaces from a previous failed prepare.
    let _ = teardown_lease(home_dir, box_id);

    stage_veth_pair(&lease, endpoint, prefix_len, config.gateway)?;
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

    delete_link_if_present(&lease.container_iface);
    delete_link_if_present(&lease.peer_iface);
    // Network-scoped bridge is shared across endpoints; delete only when empty.
    try_delete_bridge_if_idle(&lease.bridge_iface);

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
        delete_link_if_present(&lease.container_iface);
        delete_link_if_present(&lease.peer_iface);
        return Err(error);
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
    if !link_exists(bridge_iface) {
        run_ip(&["link", "add", bridge_iface, "type", "bridge"])?;
    }
    if let Err(error) = run_ip(&["link", "set", bridge_iface, "up"]) {
        // Only delete a bridge we may have just created with no slaves yet.
        try_delete_bridge_if_idle(bridge_iface);
        return Err(error);
    }
    // Gateway lives on the bridge so peers ARP a real L2 next hop. Idempotent.
    let cidr = format!("{gateway}/{prefix_len}");
    match run_ip(&["addr", "add", &cidr, "dev", bridge_iface]) {
        Ok(()) => Ok(()),
        Err(error) if error.to_string().contains("File exists") => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn link_exists(name: &str) -> bool {
    Command::new("ip")
        .args(["link", "show", "dev", name])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn try_delete_bridge_if_idle(bridge_iface: &str) {
    if !link_exists(bridge_iface) {
        return;
    }
    // `ip -o link show master <br>` lists slaves; empty means idle fabric.
    let output = Command::new("ip")
        .args(["-o", "link", "show", "master", bridge_iface])
        .output();
    let Ok(output) = output else {
        return;
    };
    if !output.status.success() {
        return;
    }
    if !String::from_utf8_lossy(&output.stdout).trim().is_empty() {
        return;
    }
    let _ = Command::new("ip")
        .args(["link", "del", bridge_iface])
        .output();
}

#[cfg(not(target_os = "linux"))]
fn try_delete_bridge_if_idle(_bridge_iface: &str) {}

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

fn delete_link_if_present(name: &str) {
    #[cfg(target_os = "linux")]
    {
        let _ = Command::new("ip").args(["link", "del", name]).output();
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = name;
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
            bridge_iface_name("dev"),
            "bv22222222c".into(),
            "bv22222222p".into(),
        );
        persist_lease(home.path(), id, &lease).unwrap();
        let loaded: HostNetDeviceLease =
            serde_json::from_str(&std::fs::read_to_string(lease_path(home.path(), id)).unwrap())
                .unwrap();
        assert_eq!(loaded, lease);
        assert_eq!(loaded.schema, LEASE_SCHEMA);
        // Teardown without real ifaces still removes the lease file.
        teardown_lease(home.path(), id).unwrap();
        assert!(!lease_path(home.path(), id).exists());
    }

    #[test]
    fn prefix_len_parses_subnet() {
        assert_eq!(prefix_len_from_subnet("10.88.0.0/24").unwrap(), 24);
        assert!(prefix_len_from_subnet("10.88.0.0").is_err());
    }
}
