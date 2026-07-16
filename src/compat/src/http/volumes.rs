use axum::extract::{rejection::JsonRejection, Extension, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::volume::{VolumeConnection, VolumeId, VolumeRecord};

use super::error::ApiError;
use super::router::LifecycleHttpState;
use super::AuthenticatedAccount;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewVolumeBody {
    name: String,
}

#[derive(Debug, Serialize)]
pub struct VolumeResponse {
    #[serde(rename = "volumeID")]
    volume_id: String,
    name: String,
}

impl From<&VolumeRecord> for VolumeResponse {
    fn from(record: &VolumeRecord) -> Self {
        Self {
            volume_id: record.volume_id().to_string(),
            name: record.name().to_string(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct VolumeWithTokenResponse {
    #[serde(rename = "volumeID")]
    volume_id: String,
    name: String,
    token: String,
}

impl From<VolumeConnection> for VolumeWithTokenResponse {
    fn from(connection: VolumeConnection) -> Self {
        Self {
            volume_id: connection.record.volume_id().to_string(),
            name: connection.record.name().to_string(),
            token: connection.token.expose_secret().to_string(),
        }
    }
}

pub async fn create(
    State(state): State<LifecycleHttpState>,
    Extension(account): Extension<AuthenticatedAccount>,
    body: Result<Json<NewVolumeBody>, JsonRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let connection = state
        .volume_service()?
        .create(&account.owner_id, &body?.0.name)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(VolumeWithTokenResponse::from(connection)),
    ))
}

pub async fn list(
    State(state): State<LifecycleHttpState>,
    Extension(account): Extension<AuthenticatedAccount>,
) -> Result<Json<Vec<VolumeResponse>>, ApiError> {
    let volumes = state
        .volume_service()?
        .list(&account.owner_id)
        .await?
        .iter()
        .map(VolumeResponse::from)
        .collect();
    Ok(Json(volumes))
}

pub async fn get(
    State(state): State<LifecycleHttpState>,
    Extension(account): Extension<AuthenticatedAccount>,
    Path(volume_id): Path<String>,
) -> Result<Json<VolumeWithTokenResponse>, ApiError> {
    let volume_id = parse_volume_id(volume_id)?;
    let connection = state
        .volume_service()?
        .get(&account.owner_id, &volume_id)
        .await?;
    Ok(Json(connection.into()))
}

pub async fn delete(
    State(state): State<LifecycleHttpState>,
    Extension(account): Extension<AuthenticatedAccount>,
    Path(volume_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let volume_id = parse_volume_id(volume_id)?;
    state
        .volume_service()?
        .delete(&account.owner_id, &volume_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

fn parse_volume_id(value: String) -> Result<VolumeId, ApiError> {
    VolumeId::new(value).map_err(|_| ApiError::volume_not_found())
}
