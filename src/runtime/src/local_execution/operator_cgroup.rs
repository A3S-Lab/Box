//! When an operator setuid spawn may ignore a cgroup v2 ancestor denial.
//!
//! `pre_exec` still runs as the unprivileged parent. EACCES there is not a
//! failed owner: `a3s-oci` migrates after the mode 4755 exec. Other failures,
//! and every non-operator spawn, stay fail-closed.

/// Defer only the operator setuid parent's cgroup-ancestor denial.
///
/// Called from Linux `oci_owner` spawn. Kept unconditionally compiled so macOS
/// unit tests can lock the policy without a Linux target.
#[must_use]
#[cfg_attr(not(all(feature = "vm", target_os = "linux")), allow(dead_code))]
pub(crate) fn defer_unprivileged_cgroup_migration(
    operator_setuid: bool,
    error: &std::io::Error,
) -> bool {
    operator_setuid && error.kind() == std::io::ErrorKind::PermissionDenied
}

#[cfg(test)]
mod tests {
    use super::defer_unprivileged_cgroup_migration;

    #[test]
    fn operator_setuid_defers_only_cgroup_ancestor_denial() {
        // The spawn path branches on `ErrorKind`, which is what Linux reports
        // for `EACCES` on the cgroup v2 ancestor. A raw errno is not a Windows
        // `GetLastError` value, so the policy test uses the kind directly.
        let denied = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "cgroup ancestor");
        assert!(defer_unprivileged_cgroup_migration(true, &denied));
        assert!(!defer_unprivileged_cgroup_migration(false, &denied));
        let missing = std::io::Error::new(std::io::ErrorKind::NotFound, "missing cgroup");
        assert!(!defer_unprivileged_cgroup_migration(true, &missing));
    }

    #[cfg(unix)]
    #[test]
    fn operator_setuid_treats_linux_eacces_as_the_ancestor_denial() {
        let denied = std::io::Error::from_raw_os_error(libc::EACCES);
        assert_eq!(denied.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(defer_unprivileged_cgroup_migration(true, &denied));
        assert!(!defer_unprivileged_cgroup_migration(false, &denied));
    }
}
