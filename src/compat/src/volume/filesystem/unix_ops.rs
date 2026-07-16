use std::ffi::{CStr, CString, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::{
    VolumeContentError, VolumeContentResult, VolumeEntry, VolumeEntryType, VolumeIdMapper,
    VolumeMetadataUpdate, MAX_DIRECTORY_DEPTH,
};

const MAX_PATH_BYTES: usize = 4096;
const MAX_COMPONENT_BYTES: usize = 255;
const DEFAULT_DIRECTORY_MODE: u32 = 0o755;
const DEFAULT_FILE_MODE: u32 = 0o644;
const INTERNAL_UPLOAD_PREFIX: &str = ".a3s-upload-";

pub struct PreparedUpload {
    parent: OwnedFd,
    temporary_name: CString,
    final_name: CString,
    temporary_device: libc::dev_t,
    temporary_inode: libc::ino_t,
    host_uid: u32,
    host_gid: u32,
    mode: u32,
    force: bool,
    armed: bool,
}

impl Drop for PreparedUpload {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(stat) = stat_at(&self.parent, &self.temporary_name) else {
            return;
        };
        if stat.st_dev == self.temporary_device && stat.st_ino == self.temporary_inode {
            unsafe {
                libc::unlinkat(self.parent.as_raw_fd(), self.temporary_name.as_ptr(), 0);
            }
        }
    }
}

pub fn initialize_root(root: &Path, ids: &dyn VolumeIdMapper) -> VolumeContentResult<()> {
    let root = open_root(root)?;
    set_identity_and_mode(
        root.as_raw_fd(),
        ids.host_uid(0)?,
        ids.host_gid(0)?,
        DEFAULT_DIRECTORY_MODE,
    )
}

pub fn stat_path(
    root: &Path,
    path: &str,
    ids: &dyn VolumeIdMapper,
) -> VolumeContentResult<VolumeEntry> {
    let path = NormalizedPath::parse(path)?;
    let root = open_root(root)?;
    if path.components.is_empty() {
        let stat = fstat(&root)?;
        return entry_from_stat(&root, None, "/", "/", &stat, ids);
    }
    let (parent, name) = open_parent(root, &path.components)?;
    let stat = stat_at(&parent, name)?;
    entry_from_stat(&parent, Some(name), path.name(), &path.display, &stat, ids)
}

pub fn list_path(
    root: &Path,
    path: &str,
    depth: u32,
    ids: &dyn VolumeIdMapper,
) -> VolumeContentResult<Vec<VolumeEntry>> {
    if depth > MAX_DIRECTORY_DEPTH {
        return Err(VolumeContentError::InvalidPath(format!(
            "directory depth cannot exceed {MAX_DIRECTORY_DEPTH}"
        )));
    }
    let path = NormalizedPath::parse(path)?;
    let root = open_root(root)?;
    let directory = open_directory_path(root, &path.components)?;
    let mut entries = Vec::new();
    if depth > 0 {
        list_directory(&directory, &path.display, depth, ids, &mut entries)?;
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

pub fn make_dir(
    root: &Path,
    path: &str,
    metadata: VolumeMetadataUpdate,
    force: bool,
    ids: &dyn VolumeIdMapper,
) -> VolumeContentResult<VolumeEntry> {
    let path = NormalizedPath::parse_non_root(path)?;
    let requested_mode = validate_mode(metadata.mode, DEFAULT_DIRECTORY_MODE)?;
    let requested_uid = ids.host_uid(metadata.uid.unwrap_or(0))?;
    let requested_gid = ids.host_gid(metadata.gid.unwrap_or(0))?;
    let default_uid = ids.host_uid(0)?;
    let default_gid = ids.host_gid(0)?;
    let mut current = open_root(root)?;

    if !force {
        let (name, parents) = path.components.split_last().ok_or_else(|| {
            VolumeContentError::InvalidPath("path identifies the root".to_string())
        })?;
        current = open_directory_path(current, parents)?;
        mkdir_at(&current, name, 0o700)?;
        let directory = open_directory_at(&current, name)?;
        set_identity_and_mode(
            directory.as_raw_fd(),
            requested_uid,
            requested_gid,
            requested_mode,
        )?;
        let stat = fstat(&directory)?;
        return entry_from_stat(&directory, None, path.name(), &path.display, &stat, ids);
    }

    for (index, name) in path.components.iter().enumerate() {
        let last = index + 1 == path.components.len();
        let created = match mkdir_at(&current, name, 0o700) {
            Ok(()) => true,
            Err(VolumeContentError::Conflict) => false,
            Err(error) => return Err(error),
        };
        let next = open_directory_at(&current, name)?;
        if created {
            let (uid, gid, mode) = if last {
                (requested_uid, requested_gid, requested_mode)
            } else {
                (default_uid, default_gid, DEFAULT_DIRECTORY_MODE)
            };
            set_identity_and_mode(next.as_raw_fd(), uid, gid, mode)?;
        }
        current = next;
    }

    let stat = fstat(&current)?;
    entry_from_stat(&current, None, path.name(), &path.display, &stat, ids)
}

pub fn update_metadata(
    root: &Path,
    path: &str,
    metadata: VolumeMetadataUpdate,
    ids: &dyn VolumeIdMapper,
) -> VolumeContentResult<VolumeEntry> {
    let path = NormalizedPath::parse_non_root(path)?;
    let root = open_root(root)?;
    let (parent, name) = open_parent(root, &path.components)?;
    let before = stat_at(&parent, name)?;
    let uid = metadata.uid.map(|value| ids.host_uid(value)).transpose()?;
    let gid = metadata.gid.map(|value| ids.host_gid(value)).transpose()?;
    let mode = metadata
        .mode
        .map(|value| validate_mode(Some(value), value))
        .transpose()?;

    if file_type(&before) == VolumeEntryType::Symlink {
        if mode.is_some() {
            return Err(VolumeContentError::InvalidPath(
                "symlink mode updates are not supported".to_string(),
            ));
        }
        if uid.is_some() || gid.is_some() {
            let result = unsafe {
                libc::fchownat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    uid.unwrap_or(u32::MAX) as libc::uid_t,
                    gid.unwrap_or(u32::MAX) as libc::gid_t,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            cvt(result, "change symlink ownership")?;
        }
    } else {
        let mut flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
        if file_type(&before) == VolumeEntryType::Directory {
            flags |= libc::O_DIRECTORY;
        }
        let entry = open_at(&parent, name, flags, 0)?;
        if uid.is_some() || gid.is_some() {
            let result = unsafe {
                libc::fchown(
                    entry.as_raw_fd(),
                    uid.unwrap_or(u32::MAX) as libc::uid_t,
                    gid.unwrap_or(u32::MAX) as libc::gid_t,
                )
            };
            cvt(result, "change volume ownership")?;
        }
        if let Some(mode) = mode {
            let result = unsafe { libc::fchmod(entry.as_raw_fd(), mode as libc::mode_t) };
            cvt(result, "change volume mode")?;
        }
    }

    let stat = stat_at(&parent, name)?;
    entry_from_stat(&parent, Some(name), path.name(), &path.display, &stat, ids)
}

pub fn remove_path(root: &Path, path: &str) -> VolumeContentResult<()> {
    let path = NormalizedPath::parse_non_root(path)?;
    let root = open_root(root)?;
    let (parent, name) = open_parent(root, &path.components)?;
    remove_entry(&parent, name)
}

pub fn open_file(root: &Path, path: &str) -> VolumeContentResult<File> {
    let path = NormalizedPath::parse_non_root(path)?;
    let root = open_root(root)?;
    let (parent, name) = open_parent(root, &path.components)?;
    let file = open_at(
        &parent,
        name,
        libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        0,
    )?;
    let stat = fstat(&file)?;
    if file_type(&stat) != VolumeEntryType::File {
        return Err(VolumeContentError::InvalidPath(
            "requested path is not a regular file".to_string(),
        ));
    }
    Ok(File::from(file))
}

pub fn prepare_upload(
    root: &Path,
    path: &str,
    metadata: VolumeMetadataUpdate,
    force: bool,
    ids: &dyn VolumeIdMapper,
) -> VolumeContentResult<(PreparedUpload, File)> {
    let path = NormalizedPath::parse_non_root(path)?;
    let root = open_root(root)?;
    let (parent, final_name) = open_parent(root, &path.components)?;
    match stat_at(&parent, final_name) {
        Ok(stat) if file_type(&stat) == VolumeEntryType::Directory => {
            return Err(VolumeContentError::Conflict)
        }
        Ok(_) if !force => return Err(VolumeContentError::Conflict),
        Ok(_) | Err(VolumeContentError::NotFound) => {}
        Err(error) => return Err(error),
    }

    let mode = validate_mode(metadata.mode, DEFAULT_FILE_MODE)?;
    let host_uid = ids.host_uid(metadata.uid.unwrap_or(0))?;
    let host_gid = ids.host_gid(metadata.gid.unwrap_or(0))?;
    let (temporary_name, file) = create_temporary_file(&parent)?;
    let stat = fstat(&file)?;
    let prepared = PreparedUpload {
        parent,
        temporary_name,
        final_name: final_name.clone(),
        temporary_device: stat.st_dev,
        temporary_inode: stat.st_ino,
        host_uid,
        host_gid,
        mode,
        force,
        armed: true,
    };
    Ok((prepared, File::from(file)))
}

pub fn finish_upload(mut prepared: PreparedUpload, file: File) -> VolumeContentResult<()> {
    let stat = fstat_file(&file)?;
    if stat.st_dev != prepared.temporary_device || stat.st_ino != prepared.temporary_inode {
        return Err(VolumeContentError::Conflict);
    }
    let linked = stat_at(&prepared.parent, &prepared.temporary_name)?;
    if linked.st_dev != prepared.temporary_device || linked.st_ino != prepared.temporary_inode {
        return Err(VolumeContentError::Conflict);
    }
    set_identity_and_mode(
        file.as_raw_fd(),
        prepared.host_uid,
        prepared.host_gid,
        prepared.mode,
    )?;
    file.sync_all()
        .map_err(|error| unavailable("sync uploaded file", error))?;
    rename_at(
        &prepared.parent,
        &prepared.temporary_name,
        &prepared.final_name,
        !prepared.force,
    )?;
    prepared.armed = false;
    sync_directory(&prepared.parent)?;
    Ok(())
}

struct NormalizedPath {
    components: Vec<CString>,
    display: String,
}

impl NormalizedPath {
    fn parse(value: &str) -> VolumeContentResult<Self> {
        if !value.starts_with('/') || value.len() > MAX_PATH_BYTES {
            return Err(VolumeContentError::InvalidPath(
                "path must be absolute and no longer than 4096 bytes".to_string(),
            ));
        }
        let mut components = Vec::new();
        let mut display_parts = Vec::new();
        for component in value.split('/') {
            if component.is_empty() {
                continue;
            }
            if component == "." || component == ".." {
                return Err(VolumeContentError::InvalidPath(
                    "path traversal components are forbidden".to_string(),
                ));
            }
            if component.starts_with(INTERNAL_UPLOAD_PREFIX) {
                return Err(VolumeContentError::InvalidPath(
                    "path uses a reserved volume component".to_string(),
                ));
            }
            if component.len() > MAX_COMPONENT_BYTES {
                return Err(VolumeContentError::InvalidPath(
                    "path component exceeds 255 bytes".to_string(),
                ));
            }
            components.push(CString::new(component).map_err(|_| {
                VolumeContentError::InvalidPath("path contains a NUL byte".to_string())
            })?);
            display_parts.push(component);
        }
        let display = if display_parts.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", display_parts.join("/"))
        };
        Ok(Self {
            components,
            display,
        })
    }

    fn parse_non_root(value: &str) -> VolumeContentResult<Self> {
        let path = Self::parse(value)?;
        if path.components.is_empty() {
            return Err(VolumeContentError::InvalidPath(
                "the volume root cannot be mutated".to_string(),
            ));
        }
        Ok(path)
    }

    fn name(&self) -> &str {
        self.display.rsplit('/').next().unwrap_or("/")
    }
}

fn open_root(path: &Path) -> VolumeContentResult<OwnedFd> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        VolumeContentError::Unavailable("volume root contains a NUL byte".to_string())
    })?;
    open_raw(
        libc::AT_FDCWD,
        &path,
        libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        0,
    )
}

fn open_parent(root: OwnedFd, components: &[CString]) -> VolumeContentResult<(OwnedFd, &CString)> {
    let (name, parents) = components
        .split_last()
        .ok_or_else(|| VolumeContentError::InvalidPath("path identifies the root".to_string()))?;
    let parent = open_directory_path(root, parents)?;
    Ok((parent, name))
}

fn open_directory_path(
    mut current: OwnedFd,
    components: &[CString],
) -> VolumeContentResult<OwnedFd> {
    for component in components {
        current = open_directory_at(&current, component)?;
    }
    Ok(current)
}

fn open_directory_at(parent: &OwnedFd, name: &CString) -> VolumeContentResult<OwnedFd> {
    open_at(
        parent,
        name,
        libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        0,
    )
}

fn open_at(
    parent: &OwnedFd,
    name: &CString,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> VolumeContentResult<OwnedFd> {
    open_raw(parent.as_raw_fd(), name, flags, mode)
}

fn open_raw(
    parent: libc::c_int,
    name: &CString,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> VolumeContentResult<OwnedFd> {
    let fd = unsafe { libc::openat(parent, name.as_ptr(), flags, mode) };
    if fd < 0 {
        return Err(io_error("open volume path", io::Error::last_os_error()));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn mkdir_at(parent: &OwnedFd, name: &CString, mode: u32) -> VolumeContentResult<()> {
    let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), mode as libc::mode_t) };
    cvt(result, "create volume directory").map(|_| ())
}

fn stat_at(parent: &OwnedFd, name: &CString) -> VolumeContentResult<libc::stat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    cvt(result, "stat volume path")?;
    Ok(unsafe { stat.assume_init() })
}

fn fstat(fd: &OwnedFd) -> VolumeContentResult<libc::stat> {
    fstat_raw(fd.as_raw_fd())
}

fn fstat_file(file: &File) -> VolumeContentResult<libc::stat> {
    fstat_raw(file.as_raw_fd())
}

fn fstat_raw(fd: libc::c_int) -> VolumeContentResult<libc::stat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe { libc::fstat(fd, stat.as_mut_ptr()) };
    cvt(result, "stat open volume path")?;
    Ok(unsafe { stat.assume_init() })
}

fn set_identity_and_mode(
    fd: libc::c_int,
    uid: u32,
    gid: u32,
    mode: u32,
) -> VolumeContentResult<()> {
    let result = unsafe { libc::fchown(fd, uid as libc::uid_t, gid as libc::gid_t) };
    cvt(result, "set volume ownership")?;
    let result = unsafe { libc::fchmod(fd, mode as libc::mode_t) };
    cvt(result, "set volume mode")?;
    Ok(())
}

fn validate_mode(value: Option<u32>, default: u32) -> VolumeContentResult<u32> {
    let value = value.unwrap_or(default);
    if value > 0o7777 {
        return Err(VolumeContentError::InvalidPath(
            "mode must contain only Unix permission and special bits".to_string(),
        ));
    }
    Ok(value)
}

fn list_directory(
    directory: &OwnedFd,
    base: &str,
    depth: u32,
    ids: &dyn VolumeIdMapper,
    output: &mut Vec<VolumeEntry>,
) -> VolumeContentResult<()> {
    for (name, display_name) in read_directory_names(directory)? {
        if display_name.starts_with(INTERNAL_UPLOAD_PREFIX) {
            continue;
        }
        let path = if base == "/" {
            format!("/{display_name}")
        } else {
            format!("{base}/{display_name}")
        };
        let stat = match stat_at(directory, &name) {
            Ok(stat) => stat,
            Err(VolumeContentError::NotFound) => continue,
            Err(error) => return Err(error),
        };
        let entry = entry_from_stat(directory, Some(&name), &display_name, &path, &stat, ids)?;
        let recurse = depth > 1 && entry.entry_type == VolumeEntryType::Directory;
        output.push(entry);
        if recurse {
            match open_directory_at(directory, &name) {
                Ok(child) => list_directory(&child, &path, depth - 1, ids, output)?,
                Err(VolumeContentError::NotFound | VolumeContentError::InvalidPath(_)) => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

fn read_directory_names(directory: &OwnedFd) -> VolumeContentResult<Vec<(CString, String)>> {
    let duplicate = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(io_error(
            "duplicate volume directory descriptor",
            io::Error::last_os_error(),
        ));
    }
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        unsafe { libc::close(duplicate) };
        return Err(io_error(
            "open volume directory stream",
            io::Error::last_os_error(),
        ));
    }
    let guard = DirectoryStream(stream);
    let mut names = Vec::new();
    loop {
        let entry = unsafe { libc::readdir(guard.0) };
        if entry.is_null() {
            break;
        }
        let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let name = CString::new(bytes).map_err(|_| {
            VolumeContentError::Unavailable("directory entry contains a NUL byte".to_string())
        })?;
        let display = OsString::from_vec(bytes.to_vec())
            .to_string_lossy()
            .into_owned();
        names.push((name, display));
    }
    names.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    Ok(names)
}

struct DirectoryStream(*mut libc::DIR);

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}

fn remove_entry(parent: &OwnedFd, name: &CString) -> VolumeContentResult<()> {
    let stat = stat_at(parent, name)?;
    if file_type(&stat) == VolumeEntryType::Directory {
        let directory = open_directory_at(parent, name)?;
        for (child, _) in read_directory_names(&directory)? {
            match remove_entry(&directory, &child) {
                Ok(()) | Err(VolumeContentError::NotFound) => {}
                Err(error) => return Err(error),
            }
        }
        let result =
            unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) };
        cvt(result, "remove volume directory")?;
    } else {
        let result = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) };
        cvt(result, "remove volume entry")?;
    }
    Ok(())
}

fn create_temporary_file(parent: &OwnedFd) -> VolumeContentResult<(CString, OwnedFd)> {
    for _ in 0..16 {
        let name = CString::new(format!(
            "{INTERNAL_UPLOAD_PREFIX}{}",
            Uuid::new_v4().simple()
        ))
        .map_err(|_| VolumeContentError::Unavailable("invalid upload name".to_string()))?;
        match open_at(
            parent,
            &name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        ) {
            Ok(file) => return Ok((name, file)),
            Err(VolumeContentError::Conflict) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(VolumeContentError::Unavailable(
        "could not allocate a unique upload file".to_string(),
    ))
}

fn rename_at(
    parent: &OwnedFd,
    source: &CString,
    destination: &CString,
    no_replace: bool,
) -> VolumeContentResult<()> {
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            parent.as_raw_fd(),
            source.as_ptr(),
            parent.as_raw_fd(),
            destination.as_ptr(),
            if no_replace {
                libc::RENAME_NOREPLACE
            } else {
                0
            },
        ) as libc::c_int
    };

    #[cfg(not(target_os = "linux"))]
    let result = {
        if no_replace && stat_at(parent, destination).is_ok() {
            return Err(VolumeContentError::Conflict);
        }
        unsafe {
            libc::renameat(
                parent.as_raw_fd(),
                source.as_ptr(),
                parent.as_raw_fd(),
                destination.as_ptr(),
            )
        }
    };

    cvt(result, "commit volume upload")?;
    Ok(())
}

fn sync_directory(directory: &OwnedFd) -> VolumeContentResult<()> {
    let result = unsafe { libc::fsync(directory.as_raw_fd()) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EINVAL) {
        return Ok(());
    }
    Err(io_error("sync volume directory", error))
}

fn entry_from_stat(
    parent: &OwnedFd,
    name: Option<&CString>,
    display_name: &str,
    path: &str,
    stat: &libc::stat,
    ids: &dyn VolumeIdMapper,
) -> VolumeContentResult<VolumeEntry> {
    let entry_type = file_type(stat);
    let target = if entry_type == VolumeEntryType::Symlink {
        name.map(|name| read_link(parent, name)).transpose()?
    } else {
        None
    };
    let (atime_seconds, atime_nanos) = atime(stat);
    let (mtime_seconds, mtime_nanos) = mtime(stat);
    let (ctime_seconds, ctime_nanos) = ctime(stat);
    Ok(VolumeEntry {
        name: display_name.to_string(),
        entry_type,
        path: path.to_string(),
        size: file_size(stat.st_size),
        mode: permission_mode(stat.st_mode),
        uid: ids.container_uid(stat.st_uid)?,
        gid: ids.container_gid(stat.st_gid)?,
        atime: timestamp(atime_seconds, atime_nanos)?,
        mtime: timestamp(mtime_seconds, mtime_nanos)?,
        ctime: timestamp(ctime_seconds, ctime_nanos)?,
        target,
    })
}

fn file_type(stat: &libc::stat) -> VolumeEntryType {
    match stat.st_mode & libc::S_IFMT {
        libc::S_IFREG => VolumeEntryType::File,
        libc::S_IFDIR => VolumeEntryType::Directory,
        libc::S_IFLNK => VolumeEntryType::Symlink,
        _ => VolumeEntryType::Unknown,
    }
}

fn read_link(parent: &OwnedFd, name: &CString) -> VolumeContentResult<String> {
    let mut capacity = 256;
    loop {
        let mut buffer = vec![0_u8; capacity];
        let length = unsafe {
            libc::readlinkat(
                parent.as_raw_fd(),
                name.as_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        };
        if length < 0 {
            return Err(io_error("read volume symlink", io::Error::last_os_error()));
        }
        let length = length as usize;
        if length < buffer.len() {
            buffer.truncate(length);
            return Ok(OsString::from_vec(buffer).to_string_lossy().into_owned());
        }
        capacity = capacity.checked_mul(2).ok_or_else(|| {
            VolumeContentError::Unavailable("volume symlink target is too large".to_string())
        })?;
        if capacity > 64 * 1024 {
            return Err(VolumeContentError::Unavailable(
                "volume symlink target is too large".to_string(),
            ));
        }
    }
}

fn timestamp(seconds: i64, nanos: i64) -> VolumeContentResult<DateTime<Utc>> {
    let nanos = u32::try_from(nanos).map_err(|_| {
        VolumeContentError::Unavailable("filesystem timestamp is invalid".to_string())
    })?;
    DateTime::from_timestamp(seconds, nanos).ok_or_else(|| {
        VolumeContentError::Unavailable("filesystem timestamp is out of range".to_string())
    })
}

// libc scalar aliases vary across Unix targets even when the protocol's wire
// representation does not. Normalize them once at the ABI boundary rather
// than spreading target-dependent casts through filesystem logic.
#[allow(clippy::unnecessary_cast)]
fn file_size(size: libc::off_t) -> i64 {
    size as i64
}

#[allow(clippy::unnecessary_cast)]
fn permission_mode(mode: libc::mode_t) -> u32 {
    (mode as u32) & 0o7777
}

#[allow(clippy::unnecessary_cast)]
fn timestamp_parts(seconds: libc::time_t, nanos: libc::c_long) -> (i64, i64) {
    (seconds as i64, nanos as i64)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn atime(stat: &libc::stat) -> (i64, i64) {
    timestamp_parts(stat.st_atime, stat.st_atime_nsec)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn mtime(stat: &libc::stat) -> (i64, i64) {
    timestamp_parts(stat.st_mtime, stat.st_mtime_nsec)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn ctime(stat: &libc::stat) -> (i64, i64) {
    timestamp_parts(stat.st_ctime, stat.st_ctime_nsec)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn atime(stat: &libc::stat) -> (i64, i64) {
    timestamp_parts(stat.st_atimespec.tv_sec, stat.st_atimespec.tv_nsec)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn mtime(stat: &libc::stat) -> (i64, i64) {
    timestamp_parts(stat.st_mtimespec.tv_sec, stat.st_mtimespec.tv_nsec)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn ctime(stat: &libc::stat) -> (i64, i64) {
    timestamp_parts(stat.st_ctimespec.tv_sec, stat.st_ctimespec.tv_nsec)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
)))]
fn atime(stat: &libc::stat) -> (i64, i64) {
    (stat.st_atime as i64, 0)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
)))]
fn mtime(stat: &libc::stat) -> (i64, i64) {
    (stat.st_mtime as i64, 0)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
)))]
fn ctime(stat: &libc::stat) -> (i64, i64) {
    (stat.st_ctime as i64, 0)
}

fn cvt(result: libc::c_int, operation: &str) -> VolumeContentResult<libc::c_int> {
    if result >= 0 {
        Ok(result)
    } else {
        Err(io_error(operation, io::Error::last_os_error()))
    }
}

fn io_error(operation: &str, error: io::Error) -> VolumeContentError {
    match error.raw_os_error() {
        Some(libc::ENOENT) => VolumeContentError::NotFound,
        Some(libc::EEXIST) | Some(libc::ENOTEMPTY) | Some(libc::EBUSY) => {
            VolumeContentError::Conflict
        }
        Some(libc::EACCES) | Some(libc::EPERM) => VolumeContentError::PermissionDenied,
        Some(libc::ELOOP) | Some(libc::ENOTDIR) | Some(libc::EINVAL) | Some(libc::ENAMETOOLONG) => {
            VolumeContentError::InvalidPath(format!("{operation}: {error}"))
        }
        _ => unavailable(operation, error),
    }
}

fn unavailable(operation: &str, error: impl std::fmt::Display) -> VolumeContentError {
    VolumeContentError::Unavailable(format!("{operation}: {error}"))
}
