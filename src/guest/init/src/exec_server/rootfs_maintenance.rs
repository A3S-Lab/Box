//! Restricted control protocol for the read-only rootfs maintenance guest.

use super::ExecListener;

#[cfg(target_os = "linux")]
use std::path::Path;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicU8, Ordering};
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use a3s_transport::FrameType;

#[cfg(target_os = "linux")]
const ROOTFS_MAINTENANCE_IDLE: u8 = 0;
#[cfg(target_os = "linux")]
const ROOTFS_MAINTENANCE_ARCHIVING: u8 = 1;
#[cfg(target_os = "linux")]
const ROOTFS_MAINTENANCE_SHUTTING_DOWN: u8 = 2;
#[cfg(target_os = "linux")]
static ROOTFS_MAINTENANCE_STATE: AtomicU8 = AtomicU8::new(ROOTFS_MAINTENANCE_IDLE);

#[cfg(target_os = "linux")]
const EXEC_CONTROL_ARCHIVE_ROOTFS: &[u8] = b"archive-rootfs-v1";
/// Host→trusted-maintenance-PID1 request to unmount its read-only disk and exit.
#[cfg(target_os = "linux")]
const EXEC_CONTROL_SHUTDOWN_MAINTENANCE: &[u8] = b"shutdown-rootfs-maintenance-v1";
/// Acknowledgement written before the maintenance shutdown flag is published.
#[cfg(target_os = "linux")]
const EXEC_SHUTDOWN_MAINTENANCE_ACK: &[u8] = b"shutdown-rootfs-maintenance-v1-ack";

#[cfg(target_os = "linux")]
pub fn begin_rootfs_maintenance_idle_shutdown() -> bool {
    ROOTFS_MAINTENANCE_STATE
        .compare_exchange(
            ROOTFS_MAINTENANCE_IDLE,
            ROOTFS_MAINTENANCE_SHUTTING_DOWN,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_ok()
}

#[cfg(target_os = "linux")]
struct RootfsMaintenanceArchiveGuard;

#[cfg(target_os = "linux")]
impl RootfsMaintenanceArchiveGuard {
    fn acquire() -> Option<Self> {
        ROOTFS_MAINTENANCE_STATE
            .compare_exchange(
                ROOTFS_MAINTENANCE_IDLE,
                ROOTFS_MAINTENANCE_ARCHIVING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
            .then_some(Self)
    }
}

#[cfg(target_os = "linux")]
impl Drop for RootfsMaintenanceArchiveGuard {
    fn drop(&mut self) {
        let _ = ROOTFS_MAINTENANCE_STATE.compare_exchange(
            ROOTFS_MAINTENANCE_ARCHIVING,
            ROOTFS_MAINTENANCE_IDLE,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }
}

/// Serve the deliberately narrow protocol exposed by the trusted rootfs
/// maintenance guest. It accepts only heartbeat, rootfs archive, and clean
/// shutdown; arbitrary exec, PTY, file mutation, and spawn-main are absent.
pub fn serve_rootfs_maintenance_server(
    listener: ExecListener,
    request_shutdown: fn(i32),
) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "linux")]
    {
        run_accept_loop(listener.0, request_shutdown)
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (listener, request_shutdown);
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn run_accept_loop(
    sock_fd: std::os::fd::OwnedFd,
    request_shutdown: fn(i32),
) -> Result<(), Box<dyn std::error::Error>> {
    use nix::sys::socket::accept;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use tracing::{error, warn};

    loop {
        match accept(sock_fd.as_raw_fd()) {
            Ok(client_fd) => {
                // SAFETY: `accept` returned a new descriptor owned by this
                // connection handler.
                let client = unsafe { OwnedFd::from_raw_fd(client_fd) };
                std::thread::spawn(move || {
                    if let Err(error) = handle_connection(client, request_shutdown) {
                        warn!(%error, "Failed to handle rootfs maintenance connection");
                    }
                });
            }
            Err(error) => {
                error!(%error, "Rootfs maintenance accept failed");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn handle_connection(
    fd: std::os::fd::OwnedFd,
    request_shutdown: fn(i32),
) -> Result<(), Box<dyn std::error::Error>> {
    let mut stream = std::fs::File::from(fd);
    let Some((frame_type, payload)) = super::read_frame(&mut stream)? else {
        return Ok(());
    };

    if frame_type == FrameType::Heartbeat as u8 {
        super::write_frame(&mut stream, FrameType::Heartbeat as u8, &payload)?;
        return Ok(());
    }
    if frame_type == FrameType::Control as u8 && payload == EXEC_CONTROL_ARCHIVE_ROOTFS {
        let Some(_archive_guard) = RootfsMaintenanceArchiveGuard::acquire() else {
            super::send_error_frame(
                &mut stream,
                "Rootfs maintenance archive is busy or shutting down",
            )?;
            return Ok(());
        };
        let root = Path::new(a3s_box_core::guest_exec::GUEST_ROOTFS_MAINTENANCE_MOUNT_PATH);
        if let Err(error) = super::stream_rootfs_archive_from(&mut stream, root, false) {
            super::send_error_frame(
                &mut stream,
                &format!("rootfs maintenance archive failed: {error}"),
            )?;
        }
        return Ok(());
    }
    if frame_type == FrameType::Control as u8 && payload == EXEC_CONTROL_SHUTDOWN_MAINTENANCE {
        match ROOTFS_MAINTENANCE_STATE.compare_exchange(
            ROOTFS_MAINTENANCE_IDLE,
            ROOTFS_MAINTENANCE_SHUTTING_DOWN,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) | Err(ROOTFS_MAINTENANCE_SHUTTING_DOWN) => {}
            Err(ROOTFS_MAINTENANCE_ARCHIVING) => {
                super::send_error_frame(&mut stream, "Rootfs maintenance archive is still active")?;
                return Ok(());
            }
            Err(_) => {
                super::send_error_frame(&mut stream, "Rootfs maintenance state is invalid")?;
                return Ok(());
            }
        }
        super::write_frame(
            &mut stream,
            FrameType::Control as u8,
            EXEC_SHUTDOWN_MAINTENANCE_ACK,
        )?;
        request_shutdown(libc::SIGTERM);
        return Ok(());
    }

    super::send_error_frame(
        &mut stream,
        "Rootfs maintenance accepts only heartbeat, archive, and shutdown",
    )?;
    Ok(())
}
