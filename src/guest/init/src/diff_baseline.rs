//! Guest-owned pristine rootfs baseline handoff.
//!
//! The runtime pre-creates a private control file only for providers that have
//! no host-visible rootfs after ownership handoff. Guest-init opens that file,
//! the share is detached, and the baseline is captured before workload launch.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

#[cfg(test)]
use a3s_box_core::rootfs_baseline::GuestDiffBaseline;
use a3s_box_core::rootfs_baseline::MAX_GUEST_DIFF_BASELINE_BYTES;

static DIFF_BASELINE_FILE: OnceLock<Mutex<File>> = OnceLock::new();

/// Retain the optional pre-created baseline file before its share is detached.
#[cfg(target_os = "linux")]
pub fn acquire_if_present(path: &Path) -> Result<bool, Box<dyn std::error::Error>> {
    use std::os::unix::fs::OpenOptionsExt;

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_GUEST_DIFF_BASELINE_BYTES as u64
    {
        return Err(format!(
            "guest diff baseline path is not a bounded plain file: {}",
            path.display()
        )
        .into());
    }

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(format!(
            "opened guest diff baseline path is not a file: {}",
            path.display()
        )
        .into());
    }
    file.set_len(0)?;
    DIFF_BASELINE_FILE
        .set(Mutex::new(file))
        .map_err(|_| "guest diff baseline file was acquired more than once")?;
    Ok(true)
}

#[cfg(not(target_os = "linux"))]
pub fn acquire_if_present(_path: &Path) -> Result<bool, Box<dyn std::error::Error>> {
    Ok(false)
}

/// Capture and persist the baseline when the runtime requested guest ownership.
pub fn persist(root: &Path) -> Result<bool, Box<dyn std::error::Error>> {
    let Some(file) = DIFF_BASELINE_FILE.get() else {
        return Ok(false);
    };
    let baseline = crate::rootfs_archive::snapshot_diff_baseline(root)?;
    baseline.validate()?;
    let bytes = serde_json::to_vec(&baseline)?;
    if bytes.len() > MAX_GUEST_DIFF_BASELINE_BYTES {
        return Err(format!(
            "guest diff baseline is {} bytes; limit is {} bytes",
            bytes.len(),
            MAX_GUEST_DIFF_BASELINE_BYTES
        )
        .into());
    }

    let mut file = file
        .lock()
        .map_err(|_| "guest diff baseline file lock is poisoned")?;
    write_baseline(&mut file, &bytes)?;
    Ok(true)
}

fn write_baseline(file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::rootfs_baseline::RootfsFileInfo;
    use std::collections::BTreeMap;
    use std::io::Read;

    #[test]
    fn baseline_write_truncates_to_one_versioned_payload() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"stale baseline bytes").unwrap();
        let baseline = GuestDiffBaseline::new(BTreeMap::from([(
            "/bin/tool".to_string(),
            RootfsFileInfo {
                size: 4,
                mode: 0o100755,
                is_dir: false,
            },
        )]));
        let payload = serde_json::to_vec(&baseline).unwrap();

        write_baseline(&mut file, &payload).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut persisted = Vec::new();
        file.read_to_end(&mut persisted).unwrap();

        assert_eq!(persisted, payload);
    }
}
