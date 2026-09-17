//! Bind Box-owned storage allocations into `a3s.oci.attachments.v2`.
//!
//! Volume lookup, authorization, and deletion stay in Box. The runtime only
//! receives immutable storage identities + caller-owned detach-only lifetime.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use a3s_box_core::{ExecutionManagerError, ExecutionManagerResult};
use a3s_oci_sdk::{
    CreateAttachments, OciBundle, StorageAccessMode, StorageAttachmentId, StorageCleanup,
    StorageOwnership,
};

use crate::{BoxRecord, VolumeStore};

/// Stable storage identity for the Box-owned Sandbox workspace bind.
const WORKSPACE_STORAGE_IDENTITY: &str = "a3s.box.workspace";

/// Attach Box-owned VolumeStore mounts and the implicit `/workspace` bind into
/// `a3s.oci.attachments.v2`.
///
/// External caller bind mounts stay unclassified (OCI mount inventory only).
/// Caller ownership + DetachOnly matches Box retaining deletion authority.
pub(super) fn attach_box_owned_volume_storage(
    bundle: &OciBundle,
    attachments: CreateAttachments,
    home_dir: &Path,
    record: &BoxRecord,
    anonymous_volumes: &[String],
) -> ExecutionManagerResult<CreateAttachments> {
    let mounts = oci_bind_mounts(bundle)?;
    let mut attachments = attach_named_and_anonymous_volumes(
        bundle,
        attachments,
        home_dir,
        record,
        anonymous_volumes,
        &mounts,
    )?;
    attachments = attach_workspace_bind(bundle, attachments, record, &mounts)?;
    Ok(attachments)
}

fn attach_named_and_anonymous_volumes(
    bundle: &OciBundle,
    attachments: CreateAttachments,
    home_dir: &Path,
    record: &BoxRecord,
    anonymous_volumes: &[String],
    mounts: &[OciBindMount],
) -> ExecutionManagerResult<CreateAttachments> {
    let mut owned_names = std::collections::HashSet::new();
    for name in record.volume_names.iter().chain(anonymous_volumes.iter()) {
        owned_names.insert(name.as_str());
    }
    if owned_names.is_empty() {
        return Ok(attachments);
    }

    let store = VolumeStore::new(home_dir.join("volumes.json"), home_dir.join("volumes"));
    let volumes = store.load().map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to load volume store for storage attachments: {error}"
        ))
    })?;

    let mut by_source = HashMap::<PathBuf, String>::new();
    for name in &owned_names {
        let Some(volume) = volumes.get(*name) else {
            return Err(ExecutionManagerError::Unavailable(format!(
                "volume '{name}' required by execution {} is missing from the volume store",
                record.id
            )));
        };
        let source = PathBuf::from(&volume.mount_point)
            .canonicalize()
            .map_err(|error| {
                ExecutionManagerError::Unavailable(format!(
                    "volume '{name}' mount point {} is not reachable for storage attachment: {error}",
                    volume.mount_point
                ))
            })?;
        if let Some(previous) = by_source.insert(source.clone(), (*name).to_string()) {
            return Err(ExecutionManagerError::Internal(format!(
                "volumes '{previous}' and '{name}' share mount point {} for execution {}",
                source.display(),
                record.id
            )));
        }
    }

    let mut attachments = attachments;
    let mut attached = 0usize;
    for (index, mount) in mounts.iter().enumerate() {
        let Some(source) = mount.source.as_ref() else {
            continue;
        };
        let Ok(canonical) = source.canonicalize() else {
            continue;
        };
        let Some(volume_name) = by_source.get(&canonical) else {
            continue;
        };
        attachments = attach_storage(bundle, attachments, index, volume_name, mount.read_only)?;
        attached += 1;
    }

    if attached != by_source.len() {
        return Err(ExecutionManagerError::Unavailable(format!(
            "execution {} planned {} Box-owned volume(s) but only attached {attached} OCI bind mount(s)",
            record.id,
            by_source.len()
        )));
    }
    Ok(attachments)
}

fn attach_workspace_bind(
    bundle: &OciBundle,
    attachments: CreateAttachments,
    record: &BoxRecord,
    mounts: &[OciBindMount],
) -> ExecutionManagerResult<CreateAttachments> {
    let workspace = record.box_dir.join("workspace");
    let Ok(workspace) = workspace.canonicalize() else {
        // No materialized workspace bind (tmpfs workspace or not yet prepared).
        return Ok(attachments);
    };
    for (index, mount) in mounts.iter().enumerate() {
        let Some(source) = mount.source.as_ref() else {
            continue;
        };
        let Ok(canonical) = source.canonicalize() else {
            continue;
        };
        if canonical != workspace {
            continue;
        }
        if mount.destination.as_deref() != Some(Path::new("/workspace")) {
            return Err(ExecutionManagerError::Internal(format!(
                "execution {} workspace source is mounted at {:?} instead of /workspace",
                record.id, mount.destination
            )));
        }
        return attach_storage(
            bundle,
            attachments,
            index,
            WORKSPACE_STORAGE_IDENTITY,
            mount.read_only,
        );
    }
    Ok(attachments)
}

fn attach_storage(
    bundle: &OciBundle,
    attachments: CreateAttachments,
    index: usize,
    identity: &str,
    read_only: bool,
) -> ExecutionManagerResult<CreateAttachments> {
    let identity = StorageAttachmentId::new(identity.to_string()).map_err(|error| {
        ExecutionManagerError::InvalidRequest(format!(
            "storage identity '{identity}' is invalid: {error}"
        ))
    })?;
    let access_mode = if read_only {
        StorageAccessMode::ReadOnly
    } else {
        StorageAccessMode::ReadWrite
    };
    attachments
        .attach_storage_mount(
            bundle,
            index,
            identity,
            access_mode,
            StorageOwnership::Caller,
            StorageCleanup::DetachOnly,
        )
        .map_err(|error| {
            ExecutionManagerError::InvalidRequest(format!(
                "failed to attach Box storage at OCI mount {index}: {error}"
            ))
        })
}

#[derive(Debug)]
struct OciBindMount {
    source: Option<PathBuf>,
    destination: Option<PathBuf>,
    read_only: bool,
}

fn oci_bind_mounts(bundle: &OciBundle) -> ExecutionManagerResult<Vec<OciBindMount>> {
    let configuration: serde_json::Value =
        serde_json::from_str(bundle.config_json()).map_err(|error| {
            ExecutionManagerError::Internal(format!(
                "failed to decode OCI config for storage attachments: {error}"
            ))
        })?;
    let Some(mounts) = configuration
        .get("mounts")
        .and_then(|value| value.as_array())
    else {
        return Ok(Vec::new());
    };
    Ok(mounts
        .iter()
        .map(|mount| {
            let typ = mount.get("type").and_then(|value| value.as_str());
            let source = mount
                .get("source")
                .and_then(|value| value.as_str())
                .map(PathBuf::from);
            let destination = mount
                .get("destination")
                .and_then(|value| value.as_str())
                .map(PathBuf::from);
            let options = mount
                .get("options")
                .and_then(|value| value.as_array())
                .map(|options| {
                    options
                        .iter()
                        .filter_map(|option| option.as_str())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let read_only = options.contains(&"ro");
            OciBindMount {
                source: if typ == Some("bind") { source } else { None },
                destination,
                read_only,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::volume::VolumeConfig;
    use a3s_oci_sdk::ProcessIo;
    use serde_json::json;

    fn make_record(home_dir: &Path, id: &str) -> BoxRecord {
        serde_json::from_value(json!({
            "id": id,
            "short_id": &id[..8],
            "name": "managed-storage",
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

    #[test]
    fn attaches_named_volume_and_promotes_schema_to_v2() {
        let home = tempfile::tempdir().unwrap();
        let store = VolumeStore::new(
            home.path().join("volumes.json"),
            home.path().join("volumes"),
        );
        let volume = store.create(VolumeConfig::new("dataset-a", "")).unwrap();
        let source = PathBuf::from(&volume.mount_point).canonicalize().unwrap();

        let bundle_dir = home.path().join("bundle");
        std::fs::create_dir_all(bundle_dir.join("rootfs")).unwrap();
        let bundle = OciBundle::from_json(
            &bundle_dir,
            serde_json::to_string(&json!({
                "ociVersion": "1.3.0",
                "root": {"path": "rootfs"},
                "process": {
                    "cwd": "/",
                    "args": ["/bin/true"],
                    "user": {"uid": 0, "gid": 0}
                },
                "mounts": [
                    {
                        "destination": "/proc",
                        "type": "proc",
                        "source": "proc",
                        "options": ["nosuid", "noexec", "nodev"]
                    },
                    {
                        "destination": "/data",
                        "type": "bind",
                        "source": source,
                        "options": ["rbind", "rprivate", "nosuid", "nodev", "rw"]
                    }
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        let mut record = make_record(home.path(), "11111111-1111-4111-8111-111111111111");
        record.volume_names = vec!["dataset-a".to_string()];

        let attachments = CreateAttachments::from_bundle(&bundle, ProcessIo::default()).unwrap();
        assert_eq!(attachments.schema_version(), "a3s.oci.attachments.v1");

        let attached =
            attach_box_owned_volume_storage(&bundle, attachments, home.path(), &record, &[])
                .unwrap();
        assert_eq!(attached.schema_version(), "a3s.oci.attachments.v2");
        assert_eq!(attached.storage().len(), 1);
        assert_eq!(attached.storage()[0].identity().as_str(), "dataset-a");
        assert_eq!(
            attached.storage()[0].access_mode(),
            StorageAccessMode::ReadWrite
        );
    }

    #[test]
    fn attaches_workspace_bind_as_box_owned_storage() {
        let home = tempfile::tempdir().unwrap();
        let id = "11111111-1111-4111-8111-111111111113";
        let record = make_record(home.path(), id);
        let workspace = record.box_dir.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let source = workspace.canonicalize().unwrap();

        let bundle_dir = home.path().join("bundle");
        std::fs::create_dir_all(bundle_dir.join("rootfs")).unwrap();
        let bundle = OciBundle::from_json(
            &bundle_dir,
            serde_json::to_string(&json!({
                "ociVersion": "1.3.0",
                "root": {"path": "rootfs"},
                "process": {
                    "cwd": "/",
                    "args": ["/bin/true"],
                    "user": {"uid": 0, "gid": 0}
                },
                "mounts": [{
                    "destination": "/workspace",
                    "type": "bind",
                    "source": source,
                    "options": ["rbind", "rprivate", "nosuid", "nodev", "rw"]
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let attachments = CreateAttachments::from_bundle(&bundle, ProcessIo::default()).unwrap();
        let attached =
            attach_box_owned_volume_storage(&bundle, attachments, home.path(), &record, &[])
                .unwrap();
        assert_eq!(attached.schema_version(), "a3s.oci.attachments.v2");
        assert_eq!(attached.storage().len(), 1);
        assert_eq!(
            attached.storage()[0].identity().as_str(),
            WORKSPACE_STORAGE_IDENTITY
        );
    }

    #[test]
    fn ignores_external_binds_without_volume_ownership() {
        let home = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let source = external.path().canonicalize().unwrap();
        let bundle_dir = home.path().join("bundle");
        std::fs::create_dir_all(bundle_dir.join("rootfs")).unwrap();
        let bundle = OciBundle::from_json(
            &bundle_dir,
            serde_json::to_string(&json!({
                "ociVersion": "1.3.0",
                "root": {"path": "rootfs"},
                "process": {
                    "cwd": "/",
                    "args": ["/bin/true"],
                    "user": {"uid": 0, "gid": 0}
                },
                "mounts": [{
                    "destination": "/external",
                    "type": "bind",
                    "source": source,
                    "options": ["rbind", "rw"]
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let record = make_record(home.path(), "11111111-1111-4111-8111-111111111112");
        let attachments = CreateAttachments::from_bundle(&bundle, ProcessIo::default()).unwrap();
        let attached =
            attach_box_owned_volume_storage(&bundle, attachments, home.path(), &record, &[])
                .unwrap();
        assert_eq!(attached.schema_version(), "a3s.oci.attachments.v1");
        assert!(attached.storage().is_empty());
    }
}
