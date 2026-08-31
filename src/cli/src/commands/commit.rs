//! `a3s-box commit` command — Create an image from a box's filesystem.
//!
//! Packages the box's rootfs into an OCI image and stores it in the
//! local image store, similar to `docker commit`.

use std::path::Path;
use std::sync::Arc;

use base64::Engine;
use clap::Args;
use sha2::{Digest, Sha256};

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct CommitArgs {
    /// Box name or ID
    pub name: String,

    /// Repository name and optionally a tag (e.g., "myimage:latest")
    pub repository: Option<String>,

    /// Commit message
    #[arg(short, long)]
    pub message: Option<String>,

    /// Author (e.g., "Name <email>")
    #[arg(short, long)]
    pub author: Option<String>,

    /// Apply Dockerfile instruction (e.g., "CMD /bin/sh")
    #[arg(short, long)]
    pub change: Vec<String>,

    /// Pause the box during commit
    #[arg(short, long, default_value = "true")]
    pub pause: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitCaptureMode {
    LiveGuest,
    OfflineDirectory,
    OfflineGuestNative,
}

pub async fn execute(args: CommitArgs) -> Result<(), Box<dyn std::error::Error>> {
    let initial_state = StateFile::load_default()?;
    let box_id = resolve::resolve(&initial_state, &args.name)?.id.clone();
    let lifecycle_lock = crate::lifecycle::acquire_box_lifecycle_lock(&box_id).await?;
    // A start/restart may have completed while this command waited. Re-read the
    // record under the same lock used by every boot path before trusting its
    // stopped state or opening any guest-controlled rootfs path.
    let state = StateFile::load_default()?;
    let record = state.find_by_id(&box_id).ok_or_else(|| {
        format!(
            "Box '{}' was removed while waiting for its lifecycle lock",
            args.name
        )
    })?;
    let capture_mode = commit_capture_mode(record)?;

    let attached_rootfs = if capture_mode != CommitCaptureMode::OfflineDirectory {
        None
    } else {
        a3s_box_runtime::rootfs::attach_persistent_rootfs(&record.box_dir)?
    };
    let rootfs_dir = if capture_mode != CommitCaptureMode::OfflineDirectory {
        None
    } else {
        Some(
            attached_rootfs
                .as_ref()
                .map(|rootfs| rootfs.path().to_path_buf())
                .or_else(|| super::resolve_box_rootfs(&record.box_dir))
                .ok_or_else(|| {
                    format!(
                        "Rootfs not found for box '{}' under {} (looked for merged/ and rootfs/). \
                         For overlay-backed boxes the filesystem is only available while the box exists; \
                         commit a running box.",
                        args.name,
                        record.box_dir.display()
                    )
                })?,
        )
    };

    let reference = args.repository.unwrap_or_else(|| {
        format!(
            "{}:latest",
            record.image.split(':').next().unwrap_or("committed")
        )
    });

    println!("Committing {}...", record.name);

    // Create a temporary directory for the OCI image layout
    let tmp = tempfile::tempdir().map_err(|e| format!("Failed to create temp dir: {e}"))?;
    let image_dir = tmp.path();
    let rootfs_tar = image_dir.join("rootfs.tar");

    capture_rootfs_tar(
        record,
        rootfs_dir.as_deref(),
        &rootfs_tar,
        args.pause,
        capture_mode,
    )
    .await?;
    // Detach an offline platform rootfs before allowing a waiting start to use
    // it, then release the lifecycle lock once no further rootfs reads occur.
    drop(attached_rootfs);
    drop(lifecycle_lock);

    // Build OCI image layout
    build_oci_image_from_tar(
        image_dir,
        &rootfs_tar,
        &reference,
        &args.message,
        &args.author,
        &args.change,
    )?;
    std::fs::remove_file(&rootfs_tar)?;

    // Compute image digest from manifest
    let manifest_bytes = std::fs::read(image_dir.join("manifest.json")).or_else(|_| {
        // Read the actual manifest blob
        find_manifest_blob(image_dir)
    })?;
    let digest = format!("sha256:{:x}", Sha256::digest(&manifest_bytes));

    // Store in image store
    let store = Arc::new(super::open_image_store()?);
    let stored = store.put(&reference, &digest, image_dir).await?;

    println!(
        "sha256:{}",
        stored
            .digest
            .strip_prefix("sha256:")
            .unwrap_or(&stored.digest)
    );

    Ok(())
}

fn commit_capture_mode(
    record: &crate::state::BoxRecord,
) -> Result<CommitCaptureMode, Box<dyn std::error::Error>> {
    let live_pid = record.pid.is_some_and(|pid| {
        crate::process::is_process_alive_with_identity(pid, record.pid_start_time)
    });
    if record.status == "running" {
        #[cfg(windows)]
        return Err(format!(
            "Windows commit requires box '{}' to be stopped because WHPX has no post-boot guest archive channel",
            record.name
        )
        .into());
        #[cfg(not(windows))]
        {
            if !live_pid {
                return Err(format!(
                    "Cannot commit running box '{}' because its host process is not live",
                    record.name
                )
                .into());
            }
            return Ok(CommitCaptureMode::LiveGuest);
        }
    }
    super::rootfs_capture::ensure_stopped_rootfs_is_unowned(record)?;
    if a3s_box_runtime::rootfs::guest_native_ext4_generation_exists(&record.box_dir)? {
        Ok(CommitCaptureMode::OfflineGuestNative)
    } else {
        Ok(CommitCaptureMode::OfflineDirectory)
    }
}

#[cfg(unix)]
async fn capture_rootfs_tar(
    record: &crate::state::BoxRecord,
    rootfs_dir: Option<&Path>,
    output: &Path,
    pause: bool,
    capture_mode: CommitCaptureMode,
) -> Result<(), Box<dyn std::error::Error>> {
    if capture_mode == CommitCaptureMode::LiveGuest && record.exec_socket_path.exists() {
        let client = a3s_box_runtime::ExecClient::connect(&record.exec_socket_path).await?;
        let mut file = tokio::fs::File::create(output).await?;
        let written = client.archive_rootfs(&mut file, pause).await?;
        if written == 0 {
            return Err("Guest rootfs archive was empty".into());
        }
        file.sync_all().await?;
        return Ok(());
    }

    if capture_mode == CommitCaptureMode::LiveGuest {
        return Err(format!(
            "Cannot commit running box '{}' because its guest archive endpoint is unavailable",
            record.name
        )
        .into());
    }

    if capture_mode == CommitCaptureMode::OfflineGuestNative {
        let mut file = tokio::fs::File::create(output).await?;
        super::rootfs_capture::archive_stopped_guest_native_rootfs(record, &mut file).await?;
        file.sync_all().await?;
        return Ok(());
    }

    let rootfs_dir = rootfs_dir.ok_or_else(|| {
        format!(
            "Cannot commit stopped box '{}' because its offline rootfs is unavailable",
            record.name
        )
    })?;
    let manifest = read_guest_rootfs_metadata(rootfs_dir)?;
    create_tar_from_guest_metadata(rootfs_dir, &manifest, output)
}

#[cfg(windows)]
async fn capture_rootfs_tar(
    record: &crate::state::BoxRecord,
    rootfs_dir: Option<&Path>,
    output: &Path,
    _pause: bool,
    capture_mode: CommitCaptureMode,
) -> Result<(), Box<dyn std::error::Error>> {
    debug_assert_eq!(capture_mode, CommitCaptureMode::OfflineDirectory);

    let rootfs_dir = rootfs_dir.ok_or_else(|| {
        format!(
            "Cannot commit stopped box '{}' because its offline rootfs is unavailable",
            record.name
        )
    })?;
    let manifest = read_guest_rootfs_metadata(rootfs_dir)?;
    create_tar_from_guest_metadata(rootfs_dir, &manifest, output)
}

const MAX_ROOTFS_METADATA_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ROOTFS_METADATA_ENTRIES: usize = 1_000_000;
const MAX_GUEST_PATH_BYTES: usize = 4096;

fn read_guest_rootfs_metadata(
    rootfs_dir: &Path,
) -> Result<a3s_box_core::rootfs_metadata::RootfsMetadataManifest, Box<dyn std::error::Error>> {
    use std::io::Read;

    let metadata_path = rootfs_dir
        .join(a3s_box_core::rootfs_metadata::ROOTFS_METADATA_PATH.trim_start_matches('/'));
    let file = open_regular_file_no_follow(&metadata_path).map_err(|error| {
        format!(
            "Guest rootfs metadata is unavailable at {}: {error}. Start the box with this A3S Box version and stop it cleanly before committing.",
            metadata_path.display()
        )
    })?;
    let length = file.metadata()?.len();
    if length > MAX_ROOTFS_METADATA_BYTES {
        return Err(format!(
            "Guest rootfs metadata at {} exceeds the {} byte limit",
            metadata_path.display(),
            MAX_ROOTFS_METADATA_BYTES
        )
        .into());
    }
    let mut bytes = Vec::with_capacity(length as usize);
    file.take(MAX_ROOTFS_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_ROOTFS_METADATA_BYTES {
        return Err("Guest rootfs metadata grew beyond the byte limit while reading".into());
    }
    let manifest: a3s_box_core::rootfs_metadata::RootfsMetadataManifest =
        serde_json::from_slice(&bytes)?;
    manifest
        .validate()
        .map_err(|error| format!("Invalid guest rootfs metadata: {error}"))?;
    if manifest.entries.len() > MAX_ROOTFS_METADATA_ENTRIES {
        return Err(format!(
            "Guest rootfs metadata has {} entries, exceeding the {} entry limit",
            manifest.entries.len(),
            MAX_ROOTFS_METADATA_ENTRIES
        )
        .into());
    }
    Ok(manifest)
}

fn create_tar_from_guest_metadata(
    rootfs_dir: &Path,
    manifest: &a3s_box_core::rootfs_metadata::RootfsMetadataManifest,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    use a3s_box_core::rootfs_metadata::RootfsEntryKind;
    use std::collections::{HashMap, HashSet};
    use std::io::Cursor;

    let mut decoded = Vec::with_capacity(manifest.entries.len());
    let mut paths = HashSet::with_capacity(manifest.entries.len());
    #[cfg(windows)]
    let mut windows_path_keys = HashSet::with_capacity(manifest.entries.len());
    for entry in &manifest.entries {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&entry.path_base64)
            .map_err(|error| format!("Invalid rootfs metadata path: {error}"))?;
        if bytes.len() > MAX_GUEST_PATH_BYTES {
            return Err("Rootfs metadata path exceeds the guest path limit".into());
        }
        let path = guest_entry_bytes_to_host_path(&bytes, "rootfs metadata path")?;
        validate_archive_path(&path)?;
        #[cfg(windows)]
        {
            let key = windows_guest_path_key(&bytes, "rootfs metadata path")?;
            if !windows_path_keys.insert(key) {
                return Err(format!(
                    "Windows-equivalent duplicate rootfs metadata path: {}",
                    path.display()
                )
                .into());
            }
        }
        if a3s_box_core::rootfs_metadata::is_runtime_internal_rootfs_path(&path) {
            return Err(format!("Reserved rootfs metadata path: {}", path.display()).into());
        }
        if !paths.insert(path.clone()) {
            return Err(format!("Duplicate rootfs metadata path: {}", path.display()).into());
        }
        decoded.push((bytes, path, entry));
    }
    decoded.sort_by(|left, right| left.0.cmp(&right.0));

    let file = std::fs::File::create(output)?;
    let mut builder = tar::Builder::new(file);
    let mut hardlinks = HashMap::<HostFileIdentity, std::path::PathBuf>::new();
    for (_, path, entry) in decoded {
        let source = resolve_source_without_link_parent(rootfs_dir, &path)?;
        let host_metadata = std::fs::symlink_metadata(&source).map_err(|error| {
            format!(
                "Rootfs changed after terminal metadata capture at {}: {error}",
                source.display()
            )
        })?;
        let mut header = tar::Header::new_gnu();
        header.set_mode(entry.mode & 0o7777);
        header.set_uid(entry.uid);
        header.set_gid(entry.gid);
        header.set_mtime(entry.mtime);

        match entry.kind {
            RootfsEntryKind::Directory => {
                if !host_metadata.file_type().is_dir() || metadata_is_reparse_point(&host_metadata)
                {
                    return Err(format!("Rootfs entry changed type: {}", path.display()).into());
                }
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                header.set_cksum();
                builder.append_data(&mut header, &path, Cursor::new([]))?;
            }
            RootfsEntryKind::Regular => {
                if !host_metadata.file_type().is_file() || metadata_is_reparse_point(&host_metadata)
                {
                    return Err(
                        format!("Rootfs entry changed after capture: {}", path.display()).into(),
                    );
                }
                let (file, identity, link_count) = open_verified_regular_file(&source, entry.size)?;
                if link_count > 1 {
                    if let Some(first_path) = identity.and_then(|id| hardlinks.get(&id)) {
                        header.set_entry_type(tar::EntryType::Link);
                        header.set_size(0);
                        header.set_link_name(first_path)?;
                        header.set_cksum();
                        builder.append_data(&mut header, &path, Cursor::new([]))?;
                        continue;
                    }
                    if let Some(identity) = identity {
                        hardlinks.insert(identity, path.clone());
                    }
                }
                header.set_entry_type(tar::EntryType::Regular);
                header.set_size(entry.size);
                header.set_cksum();
                builder.append_data(&mut header, &path, file)?;
            }
            RootfsEntryKind::Symlink => {
                if !host_metadata.file_type().is_symlink()
                    && !metadata_is_reparse_point(&host_metadata)
                {
                    return Err(format!("Rootfs entry changed type: {}", path.display()).into());
                }
                let target = entry
                    .link_target_base64
                    .as_ref()
                    .ok_or_else(|| format!("Missing symlink target: {}", path.display()))?;
                let target = base64::engine::general_purpose::STANDARD.decode(target)?;
                if target.len() > MAX_GUEST_PATH_BYTES {
                    return Err("Rootfs symlink target exceeds the guest path limit".into());
                }
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_size(0);
                append_symlink_with_raw_target(&mut builder, &mut header, &path, &target)?;
            }
        }
    }
    builder.finish()?;
    Ok(())
}

/// Append a symlink while preserving its Linux target as raw tar bytes.
///
/// A symlink target is archive data, not a host path. Converting it through a
/// Windows `PathBuf` would reject non-UTF-8 bytes and reinterpret `\` as a host
/// separator. GNU long-link records cover targets that exceed the 100-byte tar
/// header field.
fn append_symlink_with_raw_target<W: std::io::Write>(
    builder: &mut tar::Builder<W>,
    header: &mut tar::Header,
    path: &Path,
    target: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    const LINK_NAME_OFFSET: usize = 157;
    const LINK_NAME_LENGTH: usize = 100;

    if target.contains(&0) {
        return Err("Rootfs symlink target contains a NUL byte".into());
    }
    if target.len() > LINK_NAME_LENGTH {
        let mut long_header = tar::Header::new_gnu();
        long_header.set_entry_type(tar::EntryType::GNULongLink);
        long_header.set_mode(0o644);
        long_header.set_uid(0);
        long_header.set_gid(0);
        long_header.set_mtime(0);
        long_header.set_size((target.len() + 1) as u64);
        long_header.set_cksum();
        let mut contents = target.to_vec();
        contents.push(0);
        builder.append_data(
            &mut long_header,
            Path::new("././@LongLink"),
            std::io::Cursor::new(contents),
        )?;
    }

    let bytes = header.as_mut_bytes();
    let field = &mut bytes[LINK_NAME_OFFSET..LINK_NAME_OFFSET + LINK_NAME_LENGTH];
    field.fill(0);
    let inline_length = target.len().min(LINK_NAME_LENGTH);
    field[..inline_length].copy_from_slice(&target[..inline_length]);
    header.set_cksum();
    builder.append_data(header, path, std::io::Cursor::new([]))?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct HostFileIdentity(u64, u64);

#[cfg(unix)]
fn open_regular_file_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(windows)]
fn open_regular_file_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    a3s_box_core::windows_file::open_regular_file(path, None).map(|(file, _)| file)
}

#[cfg(unix)]
fn open_verified_regular_file(
    path: &Path,
    expected_size: u64,
) -> Result<(std::fs::File, Option<HostFileIdentity>, u64), Box<dyn std::error::Error>> {
    let file = open_regular_file_no_follow(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != expected_size {
        return Err(format!("Rootfs entry changed after capture: {}", path.display()).into());
    }

    use std::os::unix::fs::MetadataExt;
    Ok((
        file,
        Some(HostFileIdentity(metadata.dev(), metadata.ino())),
        metadata.nlink(),
    ))
}

#[cfg(windows)]
fn open_verified_regular_file(
    path: &Path,
    expected_size: u64,
) -> Result<(std::fs::File, Option<HostFileIdentity>, u64), Box<dyn std::error::Error>> {
    let (file, identity) = a3s_box_core::windows_file::open_regular_file(path, None)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != expected_size {
        return Err(format!("Rootfs entry changed after capture: {}", path.display()).into());
    }
    // A repeated stable file identity necessarily denotes another directory
    // entry for the same NTFS file. Checking every identity avoids relying on
    // Rust's still-unstable Windows `number_of_links` metadata extension.
    Ok((
        file,
        Some(HostFileIdentity(
            u64::from(identity.volume_serial_number),
            identity.file_id,
        )),
        2,
    ))
}

#[cfg(unix)]
fn guest_entry_bytes_to_host_path(
    bytes: &[u8],
    _description: &str,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    use std::os::unix::ffi::OsStringExt;
    Ok(std::path::PathBuf::from(std::ffi::OsString::from_vec(
        bytes.to_vec(),
    )))
}

#[cfg(windows)]
fn guest_entry_bytes_to_host_path(
    bytes: &[u8],
    description: &str,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let value = std::str::from_utf8(bytes)
        .map_err(|_| format!("{description} is not UTF-8 and cannot be represented on Windows"))?;
    validate_windows_guest_path(value, description)?;
    Ok(std::path::PathBuf::from(value))
}

#[cfg(windows)]
fn windows_guest_path_key(
    bytes: &[u8],
    description: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let value = std::str::from_utf8(bytes)
        .map_err(|_| format!("{description} is not UTF-8 and cannot be represented on Windows"))?;
    let components = validate_windows_guest_path(value, description)?;
    Ok(components
        .into_iter()
        .map(|component| component.to_lowercase())
        .collect::<Vec<_>>()
        .join("/"))
}

#[cfg(windows)]
fn validate_windows_guest_path<'a>(
    value: &'a str,
    description: &str,
) -> Result<Vec<&'a str>, Box<dyn std::error::Error>> {
    if value.is_empty() || value.starts_with('/') || value.ends_with('/') {
        return Err(format!("{description} is not a relative Linux path").into());
    }

    let mut normalized = Vec::new();
    for component in value.split('/') {
        if component == "." {
            continue;
        }
        if component.is_empty() || component == ".." {
            return Err(format!("Unsafe {description}: ambiguous path component").into());
        }
        if component.ends_with('.') || component.ends_with(' ') {
            return Err(format!(
                "{description} contains a name with a trailing dot or space that Windows aliases"
            )
            .into());
        }
        if component.chars().any(|character| {
            character <= '\u{1f}'
                || matches!(character, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*')
        }) {
            return Err(
                format!("{description} contains a name Windows cannot represent safely").into(),
            );
        }
        if is_windows_reserved_name(component) {
            return Err(format!(
                "{description} contains reserved Windows device name '{component}'"
            )
            .into());
        }
        normalized.push(component);
    }
    if normalized.is_empty() && value != "." {
        return Err(format!("{description} has no representable path component").into());
    }
    Ok(normalized)
}

#[cfg(windows)]
fn is_windows_reserved_name(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component);
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
    ) || upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"))
        .is_some_and(|suffix| suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9'))
}

fn resolve_source_without_link_parent(
    root: &Path,
    relative: &Path,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let root_metadata = std::fs::symlink_metadata(root)?;
    if !root_metadata.is_dir() || metadata_is_reparse_point(&root_metadata) {
        return Err(format!("Rootfs is not a plain directory: {}", root.display()).into());
    }

    let mut current = root.to_path_buf();
    let components: Vec<_> = relative.components().collect();
    for (index, component) in components.iter().enumerate() {
        let std::path::Component::Normal(name) = component else {
            continue;
        };
        current.push(name);
        if index + 1 < components.len() {
            let metadata = std::fs::symlink_metadata(&current)?;
            if !metadata.is_dir() || metadata_is_reparse_point(&metadata) {
                return Err(format!(
                    "Link or non-directory parent in rootfs metadata path: {}",
                    current.display()
                )
                .into());
            }
        }
    }
    Ok(current)
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

fn validate_archive_path(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err("Rootfs metadata contains an absolute or empty path".into());
    }
    if path.components().any(|component| {
        !matches!(
            component,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    }) {
        return Err(format!("Unsafe rootfs metadata path: {}", path.display()).into());
    }
    Ok(())
}

/// Find the manifest blob in the OCI layout.
fn find_manifest_blob(image_dir: &Path) -> Result<Vec<u8>, std::io::Error> {
    let index_path = image_dir.join("index.json");
    let index_data = std::fs::read_to_string(&index_path)?;
    let index: serde_json::Value = serde_json::from_str(&index_data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    if let Some(digest) = index["manifests"][0]["digest"].as_str() {
        let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
        let blob_path = image_dir.join("blobs").join("sha256").join(hex);
        std::fs::read(&blob_path)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No manifest digest in index.json",
        ))
    }
}

/// Build a minimal OCI image layout from a rootfs directory.
#[cfg(test)]
fn build_oci_image(
    output_dir: &Path,
    rootfs_dir: &Path,
    _reference: &str,
    message: &Option<String>,
    author: &Option<String>,
    changes: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let tar_path = output_dir.join("rootfs.host.tar");
    {
        let file = std::fs::File::create(&tar_path)?;
        let mut builder = tar::Builder::new(file);
        builder.follow_symlinks(false);
        builder.append_dir_all(".", rootfs_dir)?;
        builder.finish()?;
    }
    let result =
        build_oci_image_from_tar(output_dir, &tar_path, _reference, message, author, changes);
    let _ = std::fs::remove_file(tar_path);
    result
}

fn build_oci_image_from_tar(
    output_dir: &Path,
    rootfs_tar: &Path,
    _reference: &str,
    message: &Option<String>,
    author: &Option<String>,
    changes: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let blobs_dir = output_dir.join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs_dir)?;

    // 1. Create layer tarball (gzipped)
    let layer_path = blobs_dir.join("layer.tmp");
    {
        let file = std::fs::File::create(&layer_path)?;
        let mut encoder = GzEncoder::new(file, Compression::default());
        let mut input = std::fs::File::open(rootfs_tar)?;
        std::io::copy(&mut input, &mut encoder)?;
        encoder.finish()?;
    }

    // Hash the layer
    let layer_bytes = std::fs::read(&layer_path)?;
    let layer_digest = format!("{:x}", Sha256::digest(&layer_bytes));
    let layer_size = layer_bytes.len() as u64;
    let layer_blob = blobs_dir.join(&layer_digest);
    std::fs::rename(&layer_path, &layer_blob)?;

    // Compute diff_id (sha256 of uncompressed tar)
    let diff_id = compute_file_sha256(rootfs_tar)?;

    // 2. Create image config
    let mut config_obj = serde_json::json!({
        "architecture": std::env::consts::ARCH,
        "os": "linux",
        "config": {},
        "rootfs": {
            "type": "layers",
            "diff_ids": [format!("sha256:{diff_id}")]
        },
        "history": [{
            "created": chrono::Utc::now().to_rfc3339(),
            "created_by": "a3s-box commit",
            "comment": message.as_deref().unwrap_or(""),
            "author": author.as_deref().unwrap_or("")
        }]
    });

    // Apply --change directives to config
    apply_changes(&mut config_obj, changes);

    let config_bytes = serde_json::to_vec_pretty(&config_obj)?;
    let config_digest = format!("{:x}", Sha256::digest(&config_bytes));
    let config_size = config_bytes.len() as u64;
    std::fs::write(blobs_dir.join(&config_digest), &config_bytes)?;

    // 3. Create manifest
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": format!("sha256:{config_digest}"),
            "size": config_size
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": format!("sha256:{layer_digest}"),
            "size": layer_size
        }]
    });

    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    let manifest_digest = format!("{:x}", Sha256::digest(&manifest_bytes));
    let manifest_size = manifest_bytes.len() as u64;
    std::fs::write(blobs_dir.join(&manifest_digest), &manifest_bytes)?;

    // Also write as manifest.json for digest computation
    std::fs::write(output_dir.join("manifest.json"), &manifest_bytes)?;

    // 4. Create index.json
    let index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{manifest_digest}"),
            "size": manifest_size
        }]
    });
    std::fs::write(
        output_dir.join("index.json"),
        serde_json::to_vec_pretty(&index)?,
    )?;

    // 5. Create oci-layout
    std::fs::write(
        output_dir.join("oci-layout"),
        r#"{"imageLayoutVersion":"1.0.0"}"#,
    )?;

    Ok(())
}

fn compute_file_sha256(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Compute the diff_id (sha256 of uncompressed tar) for a directory.
#[cfg(test)]
fn compute_diff_id(rootfs_dir: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let mut hasher = Sha256::new();
    let buf = Vec::new();
    let mut builder = tar::Builder::new(buf);
    builder.follow_symlinks(false);
    builder.append_dir_all(".", rootfs_dir)?;
    builder.finish()?;
    let data = builder.into_inner()?;
    hasher.update(&data);
    Ok(format!("{:x}", hasher.finalize()))
}

/// Apply Dockerfile-style change directives to the image config.
fn apply_changes(config: &mut serde_json::Value, changes: &[String]) {
    for change in changes {
        let trimmed = change.trim();
        if let Some(rest) = trimmed.strip_prefix("CMD ") {
            // Honor exec form (CMD ["a","b"]) vs shell form, like Docker/import.
            config["config"]["Cmd"] = super::import::parse_exec_or_shell(rest);
        } else if let Some(rest) = trimmed.strip_prefix("ENTRYPOINT ") {
            config["config"]["Entrypoint"] = super::import::parse_exec_or_shell(rest);
        } else if let Some(rest) = trimmed.strip_prefix("ENV ") {
            // Accept both KEY=VALUE and the legacy space-separated KEY VALUE form.
            if let Ok((k, v)) = super::import::parse_key_value(rest) {
                let mut env = config["config"]["Env"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                env.push(serde_json::json!(format!("{k}={v}")));
                config["config"]["Env"] = serde_json::json!(env);
            }
        } else if let Some(rest) = trimmed.strip_prefix("EXPOSE ") {
            let ports = config["config"]["ExposedPorts"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            let mut ports = ports;
            ports.insert(format!("{rest}/tcp"), serde_json::json!({}));
            config["config"]["ExposedPorts"] = serde_json::json!(ports);
        } else if let Some(rest) = trimmed.strip_prefix("WORKDIR ") {
            config["config"]["WorkingDir"] = serde_json::json!(rest);
        } else if let Some(rest) = trimmed.strip_prefix("USER ") {
            config["config"]["User"] = serde_json::json!(rest);
        } else if let Some(rest) = trimmed.strip_prefix("LABEL ") {
            if let Ok((k, v)) = super::import::parse_key_value(rest) {
                let mut labels = config["config"]["Labels"]
                    .as_object()
                    .cloned()
                    .unwrap_or_default();
                labels.insert(k, serde_json::json!(v));
                config["config"]["Labels"] = serde_json::json!(labels);
            }
        }
    }
}

#[cfg(test)]
#[path = "commit/tests.rs"]
mod tests;
