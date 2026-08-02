use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use super::*;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct DurableFixtureState {
    pub(super) record: ContainerRecord,
    pub(super) create_operation: String,
    pub(super) start_operation: Option<String>,
    pub(super) exit_status: Option<ExitStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) process: Option<DurableFixtureProcess>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct DurableFixtureProcess {
    pub(super) request: OciExecRequest,
    pub(super) record: ProcessRecord,
    pub(super) output: Vec<OutputChunk>,
    pub(super) exit_status: Option<ExitStatus>,
    pub(super) stdin_operations: BTreeMap<String, WriteStdinRequest>,
    pub(super) close_stdin_operations: BTreeMap<String, CloseStdinRequest>,
    pub(super) signal_operations: BTreeMap<String, SignalProcessRequest>,
}

pub(super) struct DurableFixtureService {
    pub(super) state_path: PathBuf,
    pub(super) call_log: PathBuf,
    pub(super) lock: Mutex<()>,
}

impl DurableFixtureService {
    pub(super) fn new(state_path: PathBuf, call_log: PathBuf) -> Self {
        Self {
            state_path,
            call_log,
            lock: Mutex::new(()),
        }
    }

    pub(super) fn load(&self, operation: &str) -> OciResult<Option<DurableFixtureState>> {
        match std::fs::read(&self.state_path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
                oci_error(
                    ErrorCode::Internal,
                    operation,
                    format!(
                        "failed to decode durable process fixture {}: {error}",
                        self.state_path.display()
                    ),
                )
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(oci_error(
                ErrorCode::Internal,
                operation,
                format!(
                    "failed to read durable process fixture {}: {error}",
                    self.state_path.display()
                ),
            )),
        }
    }

    pub(super) fn store(&self, state: &DurableFixtureState, operation: &str) -> OciResult<()> {
        let bytes = serde_json::to_vec(state).map_err(|error| {
            oci_error(
                ErrorCode::Internal,
                operation,
                format!("failed to encode durable process fixture: {error}"),
            )
        })?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&self.state_path)
            .map_err(|error| {
                oci_error(
                    ErrorCode::Internal,
                    operation,
                    format!(
                        "failed to open durable process fixture {}: {error}",
                        self.state_path.display()
                    ),
                )
            })?;
        file.write_all(&bytes).map_err(|error| {
            oci_error(
                ErrorCode::Internal,
                operation,
                format!("failed to write durable process fixture: {error}"),
            )
        })?;
        file.sync_all().map_err(|error| {
            oci_error(
                ErrorCode::Internal,
                operation,
                format!("failed to sync durable process fixture: {error}"),
            )
        })
    }

    pub(super) fn append_call(&self, operation: &'static str) -> OciResult<()> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.call_log)
            .map_err(|error| {
                oci_error(
                    ErrorCode::Internal,
                    operation,
                    format!(
                        "failed to open process fixture call log {}: {error}",
                        self.call_log.display()
                    ),
                )
            })?;
        writeln!(file, "{operation}").map_err(|error| {
            oci_error(
                ErrorCode::Internal,
                operation,
                format!("failed to append process fixture call log: {error}"),
            )
        })?;
        file.sync_all().map_err(|error| {
            oci_error(
                ErrorCode::Internal,
                operation,
                format!("failed to sync process fixture call log: {error}"),
            )
        })
    }

    pub(super) fn rewritten_record(
        state: &DurableFixtureState,
        status: ContainerState,
    ) -> OciResult<ContainerRecord> {
        let id = ContainerId::new(state.record.state.id().to_string())?;
        runtime_record(
            &id,
            state.record.generation,
            status,
            state.record.driver,
            state.record.isolation,
            &state.record.config_digest,
            state.record.attachments_digest.as_deref(),
        )
    }
}
