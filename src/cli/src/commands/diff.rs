//! `a3s-box diff` command — Show filesystem changes in a box.
//!
//! Compares the box's rootfs against the original image layers to detect
//! added, changed, and deleted files, similar to `docker diff`.

use std::collections::HashMap;
use std::path::Path;

use a3s_box_runtime::rootfs::{RootfsFileInfo, DIFF_BASELINE_FILE};
use clap::Args;

use crate::resolve;
use crate::state::StateFile;

/// Change type for a filesystem entry.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ChangeKind {
    Added,
    Changed,
    Deleted,
}

impl std::fmt::Display for ChangeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangeKind::Added => write!(f, "A"),
            ChangeKind::Changed => write!(f, "C"),
            ChangeKind::Deleted => write!(f, "D"),
        }
    }
}

#[derive(Args)]
pub struct DiffArgs {
    /// Box name or ID
    pub name: String,
}

pub async fn execute(args: DiffArgs) -> Result<(), Box<dyn std::error::Error>> {
    let initial_state = StateFile::load_default()?;
    let box_id = resolve::resolve(&initial_state, &args.name)?.id.clone();
    let lifecycle_lock = crate::lifecycle::acquire_box_lifecycle_lock(&box_id).await?;
    let state = StateFile::load_default()?;
    let record = state.find_by_id(&box_id).ok_or_else(|| {
        format!(
            "Box '{}' was removed while waiting for its lifecycle lock",
            args.name
        )
    })?;

    // Snapshot the original image to compare against
    let snapshot_path = record.box_dir.join(DIFF_BASELINE_FILE);
    ensure_diff_baseline_present(&snapshot_path)?;

    let snapshot_data = std::fs::read_to_string(&snapshot_path)
        .map_err(|e| format!("Failed to read snapshot: {e}"))?;
    let baseline: HashMap<String, RootfsFileInfo> = serde_json::from_str(&snapshot_data)
        .map_err(|e| format!("Failed to parse snapshot: {e}"))?;

    // A guest-native block root has no host directory after ownership handoff.
    // Running MicroVMs stream one coherent guest-metadata archive over the exec
    // channel; SandboxViaOci walks the prepared host rootfs instead (Running or
    // freezer-Paused). Directory-backed and stopped compatibility roots keep
    // the local walk path.
    // Managed Sandbox pause uses the same lifecycle lock — release before that path.
    let release_for_sandbox_host = uses_live_sandbox_host_rootfs(record);
    let mut lifecycle_lock = Some(lifecycle_lock);
    if release_for_sandbox_host {
        drop(lifecycle_lock.take());
    }
    let current = current_rootfs(record, &args.name).await?;
    drop(lifecycle_lock);

    // Compute diff
    let mut changes = Vec::new();

    // Check for added and changed files
    for (path, info) in &current {
        match baseline.get(path) {
            None => changes.push((ChangeKind::Added, path.clone())),
            Some(base_info) => {
                if info.is_dir != base_info.is_dir
                    || info.mode != base_info.mode
                    || (!info.is_dir && info.size != base_info.size)
                {
                    changes.push((ChangeKind::Changed, path.clone()));
                }
            }
        }
    }

    // Check for deleted files
    for path in baseline.keys() {
        if !current.contains_key(path) {
            changes.push((ChangeKind::Deleted, path.clone()));
        }
    }

    changes.sort_by(|a, b| a.1.cmp(&b.1));

    if changes.is_empty() {
        println!("No changes detected.");
    } else {
        for (kind, path) in &changes {
            println!("{kind} {path}");
        }
    }

    Ok(())
}

async fn current_rootfs(
    record: &crate::state::BoxRecord,
    display_name: &str,
) -> Result<HashMap<String, RootfsFileInfo>, Box<dyn std::error::Error>> {
    if uses_live_sandbox_host_rootfs(record) {
        let live_pid = record.pid.is_some_and(|pid| {
            crate::process::is_process_alive_with_identity(pid, record.pid_start_time)
        });
        if !live_pid {
            return Err(format!(
                "Cannot diff box '{}' because its host process is not live",
                record.name
            )
            .into());
        }
        return current_sandbox_host_rootfs(record).await;
    }

    if record.status == "running" {
        let live_pid = record.pid.is_some_and(|pid| {
            crate::process::is_process_alive_with_identity(pid, record.pid_start_time)
        });
        if !live_pid {
            return Err(format!(
                "Cannot diff running box '{}' because its host process is not live",
                record.name
            )
            .into());
        }
        #[cfg(unix)]
        {
            if !record.exec_socket_path.exists() {
                return Err(format!(
                    "Cannot diff running box '{}' because its guest archive endpoint is unavailable",
                    record.name
                )
                .into());
            }
            let client = a3s_box_runtime::ExecClient::connect(&record.exec_socket_path).await?;
            let temporary = tempfile::tempdir()?;
            let archive_path = temporary.path().join("rootfs.tar");
            let mut output = tokio::fs::File::create(&archive_path).await?;
            let written = client.archive_rootfs(&mut output, true).await?;
            if written == 0 {
                return Err("Guest rootfs archive was empty".into());
            }
            output.sync_all().await?;
            drop(output);
            return walk_tar_archive(&archive_path);
        }
        #[cfg(not(unix))]
        {
            return Err(format!(
                "Live filesystem diff is unavailable for box '{}' on this platform",
                record.name
            )
            .into());
        }
    }

    if record.status == "paused" {
        return Err(format!(
            "Cannot diff paused MicroVM box '{}'; resume it first, or use a Sandbox",
            record.name
        )
        .into());
    }

    if super::rootfs_capture::stopped_sandbox_uses_managed_host_rootfs(record) {
        super::rootfs_capture::ensure_stopped_rootfs_is_unowned(record)?;
        return current_stopped_sandbox_host_rootfs(record).await;
    }

    if a3s_box_runtime::rootfs::guest_native_ext4_generation_exists(&record.box_dir)? {
        #[cfg(unix)]
        {
            let temporary = tempfile::tempdir()?;
            let archive_path = temporary.path().join("rootfs.tar");
            let mut output = tokio::fs::File::create(&archive_path).await?;
            super::rootfs_capture::archive_stopped_guest_native_rootfs(record, &mut output).await?;
            output.sync_all().await?;
            drop(output);
            return walk_tar_archive(&archive_path);
        }
        #[cfg(not(unix))]
        {
            return Err(format!(
                "Stopped guest-native rootfs diff is unavailable for box '{}' on this platform",
                record.name
            )
            .into());
        }
    }
    let rootfs_dir = super::resolve_box_rootfs(&record.box_dir).ok_or_else(|| {
        format!(
            "Rootfs not found for box '{}' under {} (looked for merged/ and rootfs/)",
            display_name,
            record.box_dir.display()
        )
    })?;
    // Match stopped commit/export: fail closed without guest rootfs metadata so
    // NTFS host mode/ownership is never presented as guest filesystem truth.
    let rootfs_metadata = super::commit::read_guest_rootfs_metadata(&rootfs_dir)
        .map_err(|error| format!("Cannot diff stopped box '{display_name}': {error}"))?;
    walk_guest_rootfs_metadata(&rootfs_metadata)
}

/// SandboxViaOci host-rootfs capture works while Running or freezer-Paused.
fn uses_live_sandbox_host_rootfs(record: &crate::state::BoxRecord) -> bool {
    record.isolation.is_sandbox() && matches!(record.status.as_str(), "running" | "paused")
}

#[cfg(all(unix, target_os = "linux"))]
async fn current_sandbox_host_rootfs(
    record: &crate::state::BoxRecord,
) -> Result<HashMap<String, RootfsFileInfo>, Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let archive_path = temporary.path().join("rootfs.tar");
    super::commit::capture_live_host_rootfs_tar(record, &archive_path, true).await?;
    walk_tar_archive(&archive_path)
}

#[cfg(not(all(unix, target_os = "linux")))]
async fn current_sandbox_host_rootfs(
    record: &crate::state::BoxRecord,
) -> Result<HashMap<String, RootfsFileInfo>, Box<dyn std::error::Error>> {
    Err(format!(
        "Live Sandbox host-rootfs diff is unavailable for box '{}' on this platform",
        record.name
    )
    .into())
}

#[cfg(all(unix, target_os = "linux"))]
async fn current_stopped_sandbox_host_rootfs(
    record: &crate::state::BoxRecord,
) -> Result<HashMap<String, RootfsFileInfo>, Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let archive_path = temporary.path().join("rootfs.tar");
    super::commit::capture_live_host_rootfs_tar(record, &archive_path, false).await?;
    walk_tar_archive(&archive_path)
}

#[cfg(not(all(unix, target_os = "linux")))]
async fn current_stopped_sandbox_host_rootfs(
    record: &crate::state::BoxRecord,
) -> Result<HashMap<String, RootfsFileInfo>, Box<dyn std::error::Error>> {
    Err(format!(
        "Stopped Sandbox host-rootfs diff is unavailable for box '{}' on this platform",
        record.name
    )
    .into())
}

/// Build a diff map from guest terminal rootfs metadata (not host NTFS mode bits).
fn walk_guest_rootfs_metadata(
    manifest: &a3s_box_core::rootfs_metadata::RootfsMetadataManifest,
) -> Result<HashMap<String, RootfsFileInfo>, Box<dyn std::error::Error>> {
    use a3s_box_core::rootfs_metadata::RootfsEntryKind;
    use base64::Engine;

    const DIRECTORY_MODE: u32 = 0o040000;
    const REGULAR_FILE_MODE: u32 = 0o100000;
    const SYMBOLIC_LINK_MODE: u32 = 0o120000;

    let mut entries = HashMap::with_capacity(manifest.entries.len());
    for entry in &manifest.entries {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&entry.path_base64)
            .map_err(|error| format!("Invalid rootfs metadata path: {error}"))?;
        let path = std::path::PathBuf::from(String::from_utf8_lossy(&bytes).as_ref());
        if a3s_box_core::rootfs_metadata::is_runtime_internal_rootfs_path(&path) {
            continue;
        }
        let Some(key) = guest_metadata_rootfs_key(&path)? else {
            continue;
        };
        let permissions = entry.mode & 0o7777;
        let (size, mode, is_dir) = match entry.kind {
            RootfsEntryKind::Directory => (0, DIRECTORY_MODE | permissions, true),
            RootfsEntryKind::Regular => (entry.size, REGULAR_FILE_MODE | permissions, false),
            RootfsEntryKind::Symlink => {
                let size = entry
                    .link_target_base64
                    .as_ref()
                    .and_then(|encoded| {
                        base64::engine::general_purpose::STANDARD
                            .decode(encoded)
                            .ok()
                    })
                    .map(|target| target.len() as u64)
                    .unwrap_or(0);
                (size, SYMBOLIC_LINK_MODE | permissions, false)
            }
        };
        entries.insert(key, RootfsFileInfo { size, mode, is_dir });
    }
    Ok(entries)
}

fn guest_metadata_rootfs_key(path: &Path) -> Result<Option<String>, String> {
    let mut segments = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::RootDir | std::path::Component::CurDir => {}
            std::path::Component::Normal(segment) => {
                segments.push(segment.to_string_lossy().into_owned())
            }
            _ => {
                return Err(format!(
                    "guest rootfs metadata contains an unsafe path: {}",
                    path.display()
                ))
            }
        }
    }
    Ok((!segments.is_empty()).then(|| format!("/{}", segments.join("/"))))
}

#[cfg(unix)]
fn walk_tar_archive(
    archive_path: &Path,
) -> Result<HashMap<String, RootfsFileInfo>, Box<dyn std::error::Error>> {
    use std::os::unix::ffi::OsStrExt;

    // POSIX/ustar file type bits are part of the archive mode contract and do
    // not depend on the host's `libc::mode_t` width.
    const DIRECTORY_MODE: u32 = 0o040000;
    const REGULAR_FILE_MODE: u32 = 0o100000;
    const SYMBOLIC_LINK_MODE: u32 = 0o120000;

    let mut archive = tar::Archive::new(std::fs::File::open(archive_path)?);
    let mut entries = HashMap::new();
    let mut hard_links = Vec::new();
    for item in archive.entries()? {
        let item = item?;
        let path = item.path()?.into_owned();
        let Some(key) = archive_rootfs_key(&path)? else {
            continue;
        };
        if a3s_box_core::rootfs_metadata::is_runtime_internal_rootfs_path(&path) {
            continue;
        }
        let header = item.header();
        let entry_type = header.entry_type();
        let permissions = header.mode()? & 0o7777;
        if entry_type.is_hard_link() {
            let target = item
                .link_name()?
                .map(|target| archive_rootfs_key(&target))
                .transpose()?
                .flatten()
                .ok_or_else(|| format!("hard-link target is missing for {key}"))?;
            hard_links.push((key, target));
            continue;
        }
        let (size, mode, is_dir) = if entry_type.is_dir() {
            (0, DIRECTORY_MODE | permissions, true)
        } else if entry_type.is_symlink() {
            let size = item
                .link_name()?
                .map(|target| target.as_os_str().as_bytes().len() as u64)
                .unwrap_or(0);
            (size, SYMBOLIC_LINK_MODE | permissions, false)
        } else if entry_type.is_file() {
            (header.size()?, REGULAR_FILE_MODE | permissions, false)
        } else {
            continue;
        };
        entries.insert(key, RootfsFileInfo { size, mode, is_dir });
    }
    for (path, target) in hard_links {
        let target = entries
            .get(&target)
            .cloned()
            .ok_or_else(|| format!("hard-link target {target} was not archived before {path}"))?;
        entries.insert(path, target);
    }
    Ok(entries)
}

#[cfg(unix)]
fn archive_rootfs_key(path: &Path) -> Result<Option<String>, String> {
    let mut segments = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(segment) => segments.push(segment.to_string_lossy()),
            _ => {
                return Err(format!(
                    "guest rootfs archive contains an unsafe path: {}",
                    path.display()
                ))
            }
        }
    }
    Ok((!segments.is_empty()).then(|| format!("/{}", segments.join("/"))))
}

/// Fail closed when `a3s-box diff` has no durable baseline to compare against.
fn ensure_diff_baseline_present(snapshot_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if snapshot_path.exists() {
        return Ok(());
    }
    Err(format!(
        "No baseline snapshot found at {} — cannot compute diff \
         (baseline is created at box creation/boot; refusing soft success)",
        snapshot_path.display()
    )
    .into())
}

/// Create the per-box baseline snapshot used by `a3s-box diff`.
///
/// The caller should invoke this after the rootfs is prepared and before user
/// mutations that should appear in later diff output. Managed Linux
/// SandboxViaOci prefers an OCI-mapped metadata baseline when mapping
/// artifacts exist (same contract as live/stopped `diff`); otherwise falls
/// back to a host walk for compatibility roots.
pub(crate) fn create_box_baseline_snapshot(
    box_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(rootfs_dir) = super::resolve_box_rootfs(box_dir) else {
        return Err(format!(
            "refusing to invent a rootfs diff baseline without a resolved rootfs under {}",
            box_dir.display()
        )
        .into());
    };
    #[cfg(target_os = "linux")]
    {
        if a3s_box_runtime::sandbox::rootfs::try_create_managed_sandbox_diff_baseline_if_absent(
            box_dir,
            &rootfs_dir,
        )? {
            return Ok(());
        }
    }
    a3s_box_runtime::rootfs::create_diff_baseline_if_absent(box_dir, &rootfs_dir)?;
    Ok(())
}

/// Compatibility alias for the CLI-level diff tests.
pub type FileInfo = RootfsFileInfo;

/// Walk a directory tree and collect file metadata, keyed by relative path.
pub fn walk_dir(root: &Path) -> Result<HashMap<String, FileInfo>, Box<dyn std::error::Error>> {
    Ok(a3s_box_runtime::rootfs::walk_rootfs(root)?)
}

/// Create a standalone baseline snapshot for diff behavior tests.
#[cfg(test)]
pub fn create_snapshot(
    rootfs_dir: &Path,
    snapshot_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let map = walk_dir(rootfs_dir)?;
    let json = serde_json::to_string(&map)?;
    std::fs::write(snapshot_path, json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_diff_baseline_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join(DIFF_BASELINE_FILE);
        let err = ensure_diff_baseline_present(&missing).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("No baseline snapshot found"),
            "unexpected message: {message}"
        );
        assert!(
            message.contains("refusing soft success"),
            "unexpected message: {message}"
        );
    }

    #[test]
    fn present_diff_baseline_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join(DIFF_BASELINE_FILE);
        std::fs::write(&present, "{}").unwrap();
        ensure_diff_baseline_present(&present).unwrap();
    }

    #[test]
    fn live_sandbox_host_rootfs_accepts_running_and_paused() {
        let mut running =
            crate::test_helpers::fixtures::make_record("id", "box", "running", Some(1));
        running.isolation = a3s_box_core::ExecutionIsolation::Sandbox;
        let mut paused = crate::test_helpers::fixtures::make_record("id", "box", "paused", Some(1));
        paused.isolation = a3s_box_core::ExecutionIsolation::Sandbox;
        let mut microvm_paused =
            crate::test_helpers::fixtures::make_record("id", "box", "paused", Some(1));
        microvm_paused.isolation = a3s_box_core::ExecutionIsolation::Microvm;

        assert!(uses_live_sandbox_host_rootfs(&running));
        assert!(uses_live_sandbox_host_rootfs(&paused));
        assert!(!uses_live_sandbox_host_rootfs(&microvm_paused));
    }

    #[test]
    fn test_walk_dir_empty() {
        let dir = tempfile::tempdir().unwrap();
        let map = walk_dir(dir.path()).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn test_walk_dir_with_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "world").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        std::fs::write(dir.path().join("subdir").join("nested.txt"), "data").unwrap();

        let map = walk_dir(dir.path()).unwrap();
        assert!(map.contains_key("/hello.txt"));
        assert!(map.contains_key("/subdir"));
        assert!(map.contains_key("/subdir/nested.txt"));
        assert_eq!(map["/hello.txt"].size, 5);
        assert!(!map["/hello.txt"].is_dir);
        assert!(map["/subdir"].is_dir);
    }

    #[cfg(unix)]
    #[test]
    fn guest_tar_walk_matches_regular_symlink_and_hardlink_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let rootfs = directory.path().join("rootfs");
        std::fs::create_dir_all(rootfs.join("subdir")).unwrap();
        std::fs::write(rootfs.join("subdir/file"), b"payload").unwrap();
        std::os::unix::fs::symlink("file", rootfs.join("subdir/link")).unwrap();
        std::fs::hard_link(rootfs.join("subdir/file"), rootfs.join("subdir/hardlink")).unwrap();
        let expected = walk_dir(&rootfs).unwrap();

        let archive_path = directory.path().join("rootfs.tar");
        let mut builder = tar::Builder::new(std::fs::File::create(&archive_path).unwrap());
        builder.follow_symlinks(false);
        builder.append_dir_all(".", &rootfs).unwrap();
        builder.finish().unwrap();
        let actual = walk_tar_archive(&archive_path).unwrap();

        for path in ["/subdir/file", "/subdir/link", "/subdir/hardlink"] {
            assert_eq!(actual.get(path), expected.get(path), "metadata for {path}");
        }
        assert_eq!(actual["/subdir"].mode, expected["/subdir"].mode);
        assert!(actual["/subdir"].is_dir);
    }

    #[test]
    fn test_create_snapshot_and_diff() {
        let dir = tempfile::tempdir().unwrap();
        let rootfs = dir.path().join("rootfs");
        std::fs::create_dir(&rootfs).unwrap();
        std::fs::write(rootfs.join("file1.txt"), "hello").unwrap();

        // Create snapshot
        let snap = dir.path().join("snapshot.json");
        create_snapshot(&rootfs, &snap).unwrap();

        // Parse it back
        let data = std::fs::read_to_string(&snap).unwrap();
        let baseline: HashMap<String, FileInfo> = serde_json::from_str(&data).unwrap();
        assert!(baseline.contains_key("/file1.txt"));
    }

    #[test]
    fn test_change_kind_display() {
        assert_eq!(format!("{}", ChangeKind::Added), "A");
        assert_eq!(format!("{}", ChangeKind::Changed), "C");
        assert_eq!(format!("{}", ChangeKind::Deleted), "D");
    }

    #[test]
    fn test_diff_detects_added() {
        let dir = tempfile::tempdir().unwrap();
        let rootfs = dir.path().join("rootfs");
        std::fs::create_dir(&rootfs).unwrap();
        std::fs::write(rootfs.join("original.txt"), "data").unwrap();

        let snap = dir.path().join("snapshot.json");
        create_snapshot(&rootfs, &snap).unwrap();

        // Add a new file
        std::fs::write(rootfs.join("new.txt"), "added").unwrap();

        let data = std::fs::read_to_string(&snap).unwrap();
        let baseline: HashMap<String, FileInfo> = serde_json::from_str(&data).unwrap();
        let current = walk_dir(&rootfs).unwrap();

        let mut added = Vec::new();
        for path in current.keys() {
            if !baseline.contains_key(path) {
                added.push(path.clone());
            }
        }
        assert!(added.contains(&"/new.txt".to_string()));
    }

    #[test]
    fn test_diff_detects_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let rootfs = dir.path().join("rootfs");
        std::fs::create_dir(&rootfs).unwrap();
        std::fs::write(rootfs.join("to_delete.txt"), "data").unwrap();

        let snap = dir.path().join("snapshot.json");
        create_snapshot(&rootfs, &snap).unwrap();

        // Delete the file
        std::fs::remove_file(rootfs.join("to_delete.txt")).unwrap();

        let data = std::fs::read_to_string(&snap).unwrap();
        let baseline: HashMap<String, FileInfo> = serde_json::from_str(&data).unwrap();
        let current = walk_dir(&rootfs).unwrap();

        let mut deleted = Vec::new();
        for path in baseline.keys() {
            if !current.contains_key(path) {
                deleted.push(path.clone());
            }
        }
        assert!(deleted.contains(&"/to_delete.txt".to_string()));
    }

    #[test]
    fn test_diff_detects_changed() {
        let dir = tempfile::tempdir().unwrap();
        let rootfs = dir.path().join("rootfs");
        std::fs::create_dir(&rootfs).unwrap();
        std::fs::write(rootfs.join("file.txt"), "short").unwrap();

        let snap = dir.path().join("snapshot.json");
        create_snapshot(&rootfs, &snap).unwrap();

        // Modify the file (different size)
        std::fs::write(rootfs.join("file.txt"), "much longer content").unwrap();

        let data = std::fs::read_to_string(&snap).unwrap();
        let baseline: HashMap<String, FileInfo> = serde_json::from_str(&data).unwrap();
        let current = walk_dir(&rootfs).unwrap();

        let mut changed = Vec::new();
        for (path, info) in &current {
            if let Some(base) = baseline.get(path) {
                if info.size != base.size {
                    changed.push(path.clone());
                }
            }
        }
        assert!(changed.contains(&"/file.txt".to_string()));
    }
}
