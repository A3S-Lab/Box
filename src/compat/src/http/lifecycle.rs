use std::collections::BTreeMap;
use std::num::NonZeroU32;

use axum::extract::{rejection::JsonRejection, Extension, Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use futures::{stream, StreamExt};

use crate::control::{ConnectionDisposition, ControlServiceError, PublicSandboxState, SandboxId};

use super::dto::{
    parse_list_filter, parse_metric_range, parse_metric_sandbox_ids, ListedSandboxResponse,
    NewSandboxBody, RefreshBody, SandboxDetailResponse, SandboxMetricResponse, SandboxResponse,
    SandboxesWithMetricsResponse, TimeoutBody,
};
use super::error::ApiError;
use super::router::LifecycleHttpState;
use super::AuthenticatedAccount;

const MAX_CONCURRENT_METRIC_REQUESTS: usize = 16;

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

pub async fn list_running(
    State(state): State<LifecycleHttpState>,
    Extension(account): Extension<AuthenticatedAccount>,
    RawQuery(raw_query): RawQuery,
) -> Result<Json<Vec<ListedSandboxResponse>>, ApiError> {
    let mut filter = parse_list_filter(
        account.owner_id,
        raw_query.as_deref(),
        state.cursors().as_ref(),
    )?;
    filter.states = [PublicSandboxState::Running].into_iter().collect();
    filter.limit = NonZeroU32::MAX;
    filter.after = None;
    let page = state.service().list(&filter).await?;
    let records = page
        .records
        .iter()
        .rev()
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

pub async fn refresh(
    State(state): State<LifecycleHttpState>,
    Extension(account): Extension<AuthenticatedAccount>,
    Path(sandbox_id): Path<String>,
    body: Result<Json<RefreshBody>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let sandbox_id = parse_sandbox_id(sandbox_id)?;
    let body = match body {
        Ok(Json(body)) => body,
        Err(JsonRejection::MissingJsonContentType(_)) => RefreshBody::default(),
        Err(error) => return Err(error.into()),
    };
    state
        .service()
        .refresh_timeout(&account.owner_id, &sandbox_id, body.duration()?)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_metrics(
    State(state): State<LifecycleHttpState>,
    Extension(account): Extension<AuthenticatedAccount>,
    Path(sandbox_id): Path<String>,
    RawQuery(raw_query): RawQuery,
) -> Result<Json<Vec<SandboxMetricResponse>>, ApiError> {
    let sandbox_id = parse_sandbox_id(sandbox_id)?;
    let range = parse_metric_range(raw_query.as_deref())?;
    let metrics = state
        .service()
        .current_metric(&account.owner_id, &sandbox_id)
        .await?
        .filter(|metric| range.contains(metric.timestamp.timestamp()))
        .map(SandboxMetricResponse::from)
        .into_iter()
        .collect();
    Ok(Json(metrics))
}

pub async fn get_metrics_batch(
    State(state): State<LifecycleHttpState>,
    Extension(account): Extension<AuthenticatedAccount>,
    RawQuery(raw_query): RawQuery,
) -> Result<Json<SandboxesWithMetricsResponse>, ApiError> {
    let sandbox_ids = parse_metric_sandbox_ids(raw_query.as_deref())?;
    let service = state.service();
    let owner_id = account.owner_id;
    let metrics = stream::iter(sandbox_ids.into_iter().map(|sandbox_id| {
        let owner_id = owner_id.clone();
        async move {
            let metric = service.current_metric(&owner_id, &sandbox_id).await;
            (sandbox_id, metric)
        }
    }))
    .buffer_unordered(MAX_CONCURRENT_METRIC_REQUESTS)
    .collect::<Vec<_>>()
    .await;
    let mut sandboxes = BTreeMap::new();
    for (sandbox_id, metric) in metrics {
        match metric {
            Ok(Some(metric)) => {
                sandboxes.insert(sandbox_id.to_string(), SandboxMetricResponse::from(metric));
            }
            Ok(None) | Err(ControlServiceError::NotFound(_)) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(Json(SandboxesWithMetricsResponse { sandboxes }))
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
