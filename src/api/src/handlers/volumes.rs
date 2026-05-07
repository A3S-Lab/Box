//! Volume API handlers.

use axum::{Json, extract::Path, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{ApiResult, ApiError};

/// GET /volumes - List volumes.
pub async fn list() -> ApiResult<Json<serde_json::Value>> {
    let store = a3s_box_runtime::VolumeStore::default_path()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let volumes = store.list()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let result: Vec<_> = volumes.iter().map(|v| {
        json!({
            "Name": v.name,
            "Driver": v.driver,
            "Mountpoint": v.mount_point,
            "CreatedAt": v.created_at.to_rfc3339(),
            "Status": {},
            "Labels": v.labels,
            "Scope": "local",
            "Options": {}
        })
    }).collect();

    Ok(Json(json!({
        "Volumes": result,
        "Warnings": null
    })))
}

/// Request body for volume create.
#[derive(Debug, Deserialize)]
pub struct VolumeCreateRequest {
    #[serde(rename = "Name")]
    name: String,

    #[serde(rename = "Driver")]
    driver: Option<String>,

    #[serde(rename = "DriverOpts")]
    driver_opts: Option<std::collections::HashMap<String, String>>,

    #[serde(rename = "Labels")]
    labels: Option<std::collections::HashMap<String, String>>,
}

/// POST /volumes/create - Create a volume.
pub async fn create(Json(req): Json<VolumeCreateRequest>) -> ApiResult<Json<serde_json::Value>> {
    let store = a3s_box_runtime::VolumeStore::default_path()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let mut config = a3s_box_core::volume::VolumeConfig::new(&req.name, "");
    config.driver = req.driver.unwrap_or_else(|| "local".to_string());
    if let Some(labels) = req.labels {
        config.labels = labels;
    }

    let created = store.create(config)
        .map_err(|e| ApiError::Conflict(e.to_string()))?;

    Ok(Json(json!({
        "Name": created.name,
        "Driver": created.driver,
        "Mountpoint": created.mount_point,
        "CreatedAt": created.created_at.to_rfc3339(),
        "Status": {},
        "Labels": created.labels,
        "Scope": "local",
        "Options": {}
    })))
}

/// GET /volumes/:name - Inspect a volume.
pub async fn inspect(Path(name): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    let store = a3s_box_runtime::VolumeStore::default_path()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let volume = store.get(&name)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::NotFound(format!("Volume {} not found", name)))?;

    Ok(Json(json!({
        "Name": volume.name,
        "Driver": volume.driver,
        "Mountpoint": volume.mount_point,
        "CreatedAt": volume.created_at.to_rfc3339(),
        "Status": {},
        "Labels": volume.labels,
        "Scope": "local",
        "Options": {}
    })))
}

/// DELETE /volumes/:name - Remove a volume.
pub async fn remove(
    Path(name): Path<String>,
    axum::extract::Query(query): axum::extract::Query<RemoveQuery>,
) -> ApiResult<StatusCode> {
    let store = a3s_box_runtime::VolumeStore::default_path()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    store.remove(&name, query.force)
        .map_err(|e| ApiError::Conflict(e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}
