//! Stage one host veth for SandboxViaOci named-bridge under keep-authority.
//!
//! IPAM stays in [`NetworkStore`]. The peer stays unbridged/DOWN. Create moves
//! the container end into the runtime network namespace via `linux.netDevices`.

use std::path::{Path, PathBuf};
use std::process::Command;

use a3s_box_core::{
    ExecutionManagerError, ExecutionManagerResult, NetworkEndpoint, NetworkMode,
    OCI_NATIVE_KEEP_NETWORK_DEVICE_AUTHORITY_ENV,
};
use serde::{Deserialize, Serialize};

use crate::NetworkStore;

const LEASE_SCHEMA: &str = "a3s.box.sandbox-host-netdevice.v1";
const GUEST_IFACE_NAME: &str = "eth0";

/// Durable lease for a staged host netdevice pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HostNetDeviceLease {
    pub schema: String,
    pub network: String,
    pub container_iface: String,
    pub peer_iface: String,
    pub guest_name: String,
}

impl HostNetDeviceLease {
    fn new(network: &str, container_iface: String, peer_iface: String) -> Self {
        Self {
            schema: LEASE_SCHEMA.to_string(),
            network: network.to_string(),
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

/// Stage one veth pair for Bridge+keep-authority; return lease when staged.
///
/// Non-Bridge modes return `None` without host mutation. Peer stays DOWN and
/// unbridged so OCI Create can move the container end.
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
    let lease = HostNetDeviceLease::new(network_name, container_iface, peer_iface);

    // Replace any stale lease/ifaces from a previous failed prepare.
    let _ = teardown_lease(home_dir, box_id);

    stage_veth_pair(&lease, endpoint, prefix_len)?;
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
) -> ExecutionManagerResult<()> {
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
        run_ip(&["link", "set", &lease.peer_iface, "down"])?;
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
) -> ExecutionManagerResult<()> {
    Err(ExecutionManagerError::Unavailable(
        "SandboxViaOci host netdevice staging requires Linux".to_string(),
    ))
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
        let lease = HostNetDeviceLease::new("dev", "bv22222222c".into(), "bv22222222p".into());
        persist_lease(home.path(), id, &lease).unwrap();
        let loaded: HostNetDeviceLease =
            serde_json::from_str(&std::fs::read_to_string(lease_path(home.path(), id)).unwrap())
                .unwrap();
        assert_eq!(loaded, lease);
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
