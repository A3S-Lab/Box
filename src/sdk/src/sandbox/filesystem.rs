use std::sync::Arc;

use a3s_box_core::{
    FileOp, FileRequest, FileResponse, FilesystemEntry, FilesystemOp, FilesystemRequest,
    FilesystemResponse,
};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;

use super::SandboxInner;
use crate::{ClientError, Result};

/// Metadata returned after a successful file write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteInfo {
    pub path: String,
    pub size: u64,
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

/// E2B-style file namespace attached to a local [`super::Sandbox`].
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
        let response = self
            .transfer(FileRequest {
                op: FileOp::Download,
                guest_path: path.into(),
                data: None,
                user: options.user,
            })
            .await?;
        let response = require_file_success(response)?;
        STANDARD
            .decode(response.data.unwrap_or_default())
            .map_err(|error| {
                ClientError::Guest(format!("guest returned invalid file data: {error}"))
            })
    }

    pub async fn read_text(&self, path: impl Into<String>) -> Result<String> {
        String::from_utf8(self.read(path).await?)
            .map_err(|error| ClientError::Guest(format!("guest file is not valid UTF-8: {error}")))
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
        #[cfg(unix)]
        {
            self.inner
                .client
                .transfer_execution_file(&self.inner.execution_id, generation, request)
                .await
        }
        #[cfg(not(unix))]
        {
            let _ = (generation, request);
            Err(ClientError::Execution(
                a3s_box_core::ExecutionManagerError::Unavailable(
                    "local file sessions are not available on this host".to_string(),
                ),
            ))
        }
    }

    async fn filesystem(&self, request: FilesystemRequest) -> Result<FilesystemResponse> {
        let (_, generation) = self.inner.active_execution()?;
        #[cfg(unix)]
        {
            self.inner
                .client
                .filesystem_execution(&self.inner.execution_id, generation, request)
                .await
        }
        #[cfg(not(unix))]
        {
            let _ = (generation, request);
            Err(ClientError::Execution(
                a3s_box_core::ExecutionManagerError::Unavailable(
                    "local filesystem sessions are not available on this host".to_string(),
                ),
            ))
        }
    }
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
