//! Bind Box-prepared Linux `netDevices` into `a3s.oci.attachments.v3`.
//!
//! IPAM, DNS, aliases, publication policy, and backing-network lifetime stay in
//! Box. The runtime only receives immutable namespace/interface/cleanup
//! identities plus the exact OCI mechanism already written into the bundle.

use a3s_box_core::{ExecutionManagerError, ExecutionManagerResult};
use a3s_oci_sdk::{
    CreateAttachments, NetworkAttachmentIdentity, NetworkCleanup, NetworkCleanupId,
    NetworkInterfaceId, NetworkNamespaceId, OciBundle,
};

use crate::BoxRecord;

/// Attach every prepared `linux.netDevices` entry into `a3s.oci.attachments.v3`.
///
/// Bundles without `netDevices` (today's SandboxViaOci shape) stay at v1/v2.
/// When devices are present, all of them must classify against the single OCI
/// network namespace; partial classification fails closed.
pub(super) fn attach_box_owned_linux_network(
    bundle: &OciBundle,
    attachments: CreateAttachments,
    record: &BoxRecord,
) -> ExecutionManagerResult<CreateAttachments> {
    let inventory = network_inventory(bundle)?;
    if inventory.devices.is_empty() {
        return Ok(attachments);
    }
    let Some(namespace_index) = inventory.network_namespace_index else {
        return Err(ExecutionManagerError::InvalidRequest(format!(
            "execution {} declares linux.netDevices but has no OCI network namespace",
            record.id
        )));
    };

    let cleanup = if inventory.network_namespace_has_path {
        NetworkCleanup::PreserveCallerNamespace
    } else {
        NetworkCleanup::ReleaseRuntimeNamespace
    };

    let namespace_id = format!("a3s.box.netns.{}", record.id);
    let cleanup_id = format!("a3s.box.netcleanup.{}", record.id);
    let mut attachments = attachments;
    for host_interface in &inventory.devices {
        let interface_id = format!("a3s.box.netif.{host_interface}.{}", record.id);
        let identity = NetworkAttachmentIdentity::new(
            NetworkNamespaceId::new(namespace_id.clone()).map_err(|error| {
                ExecutionManagerError::InvalidRequest(format!(
                    "network namespace identity '{namespace_id}' is invalid: {error}"
                ))
            })?,
            NetworkInterfaceId::new(interface_id.clone()).map_err(|error| {
                ExecutionManagerError::InvalidRequest(format!(
                    "network interface identity '{interface_id}' is invalid: {error}"
                ))
            })?,
            NetworkCleanupId::new(cleanup_id.clone()).map_err(|error| {
                ExecutionManagerError::InvalidRequest(format!(
                    "network cleanup identity '{cleanup_id}' is invalid: {error}"
                ))
            })?,
        );
        attachments = attachments
            .attach_linux_network_interface(
                bundle,
                namespace_index,
                host_interface,
                identity,
                cleanup,
            )
            .map_err(|error| {
                ExecutionManagerError::InvalidRequest(format!(
                    "failed to attach Box network interface '{host_interface}' for execution {}: {error}",
                    record.id
                ))
            })?;
    }
    Ok(attachments)
}

#[derive(Debug, Default)]
struct NetworkInventory {
    network_namespace_index: Option<usize>,
    network_namespace_has_path: bool,
    devices: Vec<String>,
}

fn network_inventory(bundle: &OciBundle) -> ExecutionManagerResult<NetworkInventory> {
    let configuration: serde_json::Value =
        serde_json::from_str(bundle.config_json()).map_err(|error| {
            ExecutionManagerError::Internal(format!(
                "failed to decode OCI config for network attachments: {error}"
            ))
        })?;

    let mut inventory = NetworkInventory::default();
    if let Some(namespaces) = configuration
        .pointer("/linux/namespaces")
        .and_then(|value| value.as_array())
    {
        for (index, namespace) in namespaces.iter().enumerate() {
            if namespace.get("type").and_then(|value| value.as_str()) != Some("network") {
                continue;
            }
            if inventory.network_namespace_index.is_some() {
                return Err(ExecutionManagerError::InvalidRequest(
                    "OCI bundle declares more than one network namespace for attachments.v3"
                        .to_string(),
                ));
            }
            inventory.network_namespace_index = Some(index);
            inventory.network_namespace_has_path = namespace
                .get("path")
                .and_then(|value| value.as_str())
                .is_some_and(|path| !path.is_empty());
        }
    }

    if let Some(devices) = configuration
        .pointer("/linux/netDevices")
        .and_then(|value| value.as_object())
    {
        inventory.devices = devices.keys().cloned().collect();
        inventory.devices.sort();
    }
    Ok(inventory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_oci_sdk::ProcessIo;
    use serde_json::json;
    use std::path::Path;

    fn make_record(home_dir: &Path, id: &str) -> BoxRecord {
        serde_json::from_value(json!({
            "id": id,
            "short_id": &id[..8],
            "name": "managed-network",
            "image": "alpine:latest",
            "status": "created",
            "pid": null,
            "cpus": 1,
            "memory_mb": 128,
            "volumes": [],
            "env": {},
            "cmd": ["sleep", "60"],
            "box_dir": home_dir.join("boxes").join(id),
            "console_log": home_dir.join("boxes").join(id).join("logs/console.log"),
            "created_at": "2026-07-15T00:00:00Z",
            "started_at": null,
            "auto_remove": false
        }))
        .unwrap()
    }

    fn write_bundle(home: &Path, config: serde_json::Value) -> OciBundle {
        let bundle_dir = home.join("bundle");
        std::fs::create_dir_all(bundle_dir.join("rootfs")).unwrap();
        OciBundle::from_json(&bundle_dir, serde_json::to_string(&config).unwrap()).unwrap()
    }

    #[test]
    fn leaves_schema_unchanged_without_net_devices() {
        let home = tempfile::tempdir().unwrap();
        let id = "11111111-1111-4111-8111-111111111121";
        let record = make_record(home.path(), id);
        let bundle = write_bundle(
            home.path(),
            json!({
                "ociVersion": "1.3.0",
                "root": {"path": "rootfs"},
                "process": {
                    "cwd": "/",
                    "args": ["/bin/true"],
                    "user": {"uid": 0, "gid": 0}
                },
                "linux": {
                    "namespaces": [
                        {"type": "pid"},
                        {"type": "network"}
                    ]
                }
            }),
        );
        let attachments = CreateAttachments::from_bundle(&bundle, ProcessIo::default()).unwrap();
        let attached = attach_box_owned_linux_network(&bundle, attachments, &record).unwrap();
        assert_eq!(attached.schema_version(), "a3s.oci.attachments.v1");
        assert!(attached.network_attachments().is_empty());
    }

    #[test]
    fn attaches_net_devices_and_promotes_schema_to_v3() {
        let home = tempfile::tempdir().unwrap();
        let id = "11111111-1111-4111-8111-111111111122";
        let record = make_record(home.path(), id);
        let bundle = write_bundle(
            home.path(),
            json!({
                "ociVersion": "1.3.0",
                "root": {"path": "rootfs"},
                "process": {
                    "cwd": "/",
                    "args": ["/bin/true"],
                    "user": {"uid": 0, "gid": 0}
                },
                "linux": {
                    "namespaces": [
                        {"type": "pid"},
                        {"type": "network"}
                    ],
                    "netDevices": {
                        "veth-box0": {"name": "eth0"}
                    }
                }
            }),
        );
        let attachments = CreateAttachments::from_bundle(&bundle, ProcessIo::default()).unwrap();
        let attached = attach_box_owned_linux_network(&bundle, attachments, &record).unwrap();
        assert_eq!(attached.schema_version(), "a3s.oci.attachments.v3");
        assert_eq!(attached.network_attachments().len(), 1);
        let network = &attached.network_attachments()[0];
        assert_eq!(
            network.identity().namespace().as_str(),
            format!("a3s.box.netns.{id}")
        );
        assert_eq!(
            network.identity().interface().as_str(),
            format!("a3s.box.netif.veth-box0.{id}")
        );
        assert_eq!(
            network.identity().cleanup().as_str(),
            format!("a3s.box.netcleanup.{id}")
        );
        assert_eq!(network.cleanup(), NetworkCleanup::ReleaseRuntimeNamespace);
    }

    #[test]
    fn joined_namespace_uses_preserve_caller_cleanup() {
        let home = tempfile::tempdir().unwrap();
        let id = "11111111-1111-4111-8111-111111111123";
        let record = make_record(home.path(), id);
        let bundle = write_bundle(
            home.path(),
            json!({
                "ociVersion": "1.3.0",
                "root": {"path": "rootfs"},
                "process": {
                    "cwd": "/",
                    "args": ["/bin/true"],
                    "user": {"uid": 0, "gid": 0}
                },
                "linux": {
                    "namespaces": [
                        {"type": "network", "path": "/var/run/netns/box-ns"}
                    ],
                    "netDevices": {
                        "tap0": {"name": "eth0"}
                    }
                }
            }),
        );
        let attachments = CreateAttachments::from_bundle(&bundle, ProcessIo::default()).unwrap();
        let attached = attach_box_owned_linux_network(&bundle, attachments, &record).unwrap();
        assert_eq!(attached.schema_version(), "a3s.oci.attachments.v3");
        assert_eq!(
            attached.network_attachments()[0].cleanup(),
            NetworkCleanup::PreserveCallerNamespace
        );
    }

    #[test]
    fn attaches_every_net_device_under_shared_namespace_cleanup_ids() {
        let home = tempfile::tempdir().unwrap();
        let id = "11111111-1111-4111-8111-111111111124";
        let record = make_record(home.path(), id);
        let bundle = write_bundle(
            home.path(),
            json!({
                "ociVersion": "1.3.0",
                "root": {"path": "rootfs"},
                "process": {
                    "cwd": "/",
                    "args": ["/bin/true"],
                    "user": {"uid": 0, "gid": 0}
                },
                "linux": {
                    "namespaces": [
                        {"type": "network"}
                    ],
                    "netDevices": {
                        "veth-b": {"name": "eth1"},
                        "veth-a": {"name": "eth0"}
                    }
                }
            }),
        );
        let attachments = CreateAttachments::from_bundle(&bundle, ProcessIo::default()).unwrap();
        let attached = attach_box_owned_linux_network(&bundle, attachments, &record).unwrap();
        assert_eq!(attached.schema_version(), "a3s.oci.attachments.v3");
        assert_eq!(attached.network_attachments().len(), 2);
        assert_eq!(
            attached.network_attachments()[0]
                .identity()
                .interface()
                .as_str(),
            format!("a3s.box.netif.veth-a.{id}")
        );
        assert_eq!(
            attached.network_attachments()[1]
                .identity()
                .interface()
                .as_str(),
            format!("a3s.box.netif.veth-b.{id}")
        );
        assert_eq!(
            attached.network_attachments()[0]
                .identity()
                .namespace()
                .as_str(),
            attached.network_attachments()[1]
                .identity()
                .namespace()
                .as_str()
        );
        assert_eq!(
            attached.network_attachments()[0]
                .identity()
                .cleanup()
                .as_str(),
            attached.network_attachments()[1]
                .identity()
                .cleanup()
                .as_str()
        );
    }
}
