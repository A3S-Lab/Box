use std::path::Path;

use a3s_box_core::error::{BoxError, Result};
use oci_distribution::errors::OciDistributionError;
use oci_distribution::manifest::OciDescriptor;
use oci_distribution::{Client, Reference};

use super::basic_pull::BasicPullClient;
use super::{is_unauthorized_registry_error, registry_error_summary, RegistryAuth};

/// An `AsyncWrite` that streams bytes straight to a file while computing its
/// SHA-256 and size, so a pulled blob is never fully buffered in memory.
pub(super) struct HashingFileWriter {
    file: tokio::fs::File,
    hasher: sha2::Sha256,
    bytes_written: u64,
}

impl HashingFileWriter {
    pub(super) fn new(file: tokio::fs::File) -> Self {
        use sha2::Digest as _;

        Self {
            file,
            hasher: sha2::Sha256::new(),
            bytes_written: 0,
        }
    }

    fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    pub(super) fn finalize_hex(self) -> String {
        use sha2::Digest as _;

        format!("{:x}", self.hasher.finalize())
    }
}

impl tokio::io::AsyncWrite for HashingFileWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        use sha2::Digest as _;

        let this = self.get_mut();
        match std::pin::Pin::new(&mut this.file).poll_write(cx, buf) {
            std::task::Poll::Ready(Ok(written)) => {
                this.hasher.update(&buf[..written]);
                this.bytes_written = this.bytes_written.saturating_add(written as u64);
                std::task::Poll::Ready(Ok(written))
            }
            other => other,
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().file).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().file).poll_shutdown(cx)
    }
}

pub(super) struct BlobPullTransport<'a> {
    pub(super) client: &'a Client,
    pub(super) oci_ref: &'a Reference,
    pub(super) basic_client: Option<&'a BasicPullClient>,
    pub(super) force_basic: bool,
    pub(super) auth: &'a RegistryAuth,
    pub(super) registry: &'a str,
}

/// Stream a blob to `dest`, verifying its declared size and SHA-256 before
/// atomically publishing it under its content-addressed name.
pub(super) async fn stream_and_verify_blob(
    transport: &BlobPullTransport<'_>,
    descriptor: &OciDescriptor,
    dest: &Path,
    what: &str,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let tmp = dest.with_extension("partial");
    let mut writer = create_blob_writer(&tmp, what, transport.registry).await?;

    let first_result = if transport.force_basic {
        match transport.basic_client {
            Some(basic_client) => basic_client.pull_blob(descriptor, &mut writer).await,
            None => Err(OciDistributionError::GenericError(Some(
                "preemptive Basic blob pull requires non-empty credentials".to_string(),
            ))),
        }
    } else {
        transport
            .client
            .pull_blob(transport.oci_ref, descriptor, &mut writer)
            .await
    };

    if let Err(first_error) = first_result {
        if !transport.force_basic && is_unauthorized_registry_error(&first_error) {
            if let Some(basic_client) = transport.basic_client {
                tracing::warn!(
                    error = %registry_error_summary(&first_error, transport.auth),
                    "Registry rejected the default OCI blob auth flow; retrying with preemptive Basic auth"
                );
                drop(writer);
                let _ = tokio::fs::remove_file(&tmp).await;
                writer = create_blob_writer(&tmp, what, transport.registry).await?;
                if let Err(fallback_error) = basic_client.pull_blob(descriptor, &mut writer).await {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return Err(BoxError::RegistryError {
                        registry: transport.registry.to_string(),
                        message: format!(
                            "Failed to pull {what}: default auth failed: {}; preemptive Basic retry failed: {}",
                            registry_error_summary(&first_error, transport.auth),
                            registry_error_summary(&fallback_error, transport.auth)
                        ),
                    });
                }
            } else {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(blob_pull_error(
                    transport.registry,
                    what,
                    &first_error,
                    transport.auth,
                ));
            }
        } else {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(blob_pull_error(
                transport.registry,
                what,
                &first_error,
                transport.auth,
            ));
        }
    }

    if let Err(error) = writer.flush().await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(blob_io_error(transport.registry, what, "flush", error));
    }
    if let Err(error) = writer.shutdown().await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(blob_io_error(transport.registry, what, "close", error));
    }
    let actual_size = writer.bytes_written();
    let actual_hex = writer.finalize_hex();

    if descriptor.size < 0 || actual_size != descriptor.size as u64 {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(BoxError::RegistryError {
            registry: transport.registry.to_string(),
            message: format!(
                "{what} size mismatch: expected {} bytes, received {actual_size}",
                descriptor.size
            ),
        });
    }

    match descriptor.digest.strip_prefix("sha256:") {
        Some(expected_hex) if actual_hex.eq_ignore_ascii_case(expected_hex) => {}
        Some(expected_hex) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(BoxError::RegistryError {
                registry: transport.registry.to_string(),
                message: format!(
                    "{what} digest mismatch: expected sha256:{expected_hex}, computed sha256:{actual_hex}"
                ),
            });
        }
        None => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(BoxError::RegistryError {
                registry: transport.registry.to_string(),
                message: format!(
                    "{what} uses an unsupported digest algorithm ({}); refusing to store \
                     unverifiable content (only sha256 is supported)",
                    descriptor.digest
                ),
            });
        }
    }

    if let Err(error) = tokio::fs::rename(&tmp, dest).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(BoxError::RegistryError {
            registry: transport.registry.to_string(),
            message: format!("Failed to store {what} blob: {error}"),
        });
    }
    Ok(())
}

fn blob_pull_error(
    registry: &str,
    what: &str,
    error: &OciDistributionError,
    auth: &RegistryAuth,
) -> BoxError {
    BoxError::RegistryError {
        registry: registry.to_string(),
        message: format!(
            "Failed to pull {what}: {}",
            registry_error_summary(error, auth)
        ),
    }
}

async fn create_blob_writer(tmp: &Path, what: &str, registry: &str) -> Result<HashingFileWriter> {
    tokio::fs::File::create(tmp)
        .await
        .map(HashingFileWriter::new)
        .map_err(|error| blob_io_error(registry, what, "create", error))
}

fn blob_io_error(registry: &str, what: &str, operation: &str, error: std::io::Error) -> BoxError {
    BoxError::RegistryError {
        registry: registry.to_string(),
        message: format!("Failed to {operation} {what} file: {error}"),
    }
}
