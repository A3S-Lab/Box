//! Durable filesystem baseline used by `a3s-box diff`.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Component, Path};

use a3s_box_core::error::{BoxError, Result};
pub use a3s_box_core::rootfs_baseline::RootfsFileInfo;
use a3s_box_core::rootfs_baseline::{
    GuestDiffBaseline, GUEST_DIFF_BASELINE_FILE_NAME, MAX_GUEST_DIFF_BASELINE_BYTES,
};

/// Filename stored beside each box rootfs.
pub const DIFF_BASELINE_FILE: &str = "rootfs_snapshot.json";

/// Return whether the next guest generation must capture the first baseline.
///
/// An invalid existing destination is an error rather than a reason to replace
/// it. This preserves first-writer-wins semantics and fails before VM launch.
pub fn guest_diff_baseline_required(box_dir: &Path) -> Result<bool> {
    existing_baseline_is_regular(&box_dir.join(DIFF_BASELINE_FILE)).map(|exists| !exists)
}

/// Capture a rootfs tree while excluding runtime-owned control files.
pub fn walk_rootfs(root: &Path) -> Result<HashMap<String, RootfsFileInfo>> {
    let mut entries = HashMap::new();
    walk_recursive(root, root, &mut entries)?;
    Ok(entries)
}

/// Persist the first pristine baseline for a box without ever replacing it.
///
/// Runtime callers invoke this after all host-side rootfs preparation and before
/// the workload starts. Later starts and monitor recovery therefore preserve the
/// original generation's baseline instead of racing a fast container command.
pub fn create_diff_baseline_if_absent(box_dir: &Path, rootfs: &Path) -> Result<()> {
    if existing_baseline_is_regular(&box_dir.join(DIFF_BASELINE_FILE))? {
        return Ok(());
    }

    let entries = walk_rootfs(rootfs)?;
    let encoded = serde_json::to_vec(&entries).map_err(|error| {
        BoxError::BuildError(format!("failed to encode rootfs baseline: {error}"))
    })?;
    install_baseline_bytes_if_absent(box_dir, &encoded)
}

/// Validate and publish the baseline captured inside guest-init.
///
/// The guest writes only to a private pre-opened control file. The host treats
/// that payload as untrusted, enforces its schema and bounds, then atomically
/// installs the legacy map representation consumed by `a3s-box diff`.
pub fn publish_guest_diff_baseline(box_dir: &Path) -> Result<()> {
    let destination = box_dir.join(DIFF_BASELINE_FILE);
    if existing_baseline_is_regular(&destination)? {
        consume_guest_diff_baseline_handoff(box_dir);
        return Ok(());
    }

    let source = box_dir
        .join("runtime-control")
        .join(GUEST_DIFF_BASELINE_FILE_NAME);
    let metadata = std::fs::symlink_metadata(&source).map_err(|error| {
        BoxError::BuildError(format!(
            "failed to inspect guest diff baseline {}: {error}",
            source.display()
        ))
    })?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() == 0
        || metadata.len() > MAX_GUEST_DIFF_BASELINE_BYTES as u64
    {
        return Err(BoxError::BuildError(format!(
            "guest diff baseline is not a bounded plain file: {}",
            source.display()
        )));
    }

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let file = options.open(&source).map_err(|error| {
        BoxError::BuildError(format!(
            "failed to open guest diff baseline {}: {error}",
            source.display()
        ))
    })?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_GUEST_DIFF_BASELINE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(BoxError::IoError)?;
    if bytes.is_empty() || bytes.len() > MAX_GUEST_DIFF_BASELINE_BYTES {
        return Err(BoxError::BuildError(format!(
            "guest diff baseline exceeded its {} byte limit",
            MAX_GUEST_DIFF_BASELINE_BYTES
        )));
    }
    let baseline: GuestDiffBaseline = serde_json::from_slice(&bytes).map_err(|error| {
        BoxError::BuildError(format!("failed to decode guest diff baseline: {error}"))
    })?;
    baseline
        .validate()
        .map_err(|error| BoxError::BuildError(format!("invalid guest diff baseline: {error}")))?;
    let encoded = serde_json::to_vec(&baseline.entries).map_err(|error| {
        BoxError::BuildError(format!("failed to encode published diff baseline: {error}"))
    })?;
    install_baseline_bytes_if_absent(box_dir, &encoded)?;
    consume_guest_diff_baseline_handoff(box_dir);
    Ok(())
}

fn consume_guest_diff_baseline_handoff(box_dir: &Path) {
    let control_dir = box_dir.join("runtime-control");
    let source = control_dir.join(GUEST_DIFF_BASELINE_FILE_NAME);
    let result = match std::fs::symlink_metadata(&source) {
        Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
            std::fs::remove_file(&source).and_then(|()| sync_directory(&control_dir))
        }
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "guest diff baseline handoff is not removable as a file: {}",
                source.display()
            ),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        // The canonical baseline is already durable. Cleanup failure must not
        // turn a successful boot into a failed generation.
        tracing::warn!(
            path = %source.display(),
            error = %error,
            "Failed to consume one-shot guest diff baseline handoff"
        );
    }
}

fn existing_baseline_is_regular(destination: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(BoxError::BuildError(format!(
            "diff baseline destination is not a plain file: {}",
            destination.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(BoxError::IoError(error)),
    }
}

fn install_baseline_bytes_if_absent(box_dir: &Path, encoded: &[u8]) -> Result<()> {
    let destination = box_dir.join(DIFF_BASELINE_FILE);
    let temporary = box_dir.join(format!(
        ".{DIFF_BASELINE_FILE}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(BoxError::IoError)?;
        file.write_all(encoded).map_err(BoxError::IoError)?;
        file.sync_all().map_err(BoxError::IoError)?;
    }

    // hard_link installs the fully-written file only when the destination is
    // still absent. A concurrent lifecycle helper that won the race remains the
    // authority; unlike rename, this never overwrites its earlier baseline.
    let install = std::fs::hard_link(&temporary, &destination);
    let _ = std::fs::remove_file(&temporary);
    match install {
        Ok(()) => sync_directory(box_dir).map_err(BoxError::IoError),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            existing_baseline_is_regular(&destination).map(|_| ())
        }
        Err(error) => Err(BoxError::IoError(error)),
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn walk_recursive(
    root: &Path,
    current: &Path,
    entries: &mut HashMap<String, RootfsFileInfo>,
) -> Result<()> {
    let directory = match std::fs::read_dir(current) {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(error) => return Err(BoxError::IoError(error)),
    };

    for entry in directory {
        let entry = entry.map_err(BoxError::IoError)?;
        let path = entry.path();
        let relative = match path.strip_prefix(root) {
            Ok(relative) => relative,
            Err(_) => continue,
        };
        if a3s_box_core::rootfs_metadata::is_runtime_internal_rootfs_path(relative) {
            continue;
        }

        // Do not follow symlinks while walking a container-controlled tree.
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => continue,
            Err(error) => return Err(BoxError::IoError(error)),
        };

        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::MetadataExt;
            metadata.mode()
        };
        #[cfg(not(unix))]
        let mode = 0;

        entries.insert(
            rootfs_path_string(relative),
            RootfsFileInfo {
                size: metadata.len(),
                mode,
                is_dir: metadata.is_dir(),
            },
        );
        if metadata.is_dir() {
            walk_recursive(root, &path, entries)?;
        }
    }
    Ok(())
}

fn rootfs_path_string(relative: &Path) -> String {
    let segments = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(segment) => Some(segment.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>();
    format!("/{}", segments.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn baseline_is_first_writer_wins_and_excludes_runtime_control_files() {
        let directory = tempfile::tempdir().unwrap();
        let box_dir = directory.path().join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(rootfs.join("root")).unwrap();
        std::fs::write(rootfs.join("root/original.txt"), b"original").unwrap();
        std::fs::write(rootfs.join(".a3s-box-exec.json"), b"runtime").unwrap();

        create_diff_baseline_if_absent(&box_dir, &rootfs).unwrap();
        std::fs::write(rootfs.join("root/later.txt"), b"later").unwrap();
        create_diff_baseline_if_absent(&box_dir, &rootfs).unwrap();

        let encoded = std::fs::read(box_dir.join(DIFF_BASELINE_FILE)).unwrap();
        let baseline: HashMap<String, RootfsFileInfo> = serde_json::from_slice(&encoded).unwrap();
        assert!(baseline.contains_key("/root/original.txt"));
        assert!(!baseline.contains_key("/root/later.txt"));
        assert!(!baseline.contains_key("/.a3s-box-exec.json"));
    }

    #[test]
    fn guest_baseline_is_validated_and_published_as_legacy_map() {
        let directory = tempfile::tempdir().unwrap();
        let box_dir = directory.path().join("box");
        let control_dir = box_dir.join("runtime-control");
        std::fs::create_dir_all(&control_dir).unwrap();
        let baseline = GuestDiffBaseline::new(BTreeMap::from([(
            "/usr/bin/tool".to_string(),
            RootfsFileInfo {
                size: 17,
                mode: 0o100755,
                is_dir: false,
            },
        )]));
        std::fs::write(
            control_dir.join(GUEST_DIFF_BASELINE_FILE_NAME),
            serde_json::to_vec(&baseline).unwrap(),
        )
        .unwrap();

        publish_guest_diff_baseline(&box_dir).unwrap();

        let published: HashMap<String, RootfsFileInfo> =
            serde_json::from_slice(&std::fs::read(box_dir.join(DIFF_BASELINE_FILE)).unwrap())
                .unwrap();
        assert_eq!(
            published.get("/usr/bin/tool"),
            baseline.entries.get("/usr/bin/tool")
        );
        assert!(!control_dir.join(GUEST_DIFF_BASELINE_FILE_NAME).exists());
        assert!(!guest_diff_baseline_required(&box_dir).unwrap());
    }

    #[test]
    fn guest_baseline_is_requested_only_for_the_first_generation() {
        let directory = tempfile::tempdir().unwrap();
        let box_dir = directory.path().join("box");
        std::fs::create_dir_all(&box_dir).unwrap();

        assert!(guest_diff_baseline_required(&box_dir).unwrap());
        std::fs::write(box_dir.join(DIFF_BASELINE_FILE), b"{}").unwrap();
        assert!(!guest_diff_baseline_required(&box_dir).unwrap());
    }

    #[test]
    fn guest_baseline_never_replaces_the_first_published_generation() {
        let directory = tempfile::tempdir().unwrap();
        let box_dir = directory.path().join("box");
        let rootfs = box_dir.join("rootfs");
        let control_dir = box_dir.join("runtime-control");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&control_dir).unwrap();
        std::fs::write(rootfs.join("original"), b"one").unwrap();
        create_diff_baseline_if_absent(&box_dir, &rootfs).unwrap();

        let replacement = GuestDiffBaseline::new(BTreeMap::from([(
            "/replacement".to_string(),
            RootfsFileInfo {
                size: 3,
                mode: 0o100644,
                is_dir: false,
            },
        )]));
        std::fs::write(
            control_dir.join(GUEST_DIFF_BASELINE_FILE_NAME),
            serde_json::to_vec(&replacement).unwrap(),
        )
        .unwrap();

        publish_guest_diff_baseline(&box_dir).unwrap();

        let published: HashMap<String, RootfsFileInfo> =
            serde_json::from_slice(&std::fs::read(box_dir.join(DIFF_BASELINE_FILE)).unwrap())
                .unwrap();
        assert!(published.contains_key("/original"));
        assert!(!published.contains_key("/replacement"));
        assert!(!control_dir.join(GUEST_DIFF_BASELINE_FILE_NAME).exists());
    }

    #[test]
    fn invalid_guest_baseline_is_not_published() {
        let directory = tempfile::tempdir().unwrap();
        let box_dir = directory.path().join("box");
        let control_dir = box_dir.join("runtime-control");
        std::fs::create_dir_all(&control_dir).unwrap();
        std::fs::write(
            control_dir.join(GUEST_DIFF_BASELINE_FILE_NAME),
            br#"{"schema":"a3s.box.guest-diff-baseline.v2","entries":{}}"#,
        )
        .unwrap();

        assert!(publish_guest_diff_baseline(&box_dir).is_err());
        assert!(!box_dir.join(DIFF_BASELINE_FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn guest_baseline_control_path_must_not_be_a_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let box_dir = directory.path().join("box");
        let control_dir = box_dir.join("runtime-control");
        let outside = directory.path().join("outside.json");
        std::fs::create_dir_all(&control_dir).unwrap();
        std::fs::write(&outside, b"{}").unwrap();
        std::os::unix::fs::symlink(&outside, control_dir.join(GUEST_DIFF_BASELINE_FILE_NAME))
            .unwrap();

        assert!(publish_guest_diff_baseline(&box_dir).is_err());
        assert!(!box_dir.join(DIFF_BASELINE_FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn walk_does_not_follow_rootfs_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let outside = directory.path().join("outside");
        let rootfs = directory.path().join("rootfs");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::write(outside.join("secret"), b"secret").unwrap();
        std::os::unix::fs::symlink(&outside, rootfs.join("escape")).unwrap();

        let entries = walk_rootfs(&rootfs).unwrap();
        assert!(entries.contains_key("/escape"));
        assert!(!entries.contains_key("/escape/secret"));
    }
}
