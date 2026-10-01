//! Guest-visible POSIX metadata for Windows virtio-fs shares.
//!
//! Windows virtio-fs keeps uid, gid, and mode in the VM inode table. A clean
//! shutdown records that view on the Box-owned rootfs. The host copies each
//! managed VolumeStore share into a sibling sidecar so the next box can replay
//! it with chmod and lchown. The sidecar is not NTFS ownership. Caller binds
//! and `/workspace` stay on the box rootfs. A crash before clean shutdown
//! leaves the previous sidecar in place.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::rootfs_metadata::{is_runtime_internal_rootfs_path, RootfsMetadataEntry};

/// Schema for one clean-shutdown capture of workspace and volume ownership.
pub const VOLUME_POSIX_METADATA_SCHEMA: &str = "a3s.box.volume-posix.v1";

/// Rootfs file written by guest-init. Leading dot, no directory component.
pub const VOLUME_POSIX_METADATA_FILE: &str = ".a3s_volume_metadata_v1.json";

/// Temporary sibling used while publishing the manifest atomically.
pub const VOLUME_POSIX_METADATA_TEMP_FILE: &str = ".a3s_volume_metadata_v1.json.tmp";

/// Entries whose owner or mode could not be applied. Harvest does not read
/// this file, so a stop before the next capture cannot shrink a sidecar to
/// only the failed entries.
pub const VOLUME_POSIX_METADATA_PENDING_FILE: &str = ".a3s_volume_metadata_v1.pending.json";

/// Scratch file for the pending replay. It is not the durable temp capture.
pub const VOLUME_POSIX_METADATA_PENDING_TEMP_FILE: &str =
    ".a3s_volume_metadata_v1.pending.json.tmp";

/// Scratch for the next manifest when the durable temp is already a capture.
/// A synced copy whose rename did not finish is itself a capture. The pending
/// replay scratch is not.
pub const VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE: &str =
    ".a3s_volume_metadata_v1.publish.json.tmp";

/// Guest environment that enables capture and replay. Set for persistent
/// Windows MicroVMs and for Windows MicroVMs that mount a managed volume.
/// Linux hosts leave it unset so bind mounts keep their existing ownership path.
pub const VOLUME_POSIX_METADATA_ENV: &str = "BOX_VOLUME_POSIX_METADATA";

/// Encoded manifest size limit. Matches the terminal rootfs metadata cap.
pub const VOLUME_POSIX_METADATA_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Total file and directory entries across every recorded share.
pub const VOLUME_POSIX_METADATA_MAX_ENTRIES: usize = 1_000_000;

/// Schema for one VolumeStore directory's POSIX entries.
pub const VOLUME_POSIX_SIDECAR_SCHEMA: &str = "a3s.box.volume-posix-sidecar.v1";

/// Suffix appended to the volume directory name. The file is a sibling of the
/// directory, never a child, because the directory is the virtio-fs mount.
pub const VOLUME_POSIX_SIDECAR_SUFFIX: &str = ".a3s-volume-posix.v1.json";

/// Schema for the per-box guest-path to volume-directory map.
pub const VOLUME_POSIX_BINDINGS_SCHEMA: &str = "a3s.box.volume-posix-bindings.v1";

/// Bindings file stored in the box directory, outside the guest rootfs.
pub const VOLUME_POSIX_BINDINGS_FILE: &str = "volume-posix-bindings.v1.json";

/// One virtio-fs share and the guest-visible ownership captured under it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumePosixMount {
    /// Absolute guest path, such as `/workspace` or a `BOX_VOL_*` target.
    pub guest_path: String,
    /// Paths relative to `guest_path`. `.` is the share root.
    pub entries: Vec<RootfsMetadataEntry>,
    /// The share could not be listed, so these entries are the previous
    /// capture. Harvest must not write them over a VolumeStore sidecar.
    #[serde(default)]
    pub retained: bool,
}

impl VolumePosixMount {
    pub fn retained(guest_path: String, entries: Vec<RootfsMetadataEntry>) -> Self {
        Self {
            guest_path,
            entries,
            retained: true,
        }
    }
}

/// Complete volume ownership snapshot for one persistent rootfs generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumePosixManifest {
    pub schema: String,
    pub mounts: Vec<VolumePosixMount>,
}

impl VolumePosixManifest {
    pub fn new(mounts: Vec<VolumePosixMount>) -> Self {
        Self {
            schema: VOLUME_POSIX_METADATA_SCHEMA.to_string(),
            mounts,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != VOLUME_POSIX_METADATA_SCHEMA {
            return Err(format!(
                "unsupported volume posix metadata schema: {}",
                self.schema
            ));
        }
        let mut total = 0usize;
        let mut seen_mounts = std::collections::BTreeSet::new();
        for mount in &self.mounts {
            validate_guest_mount(&mount.guest_path)?;
            if !seen_mounts.insert(mount.guest_path.as_str()) {
                return Err(format!("duplicate volume posix mount {}", mount.guest_path));
            }
            validate_entries(&mount.entries, &mut total, &mount.guest_path)?;
        }
        Ok(())
    }
}

/// POSIX entries for one VolumeStore directory, without a guest mount path.
///
/// The guest path is chosen by the next box. Keeping it out of the sidecar
/// stops a recreated mount at `/var/lib` from replaying metadata as `/data`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumePosixSidecar {
    pub schema: String,
    pub entries: Vec<RootfsMetadataEntry>,
}

impl VolumePosixSidecar {
    pub fn new(entries: Vec<RootfsMetadataEntry>) -> Self {
        Self {
            schema: VOLUME_POSIX_SIDECAR_SCHEMA.to_string(),
            entries,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != VOLUME_POSIX_SIDECAR_SCHEMA {
            return Err(format!(
                "unsupported volume posix sidecar schema: {}",
                self.schema
            ));
        }
        let mut total = 0usize;
        validate_entries(&self.entries, &mut total, "volume store")
    }
}

/// Guest mount path paired with the VolumeStore directory it used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumePosixBinding {
    pub guest_path: String,
    pub host_path: PathBuf,
}

/// Bindings written beside a box so stop can find VolumeStore directories
/// after the guest manifest no longer says which share they came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumePosixBindings {
    pub schema: String,
    pub mounts: Vec<VolumePosixBinding>,
}

impl VolumePosixBindings {
    pub fn new(mounts: Vec<VolumePosixBinding>) -> Self {
        Self {
            schema: VOLUME_POSIX_BINDINGS_SCHEMA.to_string(),
            mounts,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != VOLUME_POSIX_BINDINGS_SCHEMA {
            return Err(format!(
                "unsupported volume posix bindings schema: {}",
                self.schema
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for mount in &self.mounts {
            validate_guest_mount(&mount.guest_path)?;
            if !seen.insert(mount.guest_path.as_str()) {
                return Err(format!(
                    "duplicate volume posix binding {}",
                    mount.guest_path
                ));
            }
        }
        Ok(())
    }
}

fn validate_entries(
    entries: &[RootfsMetadataEntry],
    total: &mut usize,
    context: &str,
) -> Result<(), String> {
    let mut seen_entries = std::collections::BTreeSet::new();
    for entry in entries {
        if entry.uid > u32::MAX as u64 || entry.gid > u32::MAX as u64 {
            return Err("volume posix uid/gid exceeds Linux range".to_string());
        }
        if !seen_entries.insert(entry.path_base64.as_str()) {
            return Err(format!("duplicate volume posix entry under {context}"));
        }
        *total = total
            .checked_add(1)
            .ok_or_else(|| "volume posix metadata entry count overflowed".to_string())?;
        if *total > VOLUME_POSIX_METADATA_MAX_ENTRIES {
            return Err(format!(
                "volume posix metadata exceeds {VOLUME_POSIX_METADATA_MAX_ENTRIES} entries"
            ));
        }
    }
    Ok(())
}

/// Sidecar beside a VolumeStore directory. The directory itself is the
/// virtio-fs mount, so the file must not live inside it.
pub fn volume_posix_sidecar_path(volume_dir: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    let collapsed = lexical_volume_path(volume_dir);
    #[cfg(windows)]
    let volume_dir_located = collapsed.as_path();
    #[cfg(not(windows))]
    let volume_dir_located = volume_dir;
    let parent = volume_dir_located.parent()?;
    let name = volume_dir_located.file_name()?.to_str()?;
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        return None;
    }
    let name = sidecar_volume_name(volume_dir, name)?;
    Some(parent.join(format!("{name}{VOLUME_POSIX_SIDECAR_SUFFIX}")))
}

/// A non-verbatim Windows name drops trailing dots and spaces. Those
/// characters are not part of the directory Win32 opens, so the sidecar
/// stays beside that directory. A verbatim path keeps them.
fn sidecar_volume_name<'a>(volume_dir: &Path, name: &'a str) -> Option<&'a str> {
    #[cfg(windows)]
    {
        if !uses_verbatim_prefix(volume_dir) {
            let trimmed = name.trim_end_matches(['.', ' ']);
            if trimmed.is_empty() {
                return None;
            }
            return Some(trimmed);
        }
    }
    let _ = volume_dir;
    Some(name)
}

/// `host_path` is a managed volume when it is a direct child of `volumes_dir`.
///
/// On Windows the volume store directory is one directory when only ASCII
/// case differs, when a verbatim `\\?\` prefix or a `\\.\` drive prefix is
/// present, when `.` and `..` still name that directory, when a
/// non-verbatim name has a trailing dot or space, or when a generated 8.3
/// short name is the long directory. A non-verbatim `..` that steps out of
/// a child and lands on the volume still names that volume. A verbatim path
/// does not step through `.` or `..`.
pub fn managed_volume_directory(volumes_dir: &Path, host_path: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    let collapsed = lexical_volume_path(host_path);
    #[cfg(windows)]
    let host_located = collapsed.as_path();
    #[cfg(not(windows))]
    let host_located = host_path;
    let parent = host_located.parent()?;
    if !same_volume_store_directory(parent, volumes_dir) {
        return None;
    }
    let resolved = volume_posix_sidecar_path(host_path)?;
    if !same_volume_store_directory(resolved.parent()?, volumes_dir) {
        return None;
    }
    Some(host_path.to_path_buf())
}

fn same_volume_store_directory(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        return same_windows_directory(left, right);
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

/// Two Windows paths name one directory when only case, a verbatim
/// `\\?\` prefix, a `\\.\` drive prefix, `.`, `..` that steps back through a
/// normal component, or a trailing dot or space on a non-verbatim name
/// differ. Case uses the ordinal uppercase table, so `Größe` and `GrÖße`
/// name one directory and `ß` does not become `SS`. A `..` above a drive
/// root or UNC share stays there. A verbatim
/// path keeps `.`, `..`, and a trailing dot or space, because `\\?\` names
/// that path exactly and does not step through those components. A
/// `\\.\UNC\server\share` path names `\\server\share`. A `\\.\` name that is
/// not a drive letter or `UNC` stays distinct. A generated 8.3 name such as
/// `LONGVO~1` names the long directory when the operating system resolves
/// it. The sidecar path uses this collapsed directory, so a non-verbatim
/// child `..` still names the volume's sidecar.
#[cfg(windows)]
pub fn same_windows_directory(left: &Path, right: &Path) -> bool {
    let left = lexical_volume_components(left);
    let right = lexical_volume_components(right);
    left.len() == right.len()
        && left
            .iter()
            .zip(right.iter())
            .all(|(left, right)| ordinal_eq_ignore_case(left, right))
}

#[cfg(windows)]
fn ordinal_eq_ignore_case(left: &std::ffi::OsStr, right: &std::ffi::OsStr) -> bool {
    if left.eq_ignore_ascii_case(right) {
        return true;
    }
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Globalization::{CompareStringOrdinal, CSTR_EQUAL};

    let left: Vec<u16> = left.encode_wide().chain(std::iter::once(0)).collect();
    let right: Vec<u16> = right.encode_wide().chain(std::iter::once(0)).collect();
    unsafe { CompareStringOrdinal(left.as_ptr(), -1, right.as_ptr(), -1, 1) == CSTR_EQUAL as i32 }
}

#[cfg(windows)]
fn uses_verbatim_prefix(path: &Path) -> bool {
    matches!(
        path.components().next(),
        Some(std::path::Component::Prefix(prefix))
            if matches!(
                prefix.kind(),
                std::path::Prefix::Verbatim(_)
                    | std::path::Prefix::VerbatimDisk(_)
                    | std::path::Prefix::VerbatimUNC(_, _)
            )
    )
}

#[cfg(windows)]
fn strip_win32_trailing_dots_and_spaces(part: &std::ffi::OsStr) -> std::ffi::OsString {
    let Some(text) = part.to_str() else {
        return part.to_os_string();
    };
    std::ffi::OsString::from(text.trim_end_matches(['.', ' ']))
}

#[cfg(windows)]
fn lexical_volume_components(path: &Path) -> Vec<std::ffi::OsString> {
    let expanded = expand_generated_short_path(path);
    let path = expanded.as_path();
    let verbatim = uses_verbatim_prefix(path);
    let strip_trailing_dots_and_spaces = !verbatim;
    let path = without_verbatim_prefix(path);
    let mut parts = Vec::new();
    let mut normal_tail = 0usize;
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {
                if verbatim {
                    parts.push(std::ffi::OsString::from("."));
                    normal_tail += 1;
                }
            }
            std::path::Component::ParentDir => {
                if verbatim {
                    parts.push(std::ffi::OsString::from(".."));
                    normal_tail += 1;
                } else if normal_tail > 0 {
                    parts.pop();
                    normal_tail -= 1;
                }
            }
            std::path::Component::Normal(part) => {
                let part = if strip_trailing_dots_and_spaces {
                    strip_win32_trailing_dots_and_spaces(part)
                } else {
                    part.to_os_string()
                };
                if part.is_empty() {
                    continue;
                }
                parts.push(part);
                normal_tail += 1;
            }
            other => {
                parts.push(other.as_os_str().to_os_string());
                normal_tail = 0;
            }
        }
    }
    parts
}

/// Collapse `.` and `..` the same way [`same_windows_directory`] does, and
/// return that directory. A verbatim path keeps `.`, `..`, and trailing dots
/// and spaces in the name.
#[cfg(windows)]
fn lexical_volume_path(path: &Path) -> PathBuf {
    let expanded = expand_generated_short_path(path);
    let path = expanded.as_path();
    let verbatim = uses_verbatim_prefix(path);
    let strip_trailing_dots_and_spaces = !verbatim;
    let path = without_verbatim_prefix(path);
    let mut prefix = PathBuf::new();
    let mut normals = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {
                if verbatim {
                    normals.push(std::ffi::OsString::from("."));
                }
            }
            std::path::Component::ParentDir => {
                if verbatim {
                    normals.push(std::ffi::OsString::from(".."));
                } else {
                    normals.pop();
                }
            }
            std::path::Component::Normal(part) => {
                let part = if strip_trailing_dots_and_spaces {
                    strip_win32_trailing_dots_and_spaces(part)
                } else {
                    part.to_os_string()
                };
                if part.is_empty() {
                    continue;
                }
                normals.push(part);
            }
            std::path::Component::Prefix(prefix_component) => {
                prefix = PathBuf::from(prefix_component.as_os_str());
            }
            std::path::Component::RootDir => {
                let text = prefix.to_string_lossy();
                if text.is_empty() {
                    prefix = PathBuf::from(r"\");
                } else if !text.ends_with('\\') && !text.ends_with('/') {
                    prefix = PathBuf::from(format!("{text}\\"));
                }
            }
        }
    }
    for normal in normals {
        prefix.push(normal);
    }
    prefix
}

/// Parent of the collapsed volume directory. `volumes/data/foo/..` is stored
/// beside `volumes`, because that path names `volumes/data`.
#[cfg(windows)]
pub fn volume_directory_parent(host_path: &Path) -> Option<PathBuf> {
    let collapsed = lexical_volume_path(host_path);
    let name = collapsed.file_name()?.to_str()?;
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        return None;
    }
    sidecar_volume_name(host_path, name)?;
    Some(collapsed.parent()?.to_path_buf())
}

/// `LONGVO~1` is the generated short name of `LongVolumeName` on NTFS.
/// Win32 opens that short name as the long directory, including under
/// `\\?\`. A non-verbatim trailing dot or space is not part of the short
/// name, so `LONGVO~1.` still names that directory. A non-ASCII short name
/// such as the generated name of a Unicode directory is the same shape. Ask for the long path
/// only when a component has that shape, and keep the original path when
/// the operating system does not resolve it. A verbatim path keeps the
/// trailing dot or space.
#[cfg(windows)]
fn expand_generated_short_path(path: &Path) -> PathBuf {
    let lookup = short_name_lookup_path(path);
    if !has_generated_short_name(&lookup) {
        return path.to_path_buf();
    }
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::GetLongPathNameW;

    let wide: Vec<u16> = lookup
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut capacity = 512u32;
    loop {
        let mut buffer = vec![0u16; capacity as usize];
        let length = unsafe { GetLongPathNameW(wide.as_ptr(), buffer.as_mut_ptr(), capacity) };
        if length == 0 {
            return path.to_path_buf();
        }
        if length < capacity {
            buffer.truncate(length as usize);
            return PathBuf::from(std::ffi::OsString::from_wide(&buffer));
        }
        if length > 32_768 {
            return path.to_path_buf();
        }
        capacity = length;
    }
}

/// Non-verbatim Win32 names drop trailing dots and spaces before an 8.3
/// lookup. Verbatim paths are already exact.
#[cfg(windows)]
fn short_name_lookup_path(path: &Path) -> PathBuf {
    if uses_verbatim_prefix(path) {
        return path.to_path_buf();
    }
    let mut prefix = PathBuf::new();
    let mut normals = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normals.push(std::ffi::OsString::from(".."));
            }
            std::path::Component::Normal(part) => {
                let part = strip_win32_trailing_dots_and_spaces(part);
                if part.is_empty() {
                    continue;
                }
                normals.push(part);
            }
            std::path::Component::Prefix(prefix_component) => {
                prefix = PathBuf::from(prefix_component.as_os_str());
            }
            std::path::Component::RootDir => {
                let text = prefix.to_string_lossy();
                if text.is_empty() {
                    prefix = PathBuf::from(r"\");
                } else if !text.ends_with('\\') && !text.ends_with('/') {
                    prefix = PathBuf::from(format!("{text}\\"));
                }
            }
        }
    }
    for normal in normals {
        prefix.push(normal);
    }
    prefix
}

#[cfg(windows)]
fn has_generated_short_name(path: &Path) -> bool {
    path.components().any(|component| match component {
        std::path::Component::Normal(part) => is_generated_short_name(part),
        _ => false,
    })
}

#[cfg(windows)]
fn is_generated_short_name(part: &std::ffi::OsStr) -> bool {
    let Some(text) = part.to_str() else {
        return false;
    };
    let base = match text.split_once('.') {
        Some((base, extension)) => {
            if !is_short_name_token(extension, 3) {
                return false;
            }
            base
        }
        None => text,
    };
    let Some((name, number)) = base.split_once('~') else {
        return false;
    };
    is_short_name_token(name, 6)
        && !number.is_empty()
        && number.chars().all(|character| character.is_ascii_digit())
}

/// A generated 8.3 body is at most six characters, or three in an extension.
/// NTFS keeps non-ASCII characters in that body, so the check is not limited
/// to ASCII letters.
#[cfg(windows)]
fn is_short_name_token(text: &str, max_chars: usize) -> bool {
    let mut count = 0usize;
    for character in text.chars() {
        if character == '.'
            || character == '~'
            || character == '/'
            || character == '\\'
            || character.is_whitespace()
            || character.is_control()
        {
            return false;
        }
        count += 1;
        if count > max_chars {
            return false;
        }
    }
    count > 0
}

#[cfg(windows)]
fn without_verbatim_prefix(path: &Path) -> PathBuf {
    let mut components = path.components();
    let Some(std::path::Component::Prefix(prefix)) = components.next() else {
        return path.to_path_buf();
    };
    let rebuilt = match prefix.kind() {
        std::path::Prefix::VerbatimDisk(disk) => {
            Some(PathBuf::from(format!("{}:\\", disk as char)))
        }
        std::path::Prefix::VerbatimUNC(server, share) => Some(PathBuf::from(format!(
            r"\\{}\{}",
            server.to_string_lossy(),
            share.to_string_lossy()
        ))),
        std::path::Prefix::DeviceNS(device) => {
            if let Some(disk) = device_namespace_disk(device) {
                Some(disk)
            } else if device.eq_ignore_ascii_case("UNC") {
                device_namespace_unc_root(&mut components)
            } else {
                None
            }
        }
        _ => None,
    };
    let Some(mut rebuilt) = rebuilt else {
        return path.to_path_buf();
    };
    for component in components {
        match component {
            std::path::Component::Normal(part) => rebuilt.push(part),
            std::path::Component::CurDir => rebuilt.push("."),
            std::path::Component::ParentDir => rebuilt.push(".."),
            _ => {}
        }
    }
    rebuilt
}

/// `\\.\C:` names the same drive as `C:`. `\\.\UNC\server\share` names
/// `\\server\share`. Other device names do not.
#[cfg(windows)]
fn device_namespace_disk(device: &std::ffi::OsStr) -> Option<PathBuf> {
    let text = device.to_str()?;
    let mut chars = text.chars();
    let disk = chars.next()?;
    if chars.as_str() == ":" && disk.is_ascii_alphabetic() {
        return Some(PathBuf::from(format!("{}:\\", disk)));
    }
    None
}

/// `\\.\UNC\server\share\...` is the device-namespace spelling of
/// `\\server\share\...`.
#[cfg(windows)]
fn device_namespace_unc_root(components: &mut std::path::Components<'_>) -> Option<PathBuf> {
    if !matches!(components.next(), Some(std::path::Component::RootDir)) {
        return None;
    }
    let server = match components.next() {
        Some(std::path::Component::Normal(part)) if !part.is_empty() => part.to_os_string(),
        _ => return None,
    };
    let share = match components.next() {
        Some(std::path::Component::Normal(part)) if !part.is_empty() => part.to_os_string(),
        _ => return None,
    };
    Some(PathBuf::from(format!(
        r"\\{}\{}",
        server.to_string_lossy(),
        share.to_string_lossy()
    )))
}

/// Replace a volume's sidecar. A symlink or directory at the sidecar path is
/// rejected so a planted link cannot redirect the write.
pub fn write_volume_posix_sidecar(
    volume_dir: &Path,
    sidecar: &VolumePosixSidecar,
) -> std::io::Result<()> {
    sidecar
        .validate()
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidData, message))?;
    let path = volume_posix_sidecar_path(volume_dir).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "volume posix sidecar path is not a direct volume directory",
        )
    })?;
    reject_planted_sidecar(&path)?;
    let bytes = serde_json::to_vec(sidecar)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if bytes.len() as u64 > VOLUME_POSIX_METADATA_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "volume posix sidecar exceeds the size limit",
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    crate::fs_atomic::write_durable(&tmp, &path, &bytes)
}

pub fn read_volume_posix_sidecar(volume_dir: &Path) -> std::io::Result<Option<VolumePosixSidecar>> {
    let Some(path) = volume_posix_sidecar_path(volume_dir) else {
        return Ok(None);
    };
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) if is_regular_file(&metadata) => metadata,
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "volume posix sidecar is not a regular file",
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if metadata.len() > VOLUME_POSIX_METADATA_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "volume posix sidecar exceeds the size limit",
        ));
    }
    let file = std::fs::File::open(&path)?;
    let sidecar: VolumePosixSidecar = serde_json::from_reader(file)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    sidecar
        .validate()
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidData, message))?;
    Ok(Some(sidecar))
}

pub fn remove_volume_posix_sidecar(volume_dir: &Path) -> std::io::Result<()> {
    let Some(path) = volume_posix_sidecar_path(volume_dir) else {
        return Ok(());
    };
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if is_regular_file(&metadata) => std::fs::remove_file(path),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "volume posix sidecar is not a regular file",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn reject_planted_sidecar(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if is_regular_file(&metadata) => Ok(()),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "volume posix sidecar is not a regular file",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn is_regular_file(metadata: &std::fs::Metadata) -> bool {
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x0000_0400 != 0 {
            return false;
        }
    }
    true
}

/// Reject guest paths that could escape the share or cover runtime state.
pub fn validate_guest_mount(path: &str) -> Result<(), String> {
    if path.contains('\0') || path.contains('\\') {
        return Err("volume posix guest path contains NUL or a backslash".to_string());
    }
    if !path.starts_with('/') || path == "/" {
        return Err(
            "volume posix guest path must be absolute and must not be the guest root".to_string(),
        );
    }
    if path.contains("//") {
        return Err("volume posix guest path contains an empty component".to_string());
    }
    let relative = path.trim_start_matches('/');
    if relative
        .split('/')
        .any(|component| component == "." || component == "..")
    {
        return Err("volume posix guest path contains . or ..".to_string());
    }
    let relative_path = std::path::Path::new(relative);
    if is_runtime_internal_rootfs_path(relative_path) {
        return Err(format!(
            "volume posix guest path {path} overlaps reserved runtime state"
        ));
    }
    const RESERVED: &[&str] = &["proc", "sys", "dev"];
    if let Some(first) = relative.split('/').next() {
        if RESERVED.contains(&first) {
            return Err(format!(
                "volume posix guest path {path} overlaps a reserved guest mount"
            ));
        }
    }
    Ok(())
}

/// True when this encoded path belongs to a nested share of `parent_guest`.
///
/// An entry that is not a relative path inside the share is covered too, so
/// replay does not chmod it.
pub fn encoded_entry_covered_by_nested_share(
    parent_guest: &str,
    path_base64: &str,
    guest_paths: &[&str],
) -> bool {
    match decode_entry_relative(path_base64) {
        Some(relative) => entry_covers_nested_share(parent_guest, &relative, guest_paths),
        None => true,
    }
}

/// True when `relative` is a nested share's root or a path inside that share.
pub fn entry_covers_nested_share(
    parent_guest: &str,
    relative: &Path,
    guest_paths: &[&str],
) -> bool {
    let parent_components: Vec<&str> = guest_components(parent_guest).collect();
    let mut components = parent_components.clone();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            continue;
        };
        let Some(text) = name.to_str() else {
            return false;
        };
        components.push(text);
    }
    guest_paths.iter().any(|other| {
        if *other == parent_guest {
            return false;
        }
        let nested: Vec<&str> = guest_components(other).collect();
        nested.len() > parent_components.len()
            && nested.starts_with(&parent_components)
            && components.len() >= nested.len()
            && components.starts_with(&nested)
    })
}

/// Drop entries that belong to a nested share, or that leave this share.
pub fn strip_entries_covered_by_nested_mounts(mounts: &mut [VolumePosixMount]) -> bool {
    let owned_paths: Vec<String> = mounts
        .iter()
        .map(|mount| mount.guest_path.clone())
        .collect();
    let guest_paths: Vec<&str> = owned_paths.iter().map(String::as_str).collect();
    let mut changed = false;
    for mount in mounts.iter_mut() {
        let before = mount.entries.len();
        let parent = mount.guest_path.clone();
        mount
            .entries
            .retain(|entry| match decode_entry_relative(&entry.path_base64) {
                Some(relative) => !entry_covers_nested_share(&parent, &relative, &guest_paths),
                None => false,
            });
        if mount.entries.len() != before {
            changed = true;
        }
    }
    changed
}

fn guest_components(path: &str) -> impl Iterator<Item = &str> {
    path.trim_matches('/')
        .split('/')
        .filter(|component| !component.is_empty())
}

fn decode_entry_relative(encoded: &str) -> Option<PathBuf> {
    let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded).ok()?;
    let text = std::str::from_utf8(&raw).ok()?;
    let mut clean = PathBuf::new();
    for component in Path::new(text).components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(name) => clean.push(name),
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => return None,
        }
    }
    Some(clean)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rootfs_metadata::is_runtime_internal_rootfs_path;
    use std::path::Path;

    #[cfg(windows)]
    #[test]
    fn ordinal_case_folds_o_umlaut_and_keeps_eszett() {
        assert!(same_windows_directory(
            Path::new(r"C:\volumes\Größe"),
            Path::new(r"C:\volumes\GrÖße")
        ));
        assert!(!same_windows_directory(
            Path::new(r"C:\volumes\Straße"),
            Path::new(r"C:\volumes\STRASSE")
        ));
    }

    #[test]
    fn strip_drops_parent_entries_inside_a_nested_mount() {
        let encode =
            |path: &str| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, path);
        let entry = |path: &str| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: crate::rootfs_metadata::RootfsEntryKind::Regular,
            mode: 0o644,
            uid: 1,
            gid: 1,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        };
        let mut mounts = vec![
            VolumePosixMount {
                guest_path: "/workspace".to_string(),
                entries: vec![entry("."), entry("note.txt"), entry("cache/secret.txt")],
                retained: false,
            },
            VolumePosixMount {
                guest_path: "/workspace/cache".to_string(),
                entries: vec![entry("secret.txt")],
                retained: false,
            },
        ];
        assert!(strip_entries_covered_by_nested_mounts(&mut mounts));
        let parent: Vec<String> = mounts[0]
            .entries
            .iter()
            .map(|entry| {
                let raw = base64::Engine::decode(
                    &base64::engine::general_purpose::STANDARD,
                    &entry.path_base64,
                )
                .unwrap();
                String::from_utf8(raw).unwrap()
            })
            .collect();
        assert_eq!(parent, vec![".".to_string(), "note.txt".to_string()]);
        assert_eq!(mounts[1].entries.len(), 1);
        assert!(!strip_entries_covered_by_nested_mounts(&mut mounts));
    }

    #[test]
    fn strip_drops_entries_that_escape_the_share() {
        let encode =
            |path: &str| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, path);
        let entry = |path: &str| RootfsMetadataEntry {
            path_base64: encode(path),
            kind: crate::rootfs_metadata::RootfsEntryKind::Regular,
            mode: 0o644,
            uid: 1,
            gid: 1,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        };
        let mut mounts = vec![VolumePosixMount {
            guest_path: "/data".to_string(),
            entries: vec![entry("note.txt"), entry("../outside")],
            retained: false,
        }];
        assert!(strip_entries_covered_by_nested_mounts(&mut mounts));
        assert_eq!(mounts[0].entries.len(), 1);
        assert_eq!(mounts[0].entries[0].path_base64, encode("note.txt"));
    }

    #[test]
    fn volume_posix_manifest_files_are_runtime_internal() {
        assert!(is_runtime_internal_rootfs_path(Path::new(
            VOLUME_POSIX_METADATA_FILE
        )));
        assert!(is_runtime_internal_rootfs_path(Path::new(
            VOLUME_POSIX_METADATA_TEMP_FILE
        )));
        assert!(is_runtime_internal_rootfs_path(Path::new(
            VOLUME_POSIX_METADATA_PENDING_FILE
        )));
        assert!(is_runtime_internal_rootfs_path(Path::new(
            VOLUME_POSIX_METADATA_PENDING_TEMP_FILE
        )));
        assert!(is_runtime_internal_rootfs_path(Path::new(
            VOLUME_POSIX_METADATA_PUBLISH_TEMP_FILE
        )));
    }

    #[test]
    fn volume_posix_guest_mounts_reject_escape_and_reserved_roots() {
        assert!(validate_guest_mount("/workspace").is_ok());
        assert!(validate_guest_mount("/data/cache").is_ok());
        assert!(validate_guest_mount("/").is_err());
        assert!(validate_guest_mount("workspace").is_err());
        assert!(validate_guest_mount("/data/../etc").is_err());
        assert!(validate_guest_mount("/proc/self").is_err());
        assert!(validate_guest_mount("/run/a3s-box").is_err());
        assert!(validate_guest_mount("/.a3s_volume_metadata_v1.json").is_err());
    }

    #[test]
    fn volume_posix_manifest_rejects_duplicate_mounts() {
        let mount = VolumePosixMount {
            guest_path: "/workspace".to_string(),
            entries: Vec::new(),
            retained: false,
        };
        let manifest = VolumePosixManifest::new(vec![mount.clone(), mount]);
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn volume_posix_sidecar_is_a_sibling_and_roundtrips() {
        use crate::rootfs_metadata::{RootfsEntryKind, RootfsMetadataEntry};

        let dir = tempfile::tempdir().unwrap();
        let volumes = dir.path().join("volumes");
        let volume = volumes.join("data");
        let bind = dir.path().join("host-bind");
        std::fs::create_dir_all(&volume).unwrap();
        std::fs::create_dir_all(&bind).unwrap();

        assert_eq!(
            managed_volume_directory(&volumes, &volume).as_deref(),
            Some(volume.as_path())
        );
        assert!(managed_volume_directory(&volumes, &bind).is_none());
        assert!(managed_volume_directory(&volumes, &volumes.join("nested").join("data")).is_none());

        let entry = RootfsMetadataEntry {
            path_base64: "Lg==".to_string(),
            kind: RootfsEntryKind::Directory,
            mode: 0o750,
            uid: 1000,
            gid: 1000,
            mtime: 0,
            size: 0,
            link_target_base64: None,
        };
        write_volume_posix_sidecar(&volume, &VolumePosixSidecar::new(vec![entry.clone()])).unwrap();
        let sidecar = volume_posix_sidecar_path(&volume).unwrap();
        assert_eq!(sidecar.parent(), Some(volumes.as_path()));
        assert!(!volume.join(VOLUME_POSIX_SIDECAR_SUFFIX).exists());
        assert!(!volume.join(sidecar.file_name().unwrap()).exists());

        let loaded = read_volume_posix_sidecar(&volume).unwrap().unwrap();
        assert_eq!(loaded.entries, vec![entry]);
        remove_volume_posix_sidecar(&volume).unwrap();
        assert!(read_volume_posix_sidecar(&volume).unwrap().is_none());
    }
}
