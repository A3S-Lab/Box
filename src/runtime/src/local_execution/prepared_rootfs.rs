use std::path::{Path, PathBuf};

/// Resolve the host-visible rootfs prepared for a managed execution.
///
/// Linux cache hits run through the overlay mount at `merged`, while cache
/// misses and copy providers use `rootfs` directly. Legacy APFS providers
/// expose their mounted data below `rootfs/.a3s-rootfs`. Keep this ordering in
/// one place so lifecycle operations inspect the same filesystem view that was
/// handed to the runtime.
pub(super) fn resolve_prepared_rootfs(box_dir: &Path) -> Option<PathBuf> {
    let populated = |path: &Path| {
        path.is_dir()
            && std::fs::read_dir(path)
                .map(|mut entries| entries.next().is_some())
                .unwrap_or(false)
    };

    let merged = box_dir.join("merged");
    if crate::rootfs::is_mountpoint(&merged) || populated(&merged) {
        return Some(merged);
    }

    let rootfs = box_dir.join("rootfs");
    let apfs_data = rootfs.join(".a3s-rootfs");
    if populated(&apfs_data) {
        return Some(apfs_data);
    }

    populated(&rootfs).then_some(rootfs)
}

#[cfg(test)]
mod tests {
    use super::resolve_prepared_rootfs;

    #[test]
    fn prefers_the_overlay_runtime_view() {
        let directory = tempfile::tempdir().unwrap();
        let rootfs = directory.path().join("rootfs");
        let merged = directory.path().join("merged");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&merged).unwrap();
        std::fs::write(rootfs.join("lower"), b"lower").unwrap();
        std::fs::write(merged.join("visible"), b"merged").unwrap();

        assert_eq!(resolve_prepared_rootfs(directory.path()), Some(merged));
    }

    #[test]
    fn resolves_copy_and_legacy_apfs_layouts() {
        let copy = tempfile::tempdir().unwrap();
        let copy_rootfs = copy.path().join("rootfs");
        std::fs::create_dir_all(&copy_rootfs).unwrap();
        std::fs::write(copy_rootfs.join("visible"), b"copy").unwrap();
        assert_eq!(resolve_prepared_rootfs(copy.path()), Some(copy_rootfs));

        let apfs = tempfile::tempdir().unwrap();
        let apfs_rootfs = apfs.path().join("rootfs/.a3s-rootfs");
        std::fs::create_dir_all(&apfs_rootfs).unwrap();
        std::fs::write(apfs_rootfs.join("visible"), b"apfs").unwrap();
        assert_eq!(resolve_prepared_rootfs(apfs.path()), Some(apfs_rootfs));
    }

    #[test]
    fn rejects_absent_and_empty_layouts() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(resolve_prepared_rootfs(directory.path()), None);
        std::fs::create_dir_all(directory.path().join("merged")).unwrap();
        std::fs::create_dir_all(directory.path().join("rootfs")).unwrap();
        assert_eq!(resolve_prepared_rootfs(directory.path()), None);
    }
}
