use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use a3s_box_core::{
    FileOp, FileRequest, FileResponse, FilesystemEntry, FilesystemOp, FilesystemRequest,
    FilesystemResponse, MAX_BOUNDED_FILE_BYTES,
};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use super::SandboxInner;
use crate::{ClientError, Result};

/// Maximum size accepted by the in-memory single-file artifact export API.
pub const MAX_ARTIFACT_BYTES: u64 = MAX_BOUNDED_FILE_BYTES;

/// Metadata returned after a successful file write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteInfo {
    pub path: String,
    pub size: u64,
}

/// One verified guest file exported as a bounded build or test artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub path: String,
    pub data: Vec<u8>,
    pub size: u64,
    pub sha256: String,
    pub host_path: Option<PathBuf>,
}

/// Limits and optional host destination for [`Filesystem::export_with_options`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactExportOptions {
    pub max_bytes: u64,
    pub destination: Option<PathBuf>,
    pub user: Option<String>,
}

impl Default for ArtifactExportOptions {
    fn default() -> Self {
        Self {
            max_bytes: MAX_ARTIFACT_BYTES,
            destination: None,
            user: None,
        }
    }
}

impl ArtifactExportOptions {
    pub fn max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    pub fn destination(mut self, destination: impl Into<PathBuf>) -> Self {
        self.destination = Some(destination.into());
        self
    }

    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }
}

/// Optional guest identity for a filesystem operation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilesystemOptions {
    pub user: Option<String>,
}

impl FilesystemOptions {
    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }
}

/// File namespace attached to a local [`super::Sandbox`].
#[derive(Clone)]
pub struct Filesystem {
    pub(crate) inner: Arc<SandboxInner>,
}

impl std::fmt::Debug for Filesystem {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Filesystem")
            .field("sandbox_id", &self.inner.execution_id)
            .finish()
    }
}

impl Filesystem {
    pub async fn write(
        &self,
        path: impl Into<String>,
        data: impl AsRef<[u8]>,
    ) -> Result<WriteInfo> {
        self.write_with_options(path, data, FilesystemOptions::default())
            .await
    }

    pub async fn write_with_options(
        &self,
        path: impl Into<String>,
        data: impl AsRef<[u8]>,
        options: FilesystemOptions,
    ) -> Result<WriteInfo> {
        let path = path.into();
        let response = self
            .transfer(FileRequest {
                op: FileOp::Upload,
                guest_path: path.clone(),
                data: Some(STANDARD.encode(data.as_ref())),
                user: options.user,
                max_bytes: None,
            })
            .await?;
        require_file_success(response).map(|response| WriteInfo {
            path,
            size: response.size,
        })
    }

    pub async fn read(&self, path: impl Into<String>) -> Result<Vec<u8>> {
        self.read_with_options(path, FilesystemOptions::default())
            .await
    }

    pub async fn read_with_options(
        &self,
        path: impl Into<String>,
        options: FilesystemOptions,
    ) -> Result<Vec<u8>> {
        self.read_with_limit(path.into(), options, None).await
    }

    /// Read one file while asking the execution backend to enforce a
    /// decoded-byte ceiling.
    pub async fn read_bounded_with_options(
        &self,
        path: impl Into<String>,
        max_bytes: u64,
        options: FilesystemOptions,
    ) -> Result<Vec<u8>> {
        validate_artifact_limit(max_bytes)?;
        self.read_with_limit(path.into(), options, Some(max_bytes))
            .await
    }

    async fn read_with_limit(
        &self,
        path: String,
        options: FilesystemOptions,
        max_bytes: Option<u64>,
    ) -> Result<Vec<u8>> {
        let response = self
            .transfer(FileRequest {
                op: FileOp::Download,
                guest_path: path,
                data: None,
                user: options.user,
                max_bytes,
            })
            .await?;
        let response = require_file_success(response)?;
        let data = STANDARD
            .decode(response.data.unwrap_or_default())
            .map_err(|error| {
                ClientError::Guest(format!("guest returned invalid file data: {error}"))
            })?;
        let actual_size = u64::try_from(data.len()).map_err(|_| {
            ClientError::Guest("guest file size cannot be represented as u64".to_string())
        })?;
        if response.size != actual_size {
            return Err(ClientError::Guest(format!(
                "guest returned {} file bytes but declared {}",
                actual_size, response.size
            )));
        }
        if max_bytes.is_some_and(|limit| actual_size > limit) {
            return Err(ClientError::Guest(format!(
                "guest returned {actual_size} file bytes beyond max_bytes"
            )));
        }
        Ok(data)
    }

    pub async fn read_text(&self, path: impl Into<String>) -> Result<String> {
        String::from_utf8(self.read(path).await?)
            .map_err(|error| ClientError::Guest(format!("guest file is not valid UTF-8: {error}")))
    }

    /// Export one guest file with a fixed single-frame product ceiling.
    pub async fn export(&self, path: impl Into<String>) -> Result<Artifact> {
        self.export_with_options(path, ArtifactExportOptions::default())
            .await
    }

    /// Export one guest file after declared-size and stat/read size validation.
    ///
    /// When `destination` is set, the host file is created exclusively and an
    /// existing path is never overwritten.
    pub async fn export_with_options(
        &self,
        path: impl Into<String>,
        options: ArtifactExportOptions,
    ) -> Result<Artifact> {
        let path = path.into();
        if path.trim().is_empty() {
            return Err(ClientError::Validation(
                "artifact source path cannot be empty".to_string(),
            ));
        }
        validate_artifact_limit(options.max_bytes)?;
        if options
            .destination
            .as_deref()
            .is_some_and(|destination| destination.as_os_str().to_string_lossy().trim().is_empty())
        {
            return Err(ClientError::Validation(
                "artifact destination cannot be empty".to_string(),
            ));
        }

        let filesystem_options = FilesystemOptions {
            user: options.user.clone(),
        };
        let entry = self
            .stat_with_options(path.clone(), filesystem_options.clone())
            .await?;
        if entry.kind != a3s_box_core::FilesystemEntryKind::File {
            return Err(ClientError::Validation(format!(
                "artifact source {path:?} must be a file"
            )));
        }
        let stat_size = u64::try_from(entry.size).map_err(|_| {
            ClientError::Guest("guest returned a negative artifact size".to_string())
        })?;
        if stat_size > options.max_bytes {
            return Err(ClientError::Validation(format!(
                "artifact source is {stat_size} bytes; max_bytes is {}",
                options.max_bytes
            )));
        }

        let data = self
            .read_bounded_with_options(path.clone(), options.max_bytes, filesystem_options)
            .await?;
        let actual_size = u64::try_from(data.len()).map_err(|_| {
            ClientError::Guest("artifact size cannot be represented as u64".to_string())
        })?;
        if actual_size > options.max_bytes {
            return Err(ClientError::Guest(format!(
                "artifact source grew beyond max_bytes ({}) while reading",
                options.max_bytes
            )));
        }
        if actual_size != stat_size {
            return Err(ClientError::Guest(
                "artifact source changed size while it was being exported".to_string(),
            ));
        }
        if let Some(destination) = options.destination.as_deref() {
            write_new_host_file(destination, &data).await?;
        }

        Ok(Artifact {
            path,
            sha256: format!("{:x}", Sha256::digest(&data)),
            size: actual_size,
            data,
            host_path: options.destination,
        })
    }

    pub async fn stat(&self, path: impl Into<String>) -> Result<FilesystemEntry> {
        self.stat_with_options(path, FilesystemOptions::default())
            .await
    }

    pub async fn stat_with_options(
        &self,
        path: impl Into<String>,
        options: FilesystemOptions,
    ) -> Result<FilesystemEntry> {
        let response = self
            .filesystem(FilesystemRequest {
                op: FilesystemOp::Stat,
                path: path.into(),
                destination: None,
                depth: 0,
                user: options.user,
            })
            .await?;
        let mut response = require_filesystem_success(response)?;
        response.entry.take().ok_or_else(|| {
            ClientError::Guest("guest stat response did not include an entry".to_string())
        })
    }

    pub async fn exists(&self, path: impl Into<String>) -> Result<bool> {
        match self.stat(path).await {
            Ok(_) => Ok(true),
            Err(ClientError::Guest(message))
                if message.to_ascii_lowercase().contains("not found") =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    pub async fn list(&self, path: impl Into<String>, depth: u32) -> Result<Vec<FilesystemEntry>> {
        self.list_with_options(path, depth, FilesystemOptions::default())
            .await
    }

    pub async fn list_with_options(
        &self,
        path: impl Into<String>,
        depth: u32,
        options: FilesystemOptions,
    ) -> Result<Vec<FilesystemEntry>> {
        let response = self
            .filesystem(FilesystemRequest {
                op: FilesystemOp::ListDir,
                path: path.into(),
                destination: None,
                depth,
                user: options.user,
            })
            .await?;
        Ok(require_filesystem_success(response)?.entries)
    }

    pub async fn make_dir(&self, path: impl Into<String>) -> Result<()> {
        self.make_dir_with_options(path, FilesystemOptions::default())
            .await
    }

    pub async fn make_dir_with_options(
        &self,
        path: impl Into<String>,
        options: FilesystemOptions,
    ) -> Result<()> {
        self.mutate(FilesystemRequest {
            op: FilesystemOp::MakeDir,
            path: path.into(),
            destination: None,
            depth: 0,
            user: options.user,
        })
        .await
    }

    pub async fn move_path(
        &self,
        source: impl Into<String>,
        destination: impl Into<String>,
    ) -> Result<()> {
        self.move_path_with_options(source, destination, FilesystemOptions::default())
            .await
    }

    pub async fn move_path_with_options(
        &self,
        source: impl Into<String>,
        destination: impl Into<String>,
        options: FilesystemOptions,
    ) -> Result<()> {
        self.mutate(FilesystemRequest {
            op: FilesystemOp::Move,
            path: source.into(),
            destination: Some(destination.into()),
            depth: 0,
            user: options.user,
        })
        .await
    }

    pub async fn remove(&self, path: impl Into<String>) -> Result<()> {
        self.remove_with_options(path, FilesystemOptions::default())
            .await
    }

    pub async fn remove_with_options(
        &self,
        path: impl Into<String>,
        options: FilesystemOptions,
    ) -> Result<()> {
        self.mutate(FilesystemRequest {
            op: FilesystemOp::Remove,
            path: path.into(),
            destination: None,
            depth: 0,
            user: options.user,
        })
        .await
    }

    async fn mutate(&self, request: FilesystemRequest) -> Result<()> {
        require_filesystem_success(self.filesystem(request).await?).map(|_| ())
    }

    async fn transfer(&self, request: FileRequest) -> Result<FileResponse> {
        let (_, generation) = self.inner.active_execution()?;
        self.inner
            .client
            .transfer_execution_file(&self.inner.execution_id, generation, request)
            .await
    }

    async fn filesystem(&self, request: FilesystemRequest) -> Result<FilesystemResponse> {
        let (_, generation) = self.inner.active_execution()?;
        self.inner
            .client
            .filesystem_execution(&self.inner.execution_id, generation, request)
            .await
    }
}

fn validate_artifact_limit(max_bytes: u64) -> Result<()> {
    if max_bytes == 0 || max_bytes > MAX_ARTIFACT_BYTES {
        return Err(ClientError::Validation(format!(
            "artifact max_bytes must be between 1 and {MAX_ARTIFACT_BYTES}"
        )));
    }
    Ok(())
}

async fn write_new_host_file(destination: &Path, data: &[u8]) -> Result<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .await
        .map_err(|error| {
            ClientError::State(io::Error::new(
                error.kind(),
                format!(
                    "could not create artifact destination {}: {error}",
                    destination.display()
                ),
            ))
        })?;

    let write_result = async {
        file.write_all(data).await?;
        file.sync_all().await
    }
    .await;
    drop(file);

    if let Err(error) = write_result {
        let cleanup_error = tokio::fs::remove_file(destination).await.err();
        let cleanup = cleanup_error.map_or_else(String::new, |cleanup_error| {
            format!("; partial-file cleanup failed: {cleanup_error}")
        });
        return Err(ClientError::State(io::Error::new(
            error.kind(),
            format!(
                "could not write artifact destination {}: {error}{cleanup}",
                destination.display()
            ),
        )));
    }
    Ok(())
}

fn require_file_success(response: FileResponse) -> Result<FileResponse> {
    if response.success {
        Ok(response)
    } else {
        Err(ClientError::Guest(response.error.unwrap_or_else(|| {
            "guest file operation failed".to_string()
        })))
    }
}

fn require_filesystem_success(response: FilesystemResponse) -> Result<FilesystemResponse> {
    if response.success {
        Ok(response)
    } else {
        Err(ClientError::Guest(response.error.unwrap_or_else(|| {
            "guest filesystem operation failed".to_string()
        })))
    }
}
