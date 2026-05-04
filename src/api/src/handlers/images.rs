//! Image API handlers.

use axum::{Json, extract::{Path, Query}};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;

use crate::error::{ApiResult, ApiError};

/// Query parameters for image list.
#[derive(Debug, Deserialize, Default)]
pub struct ListQuery {
    /// Show all images (default hides intermediate images)
    #[serde(default)]
    all: bool,

    /// Filter images by reference
    filters: Option<String>,

    /// Show digest information
    #[serde(default)]
    digests: bool,
}

/// GET /images/json - List images.
pub async fn list(Query(_query): Query<ListQuery>) -> ApiResult<Json<serde_json::Value>> {
    // Open image store
    let store = super::super::open_image_store()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // List all images
    let images = store.list()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let result: Vec<_> = images.iter().map(|img| {
        json!({
            "Id": img.digest,
            "RepoTags": img.tags,
            "RepoDigests": [img.digest.clone()],
            "Created": img.created_at.timestamp(),
            "Size": img.size,
            "VirtualSize": img.size,
            "SharedSize": 0,
            "Labels": {},
            "Containers": 0
        })
    }).collect();

    Ok(Json(json!(result)))
}

fn open_image_store() -> Result<std::sync::Arc<a3s_box_runtime::oci::ImageStore>, Box<dyn std::error::Error>> {
    let store_path = a3s_box_core::dirs_home().join("images");
    Ok(std::sync::Arc::new(a3s_box_runtime::oci::ImageStore::new(store_path)?))
}

/// Query parameters for image pull.
#[derive(Debug, Deserialize, Default)]
pub struct PullQuery {
    /// Image reference to pull
    #[serde(rename = "fromImage")]
    from_image: String,

    /// Tag to pull
    tag: Option<String>,
}

/// POST /images/create - Pull an image.
pub async fn pull(Query(query): Query<PullQuery>) -> ApiResult<Json<serde_json::Value>> {
    let reference = if let Some(tag) = query.tag {
        format!("{}:{}", query.from_image, tag)
    } else {
        query.from_image
    };

    // Use a3s-box pull command
    let pull_args = a3s_box_cli::commands::pull::PullArgs {
        reference: reference.clone(),
        platform: None,
        quiet: false,
    };

    a3s_box_cli::commands::pull::execute(pull_args).await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(Json(json!({
        "status": format!("Pulled {}", reference)
    })))
}

/// GET /images/:name/json - Inspect an image.
pub async fn inspect(Path(name): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    let store = open_image_store()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Find image by name or digest
    let images = store.list()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let image = images.iter()
        .find(|img| {
            img.tags.iter().any(|tag| tag.contains(&name)) ||
            img.digest.starts_with(&name)
        })
        .ok_or_else(|| ApiError::NotFound(format!("Image {} not found", name)))?;

    Ok(Json(json!({
        "Id": image.digest,
        "RepoTags": image.tags,
        "RepoDigests": [image.digest.clone()],
        "Created": image.created_at.to_rfc3339(),
        "Size": image.size,
        "VirtualSize": image.size,
        "Architecture": "amd64",
        "Os": "linux",
        "Config": {},
        "RootFS": {
            "Type": "layers",
            "Layers": []
        }
    })))
}

/// GET /images/:name/history - Get image history.
pub async fn history(Path(name): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    let store = open_image_store()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Find image by name or digest
    let images = store.list()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let image = images.iter()
        .find(|img| {
            img.tags.iter().any(|tag| tag.contains(&name)) ||
            img.digest.starts_with(&name)
        })
        .ok_or_else(|| ApiError::NotFound(format!("Image {} not found", name)))?;

    // Return basic history (single layer for now)
    Ok(Json(json!([
        {
            "Id": image.digest,
            "Created": image.created_at.timestamp(),
            "CreatedBy": "/bin/sh -c #(nop)",
            "Tags": image.tags,
            "Size": image.size,
            "Comment": ""
        }
    ])))
}

/// Query parameters for push.
#[derive(Debug, Deserialize, Default)]
pub struct PushQuery {
    /// Tag to push
    tag: Option<String>,
}

/// POST /images/:name/push - Push an image.
pub async fn push(
    Path(name): Path<String>,
    Query(_query): Query<PushQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    // TODO: Implement actual image push to registry
    // For now, return a stub response
    Ok(Json(json!({
        "status": format!("Pushing {}", name),
        "progressDetail": {},
        "id": name
    })))
}

/// Query parameters for tag.
#[derive(Debug, Deserialize, Default)]
pub struct TagQuery {
    /// Repository name
    repo: String,

    /// Tag name
    tag: Option<String>,
}

/// POST /images/:name/tag - Tag an image.
pub async fn tag(
    Path(name): Path<String>,
    Query(query): Query<TagQuery>,
) -> ApiResult<StatusCode> {
    let store = open_image_store()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Find source image
    let images = store.list()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let _image = images.iter()
        .find(|img| {
            img.tags.iter().any(|tag| tag.contains(&name)) ||
            img.digest.starts_with(&name)
        })
        .ok_or_else(|| ApiError::NotFound(format!("Image {} not found", name)))?;

    // Build new tag
    let new_tag = if let Some(tag) = query.tag {
        format!("{}:{}", query.repo, tag)
    } else {
        format!("{}:latest", query.repo)
    };

    // TODO: Implement actual image tagging
    // For now, return success
    tracing::info!("Would tag {} as {}", name, new_tag);
    Ok(StatusCode::CREATED)
}

/// Query parameters for image remove.
#[derive(Debug, Deserialize, Default)]
pub struct RemoveQuery {
    /// Force removal
    #[serde(default)]
    force: bool,

    /// Do not delete untagged parents
    #[serde(default)]
    noprune: bool,
}

/// DELETE /images/:name - Remove an image.
pub async fn remove(
    Path(name): Path<String>,
    Query(_query): Query<RemoveQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    // Use a3s-box rmi command
    let rmi_args = a3s_box_cli::commands::rmi::RmiArgs {
        references: vec![name.clone()],
    };

    a3s_box_cli::commands::rmi::execute(rmi_args).await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(Json(json!({
        "Deleted": [
            {"Untagged": name}
        ]
    })))
}

/// Query parameters for build.
#[derive(Debug, Deserialize, Default)]
pub struct BuildQuery {
    /// Name and optional tag for the image
    #[serde(rename = "t")]
    tag: Option<String>,

    /// Path to Dockerfile
    #[serde(rename = "dockerfile")]
    dockerfile: Option<String>,

    /// Suppress verbose build output
    #[serde(default)]
    q: bool,

    /// Build arguments (JSON object)
    #[serde(rename = "buildargs")]
    build_args: Option<String>,

    /// Target platform
    platform: Option<String>,
}

/// POST /build - Build an image from a Dockerfile.
pub async fn build(Query(query): Query<BuildQuery>) -> ApiResult<Json<serde_json::Value>> {
    // Parse build args from JSON string
    let build_arg_vec = if let Some(args_json) = query.build_args {
        let args_map: HashMap<String, String> = serde_json::from_str(&args_json)
            .map_err(|e| ApiError::BadRequest(format!("Invalid build args: {}", e)))?;
        args_map.into_iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect()
    } else {
        vec![]
    };

    // Use a3s-box build command
    let build_args = a3s_box_cli::commands::build::BuildArgs {
        path: ".".to_string(), // TODO: Extract from request body (tarball)
        tag: query.tag,
        file: query.dockerfile,
        build_arg: build_arg_vec,
        quiet: query.q,
        platform: query.platform,
    };

    let result = a3s_box_cli::commands::build::execute(build_args).await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // TODO: Get actual image ID from build result
    Ok(Json(json!({
        "stream": "Successfully built image\n"
    })))
}
