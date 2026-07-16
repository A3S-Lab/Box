use std::collections::BTreeMap;

use axum::body::{boxed, Body};
use axum::extract::{rejection::JsonRejection, BodyStream, Path, RawQuery, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use crate::control::SecretToken;
use crate::volume::{
    AuthorizedVolume, VolumeContentError, VolumeEntry, VolumeId, VolumeMetadataUpdate,
    VolumeServiceError, MAX_DIRECTORY_DEPTH,
};

use super::router::LifecycleHttpState;

pub async fn stat(
    State(state): State<LifecycleHttpState>,
    Path(volume_id): Path<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Json<VolumeEntry>, VolumeApiError> {
    let query = ContentQuery::parse(query.as_deref(), &["path"])?;
    let volume = authorize(&state, &headers, volume_id).await?;
    Ok(Json(
        state
            .volume_service()
            .map_err(|_| VolumeApiError::internal())?
            .filesystem()
            .stat(&volume.root, query.path()?)
            .await?,
    ))
}

pub async fn update_metadata(
    State(state): State<LifecycleHttpState>,
    Path(volume_id): Path<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: Result<Json<MetadataBody>, JsonRejection>,
) -> Result<Json<VolumeEntry>, VolumeApiError> {
    let query = ContentQuery::parse(query.as_deref(), &["path"])?;
    let volume = authorize(&state, &headers, volume_id).await?;
    let metadata = body.map_err(VolumeApiError::from)?.0.into();
    Ok(Json(
        state
            .volume_service()
            .map_err(|_| VolumeApiError::internal())?
            .filesystem()
            .update_metadata(&volume.root, query.path()?, metadata)
            .await?,
    ))
}

pub async fn remove(
    State(state): State<LifecycleHttpState>,
    Path(volume_id): Path<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<StatusCode, VolumeApiError> {
    let query = ContentQuery::parse(query.as_deref(), &["path"])?;
    let volume = authorize(&state, &headers, volume_id).await?;
    state
        .volume_service()
        .map_err(|_| VolumeApiError::internal())?
        .filesystem()
        .remove(&volume.root, query.path()?)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list(
    State(state): State<LifecycleHttpState>,
    Path(volume_id): Path<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Json<Vec<VolumeEntry>>, VolumeApiError> {
    let query = ContentQuery::parse(query.as_deref(), &["path", "depth"])?;
    let depth = query.optional_u32("depth")?.unwrap_or(1);
    if depth > MAX_DIRECTORY_DEPTH {
        return Err(VolumeApiError::bad_request(format!(
            "depth cannot exceed {MAX_DIRECTORY_DEPTH}"
        )));
    }
    let volume = authorize(&state, &headers, volume_id).await?;
    Ok(Json(
        state
            .volume_service()
            .map_err(|_| VolumeApiError::internal())?
            .filesystem()
            .list(&volume.root, query.path()?, depth)
            .await?,
    ))
}

pub async fn make_dir(
    State(state): State<LifecycleHttpState>,
    Path(volume_id): Path<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<impl IntoResponse, VolumeApiError> {
    let query = ContentQuery::parse(query.as_deref(), &["path", "uid", "gid", "mode", "force"])?;
    let path = query.path()?.to_string();
    let metadata = query.metadata()?;
    let force = query.optional_bool("force")?.unwrap_or(false);
    let volume = authorize(&state, &headers, volume_id).await?;
    let entry = state
        .volume_service()
        .map_err(|_| VolumeApiError::internal())?
        .filesystem()
        .make_dir(&volume.root, &path, metadata, force)
        .await?;
    Ok((StatusCode::CREATED, Json(entry)))
}

pub async fn read_file(
    State(state): State<LifecycleHttpState>,
    Path(volume_id): Path<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Response, VolumeApiError> {
    let query = ContentQuery::parse(query.as_deref(), &["path"])?;
    let volume = authorize(&state, &headers, volume_id).await?;
    let mut file = state
        .volume_service()
        .map_err(|_| VolumeApiError::internal())?
        .filesystem()
        .open_file(&volume.root, query.path()?)
        .await?;

    let (mut sender, body) = Body::channel();
    tokio::spawn(async move {
        loop {
            let mut buffer = vec![0_u8; 64 * 1024];
            let read = match file.read(&mut buffer).await {
                Ok(read) => read,
                Err(_) => return,
            };
            if read == 0 {
                return;
            }
            buffer.truncate(read);
            if sender.send_data(buffer.into()).await.is_err() {
                return;
            }
        }
    });
    let mut response = Response::new(boxed(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/octet-stream"),
    );
    Ok(response)
}

pub async fn write_file(
    State(state): State<LifecycleHttpState>,
    Path(volume_id): Path<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    mut body: BodyStream,
) -> Result<impl IntoResponse, VolumeApiError> {
    let query = ContentQuery::parse(query.as_deref(), &["path", "uid", "gid", "mode", "force"])?;
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        != Some("application/octet-stream")
    {
        return Err(VolumeApiError::bad_request(
            "Content-Type must be application/octet-stream",
        ));
    }
    let path = query.path()?.to_string();
    let metadata = query.metadata()?;
    // Both pinned SDKs document overwrite as the default. `force=false`
    // explicitly requests create-only behavior.
    let force = query.optional_bool("force")?.unwrap_or(true);
    let volume = authorize(&state, &headers, volume_id).await?;
    let mut upload = state
        .volume_service()
        .map_err(|_| VolumeApiError::internal())?
        .filesystem()
        .begin_write(&volume.root, &path, metadata, force)
        .await?;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|error| {
            VolumeApiError::bad_request(format!("failed to read upload body: {error}"))
        })?;
        upload.write_all(&chunk).await?;
    }
    let entry = upload.finish().await?;
    Ok((StatusCode::CREATED, Json(entry)))
}

async fn authorize(
    state: &LifecycleHttpState,
    headers: &HeaderMap,
    volume_id: String,
) -> Result<AuthorizedVolume, VolumeApiError> {
    let volume_id = VolumeId::new(volume_id).map_err(|_| VolumeApiError::not_found())?;
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty())
        .ok_or_else(VolumeApiError::unauthorized)?;
    let token =
        SecretToken::new(authorization.to_string()).map_err(|_| VolumeApiError::unauthorized())?;
    state
        .volume_service()
        .map_err(|_| VolumeApiError::internal())?
        .authorize(&volume_id, &token)
        .await
        .map_err(Into::into)
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataBody {
    uid: Option<u32>,
    gid: Option<u32>,
    mode: Option<u32>,
}

impl From<MetadataBody> for VolumeMetadataUpdate {
    fn from(value: MetadataBody) -> Self {
        Self {
            uid: value.uid,
            gid: value.gid,
            mode: value.mode,
        }
    }
}

struct ContentQuery {
    values: BTreeMap<String, String>,
}

impl ContentQuery {
    fn parse(raw: Option<&str>, allowed: &[&str]) -> Result<Self, VolumeApiError> {
        let mut values = BTreeMap::new();
        for (name, value) in url::form_urlencoded::parse(raw.unwrap_or_default().as_bytes()) {
            if !allowed.contains(&name.as_ref()) {
                return Err(VolumeApiError::bad_request(format!(
                    "unknown volume query parameter: {name}"
                )));
            }
            if values
                .insert(name.into_owned(), value.into_owned())
                .is_some()
            {
                return Err(VolumeApiError::bad_request(
                    "volume query parameters must not be repeated",
                ));
            }
        }
        Ok(Self { values })
    }

    fn path(&self) -> Result<&str, VolumeApiError> {
        self.values
            .get("path")
            .map(String::as_str)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| VolumeApiError::bad_request("path is required"))
    }

    fn optional_u32(&self, name: &str) -> Result<Option<u32>, VolumeApiError> {
        self.values
            .get(name)
            .map(|value| {
                value.parse::<u32>().map_err(|_| {
                    VolumeApiError::bad_request(format!("{name} must be an unsigned integer"))
                })
            })
            .transpose()
    }

    fn optional_bool(&self, name: &str) -> Result<Option<bool>, VolumeApiError> {
        self.values
            .get(name)
            .map(|value| match value.as_str() {
                "true" => Ok(true),
                "false" => Ok(false),
                _ => Err(VolumeApiError::bad_request(format!(
                    "{name} must be true or false"
                ))),
            })
            .transpose()
    }

    fn metadata(&self) -> Result<VolumeMetadataUpdate, VolumeApiError> {
        Ok(VolumeMetadataUpdate {
            uid: self.optional_u32("uid")?,
            gid: self.optional_u32("gid")?,
            mode: self.optional_u32("mode")?,
        })
    }
}

#[derive(Debug)]
pub struct VolumeApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl VolumeApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "bad_request",
            message: message.into(),
        }
    }

    fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "unauthorized",
            message: "Invalid volume authentication".to_string(),
        }
    }

    fn forbidden() -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: "forbidden",
            message: "Volume token does not authorize this volume".to_string(),
        }
    }

    fn not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: "Volume or path not found".to_string(),
        }
    }

    fn conflict() -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code: "conflict",
            message: "Volume path conflicts with an existing or active entry".to_string(),
        }
    }

    fn internal() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal_server_error",
            message: "Internal server error".to_string(),
        }
    }
}

#[derive(Serialize)]
struct VolumeErrorBody {
    code: &'static str,
    message: String,
}

impl IntoResponse for VolumeApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(VolumeErrorBody {
                code: self.code,
                message: self.message,
            }),
        )
            .into_response()
    }
}

impl From<JsonRejection> for VolumeApiError {
    fn from(error: JsonRejection) -> Self {
        Self::bad_request(format!("Invalid JSON request: {}", error.body_text()))
    }
}

impl From<VolumeContentError> for VolumeApiError {
    fn from(error: VolumeContentError) -> Self {
        match error {
            VolumeContentError::InvalidPath(message) => Self::bad_request(message),
            VolumeContentError::NotFound => Self::not_found(),
            VolumeContentError::Conflict => Self::conflict(),
            VolumeContentError::PermissionDenied => Self::forbidden(),
            VolumeContentError::Unsupported(_) | VolumeContentError::Unavailable(_) => {
                Self::internal()
            }
        }
    }
}

impl From<VolumeServiceError> for VolumeApiError {
    fn from(error: VolumeServiceError) -> Self {
        match error {
            VolumeServiceError::InvalidRequest(message) => Self::bad_request(message),
            VolumeServiceError::NotFound => Self::not_found(),
            VolumeServiceError::Forbidden => Self::forbidden(),
            VolumeServiceError::Duplicate | VolumeServiceError::Conflict => Self::conflict(),
            VolumeServiceError::Repository(_)
            | VolumeServiceError::Runtime(_)
            | VolumeServiceError::Credential(_)
            | VolumeServiceError::Model(_)
            | VolumeServiceError::Content(_) => Self::internal(),
        }
    }
}
