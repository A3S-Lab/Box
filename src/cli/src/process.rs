//! Shared process management utilities for CLI commands.

/// Check if a process is alive.
#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(windows)]
pub fn is_process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::STILL_ACTIVE;
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
        windows_sys::Win32::Foundation::CloseHandle(handle);
        ok != 0 && exit_code == STILL_ACTIVE as u32
    }
}

/// Terminate a process immediately.
#[cfg(unix)]
pub fn terminate_process(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

#[cfg(windows)]
pub fn terminate_process(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle != 0 {
            TerminateProcess(handle, 1);
            CloseHandle(handle);
        }
    }
}

/// Send `signal`, wait up to `timeout` seconds, then force-terminate if still alive.
#[cfg(unix)]
pub async fn graceful_stop(pid: u32, signal: i32, timeout: u64) {
    unsafe {
        libc::kill(pid as i32, signal);
    }

    let start = std::time::Instant::now();
    let timeout_ms = timeout * 1000;
    loop {
        if !is_process_alive(pid) {
            break;
        }
        if start.elapsed().as_millis() > timeout_ms as u128 {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
}

#[cfg(windows)]
pub async fn graceful_stop(pid: u32, _signal: i32, _timeout: u64) {
    terminate_process(pid);
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_process_alive_current_process() {
        let current_pid = std::process::id();
        assert!(is_process_alive(current_pid));
    }

    #[test]
    fn test_is_process_alive_nonexistent() {
        assert!(!is_process_alive(99999));
    }

    #[cfg(unix)]
    #[test]
    fn test_is_process_alive_parent_process() {
        let parent_pid = unsafe { libc::getppid() as u32 };
        assert!(is_process_alive(parent_pid));
    }
}
