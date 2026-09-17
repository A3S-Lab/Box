//! Classify Box-managed Secret bind mounts in `CreateAttachments`.
//!
//! Secret names, values, authorization, and materialization stay in Box. The
//! runtime only receives mount-index classifications via `mark_secret_mount`.

use std::path::{Path, PathBuf};

use a3s_box_core::{ExecutionManagerError, ExecutionManagerResult};
use a3s_oci_sdk::{CreateAttachments, OciBundle};

/// Mark every OCI bind whose source is under the managed Secret root.
///
/// When `managed_secret_root` is absent, attachments are unchanged. Overlap
/// with storage classifications fails closed inside the SDK.
pub(super) fn attach_box_managed_secret_mounts(
    bundle: &OciBundle,
    attachments: CreateAttachments,
    managed_secret_root: Option<&Path>,
) -> ExecutionManagerResult<CreateAttachments> {
    let Some(root) = managed_secret_root else {
        return Ok(attachments);
    };
    let Ok(root) = root.canonicalize() else {
        return Err(ExecutionManagerError::Unavailable(format!(
            "managed Secret root {} is not reachable for attachment classification",
            root.display()
        )));
    };

    let mounts = oci_bind_mount_sources(bundle)?;
    let mut attachments = attachments;
    let mut marked = 0usize;
    for (index, source) in mounts.iter().enumerate() {
        let Some(source) = source else {
            continue;
        };
        let Ok(canonical) = source.canonicalize() else {
            continue;
        };
        if !canonical.starts_with(&root) {
            continue;
        }
        attachments = attachments.mark_secret_mount(index).map_err(|error| {
            ExecutionManagerError::InvalidRequest(format!(
                "failed to classify managed Secret mount at OCI index {index}: {error}"
            ))
        })?;
        marked += 1;
    }
    if marked == 0 {
        return Err(ExecutionManagerError::Unavailable(format!(
            "managed Secret root {} was configured but no OCI bind mounts were classified as secrets",
            root.display()
        )));
    }
    Ok(attachments)
}

fn oci_bind_mount_sources(bundle: &OciBundle) -> ExecutionManagerResult<Vec<Option<PathBuf>>> {
    let configuration: serde_json::Value =
        serde_json::from_str(bundle.config_json()).map_err(|error| {
            ExecutionManagerError::Internal(format!(
                "failed to decode OCI config for secret attachments: {error}"
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
            if typ != Some("bind") {
                return None;
            }
            mount
                .get("source")
                .and_then(|value| value.as_str())
                .map(PathBuf::from)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_oci_sdk::ProcessIo;
    use serde_json::json;

    fn write_bundle(home: &Path, config: serde_json::Value) -> OciBundle {
        let bundle_dir = home.join("bundle");
        std::fs::create_dir_all(bundle_dir.join("rootfs")).unwrap();
        OciBundle::from_json(&bundle_dir, serde_json::to_string(&config).unwrap()).unwrap()
    }

    fn secret_count(attachments: &CreateAttachments) -> usize {
        serde_json::to_value(attachments)
            .unwrap()
            .get("secrets")
            .and_then(|value| value.as_array())
            .map(|value| value.len())
            .unwrap_or(0)
    }

    #[test]
    fn leaves_attachments_unchanged_without_secret_root() {
        let home = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let source = external.path().canonicalize().unwrap();
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
                "mounts": [{
                    "destination": "/data",
                    "type": "bind",
                    "source": source,
                    "options": ["rbind", "ro"]
                }]
            }),
        );
        let attachments = CreateAttachments::from_bundle(&bundle, ProcessIo::default()).unwrap();
        let attached = attach_box_managed_secret_mounts(&bundle, attachments, None).unwrap();
        assert_eq!(secret_count(&attached), 0);
    }

    #[test]
    fn marks_bind_under_managed_secret_root() {
        let home = tempfile::tempdir().unwrap();
        let secret_root = home.path().join("runtime-secrets");
        std::fs::create_dir_all(&secret_root).unwrap();
        let secret_file = secret_root.join("token");
        std::fs::write(&secret_file, b"redacted").unwrap();
        let source = secret_file.canonicalize().unwrap();
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
                "mounts": [
                    {
                        "destination": "/proc",
                        "type": "proc",
                        "source": "proc",
                        "options": ["nosuid", "noexec", "nodev"]
                    },
                    {
                        "destination": "/run/secrets/token",
                        "type": "bind",
                        "source": source,
                        "options": ["rbind", "ro"]
                    }
                ]
            }),
        );
        let attachments = CreateAttachments::from_bundle(&bundle, ProcessIo::default()).unwrap();
        assert_eq!(secret_count(&attachments), 0);
        let attached =
            attach_box_managed_secret_mounts(&bundle, attachments, Some(&secret_root)).unwrap();
        assert_eq!(secret_count(&attached), 1);
        attached.validate(&bundle).unwrap();
    }

    #[test]
    fn ignores_binds_outside_secret_root_and_fails_if_none_match() {
        let home = tempfile::tempdir().unwrap();
        let secret_root = home.path().join("runtime-secrets");
        std::fs::create_dir_all(&secret_root).unwrap();
        let external = tempfile::tempdir().unwrap();
        let source = external.path().canonicalize().unwrap();
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
                "mounts": [{
                    "destination": "/data",
                    "type": "bind",
                    "source": source,
                    "options": ["rbind", "rw"]
                }]
            }),
        );
        let attachments = CreateAttachments::from_bundle(&bundle, ProcessIo::default()).unwrap();
        let error =
            attach_box_managed_secret_mounts(&bundle, attachments, Some(&secret_root)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no OCI bind mounts were classified as secrets"),
            "{error}"
        );
    }
}
