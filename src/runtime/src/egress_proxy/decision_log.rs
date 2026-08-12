use std::path::{Path, PathBuf};

use a3s_box_core::{
    EgressDecisionDestination, EgressDecisionProtocol, EgressEvaluation, EgressPolicyLimits,
    ExecutionGeneration, ExecutionId,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

pub const EGRESS_DECISION_LOG_SCHEMA_V1: &str = "a3s.box.egress-decision-log.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressRuntimeDecisionReason {
    AuthenticationFailed,
    MalformedRequest,
    MalformedPolicyQuery,
    RequestHeaderTimeout,
    RequestHeaderTooLarge,
    ConnectionLimitExceeded,
    PendingConnectionLimitExceeded,
    DnsQueryBudgetExhausted,
    DnsCacheBudgetExhausted,
    DnsAnswerBudgetExceeded,
    DnsTimeout,
    DnsResolutionFailed,
    ConnectTimeout,
    ConnectFailed,
    DecisionLogBudgetExhausted,
    ProxyShutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EgressDecisionEvent {
    Policy {
        evaluation: EgressEvaluation,
    },
    RuntimeDenied {
        reason: EgressRuntimeDecisionReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        protocol: Option<EgressDecisionProtocol>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        destination: Option<EgressDecisionDestination>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
    },
    BudgetExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressDecisionRecord {
    pub schema: String,
    pub sequence: u32,
    pub timestamp: DateTime<Utc>,
    pub execution_id: ExecutionId,
    pub generation: ExecutionGeneration,
    pub event: EgressDecisionEvent,
}

#[derive(Debug, Error)]
pub enum EgressDecisionLogError {
    #[error("egress decision log budget exhausted")]
    BudgetExhausted,
    #[error("egress decision record exceeds its byte limit")]
    RecordTooLarge,
    #[error("egress decision log I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("egress decision serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

struct DecisionLogState {
    file: tokio::fs::File,
    sequence: u32,
    records: u32,
    bytes: u64,
    exhausted: bool,
}

/// Bounded, generation-scoped JSON-lines decision log.
pub struct EgressDecisionLog {
    path: PathBuf,
    execution_id: ExecutionId,
    generation: ExecutionGeneration,
    limits: EgressPolicyLimits,
    terminal_reserve_bytes: u64,
    state: Mutex<DecisionLogState>,
}

impl EgressDecisionLog {
    pub async fn create(
        path: impl Into<PathBuf>,
        execution_id: ExecutionId,
        generation: ExecutionGeneration,
        limits: EgressPolicyLimits,
    ) -> Result<Self, EgressDecisionLogError> {
        let path = path.into();
        let terminal = EgressDecisionRecord {
            schema: EGRESS_DECISION_LOG_SCHEMA_V1.to_string(),
            sequence: limits.max_decision_records,
            timestamp: Utc::now(),
            execution_id: execution_id.clone(),
            generation,
            event: EgressDecisionEvent::BudgetExhausted,
        };
        let terminal_size = encoded_record(&terminal)?.len() as u64;
        if terminal_size > u64::from(limits.max_decision_record_bytes) {
            return Err(EgressDecisionLogError::RecordTooLarge);
        }
        let create_path = path.clone();
        let file = tokio::task::spawn_blocking(move || create_owner_only_file(&create_path))
            .await
            .map_err(|error| {
                std::io::Error::other(format!("decision log task failed: {error}"))
            })??;

        Ok(Self {
            path,
            execution_id,
            generation,
            limits,
            terminal_reserve_bytes: terminal_size.saturating_add(32),
            state: Mutex::new(DecisionLogState {
                file: tokio::fs::File::from_std(file),
                sequence: 0,
                records: 0,
                bytes: 0,
                exhausted: false,
            }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn append_policy(
        &self,
        evaluation: EgressEvaluation,
    ) -> Result<(), EgressDecisionLogError> {
        self.append(EgressDecisionEvent::Policy { evaluation })
            .await
    }

    pub async fn append_runtime_denied(
        &self,
        reason: EgressRuntimeDecisionReason,
        protocol: Option<EgressDecisionProtocol>,
        destination: Option<EgressDecisionDestination>,
        port: Option<u16>,
    ) -> Result<(), EgressDecisionLogError> {
        self.append(EgressDecisionEvent::RuntimeDenied {
            reason,
            protocol,
            destination,
            port,
        })
        .await
    }

    pub async fn finish(&self) -> Result<(), EgressDecisionLogError> {
        let state = self.state.lock().await;
        state.file.sync_all().await.map_err(Into::into)
    }

    async fn append(&self, event: EgressDecisionEvent) -> Result<(), EgressDecisionLogError> {
        let mut state = self.state.lock().await;
        if state.exhausted {
            return Err(EgressDecisionLogError::BudgetExhausted);
        }
        let record = self.record(state.sequence.saturating_add(1), event);
        let encoded = encoded_record(&record)?;
        let record_bytes = encoded.len() as u64;
        let leaves_terminal_record = state.records.saturating_add(1)
            < self.limits.max_decision_records
            && state
                .bytes
                .saturating_add(record_bytes)
                .saturating_add(self.terminal_reserve_bytes)
                <= self.limits.max_decision_log_bytes;
        if record_bytes > u64::from(self.limits.max_decision_record_bytes)
            || !leaves_terminal_record
        {
            self.write_terminal(&mut state).await?;
            return Err(EgressDecisionLogError::BudgetExhausted);
        }

        state.file.write_all(&encoded).await?;
        state.file.flush().await?;
        state.sequence = record.sequence;
        state.records = state.records.saturating_add(1);
        state.bytes = state.bytes.saturating_add(record_bytes);
        Ok(())
    }

    async fn write_terminal(
        &self,
        state: &mut DecisionLogState,
    ) -> Result<(), EgressDecisionLogError> {
        if state.exhausted {
            return Ok(());
        }
        let terminal = self.record(
            state.sequence.saturating_add(1),
            EgressDecisionEvent::BudgetExhausted,
        );
        let encoded = encoded_record(&terminal)?;
        if encoded.len() as u64 > self.terminal_reserve_bytes
            || state.bytes.saturating_add(encoded.len() as u64) > self.limits.max_decision_log_bytes
        {
            return Err(EgressDecisionLogError::RecordTooLarge);
        }
        state.file.write_all(&encoded).await?;
        state.file.flush().await?;
        state.sequence = terminal.sequence;
        state.records = state.records.saturating_add(1);
        state.bytes = state.bytes.saturating_add(encoded.len() as u64);
        state.exhausted = true;
        Ok(())
    }

    fn record(&self, sequence: u32, event: EgressDecisionEvent) -> EgressDecisionRecord {
        EgressDecisionRecord {
            schema: EGRESS_DECISION_LOG_SCHEMA_V1.to_string(),
            sequence,
            timestamp: Utc::now(),
            execution_id: self.execution_id.clone(),
            generation: self.generation,
            event,
        }
    }
}

fn encoded_record(record: &EgressDecisionRecord) -> Result<Vec<u8>, serde_json::Error> {
    let mut encoded = serde_json::to_vec(record)?;
    encoded.push(b'\n');
    Ok(encoded)
}

fn create_owner_only_file(path: &Path) -> std::io::Result<std::fs::File> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("decision log path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let metadata = std::fs::symlink_metadata(parent)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::other(
            "decision log parent is not a plain directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

        // SAFETY: querying the effective process UID has no preconditions.
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "decision log directory is not owned by the runtime user",
            ));
        }
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        return std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path);
    }
    #[cfg(not(unix))]
    {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
    }
}

#[cfg(test)]
mod tests {
    use a3s_box_core::{
        CompiledEgressPolicy, EgressHttpScheme, EgressPolicy, ExecutionGeneration, ExecutionId,
    };

    use super::*;

    fn identity() -> (ExecutionId, ExecutionGeneration) {
        (
            ExecutionId::new("decision-log-test").unwrap(),
            ExecutionGeneration::INITIAL,
        )
    }

    #[tokio::test]
    async fn decision_log_is_no_clobber_generation_scoped_and_redacted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("generation-1.jsonl");
        let (execution_id, generation) = identity();
        let log =
            EgressDecisionLog::create(&path, execution_id.clone(), generation, Default::default())
                .await
                .unwrap();
        let evaluation =
            CompiledEgressPolicy::compile(&EgressPolicy::allow_domains(["api.example.com"]))
                .unwrap()
                .evaluate_http(EgressHttpScheme::Https, "api.example.com", 443);
        log.append_policy(evaluation).await.unwrap();
        log.finish().await.unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let record: EgressDecisionRecord = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(record.execution_id, execution_id);
        assert_eq!(record.generation, generation);
        assert!(!content.contains("authorization"));
        assert!(!content.contains("secret"));

        assert!(EgressDecisionLog::create(
            &path,
            ExecutionId::new("decision-log-test").unwrap(),
            generation,
            Default::default(),
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn decision_budget_reserves_one_terminal_record_and_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("generation-1.jsonl");
        let (execution_id, generation) = identity();
        let mut limits = EgressPolicyLimits::default();
        limits.max_decision_records = 2;
        let log = EgressDecisionLog::create(&path, execution_id, generation, limits)
            .await
            .unwrap();
        let evaluation = CompiledEgressPolicy::compile(&EgressPolicy::DenyAll)
            .unwrap()
            .evaluate_http(EgressHttpScheme::Https, "example.com", 443);

        log.append_policy(evaluation.clone()).await.unwrap();
        assert!(matches!(
            log.append_policy(evaluation).await,
            Err(EgressDecisionLogError::BudgetExhausted)
        ));
        log.finish().await.unwrap();
        let content = tokio::fs::read_to_string(path).await.unwrap();
        let lines = content.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains("budget_exhausted"));
    }
}
