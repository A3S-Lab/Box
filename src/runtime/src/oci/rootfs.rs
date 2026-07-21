//! OCI rootfs builder.
//!
//! Extracts an OCI image into a guest rootfs directory.
//! Optionally installs the guest-init binary at /sbin/init.

use a3s_box_core::error::{BoxError, Result};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::path::{Component, Path};

use super::image::OciImage;
use super::layers::{extract_layer_with_metadata, finalize_rootfs_metadata};

/// Builder for creating a guest rootfs from an OCI image.
///
/// The image is extracted directly at the rootfs root ("/"), preserving
/// absolute symlinks and dynamic linker paths from the original image.
pub struct OciRootfsBuilder {
    /// Target rootfs directory
    rootfs_path: PathBuf,

    /// Path to the OCI image directory
    image_path: PathBuf,

    /// Path to guest init binary (optional)
    guest_init_path: Option<PathBuf>,

    /// Override for `/etc/resolv.conf` content (e.g. the pod's DNS config).
    /// When `None`, a default resolv.conf is written.
    resolv_conf: Option<String>,
}

impl OciRootfsBuilder {
    /// Create a new OCI rootfs builder.
    pub fn new(rootfs_path: impl Into<PathBuf>) -> Self {
        Self {
            rootfs_path: rootfs_path.into(),
            image_path: PathBuf::new(),
            guest_init_path: None,
            resolv_conf: None,
        }
    }

    /// Override the `/etc/resolv.conf` written into the rootfs.
    ///
    /// Used to apply a pod's CRI `DNSConfig`. An empty string is ignored so the
    /// default resolv.conf is written instead.
    pub fn with_resolv_conf(mut self, content: impl Into<String>) -> Self {
        let content = content.into();
        if !content.is_empty() {
            self.resolv_conf = Some(content);
        }
        self
    }

    /// Set the OCI image path to extract.
    pub fn with_image(mut self, path: impl Into<PathBuf>) -> Self {
        self.image_path = path.into();
        self
    }

    /// Set the path to the guest init binary.
    ///
    /// If set, the guest init binary will be installed at `/sbin/init` in the
    /// rootfs, overriding any existing init from the OCI image.
    pub fn with_guest_init(mut self, path: impl Into<PathBuf>) -> Self {
        self.guest_init_path = Some(path.into());
        self
    }

    /// Build the rootfs by extracting the OCI image.
    ///
    /// # Process
    ///
    /// 1. Create base directory structure
    /// 2. Extract image layers to rootfs root
    /// 3. Install guest init binary (if provided)
    /// 4. Ensure essential system files exist
    pub fn build(&self) -> Result<()> {
        tracing::info!(
            rootfs = %self.rootfs_path.display(),
            "Building OCI rootfs"
        );

        if self.image_path.as_os_str().is_empty() {
            return Err(BoxError::OciImageError(
                "OCI image path not set".to_string(),
            ));
        }

        self.create_base_structure()?;
        self.extract_image()?;

        if self.guest_init_path.is_some() {
            self.install_guest_init()?;
        }

        self.create_essential_files()?;
        finalize_rootfs_metadata(&self.rootfs_path)?;

        tracing::info!("OCI rootfs built successfully");
        Ok(())
    }

    /// Install or refresh only the guest-init binary in an existing rootfs.
    pub fn install_guest_init_only(&self) -> Result<()> {
        if self.guest_init_path.is_some() {
            self.install_guest_init()?;
        }
        Ok(())
    }

    /// Create the base directory structure.
    fn create_base_structure(&self) -> Result<()> {
        let dirs = [
            "dev",
            "proc",
            "sys",
            "tmp",
            "run",
            "etc",
            "var",
            "var/tmp",
            "var/log",
            "workspace",
        ];

        for dir in dirs {
            let full_path = self.rootfs_path.join(dir);
            std::fs::create_dir_all(&full_path).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to create directory {}: {}",
                    full_path.display(),
                    e
                ))
            })?;
            tracing::debug!(dir = %full_path.display(), "Created directory");
        }

        // The service can run with a restrictive umask (the production smoke
        // uses 077), but the root of a Linux container must remain traversable
        // by image users other than root. Layer archives normally omit an
        // explicit `.` entry, so without this normalization the host-created
        // rootfs directory becomes `/` with mode 0700 inside the container.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            std::fs::set_permissions(&self.rootfs_path, std::fs::Permissions::from_mode(0o755))
                .map_err(|error| {
                    BoxError::BuildError(format!(
                        "Failed to set rootfs permissions on {}: {error}",
                        self.rootfs_path.display()
                    ))
                })?;
        }

        Ok(())
    }

    /// Extract OCI image layers to the rootfs root.
    fn extract_image(&self) -> Result<()> {
        let image = OciImage::from_path(&self.image_path)?;

        tracing::info!(
            image = %self.image_path.display(),
            rootfs = %self.rootfs_path.display(),
            layers = image.layer_paths().len(),
            "Extracting OCI image"
        );

        for layer_path in image.layer_paths() {
            extract_layer_with_metadata(layer_path, &self.rootfs_path)?;
        }

        Ok(())
    }

    /// Install guest init binary to /sbin/init.
    fn install_guest_init(&self) -> Result<()> {
        let src = self
            .guest_init_path
            .as_ref()
            .ok_or_else(|| BoxError::BuildError("Guest init path not set".to_string()))?;

        if !src.exists() {
            return Err(BoxError::BuildError(format!(
                "Guest init binary not found: {}",
                src.display()
            )));
        }

        // Resolve Linux guest symlinks component-by-component on every host.
        // Using `rootfs.join("sbin")` directly is unsafe even on Unix: an image
        // may contain `sbin -> /tmp`, whose host interpretation would escape the
        // rootfs and replace `/tmp/init`. Prefer an existing `/usr/sbin` only
        // when `/sbin` is genuinely absent, preserving usr-only image support.
        let sbin_path = self.rootfs_path.join("sbin");
        let install_dir = match std::fs::symlink_metadata(&sbin_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let usr_sbin = self.rootfs_directory_path("usr/sbin")?;
                if usr_sbin.is_dir() {
                    usr_sbin
                } else {
                    self.rootfs_directory_path("sbin")?
                }
            }
            Ok(_) => self.rootfs_directory_path("sbin")?,
            Err(error) => {
                return Err(BoxError::BuildError(format!(
                    "Failed to inspect /sbin path {}: {error}",
                    sbin_path.display()
                )));
            }
        };

        std::fs::create_dir_all(&install_dir).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to create guest init install directory {}: {}",
                install_dir.display(),
                e
            ))
        })?;

        let dest = install_dir.join("init");
        // Remove any existing init (e.g., busybox symlink in Alpine)
        if dest.exists() || dest.symlink_metadata().is_ok() {
            std::fs::remove_file(&dest).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to remove existing {}: {}",
                    dest.display(),
                    e
                ))
            })?;
        }
        std::fs::copy(src, &dest).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to copy guest init to {}: {}",
                dest.display(),
                e
            ))
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&dest)
                .map_err(|e| BoxError::BuildError(format!("Failed to get permissions: {}", e)))?
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&dest, perms)
                .map_err(|e| BoxError::BuildError(format!("Failed to set permissions: {}", e)))?;
        }

        tracing::info!(
            src = %src.display(),
            dst = %dest.display(),
            "Installed guest init"
        );

        Ok(())
    }

    /// Ensure essential system files exist, preserving OCI image entries.
    fn create_essential_files(&self) -> Result<()> {
        self.ensure_passwd_entries(&[
            ("root", "root:x:0:0:root:/root:/bin/sh"),
            ("nobody", "nobody:x:65534:65534:nobody:/:/bin/false"),
        ])?;

        self.ensure_group_entries(&[("root", "root:x:0:"), ("nogroup", "nogroup:x:65534:")])?;

        self.write_file("etc/hosts", "127.0.0.1\tlocalhost\n::1\t\tlocalhost\n")?;
        let resolv_conf = self
            .resolv_conf
            .as_deref()
            .unwrap_or("nameserver 8.8.8.8\nnameserver 8.8.4.4\n");
        self.write_file("etc/resolv.conf", resolv_conf)?;
        self.write_file(
            "etc/nsswitch.conf",
            "passwd: files\ngroup: files\nhosts: files dns\n",
        )?;

        Ok(())
    }

    fn ensure_passwd_entries(&self, required: &[(&str, &str)]) -> Result<()> {
        let passwd_path = self.rootfs_file_path("etc/passwd")?;
        let existing = std::fs::read_to_string(&passwd_path).unwrap_or_default();

        let mut content = existing.clone();
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }

        for (username, entry) in required {
            let has_user = existing
                .lines()
                .any(|line| line.split(':').next() == Some(username));
            if !has_user {
                content.push_str(entry);
                content.push('\n');
            }
        }

        self.write_file("etc/passwd", &content)
    }

    fn ensure_group_entries(&self, required: &[(&str, &str)]) -> Result<()> {
        let group_path = self.rootfs_file_path("etc/group")?;
        let existing = std::fs::read_to_string(&group_path).unwrap_or_default();

        let mut content = existing.clone();
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }

        for (groupname, entry) in required {
            let has_group = existing
                .lines()
                .any(|line| line.split(':').next() == Some(groupname));
            if !has_group {
                content.push_str(entry);
                content.push('\n');
            }
        }

        self.write_file("etc/group", &content)
    }

    fn write_file(&self, relative_path: &str, content: &str) -> Result<()> {
        let full_path = write_guest_file(&self.rootfs_path, relative_path, content)?;
        tracing::debug!(path = %full_path.display(), "Created file");
        Ok(())
    }

    fn rootfs_file_path(&self, relative_path: &str) -> Result<PathBuf> {
        let relative = Path::new(relative_path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
        {
            return Err(BoxError::BuildError(format!(
                "Invalid rootfs relative path: {relative_path}"
            )));
        }

        relative.file_name().ok_or_else(|| {
            BoxError::BuildError(format!("Invalid rootfs file path: {relative_path}"))
        })?;
        self.resolve_rootfs_path(relative, true)
    }

    fn rootfs_directory_path(&self, relative_path: &str) -> Result<PathBuf> {
        let relative = Path::new(relative_path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
        {
            return Err(BoxError::BuildError(format!(
                "Invalid rootfs relative directory: {relative_path}"
            )));
        }
        self.resolve_rootfs_path(relative, false)
    }

    /// Resolve a Linux guest path without allowing any symlink hop to escape
    /// `rootfs`. The final component may be a regular file only when
    /// `allow_final_file` is true.
    ///
    /// Windows does not consider `/usr/etc` an absolute host path, and
    /// `read_link` may render it as `\usr\etc`. Treat both separators as guest
    /// separators on Windows and resolve a leading slash from the guest root.
    fn resolve_rootfs_path(&self, path: &Path, allow_final_file: bool) -> Result<PathBuf> {
        let (absolute, pending) = guest_path_components(path)?;
        if absolute {
            return Err(BoxError::BuildError(format!(
                "Rootfs path must be relative: {}",
                path.display()
            )));
        }
        self.resolve_rootfs_components(Vec::new(), pending, allow_final_file)
    }

    fn resolve_rootfs_components(
        &self,
        mut resolved: Vec<String>,
        mut pending: VecDeque<String>,
        allow_final_file: bool,
    ) -> Result<PathBuf> {
        // Linux limits path resolution to 40 symbolic-link traversals. Matching
        // that bound makes loops fail deterministically before any write.
        const MAX_SYMLINK_HOPS: usize = 40;
        let mut symlink_hops = 0_usize;

        while let Some(component) = pending.pop_front() {
            if component == ".." {
                if resolved.pop().is_none() {
                    return Err(BoxError::BuildError(
                        "Rootfs symlink target escapes rootfs".to_string(),
                    ));
                }
                continue;
            }

            let mut candidate = self.rootfs_path.clone();
            for segment in &resolved {
                candidate.push(segment);
            }
            candidate.push(&component);

            match std::fs::symlink_metadata(&candidate) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    symlink_hops += 1;
                    if symlink_hops > MAX_SYMLINK_HOPS {
                        return Err(BoxError::BuildError(format!(
                            "Too many rootfs symlink hops while resolving {}",
                            candidate.display()
                        )));
                    }
                    let target = std::fs::read_link(&candidate).map_err(|error| {
                        BoxError::BuildError(format!(
                            "Failed to resolve rootfs symlink {}: {error}",
                            candidate.display()
                        ))
                    })?;
                    let (absolute, target_components) = guest_path_components(&target)?;
                    if absolute {
                        resolved.clear();
                    }
                    for target_component in target_components.into_iter().rev() {
                        pending.push_front(target_component);
                    }
                }
                Ok(metadata) if metadata.is_dir() => resolved.push(component),
                Ok(metadata) if allow_final_file && pending.is_empty() && metadata.is_file() => {
                    resolved.push(component)
                }
                Ok(_) => {
                    return Err(BoxError::BuildError(format!(
                        "Cannot use {} as a rootfs directory because it is not a directory",
                        candidate.display()
                    )));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    resolved.push(component)
                }
                Err(error) => {
                    return Err(BoxError::BuildError(format!(
                        "Failed to inspect rootfs path {}: {error}",
                        candidate.display()
                    )));
                }
            }
        }

        let mut result = self.rootfs_path.clone();
        for segment in resolved {
            result.push(segment);
        }
        Ok(result)
    }

    /// Get the OCI image configuration.
    ///
    /// Useful for extracting entrypoint, environment, working directory, etc.
    pub fn image_config(&self) -> Result<super::image::OciImageConfig> {
        let image = OciImage::from_path(&self.image_path)?;
        Ok(image.config().clone())
    }
}

/// Resolve a file path using Linux guest symlink semantics while guaranteeing
/// that every resolved component remains beneath `rootfs_path`.
///
/// Runtime startup code must use this helper before reading, writing, or
/// changing metadata for image-owned paths. Host `Path::join`/`canonicalize`
/// interpret absolute OCI symlink targets as host paths and can otherwise
/// escape the guest rootfs.
pub(crate) fn resolve_guest_file_path(rootfs_path: &Path, relative_path: &str) -> Result<PathBuf> {
    OciRootfsBuilder::new(rootfs_path).rootfs_file_path(relative_path)
}

/// Resolve only the parent components of a guest path, leaving the final
/// directory entry itself untouched. Use this when replacing a symlink rather
/// than following it.
pub(crate) fn resolve_guest_entry_path(rootfs_path: &Path, relative_path: &str) -> Result<PathBuf> {
    let relative = Path::new(relative_path);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(BoxError::BuildError(format!(
            "Invalid rootfs relative entry: {relative_path}"
        )));
    }
    let name = relative.file_name().ok_or_else(|| {
        BoxError::BuildError(format!("Invalid rootfs entry path: {relative_path}"))
    })?;
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    let parent = parent.to_str().ok_or_else(|| {
        BoxError::BuildError(format!(
            "Rootfs entry parent is not UTF-8: {}",
            parent.display()
        ))
    })?;
    Ok(resolve_guest_directory_path(rootfs_path, parent)?.join(name))
}

/// Remove a final guest directory entry without traversing a symlink/reparse
/// point. Missing entries are already in the desired state; real directories
/// are rejected.
pub(crate) fn remove_guest_entry_no_follow(
    rootfs_path: &Path,
    relative_path: &str,
) -> Result<PathBuf> {
    let path = resolve_guest_entry_path(rootfs_path, relative_path)?;
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            return Err(BoxError::BuildError(format!(
                "Refusing to replace guest directory {} with a file",
                path.display()
            )));
        }
        Ok(_) => {
            #[cfg(windows)]
            a3s_box_core::windows_file::remove_path_no_follow(&path).map_err(|error| {
                BoxError::BuildError(format!(
                    "Failed to remove guest entry {}: {error}",
                    path.display()
                ))
            })?;
            #[cfg(not(windows))]
            std::fs::remove_file(&path).map_err(|error| {
                BoxError::BuildError(format!(
                    "Failed to remove guest entry {}: {error}",
                    path.display()
                ))
            })?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(BoxError::BuildError(format!(
                "Failed to inspect guest entry {}: {error}",
                path.display()
            )));
        }
    }
    Ok(path)
}

/// Replace the final guest entry with a newly created regular file, never
/// following an existing symlink at that entry.
pub(crate) fn replace_guest_file_no_follow(
    rootfs_path: &Path,
    relative_path: &str,
    content: impl AsRef<[u8]>,
) -> Result<PathBuf> {
    use std::io::Write as _;

    let path = remove_guest_entry_no_follow(rootfs_path, relative_path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            BoxError::BuildError(format!(
                "Failed to create guest file parent {}: {error}",
                parent.display()
            ))
        })?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| {
            BoxError::BuildError(format!(
                "Failed to create guest file {}: {error}",
                path.display()
            ))
        })?;
    file.write_all(content.as_ref()).map_err(|error| {
        BoxError::BuildError(format!(
            "Failed to write guest file {}: {error}",
            path.display()
        ))
    })?;
    file.flush().map_err(|error| {
        BoxError::BuildError(format!(
            "Failed to flush guest file {}: {error}",
            path.display()
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).map_err(
            |error| {
                BoxError::BuildError(format!(
                    "Failed to set guest file permissions on {}: {error}",
                    path.display()
                ))
            },
        )?;
    }
    Ok(path)
}

/// Directory counterpart to [`resolve_guest_file_path`].
pub(crate) fn resolve_guest_directory_path(
    rootfs_path: &Path,
    relative_path: &str,
) -> Result<PathBuf> {
    OciRootfsBuilder::new(rootfs_path).rootfs_directory_path(relative_path)
}

/// Create (or resolve) one guest directory without ever interpreting an OCI
/// absolute symlink as a host-absolute path.
pub(crate) fn ensure_guest_directory(rootfs_path: &Path, relative_path: &str) -> Result<PathBuf> {
    let path = resolve_guest_directory_path(rootfs_path, relative_path)?;
    std::fs::create_dir_all(&path).map_err(|error| {
        BoxError::BuildError(format!(
            "Failed to create guest directory {}: {error}",
            path.display()
        ))
    })?;
    Ok(path)
}

/// Read one UTF-8 guest file through the same bounded resolver used for
/// writes. A missing file is represented by `None`; malformed or unsafe paths
/// are errors rather than silently falling back to host data.
pub(crate) fn read_guest_file_to_string(
    rootfs_path: &Path,
    relative_path: &str,
) -> Result<Option<String>> {
    let path = resolve_guest_file_path(rootfs_path, relative_path)?;
    match std::fs::read_to_string(&path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(BoxError::BuildError(format!(
            "Failed to read guest file {}: {error}",
            path.display()
        ))),
    }
}

/// Write one regular guest file after safely resolving all image-owned parent
/// and final-component symlinks.
pub(crate) fn write_guest_file(
    rootfs_path: &Path,
    relative_path: &str,
    content: impl AsRef<[u8]>,
) -> Result<PathBuf> {
    let path = resolve_guest_file_path(rootfs_path, relative_path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            BoxError::BuildError(format!(
                "Failed to create guest file parent {}: {error}",
                parent.display()
            ))
        })?;
    }
    std::fs::write(&path, content).map_err(|error| {
        BoxError::BuildError(format!(
            "Failed to write guest file {}: {error}",
            path.display()
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).map_err(
            |error| {
                BoxError::BuildError(format!(
                    "Failed to set guest file permissions on {}: {error}",
                    path.display()
                ))
            },
        )?;
    }
    Ok(path)
}

fn guest_path_components(path: &Path) -> Result<(bool, VecDeque<String>)> {
    let rendered = path.to_str().ok_or_else(|| {
        BoxError::BuildError(format!(
            "Rootfs symlink target is not valid UTF-8: {}",
            path.display()
        ))
    })?;
    #[cfg(windows)]
    let rendered = rendered.replace('\\', "/");
    #[cfg(not(windows))]
    let rendered = rendered.to_string();

    let absolute = rendered.starts_with('/');
    let mut components = VecDeque::new();
    for component in rendered.split('/') {
        match component {
            "" | "." => {}
            ".." => components.push_back(component.to_string()),
            value => {
                if value.contains('\0') {
                    return Err(BoxError::BuildError(
                        "Rootfs symlink target contains NUL".to_string(),
                    ));
                }
                #[cfg(windows)]
                if value.contains(':') {
                    return Err(BoxError::BuildError(format!(
                        "Rootfs symlink target contains a Windows path prefix: {}",
                        path.display()
                    )));
                }
                let mut host_components = Path::new(value).components();
                if !matches!(host_components.next(), Some(Component::Normal(_)))
                    || host_components.next().is_some()
                {
                    return Err(BoxError::BuildError(format!(
                        "Invalid rootfs symlink component in {}",
                        path.display()
                    )));
                }
                components.push_back(value.to_string());
            }
        }
    }
    Ok((absolute, components))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    #[test]
    fn test_oci_rootfs_builder_creates_base_structure() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");
        let image = temp_dir.path().join("image");

        create_test_oci_image(&image);

        OciRootfsBuilder::new(&rootfs_path)
            .with_image(&image)
            .build()
            .unwrap();

        assert!(rootfs_path.join("dev").exists());
        assert!(rootfs_path.join("proc").exists());
        assert!(rootfs_path.join("sys").exists());
        assert!(rootfs_path.join("tmp").exists());
        assert!(rootfs_path.join("etc").exists());
        assert!(rootfs_path.join("workspace").exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_oci_rootfs_builder_makes_root_searchable_by_image_users() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");
        let image = temp_dir.path().join("image");

        std::fs::create_dir_all(&rootfs_path).unwrap();
        std::fs::set_permissions(&rootfs_path, std::fs::Permissions::from_mode(0o700)).unwrap();
        create_test_oci_image(&image);

        OciRootfsBuilder::new(&rootfs_path)
            .with_image(&image)
            .build()
            .unwrap();

        let mode = std::fs::metadata(&rootfs_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn test_oci_rootfs_builder_creates_essential_files() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");
        let image = temp_dir.path().join("image");

        create_test_oci_image(&image);

        OciRootfsBuilder::new(&rootfs_path)
            .with_image(&image)
            .build()
            .unwrap();

        assert!(rootfs_path.join("etc/passwd").exists());
        assert!(rootfs_path.join("etc/group").exists());
        assert!(rootfs_path.join("etc/hosts").exists());
        assert!(rootfs_path.join("etc/resolv.conf").exists());

        let passwd = fs::read_to_string(rootfs_path.join("etc/passwd")).unwrap();
        assert!(passwd.contains("root:x:0:0"));
    }

    #[cfg(unix)]
    #[test]
    fn test_oci_rootfs_builder_makes_essential_files_world_readable() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");
        let builder = OciRootfsBuilder::new(&rootfs_path);
        let essential_files = ["passwd", "group", "hosts", "resolv.conf", "nsswitch.conf"];

        fs::create_dir_all(rootfs_path.join("etc")).unwrap();
        for name in essential_files {
            let path = rootfs_path.join("etc").join(name);
            fs::write(&path, "image content\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }

        builder.create_essential_files().unwrap();

        for name in essential_files {
            let mode = fs::metadata(rootfs_path.join("etc").join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o644, "unexpected mode for /etc/{name}");
        }
    }

    #[test]
    fn test_oci_rootfs_builder_extracts_image_at_root() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");
        let image = temp_dir.path().join("image");

        create_test_oci_image_with_file(&image, "app/main.py", b"print('hello')");

        OciRootfsBuilder::new(&rootfs_path)
            .with_image(&image)
            .build()
            .unwrap();

        // File extracted at rootfs root, not under /agent
        let extracted = rootfs_path.join("app/main.py");
        assert!(extracted.exists());
        let content = fs::read_to_string(extracted).unwrap();
        assert_eq!(content, "print('hello')");
    }

    #[test]
    fn test_oci_rootfs_builder_no_image_set() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");

        let result = OciRootfsBuilder::new(&rootfs_path).build();

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("image path not set"));
    }

    #[test]
    fn test_oci_rootfs_builder_uses_custom_resolv_conf_and_default_when_empty() {
        let temp_dir = TempDir::new().unwrap();
        let custom_rootfs = temp_dir.path().join("custom-rootfs");
        let default_rootfs = temp_dir.path().join("default-rootfs");
        let image = temp_dir.path().join("image");
        create_test_oci_image(&image);

        OciRootfsBuilder::new(&custom_rootfs)
            .with_image(&image)
            .with_resolv_conf("nameserver 10.0.0.10\nsearch svc.cluster.local\n")
            .build()
            .unwrap();

        assert_eq!(
            fs::read_to_string(custom_rootfs.join("etc/resolv.conf")).unwrap(),
            "nameserver 10.0.0.10\nsearch svc.cluster.local\n"
        );

        OciRootfsBuilder::new(&default_rootfs)
            .with_image(&image)
            .with_resolv_conf("")
            .build()
            .unwrap();

        let resolv_conf = fs::read_to_string(default_rootfs.join("etc/resolv.conf")).unwrap();
        assert!(resolv_conf.contains("nameserver 8.8.8.8"));
        assert!(resolv_conf.contains("nameserver 8.8.4.4"));
    }

    #[test]
    fn test_oci_rootfs_builder_writes_essential_files_inside_absolute_etc_symlink() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");
        let image = temp_dir.path().join("image");

        create_test_oci_image_with_etc_symlink(&image);

        let result = OciRootfsBuilder::new(&rootfs_path)
            .with_image(&image)
            .build();
        let built = match result {
            Ok(()) => true,
            #[cfg(windows)]
            Err(error) => {
                let message = error.to_string();
                assert!(message.contains("ERROR_PRIVILEGE_NOT_HELD (1314)"));
                assert!(message.contains("Developer Mode"));
                assert!(message.contains("flattening the link would corrupt the image"));
                false
            }
            #[cfg(not(windows))]
            Err(error) => panic!("failed to build rootfs: {error}"),
        };

        if built {
            assert!(rootfs_path
                .join("etc")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink());
            assert!(rootfs_path.join("usr/etc/passwd").exists());
            assert!(rootfs_path.join("usr/etc/group").exists());
            assert!(rootfs_path.join("usr/etc/hosts").exists());
            assert!(rootfs_path.join("usr/etc/resolv.conf").exists());
            assert!(rootfs_path.join("usr/etc/nsswitch.conf").exists());
        }
    }

    #[test]
    fn test_rootfs_path_resolves_absolute_and_relative_symlink_hops() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs = temp_dir.path().join("rootfs");
        fs::create_dir_all(rootfs.join("usr")).unwrap();
        fs::create_dir_all(rootfs.join("shared/etc")).unwrap();
        if !create_dir_symlink(Path::new("/usr/etc"), &rootfs.join("etc"))
            || !create_dir_symlink(Path::new("../shared/etc"), &rootfs.join("usr/etc"))
        {
            return;
        }

        let builder = OciRootfsBuilder::new(&rootfs);
        builder.write_file("etc/hosts", "inside\n").unwrap();

        assert_eq!(
            fs::read_to_string(rootfs.join("shared/etc/hosts")).unwrap(),
            "inside\n"
        );
    }

    #[test]
    fn test_rootfs_path_rejects_intermediate_symlink_escape() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs = temp_dir.path().join("rootfs");
        let outside = temp_dir.path().join("outside");
        fs::create_dir_all(&rootfs).unwrap();
        fs::create_dir_all(&outside).unwrap();
        if !create_dir_symlink(Path::new("usr/etc"), &rootfs.join("etc"))
            || !create_dir_symlink(Path::new("../outside"), &rootfs.join("usr"))
        {
            return;
        }

        let error = OciRootfsBuilder::new(&rootfs)
            .write_file("etc/hosts", "escaped\n")
            .unwrap_err()
            .to_string();

        assert!(error.contains("escapes rootfs"), "{error}");
        assert!(!outside.join("etc/hosts").exists());
    }

    #[test]
    fn test_rootfs_path_rejects_final_file_symlink_escape() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs = temp_dir.path().join("rootfs");
        let outside = temp_dir.path().join("outside");
        fs::create_dir_all(rootfs.join("etc")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        if !create_file_symlink(Path::new("../../outside/hosts"), &rootfs.join("etc/hosts")) {
            return;
        }

        let error = OciRootfsBuilder::new(&rootfs)
            .write_file("etc/hosts", "escaped\n")
            .unwrap_err()
            .to_string();

        assert!(error.contains("escapes rootfs"), "{error}");
        assert!(!outside.join("hosts").exists());
    }

    #[test]
    fn test_rootfs_path_rejects_symlink_loop() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs = temp_dir.path().join("rootfs");
        fs::create_dir_all(&rootfs).unwrap();
        if !create_dir_symlink(Path::new("/etc"), &rootfs.join("etc")) {
            return;
        }

        let error = OciRootfsBuilder::new(&rootfs)
            .write_file("etc/hosts", "loop\n")
            .unwrap_err()
            .to_string();

        assert!(error.contains("Too many rootfs symlink hops"), "{error}");
    }

    #[test]
    fn test_replace_guest_file_no_follow_replaces_link_not_target() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs = temp_dir.path().join("rootfs");
        fs::create_dir_all(rootfs.join("shared")).unwrap();
        fs::write(rootfs.join("shared/target"), b"preserve").unwrap();
        if !create_file_symlink(Path::new("shared/target"), &rootfs.join("marker")) {
            return;
        }

        replace_guest_file_no_follow(&rootfs, "marker", b"replacement").unwrap();

        assert_eq!(fs::read(rootfs.join("shared/target")).unwrap(), b"preserve");
        assert_eq!(fs::read(rootfs.join("marker")).unwrap(), b"replacement");
        assert!(!fs::symlink_metadata(rootfs.join("marker"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn test_oci_rootfs_builder_preserves_existing_passwd_and_group_entries() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");
        let image = temp_dir.path().join("image");

        create_test_oci_image_with_files(
            &image,
            &[
                (
                    "etc/passwd",
                    b"root:x:0:0:Root User:/root:/bin/bash\napp:x:1000:1000:App:/app:/bin/sh"
                        .as_slice(),
                ),
                ("etc/group", b"root:x:0:\napp:x:1000:".as_slice()),
            ],
        );

        OciRootfsBuilder::new(&rootfs_path)
            .with_image(&image)
            .build()
            .unwrap();

        let passwd = fs::read_to_string(rootfs_path.join("etc/passwd")).unwrap();
        assert_eq!(entry_count(&passwd, "root"), 1);
        assert!(passwd.contains("root:x:0:0:Root User:/root:/bin/bash\n"));
        assert!(passwd.contains("app:x:1000:1000:App:/app:/bin/sh\n"));
        assert!(passwd.contains("nobody:x:65534:65534:nobody:/:/bin/false\n"));

        let group = fs::read_to_string(rootfs_path.join("etc/group")).unwrap();
        assert_eq!(entry_count(&group, "root"), 1);
        assert!(group.contains("root:x:0:\n"));
        assert!(group.contains("app:x:1000:\n"));
        assert!(group.contains("nogroup:x:65534:\n"));
    }

    #[test]
    fn test_install_guest_init_only_noop_without_guest_init() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");

        OciRootfsBuilder::new(&rootfs_path)
            .install_guest_init_only()
            .unwrap();

        assert!(!rootfs_path.exists());
    }

    #[test]
    fn test_install_guest_init_only_replaces_existing_init_and_sets_permissions() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");
        let guest_init = temp_dir.path().join("guest-init");

        fs::create_dir_all(rootfs_path.join("sbin")).unwrap();
        fs::write(rootfs_path.join("sbin/init"), b"old init").unwrap();
        fs::write(&guest_init, b"new guest init").unwrap();

        OciRootfsBuilder::new(&rootfs_path)
            .with_guest_init(&guest_init)
            .install_guest_init_only()
            .unwrap();

        let installed = rootfs_path.join("sbin/init");
        assert_eq!(fs::read(&installed).unwrap(), b"new guest init");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&installed).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
    }

    #[test]
    fn test_install_guest_init_only_errors_when_source_missing() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");
        let missing_guest_init = temp_dir.path().join("missing-guest-init");

        let err = OciRootfsBuilder::new(&rootfs_path)
            .with_guest_init(&missing_guest_init)
            .install_guest_init_only()
            .unwrap_err();

        assert!(err.to_string().contains("Guest init binary not found"));
    }

    #[test]
    fn test_install_guest_init_resolves_absolute_and_relative_sbin_symlinks_inside_rootfs() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs = temp_dir.path().join("rootfs");
        let guest_init = temp_dir.path().join("guest-init");
        fs::create_dir_all(rootfs.join("usr")).unwrap();
        fs::create_dir_all(rootfs.join("shared/sbin")).unwrap();
        fs::write(&guest_init, b"safe-init").unwrap();
        if !create_dir_symlink(Path::new("/usr/sbin"), &rootfs.join("sbin"))
            || !create_dir_symlink(Path::new("../shared/sbin"), &rootfs.join("usr/sbin"))
        {
            return;
        }

        OciRootfsBuilder::new(&rootfs)
            .with_guest_init(&guest_init)
            .install_guest_init_only()
            .unwrap();

        assert_eq!(
            fs::read(rootfs.join("shared/sbin/init")).unwrap(),
            b"safe-init"
        );
    }

    #[test]
    fn test_install_guest_init_rejects_sbin_symlink_escape() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs = temp_dir.path().join("rootfs");
        let outside = temp_dir.path().join("outside");
        let guest_init = temp_dir.path().join("guest-init");
        fs::create_dir_all(&rootfs).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(&guest_init, b"unsafe-init").unwrap();
        if !create_dir_symlink(Path::new("../outside"), &rootfs.join("sbin")) {
            return;
        }

        let error = OciRootfsBuilder::new(&rootfs)
            .with_guest_init(&guest_init)
            .install_guest_init_only()
            .unwrap_err()
            .to_string();

        assert!(error.contains("escapes rootfs"), "{error}");
        assert!(!outside.join("init").exists());
    }

    #[test]
    fn test_oci_rootfs_builder_image_config_reads_oci_config() {
        let temp_dir = TempDir::new().unwrap();
        let image = temp_dir.path().join("image");
        create_test_oci_image(&image);

        let config = OciRootfsBuilder::new(temp_dir.path().join("rootfs"))
            .with_image(&image)
            .image_config()
            .unwrap();

        assert_eq!(
            config.entrypoint,
            Some(vec!["/usr/local/bin/app".to_string()])
        );
        assert_eq!(config.cmd, None);
        assert_eq!(
            config.env,
            vec![(
                "PATH".to_string(),
                "/usr/local/bin:/usr/bin:/bin".to_string()
            )]
        );
        assert_eq!(config.working_dir, Some("/app".to_string()));
    }

    #[test]
    fn test_install_guest_init_prefers_usr_sbin_when_sbin_missing() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");
        let guest_init = temp_dir.path().join("guest-init");

        fs::create_dir_all(rootfs_path.join("usr").join("sbin")).unwrap();
        fs::write(&guest_init, b"guest-init").unwrap();

        let builder = OciRootfsBuilder {
            rootfs_path: rootfs_path.clone(),
            image_path: PathBuf::new(),
            guest_init_path: Some(guest_init),
            resolv_conf: None,
        };

        builder.install_guest_init().unwrap();

        assert!(rootfs_path.join("usr").join("sbin").join("init").exists());
        assert!(!rootfs_path.join("sbin").exists());
    }

    // Helper: create a minimal test OCI image
    fn create_test_oci_image(path: &Path) {
        create_test_oci_image_with_file(path, "test.txt", b"test content");
    }

    fn create_test_oci_image_with_file(path: &Path, filename: &str, content: &[u8]) {
        create_test_oci_image_with_files(path, &[(filename, content)]);
    }

    fn create_test_oci_image_with_etc_symlink(path: &Path) {
        create_test_oci_image_with_entries(path, &[], Some(("/usr/etc", "etc")));
    }

    #[cfg(unix)]
    fn create_dir_symlink(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).unwrap();
        true
    }

    #[cfg(windows)]
    fn create_dir_symlink(target: &Path, link: &Path) -> bool {
        create_windows_symlink(|| std::os::windows::fs::symlink_dir(target, link))
    }

    #[cfg(unix)]
    fn create_file_symlink(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).unwrap();
        true
    }

    #[cfg(windows)]
    fn create_file_symlink(target: &Path, link: &Path) -> bool {
        create_windows_symlink(|| std::os::windows::fs::symlink_file(target, link))
    }

    #[cfg(windows)]
    fn create_windows_symlink(create: impl FnOnce() -> std::io::Result<()>) -> bool {
        match create() {
            Ok(()) => true,
            Err(error) if error.raw_os_error() == Some(1314) => false,
            Err(error) => panic!("failed to create Windows test symlink: {error}"),
        }
    }

    fn entry_count(content: &str, name: &str) -> usize {
        content
            .lines()
            .filter(|line| line.split(':').next() == Some(name))
            .count()
    }

    fn create_test_oci_image_with_files(path: &Path, files: &[(&str, &[u8])]) {
        create_test_oci_image_with_entries(path, files, None);
    }

    fn test_content_digest(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};

        format!("sha256:{:x}", Sha256::digest(bytes))
    }

    fn test_digest_hex(digest: &str) -> &str {
        digest.strip_prefix("sha256:").unwrap()
    }

    fn write_test_oci_blob(image_path: &Path, bytes: &[u8]) -> String {
        let digest = test_content_digest(bytes);
        fs::write(
            image_path
                .join("blobs/sha256")
                .join(test_digest_hex(&digest)),
            bytes,
        )
        .unwrap();
        digest
    }

    fn create_test_oci_image_with_entries(
        path: &Path,
        files: &[(&str, &[u8])],
        symlink: Option<(&str, &str)>,
    ) {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use tar::Builder;

        fs::create_dir_all(path.join("blobs/sha256")).unwrap();
        fs::write(path.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();

        let layer_path = path.join("fixture-layer.tar.gz");
        {
            let file = fs::File::create(&layer_path).unwrap();
            let encoder = GzEncoder::new(file, Compression::default());
            let mut builder = Builder::new(encoder);

            for (filename, content) in files {
                let mut header = tar::Header::new_gnu();
                header.set_size(content.len() as u64);
                header.set_mode(0o644);
                // uid/gid must be set or a root-side ownership-preserving extraction
                // can't parse the (blank) uid field. Real OCI layers always set them.
                header.set_uid(0);
                header.set_gid(0);
                header.set_cksum();

                builder
                    .append_data(&mut header, *filename, *content)
                    .unwrap();
            }
            if let Some((target, link_name)) = symlink {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_size(0);
                header.set_mode(0o777);
                header.set_uid(0);
                header.set_gid(0);
                header.set_link_name(target).unwrap();
                header.set_cksum();
                builder
                    .append_data(&mut header, link_name, std::io::empty())
                    .unwrap();
            }
            builder.finish().unwrap();
        }
        let layer_content = fs::read(&layer_path).unwrap();
        fs::remove_file(layer_path).unwrap();
        let layer_digest = write_test_oci_blob(path, &layer_content);

        let config_content = r#"{
            "architecture": "amd64",
            "os": "linux",
            "config": {
                "Entrypoint": ["/usr/local/bin/app"],
                "Cmd": null,
                "Env": ["PATH=/usr/local/bin:/usr/bin:/bin"],
                "WorkingDir": "/app"
            },
            "rootfs": {
                "type": "layers",
                "diff_ids": ["sha256:0000000000000000000000000000000000000000000000000000000000000000"]
            },
            "history": []
        }"#;
        let config_digest = write_test_oci_blob(path, config_content.as_bytes());

        let manifest_content = format!(
            r#"{{
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {{
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": "{}",
                "size": {}
            }},
            "layers": [
                {{
                    "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                    "digest": "{}",
                    "size": {}
                }}
            ]
        }}"#,
            config_digest,
            config_content.len(),
            layer_digest,
            layer_content.len()
        );
        let manifest_digest = write_test_oci_blob(path, manifest_content.as_bytes());

        let index_content = format!(
            r#"{{
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [
                {{
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": "{}",
                    "size": {}
                }}
            ]
        }}"#,
            manifest_digest,
            manifest_content.len()
        );
        fs::write(path.join("index.json"), index_content).unwrap();
    }
}
