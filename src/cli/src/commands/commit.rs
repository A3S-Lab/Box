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

pub async fn execute(args: CommitArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, &args.name)?;

    let attached_rootfs = if record.status == "running" {
        None
    } else {
        a3s_box_runtime::rootfs::attach_persistent_rootfs(&record.box_dir)?
    };
    let rootfs_dir = attached_rootfs
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
        })?;

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

    capture_rootfs_tar(record, &rootfs_dir, &rootfs_tar, args.pause).await?;

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

async fn capture_rootfs_tar(
    record: &crate::state::BoxRecord,
    rootfs_dir: &Path,
    output: &Path,
    pause: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if record.status == "running" && record.exec_socket_path.exists() {
        let client = a3s_box_runtime::ExecClient::connect(&record.exec_socket_path).await?;
        let mut file = tokio::fs::File::create(output).await?;
        let written = client.archive_rootfs(&mut file, pause).await?;
        if written == 0 {
            return Err("Guest rootfs archive was empty".into());
        }
        file.sync_all().await?;
        return Ok(());
    }

    let metadata_path = rootfs_dir
        .join(a3s_box_core::rootfs_metadata::ROOTFS_METADATA_PATH.trim_start_matches('/'));
    let bytes = std::fs::read(&metadata_path).map_err(|error| {
        format!(
            "Guest rootfs metadata is unavailable at {}: {error}. Start the box with this A3S Box version and stop it cleanly before committing.",
            metadata_path.display()
        )
    })?;
    let manifest: a3s_box_core::rootfs_metadata::RootfsMetadataManifest =
        serde_json::from_slice(&bytes)?;
    manifest
        .validate()
        .map_err(|error| format!("Invalid guest rootfs metadata: {error}"))?;
    create_tar_from_guest_metadata(rootfs_dir, &manifest, output)
}

#[cfg(unix)]
fn create_tar_from_guest_metadata(
    rootfs_dir: &Path,
    manifest: &a3s_box_core::rootfs_metadata::RootfsMetadataManifest,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    use a3s_box_core::rootfs_metadata::RootfsEntryKind;
    use std::collections::{HashMap, HashSet};
    use std::ffi::OsString;
    use std::io::Cursor;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::MetadataExt;

    let mut decoded = Vec::with_capacity(manifest.entries.len());
    let mut paths = HashSet::with_capacity(manifest.entries.len());
    for entry in &manifest.entries {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&entry.path_base64)
            .map_err(|error| format!("Invalid rootfs metadata path: {error}"))?;
        let path = std::path::PathBuf::from(OsString::from_vec(bytes));
        validate_archive_path(&path)?;
        if !paths.insert(path.clone()) {
            return Err(format!("Duplicate rootfs metadata path: {}", path.display()).into());
        }
        decoded.push((path, entry));
    }
    decoded.sort_by(|left, right| left.0.as_os_str().cmp(right.0.as_os_str()));

    let file = std::fs::File::create(output)?;
    let mut builder = tar::Builder::new(file);
    let mut hardlinks = HashMap::<(u64, u64), std::path::PathBuf>::new();
    for (path, entry) in decoded {
        let source = rootfs_dir.join(&path);
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
                if !host_metadata.file_type().is_dir() {
                    return Err(format!("Rootfs entry changed type: {}", path.display()).into());
                }
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                header.set_cksum();
                builder.append_data(&mut header, &path, Cursor::new([]))?;
            }
            RootfsEntryKind::Regular => {
                if !host_metadata.file_type().is_file() || host_metadata.len() != entry.size {
                    return Err(
                        format!("Rootfs entry changed after capture: {}", path.display()).into(),
                    );
                }
                let inode = (host_metadata.dev(), host_metadata.ino());
                if host_metadata.nlink() > 1 {
                    if let Some(first_path) = hardlinks.get(&inode) {
                        header.set_entry_type(tar::EntryType::Link);
                        header.set_size(0);
                        header.set_link_name(first_path)?;
                        header.set_cksum();
                        builder.append_data(&mut header, &path, Cursor::new([]))?;
                        continue;
                    }
                    hardlinks.insert(inode, path.clone());
                }
                header.set_entry_type(tar::EntryType::Regular);
                header.set_size(entry.size);
                header.set_cksum();
                let file = std::fs::File::open(&source)?;
                builder.append_data(&mut header, &path, file)?;
            }
            RootfsEntryKind::Symlink => {
                if !host_metadata.file_type().is_symlink() {
                    return Err(format!("Rootfs entry changed type: {}", path.display()).into());
                }
                let target = entry
                    .link_target_base64
                    .as_ref()
                    .ok_or_else(|| format!("Missing symlink target: {}", path.display()))?;
                let target = base64::engine::general_purpose::STANDARD.decode(target)?;
                let target = std::path::PathBuf::from(OsString::from_vec(target));
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_size(0);
                header.set_cksum();
                builder.append_link(&mut header, &path, target)?;
            }
        }
    }
    builder.finish()?;
    Ok(())
}

#[cfg(not(unix))]
fn create_tar_from_guest_metadata(
    _rootfs_dir: &Path,
    _manifest: &a3s_box_core::rootfs_metadata::RootfsMetadataManifest,
    _output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    Err("Stopped-box guest metadata commit is not supported on this host".into())
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
mod tests {
    use super::*;

    #[test]
    fn test_apply_changes_cmd() {
        let mut config = serde_json::json!({"config": {}});
        apply_changes(&mut config, &["CMD /bin/bash".to_string()]);
        assert_eq!(
            config["config"]["Cmd"],
            serde_json::json!(["/bin/sh", "-c", "/bin/bash"])
        );
    }

    #[test]
    fn test_apply_changes_entrypoint() {
        let mut config = serde_json::json!({"config": {}});
        apply_changes(&mut config, &["ENTRYPOINT /app/start".to_string()]);
        assert_eq!(
            config["config"]["Entrypoint"],
            serde_json::json!(["/bin/sh", "-c", "/app/start"])
        );
    }

    #[test]
    fn test_apply_changes_env() {
        let mut config = serde_json::json!({"config": {}});
        apply_changes(&mut config, &["ENV FOO=bar".to_string()]);
        let env = config["config"]["Env"].as_array().unwrap();
        assert_eq!(env.len(), 1);
        assert_eq!(env[0], "FOO=bar");
    }

    #[test]
    fn test_apply_changes_workdir() {
        let mut config = serde_json::json!({"config": {}});
        apply_changes(&mut config, &["WORKDIR /app".to_string()]);
        assert_eq!(config["config"]["WorkingDir"], "/app");
    }

    #[test]
    fn test_apply_changes_user() {
        let mut config = serde_json::json!({"config": {}});
        apply_changes(&mut config, &["USER nobody".to_string()]);
        assert_eq!(config["config"]["User"], "nobody");
    }

    #[test]
    fn test_apply_changes_label() {
        let mut config = serde_json::json!({"config": {}});
        apply_changes(&mut config, &["LABEL version=1.0".to_string()]);
        assert_eq!(config["config"]["Labels"]["version"], "1.0");
    }

    #[test]
    fn test_apply_changes_expose() {
        let mut config = serde_json::json!({"config": {}});
        apply_changes(&mut config, &["EXPOSE 8080".to_string()]);
        assert!(config["config"]["ExposedPorts"]["8080/tcp"].is_object());
    }

    #[test]
    fn test_apply_changes_multiple() {
        let mut config = serde_json::json!({"config": {}});
        apply_changes(
            &mut config,
            &[
                "CMD /start".to_string(),
                "ENV APP=test".to_string(),
                "WORKDIR /opt".to_string(),
            ],
        );
        assert!(config["config"]["Cmd"].is_array());
        assert!(config["config"]["Env"].is_array());
        assert_eq!(config["config"]["WorkingDir"], "/opt");
    }

    #[test]
    fn test_apply_changes_empty() {
        let mut config = serde_json::json!({"config": {}});
        apply_changes(&mut config, &[]);
        assert_eq!(config["config"], serde_json::json!({}));
    }

    #[test]
    fn test_compute_diff_id() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "world").unwrap();
        let id = compute_diff_id(dir.path()).unwrap();
        assert!(!id.is_empty());
        assert_eq!(id.len(), 64); // sha256 hex
    }

    #[cfg(unix)]
    #[test]
    fn test_guest_metadata_overrides_host_uid_gid_and_mode_in_tar() {
        use a3s_box_core::rootfs_metadata::{
            RootfsEntryKind, RootfsMetadataEntry, RootfsMetadataManifest,
        };
        use std::os::unix::ffi::OsStrExt;

        let rootfs = tempfile::TempDir::new().unwrap();
        let file = rootfs.path().join("probe");
        std::fs::write(&file, b"payload").unwrap();
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(Path::new("probe").as_os_str().as_bytes());
        let manifest = RootfsMetadataManifest::new(vec![RootfsMetadataEntry {
            path_base64: encoded,
            kind: RootfsEntryKind::Regular,
            mode: 0o100755,
            uid: 0,
            gid: 0,
            mtime: 123,
            size: 7,
            link_target_base64: None,
        }]);
        let output = rootfs.path().join("rootfs.tar");

        create_tar_from_guest_metadata(rootfs.path(), &manifest, &output).unwrap();

        let mut archive = tar::Archive::new(std::fs::File::open(output).unwrap());
        let entry = archive.entries().unwrap().next().unwrap().unwrap();
        assert_eq!(entry.path().unwrap(), Path::new("probe"));
        assert_eq!(entry.header().mode().unwrap() & 0o7777, 0o755);
        assert_eq!(entry.header().uid().unwrap(), 0);
        assert_eq!(entry.header().gid().unwrap(), 0);
        assert_eq!(entry.header().mtime().unwrap(), 123);
    }

    #[cfg(unix)]
    #[test]
    fn test_guest_metadata_preserves_hardlinks_without_duplicate_payloads() {
        use a3s_box_core::rootfs_metadata::{
            RootfsEntryKind, RootfsMetadataEntry, RootfsMetadataManifest,
        };
        use std::os::unix::ffi::OsStrExt;

        let rootfs = tempfile::TempDir::new().unwrap();
        std::fs::write(rootfs.path().join("busybox"), b"payload").unwrap();
        std::fs::hard_link(rootfs.path().join("busybox"), rootfs.path().join("sh")).unwrap();
        let entries = ["busybox", "sh"]
            .into_iter()
            .map(|path| RootfsMetadataEntry {
                path_base64: base64::engine::general_purpose::STANDARD
                    .encode(Path::new(path).as_os_str().as_bytes()),
                kind: RootfsEntryKind::Regular,
                mode: 0o100755,
                uid: 0,
                gid: 0,
                mtime: 123,
                size: 7,
                link_target_base64: None,
            })
            .collect();
        let output = rootfs.path().join("rootfs.tar");

        create_tar_from_guest_metadata(
            rootfs.path(),
            &RootfsMetadataManifest::new(entries),
            &output,
        )
        .unwrap();

        let mut archive = tar::Archive::new(std::fs::File::open(output).unwrap());
        let mut entries = archive.entries().unwrap();
        let first = entries.next().unwrap().unwrap();
        assert_eq!(first.header().entry_type(), tar::EntryType::Regular);
        drop(first);
        let second = entries.next().unwrap().unwrap();
        assert_eq!(second.header().entry_type(), tar::EntryType::Link);
        assert_eq!(second.link_name().unwrap().unwrap(), Path::new("busybox"));
    }

    #[cfg(unix)]
    #[test]
    fn test_guest_metadata_rejects_parent_traversal() {
        use a3s_box_core::rootfs_metadata::{
            RootfsEntryKind, RootfsMetadataEntry, RootfsMetadataManifest,
        };
        use std::os::unix::ffi::OsStrExt;

        let rootfs = tempfile::TempDir::new().unwrap();
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(Path::new("../escape").as_os_str().as_bytes());
        let manifest = RootfsMetadataManifest::new(vec![RootfsMetadataEntry {
            path_base64: encoded,
            kind: RootfsEntryKind::Regular,
            mode: 0o100600,
            uid: 0,
            gid: 0,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        }]);

        let error = create_tar_from_guest_metadata(
            rootfs.path(),
            &manifest,
            &rootfs.path().join("rootfs.tar"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("Unsafe rootfs metadata path"));
    }

    #[test]
    fn test_build_oci_image() {
        let rootfs = tempfile::tempdir().unwrap();
        std::fs::write(rootfs.path().join("test.txt"), "data").unwrap();

        let output = tempfile::tempdir().unwrap();
        build_oci_image(
            output.path(),
            rootfs.path(),
            "test:latest",
            &Some("test commit".to_string()),
            &Some("tester".to_string()),
            &[],
        )
        .unwrap();

        // Verify OCI layout
        assert!(output.path().join("oci-layout").exists());
        assert!(output.path().join("index.json").exists());
        assert!(output.path().join("blobs/sha256").exists());

        // Verify index.json is valid
        let index: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(output.path().join("index.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(index["schemaVersion"], 2);
        assert!(index["manifests"][0]["digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:"));
    }
}
