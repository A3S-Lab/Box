use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::control::{
    ConnectionDisposition, CreateSandboxRequest, LifecyclePolicy, OnTimeoutAction,
    PublicSandboxState, SandboxConnection, SandboxId, SandboxListFilter, SandboxMetric,
    SandboxRecord,
};

use super::cursor::CursorDecoder;
use super::error::ApiError;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewSandboxBody {
    #[serde(rename = "templateID")]
    template_id: String,
    #[serde(default = "default_timeout")]
    timeout: u32,
    #[serde(rename = "autoPause", default)]
    auto_pause: bool,
    #[serde(rename = "autoPauseMemory", default = "default_true")]
    auto_pause_memory: bool,
    #[serde(rename = "autoResume", default)]
    auto_resume: AutoResumeBody,
    #[serde(default)]
    secure: bool,
    #[serde(default)]
    allow_internet_access: Option<bool>,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
    #[serde(rename = "envVars", default)]
    env_vars: BTreeMap<String, String>,
    #[serde(default)]
    network: Option<serde_json::Value>,
    #[serde(default)]
    mcp: Option<serde_json::Value>,
    #[serde(rename = "volumeMounts", default)]
    volume_mounts: Vec<serde_json::Value>,
}

impl NewSandboxBody {
    pub fn into_control(self, owner_id: String) -> Result<CreateSandboxRequest, ApiError> {
        if self.network.is_some() || self.mcp.is_some() || !self.volume_mounts.is_empty() {
            return Err(ApiError::bad_request(
                "network, MCP, and volume mount overrides are not available in this preview",
            ));
        }
        if self.auto_resume.enabled && self.auto_pause && !self.auto_pause_memory {
            return Err(ApiError::bad_request(
                "auto-resume requires memory-preserving auto-pause",
            ));
        }
        Ok(CreateSandboxRequest {
            owner_id,
            template_id: self.template_id,
            timeout_seconds: self.timeout,
            lifecycle: LifecyclePolicy {
                on_timeout: if self.auto_pause {
                    OnTimeoutAction::Pause
                } else {
                    OnTimeoutAction::Kill
                },
                auto_resume: self.auto_resume.enabled,
                keep_memory_on_pause: self.auto_pause_memory,
            },
            metadata: self.metadata,
            env_vars: self.env_vars,
            secure: self.secure,
            allow_internet_access: self.allow_internet_access,
        })
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AutoResumeBody {
    #[serde(default)]
    enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeoutBody {
    pub timeout: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PauseBody {
    #[serde(default = "default_true")]
    pub memory: bool,
}

impl Default for PauseBody {
    fn default() -> Self {
        Self { memory: true }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeBody {
    #[serde(default = "default_timeout")]
    pub timeout: u32,
    #[serde(rename = "autoPause", default)]
    pub auto_pause: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct RefreshBody {
    #[serde(default)]
    duration: Option<u32>,
}

impl RefreshBody {
    pub fn duration(self) -> Result<u32, ApiError> {
        let duration = self.duration.unwrap_or_else(default_timeout);
        if duration > 3_600 {
            return Err(ApiError::bad_request(
                "refresh duration must be between 0 and 3600 seconds",
            ));
        }
        Ok(duration.max(default_timeout()))
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct MetricRange {
    start: Option<i64>,
    end: Option<i64>,
}

impl MetricRange {
    pub fn contains(self, timestamp: i64) -> bool {
        self.start.is_none_or(|start| timestamp >= start)
            && self.end.is_none_or(|end| timestamp <= end)
    }
}

pub fn parse_metric_range(raw_query: Option<&str>) -> Result<MetricRange, ApiError> {
    let mut range = MetricRange::default();
    for (name, value) in url::form_urlencoded::parse(raw_query.unwrap_or_default().as_bytes()) {
        let parsed = value
            .parse::<i64>()
            .ok()
            .filter(|value| *value >= 0)
            .ok_or_else(|| ApiError::bad_request("metric timestamps must be non-negative"))?;
        match name.as_ref() {
            "start" if range.start.replace(parsed).is_none() => {}
            "end" if range.end.replace(parsed).is_none() => {}
            "start" | "end" => {
                return Err(ApiError::bad_request(
                    "metric timestamp parameters must not be repeated",
                ))
            }
            _ => return Err(ApiError::bad_request("unknown sandbox metrics parameter")),
        }
    }
    if matches!((range.start, range.end), (Some(start), Some(end)) if start > end) {
        return Err(ApiError::bad_request(
            "metric start timestamp must not exceed end timestamp",
        ));
    }
    Ok(range)
}

pub fn parse_metric_sandbox_ids(raw_query: Option<&str>) -> Result<Vec<SandboxId>, ApiError> {
    let mut values = None;
    for (name, value) in url::form_urlencoded::parse(raw_query.unwrap_or_default().as_bytes()) {
        if name != "sandbox_ids" || values.replace(value.into_owned()).is_some() {
            return Err(ApiError::bad_request(
                "sandbox_ids must be provided exactly once",
            ));
        }
    }
    let values = values.ok_or_else(|| ApiError::bad_request("sandbox_ids is required"))?;
    let mut unique = BTreeSet::new();
    let mut sandbox_ids = Vec::new();
    for value in values.split(',') {
        let sandbox_id = SandboxId::new(value.to_string())
            .map_err(|_| ApiError::bad_request("sandbox_ids contains an invalid Sandbox ID"))?;
        if !unique.insert(sandbox_id.to_string()) {
            return Err(ApiError::bad_request("sandbox_ids must be unique"));
        }
        sandbox_ids.push(sandbox_id);
    }
    if sandbox_ids.is_empty() || sandbox_ids.len() > 100 {
        return Err(ApiError::bad_request(
            "sandbox_ids must contain between 1 and 100 IDs",
        ));
    }
    Ok(sandbox_ids)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxMetricResponse {
    timestamp: String,
    timestamp_unix: i64,
    cpu_count: u32,
    cpu_used_pct: f32,
    mem_used: u64,
    mem_total: u64,
    mem_cache: u64,
    disk_used: u64,
    disk_total: u64,
}

impl From<SandboxMetric> for SandboxMetricResponse {
    fn from(metric: SandboxMetric) -> Self {
        Self {
            timestamp: format_time(metric.timestamp),
            timestamp_unix: metric.timestamp.timestamp(),
            cpu_count: metric.cpu_count,
            cpu_used_pct: metric.cpu_used_pct,
            mem_used: metric.mem_used,
            mem_total: metric.mem_total,
            mem_cache: metric.mem_cache,
            disk_used: metric.disk_used,
            disk_total: metric.disk_total,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct SandboxesWithMetricsResponse {
    pub sandboxes: BTreeMap<String, SandboxMetricResponse>,
}

pub fn parse_list_filter(
    owner_id: String,
    raw_query: Option<&str>,
    cursors: &dyn CursorDecoder,
) -> Result<SandboxListFilter, ApiError> {
    let mut metadata = BTreeMap::new();
    let mut states = BTreeSet::new();
    let mut limit = NonZeroU32::new(100).unwrap_or(NonZeroU32::MIN);
    let mut after = None;

    for (name, value) in url::form_urlencoded::parse(raw_query.unwrap_or_default().as_bytes()) {
        match name.as_ref() {
            "metadata" => metadata.extend(parse_metadata(&value)?),
            "state" => {
                for state in value.split(',') {
                    states.insert(match state {
                        "running" => PublicSandboxState::Running,
                        "paused" => PublicSandboxState::Paused,
                        _ => return Err(ApiError::bad_request("invalid sandbox state filter")),
                    });
                }
            }
            "limit" => {
                let parsed = value
                    .parse::<u32>()
                    .ok()
                    .filter(|value| (1..=100).contains(value))
                    .and_then(NonZeroU32::new)
                    .ok_or_else(|| ApiError::bad_request("limit must be between 1 and 100"))?;
                limit = parsed;
            }
            "nextToken" => after = cursors.decode(&value)?.or(after),
            _ => {
                return Err(ApiError::bad_request(
                    "unknown sandbox list query parameter",
                ))
            }
        }
    }

    Ok(SandboxListFilter {
        owner_id,
        metadata,
        states,
        limit,
        after,
    })
}

fn parse_metadata(value: &str) -> Result<BTreeMap<String, String>, ApiError> {
    let mut metadata = BTreeMap::new();
    for (key, value) in url::form_urlencoded::parse(value.as_bytes()) {
        let decoded = url::form_urlencoded::parse(format!("value={value}").as_bytes())
            .next()
            .map(|(_, value)| value.into_owned())
            .unwrap_or_default();
        if key.is_empty() {
            return Err(ApiError::bad_request("metadata keys cannot be empty"));
        }
        metadata.insert(key.into_owned(), decoded);
    }
    Ok(metadata)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxResponse {
    #[serde(rename = "templateID")]
    template_id: String,
    #[serde(rename = "sandboxID")]
    sandbox_id: String,
    #[serde(rename = "clientID")]
    client_id: String,
    #[serde(rename = "envdVersion")]
    envd_version: String,
    envd_access_token: String,
    traffic_access_token: Option<String>,
    domain: Option<String>,
}

impl SandboxResponse {
    pub fn from_connection(
        connection: SandboxConnection,
        client_id: String,
        domain: Option<String>,
    ) -> (Self, ConnectionDisposition) {
        let disposition = connection.disposition;
        let response = Self {
            template_id: connection.record.template_id().to_string(),
            sandbox_id: connection.record.sandbox_id().to_string(),
            client_id,
            envd_version: connection.record.envd_version().to_string(),
            envd_access_token: connection.envd_access_token.expose_secret().to_string(),
            traffic_access_token: Some(connection.traffic_access_token.expose_secret().to_string()),
            domain,
        };
        (response, disposition)
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListedSandboxResponse {
    #[serde(rename = "templateID")]
    template_id: String,
    #[serde(rename = "sandboxID")]
    sandbox_id: String,
    #[serde(rename = "clientID")]
    client_id: String,
    started_at: String,
    end_at: String,
    cpu_count: u32,
    #[serde(rename = "memoryMB")]
    memory_mb: u32,
    #[serde(rename = "diskSizeMB")]
    disk_size_mb: u32,
    metadata: BTreeMap<String, String>,
    state: PublicSandboxState,
    envd_version: String,
    volume_mounts: Vec<VolumeMountResponse>,
}

impl ListedSandboxResponse {
    pub fn from_record(record: &SandboxRecord, client_id: String) -> Option<Self> {
        Some(Self {
            template_id: record.template_id().to_string(),
            sandbox_id: record.sandbox_id().to_string(),
            client_id,
            started_at: format_time(record.started_at()?),
            end_at: format_time(record.expires_at()),
            cpu_count: record.resources().vcpus,
            memory_mb: record.resources().memory_mb,
            disk_size_mb: record.resources().disk_mb,
            metadata: record.metadata().clone(),
            state: record.public_state()?,
            envd_version: record.envd_version().to_string(),
            volume_mounts: Vec::new(),
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxDetailResponse {
    #[serde(flatten)]
    listed: ListedSandboxResponse,
    #[serde(skip_serializing_if = "Option::is_none")]
    envd_access_token: Option<String>,
    allow_internet_access: Option<bool>,
    domain: Option<String>,
    lifecycle: LifecycleResponse,
}

impl SandboxDetailResponse {
    pub fn from_record(
        record: &SandboxRecord,
        client_id: String,
        domain: Option<String>,
    ) -> Option<Self> {
        Some(Self {
            listed: ListedSandboxResponse::from_record(record, client_id)?,
            envd_access_token: None,
            allow_internet_access: record.allow_internet_access(),
            domain,
            lifecycle: LifecycleResponse {
                auto_resume: record.lifecycle().auto_resume,
                on_timeout: record.lifecycle().on_timeout,
            },
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LifecycleResponse {
    auto_resume: bool,
    on_timeout: OnTimeoutAction,
}

#[derive(Debug, Serialize)]
struct VolumeMountResponse {
    name: String,
    path: String,
}

fn default_timeout() -> u32 {
    15
}

fn default_true() -> bool {
    true
}

fn format_time(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{CursorError, CursorResult};

    struct FixtureCursor;

    impl CursorDecoder for FixtureCursor {
        fn decode(&self, value: &str) -> CursorResult<Option<crate::control::SandboxCursor>> {
            if value == "cursor-0" {
                Ok(None)
            } else {
                Err(CursorError::Invalid)
            }
        }
    }

    #[test]
    fn parses_the_official_clients_nested_metadata_query() {
        let filter = parse_list_filter(
            "fixture-client".to_string(),
            Some(
                "limit=2&metadata=team%3Dalpha%252520beta&nextToken=cursor-0&state=running%2Cpaused",
            ),
            &FixtureCursor,
        )
        .unwrap();

        assert_eq!(filter.metadata.get("team").unwrap(), "alpha beta");
        assert_eq!(filter.limit.get(), 2);
        assert_eq!(filter.states.len(), 2);
    }
}
