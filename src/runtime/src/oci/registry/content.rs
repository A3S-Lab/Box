use std::collections::HashSet;
use std::path::Path;

use a3s_box_core::error::{BoxError, Result};
use futures::{stream, StreamExt, TryStreamExt};
use oci_distribution::manifest::{OciDescriptor, OciImageManifest};
use oci_distribution::Reference;

use super::blob_pull::{stream_and_verify_blob, BlobPullTransport};
use super::progress::ProgressReporter;
use super::{validated_digest_hex, ImageReference, ImageStore, RegistryPuller};

impl RegistryPuller {
    /// Pull config and unique layers with bounded concurrency, reusing verified
    /// content from existing stored image layouts when available.
    pub(super) async fn pull_image_content(
        &self,
        reference: &ImageReference,
        oci_ref: &Reference,
        manifest: &OciImageManifest,
        blobs_dir: &Path,
        force_basic: bool,
        blob_store: Option<&ImageStore>,
    ) -> Result<()> {
        let transport = BlobPullTransport {
            client: &self.client,
            http: &self.blob_http,
            oci_ref,
            image_ref: reference,
            force_basic,
            auth: &self.auth,
            registry: &reference.registry,
            protocol: self.protocol,
            policy: &self.pull_policy,
        };

        let config = &manifest.config;
        let config_hex = validated_digest_hex(&config.digest)?;
        self.materialize_blob(
            &transport,
            config,
            &blobs_dir.join(config_hex),
            "config blob",
            blob_store,
            None,
        )
        .await?;

        let mut seen = HashSet::new();
        let layers = manifest
            .layers
            .iter()
            .filter(|layer| seen.insert(layer.digest.clone()))
            .cloned()
            .collect::<Vec<_>>();
        let total = layers.len();
        let concurrency = self
            .pull_policy
            .max_concurrent_downloads()
            .min(total.max(1));

        stream::iter(layers.into_iter().enumerate())
            .map(|(index, layer)| async move {
                let expected_size =
                    u64::try_from(layer.size).map_err(|_| BoxError::RegistryError {
                        registry: reference.registry.clone(),
                        message: format!(
                            "layer {} has a negative declared size ({})",
                            layer.digest, layer.size
                        ),
                    })?;
                let current = index + 1;
                tracing::debug!(
                    digest = %layer.digest,
                    size = layer.size,
                    current,
                    total,
                    "Scheduling registry layer pull"
                );
                if let Some(callback) = &self.progress_fn {
                    callback(current, total, &layer.digest, layer.size);
                }
                let reporter = ProgressReporter::new(
                    self.progress_event_fn.clone(),
                    current,
                    total,
                    layer.digest.clone(),
                    expected_size,
                    self.pull_policy.max_attempts(),
                );
                let digest_hex = validated_digest_hex(&layer.digest)?;
                self.materialize_blob(
                    &transport,
                    &layer,
                    &blobs_dir.join(digest_hex),
                    "layer",
                    blob_store,
                    Some(reporter),
                )
                .await?;
                if let Some(callback) = &self.progress_fn {
                    callback(current, total, &layer.digest, -layer.size);
                }
                Ok::<(), BoxError>(())
            })
            .buffer_unordered(concurrency)
            .try_collect::<Vec<_>>()
            .await?;

        Ok(())
    }

    async fn materialize_blob(
        &self,
        transport: &BlobPullTransport<'_>,
        descriptor: &OciDescriptor,
        dest: &Path,
        what: &str,
        blob_store: Option<&ImageStore>,
        mut progress: Option<ProgressReporter>,
    ) -> Result<()> {
        if let Some(store) = blob_store {
            if store
                .reuse_verified_blob(&descriptor.digest, descriptor.size, dest)
                .await?
            {
                tracing::info!(
                    digest = %descriptor.digest,
                    size = descriptor.size,
                    "Reused verified registry blob from another stored image"
                );
                if let Some(reporter) = progress.as_mut() {
                    reporter.reused();
                }
                return Ok(());
            }
        }

        stream_and_verify_blob(transport, descriptor, dest, what, progress).await
    }
}
