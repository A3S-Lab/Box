//! Re-openable workload stdio pipes and console relay lifecycle.

use super::*;

/// Relay threads forwarding the main process's stdout/stderr pipes to the console.
/// Drained at container exit so the tail of the output reaches the console (and
/// thus `logs` / the foreground terminal) before the VM halts.
static STDIO_RELAYS: std::sync::OnceLock<std::sync::Mutex<Vec<std::thread::JoinHandle<()>>>> =
    std::sync::OnceLock::new();

/// Interpose a re-openable pipe between the main container process's stdout/stderr
/// and the virtio-console.
///
/// The container would otherwise inherit guest-init's virtio-console ports as fd
/// 1/2, which are single-open: a process that re-opens `/proc/self/fd/{1,2}` or
/// `/dev/stdout`/`/dev/stderr` (Apache httpd, nginx-to-stdout, and many real apps)
/// gets `EBUSY`. We hand it pipe write-ends instead (installed onto fd 1/2 in the
/// child by `spawn_isolated`) and relay the read-ends back to the console here, so
/// re-opening works while `logs` and the split stdout/stderr streams are preserved.
///
/// File descriptors for the main-process stdio relay (set up before the fork,
/// with the relay threads started only *after* the fork — see `start_stdio_relays`).
#[cfg(target_os = "linux")]
pub(super) struct StdioRelayFds {
    /// Pipe write-ends handed to the child as fd 1/2.
    pub(super) out_w: std::os::unix::io::RawFd,
    pub(super) err_w: std::os::unix::io::RawFd,
    /// Pipe read-ends the relay threads drain.
    pub(super) out_r: std::os::unix::io::RawFd,
    pub(super) err_r: std::os::unix::io::RawFd,
    /// Console targets (dups of guest-init fd 1/2) the relays write to.
    pub(super) console_out: std::os::unix::io::RawFd,
    pub(super) console_err: std::os::unix::io::RawFd,
}

/// Create the relay pipes + console dups (NO threads yet — threads must start after
/// the container fork to stay fork-safe). Returns `None` (keep console fds, the
/// pre-fix behavior) if any fd op fails.
#[cfg(target_os = "linux")]
pub(super) fn setup_main_stdio_pipes() -> Option<StdioRelayFds> {
    use std::os::unix::io::RawFd;

    // Relay targets: dup guest-init's current stdout (fd 1 -> console.log) and
    // stderr (fd 2 -> console.err.log) so the split-stream routing is preserved.
    let console_out = unsafe { libc::dup(1) };
    let console_err = unsafe { libc::dup(2) };
    if console_out < 0 || console_err < 0 {
        unsafe {
            if console_out >= 0 {
                libc::close(console_out);
            }
            if console_err >= 0 {
                libc::close(console_err);
            }
        }
        return None;
    }

    // O_CLOEXEC so the raw pipe fds don't leak into the exec'd container; the
    // child's dup2 onto fd 1/2 clears CLOEXEC there so only those survive exec.
    let mut out_fds = [0 as RawFd; 2];
    let mut err_fds = [0 as RawFd; 2];
    if unsafe { libc::pipe2(out_fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        unsafe {
            libc::close(console_out);
            libc::close(console_err);
        }
        return None;
    }
    if unsafe { libc::pipe2(err_fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        // The out-pipe succeeded; close it too so a failed err-pipe (e.g. EMFILE)
        // doesn't leak the two out-pipe fds.
        unsafe {
            libc::close(console_out);
            libc::close(console_err);
            libc::close(out_fds[0]);
            libc::close(out_fds[1]);
        }
        return None;
    }
    Some(StdioRelayFds {
        out_w: out_fds[1],
        err_w: err_fds[1],
        out_r: out_fds[0],
        err_r: err_fds[0],
        console_out,
        console_err,
    })
}

/// Start the two relay threads (read pipe -> write console). Called *after* the
/// container fork so guest-init is single-threaded across `fork()` (fork-safety:
/// the codebase keeps the post-fork child free of locks held by other threads).
/// Consumes the read-ends + console dups; the write-ends are closed by the caller.
///
/// NOTE: a hand-rolled `read`/`write` loop — NOT `std::io::copy`. On Linux,
/// `io::copy` takes a `splice(2)` fast path for a pipe source, which on a
/// pipe → virtio-console pair returns a spurious `Ok(0)` (premature EOF). That
/// dropped the read-end immediately, so the container's first write hit a
/// reader-less pipe and died with SIGPIPE. The explicit loop avoids splice.
#[cfg(target_os = "linux")]
pub(super) fn start_stdio_relays(out_r: i32, console_out: i32, err_r: i32, console_err: i32) {
    let mut handles = Vec::with_capacity(2);
    for (read_fd, console_fd) in [(out_r, console_out), (err_r, console_err)] {
        handles.push(std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                let n = unsafe {
                    libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
                };
                if n < 0 {
                    // EINTR: a signal (e.g. the SIGTERM handler, installed without
                    // SA_RESTART) interrupted the blocking read — retry, don't
                    // mistake it for EOF and truncate the container's final output.
                    // Any other error means the pipe is gone, so stop.
                    if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    break;
                }
                // EOF — the container closed its pipe write-end (it exited), so the
                // relay is finished.
                if n == 0 {
                    break;
                }
                let mut off = 0usize;
                while off < n as usize {
                    let w = unsafe {
                        libc::write(
                            console_fd,
                            buf.as_ptr().add(off) as *const libc::c_void,
                            n as usize - off,
                        )
                    };
                    if w < 0 {
                        // Same EINTR handling for the write side: retry the same
                        // offset rather than dropping the rest of the chunk.
                        if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                            continue;
                        }
                        break;
                    }
                    if w == 0 {
                        break;
                    }
                    off += w as usize;
                }
            }
            unsafe {
                libc::close(read_fd);
                libc::close(console_fd);
            }
        }));
    }
    if let Ok(mut g) = STDIO_RELAYS
        .get_or_init(|| std::sync::Mutex::new(Vec::new()))
        .lock()
    {
        g.extend(handles);
    }
}

/// Create the standard `/dev/std{in,out,err}` + `/dev/fd` symlinks into the
/// process's own fds, the way container runtimes do.
///
/// The main container's `/dev` is a devtmpfs (real `null`/`urandom`/... nodes but
/// no std* symlinks). Apps that log to `/dev/stdout` or `/dev/stderr` (official
/// nginx, and many others) need these to resolve to their own stdio — which is
/// re-openable now that the main process's stdout/stderr are pipes (see
/// `setup_main_stdio_pipes`). Created once before the container fork so the
/// container inherits them; best-effort and idempotent.
#[cfg(target_os = "linux")]
pub(super) fn ensure_dev_std_symlinks() {
    for (link, target) in [
        ("/dev/stdin", "/proc/self/fd/0"),
        ("/dev/stdout", "/proc/self/fd/1"),
        ("/dev/stderr", "/proc/self/fd/2"),
        ("/dev/fd", "/proc/self/fd"),
    ] {
        // symlink_metadata does not follow the link, so an existing symlink whose
        // target is not yet resolvable still counts as present (idempotent).
        if std::fs::symlink_metadata(link).is_ok() {
            continue;
        }
        if let Err(e) = std::os::unix::fs::symlink(target, link) {
            warn!("Failed to symlink {link} -> {target}: {e}");
        }
    }
}

/// Drain the stdout/stderr relay threads so the container's final output reaches
/// the console before the VM halts. Idempotent; safe from any exit path.
pub(super) fn flush_stdio_relays() {
    if let Some(lock) = STDIO_RELAYS.get() {
        let handles: Vec<_> = lock
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default();
        for h in handles {
            let _ = h.join();
        }
    }
}
