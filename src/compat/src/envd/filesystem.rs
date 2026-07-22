//! Generation-fenced implementation of the pinned E2B Filesystem service.

use std::collections::BTreeMap;
use std::sync::Arc;

use a3s_box_core::{
    ExecutionGeneration, ExecutionId, ExecutionManagerError, ExecutionSessionManager,
    FilesystemEntry as SessionEntry, FilesystemEntryKind, FilesystemOp, FilesystemRequest,
    FilesystemResponse,
};
use axum::body::Body;
use axum::http::{Method, Request, Response};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize, Serializer};

use super::connect::{decode_unary, stream_encoding, stream_error, unary_ok, ConnectFailure};
use super::process::process_user;

const MAX_PATH_BYTES: usize = 4096;

#[derive(Clone)]
pub(super) struct FilesystemBroker {
    sessions: Arc<dyn ExecutionSessionManager>,
}

impl FilesystemBroker {
    pub(super) fn new(sessions: Arc<dyn ExecutionSessionManager>) -> Self {
        Self { sessions }
    }

    pub(super) async fn handle(
        &self,
        request: Request<Body>,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> Response<Body> {
        if request.method() != Method::POST {
            return ConnectFailure::invalid_argument("Connect procedures require POST")
                .unary_response();
        }
        match request.uri().path() {
            "/filesystem.Filesystem/Stat" => self.stat(request, execution_id, generation).await,
            "/filesystem.Filesystem/MakeDir" => {
                self.make_dir(request, execution_id, generation).await
            }
            "/filesystem.Filesystem/Move" => {
                self.move_entry(request, execution_id, generation).await
            }
            "/filesystem.Filesystem/ListDir" => {
                self.list_dir(request, execution_id, generation).await
            }
            "/filesystem.Filesystem/Remove" => self.remove(request, execution_id, generation).await,
            "/filesystem.Filesystem/WatchDir" => unsupported_watch(request),
            "/filesystem.Filesystem/CreateWatcher"
            | "/filesystem.Filesystem/GetWatcherEvents"
            | "/filesystem.Filesystem/RemoveWatcher" => ConnectFailure::unimplemented(
                "filesystem watcher procedures are not implemented by broker mode",
            )
            .unary_response(),
            _ => ConnectFailure::not_found("Filesystem procedure not found").unary_response(),
        }
    }

    async fn stat(
        &self,
        request: Request<Body>,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> Response<Body> {
        let user = match process_user(request.headers()) {
            Ok(user) => user,
            Err(error) => return error.unary_response(),
        };
        let (request, encoding): (StatRequest, _) = match decode_unary(request).await {
            Ok(decoded) => decoded,
            Err(error) => return error.unary_response(),
        };
        if let Err(error) = validate_path(&request.path, "StatRequest.path") {
            return error.unary_response();
        }
        let response = self
            .invoke(
                execution_id,
                generation,
                FilesystemRequest {
                    op: FilesystemOp::Stat,
                    path: request.path,
                    destination: None,
                    depth: 0,
                    user,
                },
            )
            .await;
        match response.and_then(|response| required_entry(response, "Stat")) {
            Ok(entry) => unary_ok(
                &StatResponse {
                    entry: Some(entry.into()),
                },
                encoding,
            ),
            Err(error) => error.unary_response(),
        }
    }

    async fn make_dir(
        &self,
        request: Request<Body>,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> Response<Body> {
        let user = match process_user(request.headers()) {
            Ok(user) => user,
            Err(error) => return error.unary_response(),
        };
        let (request, encoding): (MakeDirRequest, _) = match decode_unary(request).await {
            Ok(decoded) => decoded,
            Err(error) => return error.unary_response(),
        };
        if let Err(error) = validate_path(&request.path, "MakeDirRequest.path") {
            return error.unary_response();
        }
        let response = self
            .invoke(
                execution_id,
                generation,
                FilesystemRequest {
                    op: FilesystemOp::MakeDir,
                    path: request.path,
                    destination: None,
                    depth: 0,
                    user,
                },
            )
            .await;
        match response.and_then(|response| required_entry(response, "MakeDir")) {
            Ok(entry) => unary_ok(
                &MakeDirResponse {
                    entry: Some(entry.into()),
                },
                encoding,
            ),
            Err(error) => error.unary_response(),
        }
    }

    async fn move_entry(
        &self,
        request: Request<Body>,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> Response<Body> {
        let user = match process_user(request.headers()) {
            Ok(user) => user,
            Err(error) => return error.unary_response(),
        };
        let (request, encoding): (MoveRequest, _) = match decode_unary(request).await {
            Ok(decoded) => decoded,
            Err(error) => return error.unary_response(),
        };
        for (path, label) in [
            (&request.source, "MoveRequest.source"),
            (&request.destination, "MoveRequest.destination"),
        ] {
            if let Err(error) = validate_path(path, label) {
                return error.unary_response();
            }
        }
        let response = self
            .invoke(
                execution_id,
                generation,
                FilesystemRequest {
                    op: FilesystemOp::Move,
                    path: request.source,
                    destination: Some(request.destination),
                    depth: 0,
                    user,
                },
            )
            .await;
        match response.and_then(|response| required_entry(response, "Move")) {
            Ok(entry) => unary_ok(
                &MoveResponse {
                    entry: Some(entry.into()),
                },
                encoding,
            ),
            Err(error) => error.unary_response(),
        }
    }

    async fn list_dir(
        &self,
        request: Request<Body>,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> Response<Body> {
        let user = match process_user(request.headers()) {
            Ok(user) => user,
            Err(error) => return error.unary_response(),
        };
        let (request, encoding): (ListDirRequest, _) = match decode_unary(request).await {
            Ok(decoded) => decoded,
            Err(error) => return error.unary_response(),
        };
        if let Err(error) = validate_path(&request.path, "ListDirRequest.path") {
            return error.unary_response();
        }
        let response = self
            .invoke(
                execution_id,
                generation,
                FilesystemRequest {
                    op: FilesystemOp::ListDir,
                    path: request.path,
                    destination: None,
                    depth: request.depth,
                    user,
                },
            )
            .await;
        match response {
            Ok(response) => unary_ok(
                &ListDirResponse {
                    entries: response.entries.into_iter().map(EntryInfo::from).collect(),
                },
                encoding,
            ),
            Err(error) => error.unary_response(),
        }
    }

    async fn remove(
        &self,
        request: Request<Body>,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> Response<Body> {
        let user = match process_user(request.headers()) {
            Ok(user) => user,
            Err(error) => return error.unary_response(),
        };
        let (request, encoding): (RemoveRequest, _) = match decode_unary(request).await {
            Ok(decoded) => decoded,
            Err(error) => return error.unary_response(),
        };
        if let Err(error) = validate_path(&request.path, "RemoveRequest.path") {
            return error.unary_response();
        }
        match self
            .invoke(
                execution_id,
                generation,
                FilesystemRequest {
                    op: FilesystemOp::Remove,
                    path: request.path,
                    destination: None,
                    depth: 0,
                    user,
                },
            )
            .await
        {
            Ok(_) => unary_ok(&RemoveResponse {}, encoding),
            Err(error) => error.unary_response(),
        }
    }

    async fn invoke(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        request: FilesystemRequest,
    ) -> Result<FilesystemResponse, ConnectFailure> {
        let response = self
            .sessions
            .filesystem(execution_id, generation, request)
            .await
            .map_err(manager_failure)?;
        if response.success {
            Ok(response)
        } else {
            Err(guest_failure(response))
        }
    }
}

fn unsupported_watch(request: Request<Body>) -> Response<Body> {
    let error = ConnectFailure::unimplemented(
        "filesystem watch streams are not implemented by broker mode",
    );
    match stream_encoding(&request) {
        Ok(encoding) => stream_error(&error, encoding),
        Err(media_error) => media_error.unary_response(),
    }
}

fn validate_path(path: &str, label: &str) -> Result<(), ConnectFailure> {
    if path.contains('\0') || path.len() > MAX_PATH_BYTES {
        return Err(ConnectFailure::invalid_argument(format!(
            "{label} must not contain NUL or exceed {MAX_PATH_BYTES} bytes"
        )));
    }
    Ok(())
}

fn required_entry(
    response: FilesystemResponse,
    procedure: &str,
) -> Result<SessionEntry, ConnectFailure> {
    response.entry.ok_or_else(|| {
        ConnectFailure::internal(format!(
            "guest returned a successful {procedure} response without entry metadata"
        ))
    })
}

fn manager_failure(error: ExecutionManagerError) -> ConnectFailure {
    match error {
        ExecutionManagerError::InvalidRequest(message) => ConnectFailure::invalid_argument(message),
        ExecutionManagerError::NotFound(execution_id) => {
            ConnectFailure::not_found(format!("execution {execution_id} not found"))
        }
        ExecutionManagerError::Conflict { message, .. } => {
            ConnectFailure::failed_precondition(message)
        }
        ExecutionManagerError::Unavailable(message) => ConnectFailure::unavailable(message),
        ExecutionManagerError::Internal(message) => ConnectFailure::internal(message),
    }
}

fn guest_failure(response: FilesystemResponse) -> ConnectFailure {
    let message = response
        .error
        .unwrap_or_else(|| "guest filesystem operation failed without an error".to_string());
    let lower = message.to_ascii_lowercase();
    if lower.contains("not found") || lower.contains("does not exist") {
        ConnectFailure::not_found(message)
    } else if lower.contains("already exists") {
        ConnectFailure::already_exists(message)
    } else if lower.contains("user") && (lower.contains("invalid") || lower.contains("unknown")) {
        ConnectFailure::unauthenticated(message)
    } else if lower.contains("limit") {
        ConnectFailure::resource_exhausted(message)
    } else if lower.contains("path")
        || lower.contains("directory")
        || lower.contains("destination")
        || lower.contains("refusing")
    {
        ConnectFailure::invalid_argument(message)
    } else {
        ConnectFailure::internal(message)
    }
}

#[derive(Clone, PartialEq, ::prost::Message, Deserialize)]
struct StatRequest {
    #[prost(string, tag = "1")]
    path: String,
}

#[derive(Clone, PartialEq, ::prost::Message, Deserialize)]
struct MakeDirRequest {
    #[prost(string, tag = "1")]
    path: String,
}

#[derive(Clone, PartialEq, ::prost::Message, Deserialize)]
struct MoveRequest {
    #[prost(string, tag = "1")]
    source: String,
    #[prost(string, tag = "2")]
    destination: String,
}

#[derive(Clone, PartialEq, ::prost::Message, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListDirRequest {
    #[prost(string, tag = "1")]
    path: String,
    #[prost(uint32, tag = "2")]
    depth: u32,
}

#[derive(Clone, PartialEq, ::prost::Message, Deserialize)]
struct RemoveRequest {
    #[prost(string, tag = "1")]
    path: String,
}

#[derive(Clone, PartialEq, ::prost::Message, Serialize)]
struct StatResponse {
    #[prost(message, optional, tag = "1")]
    #[serde(skip_serializing_if = "Option::is_none")]
    entry: Option<EntryInfo>,
}

#[derive(Clone, PartialEq, ::prost::Message, Serialize)]
struct MakeDirResponse {
    #[prost(message, optional, tag = "1")]
    #[serde(skip_serializing_if = "Option::is_none")]
    entry: Option<EntryInfo>,
}

#[derive(Clone, PartialEq, ::prost::Message, Serialize)]
struct MoveResponse {
    #[prost(message, optional, tag = "1")]
    #[serde(skip_serializing_if = "Option::is_none")]
    entry: Option<EntryInfo>,
}

#[derive(Clone, PartialEq, ::prost::Message, Serialize)]
struct ListDirResponse {
    #[prost(message, repeated, tag = "1")]
    entries: Vec<EntryInfo>,
}

#[derive(Clone, PartialEq, ::prost::Message, Serialize)]
struct RemoveResponse {}

#[derive(Clone, PartialEq, ::prost::Message, Serialize)]
#[serde(rename_all = "camelCase")]
struct EntryInfo {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(enumeration = "FileType", tag = "2")]
    #[serde(rename = "type", serialize_with = "serialize_file_type")]
    kind: i32,
    #[prost(string, tag = "3")]
    path: String,
    #[prost(int64, tag = "4")]
    #[serde(serialize_with = "serialize_i64")]
    size: i64,
    #[prost(uint32, tag = "5")]
    mode: u32,
    #[prost(string, tag = "6")]
    permissions: String,
    #[prost(string, tag = "7")]
    owner: String,
    #[prost(string, tag = "8")]
    group: String,
    #[prost(message, optional, tag = "9")]
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_timestamp"
    )]
    modified_time: Option<ProtoTimestamp>,
    #[prost(string, optional, tag = "10")]
    #[serde(skip_serializing_if = "Option::is_none")]
    symlink_target: Option<String>,
    #[prost(btree_map = "string, string", tag = "11")]
    metadata: BTreeMap<String, String>,
}

impl From<SessionEntry> for EntryInfo {
    fn from(entry: SessionEntry) -> Self {
        let kind = match entry.kind {
            FilesystemEntryKind::Unspecified => FileType::Unspecified,
            FilesystemEntryKind::File => FileType::File,
            FilesystemEntryKind::Directory => FileType::Directory,
        } as i32;
        Self {
            name: entry.name,
            kind,
            path: entry.path,
            size: entry.size,
            mode: entry.mode,
            permissions: entry.permissions,
            owner: entry.owner,
            group: entry.group,
            modified_time: Some(ProtoTimestamp {
                seconds: entry.modified_seconds,
                nanos: entry.modified_nanos,
            }),
            symlink_target: entry.symlink_target,
            metadata: entry.metadata,
        }
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
struct ProtoTimestamp {
    #[prost(int64, tag = "1")]
    seconds: i64,
    #[prost(int32, tag = "2")]
    nanos: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ::prost::Enumeration)]
#[repr(i32)]
enum FileType {
    Unspecified = 0,
    File = 1,
    Directory = 2,
}

fn serialize_file_type<S>(value: &i32, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let name = match *value {
        1 => "FILE_TYPE_FILE",
        2 => "FILE_TYPE_DIRECTORY",
        _ => "FILE_TYPE_UNSPECIFIED",
    };
    serializer.serialize_str(name)
}

fn serialize_i64<S>(value: &i64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&value.to_string())
}

fn serialize_optional_timestamp<S>(
    value: &Option<ProtoTimestamp>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let Some(timestamp) = value else {
        return serializer.serialize_none();
    };
    let nanos = u32::try_from(timestamp.nanos).map_err(serde::ser::Error::custom)?;
    let datetime = DateTime::<Utc>::from_timestamp(timestamp.seconds, nanos)
        .ok_or_else(|| serde::ser::Error::custom("filesystem timestamp is out of range"))?;
    serializer.serialize_some(&datetime.to_rfc3339_opts(SecondsFormat::AutoSi, true))
}

#[cfg(test)]
#[path = "filesystem_tests.rs"]
mod tests;
