//! Safe access to files whose final path component is writable by a Windows guest.
//!
//! WHPX shares the extracted rootfs with the guest. A guest running as root can
//! replace a log or marker path with a symbolic link/reparse point, so ordinary
//! `File::open` would let it redirect host reads or writes outside the rootfs.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, GetFileType, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_TYPE_DISK,
};

/// Stable identity of one regular file on a Windows volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowsFileIdentity {
    pub volume_serial_number: u32,
    pub file_id: u64,
}

/// Open a regular disk file without following a final reparse point.
///
/// When `expected` is present, replacement by a different regular file is also
/// rejected. This is required by tailers which reopen a path after reaching EOF.
pub fn open_regular_file(
    path: &Path,
    expected: Option<WindowsFileIdentity>,
) -> io::Result<(File, WindowsFileIdentity)> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    open_and_validate(path, &options, expected)
}

/// Open a regular disk file for truncation without following a final reparse
/// point, optionally requiring it to be the same file opened by a tailer.
pub fn open_regular_file_for_write(
    path: &Path,
    expected: Option<WindowsFileIdentity>,
) -> io::Result<(File, WindowsFileIdentity)> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    open_and_validate(path, &options, expected)
}

fn open_and_validate(
    path: &Path,
    options: &OpenOptions,
    expected: Option<WindowsFileIdentity>,
) -> io::Result<(File, WindowsFileIdentity)> {
    let file = options.open(path)?;
    let identity = regular_file_identity(&file)?;
    if expected.is_some_and(|expected| expected != identity) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing replaced Windows guest file {} (expected {expected:?}, opened {identity:?})",
                path.display()
            ),
        ));
    }
    Ok((file, identity))
}

/// Return the volume/file identity after verifying that `file` is a regular
/// disk file and its handle does not refer to a reparse point.
pub fn regular_file_identity(file: &File) -> io::Result<WindowsFileIdentity> {
    let handle = file.as_raw_handle() as HANDLE;
    let mut information = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: `handle` belongs to `file`; the output points to writable storage
    // of the exact structure expected by GetFileInformationByHandle.
    if unsafe { GetFileInformationByHandle(handle, information.as_mut_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the API reported success and initialized the output structure.
    let information = unsafe { information.assume_init() };
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to follow a Windows reparse point",
        ));
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0
        || unsafe { GetFileType(handle) } != FILE_TYPE_DISK
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows guest path is not a regular disk file",
        ));
    }

    Ok(WindowsFileIdentity {
        volume_serial_number: information.dwVolumeSerialNumber,
        file_id: (u64::from(information.nFileIndexHigh) << 32)
            | u64::from(information.nFileIndexLow),
    })
}

/// Remove one file, empty directory, or reparse point without traversing it.
/// Missing paths are already in the desired state.
pub fn remove_path_no_follow(path: &Path) -> io::Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };

    if metadata.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0 {
        std::fs::remove_dir(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Recreate a directory junction without copying the target.
///
/// `Ok(false)` means `source` is not a mount-point junction. A junction does
/// not need `SeCreateSymbolicLinkPrivilege`, so snapshot and rootfs copies
/// keep the link instead of failing closed or walking into the target.
pub fn recreate_directory_junction(source: &Path, destination: &Path) -> io::Result<bool> {
    if !is_mount_point_junction(source)? {
        return Ok(false);
    }
    let target = std::fs::read_link(source)?;
    remove_directory_junction(destination)?;
    create_mount_point_junction(destination, &target)?;
    Ok(true)
}

/// Remove a destination mount-point junction without deleting its target.
///
/// `Ok(false)` means `path` is missing or is not a mount-point junction.
/// Directory copies call this before `create_dir_all`, which otherwise follows
/// the junction and writes into the target.
pub fn remove_directory_junction(path: &Path) -> io::Result<bool> {
    match is_mount_point_junction(path) {
        Ok(false) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
        Ok(true) => {
            std::fs::remove_dir(path)?;
            Ok(true)
        }
    }
}

fn is_mount_point_junction(path: &Path) -> io::Result<bool> {
    use std::mem::size_of;
    use windows_sys::Win32::Storage::FileSystem::{
        FileAttributeTagInfo, GetFileInformationByHandleEx, FILE_ATTRIBUTE_TAG_INFO,
        FILE_FLAG_BACKUP_SEMANTICS,
    };

    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;

    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path)?;
    let mut info = FILE_ATTRIBUTE_TAG_INFO {
        FileAttributes: 0,
        ReparseTag: 0,
    };
    let read = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileAttributeTagInfo,
            &mut info as *mut FILE_ATTRIBUTE_TAG_INFO as *mut _,
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if read == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.ReparseTag == IO_REPARSE_TAG_MOUNT_POINT)
}

fn create_mount_point_junction(link: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    use windows_sys::Win32::System::IO::DeviceIoControl;

    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
    const FSCTL_SET_REPARSE_POINT: u32 = 589988;

    let wide = target.as_os_str().encode_wide().collect::<Vec<_>>();
    let print = strip_verbatim_prefix(&wide);
    if print.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "directory junction target is empty",
        ));
    }
    let mut substitute = vec![
        u16::from(b'\\'),
        u16::from(b'?'),
        u16::from(b'?'),
        u16::from(b'\\'),
    ];
    if print.len() >= 2 && print[0] == u16::from(b'\\') && print[1] == u16::from(b'\\') {
        substitute.extend([
            u16::from(b'U'),
            u16::from(b'N'),
            u16::from(b'C'),
            u16::from(b'\\'),
        ]);
        substitute.extend_from_slice(&print[2..]);
    } else {
        substitute.extend_from_slice(print);
    }
    let mut print_name = print.to_vec();
    substitute.push(0);
    print_name.push(0);
    let substitute_len = u16::try_from((substitute.len() - 1) * 2).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "directory junction target is too long",
        )
    })?;
    let print_len = u16::try_from((print_name.len() - 1) * 2).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "directory junction target is too long",
        )
    })?;
    let print_offset = u16::try_from(substitute.len() * 2).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "directory junction target is too long",
        )
    })?;
    let data_len = print_offset
        .checked_add(u16::try_from(print_name.len() * 2).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "directory junction target is too long",
            )
        })?)
        .and_then(|len| len.checked_add(8))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "directory junction target is too long",
            )
        })?;

    let mut buffer = Vec::with_capacity(8 + usize::from(data_len));
    buffer.extend_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    buffer.extend_from_slice(&data_len.to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes());
    buffer.extend_from_slice(&substitute_len.to_le_bytes());
    buffer.extend_from_slice(&print_offset.to_le_bytes());
    buffer.extend_from_slice(&print_len.to_le_bytes());
    for unit in substitute.into_iter().chain(print_name) {
        buffer.extend_from_slice(&unit.to_le_bytes());
    }

    std::fs::create_dir(link)?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let file = match options.open(link) {
        Ok(file) => file,
        Err(error) => {
            let _ = std::fs::remove_dir(link);
            return Err(error);
        }
    };
    let mut returned = 0u32;
    let set = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as HANDLE,
            FSCTL_SET_REPARSE_POINT,
            buffer.as_ptr() as *const _,
            buffer.len() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    drop(file);
    if set == 0 {
        let error = io::Error::last_os_error();
        let _ = std::fs::remove_dir(link);
        return Err(error);
    }
    Ok(())
}

fn strip_verbatim_prefix(wide: &[u16]) -> &[u16] {
    if wide.len() >= 4
        && wide[0] == u16::from(b'\\')
        && wide[1] == u16::from(b'\\')
        && wide[2] == u16::from(b'?')
        && wide[3] == u16::from(b'\\')
    {
        &wide[4..]
    } else {
        wide
    }
}

/// Replace an untrusted marker/stream path with a new regular file.
///
/// Removal never follows a reparse point and `create_new` makes the final create
/// atomic: a path inserted between removal and creation causes a safe failure.
pub fn replace_regular_file(path: &Path, contents: &[u8]) -> io::Result<WindowsFileIdentity> {
    remove_path_no_follow(path)?;

    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let (mut file, identity) = open_and_validate(path, &options, None)?;
    file.write_all(contents)?;
    file.flush()?;
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows_symlink::{is_capability_denial, WindowsSymlinkPrivilegeGuard};

    fn symlink_file_or_skip(target: &Path, link: &Path) -> bool {
        let guard = WindowsSymlinkPrivilegeGuard::acquire();
        let assigned_privilege_enabled = guard.assigned_privilege_enabled();
        match std::os::windows::fs::symlink_file(target, link) {
            Ok(()) => true,
            Err(error) if is_capability_denial(&error, assigned_privilege_enabled) => false,
            Err(error) => panic!("failed to create test symlink: {error}"),
        }
    }

    #[test]
    fn rejects_reparse_points_and_preserves_their_targets() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("host-secret.txt");
        let link = temp.path().join("guest.log");
        std::fs::write(&target, b"host secret").unwrap();
        if !symlink_file_or_skip(&target, &link) {
            return;
        }

        assert!(open_regular_file(&link, None).is_err());
        replace_regular_file(&link, b"safe marker\n").unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"host secret");
        assert_eq!(std::fs::read(&link).unwrap(), b"safe marker\n");
        assert!(!std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn rejects_a_regular_file_replacement_when_identity_is_pinned() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("guest.log");
        std::fs::write(&path, b"first").unwrap();
        let (original, identity) = open_regular_file(&path, None).unwrap();

        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"replacement").unwrap();

        assert!(open_regular_file(&path, Some(identity)).is_err());
        drop(original);
    }
}
