use std::collections::BTreeMap;

use axum::extract::{Extension, Path, RawQuery, State};
use axum::Json;
use chrono::SecondsFormat;
use serde::Serialize;

use crate::control::{SandboxId, SandboxLog};

use super::error::ApiError;
use super::router::LifecycleHttpState;
use super::AuthenticatedAccount;

const DEFAULT_LOG_LIMIT: u32 = 1_000;
const V2_MAX_LOG_LIMIT: u32 = 1_000;

pub async fn legacy(
    State(state): State<LifecycleHttpState>,
    Extension(account): Extension<AuthenticatedAccount>,
    Path(sandbox_id): Path<String>,
    RawQuery(raw_query): RawQuery,
) -> Result<Json<LegacySandboxLogsResponse>, ApiError> {
    let sandbox_id = parse_sandbox_id(sandbox_id)?;
    let query = LogQuery::parse_legacy(raw_query.as_deref())?;
    let logs = state.service().logs(&account.owner_id, &sandbox_id).await?;
    let entries = select_logs(&logs, &query);
    let legacy_logs = entries
        .iter()
        .map(|entry| LegacySandboxLogResponse {
            timestamp: entry.timestamp.clone(),
            line: serde_json::json!({
                "level": entry.level,
                "logger": "a3s-box-runtime",
                "message": entry.message,
                "stream": entry.fields.get("stream"),
            })
            .to_string(),
        })
        .collect();
    Ok(Json(LegacySandboxLogsResponse {
        logs: legacy_logs,
        log_entries: entries,
    }))
}

pub async fn v2(
    State(state): State<LifecycleHttpState>,
    Extension(account): Extension<AuthenticatedAccount>,
    Path(sandbox_id): Path<String>,
    RawQuery(raw_query): RawQuery,
) -> Result<Json<SandboxLogsV2Response>, ApiError> {
    let sandbox_id = parse_sandbox_id(sandbox_id)?;
    let query = LogQuery::parse_v2(raw_query.as_deref())?;
    let logs = state.service().logs(&account.owner_id, &sandbox_id).await?;
    Ok(Json(SandboxLogsV2Response {
        logs: select_logs(&logs, &query),
    }))
}

fn parse_sandbox_id(value: String) -> Result<SandboxId, ApiError> {
    SandboxId::new(value).map_err(|_| ApiError::bad_request("Invalid sandbox ID"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogsDirection {
    Forward,
    Backward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug)]
struct LogQuery {
    cursor_millis: Option<i64>,
    limit: u32,
    direction: LogsDirection,
    minimum_level: LogLevel,
    search: Option<String>,
}

impl LogQuery {
    fn parse_legacy(raw_query: Option<&str>) -> Result<Self, ApiError> {
        let mut cursor_millis = None;
        let mut limit = None;
        for (name, value) in url::form_urlencoded::parse(raw_query.unwrap_or_default().as_bytes()) {
            match name.as_ref() {
                "start" if cursor_millis.is_none() => {
                    cursor_millis = Some(parse_non_negative_i64(&value, "start")?);
                }
                "limit" if limit.is_none() => {
                    limit = Some(parse_i32_limit(&value, "limit")?);
                }
                "start" | "limit" => {
                    return Err(ApiError::bad_request(
                        "sandbox log query parameters must not be repeated",
                    ));
                }
                _ => return Err(ApiError::bad_request("unknown sandbox logs parameter")),
            }
        }
        Ok(Self {
            cursor_millis,
            limit: limit.unwrap_or(DEFAULT_LOG_LIMIT),
            direction: LogsDirection::Forward,
            minimum_level: LogLevel::Debug,
            search: None,
        })
    }

    fn parse_v2(raw_query: Option<&str>) -> Result<Self, ApiError> {
        let mut cursor_millis = None;
        let mut limit = None;
        let mut direction = None;
        let mut minimum_level = None;
        let mut search = None;
        for (name, value) in url::form_urlencoded::parse(raw_query.unwrap_or_default().as_bytes()) {
            match name.as_ref() {
                "cursor" if cursor_millis.is_none() => {
                    cursor_millis = Some(parse_non_negative_i64(&value, "cursor")?);
                }
                "limit" if limit.is_none() => {
                    let parsed = parse_i32_limit(&value, "limit")?;
                    if parsed > V2_MAX_LOG_LIMIT {
                        return Err(ApiError::bad_request(
                            "log limit must be between 0 and 1000",
                        ));
                    }
                    limit = Some(parsed);
                }
                "direction" if direction.is_none() => {
                    direction = Some(match value.as_ref() {
                        "forward" => LogsDirection::Forward,
                        "backward" => LogsDirection::Backward,
                        _ => return Err(ApiError::bad_request("invalid sandbox log direction")),
                    });
                }
                "level" if minimum_level.is_none() => {
                    minimum_level = Some(match value.as_ref() {
                        "debug" => LogLevel::Debug,
                        "info" => LogLevel::Info,
                        "warn" => LogLevel::Warn,
                        "error" => LogLevel::Error,
                        _ => return Err(ApiError::bad_request("invalid sandbox log level")),
                    });
                }
                "search" if search.is_none() => {
                    if value.chars().count() > 256 {
                        return Err(ApiError::bad_request(
                            "sandbox log search must not exceed 256 characters",
                        ));
                    }
                    search = Some(value.into_owned());
                }
                "cursor" | "limit" | "direction" | "level" | "search" => {
                    return Err(ApiError::bad_request(
                        "sandbox log query parameters must not be repeated",
                    ));
                }
                _ => return Err(ApiError::bad_request("unknown sandbox logs parameter")),
            }
        }
        Ok(Self {
            cursor_millis,
            limit: limit.unwrap_or(DEFAULT_LOG_LIMIT),
            direction: direction.unwrap_or(LogsDirection::Forward),
            minimum_level: minimum_level.unwrap_or(LogLevel::Debug),
            search,
        })
    }
}

fn parse_non_negative_i64(value: &str, name: &str) -> Result<i64, ApiError> {
    value
        .parse::<i64>()
        .ok()
        .filter(|value| *value >= 0)
        .ok_or_else(|| ApiError::bad_request(format!("{name} must be a non-negative integer")))
}

fn parse_i32_limit(value: &str, name: &str) -> Result<u32, ApiError> {
    value
        .parse::<i32>()
        .ok()
        .filter(|value| *value >= 0)
        .map(|value| value as u32)
        .ok_or_else(|| ApiError::bad_request(format!("{name} must be a non-negative integer")))
}

fn select_logs(logs: &[SandboxLog], query: &LogQuery) -> Vec<SandboxLogEntryResponse> {
    let mut ordered = logs.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|log| log.timestamp);
    let include = |log: &&SandboxLog| {
        let timestamp = log.timestamp.timestamp_millis();
        let in_range = match (query.direction, query.cursor_millis) {
            (LogsDirection::Forward, Some(cursor)) => timestamp >= cursor,
            (LogsDirection::Backward, Some(cursor)) => timestamp <= cursor,
            (_, None) => true,
        };
        let level = level_for_stream(&log.stream);
        in_range
            && level >= query.minimum_level
            && query
                .search
                .as_ref()
                .is_none_or(|search| log.message.contains(search))
    };
    let convert = |log: &SandboxLog| SandboxLogEntryResponse {
        timestamp: log.timestamp.to_rfc3339_opts(SecondsFormat::AutoSi, true),
        level: level_for_stream(&log.stream),
        message: log.message.clone(),
        fields: BTreeMap::from([
            ("logger".to_string(), "a3s-box-runtime".to_string()),
            ("stream".to_string(), log.stream.clone()),
        ]),
    };

    match query.direction {
        LogsDirection::Forward => ordered
            .into_iter()
            .filter(include)
            .take(query.limit as usize)
            .map(convert)
            .collect(),
        LogsDirection::Backward => ordered
            .into_iter()
            .rev()
            .filter(include)
            .take(query.limit as usize)
            .map(convert)
            .collect(),
    }
}

fn level_for_stream(stream: &str) -> LogLevel {
    if stream == "stderr" {
        LogLevel::Error
    } else {
        LogLevel::Info
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacySandboxLogsResponse {
    logs: Vec<LegacySandboxLogResponse>,
    log_entries: Vec<SandboxLogEntryResponse>,
}

#[derive(Debug, Serialize)]
struct LegacySandboxLogResponse {
    timestamp: String,
    line: String,
}

#[derive(Debug, Clone, Serialize)]
struct SandboxLogEntryResponse {
    timestamp: String,
    level: LogLevel,
    message: String,
    fields: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct SandboxLogsV2Response {
    logs: Vec<SandboxLogEntryResponse>,
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;

    fn log(second: u32, stream: &str, message: &str) -> SandboxLog {
        SandboxLog {
            timestamp: Utc
                .with_ymd_and_hms(2026, 7, 14, 12, 0, second)
                .single()
                .unwrap(),
            stream: stream.to_string(),
            message: message.to_string(),
        }
    }

    #[test]
    fn filters_v2_logs_by_cursor_direction_level_and_search() {
        let logs = vec![
            log(2, "stdout", "ready"),
            log(0, "stdout", "starting"),
            log(1, "stderr", "failed once"),
        ];
        let raw_query = format!(
            "cursor={}&limit=1&direction=backward&level=error&search=failed",
            logs[0].timestamp.timestamp_millis()
        );
        let query = LogQuery::parse_v2(Some(&raw_query)).unwrap();
        let selected = select_logs(&logs, &query);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].message, "failed once");
        assert_eq!(selected[0].level, LogLevel::Error);

        let forward = select_logs(&logs, &LogQuery::parse_v2(None).unwrap());
        assert_eq!(
            forward
                .iter()
                .map(|entry| entry.message.as_str())
                .collect::<Vec<_>>(),
            vec!["starting", "failed once", "ready"]
        );
    }

    #[test]
    fn validates_log_query_bounds_and_duplicates() {
        assert!(LogQuery::parse_legacy(Some("limit=-1")).is_err());
        assert!(LogQuery::parse_v2(Some("limit=1001")).is_err());
        assert!(LogQuery::parse_v2(Some("direction=sideways")).is_err());
        assert!(LogQuery::parse_v2(Some("search=a&search=b")).is_err());
    }
}
