//! Guest-captured filesystem metadata used by stopped-box commit.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::guest_exec::RUNTIME_EXEC_CONFIG_PATH;

/// Location written inside a persistent rootfs before guest shutdown.
pub const ROOTFS_METADATA_PATH: &str = "/.a3s_rootfs_metadata_v1.json";
/// Temporary sibling used while publishing terminal metadata atomically.
pub const ROOTFS_METADATA_TEMP_PATH: &str = "/.a3s_rootfs_metadata_v1.json.tmp";
/// One-shot replay location used while a new guest generation is booting.
///
/// Before boot, the host atomically renames the last terminal manifest here.
/// A clean guest exit creates a new [`ROOTFS_METADATA_PATH`]; a crash therefore
/// leaves that canonical completion marker absent and stopped-box commit fails
/// closed instead of silently reusing metadata from an older generation.
pub const PREVIOUS_ROOTFS_METADATA_PATH: &str = "/.a3s_rootfs_metadata_v1.previous.json";
/// Location used to carry OCI header ownership across a rootless host extraction.
pub const IMAGE_ROOTFS_METADATA_PATH: &str = "/.a3s_image_metadata_v1.json";
/// Temporary sibling used while publishing immutable image metadata.
pub const IMAGE_ROOTFS_METADATA_TEMP_PATH: &str = "/.a3s_image_metadata_v1.json.tmp";
/// Runtime-staged container environment consumed by guest-init before exec.
pub const RUNTIME_ENV_PATH: &str = "/.a3s-box-env";
/// Stable manifest schema identifier.
pub const ROOTFS_METADATA_SCHEMA: &str = "a3s.box.rootfs-metadata.v1";

#[cfg(windows)]
const MAX_TERMINAL_ROOTFS_METADATA_BYTES: u64 = 64 * 1024 * 1024;
#[cfg(windows)]
const MAX_TERMINAL_ROOTFS_METADATA_ENTRIES: usize = 1_000_000;

/// Return whether a relative rootfs path belongs to runtime bookkeeping rather
/// than the container filesystem captured by commit or Snapshot.
pub fn is_runtime_internal_rootfs_path(path: &Path) -> bool {
    let mut name = None;
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(component) if name.is_none() => name = Some(component),
            _ => return false,
        }
    }
    let Some(name) = name.and_then(|name| name.to_str()) else {
        return false;
    };
    let exec_config_name = RUNTIME_EXEC_CONFIG_PATH.trim_start_matches('/');
    #[cfg(windows)]
    if name.eq_ignore_ascii_case(exec_config_name) {
        return true;
    }
    #[cfg(not(windows))]
    if name == exec_config_name {
        return true;
    }
    const INTERNAL: &[&str] = &[
        ".a3s_rootfs_metadata_v1.json",
        ".a3s_rootfs_metadata_v1.json.tmp",
        ".a3s_rootfs_metadata_v1.previous.json",
        ".a3s_image_metadata_v1.json",
        ".a3s_image_metadata_v1.json.tmp",
        ".a3s_exit_code",
        ".a3s_host_live_logs_drained",
        ".a3s_host_result_collected",
        // Written by libkrun around PID 1 startup and exit. These files can
        // change after guest-init captures terminal metadata.
        "init-rust.log",
        "init.krun.log",
        "init.trace.log",
        // Raw WHPX guest streams consumed by the host logging pipeline.
        "guest-init.stdout.log",
        "guest-init.stderr.log",
    ];
    #[cfg(windows)]
    return INTERNAL
        .iter()
        .any(|internal| name.eq_ignore_ascii_case(internal));
    #[cfg(not(windows))]
    INTERNAL.contains(&name)
}

/// Atomically invalidate the last terminal manifest before boot while retaining
/// it at the one-shot replay path.
///
/// `root` must be the plain rootfs directory, not a link/reparse point. The
/// source manifest must likewise be a regular file. These checks matter on the
/// host, where following a guest-created link could rename a path outside the
/// exported rootfs.
pub fn stage_terminal_rootfs_metadata_for_boot(root: &Path) -> std::io::Result<bool> {
    validate_plain_root(root)?;

    // A temporary manifest is scoped to exactly one guest generation. Remove
    // leftovers before every boot so a later forced stop can never publish a
    // fully-written temporary manifest from an older generation.
    let temporary = root.join(ROOTFS_METADATA_TEMP_PATH.trim_start_matches('/'));
    remove_path_no_follow(&temporary)?;

    let terminal = root.join(ROOTFS_METADATA_PATH.trim_start_matches('/'));
    let terminal_metadata = match std::fs::symlink_metadata(&terminal) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Also fence an already-staged generation. A prior rename may have
            // succeeded before its directory sync reported an error; retries
            // must not bypass that durability failure as a no-op.
            staging_directory_fence(root)?;
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    if !terminal_metadata.is_file() || metadata_is_reparse_point(&terminal_metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "terminal rootfs metadata is not a plain file: {}",
                terminal.display()
            ),
        ));
    }

    let previous = root.join(PREVIOUS_ROOTFS_METADATA_PATH.trim_start_matches('/'));
    match std::fs::symlink_metadata(&previous) {
        Ok(metadata) if metadata.is_dir() && !metadata_is_reparse_point(&metadata) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "previous rootfs metadata path is a directory: {}",
                    previous.display()
                ),
            ));
        }
        Ok(_) => remove_path_no_follow(&previous)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    durable_stage_rename(root, &terminal, &previous)?;
    Ok(true)
}

/// Validate and publish terminal metadata that a Windows WHPX guest durably
/// wrote but could not rename through virtio-fs.
///
/// This must run only after the guest process has exited. Invalid or partial
/// metadata remains unpublished so stopped-box commit continues to fail closed.
#[cfg(windows)]
pub fn finalize_terminal_rootfs_metadata(root: &Path) -> std::io::Result<bool> {
    use std::io::Read;

    validate_plain_root(root)?;

    let temporary = root.join(ROOTFS_METADATA_TEMP_PATH.trim_start_matches('/'));
    let (mut file, _) = match crate::windows_file::open_regular_file(&temporary, None) {
        Ok(opened) => opened,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let length = file.metadata()?.len();
    if length > MAX_TERMINAL_ROOTFS_METADATA_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "terminal rootfs metadata {} exceeds the {} byte limit",
                temporary.display(),
                MAX_TERMINAL_ROOTFS_METADATA_BYTES
            ),
        ));
    }

    let mut bytes = Vec::with_capacity(length as usize);
    Read::by_ref(&mut file)
        .take(MAX_TERMINAL_ROOTFS_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_TERMINAL_ROOTFS_METADATA_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "terminal rootfs metadata grew beyond the byte limit while reading",
        ));
    }

    let manifest: RootfsMetadataManifest = serde_json::from_slice(&bytes).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "invalid terminal rootfs metadata {}: {error}",
                temporary.display()
            ),
        )
    })?;
    manifest.validate().map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid terminal rootfs metadata: {error}"),
        )
    })?;
    if manifest.entries.len() > MAX_TERMINAL_ROOTFS_METADATA_ENTRIES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "terminal rootfs metadata has {} entries, exceeding the {} entry limit",
                manifest.entries.len(),
                MAX_TERMINAL_ROOTFS_METADATA_ENTRIES
            ),
        ));
    }
    drop(file);

    let terminal = root.join(ROOTFS_METADATA_PATH.trim_start_matches('/'));
    match std::fs::symlink_metadata(&terminal) {
        Ok(metadata) if !metadata.is_file() || metadata_is_reparse_point(&metadata) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "terminal rootfs metadata target is not a plain file: {}",
                    terminal.display()
                ),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    durable_stage_rename(root, &temporary, &terminal)?;
    Ok(true)
}

#[cfg(unix)]
fn staging_directory_fence(root: &Path) -> std::io::Result<()> {
    std::fs::File::open(root)?.sync_all()
}

#[cfg(not(unix))]
fn staging_directory_fence(_root: &Path) -> std::io::Result<()> {
    // Windows successful moves use MOVEFILE_WRITE_THROUGH below.
    Ok(())
}

#[cfg(unix)]
fn durable_stage_rename(root: &Path, terminal: &Path, previous: &Path) -> std::io::Result<()> {
    std::fs::rename(terminal, previous)?;
    staging_directory_fence(root)
}

#[cfg(windows)]
fn durable_stage_rename(_root: &Path, terminal: &Path, previous: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let terminal: Vec<u16> = terminal
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let previous: Vec<u16> = previous
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    if unsafe {
        MoveFileExW(
            terminal.as_ptr(),
            previous.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn durable_stage_rename(_root: &Path, terminal: &Path, previous: &Path) -> std::io::Result<()> {
    std::fs::rename(terminal, previous)
}

fn validate_plain_root(root: &Path) -> std::io::Result<()> {
    let root_metadata = std::fs::symlink_metadata(root)?;
    if !root_metadata.is_dir() || metadata_is_reparse_point(&root_metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("rootfs is not a plain directory: {}", root.display()),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn remove_path_no_follow(path: &Path) -> std::io::Result<()> {
    crate::windows_file::remove_path_no_follow(path)
}

#[cfg(not(windows))]
fn remove_path_no_follow(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Return the canonical mode for rootfs files generated by the runtime.
///
/// OCI image and terminal manifests describe the image or previous container
/// generation. The runtime rewrites these files for every launch, so replaying
/// older manifest metadata after that write would either reject the refreshed
/// guest init or make active resolver and hostname configuration inaccessible
/// to non-root image users.
pub fn runtime_managed_rootfs_mode(path: &Path) -> Option<u32> {
    match path.to_str() {
        Some("etc/hostname" | "etc/hosts" | "etc/resolv.conf") => Some(0o644),
        Some("sbin/init" | "usr/sbin/init") => Some(0o755),
        Some(".a3s-box-env") => Some(0o600),
        Some(path) if path == RUNTIME_EXEC_CONFIG_PATH.trim_start_matches('/') => Some(0o600),
        _ => None,
    }
}

/// Metadata kind supported by OCI rootfs archives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootfsEntryKind {
    Directory,
    Regular,
    Symlink,
}

/// One guest-visible filesystem entry. Paths are base64-encoded raw Unix bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootfsMetadataEntry {
    pub path_base64: String,
    pub kind: RootfsEntryKind,
    pub mode: u32,
    pub uid: u64,
    pub gid: u64,
    pub mtime: u64,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_target_base64: Option<String>,
}

/// Complete terminal metadata snapshot for one rootfs generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootfsMetadataManifest {
    pub schema: String,
    pub entries: Vec<RootfsMetadataEntry>,
}

impl RootfsMetadataManifest {
    pub fn new(entries: Vec<RootfsMetadataEntry>) -> Self {
        Self {
            schema: ROOTFS_METADATA_SCHEMA.to_string(),
            entries,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != ROOTFS_METADATA_SCHEMA {
            return Err(format!(
                "unsupported rootfs metadata schema: {}",
                self.schema
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_managed_rootfs_files_have_canonical_modes() {
        for path in ["etc/hostname", "etc/hosts", "etc/resolv.conf"] {
            assert_eq!(runtime_managed_rootfs_mode(Path::new(path)), Some(0o644));
        }
        for path in ["sbin/init", "usr/sbin/init"] {
            assert_eq!(runtime_managed_rootfs_mode(Path::new(path)), Some(0o755));
        }
        assert_eq!(
            runtime_managed_rootfs_mode(Path::new(".a3s-box-env")),
            Some(0o600)
        );
        assert_eq!(
            runtime_managed_rootfs_mode(Path::new(".a3s-box-exec.json")),
            Some(0o600)
        );
        assert_eq!(runtime_managed_rootfs_mode(Path::new("etc/passwd")), None);
    }

    #[test]
    fn runtime_internal_rootfs_paths_are_single_root_entries() {
        for path in [
            ".a3s_rootfs_metadata_v1.json",
            ".a3s_rootfs_metadata_v1.json.tmp",
            ".a3s_exit_code",
            ".a3s_host_live_logs_drained",
            ".a3s-box-exec.json",
            "guest-init.stdout.log",
            "init-rust.log",
            "init.krun.log",
            "./init.trace.log",
        ] {
            assert!(is_runtime_internal_rootfs_path(Path::new(path)), "{path}");
        }
        assert!(!is_runtime_internal_rootfs_path(Path::new(
            "var/log/init-rust.log"
        )));
        assert!(!is_runtime_internal_rootfs_path(Path::new(
            "/init-rust.log"
        )));

        #[cfg(windows)]
        assert!(is_runtime_internal_rootfs_path(Path::new("INIT-RUST.LOG")));
    }

    #[test]
    fn boot_staging_moves_terminal_metadata_to_one_shot_replay_path() {
        let root = tempfile::tempdir().unwrap();
        let terminal = root
            .path()
            .join(ROOTFS_METADATA_PATH.trim_start_matches('/'));
        let previous = root
            .path()
            .join(PREVIOUS_ROOTFS_METADATA_PATH.trim_start_matches('/'));
        std::fs::write(&terminal, b"new generation").unwrap();
        std::fs::write(&previous, b"old generation").unwrap();

        assert!(stage_terminal_rootfs_metadata_for_boot(root.path()).unwrap());
        assert!(!terminal.exists());
        assert_eq!(std::fs::read(previous).unwrap(), b"new generation");
        assert!(!stage_terminal_rootfs_metadata_for_boot(root.path()).unwrap());
    }

    #[test]
    fn boot_staging_removes_stale_temporary_metadata() {
        let root = tempfile::tempdir().unwrap();
        let temporary = root
            .path()
            .join(ROOTFS_METADATA_TEMP_PATH.trim_start_matches('/'));
        std::fs::write(&temporary, b"stale generation").unwrap();

        assert!(!stage_terminal_rootfs_metadata_for_boot(root.path()).unwrap());
        assert!(!temporary.exists());
    }

    #[test]
    fn boot_staging_accepts_an_absent_temporary_manifest() {
        let root = tempfile::tempdir().unwrap();

        assert!(!stage_terminal_rootfs_metadata_for_boot(root.path()).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn windows_finalization_publishes_valid_temporary_metadata() {
        let root = tempfile::tempdir().unwrap();
        let temporary = root
            .path()
            .join(ROOTFS_METADATA_TEMP_PATH.trim_start_matches('/'));
        let terminal = root
            .path()
            .join(ROOTFS_METADATA_PATH.trim_start_matches('/'));
        let manifest = RootfsMetadataManifest::new(Vec::new());
        std::fs::write(&temporary, serde_json::to_vec(&manifest).unwrap()).unwrap();

        assert!(finalize_terminal_rootfs_metadata(root.path()).unwrap());
        assert!(!temporary.exists());
        let published: RootfsMetadataManifest =
            serde_json::from_slice(&std::fs::read(terminal).unwrap()).unwrap();
        assert_eq!(published, manifest);
    }

    #[cfg(windows)]
    #[test]
    fn windows_finalization_rejects_partial_temporary_metadata() {
        let root = tempfile::tempdir().unwrap();
        let temporary = root
            .path()
            .join(ROOTFS_METADATA_TEMP_PATH.trim_start_matches('/'));
        let terminal = root
            .path()
            .join(ROOTFS_METADATA_PATH.trim_start_matches('/'));
        std::fs::write(&temporary, br#"{"schema":"#).unwrap();

        let error = finalize_terminal_rootfs_metadata(root.path()).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(temporary.exists());
        assert!(!terminal.exists());
    }

    #[cfg(unix)]
    #[test]
    fn boot_staging_rejects_linked_terminal_metadata() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let terminal = root
            .path()
            .join(ROOTFS_METADATA_PATH.trim_start_matches('/'));
        std::os::unix::fs::symlink(outside.path(), terminal).unwrap();

        let error = stage_terminal_rootfs_metadata_for_boot(root.path()).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
