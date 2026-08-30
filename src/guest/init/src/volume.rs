//! OCI-compatible named-volume initialization inside the guest.

use std::path::Path;

struct VolumeInitializationLock(std::fs::File);

impl VolumeInitializationLock {
    fn acquire(directory: &Path) -> std::io::Result<Self> {
        use std::os::fd::AsRawFd;

        let file = std::fs::File::open(directory)?;
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(Self(file));
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

impl Drop for VolumeInitializationLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;

        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Populate an empty named volume from the directory hidden by its mount.
///
/// Docker copies an image directory's existing contents and metadata into a
/// newly created volume before starting the container. The MicroVM runtime has
/// to perform that copy in the guest because Linux uid/gid metadata for a
/// macOS virtio-fs share is represented by the guest filesystem protocol.
/// Returns `true` when initialization occurred and `false` for an existing,
/// non-empty volume.
pub fn initialize_named_volume(
    source: &Path,
    destination: &Path,
) -> Result<bool, Box<dyn std::error::Error>> {
    // Multiple boxes may mount the same newly created volume concurrently.
    // Serialize the empty check and copy on the shared directory inode so only
    // one image can seed it; no runtime-private marker is exposed to workloads.
    let _initialization_lock = VolumeInitializationLock::acquire(destination)?;
    if std::fs::read_dir(destination)?
        .next()
        .transpose()?
        .is_some()
    {
        return Ok(false);
    }

    // A valid container mount target can be a directory symlink in the image
    // (for example `/var/run -> /run`). Seed from the resolved directory just
    // as mount(2) resolves the final target.
    let source_metadata = std::fs::metadata(source)?;
    let destination_metadata = std::fs::symlink_metadata(destination)?;
    if !source_metadata.file_type().is_dir() {
        return Err(format!(
            "named-volume seed source is not a directory: {}",
            source.display()
        )
        .into());
    }

    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        copy_directory_contents(source, destination)?;
        apply_root_metadata(destination, &source_metadata)?;
        Ok(())
    })();

    if let Err(error) = result {
        if let Err(rollback_error) = rollback_initialization(destination, &destination_metadata) {
            return Err(format!(
                "named-volume initialization failed: {error}; rollback failed: {rollback_error}"
            )
            .into());
        }
        return Err(error);
    }
    Ok(true)
}

/// Stream a tar archive directly from the image directory into the volume.
///
/// A filesystem temporary can be located below the very directory being
/// copied (a valid volume target is `/` or `/run`) and an unbounded in-memory
/// archive scales with the image data. A socket pair keeps the copy bounded by
/// kernel backpressure and does not introduce any path visible to the image or
/// workload.
fn copy_directory_contents(
    source: &Path,
    destination: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::net::UnixStream;

    let (reader, writer) = UnixStream::pair()?;
    let source = source.to_path_buf();
    let producer = std::thread::Builder::new()
        .name("a3s-volume-copy".to_string())
        .spawn(move || -> Result<(), String> {
            let mut builder = tar::Builder::new(writer);
            builder.follow_symlinks(false);
            builder
                .append_dir_all(Path::new(""), &source)
                .and_then(|()| builder.finish())
                .map_err(|error| error.to_string())
        })?;

    // Scope the reader so an unpack failure closes it before joining a writer
    // that may be blocked by socket backpressure.
    let unpack_result = {
        let mut archive = tar::Archive::new(reader);
        archive.set_preserve_permissions(true);
        archive.set_preserve_ownerships(true);
        archive.set_preserve_mtime(true);
        archive
            .unpack(destination)
            .map_err(|error| error.to_string())
    };
    let produce_result = producer
        .join()
        .map_err(|_| "named-volume archive producer panicked".to_string())?;

    match (produce_result, unpack_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(produce), Ok(())) => Err(produce.into()),
        (Ok(()), Err(unpack)) => Err(unpack.into()),
        (Err(produce), Err(unpack)) => Err(format!(
            "named-volume archive failed while producing ({produce}) and unpacking ({unpack})"
        )
        .into()),
    }
}

fn rollback_initialization(
    destination: &Path,
    original_metadata: &std::fs::Metadata,
) -> Result<(), Box<dyn std::error::Error>> {
    clear_directory(destination)?;
    apply_root_metadata(destination, original_metadata)
}

fn apply_root_metadata(
    destination: &Path,
    source_metadata: &std::fs::Metadata,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let path = std::ffi::CString::new(destination.as_os_str().as_bytes())?;
    if unsafe { libc::lchown(path.as_ptr(), source_metadata.uid(), source_metadata.gid()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    std::fs::set_permissions(
        destination,
        std::fs::Permissions::from_mode(source_metadata.mode() & 0o7777),
    )?;
    Ok(())
}

fn clear_directory(path: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let entry_path = entry.path();
        match std::fs::symlink_metadata(&entry_path) {
            Ok(metadata) if metadata.file_type().is_dir() => {
                std::fs::remove_dir_all(entry_path)?;
            }
            Ok(_) => {
                std::fs::remove_file(entry_path)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_image_seed_only_when_the_volume_is_empty() {
        #[cfg(unix)]
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        std::fs::create_dir_all(source.join("nested")).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(source.join("nested/config.txt"), b"from-image").unwrap();
        std::fs::write(
            source.join(".a3s-volume-seed-0.tar"),
            b"ordinary image data",
        )
        .unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("nested/config.txt", source.join("config-link")).unwrap();
            std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o710)).unwrap();
            std::fs::set_permissions(
                source.join("nested"),
                std::fs::Permissions::from_mode(0o550),
            )
            .unwrap();
            std::fs::set_permissions(
                source.join("nested/config.txt"),
                std::fs::Permissions::from_mode(0o640),
            )
            .unwrap();
        }

        assert!(initialize_named_volume(&source, &destination).unwrap());
        assert_eq!(
            std::fs::read(destination.join("nested/config.txt")).unwrap(),
            b"from-image"
        );
        #[cfg(unix)]
        assert_eq!(
            std::fs::read_link(destination.join("config-link")).unwrap(),
            std::path::PathBuf::from("nested/config.txt")
        );
        assert_eq!(
            std::fs::read(destination.join(".a3s-volume-seed-0.tar")).unwrap(),
            b"ordinary image data"
        );
        #[cfg(unix)]
        {
            let source_root = std::fs::metadata(&source).unwrap();
            let destination_root = std::fs::metadata(&destination).unwrap();
            assert_eq!(destination_root.mode() & 0o7777, 0o710);
            assert_eq!(destination_root.uid(), source_root.uid());
            assert_eq!(destination_root.gid(), source_root.gid());
            assert_eq!(
                std::fs::metadata(destination.join("nested"))
                    .unwrap()
                    .mode()
                    & 0o7777,
                0o550
            );
            assert_eq!(
                std::fs::metadata(destination.join("nested/config.txt"))
                    .unwrap()
                    .mode()
                    & 0o7777,
                0o640
            );
        }

        std::fs::write(source.join("later.txt"), b"must-not-copy").unwrap();
        assert!(!initialize_named_volume(&source, &destination).unwrap());
        assert!(!destination.join("later.txt").exists());
        #[cfg(unix)]
        {
            std::fs::set_permissions(
                source.join("nested"),
                std::fs::Permissions::from_mode(0o750),
            )
            .unwrap();
            std::fs::set_permissions(
                destination.join("nested"),
                std::fs::Permissions::from_mode(0o750),
            )
            .unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn failed_seed_copy_rolls_back_partial_contents_and_root_mode() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o711)).unwrap();
        std::fs::write(source.join("possible-partial.txt"), b"partial").unwrap();
        let _socket =
            std::os::unix::net::UnixListener::bind(source.join("unsupported.sock")).unwrap();

        let error = initialize_named_volume(&source, &destination).unwrap_err();

        assert!(error.to_string().contains("socket can not be archived"));
        assert!(std::fs::read_dir(&destination).unwrap().next().is_none());
        assert_eq!(
            std::fs::metadata(&destination)
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o711
        );
    }

    #[cfg(unix)]
    #[test]
    fn serializes_the_empty_check_and_copy_for_a_shared_volume() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(source.join("seed.txt"), b"seed").unwrap();

        let held_lock = VolumeInitializationLock::acquire(&destination).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let source_for_thread = source.clone();
        let destination_for_thread = destination.clone();
        let worker = std::thread::spawn(move || {
            let result = initialize_named_volume(&source_for_thread, &destination_for_thread);
            sender
                .send(result.map_err(|error| error.to_string()))
                .unwrap();
        });

        assert!(
            receiver
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "initialization must wait while another box owns the volume lock"
        );
        drop(held_lock);

        assert!(receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            .unwrap());
        worker.join().unwrap();
        assert_eq!(
            std::fs::read(destination.join("seed.txt")).unwrap(),
            b"seed"
        );
    }
}
