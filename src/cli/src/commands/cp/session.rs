//! Runtime-route selection for `a3s-box cp`.

use std::path::PathBuf;
use std::sync::Arc;

use a3s_box_core::exec::{
    ExecOutput, ExecRequest, FileRequest, FileResponse, FilesystemRequest, FilesystemResponse,
};
use a3s_box_core::{ExecutionGeneration, ExecutionId, ExecutionSessionManager};
use a3s_box_runtime::ExecClient;

use crate::resolve;
use crate::state::{BoxRecord, StateFile};

/// Backend-neutral copy channel selected from the record's durable runtime route.
pub(super) enum CopySession {
    Managed {
        manager: Arc<dyn ExecutionSessionManager>,
        execution_id: ExecutionId,
        generation: ExecutionGeneration,
    },
    Legacy(ExecClient),
}

impl CopySession {
    pub(super) async fn execute(
        &self,
        request: ExecRequest,
    ) -> Result<ExecOutput, Box<dyn std::error::Error>> {
        match self {
            Self::Managed {
                manager,
                execution_id,
                generation,
            } => manager
                .execute(execution_id, *generation, request)
                .await
                .map_err(|error| error.into()),
            Self::Legacy(client) => client
                .exec_command(&request)
                .await
                .map_err(|error| error.into()),
        }
    }

    pub(super) async fn transfer_file(
        &self,
        request: FileRequest,
    ) -> Result<FileResponse, Box<dyn std::error::Error>> {
        match self {
            Self::Managed {
                manager,
                execution_id,
                generation,
            } => manager
                .transfer_file(execution_id, *generation, request)
                .await
                .map_err(|error| error.into()),
            Self::Legacy(client) => client
                .file_transfer(&request)
                .await
                .map_err(|error| error.into()),
        }
    }

    pub(super) async fn filesystem(
        &self,
        request: FilesystemRequest,
    ) -> Result<FilesystemResponse, Box<dyn std::error::Error>> {
        match self {
            Self::Managed {
                manager,
                execution_id,
                generation,
            } => manager
                .filesystem(execution_id, *generation, request)
                .await
                .map_err(|error| error.into()),
            Self::Legacy(client) => client
                .filesystem(&request)
                .await
                .map_err(|error| error.into()),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum CopyRoute {
    Managed {
        execution_id: ExecutionId,
        generation: ExecutionGeneration,
    },
    Legacy {
        exec_socket_path: PathBuf,
    },
}

/// Select the copy transport without allowing OCI-routed records to fall back
/// to a Box-owned compatibility socket.
pub(super) fn resolve_copy_route(
    record: &BoxRecord,
) -> Result<CopyRoute, Box<dyn std::error::Error>> {
    if record
        .managed_execution
        .as_ref()
        .is_some_and(a3s_box_runtime::ManagedExecutionMetadata::is_oci_routed)
    {
        if record.status != "running" {
            return Err(format!("Box {} is not running", record.name).into());
        }
        let metadata = record
            .managed_execution
            .as_ref()
            .ok_or_else(|| format!("Box {} lost managed execution metadata", record.name))?;
        return Ok(CopyRoute::Managed {
            execution_id: ExecutionId::new(record.id.clone())?,
            generation: metadata.generation,
        });
    }

    let exec_socket_path = crate::socket_paths::require_runtime_socket(
        record,
        crate::socket_paths::RuntimeSocket::Exec,
    )
    .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
    Ok(CopyRoute::Legacy { exec_socket_path })
}

/// Connect to the persisted runtime route for one box.
pub(super) async fn connect_copy_session(
    box_name: &str,
) -> Result<CopySession, Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, box_name)?;
    match resolve_copy_route(record)? {
        CopyRoute::Managed {
            execution_id,
            generation,
        } => {
            let home = a3s_box_core::dirs_home();
            let manager = super::super::configured_local_execution_manager(&home).await?;
            Ok(CopySession::Managed {
                manager: Arc::new(manager),
                execution_id,
                generation,
            })
        }
        CopyRoute::Legacy { exec_socket_path } => Ok(CopySession::Legacy(
            ExecClient::connect(&exec_socket_path).await?,
        )),
    }
}
