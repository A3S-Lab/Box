#[cfg(target_os = "linux")]
use std::path::PathBuf;

use a3s_box_core::ExecutionIsolation;

#[cfg(target_os = "linux")]
use super::fixture::BoxRuntimeConformanceFixture;
use super::{require, Result};

pub(super) const TMPFS_SIZE_BYTES: u64 = 4 * 1024 * 1024;

pub(super) fn require_bind_config(
    record: &crate::BoxRecord,
    target: &str,
    read_only: bool,
) -> Result<()> {
    let config = &record
        .managed_execution
        .as_ref()
        .ok_or_else(|| super::protocol("Volume fixture lost managed metadata"))?
        .request
        .config;
    let expected_suffix = format!(":{target}:{}", if read_only { "ro" } else { "rw" });
    require(
        config
            .volumes
            .iter()
            .filter(|volume| volume.ends_with(&expected_suffix))
            .count()
            == 1,
        "Runtime Volume intent changed before provider launch",
    )?;
    if record.isolation == ExecutionIsolation::Microvm {
        return Ok(());
    }

    let bundle = record.box_dir.join("sandbox/bundle/config.json");
    let value: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&bundle)
            .map_err(|error| super::external("read Volume Sandbox OCI configuration", error))?,
    )
    .map_err(|error| super::external("decode Volume Sandbox OCI configuration", error))?;
    let mount = value["mounts"]
        .as_array()
        .and_then(|mounts| {
            mounts
                .iter()
                .find(|mount| mount["destination"].as_str() == Some(target))
        })
        .ok_or_else(|| super::protocol("Sandbox OCI configuration omitted the Runtime Volume"))?;
    let options = mount["options"]
        .as_array()
        .ok_or_else(|| super::protocol("Runtime Volume has no OCI mount options"))?;
    let expected_mode = if read_only { "ro" } else { "rw" };
    require(
        mount["type"] == "bind"
            && options.iter().any(|option| option == "rbind")
            && options.iter().any(|option| option == expected_mode),
        "Sandbox OCI bind mount did not preserve Runtime Volume access mode",
    )
}

pub(super) fn require_tmpfs_config(
    record: &crate::BoxRecord,
    target: &str,
    read_only: bool,
) -> Result<()> {
    let config = &record
        .managed_execution
        .as_ref()
        .ok_or_else(|| super::protocol("tmpfs fixture lost managed metadata"))?
        .request
        .config;
    let expected = format!(
        "{target}:size={TMPFS_SIZE_BYTES},{}",
        if read_only { "ro" } else { "rw" }
    );
    require(
        config.tmpfs == vec![expected],
        "Runtime tmpfs intent changed before provider launch",
    )
}

pub(super) fn require_live_tmpfs_mount(
    record: &crate::BoxRecord,
    target: &str,
    read_only: bool,
) -> Result<()> {
    require_tmpfs_config(record, target, read_only)?;
    if record.isolation == ExecutionIsolation::Microvm {
        return Ok(());
    }

    let bundle = record.box_dir.join("sandbox/bundle/config.json");
    let value: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&bundle)
            .map_err(|error| super::external("read tmpfs Sandbox OCI configuration", error))?,
    )
    .map_err(|error| super::external("decode tmpfs Sandbox OCI configuration", error))?;
    let mount = value["mounts"]
        .as_array()
        .and_then(|mounts| {
            mounts
                .iter()
                .find(|mount| mount["destination"].as_str() == Some(target))
        })
        .ok_or_else(|| super::protocol("Sandbox OCI configuration omitted the Runtime tmpfs"))?;
    let options = mount["options"]
        .as_array()
        .ok_or_else(|| super::protocol("Runtime tmpfs has no OCI mount options"))?;
    let expected_mode = if read_only { "ro" } else { "rw" };
    let expected_size = format!("size={TMPFS_SIZE_BYTES}");
    require(
        mount["type"] == "tmpfs"
            && options.iter().any(|option| option == expected_mode)
            && options
                .iter()
                .any(|option| option.as_str() == Some(expected_size.as_str())),
        "Sandbox OCI tmpfs did not preserve size and access mode",
    )
}

#[cfg(target_os = "linux")]
pub(super) fn sandbox_private_artifact_alias(
    fixture: &BoxRuntimeConformanceFixture,
    record: &crate::BoxRecord,
    target: &str,
) -> Result<(PathBuf, PathBuf)> {
    let alias_root = crate::sandbox::sandbox_mount_alias_root(&fixture.home_dir, &record.id);
    let bundle: serde_json::Value = serde_json::from_slice(
        &std::fs::read(record.box_dir.join("sandbox/bundle/config.json"))
            .map_err(|error| super::external("read private Artifact OCI bundle", error))?,
    )
    .map_err(|error| super::external("decode private Artifact OCI bundle", error))?;
    let alias = bundle["mounts"]
        .as_array()
        .and_then(|mounts| {
            mounts
                .iter()
                .find(|mount| mount["destination"].as_str() == Some(target))
        })
        .and_then(|mount| mount["source"].as_str())
        .map(PathBuf::from)
        .ok_or_else(|| super::protocol("private Artifact OCI mount has no source"))?;
    require(
        alias.parent() == Some(alias_root.as_path()) && alias.is_dir(),
        "private Artifact did not use its Box-owned attachment alias",
    )?;
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|error| super::external("read private Artifact mountinfo", error))?;
    require(
        mountinfo.lines().any(|line| {
            let mut fields = line.split_whitespace();
            fields.nth(4) == alias.to_str()
                && fields
                    .next()
                    .is_some_and(|options| options.split(',').any(|option| option == "ro"))
        }),
        "Box-owned private Artifact alias is not a read-only host mount",
    )?;
    Ok((alias_root, alias))
}
