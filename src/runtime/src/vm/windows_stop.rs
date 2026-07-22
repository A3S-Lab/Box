use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use a3s_box_core::exec::{WINDOWS_STOP_REQUEST_FILE, WINDOWS_STOP_REQUEST_TEMP_FILE};

pub(crate) fn request_path(socket_dir: &Path) -> PathBuf {
    socket_dir.join(WINDOWS_STOP_REQUEST_FILE)
}

fn temporary_path(socket_dir: &Path) -> PathBuf {
    socket_dir.join(WINDOWS_STOP_REQUEST_TEMP_FILE)
}

fn remove_control_file(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Windows stop control path is a directory: {}",
                    path.display()
                ),
            ))
        }
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub(crate) fn clear(socket_dir: &Path) -> io::Result<()> {
    remove_control_file(&request_path(socket_dir))?;
    remove_control_file(&temporary_path(socket_dir))
}

pub(crate) fn stage(socket_dir: &Path, signal: i32) -> io::Result<PathBuf> {
    if !(1..=64).contains(&signal) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Windows stop signal must be between 1 and 64, got {signal}"),
        ));
    }

    clear(socket_dir)?;
    let request = request_path(socket_dir);
    let temporary = temporary_path(socket_dir);
    if let Err(error) =
        a3s_box_core::fs_atomic::write_durable(&temporary, &request, signal.to_string().as_bytes())
    {
        let _ = remove_control_file(&temporary);
        return Err(error);
    }
    Ok(request)
}

/// Wait until the forwarding worker removes a staged request after writing it
/// to the connected guest control channel.
pub(crate) async fn wait_until_delivered(request: &Path, timeout: Duration) -> io::Result<bool> {
    const POLL_INTERVAL: Duration = Duration::from_millis(10);

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::fs::symlink_metadata(request).await {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Windows stop control path is a directory: {}",
                        request.display()
                    ),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(error),
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        tokio::time::sleep(POLL_INTERVAL.min(remaining)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_publishes_exact_signal_and_clear_removes_it() {
        let directory = tempfile::tempdir().unwrap();

        let request = stage(directory.path(), 15).unwrap();

        assert_eq!(std::fs::read_to_string(&request).unwrap(), "15");
        assert!(!temporary_path(directory.path()).exists());

        clear(directory.path()).unwrap();
        assert!(!request.exists());
    }

    #[test]
    fn stage_replaces_a_stale_request() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(request_path(directory.path()), "2").unwrap();

        let request = stage(directory.path(), 9).unwrap();

        assert_eq!(std::fs::read_to_string(request).unwrap(), "9");
    }

    #[test]
    fn stage_rejects_invalid_signals_without_creating_a_request() {
        let directory = tempfile::tempdir().unwrap();

        let error = stage(directory.path(), 0).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!request_path(directory.path()).exists());
    }

    #[test]
    fn clear_refuses_to_recursively_remove_a_control_directory() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(request_path(directory.path())).unwrap();

        let error = clear(directory.path()).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(request_path(directory.path()).is_dir());
    }

    #[tokio::test]
    async fn delivery_wait_observes_worker_removal() {
        let directory = tempfile::tempdir().unwrap();
        let request = stage(directory.path(), 15).unwrap();
        let worker_request = request.clone();
        let worker = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            tokio::fs::remove_file(worker_request).await.unwrap();
        });

        assert!(wait_until_delivered(&request, Duration::from_secs(1))
            .await
            .unwrap());
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn delivery_wait_times_out_while_request_remains() {
        let directory = tempfile::tempdir().unwrap();
        let request = stage(directory.path(), 15).unwrap();

        assert!(!wait_until_delivered(&request, Duration::from_millis(20))
            .await
            .unwrap());
        assert!(request.exists());
    }
}
