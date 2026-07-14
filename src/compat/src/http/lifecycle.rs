use axum::extract::{rejection::JsonRejection, Extension, Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use crate::control::{ConnectionDisposition, SandboxId};

use super::dto::{
    parse_list_filter, ListedSandboxResponse, NewSandboxBody, SandboxDetailResponse,
    SandboxResponse, TimeoutBody,
};
use super::error::ApiError;
use super::router::LifecycleHttpState;
use super::AuthenticatedAccount;

pub async fn create(
    State(state): State<LifecycleHttpState>,
    Extension(account): Extension<AuthenticatedAccount>,
    body: Result<Json<NewSandboxBody>, JsonRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let request = body?.0.into_control(account.owner_id)?;
    let connection = state.service().create(request).await?;
    let (response, _) = SandboxResponse::from_connection(
        connection,
        account.client_id,
        state.domain().map(str::to_string),
    );
    Ok((StatusCode::CREATED, Json(response)))
}

pub async fn connect(
    State(state): State<LifecycleHttpState>,
    Extension(account): Extension<AuthenticatedAccount>,
    Path(sandbox_id): Path<String>,
    body: Result<Json<TimeoutBody>, JsonRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let sandbox_id = parse_sandbox_id(sandbox_id)?;
    let connection = state
        .service()
        .connect(&account.owner_id, &sandbox_id, body?.0.timeout)
        .await?;
    let (response, disposition) = SandboxResponse::from_connection(
        connection,
        account.client_id,
        state.domain().map(str::to_string),
    );
    let status = match disposition {
        ConnectionDisposition::Resumed => StatusCode::CREATED,
        ConnectionDisposition::AlreadyRunning | ConnectionDisposition::Created => StatusCode::OK,
    };
    Ok((status, Json(response)))
}

pub async fn list(
    State(state): State<LifecycleHttpState>,
    Extension(account): Extension<AuthenticatedAccount>,
    RawQuery(raw_query): RawQuery,
) -> Result<Json<Vec<ListedSandboxResponse>>, ApiError> {
    let filter = parse_list_filter(
        account.owner_id,
        raw_query.as_deref(),
        state.cursors().as_ref(),
    )?;
    let page = state.service().list(&filter).await?;
    let records = page
        .records
        .iter()
        .filter_map(|record| ListedSandboxResponse::from_record(record, account.client_id.clone()))
        .collect();
    Ok(Json(records))
}

pub async fn get(
    State(state): State<LifecycleHttpState>,
    Extension(account): Extension<AuthenticatedAccount>,
    Path(sandbox_id): Path<String>,
) -> Result<Json<SandboxDetailResponse>, ApiError> {
    let sandbox_id = parse_sandbox_id(sandbox_id)?;
    let record = state.service().get(&account.owner_id, &sandbox_id).await?;
    let response = SandboxDetailResponse::from_record(
        &record,
        account.client_id,
        state.domain().map(str::to_string),
    )
    .ok_or_else(ApiError::not_found)?;
    Ok(Json(response))
}

pub async fn set_timeout(
    State(state): State<LifecycleHttpState>,
    Extension(account): Extension<AuthenticatedAccount>,
    Path(sandbox_id): Path<String>,
    body: Result<Json<TimeoutBody>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let sandbox_id = parse_sandbox_id(sandbox_id)?;
    state
        .service()
        .set_timeout(&account.owner_id, &sandbox_id, body?.0.timeout)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn kill(
    State(state): State<LifecycleHttpState>,
    Extension(account): Extension<AuthenticatedAccount>,
    Path(sandbox_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let sandbox_id = parse_sandbox_id(sandbox_id)?;
    if state.service().kill(&account.owner_id, &sandbox_id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found())
    }
}

fn parse_sandbox_id(value: String) -> Result<SandboxId, ApiError> {
    SandboxId::new(value).map_err(|_| ApiError::bad_request("Invalid sandbox ID"))
}
