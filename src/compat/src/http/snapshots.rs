use std::num::NonZeroU32;

use axum::extract::{rejection::JsonRejection, Extension, Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::control::SandboxId;
use crate::snapshot::{SnapshotCursor, SnapshotRecord};

use super::error::ApiError;
use super::router::LifecycleHttpState;
use super::AuthenticatedAccount;

const DEFAULT_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 1_000;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSnapshotBody {
    name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SnapshotResponse {
    #[serde(rename = "snapshotID")]
    snapshot_id: String,
    names: Vec<String>,
}

impl From<&SnapshotRecord> for SnapshotResponse {
    fn from(record: &SnapshotRecord) -> Self {
        Self {
            snapshot_id: record.reference().to_string(),
            names: record.names(),
        }
    }
}

pub async fn create(
    State(state): State<LifecycleHttpState>,
    Extension(account): Extension<AuthenticatedAccount>,
    Path(sandbox_id): Path<String>,
    body: Result<Json<CreateSnapshotBody>, JsonRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let sandbox_id = parse_sandbox_id(sandbox_id)?;
    let body = body?.0;
    let snapshot = state
        .service()
        .create_snapshot(&account.owner_id, &sandbox_id, body.name.as_deref())
        .await?;
    Ok((StatusCode::CREATED, Json(SnapshotResponse::from(&snapshot))))
}

pub async fn list(
    State(state): State<LifecycleHttpState>,
    Extension(account): Extension<AuthenticatedAccount>,
    RawQuery(raw_query): RawQuery,
) -> Result<impl IntoResponse, ApiError> {
    let query = parse_list_query(raw_query.as_deref())?;
    let page = state
        .snapshot_service()?
        .list(
            &account.owner_id,
            query.sandbox_id.as_ref(),
            query.limit,
            query.after.as_ref(),
        )
        .await?;
    let mut headers = HeaderMap::new();
    if let Some(cursor) = page.next.as_ref() {
        let encoded = encode_cursor(cursor)?;
        headers.insert(
            "x-next-token",
            HeaderValue::from_str(&encoded).map_err(|_| ApiError::internal())?,
        );
    }
    let snapshots = page
        .records
        .iter()
        .map(SnapshotResponse::from)
        .collect::<Vec<_>>();
    Ok((headers, Json(snapshots)))
}

pub async fn delete(
    State(state): State<LifecycleHttpState>,
    Extension(account): Extension<AuthenticatedAccount>,
    Path(template_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let reference = template_id.trim_start_matches('/');
    if reference.is_empty() {
        return Err(ApiError::snapshot_not_found());
    }
    if state
        .snapshot_service()?
        .delete(&account.owner_id, reference)
        .await?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::snapshot_not_found())
    }
}

struct SnapshotListQuery {
    sandbox_id: Option<SandboxId>,
    limit: NonZeroU32,
    after: Option<SnapshotCursor>,
}

fn parse_list_query(raw_query: Option<&str>) -> Result<SnapshotListQuery, ApiError> {
    let mut sandbox_id = None;
    let mut limit = None;
    let mut after = None;
    for (key, value) in url::form_urlencoded::parse(raw_query.unwrap_or_default().as_bytes()) {
        match key.as_ref() {
            "sandboxID" if sandbox_id.is_none() => {
                sandbox_id = Some(parse_sandbox_id(value.into_owned())?);
            }
            "limit" if limit.is_none() => {
                let parsed = value
                    .parse::<u32>()
                    .ok()
                    .filter(|value| *value <= MAX_LIMIT)
                    .and_then(NonZeroU32::new)
                    .ok_or_else(|| {
                        ApiError::bad_request(format!(
                            "Snapshot limit must be between 1 and {MAX_LIMIT}"
                        ))
                    })?;
                limit = Some(parsed);
            }
            "nextToken" if after.is_none() => after = Some(decode_cursor(&value)?),
            _ => return Err(ApiError::bad_request("Invalid snapshot list query")),
        }
    }
    Ok(SnapshotListQuery {
        sandbox_id,
        limit: limit.unwrap_or(NonZeroU32::new(DEFAULT_LIMIT).ok_or_else(ApiError::internal)?),
        after,
    })
}

fn parse_sandbox_id(value: String) -> Result<SandboxId, ApiError> {
    SandboxId::new(value).map_err(|_| ApiError::not_found())
}

fn encode_cursor(cursor: &SnapshotCursor) -> Result<String, ApiError> {
    let bytes = serde_json::to_vec(cursor).map_err(|_| ApiError::internal())?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn decode_cursor(value: &str) -> Result<SnapshotCursor, ApiError> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| ApiError::bad_request("Invalid snapshot pagination cursor"))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| ApiError::bad_request("Invalid snapshot pagination cursor"))
}
