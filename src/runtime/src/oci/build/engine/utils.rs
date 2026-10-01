//! Utility functions for the build engine.

use std::collections::HashMap;
use std::path::Path;

use a3s_box_core::error::{BoxError, Result};

use super::super::dockerignore::DockerIgnore;
#[cfg(test)]
use super::super::layer::sha256_bytes;

/// Check if a filename looks like a tar archive.
pub(super) fn is_tar_archive(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.ends_with(".tar")
        || lower.ends_with(".tar.gz")
        || lower.ends_with(".tgz")
        || lower.ends_with(".tar.bz2")
        || lower.ends_with(".tbz2")
        || lower.ends_with(".tar.xz")
        || lower.ends_with(".txz")
}

/// Extract every archive entry inside `dst`.
///
/// On Windows, a directory junction already at an entry path is removed first.
/// `unpack_in` otherwise refuses the entry because the junction resolves
/// outside `dst`, and following it would write into that other directory.
fn unpack_archive<R: std::io::Read>(
    archive: &mut tar::Archive<R>,
    dst: &Path,
) -> std::io::Result<()> {
    for entry in archive.entries()? {
        let mut entry = entry?;
        #[cfg(windows)]
        {
            let path = entry.path()?;
            if !path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir | std::path::Component::Prefix(_)
                )
            }) {
                let mut cursor = dst.to_path_buf();
                for component in path.components() {
                    let std::path::Component::Normal(name) = component else {
                        continue;
                    };
                    cursor.push(name);
                    a3s_box_core::windows_file::remove_directory_junction(&cursor)?;
                }
            }
        }
        entry.unpack_in(dst)?;
    }
    Ok(())
}

/// Extract a tar archive to a destination directory.
pub(super) fn extract_tar_to_dst(archive_path: &Path, dst: &Path) -> Result<()> {
    use crate::oci::limited_reader::LimitedReader;
    use flate2::read::GzDecoder;
    use std::io::BufReader;

    #[cfg(windows)]
    {
        refuse_build_context_junction(archive_path)?;
        refuse_build_context_junction(dst)?;
    }

    std::fs::create_dir_all(dst).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to create extraction directory {}: {}",
            dst.display(),
            e
        ))
    })?;

    let file = std::fs::File::open(archive_path).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to open archive {}: {}",
            archive_path.display(),
            e
        ))
    })?;

    let name = archive_path.to_str().unwrap_or("").to_lowercase();

    // Bound decompressed output so a compression-bomb archive (`ADD app.tar.gz`)
    // cannot fill the build host's disk. Mirrors the MAX_ADD_URL_BYTES cap on the
    // ADD-URL path (which the local-archive path previously lacked); tune with
    // A3S_BOX_MAX_BUILD_EXTRACT_BYTES.
    let max_bytes = crate::oci::limited_reader::cap_from_env(
        "A3S_BOX_MAX_BUILD_EXTRACT_BYTES",
        4 * 1024 * 1024 * 1024,
    );

    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        let decoder = LimitedReader::new(GzDecoder::new(BufReader::new(file)), max_bytes);
        let mut archive = tar::Archive::new(decoder);
        unpack_archive(&mut archive, dst).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to extract tar.gz {}: {}",
                archive_path.display(),
                e
            ))
        })?;
    } else if name.ends_with(".tar.bz2") || name.ends_with(".tbz2") {
        #[cfg(not(unix))]
        return Err(BoxError::BuildError(format!(
            "Unsupported archive format on Windows: {}",
            archive_path.display()
        )));

        #[cfg(unix)]
        {
            use bzip2::read::BzDecoder;

            let decoder = LimitedReader::new(BzDecoder::new(BufReader::new(file)), max_bytes);
            let mut archive = tar::Archive::new(decoder);
            unpack_archive(&mut archive, dst).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to extract tar.bz2 {}: {}",
                    archive_path.display(),
                    e
                ))
            })?;
        }
    } else if name.ends_with(".tar.xz") || name.ends_with(".txz") {
        #[cfg(not(unix))]
        return Err(BoxError::BuildError(format!(
            "Unsupported archive format on Windows: {}",
            archive_path.display()
        )));

        #[cfg(unix)]
        {
            use xz2::read::XzDecoder;

            let decoder = LimitedReader::new(XzDecoder::new(BufReader::new(file)), max_bytes);
            let mut archive = tar::Archive::new(decoder);
            unpack_archive(&mut archive, dst).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to extract tar.xz {}: {}",
                    archive_path.display(),
                    e
                ))
            })?;
        }
    } else if name.ends_with(".tar") {
        let mut archive = tar::Archive::new(LimitedReader::new(BufReader::new(file), max_bytes));
        unpack_archive(&mut archive, dst).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to extract tar {}: {}",
                archive_path.display(),
                e
            ))
        })?;
    } else {
        return Err(BoxError::BuildError(format!(
            "Unsupported archive format: {}",
            archive_path.display()
        )));
    }

    Ok(())
}

/// Resolve a path relative to a working directory.
///
/// If `path` is absolute, return it as-is. Otherwise, join with `workdir`.
pub(super) fn resolve_path(workdir: &str, path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{}/{}", workdir.trim_end_matches('/'), path)
    }
}

/// Reject a COPY/ADD path containing a `..` component. `Path::join` does not
/// normalize, so a preserved `..` is resolved by the OS at access time and
/// escapes the build context (source) or rootfs (destination) — Docker forbids
/// this ("forbidden path outside the build context").
pub(super) fn reject_path_traversal(path: &str) -> Result<()> {
    if Path::new(path)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(BoxError::BuildError(format!(
            "COPY/ADD path '{path}' contains '..' (forbidden: it would escape the \
             build context / rootfs)"
        )));
    }
    Ok(())
}

/// Assert that `candidate` resolves *inside* `base`, following symlinks. Guards
/// against a base-image (or prior-COPY) symlink whose target leaves the rootfs:
/// canonicalize the deepest existing ancestor of `candidate` (it may not exist
/// yet for a write) and verify it stays within the canonicalized `base`.
pub(super) fn assert_within(base: &Path, candidate: &Path) -> Result<()> {
    let base_real = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
    let mut probe = candidate;
    let existing = loop {
        if probe.exists() {
            break probe.canonicalize().unwrap_or_else(|_| probe.to_path_buf());
        }
        match probe.parent() {
            Some(parent) => probe = parent,
            None => break base_real.clone(),
        }
    };
    if !existing.starts_with(&base_real) {
        return Err(BoxError::BuildError(format!(
            "COPY/ADD path '{}' escapes '{}' (forbidden)",
            candidate.display(),
            base.display()
        )));
    }
    Ok(())
}

/// Expand `${VAR}` and `$VAR` references in a string using build args.
pub(super) fn expand_args(s: &str, args: &HashMap<String, String>) -> String {
    // Scan for $NAME / ${NAME} and expand each to the WHOLE identifier's value.
    // A naive per-key string replace collides on prefixes (replacing `$VAR` also
    // rewrites the `$VAR` inside `$VARLONG` / `$VARX`), and the result depends on
    // HashMap iteration order. Here `$VAR` matches the longest [A-Za-z0-9_] run,
    // so it never partially rewrites a longer name. An undefined name is left
    // literal (the prior behavior — only defined keys were substituted).
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'$' && i + 1 < b.len() {
            if b[i + 1] == b'{' {
                if let Some(rel) = s[i + 2..].find('}') {
                    let name = &s[i + 2..i + 2 + rel];
                    match args.get(name) {
                        Some(v) => out.extend_from_slice(v.as_bytes()),
                        None => out.extend_from_slice(&b[i..i + 2 + rel + 1]),
                    }
                    i += 2 + rel + 1;
                    continue;
                }
            } else {
                let start = i + 1;
                let mut j = start;
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                    j += 1;
                }
                if j > start {
                    let name = &s[start..j];
                    match args.get(name) {
                        Some(v) => out.extend_from_slice(v.as_bytes()),
                        None => out.extend_from_slice(&b[i..j]),
                    }
                    i = j;
                    continue;
                }
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Compute the diff_id (SHA256 of uncompressed layer content).
pub(super) fn compute_diff_id(layer_path: &Path) -> Result<String> {
    use flate2::read::GzDecoder;
    use sha2::{Digest, Sha256};
    use std::io::{BufRead, BufReader, Read};

    let file = std::fs::File::open(layer_path)
        .map_err(|e| BoxError::BuildError(format!("Failed to read layer for diff_id: {}", e)))?;
    let mut buffered = BufReader::new(file);
    let prefix = buffered.fill_buf().map_err(|error| {
        BoxError::BuildError(format!("Failed to inspect layer for diff_id: {error}"))
    })?;
    let is_gzip = prefix.starts_with(&[0x1f, 0x8b]);
    let is_zstd = prefix.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]);

    fn hash_reader(mut reader: impl Read, format: &str) -> Result<String> {
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = reader.read(&mut buffer).map_err(|error| {
                BoxError::BuildError(format!(
                    "Failed to read {format} layer for diff_id: {error}"
                ))
            })?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(hex::encode(hasher.finalize()))
    }

    if is_gzip {
        hash_reader(GzDecoder::new(buffered), "gzip-compressed")
    } else if is_zstd {
        let decoder = zstd::stream::read::Decoder::new(buffered).map_err(|error| {
            BoxError::BuildError(format!(
                "Failed to initialize zstd layer decoder for diff_id: {error}"
            ))
        })?;
        hash_reader(decoder, "zstd-compressed")
    } else {
        hash_reader(buffered, "uncompressed")
    }
}

/// Recreate `dst` as the same Windows mount-point junction as `src`.
///
/// `read_dir` follows a junction, so a source that is itself a junction must be
/// recreated before the recursive copy. A child junction is handled later.
#[cfg(windows)]
pub(super) fn recreate_copied_source_junction(src: &Path, dst: &Path) -> Result<bool> {
    refuse_copy_through_ancestor_junction(src)?;
    refuse_copy_through_ancestor_junction(dst)?;
    if let Some(parent) = dst.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|error| {
            BoxError::BuildError(format!(
                "Failed to create directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    a3s_box_core::windows_file::recreate_directory_junction(src, dst).map_err(|error| {
        BoxError::BuildError(format!(
            "Failed to recreate directory junction {}: {error}",
            dst.display()
        ))
    })
}

/// Refuse a copy reached through a directory junction above the leaf.
/// The leaf may itself be a junction; that case is recreated by the caller.
#[cfg(windows)]
pub(super) fn refuse_copy_through_ancestor_junction(path: &Path) -> Result<()> {
    let mut prefix = std::path::PathBuf::new();
    let components: Vec<_> = path.components().collect();
    for (index, component) in components.iter().enumerate() {
        prefix.push(component);
        if index + 1 == components.len() {
            break;
        }
        let metadata = match std::fs::symlink_metadata(&prefix) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(BoxError::BuildError(format!(
                    "Failed to inspect copy path {}: {error}",
                    path.display()
                )))
            }
        };
        if metadata.file_type().is_symlink() || windows_reparse_point(&metadata) {
            return Err(BoxError::BuildError(format!(
                "refusing to copy through a directory junction: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Refuse a build context that is a directory junction or sits under one.
#[cfg(windows)]
pub(super) fn refuse_build_context_junction(context_dir: &Path) -> Result<()> {
    let mut prefix = std::path::PathBuf::new();
    for component in context_dir.components() {
        prefix.push(component);
        let metadata = match std::fs::symlink_metadata(&prefix) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(BoxError::IoError(error)),
        };
        if metadata.file_type().is_symlink() || windows_reparse_point(&metadata) {
            return Err(BoxError::BuildError(format!(
                "refusing to build through a directory junction: {}",
                context_dir.display()
            )));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn windows_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x0000_0400 != 0
}

/// Remove a destination junction before copying so `create_dir_all` does not
/// follow it into the target directory.
#[cfg(windows)]
pub(super) fn remove_copied_destination_junction(dst: &Path) -> Result<()> {
    a3s_box_core::windows_file::remove_directory_junction(dst).map_err(|error| {
        BoxError::BuildError(format!(
            "Failed to remove destination directory junction {}: {error}",
            dst.display()
        ))
    })?;
    Ok(())
}

/// Recursively copy a directory.
/// Recursively copy `src` to `dst`, tracking each entry's path relative to the
/// build context (`rel_base`) and skipping entries excluded by `.dockerignore`.
/// An excluded directory is pruned (not descended into). When `ignore` is
/// `None` (e.g. `COPY --from`), nothing is filtered.
pub(super) fn copy_dir_filtered(
    src: &Path,
    dst: &Path,
    rel_base: &Path,
    ignore: Option<&DockerIgnore>,
) -> Result<()> {
    #[cfg(windows)]
    remove_copied_destination_junction(dst)?;
    #[cfg(windows)]
    if recreate_copied_source_junction(src, dst)? {
        return Ok(());
    }
    #[cfg(windows)]
    refuse_copy_through_ancestor_junction(src)?;
    #[cfg(windows)]
    refuse_copy_through_ancestor_junction(dst)?;
    std::fs::create_dir_all(dst).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to create directory {}: {}",
            dst.display(),
            e
        ))
    })?;

    for entry in std::fs::read_dir(src).map_err(|e| {
        BoxError::BuildError(format!("Failed to read directory {}: {}", src.display(), e))
    })? {
        let entry =
            entry.map_err(|e| BoxError::BuildError(format!("Failed to read entry: {}", e)))?;
        let src_path = entry.path();
        let entry_rel = rel_base.join(entry.file_name());

        // Skip paths excluded by .dockerignore (prunes the whole subtree).
        if let Some(ign) = ignore {
            if ign.is_excluded(&entry_rel) {
                continue;
            }
        }

        let dst_path = dst.join(entry.file_name());

        // Use the no-follow file type so a symlink is preserved as a symlink
        // (Docker copies symlinks verbatim; following them would duplicate the
        // target content and lose e.g. shared-library `.so -> .so.1` links).
        let file_type = entry
            .file_type()
            .map_err(|e| BoxError::BuildError(format!("Failed to stat entry: {}", e)))?;

        if file_type.is_symlink() {
            let target = std::fs::read_link(&src_path).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to read symlink {}: {}",
                    src_path.display(),
                    e
                ))
            })?;
            let _ = std::fs::remove_file(&dst_path);
            #[cfg(windows)]
            if a3s_box_core::windows_file::recreate_directory_junction(&src_path, &dst_path)
                .map_err(|error| {
                    BoxError::BuildError(format!(
                        "Failed to recreate directory junction {} -> {}: {error}",
                        dst_path.display(),
                        target.display()
                    ))
                })?
            {
                continue;
            }
            symlink_to(&target, &dst_path)?;
        } else if file_type.is_dir() {
            copy_dir_filtered(&src_path, &dst_path, &entry_rel, ignore)?;
        } else {
            #[cfg(windows)]
            remove_copied_destination_junction(&dst_path)?;
            std::fs::copy(&src_path, &dst_path).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to copy {} to {}: {}",
                    src_path.display(),
                    dst_path.display(),
                    e
                ))
            })?;
        }
    }
    Ok(())
}

/// Create a symlink at `link` pointing at `target` (best-effort cross-platform).
fn symlink_to(target: &Path, link: &Path) -> Result<()> {
    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(target, link);
    #[cfg(not(unix))]
    let result = std::fs::write(link, Vec::new()); // non-unix fallback: placeholder file
    result.map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to create symlink {} -> {}: {}",
            link.display(),
            target.display(),
            e
        ))
    })
}

/// Format a byte size as a human-readable string.
/// Parse a `--chown` value (`user[:group]`, numeric or named) into a
/// `(uid, gid)` pair. Named users/groups are resolved from the base image's
/// `/etc/passwd` and `/etc/group` inside `rootfs_dir`.
pub(super) fn resolve_chown(spec: &str, rootfs_dir: &Path) -> Result<(u32, u32)> {
    let (user_part, group_part) = match spec.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (spec, None),
    };

    let uid = resolve_user(user_part, rootfs_dir)?;
    let gid = match group_part {
        Some(g) => resolve_group(g, rootfs_dir)?,
        None => uid_to_gid(uid, rootfs_dir)?.unwrap_or(uid),
    };
    Ok((uid, gid))
}

fn resolve_user(user: &str, rootfs: &Path) -> Result<u32> {
    if let Ok(n) = user.parse::<u32>() {
        return Ok(n);
    }
    // Look up in rootfs /etc/passwd: root:x:0:0:...
    let passwd =
        crate::oci::rootfs::read_guest_file_to_string(rootfs, "etc/passwd")?.unwrap_or_default();
    for line in passwd.lines() {
        let f: Vec<&str> = line.splitn(4, ':').collect();
        if f.len() >= 3 && f[0] == user {
            return f[2].parse::<u32>().map_err(|_| {
                BoxError::BuildError(format!("Invalid UID for user '{}' in /etc/passwd", user))
            });
        }
    }
    Err(BoxError::BuildError(format!(
        "COPY --chown: user '{}' not found in rootfs /etc/passwd",
        user
    )))
}

fn resolve_group(group: &str, rootfs: &Path) -> Result<u32> {
    if let Ok(n) = group.parse::<u32>() {
        return Ok(n);
    }
    let etc_group =
        crate::oci::rootfs::read_guest_file_to_string(rootfs, "etc/group")?.unwrap_or_default();
    for line in etc_group.lines() {
        let f: Vec<&str> = line.splitn(4, ':').collect();
        if f.len() >= 3 && f[0] == group {
            return f[2].parse::<u32>().map_err(|_| {
                BoxError::BuildError(format!("Invalid GID for group '{}' in /etc/group", group))
            });
        }
    }
    Err(BoxError::BuildError(format!(
        "COPY --chown: group '{}' not found in rootfs /etc/group",
        group
    )))
}

/// Get the primary GID for a UID from /etc/passwd (field 4).
fn uid_to_gid(uid: u32, rootfs: &Path) -> Result<Option<u32>> {
    let Some(passwd) = crate::oci::rootfs::read_guest_file_to_string(rootfs, "etc/passwd")? else {
        return Ok(None);
    };
    for line in passwd.lines() {
        let f: Vec<&str> = line.splitn(5, ':').collect();
        if f.len() >= 4 && f[2].parse::<u32>().ok() == Some(uid) {
            return Ok(f[3].parse::<u32>().ok());
        }
    }
    Ok(None)
}

/// Placeholder — no longer used; chown is applied in tar headers, not the
/// host filesystem.
#[allow(dead_code)]
pub(super) fn apply_chown_recursive(_dir: &Path, _uid: u32, _gid: u32) -> Result<()> {
    Ok(())
}

pub(super) fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn refuse_build_context_junction_rejects_the_context_directory() {
        use std::os::windows::process::CommandExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
        let parent = tmp.path().join("parent");
        std::fs::create_dir_all(&parent).unwrap();
        let link = parent.join("link");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());

        let error = refuse_build_context_junction(&link)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("junction"),
            "build context directory was admitted through a junction: {error}"
        );
        assert_eq!(
            std::fs::read(outside.join("secret.txt")).unwrap(),
            b"secret"
        );
    }

    #[cfg(windows)]
    #[test]
    fn refuse_build_context_junction_rejects_a_child_directory() {
        use std::os::windows::process::CommandExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let nested = tmp.path().join("outside").join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("secret.txt"), b"secret").unwrap();
        let parent = tmp.path().join("parent");
        std::fs::create_dir_all(&parent).unwrap();
        let link = parent.join("link");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            tmp.path().join("outside").display()
        ));
        assert!(command.status().expect("mklink").success());

        let error = refuse_build_context_junction(&link.join("nested"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("junction"),
            "build context child was admitted through a junction: {error}"
        );
        assert_eq!(std::fs::read(nested.join("secret.txt")).unwrap(), b"secret");
    }

    #[test]
    fn compute_diff_id_accepts_an_uncompressed_oci_layer() {
        let temporary = tempfile::tempdir().unwrap();
        let layer = temporary.path().join("layer.tar");
        let contents = b"uncompressed OCI layer bytes";
        std::fs::write(&layer, contents).unwrap();

        assert_eq!(compute_diff_id(&layer).unwrap(), sha256_bytes(contents));
    }

    #[test]
    fn compute_diff_id_accepts_a_gzip_oci_layer() {
        use std::io::Write;

        let temporary = tempfile::tempdir().unwrap();
        let layer = temporary.path().join("layer.tar.gz");
        let contents = b"gzip OCI layer bytes";
        let file = std::fs::File::create(&layer).unwrap();
        let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        encoder.write_all(contents).unwrap();
        encoder.finish().unwrap();

        assert_eq!(compute_diff_id(&layer).unwrap(), sha256_bytes(contents));
    }

    #[test]
    fn compute_diff_id_accepts_a_zstd_oci_layer() {
        let temporary = tempfile::tempdir().unwrap();
        let layer = temporary.path().join("layer.tar.zst");
        let contents = b"zstd OCI layer bytes";
        let encoded = zstd::stream::encode_all(&contents[..], 1).unwrap();
        std::fs::write(&layer, encoded).unwrap();

        assert_eq!(compute_diff_id(&layer).unwrap(), sha256_bytes(contents));
    }

    #[test]
    fn test_expand_args_no_prefix_collision() {
        let mut args = HashMap::new();
        args.insert("VAR".to_string(), "x".to_string());
        args.insert("VARLONG".to_string(), "y".to_string());
        // $VARLONG must expand to the longer key, not `$VAR` + "LONG".
        assert_eq!(expand_args("$VARLONG", &args), "y");
        assert_eq!(expand_args("${VARLONG}", &args), "y");
        assert_eq!(expand_args("$VAR", &args), "x");
        assert_eq!(expand_args("$VAR/$VARLONG", &args), "x/y");
        assert_eq!(expand_args("a-${VAR}-b", &args), "a-x-b");
        // $VARX (VARX undefined) must NOT become "x" + "X" — left literal.
        assert_eq!(expand_args("$VARX", &args), "$VARX");
        // Undefined names are left literal (prior behavior).
        assert_eq!(expand_args("$UNSET", &args), "$UNSET");
        assert_eq!(expand_args("${UNSET}", &args), "${UNSET}");
    }

    #[test]
    fn test_reject_path_traversal() {
        assert!(reject_path_traversal("../etc/passwd").is_err());
        assert!(reject_path_traversal("a/../../b").is_err());
        assert!(reject_path_traversal("..").is_err());
        assert!(reject_path_traversal("/abs/ok").is_ok());
        assert!(reject_path_traversal("rel/ok/path").is_ok());
        assert!(reject_path_traversal(".").is_ok());
    }

    #[test]
    fn test_assert_within_contains_and_rejects() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::write(base.join("sub/f"), "x").unwrap();

        // Inside (existing) and a not-yet-existing path whose parent is inside.
        assert!(assert_within(base, &base.join("sub/f")).is_ok());
        assert!(assert_within(base, &base.join("new/not/yet")).is_ok());
        // An absolute path outside the base is rejected.
        assert!(assert_within(base, std::path::Path::new("/etc/passwd")).is_err());

        // A symlink whose target escapes the base is rejected (symlink-follow).
        #[cfg(unix)]
        {
            let link = base.join("escape");
            std::os::unix::fs::symlink("/etc", &link).unwrap();
            assert!(assert_within(base, &link.join("passwd")).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_chown_rejects_rootfs_passwd_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("rootfs");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(rootfs.join("etc")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("passwd"), "hostuser:x:4242:4242::/:/bin/sh\n").unwrap();
        std::os::unix::fs::symlink("../../outside/passwd", rootfs.join("etc/passwd")).unwrap();

        let error = resolve_chown("hostuser", &rootfs).unwrap_err().to_string();

        assert!(error.contains("escapes rootfs"), "{error}");
    }

    // --- is_tar_archive tests ---

    #[test]
    fn test_is_tar_archive_tar() {
        assert!(is_tar_archive("file.tar"));
    }

    #[test]
    fn test_is_tar_archive_tar_gz() {
        assert!(is_tar_archive("file.tar.gz"));
        assert!(is_tar_archive("file.tgz"));
    }

    #[test]
    fn test_is_tar_archive_tar_bz2() {
        assert!(is_tar_archive("file.tar.bz2"));
        assert!(is_tar_archive("file.tbz2"));
    }

    #[test]
    fn test_is_tar_archive_tar_xz() {
        assert!(is_tar_archive("file.tar.xz"));
        assert!(is_tar_archive("file.txz"));
    }

    #[test]
    fn test_is_tar_archive_case_insensitive() {
        assert!(is_tar_archive("FILE.TAR.GZ"));
        assert!(is_tar_archive("Data.Tar.Bz2"));
        assert!(is_tar_archive("ARCHIVE.TAR.XZ"));
    }

    #[test]
    fn test_is_tar_archive_non_archive() {
        assert!(!is_tar_archive("file.txt"));
        assert!(!is_tar_archive("file.zip"));
        assert!(!is_tar_archive("file.gz"));
        assert!(!is_tar_archive("file.bz2"));
        assert!(!is_tar_archive("file.xz"));
    }

    // --- extract_tar_to_dst tests ---

    /// Helper: create a tar archive with a single file.
    fn create_test_tar(
        dir: &std::path::Path,
        filename: &str,
        content: &[u8],
    ) -> std::path::PathBuf {
        let tar_path = dir.join(filename);
        let file = std::fs::File::create(&tar_path).unwrap();
        let mut builder = tar::Builder::new(file);

        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "test.txt", content)
            .unwrap();
        builder.finish().unwrap();

        tar_path
    }

    #[test]
    fn test_extract_plain_tar() {
        let tmp = tempfile::tempdir().unwrap();
        let tar_path = create_test_tar(tmp.path(), "test.tar", b"hello tar");

        let dst = tmp.path().join("out");
        extract_tar_to_dst(&tar_path, &dst).unwrap();
        assert!(dst.join("test.txt").exists());
    }

    #[cfg(windows)]
    #[test]
    fn extract_tar_to_dst_does_not_write_through_a_child_junction() {
        use std::os::windows::process::CommandExt;

        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
        let dst = tmp.path().join("out");
        std::fs::create_dir_all(&dst).unwrap();
        let child = dst.join("child");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            child.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());

        let tar_path = tmp.path().join("layer.tar");
        let file = std::fs::File::create(&tar_path).unwrap();
        let mut builder = tar::Builder::new(file);
        let content = b"planted";
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "child/planted.txt", &content[..])
            .unwrap();
        builder.finish().unwrap();

        extract_tar_to_dst(&tar_path, &dst).unwrap();

        assert!(
            !outside.join("planted.txt").exists(),
            "archive extract wrote through the child junction"
        );
        assert_eq!(
            std::fs::read(outside.join("secret.txt")).unwrap(),
            b"secret"
        );
        assert_eq!(
            std::fs::read(dst.join("child/planted.txt")).unwrap(),
            b"planted"
        );
    }

    #[cfg(windows)]
    #[test]
    fn extract_tar_to_dst_does_not_open_through_an_ancestor_junction() {
        use std::os::windows::process::CommandExt;

        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let tar_path = create_test_tar(&outside, "app.tar", b"secret");
        let parent = tmp.path().join("parent");
        std::fs::create_dir_all(&parent).unwrap();
        let link = parent.join("link");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());
        let dst = tmp.path().join("out");

        let error = extract_tar_to_dst(&link.join("app.tar"), &dst)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("junction"),
            "archive was opened through an ancestor junction: {error}"
        );
        assert!(!dst.join("test.txt").exists());
        assert_eq!(std::fs::read(&tar_path).unwrap().is_empty(), false);
    }

    #[cfg(windows)]
    #[test]
    fn extract_tar_to_dst_does_not_create_through_an_ancestor_junction() {
        use std::os::windows::process::CommandExt;

        let tmp = tempfile::tempdir().unwrap();
        let tar_path = create_test_tar(tmp.path(), "app.tar", b"secret");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let parent = tmp.path().join("parent");
        std::fs::create_dir_all(&parent).unwrap();
        let link = parent.join("link");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());

        let error = extract_tar_to_dst(&tar_path, &link.join("out"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("junction"),
            "extract created a destination through an ancestor junction: {error}"
        );
        assert!(!outside.join("out").exists());
    }

    #[test]
    fn test_extract_tar_gz() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;

        let tmp = tempfile::tempdir().unwrap();

        // Create a tar in memory
        let mut tar_data = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_data);
            let mut header = tar::Header::new_gnu();
            let content = b"hello gzip";
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "test.txt", &content[..])
                .unwrap();
            builder.finish().unwrap();
        }

        // Gzip compress
        let gz_path = tmp.path().join("test.tar.gz");
        let gz_file = std::fs::File::create(&gz_path).unwrap();
        let mut encoder = GzEncoder::new(gz_file, Compression::default());
        encoder.write_all(&tar_data).unwrap();
        encoder.finish().unwrap();

        let dst = tmp.path().join("out");
        extract_tar_to_dst(&gz_path, &dst).unwrap();
        assert!(dst.join("test.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_extract_tar_bz2() {
        use bzip2::write::BzEncoder;
        use bzip2::Compression;
        use std::io::Write;

        let tmp = tempfile::tempdir().unwrap();

        // Create a tar in memory
        let mut tar_data = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_data);
            let mut header = tar::Header::new_gnu();
            let content = b"hello bzip2";
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "test.txt", &content[..])
                .unwrap();
            builder.finish().unwrap();
        }

        // Bzip2 compress
        let bz2_path = tmp.path().join("test.tar.bz2");
        let bz2_file = std::fs::File::create(&bz2_path).unwrap();
        let mut encoder = BzEncoder::new(bz2_file, Compression::default());
        encoder.write_all(&tar_data).unwrap();
        encoder.finish().unwrap();

        let dst = tmp.path().join("out");
        extract_tar_to_dst(&bz2_path, &dst).unwrap();
        assert!(dst.join("test.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_extract_tar_xz() {
        use std::io::Write;
        use xz2::write::XzEncoder;

        let tmp = tempfile::tempdir().unwrap();

        // Create a tar in memory
        let mut tar_data = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_data);
            let mut header = tar::Header::new_gnu();
            let content = b"hello xz";
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "test.txt", &content[..])
                .unwrap();
            builder.finish().unwrap();
        }

        // XZ compress
        let xz_path = tmp.path().join("test.tar.xz");
        let xz_file = std::fs::File::create(&xz_path).unwrap();
        let mut encoder = XzEncoder::new(xz_file, 6);
        encoder.write_all(&tar_data).unwrap();
        encoder.finish().unwrap();

        let dst = tmp.path().join("out");
        extract_tar_to_dst(&xz_path, &dst).unwrap();
        assert!(dst.join("test.txt").exists());
    }

    #[test]
    fn test_extract_nonexistent_file_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let result = extract_tar_to_dst(
            &tmp.path().join("nonexistent.tar.gz"),
            &tmp.path().join("out"),
        );
        assert!(result.is_err());
    }

    // --- resolve_path tests ---

    #[test]
    fn test_resolve_path_absolute() {
        assert_eq!(resolve_path("/work", "/etc/config"), "/etc/config");
    }

    #[test]
    fn test_resolve_path_relative() {
        assert_eq!(resolve_path("/work", "src/main.rs"), "/work/src/main.rs");
    }

    // --- format_size tests ---

    #[test]
    fn test_format_size_bytes() {
        assert_eq!(format_size(42), "42 B");
    }

    #[test]
    fn test_format_size_kb() {
        assert_eq!(format_size(2048), "2.0 KB");
    }

    #[test]
    fn test_format_size_mb() {
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn test_format_size_gb() {
        assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2.0 GB");
    }
}
