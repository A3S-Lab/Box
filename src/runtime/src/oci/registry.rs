//! OCI registry client for pulling images.
//!
//! Uses the `oci-distribution` crate to pull images from container registries
//! (Docker Hub, GHCR, etc.) and writes them as OCI image layouts on disk.

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use oci_distribution::client::{ClientConfig, ClientProtocol};
use oci_distribution::manifest::{OciImageManifest, OciManifest};
use oci_distribution::secrets::RegistryAuth as OciRegistryAuth;
use oci_distribution::{Client, Reference};

use super::reference::ImageReference;

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

    /// Convert to oci-distribution auth type.
    fn to_oci_auth(&self) -> OciRegistryAuth {
        match (&self.username, &self.password) {
            (Some(u), Some(p)) => OciRegistryAuth::Basic(u.clone(), p.clone()),
            _ => OciRegistryAuth::Anonymous,
        }
    }
}

/// Pulls OCI images from container registries.
pub struct RegistryPuller {
    client: Client,
    auth: RegistryAuth,
}

impl RegistryPuller {
    /// Create a new registry puller with anonymous authentication.
    pub fn new() -> Self {
        Self::with_auth(RegistryAuth::anonymous())
    }

    /// Create a new registry puller with the given authentication.
    pub fn with_auth(auth: RegistryAuth) -> Self {
        let config = ClientConfig {
            protocol: ClientProtocol::Https,
            ..Default::default()
        };
        let client = Client::new(config);

        Self { client, auth }
    }

    /// Pull an image and write it as an OCI image layout to `target_dir`.
    ///
    /// The resulting directory will contain:
    /// - `oci-layout`
    /// - `index.json`
    /// - `blobs/sha256/...`
    pub async fn pull(
        &self,
        reference: &ImageReference,
        target_dir: &Path,
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

        // Pull manifest and config
        let auth = self.auth.to_oci_auth();
        let (manifest, manifest_digest) = self
            .client
            .pull_manifest(&oci_ref, &auth)
            .await
            .map_err(|e| BoxError::RegistryError {
                registry: reference.registry.clone(),
                message: format!("Failed to pull manifest: {}", e),
            })?;

        // Write manifest blob
        let manifest_json = serde_json::to_vec(&manifest)?;
        let manifest_digest_hex = manifest_digest
            .strip_prefix("sha256:")
            .unwrap_or(&manifest_digest);
        std::fs::write(blobs_dir.join(manifest_digest_hex), &manifest_json).map_err(|e| {
            BoxError::RegistryError {
                registry: reference.registry.clone(),
                message: format!("Failed to write manifest: {}", e),
            }
        })?;

        // Pull image config and layers based on manifest type
        match manifest {
            OciManifest::Image(image_manifest) => {
                self.pull_image_content(
                    &oci_ref,
                    &image_manifest,
                    &blobs_dir,
                    &reference.registry,
                )
                .await?;
            }
            OciManifest::ImageIndex(_) => {
                return Err(BoxError::RegistryError {
                    registry: reference.registry.clone(),
                    message:
                        "Unsupported manifest type (only OCI image manifests are supported)"
                            .to_string(),
                });
            }
        }

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
    pub async fn pull_manifest_digest(
        &self,
        reference: &ImageReference,
    ) -> Result<String> {
        let oci_ref = self.to_oci_reference(reference)?;
        let auth = self.auth.to_oci_auth();

        let (_manifest, digest) = self
            .client
            .pull_manifest(&oci_ref, &auth)
            .await
            .map_err(|e| BoxError::RegistryError {
                registry: reference.registry.clone(),
                message: format!("Failed to pull manifest: {}", e),
            })?;

        Ok(digest)
    }

    /// Pull config and layers for an image manifest, writing blobs to disk.
    async fn pull_image_content(
        &self,
        oci_ref: &Reference,
        manifest: &OciImageManifest,
        blobs_dir: &Path,
        registry: &str,
    ) -> Result<()> {
        // Pull config blob using pull_blob (streams to a Vec<u8>)
        let config_descriptor = &manifest.config;
        let mut config_data: Vec<u8> = Vec::new();
        self.client
            .pull_blob(oci_ref, config_descriptor, &mut config_data)
            .await
            .map_err(|e| BoxError::RegistryError {
                registry: registry.to_string(),
                message: format!("Failed to pull config blob: {}", e),
            })?;

        let config_digest_hex = config_descriptor
            .digest
            .strip_prefix("sha256:")
            .unwrap_or(&config_descriptor.digest);
        std::fs::write(blobs_dir.join(config_digest_hex), &config_data).map_err(|e| {
            BoxError::RegistryError {
                registry: registry.to_string(),
                message: format!("Failed to write config blob: {}", e),
            }
        })?;

        // Pull layer blobs
        for layer in &manifest.layers {
            tracing::debug!(
                digest = %layer.digest,
                size = layer.size,
                "Pulling layer"
            );

            let mut layer_data: Vec<u8> = Vec::new();
            self.client
                .pull_blob(oci_ref, layer, &mut layer_data)
                .await
                .map_err(|e| BoxError::RegistryError {
                    registry: registry.to_string(),
                    message: format!("Failed to pull layer {}: {}", layer.digest, e),
                })?;

            let layer_digest_hex = layer
                .digest
                .strip_prefix("sha256:")
                .unwrap_or(&layer.digest);
            std::fs::write(blobs_dir.join(layer_digest_hex), &layer_data).map_err(|e| {
                BoxError::RegistryError {
                    registry: registry.to_string(),
                    message: format!("Failed to write layer blob: {}", e),
                }
            })?;
        }

        Ok(())
    }

    /// Convert an ImageReference to an oci-distribution Reference.
    fn to_oci_reference(&self, reference: &ImageReference) -> Result<Reference> {
        let ref_str = if let Some(ref digest) = reference.digest {
            format!(
                "{}/{}@{}",
                reference.registry, reference.repository, digest
            )
        } else if let Some(ref tag) = reference.tag {
            format!(
                "{}/{}:{}",
                reference.registry, reference.repository, tag
            )
        } else {
            format!(
                "{}/{}:latest",
                reference.registry, reference.repository
            )
        };

        ref_str.parse::<Reference>().map_err(|e| {
            BoxError::OciImageError(format!(
                "Invalid OCI reference '{}': {}",
                ref_str, e
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_auth_anonymous() {
        let auth = RegistryAuth::anonymous();
        assert!(auth.username.is_none());
        assert!(auth.password.is_none());
    }

    #[test]
    fn test_registry_auth_basic() {
        let auth = RegistryAuth::basic("user", "pass");
        assert_eq!(auth.username, Some("user".to_string()));
        assert_eq!(auth.password, Some("pass".to_string()));
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
}
