//! One-way terminal-status handoff from guest-init to the host runtime.
//!
//! Guest-init opens the host-backed status file and immediately unmounts its
//! private virtio-fs share before any workload process starts. The retained
//! close-on-exec descriptor lets PID 1 publish the final exit code without
//! exposing a writable host path to the workload or placing lifecycle state in
//! the guest root filesystem.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use a3s_box_core::guest_exec::{GuestTerminalStatus, MAX_GUEST_TERMINAL_STATUS_BYTES};

static TERMINAL_STATUS_FILE: OnceLock<Mutex<File>> = OnceLock::new();

/// Open and retain the pre-created terminal status file.
#[cfg(target_os = "linux")]
pub fn acquire(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::OpenOptionsExt;

    let path_metadata = std::fs::symlink_metadata(path)?;
    if !path_metadata.is_file() || path_metadata.file_type().is_symlink() {
        return Err(format!(
            "guest terminal status path is not a plain file: {}",
            path.display()
        )
        .into());
    }
    if path_metadata.len() > MAX_GUEST_TERMINAL_STATUS_BYTES as u64 {
        return Err(format!(
            "guest terminal status file {} exceeds {} bytes",
            path.display(),
            MAX_GUEST_TERMINAL_STATUS_BYTES
        )
        .into());
    }

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    let opened_metadata = file.metadata()?;
    if !opened_metadata.is_file() {
        return Err(format!(
            "opened guest terminal status path is not a file: {}",
            path.display()
        )
        .into());
    }
    file.set_len(0)?;

    TERMINAL_STATUS_FILE
        .set(Mutex::new(file))
        .map_err(|_| "guest terminal status file was acquired more than once".into())
}

#[cfg(not(target_os = "linux"))]
pub fn acquire(_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    Ok(())
}

/// Persist the final workload exit code if a MicroVM terminal channel exists.
///
/// Returns `Ok(false)` for host-sandbox and legacy boot paths that did not
/// acquire the private channel.
pub fn persist(exit_code: i32) -> Result<bool, Box<dyn std::error::Error>> {
    let Some(file) = TERMINAL_STATUS_FILE.get() else {
        return Ok(false);
    };
    let status = GuestTerminalStatus::new(exit_code);
    status.validate()?;
    let bytes = serde_json::to_vec(&status)?;
    if bytes.len() > MAX_GUEST_TERMINAL_STATUS_BYTES {
        return Err(format!(
            "guest terminal status is {} bytes; limit is {} bytes",
            bytes.len(),
            MAX_GUEST_TERMINAL_STATUS_BYTES
        )
        .into());
    }

    let mut file = file
        .lock()
        .map_err(|_| "guest terminal status file lock is poisoned")?;
    write_status(&mut file, &bytes)?;
    Ok(true)
}

/// Publish the final guest-owned block-root handoff acknowledgement.
///
/// This must run only after the root filesystem has been remounted read-only.
/// The status descriptor points at the already-unmounted private host channel,
/// so updating it cannot make the guest root writable again.
pub fn mark_rootfs_quiesced() -> Result<bool, Box<dyn std::error::Error>> {
    let Some(file) = TERMINAL_STATUS_FILE.get() else {
        return Ok(false);
    };
    let mut file = file
        .lock()
        .map_err(|_| "guest terminal status file lock is poisoned")?;
    mark_status_rootfs_quiesced(&mut file)?;
    Ok(true)
}

fn mark_status_rootfs_quiesced(file: &mut File) -> Result<(), Box<dyn std::error::Error>> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(MAX_GUEST_TERMINAL_STATUS_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.is_empty() || bytes.len() > MAX_GUEST_TERMINAL_STATUS_BYTES {
        return Err("guest terminal status is absent or exceeds its size limit".into());
    }
    let status = serde_json::from_slice::<GuestTerminalStatus>(&bytes)?;
    status.validate()?;
    let bytes = serde_json::to_vec(&status.with_rootfs_quiesced())?;
    if bytes.len() > MAX_GUEST_TERMINAL_STATUS_BYTES {
        return Err(format!(
            "guest terminal status is {} bytes; limit is {} bytes",
            bytes.len(),
            MAX_GUEST_TERMINAL_STATUS_BYTES
        )
        .into());
    }
    write_status(file, &bytes)?;
    Ok(())
}

fn write_status(file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn terminal_status_write_truncates_and_syncs_versioned_payload() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&[b'x'; MAX_GUEST_TERMINAL_STATUS_BYTES])
            .unwrap();
        let payload = serde_json::to_vec(&GuestTerminalStatus::new(23)).unwrap();

        write_status(&mut file, &payload).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut persisted = Vec::new();
        file.read_to_end(&mut persisted).unwrap();

        assert_eq!(persisted, payload);
    }

    #[test]
    fn rootfs_quiescence_ack_preserves_the_terminal_exit_code() {
        let mut file = tempfile::tempfile().unwrap();
        let payload = serde_json::to_vec(&GuestTerminalStatus::new(137)).unwrap();
        write_status(&mut file, &payload).unwrap();

        mark_status_rootfs_quiesced(&mut file).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut persisted = Vec::new();
        file.read_to_end(&mut persisted).unwrap();
        let status = serde_json::from_slice::<GuestTerminalStatus>(&persisted).unwrap();

        assert_eq!(status.exit_code, 137);
        assert!(status.rootfs_quiesced);
    }
}
