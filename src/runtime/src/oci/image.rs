//! OCI image parsing and representation.
//!
//! Handles parsing of OCI image layout including manifest and configuration.

use a3s_box_core::error::{BoxError, Result};
use oci_spec::image::{Descriptor, ImageConfiguration, ImageIndex, ImageManifest};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

pub(crate) const MAX_OCI_LAYOUT_BYTES: u64 = 64 * 1024;
pub(crate) const MAX_OCI_INDEX_BYTES: u64 = 4 * 1024 * 1024;
pub(crate) const MAX_OCI_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_OCI_CONFIG_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_OCI_LAYER_BLOB_BYTES: u64 = 16 * 1024 * 1024 * 1024;

/// Validate the only digest form accepted for local OCI blob paths.
///
/// Requiring lowercase canonical SHA-256 makes the returned value safe as one
/// path component and rejects alternate algorithms, separators, and `..`.
pub(crate) fn canonical_sha256_digest_hex(digest: &str) -> Result<&str> {
    digest
        .strip_prefix("sha256:")
        .filter(|hex| {
            hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .ok_or_else(|| {
            BoxError::OciImageError(format!(
                "malformed content digest (expected canonical OCI sha256:<64 lowercase hex>): {digest:?}"
            ))
        })
}

/// Reject symlink/reparse-backed directories before walking an OCI layout.
pub(crate) fn validate_plain_directory(path: &Path, what: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        BoxError::OciImageError(format!(
            "Failed to inspect {what} directory {}: {error}",
            path.display()
        ))
    })?;

    #[cfg(windows)]
    let is_link_or_reparse = {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        metadata.file_type().is_symlink()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    };
    #[cfg(not(windows))]
    let is_link_or_reparse = metadata.file_type().is_symlink();

    if is_link_or_reparse || !metadata.is_dir() {
        return Err(BoxError::OciImageError(format!(
            "refusing {what} directory {} because it is not a plain directory (symlink/reparse or non-directory)",
            path.display()
        )));
    }

    Ok(())
}

fn open_regular_file_no_follow(path: &Path, what: &str) -> Result<File> {
    #[cfg(windows)]
    let opened = a3s_box_core::windows_file::open_regular_file(path, None).map(|(file, _)| file);

    #[cfg(unix)]
    let opened = {
        use std::os::unix::fs::OpenOptionsExt;

        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
    };

    #[cfg(not(any(windows, unix)))]
    let opened = std::fs::File::open(path);

    let file = opened.map_err(|error| {
        BoxError::OciImageError(format!(
            "Failed to open {what} at {} without following links: {error}",
            path.display()
        ))
    })?;
    let metadata = file.metadata().map_err(|error| {
        BoxError::OciImageError(format!(
            "Failed to inspect opened {what} at {}: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(BoxError::OciImageError(format!(
            "refusing {what} at {} because it is not a regular file",
            path.display()
        )));
    }
    Ok(file)
}

fn checked_opened_length(file: &File, path: &Path, what: &str, limit: u64) -> Result<u64> {
    let length = file
        .metadata()
        .map_err(|error| {
            BoxError::OciImageError(format!(
                "Failed to inspect {what} at {}: {error}",
                path.display()
            ))
        })?
        .len();
    if length > limit {
        return Err(BoxError::OciImageError(format!(
            "refusing {what} at {}: {length} bytes exceeds the {limit}-byte limit",
            path.display()
        )));
    }
    Ok(length)
}

/// Read a regular file through a no-follow handle with a hard byte ceiling.
pub(crate) fn read_regular_file_bounded(path: &Path, limit: u64, what: &str) -> Result<Vec<u8>> {
    let file = open_regular_file_no_follow(path, what)?;
    let length = checked_opened_length(&file, path, what, limit)?;
    let capacity = usize::try_from(length).map_err(|_| {
        BoxError::OciImageError(format!(
            "refusing {what} at {}: file length does not fit in memory",
            path.display()
        ))
    })?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(capacity).map_err(|error| {
        BoxError::OciImageError(format!(
            "Failed to reserve memory for {what} at {}: {error}",
            path.display()
        ))
    })?;
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| {
            BoxError::OciImageError(format!(
                "Failed to read {what} at {}: {error}",
                path.display()
            ))
        })?;
    if bytes.len() as u64 > limit {
        return Err(BoxError::OciImageError(format!(
            "refusing {what} at {}: content grew beyond the {limit}-byte limit while reading",
            path.display()
        )));
    }
    Ok(bytes)
}

fn expected_descriptor_size(size: i64, what: &str) -> Result<u64> {
    u64::try_from(size).map_err(|_| {
        BoxError::OciImageError(format!(
            "refusing {what}: descriptor declares a negative size ({size})"
        ))
    })
}

/// Verify descriptor size and SHA-256 against the exact bytes to be consumed.
pub(crate) fn validate_descriptor_bytes(
    digest: &str,
    size: i64,
    bytes: &[u8],
    what: &str,
) -> Result<()> {
    let expected_hex = canonical_sha256_digest_hex(digest)?;
    let expected_size = expected_descriptor_size(size, what)?;
    let actual_size = bytes.len() as u64;
    if actual_size != expected_size {
        return Err(BoxError::OciImageError(format!(
            "refusing {what} {digest}: descriptor size {expected_size} does not match actual size {actual_size}"
        )));
    }
    let actual_hex = format!("{:x}", Sha256::digest(bytes));
    if actual_hex != expected_hex {
        return Err(BoxError::OciImageError(format!(
            "refusing {what} {digest}: descriptor digest does not match actual bytes (sha256:{actual_hex})"
        )));
    }
    Ok(())
}

fn blob_path(root_dir: &Path, digest: &str) -> Result<PathBuf> {
    let hex = canonical_sha256_digest_hex(digest)?;
    Ok(root_dir.join("blobs").join("sha256").join(hex))
}

pub(crate) fn read_verified_oci_blob(
    root_dir: &Path,
    digest: &str,
    size: i64,
    limit: u64,
    what: &str,
) -> Result<Vec<u8>> {
    canonical_sha256_digest_hex(digest)?;
    let expected_size = expected_descriptor_size(size, what)?;
    if expected_size > limit {
        return Err(BoxError::OciImageError(format!(
            "refusing {what} {digest}: descriptor size {expected_size} exceeds the {limit}-byte limit"
        )));
    }
    let path = blob_path(root_dir, digest)?;
    let bytes = read_regular_file_bounded(&path, limit, what).map_err(|error| {
        BoxError::OciImageError(format!("Failed to read {what} {digest}: {error}"))
    })?;
    validate_descriptor_bytes(digest, size, &bytes, what)?;
    Ok(bytes)
}

fn verify_oci_blob_file(
    root_dir: &Path,
    digest: &str,
    size: i64,
    limit: u64,
    what: &str,
) -> Result<PathBuf> {
    let expected_hex = canonical_sha256_digest_hex(digest)?;
    let expected_size = expected_descriptor_size(size, what)?;
    if expected_size > limit {
        return Err(BoxError::OciImageError(format!(
            "refusing {what} {digest}: descriptor size {expected_size} exceeds the {limit}-byte limit"
        )));
    }

    let path = blob_path(root_dir, digest)?;
    let mut file = open_regular_file_no_follow(&path, what).map_err(|error| {
        BoxError::OciImageError(format!("Failed to open {what} {digest}: {error}"))
    })?;
    let opened_size = checked_opened_length(&file, &path, what, limit)?;
    if opened_size != expected_size {
        return Err(BoxError::OciImageError(format!(
            "refusing {what} {digest}: descriptor size {expected_size} does not match actual size {opened_size}"
        )));
    }

    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| {
            BoxError::OciImageError(format!(
                "Failed to read {what} at {}: {error}",
                path.display()
            ))
        })?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > expected_size {
            return Err(BoxError::OciImageError(format!(
                "refusing {what} {digest}: content grew beyond its descriptor size {expected_size} while reading"
            )));
        }
        hasher.update(&buffer[..read]);
    }
    if total != expected_size {
        return Err(BoxError::OciImageError(format!(
            "refusing {what} {digest}: descriptor size {expected_size} does not match bytes read {total}"
        )));
    }
    let actual_hex = format!("{:x}", hasher.finalize());
    if actual_hex != expected_hex {
        return Err(BoxError::OciImageError(format!(
            "refusing {what} {digest}: descriptor digest does not match actual bytes (sha256:{actual_hex})"
        )));
    }
    Ok(path)
}

/// Health check configuration from OCI image config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OciHealthCheck {
    pub test: Vec<String>,
    pub interval: Option<u64>,
    pub timeout: Option<u64>,
    pub retries: Option<u32>,
    pub start_period: Option<u64>,
}

impl OciHealthCheck {
    /// Whether this Docker-compatible health check contains an executable test.
    ///
    /// `NONE` disables an inherited image health check. Malformed `CMD` and
    /// `CMD-SHELL` arrays without a non-empty command are also non-effective;
    /// unknown non-empty forms are kept fail-closed for forward compatibility.
    pub fn is_enabled(&self) -> bool {
        let Some(marker) = self.test.first() else {
            return false;
        };
        if marker.eq_ignore_ascii_case("NONE") {
            return false;
        }
        if marker.eq_ignore_ascii_case("CMD") || marker.eq_ignore_ascii_case("CMD-SHELL") {
            return self
                .test
                .get(1..)
                .is_some_and(|command| command.iter().any(|part| !part.trim().is_empty()));
        }
        self.test.iter().any(|part| !part.trim().is_empty())
    }
}

/// Represents an OCI image loaded from disk.
#[derive(Debug)]
pub struct OciImage {
    /// Root directory of the OCI image layout
    root_dir: PathBuf,

    /// Manifest digest (e.g. "sha256:abc123...")
    manifest_digest: String,

    /// Image configuration
    config: OciImageConfig,

    /// Paths to layer blobs (in order, bottom to top)
    layer_paths: Vec<PathBuf>,
}

/// Parsed OCI image configuration with entrypoint and environment.
#[derive(Debug, Clone)]
pub struct OciImageConfig {
    /// Entrypoint command
    pub entrypoint: Option<Vec<String>>,

    /// Default command arguments
    pub cmd: Option<Vec<String>>,

    /// Environment variables
    pub env: Vec<(String, String)>,

    /// Working directory
    pub working_dir: Option<String>,

    /// User to run as
    pub user: Option<String>,

    /// Exposed ports
    pub exposed_ports: Vec<String>,

    /// Labels
    pub labels: std::collections::HashMap<String, String>,

    /// Volumes declared in the image (OCI VOLUME directive)
    pub volumes: Vec<String>,

    /// Stop signal
    pub stop_signal: Option<String>,

    /// Health check configuration
    pub health_check: Option<OciHealthCheck>,

    /// ONBUILD triggers
    pub onbuild: Vec<String>,
}

impl OciImage {
    /// Load an OCI image from a directory.
    ///
    /// The directory must contain a valid OCI image layout:
    /// - oci-layout file
    /// - index.json
    /// - blobs/sha256/ directory with manifest, config, and layers
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the OCI image directory
    ///
    /// # Errors
    ///
    /// Returns error if:
    /// - Directory doesn't exist
    /// - OCI layout is invalid
    /// - Manifest or config cannot be parsed
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let root_dir = path.as_ref().to_path_buf();

        // Validate OCI layout
        Self::validate_oci_layout(&root_dir)?;

        // Load index.json
        let index = Self::load_index(&root_dir)?;

        // Get the manifest descriptor from index. Its digest and size are both
        // authenticated before the manifest bytes are parsed.
        let manifest_descriptor = index
            .manifests()
            .first()
            .ok_or_else(|| BoxError::OciImageError("No manifests in index.json".to_string()))?;
        let manifest_digest = manifest_descriptor.digest().to_string();

        // Load manifest
        let manifest = Self::load_manifest(&root_dir, manifest_descriptor)?;

        // Load config
        let config = Self::load_config(&root_dir, manifest.config())?;

        // Verify every layer through a no-follow handle before exposing paths
        // that extraction will subsequently consume.
        let layer_paths = manifest
            .layers()
            .iter()
            .map(|layer| {
                verify_oci_blob_file(
                    &root_dir,
                    layer.digest(),
                    layer.size(),
                    MAX_OCI_LAYER_BLOB_BYTES,
                    "layer blob",
                )
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            root_dir,
            manifest_digest,
            config,
            layer_paths,
        })
    }

    /// Get the image configuration.
    pub fn config(&self) -> &OciImageConfig {
        &self.config
    }

    /// Get paths to all layer blobs (in order, bottom to top).
    pub fn layer_paths(&self) -> &[PathBuf] {
        &self.layer_paths
    }

    /// Get the root directory of the OCI image.
    pub fn root_dir(&self) -> &Path {
        &self.root_dir
    }

    /// Get the manifest digest (e.g. `"sha256:abc123..."`).
    pub fn manifest_digest(&self) -> &str {
        &self.manifest_digest
    }

    /// Get the entrypoint command.
    ///
    /// Returns the entrypoint from config, or None if not set.
    pub fn entrypoint(&self) -> Option<&[String]> {
        self.config.entrypoint.as_deref()
    }

    /// Get the default command arguments.
    pub fn cmd(&self) -> Option<&[String]> {
        self.config.cmd.as_deref()
    }

    /// Get environment variables.
    pub fn env(&self) -> &[(String, String)] {
        &self.config.env
    }

    /// Get the working directory.
    pub fn working_dir(&self) -> Option<&str> {
        self.config.working_dir.as_deref()
    }

    /// Get a label value by key.
    pub fn label(&self, key: &str) -> Option<&str> {
        self.config.labels.get(key).map(|s| s.as_str())
    }

    /// Validate that the directory contains a valid OCI layout.
    fn validate_oci_layout(root_dir: &Path) -> Result<()> {
        validate_plain_directory(root_dir, "OCI image root")?;

        // Open the top-level metadata without following links. Reading it here
        // also prevents oversized metadata from passing an existence-only check.
        let oci_layout_path = root_dir.join("oci-layout");
        read_regular_file_bounded(&oci_layout_path, MAX_OCI_LAYOUT_BYTES, "oci-layout")?;

        let index_path = root_dir.join("index.json");
        read_regular_file_bounded(&index_path, MAX_OCI_INDEX_BYTES, "index.json")?;

        let blobs_dir = root_dir.join("blobs");
        validate_plain_directory(&blobs_dir, "OCI blobs")?;
        validate_plain_directory(&blobs_dir.join("sha256"), "OCI sha256 blobs")?;

        Ok(())
    }

    /// Load the image index from index.json.
    fn load_index(root_dir: &Path) -> Result<ImageIndex> {
        let index_path = root_dir.join("index.json");
        let content = read_regular_file_bounded(&index_path, MAX_OCI_INDEX_BYTES, "index.json")?;

        serde_json::from_slice(&content)
            .map_err(|e| BoxError::OciImageError(format!("Failed to parse index.json: {}", e)))
    }

    /// Load the image manifest from blobs.
    fn load_manifest(root_dir: &Path, descriptor: &Descriptor) -> Result<ImageManifest> {
        let content = read_verified_oci_blob(
            root_dir,
            descriptor.digest(),
            descriptor.size(),
            MAX_OCI_MANIFEST_BYTES,
            "manifest blob",
        )?;

        serde_json::from_slice(&content)
            .map_err(|e| BoxError::OciImageError(format!("Failed to parse manifest: {}", e)))
    }

    /// Load the image configuration from blobs.
    fn load_config(root_dir: &Path, descriptor: &Descriptor) -> Result<OciImageConfig> {
        let content = read_verified_oci_blob(
            root_dir,
            descriptor.digest(),
            descriptor.size(),
            MAX_OCI_CONFIG_BYTES,
            "config blob",
        )?;

        let oci_config: ImageConfiguration = serde_json::from_slice(&content)
            .map_err(|e| BoxError::OciImageError(format!("Failed to parse config: {}", e)))?;

        let raw_config: serde_json::Value = serde_json::from_slice(&content)
            .map_err(|e| BoxError::OciImageError(format!("Failed to parse config JSON: {}", e)))?;

        // oci-spec 0.6 does not model OnBuild or Healthcheck, so parse those
        // Docker-compatible image fields directly from raw JSON.
        let onbuild: Vec<String> = raw_config
            .get("config")
            .and_then(|c| c.get("OnBuild"))
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        let health_check = Self::parse_health_check_from_raw(&raw_config);

        let mut config = OciImageConfig::from_oci_config(&oci_config, onbuild);
        config.health_check = health_check;
        Ok(config)
    }

    /// Parse Docker-compatible Healthcheck metadata from raw image config JSON.
    fn parse_health_check_from_raw(raw_config: &serde_json::Value) -> Option<OciHealthCheck> {
        let health = raw_config
            .get("config")
            .and_then(|c| c.get("Healthcheck").or_else(|| c.get("healthcheck")))?;

        let test = health.get("Test").or_else(|| health.get("test"))?;
        let test: Vec<String> = serde_json::from_value(test.clone()).ok()?;
        if test.is_empty() {
            return None;
        }

        if test
            .first()
            .is_some_and(|marker| marker.eq_ignore_ascii_case("NONE"))
        {
            return None;
        }

        Some(OciHealthCheck {
            test,
            interval: health
                .get("Interval")
                .or_else(|| health.get("interval"))
                .and_then(duration_seconds_from_json),
            timeout: health
                .get("Timeout")
                .or_else(|| health.get("timeout"))
                .and_then(duration_seconds_from_json),
            retries: health
                .get("Retries")
                .or_else(|| health.get("retries"))
                .and_then(u32_from_json)
                .filter(|value| *value > 0),
            start_period: health
                .get("StartPeriod")
                .or_else(|| health.get("start_period"))
                .and_then(duration_seconds_from_json),
        })
    }
}

fn duration_seconds_from_json(value: &serde_json::Value) -> Option<u64> {
    let nanos = u64_from_json(value)?;
    if nanos == 0 {
        return None;
    }
    Some(nanos.div_ceil(1_000_000_000).max(1))
}

fn u64_from_json(value: &serde_json::Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse::<u64>().ok()))
}

fn u32_from_json(value: &serde_json::Value) -> Option<u32> {
    u64_from_json(value).and_then(|value| u32::try_from(value).ok())
}

impl OciImageConfig {
    /// Create from OCI spec ImageConfiguration.
    fn from_oci_config(oci_config: &ImageConfiguration, onbuild: Vec<String>) -> Self {
        let config = oci_config.config();

        let entrypoint = config.as_ref().and_then(|c| c.entrypoint().clone());
        let cmd = config.as_ref().and_then(|c| c.cmd().clone());
        let working_dir = config.as_ref().and_then(|c| c.working_dir().clone());
        let user = config.as_ref().and_then(|c| c.user().clone());

        // Parse environment variables
        let env = config
            .as_ref()
            .and_then(|c| c.env().as_ref())
            .map(|env_list| {
                env_list
                    .iter()
                    .filter_map(|e| {
                        let parts: Vec<&str> = e.splitn(2, '=').collect();
                        if parts.len() == 2 {
                            Some((parts[0].to_string(), parts[1].to_string()))
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Parse exposed ports
        let exposed_ports = config
            .as_ref()
            .and_then(|c| c.exposed_ports().as_ref())
            .map(|ports| ports.to_vec())
            .unwrap_or_default();

        // Parse labels
        let labels = config
            .as_ref()
            .and_then(|c| c.labels().clone())
            .unwrap_or_default();

        // Parse volumes (OCI VOLUME directive)
        let volumes = config
            .as_ref()
            .and_then(|c| c.volumes().as_ref())
            .map(|vols| vols.to_vec())
            .unwrap_or_default();

        // Parse stop signal
        let stop_signal = config.as_ref().and_then(|c| c.stop_signal().clone());

        // Healthcheck is filled by load_config from raw JSON because oci-spec
        // 0.6 does not expose the Docker-compatible field.
        let health_check = None;

        Self {
            entrypoint,
            cmd,
            env,
            working_dir,
            user,
            exposed_ports,
            labels,
            volumes,
            stop_signal,
            health_check,
            onbuild,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_validate_oci_layout_missing_oci_layout_file() {
        let temp_dir = TempDir::new().unwrap();

        let result = OciImage::validate_oci_layout(temp_dir.path());

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("oci-layout"));
    }

    #[test]
    fn test_validate_oci_layout_missing_index_json() {
        let temp_dir = TempDir::new().unwrap();

        // Create oci-layout file
        fs::write(
            temp_dir.path().join("oci-layout"),
            r#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .unwrap();

        let result = OciImage::validate_oci_layout(temp_dir.path());

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("index.json"));
    }

    #[test]
    fn test_validate_oci_layout_missing_blobs() {
        let temp_dir = TempDir::new().unwrap();

        // Create oci-layout file
        fs::write(
            temp_dir.path().join("oci-layout"),
            r#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .unwrap();

        // Create index.json
        fs::write(temp_dir.path().join("index.json"), "{}").unwrap();

        let result = OciImage::validate_oci_layout(temp_dir.path());

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("blobs"));
    }

    #[test]
    fn test_validate_oci_layout_valid() {
        let temp_dir = TempDir::new().unwrap();

        // Create valid OCI layout structure
        create_minimal_oci_layout(temp_dir.path());

        let result = OciImage::validate_oci_layout(temp_dir.path());

        assert!(result.is_ok());
    }

    #[test]
    fn test_blob_path() {
        let root = PathBuf::from("/images/test");
        let digest = format!("sha256:{}", "a".repeat(64));

        let path = blob_path(&root, &digest).unwrap();
        assert_eq!(
            path,
            PathBuf::from(format!("/images/test/blobs/sha256/{}", "a".repeat(64)))
        );

        assert!(blob_path(&root, "abc123").is_err());
        assert!(blob_path(
            &root,
            "sha256:../../../../../../../../windows/system32/drivers/etc/hosts"
        )
        .is_err());
    }

    #[test]
    fn test_from_path_valid_image() {
        let temp_dir = TempDir::new().unwrap();

        // Create a complete OCI image layout
        create_complete_oci_image(temp_dir.path());

        let image = OciImage::from_path(temp_dir.path()).unwrap();

        // Verify config was parsed
        assert_eq!(image.entrypoint(), Some(&["/bin/agent".to_string()][..]));
        assert_eq!(
            image.cmd(),
            Some(&["--port".to_string(), "8080".to_string()][..])
        );
        assert_eq!(image.working_dir(), Some("/workspace"));

        // Verify env was parsed
        let env = image.env();
        assert!(env
            .iter()
            .any(|(k, v)| k == "PATH" && v.contains("/usr/bin")));

        // Verify labels
        assert_eq!(image.label("a3s.type"), Some("agent"));

        // Verify Docker-compatible Healthcheck was parsed from raw config JSON.
        let health_check = image.config().health_check.as_ref().unwrap();
        assert_eq!(
            health_check.test,
            vec!["CMD-SHELL".to_string(), "test -f /tmp/healthy".to_string()]
        );
        assert_eq!(health_check.interval, Some(30));
        assert_eq!(health_check.timeout, Some(2));
        assert_eq!(health_check.retries, Some(2));
        assert_eq!(health_check.start_period, Some(5));

        // Verify layer paths
        assert_eq!(image.layer_paths().len(), 1);
    }

    #[test]
    fn test_from_path_exposes_manifest_digest() {
        let temp_dir = TempDir::new().unwrap();
        let layout = create_complete_oci_image(temp_dir.path());
        let image = OciImage::from_path(temp_dir.path()).unwrap();
        assert_eq!(image.manifest_digest(), layout.manifest_digest);
    }

    #[test]
    fn test_from_path_nonexistent() {
        let result = OciImage::from_path("/nonexistent/path");

        assert!(result.is_err());
    }

    #[test]
    fn test_from_path_rejects_oversized_index() {
        let temp_dir = TempDir::new().unwrap();
        create_minimal_oci_layout(temp_dir.path());
        fs::File::create(temp_dir.path().join("index.json"))
            .unwrap()
            .set_len(MAX_OCI_INDEX_BYTES + 1)
            .unwrap();

        let error = OciImage::from_path(temp_dir.path()).unwrap_err();
        assert!(error.to_string().contains("limit"), "{error}");
    }

    #[test]
    fn test_from_path_rejects_blob_digest_mismatch() {
        let temp_dir = TempDir::new().unwrap();
        let layout = create_complete_oci_image(temp_dir.path());
        let config_path = temp_dir
            .path()
            .join("blobs/sha256")
            .join(digest_hex(&layout.config_digest));
        let mut content = fs::read(&config_path).unwrap();
        content[0] ^= 1;
        fs::write(config_path, content).unwrap();

        let error = OciImage::from_path(temp_dir.path()).unwrap_err();
        assert!(error.to_string().contains("digest"), "{error}");
    }

    #[test]
    fn test_from_path_rejects_noncanonical_manifest_digest() {
        let temp_dir = TempDir::new().unwrap();
        create_minimal_oci_layout(temp_dir.path());
        fs::write(
            temp_dir.path().join("index.json"),
            r#"{"schemaVersion":2,"manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:../../../../outside","size":0}]}"#,
        )
        .unwrap();

        assert!(OciImage::from_path(temp_dir.path()).is_err());
    }

    #[test]
    fn test_from_path_rejects_index_symlink() {
        let temp_dir = TempDir::new().unwrap();
        let layout = temp_dir.path().join("layout");
        fs::create_dir_all(layout.join("blobs/sha256")).unwrap();
        fs::write(
            layout.join("oci-layout"),
            r#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .unwrap();
        let outside = temp_dir.path().join("outside-index.json");
        fs::write(&outside, "{}").unwrap();
        if !symlink_file_for_test(&outside, &layout.join("index.json")) {
            return;
        }

        let error = OciImage::from_path(&layout).unwrap_err();
        assert!(error.to_string().contains("index.json"), "{error}");
    }

    #[test]
    fn test_from_path_rejects_layer_blob_symlink() {
        let temp_dir = TempDir::new().unwrap();
        let layout = create_complete_oci_image(temp_dir.path());
        let layer_path = temp_dir
            .path()
            .join("blobs/sha256")
            .join(digest_hex(&layout.layer_digest));
        let layer_content = fs::read(&layer_path).unwrap();
        let outside = temp_dir.path().join("outside-layer.tar.gz");
        fs::write(&outside, layer_content).unwrap();
        fs::remove_file(&layer_path).unwrap();
        if !symlink_file_for_test(&outside, &layer_path) {
            return;
        }

        let error = OciImage::from_path(temp_dir.path()).unwrap_err();
        assert!(error.to_string().contains("layer blob"), "{error}");
    }

    #[test]
    fn test_validate_layout_rejects_reparse_blobs_directory() {
        let temp_dir = TempDir::new().unwrap();
        let layout = temp_dir.path().join("layout");
        let outside_blobs = temp_dir.path().join("outside-blobs");
        fs::create_dir_all(&layout).unwrap();
        fs::create_dir_all(outside_blobs.join("sha256")).unwrap();
        fs::write(
            layout.join("oci-layout"),
            r#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .unwrap();
        fs::write(layout.join("index.json"), "{}").unwrap();
        if !symlink_dir_for_test(&outside_blobs, &layout.join("blobs")) {
            return;
        }

        let error = OciImage::validate_oci_layout(&layout).unwrap_err();
        assert!(error.to_string().contains("plain directory"), "{error}");
    }

    #[test]
    fn test_from_path_rejects_reparse_root_directory() {
        let temp_dir = TempDir::new().unwrap();
        let target = temp_dir.path().join("target-layout");
        fs::create_dir_all(&target).unwrap();
        create_complete_oci_image(&target);
        let linked_root = temp_dir.path().join("linked-layout");
        if !symlink_dir_for_test(&target, &linked_root) {
            return;
        }

        let error = OciImage::from_path(linked_root).unwrap_err();
        assert!(error.to_string().contains("plain directory"), "{error}");
    }

    #[cfg(unix)]
    fn symlink_file_for_test(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).unwrap();
        true
    }

    #[cfg(windows)]
    fn symlink_file_for_test(target: &Path, link: &Path) -> bool {
        windows_symlink_for_test(|| std::os::windows::fs::symlink_file(target, link))
    }

    #[cfg(not(any(unix, windows)))]
    fn symlink_file_for_test(_target: &Path, _link: &Path) -> bool {
        false
    }

    #[cfg(unix)]
    fn symlink_dir_for_test(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).unwrap();
        true
    }

    #[cfg(windows)]
    fn symlink_dir_for_test(target: &Path, link: &Path) -> bool {
        windows_symlink_for_test(|| std::os::windows::fs::symlink_dir(target, link))
    }

    #[cfg(not(any(unix, windows)))]
    fn symlink_dir_for_test(_target: &Path, _link: &Path) -> bool {
        false
    }

    #[cfg(windows)]
    fn windows_symlink_for_test(create: impl FnOnce() -> std::io::Result<()>) -> bool {
        match create() {
            Ok(()) => true,
            Err(error) if error.raw_os_error() == Some(1314) => false,
            Err(error) => panic!("failed to create Windows test symlink: {error}"),
        }
    }

    // Helper function to create minimal OCI layout structure
    fn create_minimal_oci_layout(path: &Path) {
        fs::write(path.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();

        fs::write(path.join("index.json"), "{}").unwrap();

        fs::create_dir_all(path.join("blobs/sha256")).unwrap();
    }

    #[derive(Debug)]
    struct CompleteLayout {
        manifest_digest: String,
        config_digest: String,
        layer_digest: String,
    }

    fn sha256_digest(bytes: &[u8]) -> String {
        format!("sha256:{:x}", Sha256::digest(bytes))
    }

    fn digest_hex(digest: &str) -> &str {
        digest.strip_prefix("sha256:").unwrap()
    }

    fn write_blob(path: &Path, bytes: &[u8]) -> String {
        let digest = sha256_digest(bytes);
        fs::write(path.join("blobs/sha256").join(digest_hex(&digest)), bytes).unwrap();
        digest
    }

    // Helper function to create a complete, cryptographically consistent OCI image.
    fn create_complete_oci_image(path: &Path) -> CompleteLayout {
        // Create directory structure
        fs::create_dir_all(path.join("blobs/sha256")).unwrap();

        // Create oci-layout
        fs::write(path.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();

        // Create config blob
        let config_content = r#"{
            "architecture": "amd64",
            "os": "linux",
            "config": {
                "Entrypoint": ["/bin/agent"],
                "Cmd": ["--port", "8080"],
                "Env": ["PATH=/usr/local/bin:/usr/bin:/bin"],
                "WorkingDir": "/workspace",
                "Labels": {
                    "a3s.type": "agent",
                    "a3s.version": "1.0.0"
                },
                "Healthcheck": {
                    "Test": ["CMD-SHELL", "test -f /tmp/healthy"],
                    "Interval": 30000000000,
                    "Timeout": 1500000000,
                    "Retries": 2,
                    "StartPeriod": 5000000000
                }
            },
            "rootfs": {
                "type": "layers",
                "diff_ids": ["sha256:0000000000000000000000000000000000000000000000000000000000000000"]
            },
            "history": []
        }"#;
        let config_digest = write_blob(path, config_content.as_bytes());

        // Create layer blob (minimal tar.gz for testing).
        let layer_build_path = path.join("fixture-layer.tar.gz");
        create_test_layer(&layer_build_path);
        let layer_content = fs::read(&layer_build_path).unwrap();
        fs::remove_file(layer_build_path).unwrap();
        let layer_digest = write_blob(path, &layer_content);

        // Create manifest blob
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
        let manifest_digest = write_blob(path, manifest_content.as_bytes());

        // Create index.json
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

        CompleteLayout {
            manifest_digest,
            config_digest,
            layer_digest,
        }
    }

    #[test]
    fn test_from_oci_config_parses_volumes() {
        // Directly test OciImageConfig::from_oci_config with volumes
        let config_json = r#"{
            "architecture": "amd64",
            "os": "linux",
            "config": {
                "Volumes": {
                    "/data": {},
                    "/var/log": {}
                }
            },
            "rootfs": {
                "type": "layers",
                "diff_ids": []
            },
            "history": []
        }"#;
        let oci_config: oci_spec::image::ImageConfiguration =
            serde_json::from_str(config_json).unwrap();
        let config = OciImageConfig::from_oci_config(&oci_config, Vec::new());
        assert_eq!(config.volumes.len(), 2);
        assert!(config.volumes.contains(&"/data".to_string()));
        assert!(config.volumes.contains(&"/var/log".to_string()));
    }

    #[test]
    fn test_from_oci_config_no_volumes() {
        let config_json = r#"{
            "architecture": "amd64",
            "os": "linux",
            "config": {},
            "rootfs": {
                "type": "layers",
                "diff_ids": []
            },
            "history": []
        }"#;
        let oci_config: oci_spec::image::ImageConfiguration =
            serde_json::from_str(config_json).unwrap();
        let config = OciImageConfig::from_oci_config(&oci_config, Vec::new());
        assert!(config.volumes.is_empty());
    }

    #[test]
    fn test_parse_health_check_cmd() {
        let raw = serde_json::json!({
            "config": {
                "Healthcheck": {
                    "Test": ["CMD", "curl", "-f", "http://localhost/"],
                    "Interval": 30000000000u64,
                    "Timeout": 5000000000u64,
                    "Retries": 3u64,
                    "StartPeriod": 0u64
                }
            }
        });

        let hc = OciImage::parse_health_check_from_raw(&raw).unwrap();
        assert_eq!(hc.test, vec!["CMD", "curl", "-f", "http://localhost/"]);
        assert_eq!(hc.interval, Some(30));
        assert_eq!(hc.timeout, Some(5));
        assert_eq!(hc.retries, Some(3));
        assert_eq!(hc.start_period, None);
    }

    #[test]
    fn test_parse_health_check_cmd_shell_and_ceil_durations() {
        let raw = serde_json::json!({
            "config": {
                "Healthcheck": {
                    "Test": ["CMD-SHELL", "wget -qO- http://localhost/health"],
                    "Interval": 1500000000u64,
                    "Timeout": "1",
                    "Retries": "2",
                    "StartPeriod": 1u64
                }
            }
        });

        let hc = OciImage::parse_health_check_from_raw(&raw).unwrap();
        assert_eq!(
            hc.test,
            vec!["CMD-SHELL", "wget -qO- http://localhost/health"]
        );
        assert_eq!(hc.interval, Some(2));
        assert_eq!(hc.timeout, Some(1));
        assert_eq!(hc.retries, Some(2));
        assert_eq!(hc.start_period, Some(1));
    }

    #[test]
    fn test_parse_health_check_none_disables() {
        let raw = serde_json::json!({
            "config": {
                "Healthcheck": {
                    "Test": ["NONE"]
                }
            }
        });

        assert!(OciImage::parse_health_check_from_raw(&raw).is_none());
    }

    #[test]
    fn health_check_enabled_semantics_match_docker_forms() {
        let health_check = |test: &[&str]| OciHealthCheck {
            test: test.iter().map(|part| (*part).to_string()).collect(),
            interval: None,
            timeout: None,
            retries: None,
            start_period: None,
        };

        assert!(health_check(&["CMD", "/bin/true"]).is_enabled());
        assert!(health_check(&["CMD-SHELL", "test -f /ready"]).is_enabled());
        assert!(health_check(&["future-form", "probe"]).is_enabled());
        assert!(!health_check(&[]).is_enabled());
        assert!(!health_check(&["NONE"]).is_enabled());
        assert!(!health_check(&["none", "ignored"]).is_enabled());
        assert!(!health_check(&["CMD"]).is_enabled());
        assert!(!health_check(&["CMD-SHELL", "  "]).is_enabled());
    }

    #[test]
    fn test_load_config_parses_onbuild_triggers() {
        // Verify that OnBuild entries in the raw OCI config JSON are parsed
        // and surfaced in OciImageConfig.onbuild (oci-spec 0.6 doesn't model this field).
        let temp_dir = TempDir::new().unwrap();
        fs::create_dir_all(temp_dir.path().join("blobs/sha256")).unwrap();
        fs::write(
            temp_dir.path().join("oci-layout"),
            r#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .unwrap();

        let config_content = r#"{
            "architecture": "amd64",
            "os": "linux",
            "config": {
                "OnBuild": ["RUN echo hello", "COPY . /app"]
            },
            "rootfs": {"type": "layers", "diff_ids": []},
            "history": []
        }"#;
        let config_digest = write_blob(temp_dir.path(), config_content.as_bytes());

        let manifest_content = format!(
            r#"{{"schemaVersion":2,"config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},"layers":[]}}"#,
            config_digest,
            config_content.len()
        );
        let manifest_digest = write_blob(temp_dir.path(), manifest_content.as_bytes());

        let index_content = format!(
            r#"{{"schemaVersion":2,"manifests":[{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{}","size":{}}}]}}"#,
            manifest_digest,
            manifest_content.len()
        );
        fs::write(temp_dir.path().join("index.json"), index_content).unwrap();

        let image = OciImage::from_path(temp_dir.path()).unwrap();
        assert_eq!(
            image.config().onbuild,
            vec!["RUN echo hello", "COPY . /app"]
        );
    }

    // Helper function to create a test layer (minimal tar.gz)
    fn create_test_layer(path: &Path) {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use tar::Builder;

        let file = fs::File::create(path).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = Builder::new(encoder);

        // Add a simple file
        let mut header = tar::Header::new_gnu();
        header.set_size(5);
        header.set_mode(0o644);
        header.set_cksum();

        builder
            .append_data(&mut header, "test.txt", b"hello" as &[u8])
            .unwrap();
        builder.finish().unwrap();
    }
}
