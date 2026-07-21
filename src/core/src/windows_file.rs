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

    fn symlink_file_or_skip(target: &Path, link: &Path) -> bool {
        match std::os::windows::fs::symlink_file(target, link) {
            Ok(()) => true,
            Err(error) if error.raw_os_error() == Some(1314) => false,
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
