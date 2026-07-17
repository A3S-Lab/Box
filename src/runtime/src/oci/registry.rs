//! OCI registry client for pulling and pushing images.
//!
//! Uses the `oci-distribution` crate to interact with container registries
//! (Docker Hub, GHCR, etc.).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use a3s_box_core::error::{BoxError, Result};
use oci_distribution::client::{ClientConfig, ClientProtocol, Config, ImageLayer, PushResponse};
use oci_distribution::errors::{OciDistributionError, OciErrorCode};
use oci_distribution::manifest::{ImageIndexEntry, OciImageManifest, OCI_IMAGE_MEDIA_TYPE};
use oci_distribution::secrets::RegistryAuth as OciRegistryAuth;
use oci_distribution::{Client, Reference};
use oci_reqwest::header::{ACCEPT, CONTENT_LENGTH, CONTENT_TYPE, LOCATION};

use super::credentials::CredentialStore;
use super::reference::ImageReference;
use super::signing::{verify_image_signature, SignaturePolicy, VerifyResult};
use super::store::ImageStore;

mod basic_pull;
mod blob_pull;
mod content;
mod policy;
mod progress;

pub use policy::RegistryPullPolicy;
pub use progress::{PullProgress, PullProgressEventFn, PullProgressState};

use basic_pull::{BasicImageManifest, BasicPullClient};
#[cfg(test)]
use blob_pull::HashingFileWriter;

const REGISTRY_PROTOCOL_ENV: &str = "A3S_REGISTRY_PROTOCOL";
const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json";

/// Transport protocol used for registry operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryProtocol {
    /// Use HTTPS and verify TLS certificates.
    Https,
    /// Use plain HTTP for trusted private registries.
    Http,
}

impl RegistryProtocol {
    /// Return the default protocol, honoring the legacy environment override.
    pub fn from_env() -> Self {
        match std::env::var(REGISTRY_PROTOCOL_ENV) {
            Ok(value) if value.eq_ignore_ascii_case("http") => Self::Http,
            _ => Self::Https,
        }
    }

    fn client_protocol(self) -> ClientProtocol {
        match self {
            Self::Https => ClientProtocol::Https,
            Self::Http => ClientProtocol::Http,
        }
    }

    fn scheme(self) -> &'static str {
        match self {
            Self::Https => "https",
            Self::Http => "http",
        }
    }
}

/// Validate a registry-supplied content digest and return its hex body.
///
/// `oci-distribution` returns the `Docker-Content-Digest` header verbatim with
/// no validation, so a malicious/compromised registry (or a MITM when
/// `A3S_REGISTRY_PROTOCOL=http`) can return e.g. `sha256:../../../../etc/cron.d/x`.
/// That value is used to build on-disk paths (the manifest/blob write, the pull
/// temp dir, the store key); without this check the `..` components make it a
/// path-traversal arbitrary-file write/delete primitive that runs in the default
/// config (signature policy is Skip). Require the canonical
/// `sha256:<64 lowercase hex>` form and reject anything else.
pub(crate) fn validated_digest_hex(digest: &str) -> Result<&str> {
    digest
        .strip_prefix("sha256:")
        .filter(|hex| {
            hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        .ok_or_else(|| {
            BoxError::OciImageError(format!(
                "registry returned a malformed content digest (expected sha256:<64 hex>): {digest:?}"
            ))
        })
}

/// Callback type for layer pull progress: `(current, total, digest, size_bytes)`.
type PullProgressFn = Arc<dyn Fn(usize, usize, &str, i64) + Send + Sync>;

struct PulledImageManifest {
    manifest: OciImageManifest,
    digest: String,
    bytes: Option<Vec<u8>>,
    used_basic: bool,
}

/// Authentication credentials for a container registry.
#[derive(Debug, Clone)]
pub struct RegistryAuth {
    username: Option<String>,
    password: Option<String>,
}

impl RegistryAuth {
    /// Create anonymous authentication (no credentials).
    pub fn anonymous() -> Self {
        Self {
            username: None,
            password: None,
        }
    }

    /// Create basic authentication with username and password.
    pub fn basic(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: Some(username.into()),
            password: Some(password.into()),
        }
    }

    /// Create authentication from environment variables.
    ///
    /// Reads `REGISTRY_USERNAME` and `REGISTRY_PASSWORD`.
    /// Falls back to anonymous if not set.
    pub fn from_env() -> Self {
        let username = std::env::var("REGISTRY_USERNAME").ok();
        let password = std::env::var("REGISTRY_PASSWORD").ok();

        if username.is_some() && password.is_some() {
            Self { username, password }
        } else {
            Self::anonymous()
        }
    }

    /// Create authentication from the credential store, falling back to env vars,
    /// then anonymous.
    pub fn from_credential_store(registry: &str) -> Self {
        if let Ok(store) = CredentialStore::default_path() {
            if let Some(auth) = Self::from_store(&store, registry) {
                return auth;
            }
        }
        Self::from_external_sources(registry)
    }

    /// Create authentication from an explicit A3S home credential store.
    ///
    /// Runtime services can own a home directory without mutating the process
    /// `A3S_HOME`. Registry-specific A3S credentials still take precedence over
    /// supported Docker credentials and environment fallback.
    pub fn from_credential_store_at(home_dir: &Path, registry: &str) -> Self {
        let store = CredentialStore::new(home_dir.join("auth").join("credentials.json"));
        if let Some(auth) = Self::from_store(&store, registry) {
            return auth;
        }
        Self::from_external_sources(registry)
    }

    fn from_store(store: &CredentialStore, registry: &str) -> Option<Self> {
        store
            .get(registry)
            .ok()
            .flatten()
            .map(|(username, password)| Self::basic(username, password))
    }

    fn from_external_sources(registry: &str) -> Self {
        if let Some((username, password)) = super::credentials::docker_credentials(registry) {
            return Self::basic(username, password);
        }
        Self::from_env()
    }

    /// Convert to oci-distribution auth type.
    fn to_oci_auth(&self) -> OciRegistryAuth {
        match (&self.username, &self.password) {
            (Some(u), Some(p)) => OciRegistryAuth::Basic(u.clone(), p.clone()),
            _ => OciRegistryAuth::Anonymous,
        }
    }

    /// Return basic credentials when this auth value is not anonymous.
    pub fn basic_credentials(&self) -> Option<(String, String)> {
        match (&self.username, &self.password) {
            (Some(username), Some(password)) if !username.is_empty() && !password.is_empty() => {
                Some((username.clone(), password.clone()))
            }
            _ => None,
        }
    }
}

/// Pulls OCI images from container registries.
pub(crate) struct RegistryPuller {
    client: Client,
    auth: RegistryAuth,
    protocol: RegistryProtocol,
    target_arch: String,
    /// Signature verification policy (default: Skip).
    signature_policy: SignaturePolicy,
    /// Optional layer progress callback: (current, total, digest, size_bytes).
    progress_fn: Option<PullProgressFn>,
    /// Optional structured progress callback with actual downloaded bytes.
    progress_event_fn: Option<PullProgressEventFn>,
    /// Bounded retry, timeout, and concurrency settings for blob transfers.
    pull_policy: RegistryPullPolicy,
    /// Shared HTTP connection pool for resumable blob requests.
    blob_http: oci_reqwest::Client,
}

impl Default for RegistryPuller {
    fn default() -> Self {
        Self::new()
    }
}

impl RegistryPuller {
    /// Create a new registry puller with anonymous authentication.
    pub fn new() -> Self {
        Self::with_auth(RegistryAuth::anonymous())
    }

    /// Create a new registry puller with the given authentication.
    pub fn with_auth(auth: RegistryAuth) -> Self {
        Self::with_auth_arch_and_protocol(
            auth,
            resolve_target_arch(None),
            RegistryProtocol::from_env(),
        )
    }

    /// Like [`with_auth`](Self::with_auth) but resolves multi-arch image indexes
    /// to an explicit `--platform` (e.g. "linux/arm64") instead of the host
    /// architecture. `None` keeps the host-architecture default.
    pub fn with_auth_and_platform(auth: RegistryAuth, platform: Option<String>) -> Self {
        let Some(platform) = platform else {
            return Self::with_auth(auth);
        };
        let arch = resolve_target_arch(Some(&platform));
        Self::with_auth_arch_and_protocol(auth, arch, RegistryProtocol::from_env())
    }

    fn with_auth_arch_and_protocol(
        auth: RegistryAuth,
        target_arch: String,
        protocol: RegistryProtocol,
    ) -> Self {
        let config = ClientConfig {
            protocol: protocol.client_protocol(),
            platform_resolver: Some(Box::new(platform_resolver_for(target_arch.clone()))),
            ..Default::default()
        };
        let client = Client::new(config);

        Self {
            client,
            auth,
            protocol,
            target_arch,
            signature_policy: SignaturePolicy::default(),
            progress_fn: None,
            progress_event_fn: None,
            pull_policy: RegistryPullPolicy::from_env(),
            blob_http: oci_reqwest::Client::new(),
        }
    }

    /// Set the signature verification policy.
    pub fn with_signature_policy(mut self, policy: SignaturePolicy) -> Self {
        self.signature_policy = policy;
        self
    }

    /// Set a progress callback invoked for each layer: `(current, total, digest, size_bytes)`.
    pub fn with_progress_fn(mut self, f: PullProgressFn) -> Self {
        self.progress_fn = Some(f);
        self
    }

    /// Set a structured progress callback that reports actual downloaded bytes.
    pub fn with_progress_event_fn(mut self, f: PullProgressEventFn) -> Self {
        self.progress_event_fn = Some(f);
        self
    }

    /// Override bounded registry transfer settings.
    pub fn with_pull_policy(mut self, policy: RegistryPullPolicy) -> Self {
        self.pull_policy = policy;
        self
    }

    /// Pull an image and write it as an OCI image layout to `target_dir`.
    ///
    /// The resulting directory will contain:
    /// - `oci-layout`
    /// - `index.json`
    /// - `blobs/sha256/...`
    pub(crate) async fn pull_with_store(
        &self,
        reference: &ImageReference,
        target_dir: &Path,
        blob_store: Option<&ImageStore>,
    ) -> Result<PathBuf> {
        let oci_ref = self.to_oci_reference(reference)?;

        tracing::info!(
            reference = %reference,
            target = %target_dir.display(),
            "Pulling image from registry"
        );

        // Create target directory structure
        let blobs_dir = target_dir.join("blobs").join("sha256");
        std::fs::create_dir_all(&blobs_dir).map_err(|e| BoxError::RegistryError {
            registry: reference.registry.clone(),
            message: format!("Failed to create blobs directory: {}", e),
        })?;

        // Pull manifest (resolves multi-arch image indexes to current platform)
        let pulled_manifest = self
            .pull_image_manifest_with_auth_fallback(reference, &oci_ref)
            .await?;
        let image_manifest = pulled_manifest.manifest;
        let manifest_digest = pulled_manifest.digest;

        // Verify image signature before downloading layers
        let verify_result = verify_image_signature(
            &self.signature_policy,
            &reference.registry,
            &reference.repository,
            &manifest_digest,
        )
        .await;

        if !verify_result.is_ok() {
            return Err(BoxError::RegistryError {
                registry: reference.registry.clone(),
                message: match verify_result {
                    VerifyResult::NoSignature => format!(
                        "Image {}:{} has no signature and policy requires verification",
                        reference.repository,
                        reference.tag.as_deref().unwrap_or("latest")
                    ),
                    VerifyResult::Failed(msg) => format!(
                        "Image signature verification failed for {}:{}: {}",
                        reference.repository,
                        reference.tag.as_deref().unwrap_or("latest"),
                        msg
                    ),
                    _ => "Signature verification failed".to_string(),
                },
            });
        }

        // Write manifest blob. Validate the registry-returned digest first: it is
        // the Docker-Content-Digest header verbatim, and feeding `sha256:../../x`
        // into blobs_dir.join() would write the (attacker-shaped) manifest JSON to
        // an arbitrary host path outside the store.
        let manifest_json = pulled_manifest
            .bytes
            .unwrap_or(serde_json::to_vec(&image_manifest)?);
        let manifest_digest_hex = validated_digest_hex(&manifest_digest)?;
        std::fs::write(blobs_dir.join(manifest_digest_hex), &manifest_json).map_err(|e| {
            BoxError::RegistryError {
                registry: reference.registry.clone(),
                message: format!("Failed to write manifest: {}", e),
            }
        })?;

        // Pull image config and layers
        self.pull_image_content(
            reference,
            &oci_ref,
            &image_manifest,
            &blobs_dir,
            pulled_manifest.used_basic,
            blob_store,
        )
        .await?;

        // Write oci-layout file
        std::fs::write(
            target_dir.join("oci-layout"),
            r#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .map_err(|e| BoxError::RegistryError {
            registry: reference.registry.clone(),
            message: format!("Failed to write oci-layout: {}", e),
        })?;

        // Write index.json
        let index = serde_json::json!({
            "schemaVersion": 2,
            "manifests": [{
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": manifest_digest,
                "size": manifest_json.len()
            }]
        });
        std::fs::write(
            target_dir.join("index.json"),
            serde_json::to_string_pretty(&index)?,
        )
        .map_err(|e| BoxError::RegistryError {
            registry: reference.registry.clone(),
            message: format!("Failed to write index.json: {}", e),
        })?;

        tracing::info!(
            reference = %reference,
            digest = %manifest_digest,
            "Image pulled successfully"
        );

        Ok(target_dir.to_path_buf())
    }

    /// Pull the manifest digest string for an image reference.
    pub async fn pull_manifest_digest(&self, reference: &ImageReference) -> Result<String> {
        let oci_ref = self.to_oci_reference(reference)?;
        let auth = self.auth.to_oci_auth();

        match self.client.pull_manifest(&oci_ref, &auth).await {
            Ok((_manifest, digest)) => {
                validated_digest_hex(&digest)?;
                Ok(digest)
            }
            Err(first_error)
                if is_unauthorized_registry_error(&first_error)
                    && self.auth.basic_credentials().is_some() =>
            {
                tracing::warn!(
                    reference = %reference,
                    error = %registry_error_summary(&first_error, &self.auth),
                    "Registry rejected the default OCI manifest auth flow; retrying with preemptive Basic auth"
                );
                let basic_client = self
                    .basic_pull_client(reference)
                    .map_err(|fallback_error| {
                        self.combined_pull_error(reference, &first_error, &fallback_error)
                    })?;
                basic_client
                    .pull_manifest_digest(reference)
                    .await
                    .map_err(|fallback_error| {
                        self.combined_pull_error(reference, &first_error, &fallback_error)
                    })
            }
            Err(error) => Err(self.pull_error(reference, &error)),
        }
    }

    async fn pull_image_manifest_with_auth_fallback(
        &self,
        reference: &ImageReference,
        oci_ref: &Reference,
    ) -> Result<PulledImageManifest> {
        let auth = self.auth.to_oci_auth();
        match self.client.pull_image_manifest(oci_ref, &auth).await {
            Ok((manifest, digest)) => Ok(PulledImageManifest {
                manifest,
                digest,
                bytes: None,
                used_basic: false,
            }),
            Err(first_error)
                if is_unauthorized_registry_error(&first_error)
                    && self.auth.basic_credentials().is_some() =>
            {
                tracing::warn!(
                    reference = %reference,
                    error = %registry_error_summary(&first_error, &self.auth),
                    "Registry rejected the default OCI manifest auth flow; retrying with preemptive Basic auth"
                );
                let basic_client = self
                    .basic_pull_client(reference)
                    .map_err(|fallback_error| {
                        self.combined_pull_error(reference, &first_error, &fallback_error)
                    })?;
                let BasicImageManifest {
                    manifest,
                    digest,
                    bytes,
                } = basic_client.pull_image_manifest(reference).await.map_err(
                    |fallback_error| {
                        self.combined_pull_error(reference, &first_error, &fallback_error)
                    },
                )?;
                Ok(PulledImageManifest {
                    manifest,
                    digest,
                    bytes: Some(bytes),
                    used_basic: true,
                })
            }
            Err(error) => Err(self.pull_error(reference, &error)),
        }
    }

    fn basic_pull_client(
        &self,
        reference: &ImageReference,
    ) -> std::result::Result<BasicPullClient, OciDistributionError> {
        BasicPullClient::new(
            self.protocol,
            reference,
            &self.auth,
            self.target_arch.clone(),
        )
    }

    fn pull_error(&self, reference: &ImageReference, error: &OciDistributionError) -> BoxError {
        BoxError::RegistryError {
            registry: reference.registry.clone(),
            message: format!(
                "Failed to pull manifest: {}",
                registry_error_summary(error, &self.auth)
            ),
        }
    }

    fn combined_pull_error(
        &self,
        reference: &ImageReference,
        first_error: &OciDistributionError,
        fallback_error: &OciDistributionError,
    ) -> BoxError {
        BoxError::RegistryError {
            registry: reference.registry.clone(),
            message: format!(
                "Failed to pull manifest: default auth failed: {}; preemptive Basic retry failed: {}",
                registry_error_summary(first_error, &self.auth),
                registry_error_summary(fallback_error, &self.auth)
            ),
        }
    }

    /// Convert an ImageReference to an oci-distribution Reference.
    fn to_oci_reference(&self, reference: &ImageReference) -> Result<Reference> {
        let ref_str = if let Some(ref digest) = reference.digest {
            format!("{}/{}@{}", reference.registry, reference.repository, digest)
        } else if let Some(ref tag) = reference.tag {
            format!("{}/{}:{}", reference.registry, reference.repository, tag)
        } else {
            format!("{}/{}:latest", reference.registry, reference.repository)
        };

        ref_str.parse::<Reference>().map_err(|e| {
            BoxError::OciImageError(format!("Invalid OCI reference '{}': {}", ref_str, e))
        })
    }
}

/// Result of a successful image push.
#[derive(Debug, Clone)]
pub struct PushResult {
    /// URL of the pushed config blob.
    pub config_url: String,
    /// URL of the pushed manifest.
    pub manifest_url: String,
    /// Digest of the pushed manifest (e.g., "sha256:abc123...").
    pub manifest_digest: String,
}

struct PushUpload<'a> {
    oci_ref: &'a Reference,
    layers: &'a [ImageLayer],
    config: &'a Config,
    manifest: &'a OciImageManifest,
    manifest_data: &'a [u8],
    expected_manifest_digest: &'a str,
}

/// Pushes OCI images to container registries.
pub struct RegistryPusher {
    client: Client,
    auth: RegistryAuth,
    protocol: RegistryProtocol,
}

impl Default for RegistryPusher {
    fn default() -> Self {
        Self::new()
    }
}

impl RegistryPusher {
    /// Create a new registry pusher with anonymous authentication.
    pub fn new() -> Self {
        Self::with_auth(RegistryAuth::anonymous())
    }

    /// Create a new registry pusher with the given authentication.
    pub fn with_auth(auth: RegistryAuth) -> Self {
        Self::with_auth_and_protocol(auth, RegistryProtocol::from_env())
    }

    /// Create a new registry pusher with explicit authentication and protocol.
    pub fn with_auth_and_protocol(auth: RegistryAuth, protocol: RegistryProtocol) -> Self {
        let config = ClientConfig {
            protocol: protocol.client_protocol(),
            ..Default::default()
        };
        let client = Client::new(config);
        Self {
            client,
            auth,
            protocol,
        }
    }

    /// Push a local OCI image layout to a registry.
    ///
    /// Reads the OCI layout from `image_dir` (index.json → manifest → config + layers),
    /// then pushes all blobs and the manifest to the target registry.
    pub async fn push(&self, reference: &ImageReference, image_dir: &Path) -> Result<PushResult> {
        let oci_ref = self.to_oci_reference(reference)?;

        tracing::info!(
            reference = %reference,
            source = %image_dir.display(),
            "Pushing image to registry"
        );

        // Read index.json to find the manifest digest
        let index_path = image_dir.join("index.json");
        let index_data = std::fs::read_to_string(&index_path)
            .map_err(|e| BoxError::OciImageError(format!("Failed to read index.json: {}", e)))?;
        let index: serde_json::Value = serde_json::from_str(&index_data)?;

        let manifest_digest = index["manifests"][0]["digest"].as_str().ok_or_else(|| {
            BoxError::OciImageError("No manifest digest in index.json".to_string())
        })?;

        // Read manifest blob
        let manifest_digest_hex = manifest_digest
            .strip_prefix("sha256:")
            .unwrap_or(manifest_digest);
        let blobs_dir = image_dir.join("blobs").join("sha256");
        let manifest_data = std::fs::read(blobs_dir.join(manifest_digest_hex))
            .map_err(|e| BoxError::OciImageError(format!("Failed to read manifest blob: {}", e)))?;
        let manifest: OciImageManifest = serde_json::from_slice(&manifest_data)?;

        // Read config blob
        let config_digest_hex = manifest
            .config
            .digest
            .strip_prefix("sha256:")
            .unwrap_or(&manifest.config.digest);
        let config_data = std::fs::read(blobs_dir.join(config_digest_hex))
            .map_err(|e| BoxError::OciImageError(format!("Failed to read config blob: {}", e)))?;
        let config = Config::new(config_data, manifest.config.media_type.clone(), None);

        // Read layer blobs
        let mut layers = Vec::new();
        for layer_desc in &manifest.layers {
            let layer_digest_hex = layer_desc
                .digest
                .strip_prefix("sha256:")
                .unwrap_or(&layer_desc.digest);
            let layer_data = std::fs::read(blobs_dir.join(layer_digest_hex)).map_err(|e| {
                BoxError::OciImageError(format!(
                    "Failed to read layer blob {}: {}",
                    layer_desc.digest, e
                ))
            })?;

            tracing::debug!(
                digest = %layer_desc.digest,
                size = layer_data.len(),
                "Read layer for push"
            );

            layers.push(ImageLayer::new(
                layer_data,
                layer_desc.media_type.clone(),
                None,
            ));
        }

        let response = self
            .push_with_repository_create_retry(
                reference,
                PushUpload {
                    oci_ref: &oci_ref,
                    layers: &layers,
                    config: &config,
                    manifest: &manifest,
                    manifest_data: &manifest_data,
                    expected_manifest_digest: manifest_digest,
                },
            )
            .await?;

        tracing::info!(
            reference = %reference,
            manifest_url = %response.manifest_url,
            "Image pushed successfully"
        );

        Ok(PushResult {
            config_url: response.config_url,
            manifest_url: response.manifest_url,
            manifest_digest: manifest_digest.to_string(),
        })
    }

    /// Convert an ImageReference to an oci-distribution Reference.
    fn to_oci_reference(&self, reference: &ImageReference) -> Result<Reference> {
        let ref_str = if let Some(ref tag) = reference.tag {
            format!("{}/{}:{}", reference.registry, reference.repository, tag)
        } else {
            format!("{}/{}:latest", reference.registry, reference.repository)
        };

        ref_str.parse::<Reference>().map_err(|e| {
            BoxError::OciImageError(format!("Invalid OCI reference '{}': {}", ref_str, e))
        })
    }

    async fn push_with_repository_create_retry(
        &self,
        reference: &ImageReference,
        upload: PushUpload<'_>,
    ) -> Result<PushResponse> {
        let response = match self
            .push_once(
                reference,
                upload.oci_ref,
                upload.layers,
                upload.config,
                upload.manifest,
                upload.manifest_data,
            )
            .await
        {
            Ok(response) => Ok(response),
            Err(first_error) if is_repository_already_exists_push_error(&first_error) => {
                tracing::warn!(
                    reference = %reference,
                    error = %push_error_summary(&first_error),
                    "Registry reported repository already exists during push; retrying once"
                );
                self.push_once(
                    reference,
                    upload.oci_ref,
                    upload.layers,
                    upload.config,
                    upload.manifest,
                    upload.manifest_data,
                )
                .await
                .map_err(|retry_error| BoxError::RegistryError {
                    registry: reference.registry.clone(),
                    message: format!(
                        "Failed to push image after retrying repository creation race: first error: {}; retry error: {}",
                        push_error_summary(&first_error),
                        push_error_summary(&retry_error)
                    ),
                })
            }
            Err(error) => Err(BoxError::RegistryError {
                registry: reference.registry.clone(),
                message: format!("Failed to push image: {}", push_error_summary(&error)),
            }),
        }?;

        self.verify_pushed_manifest(reference, upload.oci_ref, upload.expected_manifest_digest)
            .await?;
        Ok(response)
    }

    async fn push_once(
        &self,
        reference: &ImageReference,
        oci_ref: &Reference,
        layers: &[ImageLayer],
        config: &Config,
        manifest: &OciImageManifest,
        manifest_data: &[u8],
    ) -> std::result::Result<PushResponse, OciDistributionError> {
        let auth = self.auth.to_oci_auth();
        match self
            .client
            .push(
                oci_ref,
                layers,
                config.clone(),
                &auth,
                Some(manifest.clone()),
            )
            .await
        {
            Err(first_error)
                if is_unauthorized_registry_error(&first_error)
                    && self.auth.basic_credentials().is_some() =>
            {
                tracing::warn!(
                    reference = %reference,
                    error = %push_error_summary(&first_error),
                    "Registry rejected the default OCI push auth flow; retrying with preemptive Basic auth"
                );
                self.push_with_preemptive_basic_auth(
                    reference,
                    layers,
                    config,
                    manifest,
                    manifest_data,
                )
                .await
                .map_err(|fallback_error| {
                    OciDistributionError::GenericError(Some(format!(
                        "default push auth failed: {}; preemptive Basic auth retry failed: {}",
                        push_error_summary(&first_error),
                        push_error_summary(&fallback_error)
                    )))
                })
            }
            result => result,
        }
    }

    async fn push_with_preemptive_basic_auth(
        &self,
        reference: &ImageReference,
        layers: &[ImageLayer],
        config: &Config,
        manifest: &OciImageManifest,
        manifest_data: &[u8],
    ) -> std::result::Result<PushResponse, OciDistributionError> {
        let (username, password) = self.auth.basic_credentials().ok_or_else(|| {
            OciDistributionError::GenericError(Some(
                "preemptive Basic auth retry requires non-empty credentials".to_string(),
            ))
        })?;
        let http = oci_reqwest::Client::new();
        let base = registry_base_url(self.protocol, reference)?;

        for (layer, descriptor) in layers.iter().zip(&manifest.layers) {
            push_blob_with_basic_auth(
                &http,
                &base,
                &reference.repository,
                &username,
                &password,
                &descriptor.digest,
                &layer.data,
            )
            .await?;
        }

        let config_url = push_blob_with_basic_auth(
            &http,
            &base,
            &reference.repository,
            &username,
            &password,
            &manifest.config.digest,
            &config.data,
        )
        .await?;

        let manifest_ref = reference
            .tag
            .as_deref()
            .or(reference.digest.as_deref())
            .unwrap_or("latest");
        let manifest_url = registry_manifest_url(&base, &reference.repository, manifest_ref)?;
        let media_type = manifest
            .media_type
            .as_deref()
            .unwrap_or(OCI_IMAGE_MEDIA_TYPE);
        let response = http
            .put(manifest_url.clone())
            .basic_auth(&username, Some(&password))
            .header(CONTENT_TYPE, media_type)
            .body(manifest_data.to_vec())
            .send()
            .await?;
        let response = ensure_registry_status(
            response,
            &[
                oci_reqwest::StatusCode::CREATED,
                oci_reqwest::StatusCode::OK,
            ],
            manifest_url.as_str(),
        )
        .await?;
        let manifest_url = response_location_or_url(&response, &manifest_url)?;

        Ok(PushResponse {
            config_url,
            manifest_url,
        })
    }

    async fn verify_pushed_manifest(
        &self,
        reference: &ImageReference,
        oci_ref: &Reference,
        expected_digest: &str,
    ) -> Result<()> {
        let auth = self.auth.to_oci_auth();
        match self.client.pull_manifest(oci_ref, &auth).await {
            Ok((_manifest, remote_digest)) => {
                verify_remote_manifest_digest(reference, expected_digest, &remote_digest)
            }
            Err(error)
                if is_unauthorized_registry_error(&error)
                    && self.auth.basic_credentials().is_some() =>
            {
                let remote_digest = self
                    .fetch_manifest_digest_with_basic_auth(reference)
                    .await
                    .map_err(|fallback_error| BoxError::RegistryError {
                        registry: reference.registry.clone(),
                        message: format!(
                            "Manifest creation could not be verified after push: default verification failed: {}; preemptive Basic verification failed: {}",
                            push_error_summary(&error),
                            push_error_summary(&fallback_error)
                        ),
                    })?;
                verify_remote_manifest_digest(reference, expected_digest, &remote_digest)
            }
            Err(error) => Err(BoxError::RegistryError {
                registry: reference.registry.clone(),
                message: format!(
                    "Manifest creation could not be verified after push: {}; blobs may have uploaded but the manifest may be missing",
                    push_error_summary(&error)
                ),
            }),
        }
    }

    async fn fetch_manifest_digest_with_basic_auth(
        &self,
        reference: &ImageReference,
    ) -> std::result::Result<String, OciDistributionError> {
        let (username, password) = self.auth.basic_credentials().ok_or_else(|| {
            OciDistributionError::GenericError(Some(
                "preemptive Basic manifest verification requires non-empty credentials".to_string(),
            ))
        })?;
        let http = oci_reqwest::Client::new();
        let base = registry_base_url(self.protocol, reference)?;
        let manifest_ref = reference
            .tag
            .as_deref()
            .or(reference.digest.as_deref())
            .unwrap_or("latest");
        let manifest_url = registry_manifest_url(&base, &reference.repository, manifest_ref)?;
        let response = http
            .get(manifest_url.clone())
            .basic_auth(username, Some(password))
            .header(ACCEPT, MANIFEST_ACCEPT)
            .send()
            .await?;
        let response = ensure_registry_status(
            response,
            &[oci_reqwest::StatusCode::OK],
            manifest_url.as_str(),
        )
        .await?;
        let header_digest = response
            .headers()
            .get("docker-content-digest")
            .map(|value| value.to_str().map(str::to_string))
            .transpose()?;
        if let Some(digest) = header_digest {
            return Ok(digest);
        }

        let bytes = response.bytes().await?;
        Ok(manifest_digest_from_bytes(&bytes))
    }
}

fn registry_base_url(
    protocol: RegistryProtocol,
    reference: &ImageReference,
) -> std::result::Result<oci_reqwest::Url, OciDistributionError> {
    oci_reqwest::Url::parse(&format!("{}://{}", protocol.scheme(), reference.registry))
        .map_err(|e| OciDistributionError::UrlParseError(e.to_string()))
}

fn registry_blob_upload_url(
    base: &oci_reqwest::Url,
    repository: &str,
) -> std::result::Result<oci_reqwest::Url, OciDistributionError> {
    oci_reqwest::Url::parse(&format!(
        "{}/v2/{repository}/blobs/uploads/",
        base.as_str().trim_end_matches('/')
    ))
    .map_err(|e| OciDistributionError::UrlParseError(e.to_string()))
}

fn registry_manifest_url(
    base: &oci_reqwest::Url,
    repository: &str,
    reference: &str,
) -> std::result::Result<oci_reqwest::Url, OciDistributionError> {
    oci_reqwest::Url::parse(&format!(
        "{}/v2/{repository}/manifests/{reference}",
        base.as_str().trim_end_matches('/')
    ))
    .map_err(|e| OciDistributionError::UrlParseError(e.to_string()))
}

fn registry_blob_url(
    base: &oci_reqwest::Url,
    repository: &str,
    digest: &str,
) -> std::result::Result<oci_reqwest::Url, OciDistributionError> {
    oci_reqwest::Url::parse(&format!(
        "{}/v2/{repository}/blobs/{digest}",
        base.as_str().trim_end_matches('/')
    ))
    .map_err(|e| OciDistributionError::UrlParseError(e.to_string()))
}

fn resolve_registry_location(
    base: &oci_reqwest::Url,
    location: &str,
) -> std::result::Result<oci_reqwest::Url, OciDistributionError> {
    oci_reqwest::Url::parse(location)
        .or_else(|_| base.join(location))
        .map_err(|e| OciDistributionError::UrlParseError(e.to_string()))
}

fn append_digest_param(location: &oci_reqwest::Url, digest: &str) -> oci_reqwest::Url {
    let mut url = location.clone();
    url.query_pairs_mut().append_pair("digest", digest);
    url
}

fn response_location_or_url(
    response: &oci_reqwest::Response,
    fallback: &oci_reqwest::Url,
) -> std::result::Result<String, OciDistributionError> {
    response
        .headers()
        .get(LOCATION)
        .map(|value| value.to_str().map(str::to_string))
        .transpose()
        .map(|location| location.unwrap_or_else(|| fallback.as_str().to_string()))
        .map_err(OciDistributionError::HeaderValueError)
}

fn verify_remote_manifest_digest(
    reference: &ImageReference,
    expected_digest: &str,
    remote_digest: &str,
) -> Result<()> {
    if manifest_digests_match(expected_digest, remote_digest) {
        return Ok(());
    }

    Err(BoxError::RegistryError {
        registry: reference.registry.clone(),
        message: format!(
            "Manifest verification failed after push for {}/{}: expected {}, registry returned {}",
            reference.registry, reference.repository, expected_digest, remote_digest
        ),
    })
}

fn manifest_digests_match(expected_digest: &str, remote_digest: &str) -> bool {
    expected_digest.eq_ignore_ascii_case(remote_digest)
}

fn manifest_digest_from_bytes(bytes: &[u8]) -> String {
    use sha2::Digest as _;

    format!("sha256:{:x}", sha2::Sha256::digest(bytes))
}

async fn ensure_registry_status(
    response: oci_reqwest::Response,
    expected: &[oci_reqwest::StatusCode],
    url: &str,
) -> std::result::Result<oci_reqwest::Response, OciDistributionError> {
    let status = response.status();
    if expected.contains(&status) {
        return Ok(response);
    }

    let message = response.text().await.unwrap_or_default();
    if status == oci_reqwest::StatusCode::UNAUTHORIZED {
        return Err(OciDistributionError::UnauthorizedError {
            url: url.to_string(),
        });
    }

    Err(OciDistributionError::ServerError {
        code: status.as_u16(),
        url: url.to_string(),
        message,
    })
}

async fn push_blob_with_basic_auth(
    http: &oci_reqwest::Client,
    base: &oci_reqwest::Url,
    repository: &str,
    username: &str,
    password: &str,
    digest: &str,
    data: &[u8],
) -> std::result::Result<String, OciDistributionError> {
    if data.is_empty() {
        return Err(OciDistributionError::PushNoDataError);
    }

    let upload_url = registry_blob_upload_url(base, repository)?;
    let response = http
        .post(upload_url.clone())
        .basic_auth(username, Some(password))
        .header(CONTENT_LENGTH, "0")
        .send()
        .await?;
    let response = ensure_registry_status(
        response,
        &[oci_reqwest::StatusCode::ACCEPTED],
        upload_url.as_str(),
    )
    .await?;
    let location = response
        .headers()
        .get(LOCATION)
        .ok_or(OciDistributionError::RegistryNoLocationError)?
        .to_str()?;
    let location = resolve_registry_location(base, location)?;

    let response = http
        .patch(location.clone())
        .basic_auth(username, Some(password))
        .header(CONTENT_TYPE, "application/octet-stream")
        .header(CONTENT_LENGTH, data.len().to_string())
        .body(data.to_vec())
        .send()
        .await?;
    let response = ensure_registry_status(
        response,
        &[oci_reqwest::StatusCode::ACCEPTED],
        location.as_str(),
    )
    .await?;
    let location = response
        .headers()
        .get(LOCATION)
        .map(|value| value.to_str())
        .transpose()?
        .map(|location| resolve_registry_location(base, location))
        .transpose()?
        .unwrap_or(location);

    let complete_url = append_digest_param(&location, digest);
    let response = http
        .put(complete_url.clone())
        .basic_auth(username, Some(password))
        .header(CONTENT_LENGTH, "0")
        .send()
        .await?;
    let response = ensure_registry_status(
        response,
        &[oci_reqwest::StatusCode::CREATED],
        complete_url.as_str(),
    )
    .await?;
    response_location_or_url(&response, &complete_url)
}

fn is_unauthorized_registry_error(error: &OciDistributionError) -> bool {
    match error {
        OciDistributionError::UnauthorizedError { .. }
        | OciDistributionError::AuthenticationFailure(_)
        | OciDistributionError::ServerError { code: 401, .. } => true,
        OciDistributionError::RequestError(error) => error
            .status()
            .is_some_and(|status| status == oci_reqwest::StatusCode::UNAUTHORIZED),
        OciDistributionError::RegistryError { envelope, .. } => envelope
            .errors
            .iter()
            .any(|err| matches!(err.code, OciErrorCode::Unauthorized)),
        _ => false,
    }
}

fn registry_error_summary(error: &OciDistributionError, auth: &RegistryAuth) -> String {
    let mut message = error.to_string();
    if let Some((username, password)) = auth.basic_credentials() {
        for secret in [username, password] {
            if !secret.is_empty() {
                message = message.replace(&secret, "[redacted]");
            }
        }
    }
    message
}

fn is_repository_already_exists_push_error(error: &OciDistributionError) -> bool {
    match error {
        OciDistributionError::ServerError { code, message, .. } => {
            *code == 409 || looks_like_repository_already_exists(message)
        }
        OciDistributionError::RegistryError { envelope, .. } => envelope.errors.iter().any(|err| {
            let name_error = matches!(
                &err.code,
                OciErrorCode::NameInvalid | OciErrorCode::NameUnknown
            );
            (name_error || matches!(&err.code, OciErrorCode::Denied))
                && (looks_like_repository_already_exists(&err.message)
                    || looks_like_repository_already_exists(&err.detail.to_string()))
        }),
        OciDistributionError::GenericError(Some(message))
        | OciDistributionError::SpecViolationError(message) => {
            looks_like_repository_already_exists(message)
        }
        _ => false,
    }
}

fn looks_like_repository_already_exists(message: &str) -> bool {
    let message = message.to_lowercase();
    message.contains("already exists")
        || message.contains("resource exists")
        || message.contains("duplicate")
        || message.contains("已存在")
        || message.contains("重复创建")
}

fn push_error_summary(error: &OciDistributionError) -> String {
    let mut message = error.to_string();
    if matches!(
        error,
        OciDistributionError::UnauthorizedError { .. }
            | OciDistributionError::AuthenticationFailure(_)
            | OciDistributionError::ServerError { code: 401, .. }
    ) {
        message.push_str(
            "; checked A3S credentials, Docker config/credential helpers, and REGISTRY_USERNAME/REGISTRY_PASSWORD",
        );
    }
    message
}

/// Platform resolver that always selects linux images matching the host architecture.
///
/// Container images run inside a Linux microVM regardless of the host OS,
/// so we always look for `os: "linux"` with the host's CPU architecture.
/// Resolve the target architecture (OCI/Docker naming) from an optional
/// `--platform` string ("linux/amd64", "linux/arm64/v8", or a bare "arm64"),
/// defaulting to the host architecture.
fn resolve_target_arch(platform: Option<&str>) -> String {
    let raw = match platform {
        // Docker/OCI platform is os/arch[/variant]; accept a bare arch too.
        Some(p) => p
            .split('/')
            .nth(1)
            .or_else(|| p.split('/').next())
            .unwrap_or(p)
            .to_string(),
        None => std::env::consts::ARCH.to_string(),
    };
    match raw.as_str() {
        "x86_64" => "amd64".to_string(),
        "aarch64" => "arm64".to_string(),
        other => other.to_string(),
    }
}

/// Build a resolver selecting the `linux` manifest for `arch` from a multi-arch
/// image index. Container images run inside a Linux microVM, so the os is
/// always "linux".
fn platform_resolver_for(arch: String) -> impl Fn(&[ImageIndexEntry]) -> Option<String> {
    move |manifests: &[ImageIndexEntry]| {
        manifests
            .iter()
            .find(|entry| {
                entry
                    .platform
                    .as_ref()
                    .is_some_and(|p| p.os == "linux" && p.architecture == arch)
            })
            .map(|entry| entry.digest.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn test_image_reference() -> ImageReference {
        ImageReference {
            registry: "registry.example.com".to_string(),
            repository: "a3s/app".to_string(),
            tag: Some("latest".to_string()),
            digest: None,
        }
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    #[test]
    fn test_registry_auth_anonymous() {
        let auth = RegistryAuth::anonymous();
        assert!(auth.username.is_none());
        assert!(auth.password.is_none());
    }

    #[test]
    fn validated_digest_hex_rejects_path_traversal_and_non_hex() {
        // A real digest passes and yields the bare hex (used as a path component).
        let good = format!("sha256:{}", "a".repeat(64));
        assert_eq!(validated_digest_hex(&good).unwrap(), "a".repeat(64));
        assert_eq!(
            validated_digest_hex(
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            )
            .unwrap(),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );

        // SECURITY: every path-traversal / malformed digest a malicious registry
        // could return MUST be rejected before it reaches blobs_dir.join().
        for evil in [
            "sha256:../../../../etc/cron.d/x",
            "sha256:..",
            "sha256:../x",
            "sha256:abc/../def",
            "sha256:/etc/passwd",
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcde/", // 63 + slash
            &format!("sha256:{}", "A".repeat(64)), // uppercase not allowed (non-canonical)
            &format!("sha256:{}", "g".repeat(64)), // non-hex
            &format!("sha256:{}", "a".repeat(63)), // too short
            &format!("sha256:{}", "a".repeat(65)), // too long
            "sha512:0000000000000000000000000000000000000000000000000000000000000000",
            "../../../etc/passwd",
            "",
        ] {
            assert!(
                validated_digest_hex(evil).is_err(),
                "must reject malicious/malformed digest: {evil:?}"
            );
        }
    }

    #[test]
    fn test_resolve_target_arch() {
        // os/arch[/variant] and bare arch, normalized to OCI/Docker names.
        assert_eq!(resolve_target_arch(Some("linux/arm64")), "arm64");
        assert_eq!(resolve_target_arch(Some("linux/amd64")), "amd64");
        assert_eq!(resolve_target_arch(Some("linux/arm64/v8")), "arm64");
        assert_eq!(resolve_target_arch(Some("arm64")), "arm64");
        assert_eq!(resolve_target_arch(Some("linux/x86_64")), "amd64");
        assert_eq!(resolve_target_arch(Some("aarch64")), "arm64");
        // No platform -> host arch (non-empty, normalized for common hosts).
        assert!(!resolve_target_arch(None).is_empty());
    }

    #[tokio::test]
    async fn test_hashing_file_writer_matches_sha256() {
        use sha2::{Digest, Sha256};
        use tokio::io::AsyncWriteExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob");
        let payload = b"a3s-box streaming blob hash test payload";

        let mut writer = HashingFileWriter::new(tokio::fs::File::create(&path).await.unwrap());
        // Write in two chunks to exercise incremental hashing.
        writer.write_all(&payload[..10]).await.unwrap();
        writer.write_all(&payload[10..]).await.unwrap();
        writer.flush().await.unwrap();
        writer.shutdown().await.unwrap();
        let streamed = writer.finalize_hex();

        let expected = format!("{:x}", Sha256::digest(payload));
        assert_eq!(
            streamed, expected,
            "streamed hash must equal sha256(payload)"
        );
        // The file on disk must contain exactly the written bytes.
        assert_eq!(std::fs::read(&path).unwrap(), payload);
    }

    #[test]
    fn test_registry_auth_basic() {
        let auth = RegistryAuth::basic("user", "pass");
        assert_eq!(auth.username, Some("user".to_string()));
        assert_eq!(auth.password, Some("pass".to_string()));
    }

    #[test]
    fn explicit_home_registry_credentials_are_loaded() {
        let home = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(home.path().join("auth/credentials.json"));
        store
            .store(
                "manager-auth.invalid:5443",
                "manager-user",
                "manager-secret",
            )
            .unwrap();

        let auth = RegistryAuth::from_credential_store_at(home.path(), "manager-auth.invalid:5443");

        assert_eq!(
            auth.basic_credentials(),
            Some(("manager-user".to_string(), "manager-secret".to_string()))
        );
    }

    #[test]
    fn malformed_explicit_home_store_falls_back_to_environment() {
        let _guard = env_lock();
        let previous_username = std::env::var_os("REGISTRY_USERNAME");
        let previous_password = std::env::var_os("REGISTRY_PASSWORD");
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("auth")).unwrap();
        std::fs::write(home.path().join("auth/credentials.json"), b"not-json").unwrap();
        std::env::set_var("REGISTRY_USERNAME", "fallback-user");
        std::env::set_var("REGISTRY_PASSWORD", "fallback-secret");

        let auth = RegistryAuth::from_credential_store_at(
            home.path(),
            "malformed-manager-auth.invalid:5443",
        );

        assert_eq!(
            auth.basic_credentials(),
            Some(("fallback-user".to_string(), "fallback-secret".to_string()))
        );
        match previous_username {
            Some(value) => std::env::set_var("REGISTRY_USERNAME", value),
            None => std::env::remove_var("REGISTRY_USERNAME"),
        }
        match previous_password {
            Some(value) => std::env::set_var("REGISTRY_PASSWORD", value),
            None => std::env::remove_var("REGISTRY_PASSWORD"),
        }
    }

    #[test]
    fn test_registry_auth_to_oci_anonymous() {
        let auth = RegistryAuth::anonymous();
        let oci_auth = auth.to_oci_auth();
        assert!(matches!(oci_auth, OciRegistryAuth::Anonymous));
    }

    #[test]
    fn test_registry_auth_to_oci_basic() {
        let auth = RegistryAuth::basic("user", "pass");
        let oci_auth = auth.to_oci_auth();
        assert!(matches!(oci_auth, OciRegistryAuth::Basic(_, _)));
    }

    #[test]
    fn test_registry_protocol_defaults_to_https() {
        let _guard = env_lock();
        std::env::remove_var(REGISTRY_PROTOCOL_ENV);
        assert_eq!(RegistryProtocol::from_env(), RegistryProtocol::Https);
    }

    #[test]
    fn test_registry_protocol_can_use_http_for_local_testing() {
        let _guard = env_lock();
        std::env::set_var(REGISTRY_PROTOCOL_ENV, "http");
        assert_eq!(RegistryProtocol::from_env(), RegistryProtocol::Http);
        std::env::remove_var(REGISTRY_PROTOCOL_ENV);
    }

    #[test]
    fn test_registry_protocol_rejects_unknown_values_to_https() {
        let _guard = env_lock();
        std::env::set_var(REGISTRY_PROTOCOL_ENV, "ftp");
        assert_eq!(RegistryProtocol::from_env(), RegistryProtocol::Https);
        std::env::remove_var(REGISTRY_PROTOCOL_ENV);
    }

    #[test]
    fn registry_base_url_uses_explicit_protocol() {
        let reference = test_image_reference();

        assert_eq!(
            registry_base_url(RegistryProtocol::Https, &reference)
                .unwrap()
                .as_str(),
            "https://registry.example.com/"
        );
        assert_eq!(
            registry_base_url(RegistryProtocol::Http, &reference)
                .unwrap()
                .as_str(),
            "http://registry.example.com/"
        );
    }

    #[test]
    fn test_repository_exists_push_error_matches_chinese_registry_message() {
        let error = OciDistributionError::ServerError {
            code: 500,
            url: "http://10.12.111.133:49164/v2/a3s/api/blobs/uploads/".to_string(),
            message: "该资源已存在，请勿重复创建".to_string(),
        };

        assert!(is_repository_already_exists_push_error(&error));
    }

    #[test]
    fn test_repository_exists_push_error_retries_conflict_status() {
        let error = OciDistributionError::ServerError {
            code: 409,
            url: "http://registry.example.com/v2/a3s/api/blobs/uploads/".to_string(),
            message: "conflict".to_string(),
        };

        assert!(is_repository_already_exists_push_error(&error));
    }

    #[test]
    fn test_unauthorized_push_error_is_not_repository_retryable() {
        let error = OciDistributionError::UnauthorizedError {
            url: "http://registry.example.com/v2/a3s/web/blobs/uploads/".to_string(),
        };

        assert!(!is_repository_already_exists_push_error(&error));
        assert!(push_error_summary(&error).contains("Docker config/credential helpers"));
    }

    #[test]
    fn test_manifest_digest_from_bytes_uses_sha256() {
        assert_eq!(
            manifest_digest_from_bytes(b"hello"),
            "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn test_verify_remote_manifest_digest_accepts_matching_digest() {
        let reference = test_image_reference();
        let digest = format!("sha256:{}", "a".repeat(64));

        verify_remote_manifest_digest(&reference, &digest, &digest.to_uppercase()).unwrap();
    }

    #[test]
    fn test_verify_remote_manifest_digest_rejects_mismatch() {
        let reference = test_image_reference();
        let err = verify_remote_manifest_digest(
            &reference,
            &format!("sha256:{}", "a".repeat(64)),
            &format!("sha256:{}", "b".repeat(64)),
        )
        .unwrap_err();

        let message = err.to_string();
        assert!(message.contains("Manifest verification failed after push"));
        assert!(message.contains("registry.example.com/a3s/app"));
    }

    #[test]
    fn test_to_oci_reference_with_tag() {
        let puller = RegistryPuller::new();
        let img_ref = ImageReference {
            registry: "ghcr.io".to_string(),
            repository: "a3s-box/code".to_string(),
            tag: Some("v0.1.0".to_string()),
            digest: None,
        };
        let oci_ref = puller.to_oci_reference(&img_ref).unwrap();
        assert_eq!(oci_ref.to_string(), "ghcr.io/a3s-box/code:v0.1.0");
    }

    #[test]
    fn test_to_oci_reference_with_digest() {
        let puller = RegistryPuller::new();
        let img_ref = ImageReference {
            registry: "ghcr.io".to_string(),
            repository: "a3s-box/code".to_string(),
            tag: None,
            digest: Some(
                "sha256:abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890"
                    .to_string(),
            ),
        };
        let oci_ref = puller.to_oci_reference(&img_ref).unwrap();
        let ref_str = oci_ref.to_string();
        assert!(ref_str.contains("sha256:"));
    }

    #[test]
    fn test_to_oci_reference_default_tag() {
        let puller = RegistryPuller::new();
        let img_ref = ImageReference {
            registry: "docker.io".to_string(),
            repository: "library/nginx".to_string(),
            tag: None,
            digest: None,
        };
        let oci_ref = puller.to_oci_reference(&img_ref).unwrap();
        assert!(oci_ref.to_string().contains("latest"));
    }

    #[test]
    fn test_pusher_to_oci_reference_with_tag_and_default_latest() {
        let pusher = RegistryPusher::new();
        let tagged = test_image_reference();
        assert_eq!(
            pusher.to_oci_reference(&tagged).unwrap().to_string(),
            "registry.example.com/a3s/app:latest"
        );

        let tagless = ImageReference {
            tag: None,
            ..test_image_reference()
        };
        assert_eq!(
            pusher.to_oci_reference(&tagless).unwrap().to_string(),
            "registry.example.com/a3s/app:latest"
        );
    }

    #[tokio::test]
    async fn test_push_missing_index_fails_before_registry() {
        let dir = tempfile::tempdir().unwrap();
        let err = RegistryPusher::new()
            .push(&test_image_reference(), dir.path())
            .await
            .unwrap_err();

        assert!(err.to_string().contains("Failed to read index.json"));
    }

    #[tokio::test]
    async fn test_push_index_without_manifest_digest_fails_before_registry() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("index.json"),
            r#"{"schemaVersion":2,"manifests":[{}]}"#,
        )
        .unwrap();

        let err = RegistryPusher::new()
            .push(&test_image_reference(), dir.path())
            .await
            .unwrap_err();

        assert!(err.to_string().contains("No manifest digest in index.json"));
    }

    #[tokio::test]
    async fn test_push_missing_manifest_blob_fails_before_registry() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_digest = format!("sha256:{}", "a".repeat(64));
        std::fs::write(
            dir.path().join("index.json"),
            serde_json::json!({
                "schemaVersion": 2,
                "manifests": [{"digest": manifest_digest}]
            })
            .to_string(),
        )
        .unwrap();

        let err = RegistryPusher::new()
            .push(&test_image_reference(), dir.path())
            .await
            .unwrap_err();

        assert!(err.to_string().contains("Failed to read manifest blob"));
    }

    #[tokio::test]
    async fn test_push_missing_config_blob_fails_before_registry() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = dir.path().join("blobs/sha256");
        std::fs::create_dir_all(&blobs).unwrap();
        let manifest_digest_hex = "a".repeat(64);
        let config_digest = format!("sha256:{}", "b".repeat(64));
        let manifest = OciImageManifest {
            config: oci_distribution::manifest::OciDescriptor {
                media_type: "application/vnd.oci.image.config.v1+json".to_string(),
                digest: config_digest,
                size: 2,
                ..Default::default()
            },
            ..Default::default()
        };
        std::fs::write(
            dir.path().join("index.json"),
            serde_json::json!({
                "schemaVersion": 2,
                "manifests": [{"digest": format!("sha256:{manifest_digest_hex}")}]
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            blobs.join(&manifest_digest_hex),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        let err = RegistryPusher::new()
            .push(&test_image_reference(), dir.path())
            .await
            .unwrap_err();

        assert!(err.to_string().contains("Failed to read config blob"));
    }

    #[tokio::test]
    async fn test_push_missing_layer_blob_fails_before_registry() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = dir.path().join("blobs/sha256");
        std::fs::create_dir_all(&blobs).unwrap();
        let manifest_digest_hex = "a".repeat(64);
        let config_digest_hex = "b".repeat(64);
        let layer_digest = format!("sha256:{}", "c".repeat(64));
        let manifest = OciImageManifest {
            config: oci_distribution::manifest::OciDescriptor {
                media_type: "application/vnd.oci.image.config.v1+json".to_string(),
                digest: format!("sha256:{config_digest_hex}"),
                size: 2,
                ..Default::default()
            },
            layers: vec![oci_distribution::manifest::OciDescriptor {
                media_type: "application/vnd.oci.image.layer.v1.tar".to_string(),
                digest: layer_digest.clone(),
                size: 42,
                ..Default::default()
            }],
            ..Default::default()
        };
        std::fs::write(
            dir.path().join("index.json"),
            serde_json::json!({
                "schemaVersion": 2,
                "manifests": [{"digest": format!("sha256:{manifest_digest_hex}")}]
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            blobs.join(&manifest_digest_hex),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        std::fs::write(blobs.join(config_digest_hex), b"{}").unwrap();

        let err = RegistryPusher::new()
            .push(&test_image_reference(), dir.path())
            .await
            .unwrap_err();

        assert!(err.to_string().contains("Failed to read layer blob"));
        assert!(err.to_string().contains(&layer_digest));
    }
}

#[cfg(test)]
mod basic_pull_tests;
