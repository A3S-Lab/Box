use std::path::Path;

use a3s_box_core::error::{BoxError, Result};
use oci_distribution::manifest::OciDescriptor;
use oci_distribution::{Client, Reference};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use super::progress::ProgressReporter;
use super::{ImageReference, RegistryAuth, RegistryProtocol, RegistryPullPolicy};

mod http;

/// An `AsyncWrite` that streams bytes straight to a file while computing its
/// SHA-256 and size, so a pulled blob is never fully buffered in memory.
pub(super) struct HashingFileWriter {
    file: tokio::fs::File,
    hasher: sha2::Sha256,
    bytes_written: u64,
}

impl HashingFileWriter {
    #[cfg(test)]
    pub(super) fn new(file: tokio::fs::File) -> Self {
        use sha2::Digest as _;

        Self {
            file,
            hasher: sha2::Sha256::new(),
            bytes_written: 0,
        }
    }

    async fn open_resumable(path: &Path, expected_size: u64) -> Result<Self> {
        use sha2::Digest as _;

        if let Ok(metadata) = tokio::fs::symlink_metadata(path).await {
            if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                return Err(BoxError::OciImageError(format!(
                    "Registry partial blob is not a regular file: {}",
                    path.display()
                )));
            }
        }

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)
            .await
            .map_err(|error| {
                BoxError::OciImageError(format!(
                    "Failed to open registry partial blob {}: {error}",
                    path.display()
                ))
            })?;
        let mut bytes_written = file
            .metadata()
            .await
            .map_err(|error| {
                BoxError::OciImageError(format!(
                    "Failed to inspect registry partial blob {}: {error}",
                    path.display()
                ))
            })?
            .len();
        if bytes_written > expected_size {
            file.set_len(0).await.map_err(|error| {
                BoxError::OciImageError(format!(
                    "Failed to reset oversized registry partial blob {}: {error}",
                    path.display()
                ))
            })?;
            bytes_written = 0;
        }

        file.seek(std::io::SeekFrom::Start(0))
            .await
            .map_err(|error| blob_file_error(path, "seek", error))?;
        let mut hasher = sha2::Sha256::new();
        let mut remaining = bytes_written;
        let mut buffer = [0_u8; 64 * 1024];
        while remaining > 0 {
            let limit = usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
            let read = file
                .read(&mut buffer[..limit])
                .await
                .map_err(|error| blob_file_error(path, "rehash", error))?;
            if read == 0 {
                return Err(BoxError::OciImageError(format!(
                    "Registry partial blob {} changed while being resumed",
                    path.display()
                )));
            }
            hasher.update(&buffer[..read]);
            remaining = remaining.saturating_sub(read as u64);
        }
        file.seek(std::io::SeekFrom::Start(bytes_written))
            .await
            .map_err(|error| blob_file_error(path, "seek", error))?;

        Ok(Self {
            file,
            hasher,
            bytes_written,
        })
    }

    pub(super) fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    fn current_hex(&self) -> String {
        use sha2::Digest as _;

        format!("{:x}", self.hasher.clone().finalize())
    }

    pub(super) fn finalize_hex(self) -> String {
        use sha2::Digest as _;

        format!("{:x}", self.hasher.finalize())
    }

    pub(super) async fn reset(&mut self) -> std::io::Result<()> {
        use sha2::Digest as _;

        self.file.set_len(0).await?;
        self.file.seek(std::io::SeekFrom::Start(0)).await?;
        self.hasher = sha2::Sha256::new();
        self.bytes_written = 0;
        Ok(())
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

#[derive(Clone, Copy)]
pub(super) struct BlobPullTransport<'a> {
    pub(super) client: &'a Client,
    pub(super) http: &'a oci_reqwest::Client,
    pub(super) oci_ref: &'a Reference,
    pub(super) image_ref: &'a ImageReference,
    pub(super) force_basic: bool,
    pub(super) auth: &'a RegistryAuth,
    pub(super) registry: &'a str,
    pub(super) protocol: RegistryProtocol,
    pub(super) policy: &'a RegistryPullPolicy,
}

/// Stream a blob to `dest`, resuming verified partial content and requiring
/// declared-size and SHA-256 validation before atomic publication.
pub(super) async fn stream_and_verify_blob(
    transport: &BlobPullTransport<'_>,
    descriptor: &OciDescriptor,
    dest: &Path,
    what: &str,
    mut progress: Option<ProgressReporter>,
) -> Result<()> {
    let expected_size = u64::try_from(descriptor.size).map_err(|_| {
        blob_validation_error(
            transport.registry,
            format!("{what} has a negative declared size ({})", descriptor.size),
        )
    })?;
    let expected_hex = descriptor
        .digest
        .strip_prefix("sha256:")
        .filter(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
        .ok_or_else(|| {
            blob_validation_error(
                transport.registry,
                format!(
                    "{what} uses an unsupported or malformed digest {}; expected sha256:<64 lowercase hex>",
                    descriptor.digest
                ),
            )
        })?;

    let tmp = dest.with_extension("partial");
    let mut writer = HashingFileWriter::open_resumable(&tmp, expected_size).await?;
    if let Some(reporter) = progress.as_mut() {
        reporter.downloading(writer.bytes_written(), 1, true);
    }

    for attempt in 1..=transport.policy.max_attempts() {
        if writer.bytes_written() == expected_size {
            if writer.current_hex().eq_ignore_ascii_case(expected_hex) {
                return publish_blob(
                    transport.registry,
                    what,
                    &tmp,
                    dest,
                    writer,
                    progress,
                    attempt,
                )
                .await;
            }
            writer
                .reset()
                .await
                .map_err(|error| blob_io_error(transport.registry, what, "reset", error))?;
        }

        let attempt_result = http::transfer_attempt(
            transport,
            descriptor,
            &mut writer,
            progress.as_mut(),
            attempt,
            expected_size,
        )
        .await;

        let failure = match attempt_result {
            Ok(()) if writer.bytes_written() < expected_size => {
                Some(http::AttemptFailure::retryable(format!(
                    "response ended after {} of {expected_size} bytes",
                    writer.bytes_written()
                )))
            }
            Ok(()) if writer.bytes_written() > expected_size => Some(http::AttemptFailure::fatal(
                format!(
                    "response exceeded declared size {expected_size} bytes (received {})",
                    writer.bytes_written()
                ),
                true,
            )),
            Ok(()) if !writer.current_hex().eq_ignore_ascii_case(expected_hex) => {
                Some(http::AttemptFailure::retryable_reset(format!(
                    "digest mismatch after receiving {expected_size} bytes"
                )))
            }
            Ok(()) => {
                return publish_blob(
                    transport.registry,
                    what,
                    &tmp,
                    dest,
                    writer,
                    progress,
                    attempt,
                )
                .await;
            }
            Err(failure) => Some(failure),
        };

        let Some(failure) = failure else {
            return Err(blob_validation_error(
                transport.registry,
                format!("Failed to pull {what}: transfer ended without a terminal result"),
            ));
        };
        if failure.reset_partial {
            writer
                .reset()
                .await
                .map_err(|error| blob_io_error(transport.registry, what, "reset", error))?;
        }
        if !failure.retryable || attempt == transport.policy.max_attempts() {
            return Err(blob_validation_error(
                transport.registry,
                format!(
                    "Failed to pull {what} after {attempt} attempt(s); downloaded {} of {expected_size} bytes: {}",
                    writer.bytes_written(),
                    failure.message
                ),
            ));
        }

        writer
            .flush()
            .await
            .map_err(|error| blob_io_error(transport.registry, what, "flush", error))?;
        let delay = transport.policy.retry_delay(attempt);
        if let Some(reporter) = progress.as_mut() {
            reporter.retrying(writer.bytes_written(), attempt + 1, delay);
        }
        tracing::warn!(
            registry = transport.registry,
            digest = %descriptor.digest,
            attempt,
            next_attempt = attempt + 1,
            downloaded_bytes = writer.bytes_written(),
            retry_delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            error = %failure.message,
            "Retrying registry blob transfer"
        );
        tokio::time::sleep(delay).await;
    }

    Err(blob_validation_error(
        transport.registry,
        format!("Failed to pull {what}: retry policy exhausted"),
    ))
}

async fn publish_blob(
    registry: &str,
    what: &str,
    tmp: &Path,
    dest: &Path,
    mut writer: HashingFileWriter,
    mut progress: Option<ProgressReporter>,
    attempt: usize,
) -> Result<()> {
    writer
        .flush()
        .await
        .map_err(|error| blob_io_error(registry, what, "flush", error))?;
    writer
        .shutdown()
        .await
        .map_err(|error| blob_io_error(registry, what, "close", error))?;
    let actual_size = writer.bytes_written();
    let _ = writer.finalize_hex();
    tokio::fs::rename(tmp, dest)
        .await
        .map_err(|error| blob_io_error(registry, what, "publish", error))?;
    if let Some(reporter) = progress.as_mut() {
        reporter.complete(actual_size, attempt);
    }
    Ok(())
}

fn blob_validation_error(registry: &str, message: String) -> BoxError {
    BoxError::RegistryError {
        registry: registry.to_string(),
        message,
    }
}

fn blob_io_error(registry: &str, what: &str, operation: &str, error: std::io::Error) -> BoxError {
    BoxError::RegistryError {
        registry: registry.to_string(),
        message: format!("Failed to {operation} {what} file: {error}"),
    }
}

fn blob_file_error(path: &Path, operation: &str, error: std::io::Error) -> BoxError {
    BoxError::OciImageError(format!(
        "Failed to {operation} registry partial blob {}: {error}",
        path.display()
    ))
}
