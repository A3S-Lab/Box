use oci_distribution::errors::OciDistributionError;
use oci_distribution::manifest::{
    OciImageManifest, OciManifest, IMAGE_MANIFEST_LIST_MEDIA_TYPE, IMAGE_MANIFEST_MEDIA_TYPE,
    OCI_IMAGE_INDEX_MEDIA_TYPE, OCI_IMAGE_MEDIA_TYPE,
};
use oci_reqwest::header::ACCEPT;
use sha2::Digest as _;

use super::{
    registry_base_url, registry_manifest_url, ImageReference, RegistryAuth, RegistryProtocol,
    MANIFEST_ACCEPT,
};

pub(super) struct BasicImageManifest {
    pub(super) manifest: OciImageManifest,
    pub(super) digest: String,
    pub(super) bytes: Vec<u8>,
}

struct RawManifest {
    manifest: OciManifest,
    digest: String,
    bytes: Vec<u8>,
}

/// Minimal OCI pull transport for registries that require preemptive HTTP
/// Basic authentication on protected endpoints but do not advertise that
/// challenge from `/v2/`.
pub(super) struct BasicPullClient {
    http: oci_reqwest::Client,
    base: oci_reqwest::Url,
    repository: String,
    username: String,
    password: String,
    target_arch: String,
}

impl BasicPullClient {
    pub(super) fn new(
        protocol: RegistryProtocol,
        reference: &ImageReference,
        auth: &RegistryAuth,
        target_arch: String,
    ) -> std::result::Result<Self, OciDistributionError> {
        let (username, password) = auth.basic_credentials().ok_or_else(|| {
            OciDistributionError::GenericError(Some(
                "preemptive Basic pull requires non-empty credentials".to_string(),
            ))
        })?;

        Ok(Self {
            http: oci_reqwest::Client::builder().build()?,
            base: registry_base_url(protocol, reference)?,
            repository: reference.repository.clone(),
            username,
            password,
            target_arch,
        })
    }

    pub(super) async fn pull_manifest_digest(
        &self,
        reference: &ImageReference,
    ) -> std::result::Result<String, OciDistributionError> {
        let manifest_ref = manifest_reference(reference);
        let response = self
            .fetch_manifest(manifest_ref, reference.digest.as_deref(), None)
            .await?;
        Ok(response.digest)
    }

    pub(super) async fn pull_image_manifest(
        &self,
        reference: &ImageReference,
    ) -> std::result::Result<BasicImageManifest, OciDistributionError> {
        let manifest_ref = manifest_reference(reference);
        let root = self
            .fetch_manifest(manifest_ref, reference.digest.as_deref(), None)
            .await?;

        match root.manifest {
            OciManifest::Image(manifest) => Ok(BasicImageManifest {
                manifest,
                digest: root.digest,
                bytes: root.bytes,
            }),
            OciManifest::ImageIndex(index) => {
                let entry = index
                    .manifests
                    .iter()
                    .find(|entry| {
                        entry.platform.as_ref().is_some_and(|platform| {
                            platform.os == "linux" && platform.architecture == self.target_arch
                        })
                    })
                    .cloned()
                    .ok_or_else(|| {
                        OciDistributionError::ImageManifestNotFoundError(format!(
                            "no linux/{} entry found in image index",
                            self.target_arch
                        ))
                    })?;
                let selected = self
                    .fetch_manifest(&entry.digest, Some(&entry.digest), Some(entry.size))
                    .await?;
                match selected.manifest {
                    OciManifest::Image(manifest) => Ok(BasicImageManifest {
                        manifest,
                        digest: selected.digest,
                        bytes: selected.bytes,
                    }),
                    OciManifest::ImageIndex(_) => {
                        Err(OciDistributionError::ImageManifestNotFoundError(
                            "selected image-index entry resolved to another image index"
                                .to_string(),
                        ))
                    }
                }
            }
        }
    }

    async fn fetch_manifest(
        &self,
        reference: &str,
        expected_digest: Option<&str>,
        expected_size: Option<i64>,
    ) -> std::result::Result<RawManifest, OciDistributionError> {
        let url = registry_manifest_url(&self.base, &self.repository, reference)?;
        let response = self
            .http
            .get(url.clone())
            .basic_auth(&self.username, Some(&self.password))
            .header(ACCEPT, MANIFEST_ACCEPT)
            .send()
            .await?;
        let response = ensure_pull_status(response, url.as_str())?;
        let header_digest = response
            .headers()
            .get("docker-content-digest")
            .map(|value| value.to_str().map(str::to_string))
            .transpose()?;
        let bytes = response.bytes().await?.to_vec();

        if let Some(expected_size) = expected_size {
            if expected_size < 0 || bytes.len() as i64 != expected_size {
                return Err(OciDistributionError::SpecViolationError(format!(
                    "manifest size mismatch: expected {expected_size} bytes, received {}",
                    bytes.len()
                )));
            }
        }

        let computed_digest = format!("sha256:{:x}", sha2::Sha256::digest(&bytes));
        if let Some(expected_digest) = expected_digest {
            validate_sha256_digest(expected_digest)?;
            ensure_digest_matches("manifest descriptor", expected_digest, &computed_digest)?;
        }
        if let Some(header_digest) = header_digest.as_deref() {
            validate_sha256_digest(header_digest)?;
            ensure_digest_matches(
                "Docker-Content-Digest header",
                header_digest,
                &computed_digest,
            )?;
        }
        let digest = header_digest.unwrap_or(computed_digest);

        let manifest: OciManifest = serde_json::from_slice(&bytes)
            .map_err(|error| OciDistributionError::ManifestParsingError(error.to_string()))?;
        validate_manifest(&manifest)?;

        Ok(RawManifest {
            manifest,
            digest,
            bytes,
        })
    }
}

fn manifest_reference(reference: &ImageReference) -> &str {
    reference
        .tag
        .as_deref()
        .or(reference.digest.as_deref())
        .unwrap_or("latest")
}

fn validate_manifest(manifest: &OciManifest) -> std::result::Result<(), OciDistributionError> {
    let (schema_version, media_type) = match manifest {
        OciManifest::Image(manifest) => (manifest.schema_version, manifest.media_type.as_deref()),
        OciManifest::ImageIndex(index) => (index.schema_version, index.media_type.as_deref()),
    };
    if schema_version != 2 {
        return Err(OciDistributionError::UnsupportedSchemaVersionError(
            i32::from(schema_version),
        ));
    }
    if let Some(media_type) = media_type {
        if ![
            IMAGE_MANIFEST_MEDIA_TYPE,
            OCI_IMAGE_MEDIA_TYPE,
            IMAGE_MANIFEST_LIST_MEDIA_TYPE,
            OCI_IMAGE_INDEX_MEDIA_TYPE,
        ]
        .contains(&media_type)
        {
            return Err(OciDistributionError::UnsupportedMediaTypeError(
                media_type.to_string(),
            ));
        }
    }
    Ok(())
}

fn validate_sha256_digest(digest: &str) -> std::result::Result<(), OciDistributionError> {
    let valid = digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    });
    if valid {
        Ok(())
    } else {
        Err(OciDistributionError::SpecViolationError(
            "registry returned a malformed content digest (expected sha256:<64 lowercase hex>)"
                .to_string(),
        ))
    }
}

fn ensure_digest_matches(
    source: &str,
    expected: &str,
    computed: &str,
) -> std::result::Result<(), OciDistributionError> {
    if expected.eq_ignore_ascii_case(computed) {
        Ok(())
    } else {
        Err(OciDistributionError::SpecViolationError(format!(
            "{source} digest mismatch: expected {expected}, computed {computed}"
        )))
    }
}

fn ensure_pull_status(
    response: oci_reqwest::Response,
    url: &str,
) -> std::result::Result<oci_reqwest::Response, OciDistributionError> {
    let status = response.status();
    if status == oci_reqwest::StatusCode::OK {
        return Ok(response);
    }
    if status == oci_reqwest::StatusCode::UNAUTHORIZED {
        return Err(OciDistributionError::UnauthorizedError {
            url: url.to_string(),
        });
    }

    // Do not include a registry-controlled response body. A hostile endpoint
    // could echo the Authorization header or submitted credentials into it.
    Err(OciDistributionError::ServerError {
        code: status.as_u16(),
        url: url.to_string(),
        message: "registry request failed".to_string(),
    })
}
