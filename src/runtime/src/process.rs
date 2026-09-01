//! Host process identity helpers shared by runtime consumers.

/// Check whether a host process exists.
///
/// On Unix, `EPERM` still means the process exists even though the caller is
/// not allowed to signal it.
#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub fn is_process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_INFORMATION,
    };

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_INFORMATION, 0, pid);
        if handle == 0 {
            return false;
        }
        let mut exit_code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut exit_code);
        CloseHandle(handle);
        ok != 0 && exit_code == STILL_ACTIVE as u32
    }
}

#[cfg(not(any(unix, windows)))]
pub fn is_process_alive(_pid: u32) -> bool {
    false
}

/// Read a process start time as a stable PID identity token.
///
/// Linux returns field 22 of `/proc/<pid>/stat`, measured in clock ticks since
/// boot. macOS returns the `proc_bsdinfo` start timestamp in microseconds. Both
/// distinguish a recorded process from a later process that reused the same PID.
#[cfg(target_os = "linux")]
pub fn pid_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    linux_process_identity_from_stat(&stat).map(|(_, start_time)| start_time)
}

#[cfg(target_os = "macos")]
pub fn pid_start_time(pid: u32) -> Option<u64> {
    macos_process_identity(pid).map(|identity| identity.start_time)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn pid_start_time(_pid: u32) -> Option<u64> {
    None
}

/// Check process liveness and, when recorded, its stable identity token.
///
/// Records created before PID identity tokens were introduced contain no
/// expected start time and retain their legacy liveness behavior.
pub fn is_process_alive_with_identity(pid: u32, expected_start_time: Option<u64>) -> bool {
    if !is_process_alive(pid) {
        return false;
    }

    match expected_start_time {
        Some(expected) => pid_start_time(pid) == Some(expected),
        None => true,
    }
}

/// Check whether a process identity is actively running rather than a zombie.
///
/// A completed child remains addressable by `kill(pid, 0)` until its parent
/// reaps it. Lifecycle ownership still uses [`is_process_alive_with_identity`]
/// when that distinction matters; completion waiters use this helper so a
/// fully drained worker zombie is treated as finished immediately.
#[cfg(target_os = "linux")]
pub fn is_process_running_with_identity(pid: u32, expected_start_time: Option<u64>) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    linux_process_identity_from_stat(&stat).is_some_and(|(state, start_time)| {
        is_linux_process_state_running(state)
            && expected_start_time
                .map(|expected| expected == start_time)
                .unwrap_or(true)
    })
}

#[cfg(target_os = "macos")]
pub fn is_process_running_with_identity(pid: u32, expected_start_time: Option<u64>) -> bool {
    match macos_process_identity(pid) {
        Some(identity) => {
            identity.running
                && expected_start_time
                    .map(|expected| expected == identity.start_time)
                    .unwrap_or(true)
        }
        // A legacy record without an identity token can retain the portable
        // existence fallback if libproc denies inspection. New records fail
        // closed because signalling a PID with an unverified identity is unsafe.
        None => expected_start_time.is_none() && is_process_alive(pid),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn is_process_running_with_identity(pid: u32, expected_start_time: Option<u64>) -> bool {
    is_process_alive_with_identity(pid, expected_start_time)
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MacosProcessIdentity {
    start_time: u64,
    running: bool,
}

#[cfg(target_os = "macos")]
fn macos_process_identity(pid: u32) -> Option<MacosProcessIdentity> {
    let raw_pid = i32::try_from(pid).ok().filter(|pid| *pid > 0)?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let expected_size = std::mem::size_of::<libc::proc_bsdinfo>();
    // SAFETY: `info` points to writable storage of exactly the size passed to
    // libproc. A full-size return is required before the value is initialized.
    let read = unsafe {
        libc::proc_pidinfo(
            raw_pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            i32::try_from(expected_size).ok()?,
        )
    };
    if read != i32::try_from(expected_size).ok()? {
        return None;
    }
    // SAFETY: proc_pidinfo returned the complete proc_bsdinfo payload above.
    let info = unsafe { info.assume_init() };
    Some(MacosProcessIdentity {
        start_time: info
            .pbi_start_tvsec
            .saturating_mul(1_000_000)
            .saturating_add(info.pbi_start_tvusec),
        running: info.pbi_status != libc::SZOMB,
    })
}

/// Wait until a process identity is no longer actively executing.
///
/// Unlike [`wait_for_process_exit_with_identity`], this accepts an unreaped
/// zombie as stopped. That distinction is required when a detached runtime
/// owner was spawned by a short-lived client process: a later recovery process
/// cannot reap the former client's child, but it can safely remove runtime
/// state once that child has finished executing.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn wait_for_process_stop_with_identity(
    pid: u32,
    expected_start_time: u64,
    timeout: std::time::Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !is_process_running_with_identity(pid, Some(expected_start_time)) {
            // Recovery can discard the original `Child` handle while the
            // runtime owner remains our child. Reap that completed child when
            // possible, while preserving the stopped semantics for processes
            // owned by another parent.
            let _ = try_reap_exited_child_with_identity(pid, expected_start_time);
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Wait for a Linux process identity to disappear, reaping it when it is an
/// exited child of the current process.
///
/// Recovered runtime handles retain only a durable PID/start-time pair. When a
/// worker was originally spawned by this process, dropping its `Child` handle
/// does not transfer wait ownership: the completed worker remains a zombie
/// until an explicit `waitpid`. Workers inherited by another process cannot be
/// reaped here, so this helper waits for their owner to reap them instead.
#[cfg(target_os = "linux")]
pub(crate) fn wait_for_process_exit_with_identity(
    pid: u32,
    expected_start_time: u64,
    timeout: std::time::Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !is_process_alive_with_identity(pid, Some(expected_start_time)) {
            return true;
        }
        if !is_process_running_with_identity(pid, Some(expected_start_time))
            && try_reap_exited_child_with_identity(pid, expected_start_time)
        {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Try to reap an exited process when it is a child of the current process.
///
/// Returns `true` once the recorded identity has disappeared. `false` means
/// the process is still present and must be reaped by its owning parent.
#[cfg(unix)]
fn try_reap_exited_child_with_identity(pid: u32, expected_start_time: u64) -> bool {
    if !is_process_alive_with_identity(pid, Some(expected_start_time)) {
        return true;
    }

    let Ok(raw_pid) = i32::try_from(pid) else {
        return false;
    };
    let mut status = 0;
    let waited = unsafe { libc::waitpid(raw_pid, &mut status, libc::WNOHANG) };
    waited == raw_pid || !is_process_alive_with_identity(pid, Some(expected_start_time))
}

#[cfg(not(unix))]
#[allow(dead_code)]
fn try_reap_exited_child_with_identity(pid: u32, expected_start_time: u64) -> bool {
    !is_process_alive_with_identity(pid, Some(expected_start_time))
}

#[cfg(target_os = "linux")]
fn linux_process_identity_from_stat(stat: &str) -> Option<(char, u64)> {
    // `comm` may contain spaces and parentheses, so fields begin after the
    // final `)`. Field 3 is then token zero and field 22 is token 19.
    let fields: Vec<&str> = stat
        .get(stat.rfind(')')? + 1..)?
        .split_whitespace()
        .collect();
    let state = fields.first()?.chars().next()?;
    let start_time = fields.get(19)?.parse().ok()?;
    Some((state, start_time))
}

#[cfg(target_os = "linux")]
const fn is_linux_process_state_running(state: char) -> bool {
    !matches!(state, 'Z' | 'X' | 'x')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_is_alive() {
        assert!(is_process_alive(std::process::id()));
    }

    #[test]
    fn missing_process_is_not_alive() {
        assert!(!is_process_alive(0x7fff_fffe));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_identity_distinguishes_a_reused_pid() {
        let pid = std::process::id();
        let start_time = pid_start_time(pid);
        assert!(
            start_time.is_some(),
            "live process must have a start-time token"
        );
        assert!(is_process_alive_with_identity(pid, start_time));
        assert!(!is_process_alive_with_identity(pid, Some(u64::MAX)));
        assert!(is_process_running_with_identity(pid, start_time));
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[allow(clippy::zombie_processes)] // Intentionally retain Child until state inspection.
    fn macos_running_identity_rejects_a_zombie() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let start_time = pid_start_time(pid).expect("capture child identity");
        child.kill().expect("terminate child without reaping it");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while is_process_running_with_identity(pid, Some(start_time))
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        assert!(!is_process_running_with_identity(pid, Some(start_time)));
        let _ = child.wait();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_start_time_after_complex_command_name() {
        let stat =
            "123 (command (with) spaces) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 4242";
        assert_eq!(linux_process_identity_from_stat(stat), Some(('S', 4242)));
        assert_eq!(linux_process_identity_from_stat("malformed"), None);
        assert_eq!(linux_process_identity_from_stat("123 (short) S 1"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn identity_rejects_a_reused_pid() {
        let pid = std::process::id();
        let start_time = pid_start_time(pid);
        assert!(start_time.is_some());
        assert!(is_process_alive_with_identity(pid, start_time));
        assert!(!is_process_alive_with_identity(pid, Some(u64::MAX)));
        assert!(is_process_alive_with_identity(pid, None));
        assert!(!is_process_alive_with_identity(0x7fff_fffe, None));
        assert!(is_process_running_with_identity(pid, start_time));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn classifies_zombie_and_dead_states_as_completed() {
        for state in ['Z', 'X', 'x'] {
            let stat = format!(
                "123 (completed worker) {state} 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 4242"
            );
            assert_eq!(linux_process_identity_from_stat(&stat), Some((state, 4242)));
            assert!(!is_linux_process_state_running(state));
        }
        for state in ['R', 'S', 'D', 'T', 't', 'I'] {
            assert!(is_linux_process_state_running(state));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[allow(clippy::zombie_processes)] // Deliberately drop Child to exercise recovered waitpid.
    fn recovered_identity_reaps_an_exited_child() {
        let child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        let start_time = pid_start_time(pid).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while is_process_running_with_identity(pid, Some(start_time))
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        assert!(is_process_alive_with_identity(pid, Some(start_time)));
        assert!(!is_process_running_with_identity(pid, Some(start_time)));
        assert!(wait_for_process_exit_with_identity(
            pid,
            start_time,
            std::time::Duration::from_secs(1),
        ));
        assert!(!is_process_alive_with_identity(pid, Some(start_time)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[allow(clippy::zombie_processes)] // Deliberately drop Child to exercise recovered waitpid.
    fn stop_waiter_reaps_an_exited_child() {
        let child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        let start_time = pid_start_time(pid).unwrap();
        drop(child);

        assert!(wait_for_process_stop_with_identity(
            pid,
            start_time,
            std::time::Duration::from_secs(1),
        ));
        assert!(!is_process_alive_with_identity(pid, Some(start_time)));
    }
}
