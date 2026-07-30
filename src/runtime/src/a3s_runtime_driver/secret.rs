//! Transient Runtime Secret material owned by the Box provider boundary.

use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use a3s_runtime::contract::{RuntimeUnitSpec, SecretReference, SecretTarget};
use a3s_runtime::{RuntimeError, RuntimeResult};
use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use zeroize::{Zeroize, Zeroizing};

use crate::local_execution::{TransientRegistryAuthBroker, TransientRegistryAuthLease};
use crate::{BoxRecord, ImagePuller, ImageReference, ImageStore, RegistryAuth};

const MAX_SECRET_BYTES: usize = 1024 * 1024;
const MAX_REGISTRY_USERNAME_BYTES: usize = 255;
const MAX_REGISTRY_PASSWORD_BYTES: usize = 16 * 1024;
const TMPFS_MAGIC: libc::c_long = 0x0102_1994;

/// Provider-neutral Secret resolver supplied by the authenticated caller.
///
/// Box deliberately accepts the shared Runtime [`SecretReference`] rather than
/// a Cloud type. The caller owns authorization and remote transport; Box owns
/// only transient node-local materialization and cleanup. A reference must
/// resolve to the same bytes for the lifetime of one Runtime specification;
/// rotation uses a new reference and therefore a new specification digest.
#[async_trait]
pub trait BoxSecretMaterializer: Send + Sync {
    async fn materialize(
        &self,
        reference: &SecretReference,
    ) -> Result<BoxSecretMaterial, BoxSecretMaterializationError>;

    /// Resolve one registry credential immediately before an uncached pull.
    async fn materialize_registry_credential(
        &self,
        reference: &SecretReference,
        registry: &str,
    ) -> Result<BoxRegistryCredential, BoxSecretMaterializationError>;
}

/// Zeroizing Secret bytes returned across the Box materialization port.
pub struct BoxSecretMaterial(Zeroizing<Vec<u8>>);

impl BoxSecretMaterial {
    pub fn new(value: impl Into<Vec<u8>>) -> Result<Self, BoxSecretMaterializationError> {
        let mut value = value.into();
        if value.is_empty() || value.len() > MAX_SECRET_BYTES {
            value.zeroize();
            return Err(BoxSecretMaterializationError::Rejected(
                "Secret material must contain between 1 byte and 1 MiB".into(),
            ));
        }
        Ok(Self(Zeroizing::new(value)))
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_slice()
    }
}

impl fmt::Debug for BoxSecretMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted-box-secret-material>")
    }
}

/// Zeroizing Basic-auth material used only for one registry pull boundary.
pub struct BoxRegistryCredential {
    username: Zeroizing<String>,
    password: Zeroizing<String>,
}

impl BoxRegistryCredential {
    pub fn new(
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<Self, BoxSecretMaterializationError> {
        let mut username = username.into();
        let mut password = password.into();
        let valid_username =
            valid_registry_field(&username, MAX_REGISTRY_USERNAME_BYTES) && !username.contains(':');
        let valid_password = valid_registry_field(&password, MAX_REGISTRY_PASSWORD_BYTES);
        if !valid_username || !valid_password {
            username.zeroize();
            password.zeroize();
            return Err(BoxSecretMaterializationError::Rejected(
                "Registry credential material is invalid".into(),
            ));
        }
        Ok(Self {
            username: Zeroizing::new(username),
            password: Zeroizing::new(password),
        })
    }

    pub fn username(&self) -> &str {
        self.username.as_str()
    }

    pub fn password(&self) -> &str {
        self.password.as_str()
    }
}

impl fmt::Debug for BoxRegistryCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted-box-registry-credential>")
    }
}

/// Stable, non-sensitive failure categories for caller-provided resolvers.
#[derive(Debug, thiserror::Error)]
pub enum BoxSecretMaterializationError {
    #[error("Secret reference was rejected: {0}")]
    Rejected(String),
    #[error("Secret material is temporarily unavailable: {0}")]
    Unavailable(String),
}

#[derive(Clone)]
pub(super) struct SecretMaterializationOwner {
    root: PathBuf,
    materializer: Option<Arc<dyn BoxSecretMaterializer>>,
}

impl SecretMaterializationOwner {
    pub(super) fn new(root: PathBuf, materializer: Option<Arc<dyn BoxSecretMaterializer>>) -> Self {
        Self { root, materializer }
    }

    pub(super) fn configured(&self) -> bool {
        self.materializer.is_some()
    }

    pub(super) fn require_configured_for(&self, spec: &RuntimeUnitSpec) -> RuntimeResult<()> {
        if !spec.secrets.is_empty() && self.materializer.is_none() {
            return Err(RuntimeError::UnsupportedCapabilities(vec![
                "feature:SecretReferences".into(),
            ]));
        }
        Ok(())
    }

    pub(super) async fn require_ready(&self) -> RuntimeResult<()> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || validate_secret_root(&root))
            .await
            .map_err(|error| {
                RuntimeError::ProviderUnavailable(format!(
                    "Box Secret-root validation task failed: {error}"
                ))
            })?
    }

    pub(super) async fn materialize_for_start(&self, spec: &RuntimeUnitSpec) -> RuntimeResult<()> {
        let container_secrets = spec
            .secrets
            .iter()
            .enumerate()
            .filter(|(_, reference)| !matches!(reference.target, SecretTarget::RegistryCredential))
            .collect::<Vec<_>>();
        if container_secrets.is_empty() {
            return Ok(());
        }
        let materializer = self.materializer.as_ref().ok_or_else(|| {
            RuntimeError::UnsupportedCapabilities(vec!["feature:SecretReferences".into()])
        })?;
        self.require_ready().await?;
        let directory = secret_directory(&self.root, spec)?;
        ensure_private_directory(&directory).await?;

        for (index, reference) in container_secrets {
            let material = match materializer.materialize(reference).await {
                Ok(material) => material,
                Err(error) => {
                    self.cleanup_directory(&directory).await?;
                    return Err(map_materialization_error(error));
                }
            };
            if let Err(error) = validate_material_for_target(material.as_bytes(), &reference.target)
            {
                self.cleanup_directory(&directory).await?;
                return Err(error);
            }
            let path = secret_file(&self.root, spec, index)?;
            if let Err(error) =
                write_secret_atomically(&path, material.as_bytes(), secret_mode(&reference.target))
                    .await
            {
                self.cleanup_directory(&directory).await?;
                return Err(error);
            }
        }
        Ok(())
    }

    pub(super) async fn require_materialized(&self, spec: &RuntimeUnitSpec) -> RuntimeResult<()> {
        if !spec
            .secrets
            .iter()
            .any(|reference| !matches!(reference.target, SecretTarget::RegistryCredential))
        {
            return Ok(());
        }
        self.require_ready().await?;
        for (index, reference) in spec.secrets.iter().enumerate() {
            if matches!(reference.target, SecretTarget::RegistryCredential) {
                continue;
            }
            let path = secret_file(&self.root, spec, index)?;
            let expected_mode = secret_mode(&reference.target);
            tokio::task::spawn_blocking(move || validate_materialized_file(&path, expected_mode))
                .await
                .map_err(|error| {
                    RuntimeError::ProviderUnavailable(format!(
                        "Box Secret-file validation task failed: {error}"
                    ))
                })??;
        }
        Ok(())
    }

    pub(super) async fn resolve_for_redaction(
        &self,
        spec: &RuntimeUnitSpec,
    ) -> RuntimeResult<Vec<BoxSecretMaterial>> {
        if spec.secrets.is_empty() {
            return Ok(Vec::new());
        }
        let mut materials = Vec::with_capacity(spec.secrets.len());
        let materializer = self.materializer.as_ref().ok_or_else(|| {
            RuntimeError::UnsupportedCapabilities(vec!["feature:SecretReferences".into()])
        })?;
        for reference in &spec.secrets {
            if matches!(reference.target, SecretTarget::RegistryCredential) {
                continue;
            }
            let material = materializer
                .materialize(reference)
                .await
                .map_err(map_materialization_error)?;
            validate_material_for_target(material.as_bytes(), &reference.target)?;
            materials.push(material);
        }
        Ok(materials)
    }

    pub(super) async fn prepare_registry_auth_for_start(
        &self,
        spec: &RuntimeUnitSpec,
        record: &BoxRecord,
        home_dir: &Path,
        broker: Option<&TransientRegistryAuthBroker>,
    ) -> RuntimeResult<Option<TransientRegistryAuthLease>> {
        let registry_reference = registry_reference(spec)?;
        let Some(broker) = broker else {
            if registry_reference.is_some() {
                return Err(RuntimeError::UnsupportedCapabilities(vec![
                    "feature:RegistryCredentials".into(),
                ]));
            }
            return Ok(None);
        };
        let metadata = record.managed_execution.as_ref().ok_or_else(|| {
            RuntimeError::Protocol("Box execution lost managed creation metadata".into())
        })?;
        let image = &metadata.request.config.image;
        let auth = match registry_reference {
            Some(reference) if !image_is_cached(home_dir, image).await? => {
                let registry = ImageReference::parse(image)
                    .map_err(|_| {
                        RuntimeError::Protocol(
                            "Box managed artifact has an invalid registry identity".into(),
                        )
                    })?
                    .registry;
                let materializer = self.materializer.as_ref().ok_or_else(|| {
                    RuntimeError::UnsupportedCapabilities(vec!["feature:SecretReferences".into()])
                })?;
                let credential = materializer
                    .materialize_registry_credential(reference, &registry)
                    .await
                    .map_err(map_materialization_error)?;
                RegistryAuth::basic(credential.username(), credential.password())
            }
            Some(_) | None => RegistryAuth::anonymous(),
        };
        broker.bind(&record.id, auth).map(Some).map_err(|error| {
            RuntimeError::ProviderUnavailable(format!(
                "Box transient registry credential handoff failed: {error}"
            ))
        })
    }

    pub(super) async fn cleanup_spec(&self, spec: &RuntimeUnitSpec) -> RuntimeResult<()> {
        let directory = secret_directory(&self.root, spec)?;
        self.cleanup_directory(&directory).await
    }

    pub(super) async fn cleanup_digest(&self, digest: &str) -> RuntimeResult<()> {
        let directory = self.root.join(digest_component(digest)?);
        self.cleanup_directory(&directory).await
    }

    async fn cleanup_directory(&self, directory: &Path) -> RuntimeResult<()> {
        let root = self.root.clone();
        let directory = directory.to_path_buf();
        tokio::task::spawn_blocking(move || remove_secret_directory(&root, &directory))
            .await
            .map_err(|error| {
                RuntimeError::ProviderUnavailable(format!(
                    "Box Secret cleanup task failed: {error}"
                ))
            })?
    }
}

pub(super) fn secret_file(
    root: &Path,
    spec: &RuntimeUnitSpec,
    index: usize,
) -> RuntimeResult<PathBuf> {
    if index >= spec.secrets.len() {
        return Err(RuntimeError::Protocol(
            "Box Secret-file index is outside the Runtime specification".into(),
        ));
    }
    Ok(secret_directory(root, spec)?.join(format!("{index:03}.secret")))
}

pub(super) fn secret_directory(root: &Path, spec: &RuntimeUnitSpec) -> RuntimeResult<PathBuf> {
    let digest = spec.digest().map_err(RuntimeError::InvalidRequest)?;
    Ok(root.join(digest_component(&digest)?))
}

fn digest_component(digest: &str) -> RuntimeResult<&str> {
    let component = digest.strip_prefix("sha256:").ok_or_else(|| {
        RuntimeError::Protocol("Box Secret identity requires a SHA-256 specification digest".into())
    })?;
    if component.len() != 64 || !component.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(RuntimeError::Protocol(
            "Box Secret identity contains an invalid specification digest".into(),
        ));
    }
    Ok(component)
}

fn secret_mode(target: &SecretTarget) -> u32 {
    match target {
        SecretTarget::File { mode, .. } => *mode,
        SecretTarget::Environment { .. } | SecretTarget::RegistryCredential => 0o400,
    }
}

fn validate_material_for_target(bytes: &[u8], target: &SecretTarget) -> RuntimeResult<()> {
    if matches!(target, SecretTarget::Environment { .. })
        && (std::str::from_utf8(bytes).is_err() || bytes.contains(&0))
    {
        return Err(RuntimeError::InvalidRequest(
            "Box Secret environment material must be non-empty UTF-8 without NUL bytes".into(),
        ));
    }
    Ok(())
}

fn valid_registry_field(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
}

fn registry_reference(spec: &RuntimeUnitSpec) -> RuntimeResult<Option<&SecretReference>> {
    let mut references = spec
        .secrets
        .iter()
        .filter(|reference| matches!(reference.target, SecretTarget::RegistryCredential));
    let first = references.next();
    if references.next().is_some() {
        return Err(RuntimeError::InvalidRequest(
            "Box Runtime specification has multiple registry credential Secrets".into(),
        ));
    }
    Ok(first)
}

async fn image_is_cached(home_dir: &Path, reference: &str) -> RuntimeResult<bool> {
    let images = home_dir.join("images");
    let store = ImageStore::new(&images, crate::DEFAULT_IMAGE_CACHE_SIZE).map_err(|error| {
        RuntimeError::ProviderUnavailable(format!(
            "Box image cache could not be inspected before registry authorization: {error}"
        ))
    })?;
    Ok(ImagePuller::new(Arc::new(store), RegistryAuth::anonymous())
        .is_cached(reference)
        .await)
}

fn map_materialization_error(error: BoxSecretMaterializationError) -> RuntimeError {
    match error {
        BoxSecretMaterializationError::Rejected(_) => RuntimeError::InvalidRequest(
            "Box Secret reference was rejected by the caller materializer".into(),
        ),
        BoxSecretMaterializationError::Unavailable(_) => RuntimeError::ProviderUnavailable(
            "Box Secret materializer is temporarily unavailable".into(),
        ),
    }
}

async fn ensure_private_directory(path: &Path) -> RuntimeResult<()> {
    match tokio::fs::create_dir(path).await {
        Ok(()) => {
            tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                .await
                .map_err(secret_io_error)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(secret_io_error(error)),
    }
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || validate_private_directory(&path))
        .await
        .map_err(|error| {
            RuntimeError::ProviderUnavailable(format!(
                "Box Secret-directory validation task failed: {error}"
            ))
        })?
}

async fn write_secret_atomically(path: &Path, bytes: &[u8], mode: u32) -> RuntimeResult<()> {
    let parent = path.parent().ok_or_else(|| {
        RuntimeError::Protocol("Box Secret file has no materialization directory".into())
    })?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("secret"),
        uuid::Uuid::new_v4().simple()
    ));
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .await
        .map_err(secret_io_error)?;
    let result = async {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .await
            .map_err(secret_io_error)?;
        file.write_all(bytes).await.map_err(secret_io_error)?;
        file.flush().await.map_err(secret_io_error)?;
        file.sync_all().await.map_err(secret_io_error)?;
        file.set_permissions(std::fs::Permissions::from_mode(mode))
            .await
            .map_err(secret_io_error)?;
        drop(file);
        tokio::fs::rename(&temporary, path)
            .await
            .map_err(secret_io_error)?;
        sync_directory(parent).await
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result
}

async fn sync_directory(path: &Path) -> RuntimeResult<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        std::fs::File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(secret_io_error)
    })
    .await
    .map_err(|error| {
        RuntimeError::ProviderUnavailable(format!("Box Secret sync task failed: {error}"))
    })?
}

fn validate_secret_root(root: &Path) -> RuntimeResult<()> {
    validate_absolute_normalized(root, "Secret root")?;
    let metadata = std::fs::symlink_metadata(root).map_err(secret_io_error)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(RuntimeError::ProviderUnavailable(
            "Box Secret root is not a plain directory".into(),
        ));
    }
    let canonical = root.canonicalize().map_err(secret_io_error)?;
    if canonical != root {
        return Err(RuntimeError::ProviderUnavailable(
            "Box Secret root must already be canonical and contain no links".into(),
        ));
    }
    if metadata.uid() != unsafe { libc::geteuid() }
        || !matches!(metadata.mode() & 0o7777, 0o700 | 0o710)
    {
        return Err(RuntimeError::ProviderUnavailable(
            "Box Secret root must be provider-owned, non-listable, and inaccessible to other users"
                .into(),
        ));
    }
    let path = std::ffi::CString::new(root.as_os_str().as_bytes())
        .map_err(|_| RuntimeError::InvalidRequest("Box Secret root contains a NUL byte".into()))?;
    let mut status = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::statfs(path.as_ptr(), status.as_mut_ptr()) } != 0 {
        return Err(secret_io_error(std::io::Error::last_os_error()));
    }
    let status = unsafe { status.assume_init() };
    if status.f_type as libc::c_long != TMPFS_MAGIC {
        return Err(RuntimeError::ProviderUnavailable(
            "Box Secret root must be a Linux tmpfs mount".into(),
        ));
    }
    Ok(())
}

fn validate_private_directory(path: &Path) -> RuntimeResult<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(secret_io_error)?;
    if !metadata.file_type().is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || !matches!(metadata.mode() & 0o7777, 0o700 | 0o710)
    {
        return Err(RuntimeError::ProviderUnavailable(
            "Box Secret materialization directory is not a private provider-owned directory".into(),
        ));
    }
    Ok(())
}

fn validate_materialized_file(path: &Path, expected_mode: u32) -> RuntimeResult<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(secret_io_error)?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.nlink() != 1
        || metadata.len() == 0
        || metadata.len() > MAX_SECRET_BYTES as u64
        || metadata.mode() & 0o777 != expected_mode
    {
        return Err(RuntimeError::ProviderUnavailable(
            "Box Secret material is missing or violates its regular-file, size, or mode contract"
                .into(),
        ));
    }
    Ok(())
}

fn remove_secret_directory(root: &Path, directory: &Path) -> RuntimeResult<()> {
    validate_absolute_normalized(root, "Secret root")?;
    if directory.parent() != Some(root) {
        return Err(RuntimeError::Protocol(
            "Box Secret cleanup target escaped its configured root".into(),
        ));
    }
    match std::fs::symlink_metadata(directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(secret_io_error(error)),
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(RuntimeError::ProviderUnavailable(
                "Box Secret cleanup target is not a plain directory".into(),
            ))
        }
    }
    std::fs::remove_dir_all(directory).map_err(secret_io_error)?;
    std::fs::File::open(root)
        .and_then(|directory| directory.sync_all())
        .map_err(secret_io_error)
}

fn validate_absolute_normalized(path: &Path, label: &str) -> RuntimeResult<()> {
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::CurDir | Component::ParentDir | Component::Prefix(_)
            )
        })
    {
        return Err(RuntimeError::InvalidRequest(format!(
            "Box {label} must be an absolute normalized Linux path"
        )));
    }
    Ok(())
}

fn secret_io_error(error: std::io::Error) -> RuntimeError {
    RuntimeError::ProviderUnavailable(format!("Box Secret filesystem operation failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn material_debug_output_never_contains_plaintext() {
        let material = BoxSecretMaterial::new(b"box-secret-fixture".to_vec()).unwrap();
        assert_eq!(format!("{material:?}"), "<redacted-box-secret-material>");
    }

    #[test]
    fn registry_credential_is_bounded_and_redacted() {
        let credential = BoxRegistryCredential::new("registry-user", "registry-password").unwrap();
        assert_eq!(credential.username(), "registry-user");
        assert_eq!(credential.password(), "registry-password");
        assert_eq!(
            format!("{credential:?}"),
            "<redacted-box-registry-credential>"
        );

        for (username, password) in [
            ("", "password"),
            ("user:name", "password"),
            ("username", ""),
            ("username", "password\nleak"),
            ("username", "password\tleak"),
        ] {
            assert!(BoxRegistryCredential::new(username, password).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn private_directory_allows_only_sandbox_search_access() {
        let directory = tempfile::tempdir().unwrap();
        for mode in [0o700, 0o710] {
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(mode))
                .unwrap();
            validate_private_directory(directory.path()).unwrap();
        }

        for mode in [0o711, 0o720, 0o740, 0o770] {
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(mode))
                .unwrap();
            assert!(validate_private_directory(directory.path()).is_err());
        }
    }
}
