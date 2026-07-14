//! Validation and ownership transfer for host-provided control listeners.

#[cfg(target_os = "linux")]
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

/// Adopt an inherited, already-bound Unix stream listener.
///
/// Validation happens before ownership transfer so an invalid descriptor is
/// never accidentally closed by this function. The adopted descriptor is set
/// `CLOEXEC` before guest-init forks the workload.
#[cfg(target_os = "linux")]
pub(crate) fn adopt_unix_listener(fd: RawFd, label: &str) -> std::io::Result<OwnedFd> {
    if fd < 3 {
        return Err(invalid_listener(label, "descriptor must be at least 3"));
    }

    let descriptor_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if descriptor_flags < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let socket_type = get_socket_option(fd, libc::SO_TYPE)?;
    if socket_type != libc::SOCK_STREAM {
        return Err(invalid_listener(label, "descriptor is not a stream socket"));
    }
    let accepting = get_socket_option(fd, libc::SO_ACCEPTCONN)?;
    if accepting != 1 {
        return Err(invalid_listener(label, "socket is not listening"));
    }

    let mut address: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockname(
            fd,
            &mut address as *mut libc::sockaddr_storage as *mut libc::sockaddr,
            &mut length,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if address.ss_family as i32 != libc::AF_UNIX {
        return Err(invalid_listener(label, "listener is not an AF_UNIX socket"));
    }

    if unsafe { libc::fcntl(fd, libc::F_SETFD, descriptor_flags | libc::FD_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    // SAFETY: every check above succeeded and the caller transfers exclusive
    // ownership of the inherited descriptor to this function.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn get_socket_option(fd: RawFd, option: libc::c_int) -> std::io::Result<libc::c_int> {
    let mut value = 0;
    let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            &mut value as *mut libc::c_int as *mut libc::c_void,
            &mut length,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(value)
}

#[cfg(target_os = "linux")]
fn invalid_listener(label: &str, reason: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("invalid inherited {label} listener: {reason}"),
    )
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::fd::{AsRawFd, IntoRawFd};

    #[test]
    fn adopts_listening_unix_stream_and_sets_cloexec() {
        let directory = tempfile::tempdir().unwrap();
        let listener =
            std::os::unix::net::UnixListener::bind(directory.path().join("control.sock")).unwrap();
        let inherited_fd = listener.into_raw_fd();

        let owned = adopt_unix_listener(inherited_fd, "test").unwrap();
        let flags = unsafe { libc::fcntl(owned.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
    }

    #[test]
    fn rejects_non_listening_socket_without_taking_ownership() {
        let mut descriptors = [0; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_STREAM,
                    0,
                    descriptors.as_mut_ptr(),
                )
            },
            0
        );
        let error = adopt_unix_listener(descriptors[0], "test").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert_ne!(unsafe { libc::fcntl(descriptors[0], libc::F_GETFD) }, -1);
        unsafe {
            libc::close(descriptors[0]);
            libc::close(descriptors[1]);
        }
    }
}
