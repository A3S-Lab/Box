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

/// Read a process's Linux start time as a stable PID identity token.
///
/// The value is field 22 of `/proc/<pid>/stat`, measured in clock ticks since
/// boot. It distinguishes a recorded process from a later process that reused
/// the same PID. Other platforms return `None` until they provide an equivalent
/// stable token.
#[cfg(target_os = "linux")]
pub fn pid_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    linux_pid_start_time_from_stat(&stat)
}

#[cfg(not(target_os = "linux"))]
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

#[cfg(target_os = "linux")]
fn linux_pid_start_time_from_stat(stat: &str) -> Option<u64> {
    // `comm` may contain spaces and parentheses, so fields begin after the
    // final `)`. Field 3 is then token zero and field 22 is token 19.
    let fields = stat.get(stat.rfind(')')? + 1..)?;
    fields.split_whitespace().nth(19)?.parse().ok()
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

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_start_time_after_complex_command_name() {
        let stat =
            "123 (command (with) spaces) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 4242";
        assert_eq!(linux_pid_start_time_from_stat(stat), Some(4242));
        assert_eq!(linux_pid_start_time_from_stat("malformed"), None);
        assert_eq!(linux_pid_start_time_from_stat("123 (short) S 1"), None);
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
    }
}
