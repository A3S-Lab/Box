//! Scoped Windows capability for preserving OCI symbolic links.
//!
//! Windows administrator and service tokens may contain
//! `SeCreateSymbolicLinkPrivilege` in a disabled state. OCI extraction and
//! rootfs diagnostics must observe the same effective capability, so this
//! module owns the single process-token implementation used by both paths.

use std::sync::{Mutex, MutexGuard};

static WINDOWS_SYMLINK_PRIVILEGE_LOCK: Mutex<()> = Mutex::new(());

/// Serialized scope which enables an already assigned symbolic-link privilege.
///
/// The scope does not grant a privilege absent from the process token. In that
/// case it leaves the token unchanged so Windows Developer Mode can still
/// authorize `CreateSymbolicLinkW`. The previous token state is restored before
/// the process-wide serialization lock is released.
#[must_use]
#[doc(hidden)]
pub struct WindowsSymlinkPrivilegeGuard {
    // Field order is intentional: restore the process privilege before
    // releasing the serialization lock.
    privilege: Option<WindowsTokenPrivilegeGuard>,
    _lock: MutexGuard<'static, ()>,
}

impl WindowsSymlinkPrivilegeGuard {
    /// Acquire the process-wide scope used around a bounded symlink operation.
    pub fn acquire() -> Self {
        let lock = WINDOWS_SYMLINK_PRIVILEGE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let privilege = match WindowsTokenPrivilegeGuard::enable_symlink_creation() {
            Ok(privilege) => privilege,
            Err(error) => {
                tracing::debug!(
                    error = %error,
                    "Could not enable the optional Windows symlink privilege"
                );
                None
            }
        };
        Self {
            privilege,
            _lock: lock,
        }
    }

    /// Whether the process token's assigned privilege was enabled for this scope.
    ///
    /// A `false` result does not rule out Developer Mode as an alternate way to
    /// authorize symbolic-link creation.
    pub fn assigned_privilege_enabled(&self) -> bool {
        self.privilege.is_some()
    }
}

/// Explain a denied symlink operation using the effective privilege state.
#[doc(hidden)]
pub fn denial_diagnostic(assigned_privilege_enabled: bool) -> &'static str {
    if assigned_privilege_enabled {
        "SeCreateSymbolicLinkPrivilege was enabled, but the target directory ACL or endpoint \
         security denied symbolic-link creation; verify the target ACL and allow the approved \
         A3S Box executable and target directory in endpoint security; ERROR_ACCESS_DENIED (5) \
         or ERROR_PRIVILEGE_NOT_HELD (1314)"
    } else {
        "enable Windows Developer Mode or grant SeCreateSymbolicLinkPrivilege and allow the \
         target directory; ERROR_ACCESS_DENIED (5) or ERROR_PRIVILEGE_NOT_HELD (1314)"
    }
}

/// Returns whether a failed link creation means that this process simply lacks
/// the Windows capability required to create OCI links.
///
/// `ERROR_ACCESS_DENIED` is only treated as an expected capability denial when
/// the token does not contain an assigned symbolic-link privilege. If the
/// privilege was assigned and enabled, access denied is preserved as a real
/// ACL or endpoint-security failure instead of being hidden by a test skip.
#[doc(hidden)]
pub fn is_capability_denial(error: &std::io::Error, assigned_privilege_enabled: bool) -> bool {
    match error.raw_os_error() {
        Some(1314) => true,
        Some(5) => !assigned_privilege_enabled,
        _ => false,
    }
}

struct WindowsTokenPrivilegeGuard {
    token: windows_sys::Win32::Foundation::HANDLE,
    previous: windows_sys::Win32::Security::TOKEN_PRIVILEGES,
}

impl WindowsTokenPrivilegeGuard {
    fn enable_symlink_creation() -> std::io::Result<Option<Self>> {
        use std::mem::size_of;
        use std::ptr::null;
        use windows_sys::Win32::Foundation::{
            CloseHandle, GetLastError, SetLastError, ERROR_NOT_ALL_ASSIGNED, ERROR_SUCCESS, LUID,
        };
        use windows_sys::Win32::Security::{
            AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES,
            SE_CREATE_SYMBOLIC_LINK_NAME, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES,
            TOKEN_PRIVILEGES, TOKEN_QUERY,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        let mut token = 0;
        // SAFETY: `token` is a valid output pointer and the pseudo process
        // handle remains valid for the duration of this call.
        if unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut token,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }

        let mut luid = LUID {
            LowPart: 0,
            HighPart: 0,
        };
        // SAFETY: the privilege name is a static NUL-terminated Windows string,
        // `luid` is writable, and the local-system name is intentionally null.
        if unsafe { LookupPrivilegeValueW(null(), SE_CREATE_SYMBOLIC_LINK_NAME, &mut luid) } == 0 {
            let error = std::io::Error::last_os_error();
            // SAFETY: `token` was returned by OpenProcessToken above.
            unsafe { CloseHandle(token) };
            return Err(error);
        }

        let requested = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        let mut previous = TOKEN_PRIVILEGES {
            PrivilegeCount: 0,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: LUID {
                    LowPart: 0,
                    HighPart: 0,
                },
                Attributes: 0,
            }],
        };
        let mut previous_length = 0;
        // AdjustTokenPrivileges can return success while reporting
        // ERROR_NOT_ALL_ASSIGNED through GetLastError.
        // SAFETY: both TOKEN_PRIVILEGES buffers are initialized and sized for
        // the single privilege requested here.
        unsafe { SetLastError(ERROR_SUCCESS) };
        let adjusted = unsafe {
            AdjustTokenPrivileges(
                token,
                0,
                &requested,
                size_of::<TOKEN_PRIVILEGES>() as u32,
                &mut previous,
                &mut previous_length,
            )
        };
        let adjustment_error = unsafe { GetLastError() };
        if adjusted == 0 {
            // SAFETY: `token` was returned by OpenProcessToken above.
            unsafe { CloseHandle(token) };
            return if adjustment_error == ERROR_SUCCESS {
                Err(std::io::Error::other(
                    "AdjustTokenPrivileges failed without a Windows error code",
                ))
            } else {
                Err(std::io::Error::from_raw_os_error(adjustment_error as i32))
            };
        }
        if adjustment_error != ERROR_SUCCESS {
            // SAFETY: `token` was returned by OpenProcessToken above.
            unsafe { CloseHandle(token) };
            if adjustment_error == ERROR_NOT_ALL_ASSIGNED {
                return Ok(None);
            }
            return Err(std::io::Error::from_raw_os_error(adjustment_error as i32));
        }

        Ok(Some(Self { token, previous }))
    }
}

impl Drop for WindowsTokenPrivilegeGuard {
    fn drop(&mut self) {
        use std::ptr::null_mut;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Security::AdjustTokenPrivileges;

        // SAFETY: this restores the exact privilege state captured from the
        // same open token, then closes that owned handle.
        unsafe {
            AdjustTokenPrivileges(self.token, 0, &self.previous, 0, null_mut(), null_mut());
            CloseHandle(self.token);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denial_diagnostic_distinguishes_endpoint_policy() {
        let enabled = denial_diagnostic(true);
        assert!(enabled.contains("SeCreateSymbolicLinkPrivilege was enabled"));
        assert!(enabled.contains("endpoint security"));
        assert!(!enabled.contains("enable Windows Developer Mode"));

        let unavailable = denial_diagnostic(false);
        assert!(unavailable.contains("enable Windows Developer Mode"));
        assert!(unavailable.contains("grant SeCreateSymbolicLinkPrivilege"));
    }

    #[test]
    fn capability_denial_does_not_hide_acl_errors_when_privilege_is_enabled() {
        assert!(is_capability_denial(
            &std::io::Error::from_raw_os_error(1314),
            true
        ));
        assert!(is_capability_denial(
            &std::io::Error::from_raw_os_error(5),
            false
        ));
        assert!(!is_capability_denial(
            &std::io::Error::from_raw_os_error(5),
            true
        ));
    }

    #[test]
    fn acquired_scope_preserves_a_real_link_when_the_identity_is_capable() {
        let temporary = tempfile::tempdir().unwrap();
        std::fs::write(temporary.path().join("target"), b"probe").unwrap();

        let result = {
            let _guard = WindowsSymlinkPrivilegeGuard::acquire();
            std::os::windows::fs::symlink_file("target", temporary.path().join("link"))
        };
        match result {
            Ok(()) => {
                let link = temporary.path().join("link");
                assert!(std::fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink());
                assert_eq!(
                    std::fs::read_link(link).unwrap(),
                    std::path::Path::new("target")
                );
            }
            Err(error) if matches!(error.raw_os_error(), Some(5) | Some(1314)) => {
                eprintln!("skipping Windows symlink privilege test: {error}");
            }
            Err(error) => panic!("Windows symlink privilege scope failed unexpectedly: {error}"),
        }

        // Dropping the first scope must release the process-wide lock.
        let _second = WindowsSymlinkPrivilegeGuard::acquire();
    }
}
