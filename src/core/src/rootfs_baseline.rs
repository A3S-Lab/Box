//! Versioned guest-to-host contract for the pristine rootfs diff baseline.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

/// Host-side file name in the private lifecycle-control share.
pub const GUEST_DIFF_BASELINE_FILE_NAME: &str = "baseline.json";
/// Fixed path opened by guest-init before the lifecycle-control share is detached.
pub const GUEST_DIFF_BASELINE_PATH: &str = "/run/a3s-box/terminal/baseline.json";
/// Versioned schema emitted by guest-init and validated by the host runtime.
pub const GUEST_DIFF_BASELINE_SCHEMA: &str = "a3s.box.guest-diff-baseline.v1";
/// Bound host memory and disk consumption for one serialized baseline.
pub const MAX_GUEST_DIFF_BASELINE_BYTES: usize = 64 * 1024 * 1024;
/// Bound pathological trees independently of their serialized byte size.
pub const MAX_GUEST_DIFF_BASELINE_ENTRIES: usize = 1_000_000;

/// Minimal file metadata needed to classify rootfs changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootfsFileInfo {
    pub size: u64,
    pub mode: u32,
    pub is_dir: bool,
}

/// Pristine rootfs metadata captured inside the guest before workload launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestDiffBaseline {
    pub schema: String,
    pub entries: BTreeMap<String, RootfsFileInfo>,
}

impl GuestDiffBaseline {
    pub fn new(entries: BTreeMap<String, RootfsFileInfo>) -> Self {
        Self {
            schema: GUEST_DIFF_BASELINE_SCHEMA.to_string(),
            entries,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != GUEST_DIFF_BASELINE_SCHEMA {
            return Err(format!(
                "unsupported guest diff baseline schema: {}",
                self.schema
            ));
        }
        if self.entries.len() > MAX_GUEST_DIFF_BASELINE_ENTRIES {
            return Err(format!(
                "guest diff baseline contains {} entries; limit is {}",
                self.entries.len(),
                MAX_GUEST_DIFF_BASELINE_ENTRIES
            ));
        }
        for path in self.entries.keys() {
            validate_rootfs_path(path)?;
        }
        Ok(())
    }
}

fn validate_rootfs_path(path: &str) -> Result<(), String> {
    if path == "/" || path.contains('\0') {
        return Err(format!("invalid guest diff baseline path {path:?}"));
    }

    let parsed = Path::new(path);
    let mut components = parsed.components();
    if components.next() != Some(Component::RootDir) {
        return Err(format!(
            "guest diff baseline path is not absolute: {path:?}"
        ));
    }
    let mut names = Vec::new();
    for component in components {
        match component {
            Component::Normal(name) => names.push(name.to_string_lossy()),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(format!("guest diff baseline path is unsafe: {path:?}"));
            }
        }
    }
    let canonical = format!("/{}", names.join("/"));
    if canonical != path {
        return Err(format!(
            "guest diff baseline path is not canonical: {path:?}"
        ));
    }
    let relative = Path::new(path.trim_start_matches('/'));
    if crate::rootfs_metadata::is_runtime_internal_rootfs_path(relative) {
        return Err(format!(
            "guest diff baseline contains runtime-owned path {path:?}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versioned_baseline_accepts_canonical_guest_paths() {
        let baseline = GuestDiffBaseline::new(BTreeMap::from([(
            "/usr/bin/tool".to_string(),
            RootfsFileInfo {
                size: 7,
                mode: 0o100755,
                is_dir: false,
            },
        )]));

        assert!(baseline.validate().is_ok());
    }

    #[test]
    fn baseline_rejects_unsafe_and_runtime_owned_paths() {
        for path in [
            "relative",
            "/",
            "/usr//bin",
            "/usr/../etc/passwd",
            "/run/a3s-box/terminal/status.json",
        ] {
            let baseline = GuestDiffBaseline::new(BTreeMap::from([(
                path.to_string(),
                RootfsFileInfo {
                    size: 0,
                    mode: 0,
                    is_dir: false,
                },
            )]));
            assert!(baseline.validate().is_err(), "accepted {path:?}");
        }
    }
}
