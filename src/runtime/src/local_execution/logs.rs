use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use a3s_box_core::log::{LogDriver, LogEntry};
use a3s_box_core::{
    ExecutionGeneration, ExecutionId, ExecutionManagerError, ExecutionManagerResult,
};
use flate2::read::GzDecoder;

use super::support::require_generation;
use super::LocalExecutionManager;

const MAX_DECOMPRESSED_LOG_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ROTATED_LOG_FILES: u32 = 100;

impl LocalExecutionManager {
    pub(super) async fn read_structured_logs(
        &self,
        execution_id: &ExecutionId,
        expected_generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<Vec<LogEntry>> {
        let record = self
            .get(execution_id)
            .await?
            .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
        require_generation(&record, execution_id, expected_generation)?;
        if record.log_config.driver != LogDriver::JsonFile {
            return Ok(Vec::new());
        }

        let expected_box_dir = safe_box_dir(&self.home_dir, execution_id)?;
        if record.box_dir != expected_box_dir {
            return Err(ExecutionManagerError::Internal(format!(
                "execution {execution_id} has an unexpected log directory"
            )));
        }
        let log_dir = expected_box_dir.join("logs");
        let max_files = record.log_config.max_file().min(MAX_ROTATED_LOG_FILES);
        tokio::task::spawn_blocking(move || read_log_files(&log_dir, max_files))
            .await
            .map_err(|error| {
                ExecutionManagerError::Internal(format!("structured log reader failed: {error}"))
            })?
            .map_err(ExecutionManagerError::Internal)
    }
}

fn safe_box_dir(home_dir: &Path, execution_id: &ExecutionId) -> ExecutionManagerResult<PathBuf> {
    let value = execution_id.as_str();
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value.contains('\0')
    {
        return Err(ExecutionManagerError::Internal(format!(
            "execution {execution_id} has an unsafe internal identity"
        )));
    }
    Ok(home_dir.join("boxes").join(value))
}

fn read_log_files(log_dir: &Path, max_files: u32) -> Result<Vec<LogEntry>, String> {
    let base = log_dir.join("container.json");
    let mut entries = Vec::new();
    let mut bytes_read = 0_u64;

    for index in (1..=max_files).rev() {
        let path = rotated_path(&base, index);
        let Some(file) = open_if_present(&path)? else {
            continue;
        };
        read_entries(
            GzDecoder::new(file),
            &path,
            false,
            &mut bytes_read,
            &mut entries,
        )?;
    }
    if let Some(file) = open_if_present(&base)? {
        read_entries(file, &base, true, &mut bytes_read, &mut entries)?;
    }
    Ok(entries)
}

fn open_if_present(path: &Path) -> Result<Option<std::fs::File>, String> {
    match std::fs::File::open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("failed to open {}: {error}", path.display())),
    }
}

fn read_entries(
    source: impl Read,
    path: &Path,
    allow_trailing_partial: bool,
    bytes_read: &mut u64,
    entries: &mut Vec<LogEntry>,
) -> Result<(), String> {
    let mut reader = BufReader::new(source);
    let mut line = Vec::new();
    loop {
        line.clear();
        let count = reader
            .read_until(b'\n', &mut line)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        if count == 0 {
            return Ok(());
        }
        *bytes_read = bytes_read
            .checked_add(count as u64)
            .ok_or_else(|| "structured log byte count overflowed".to_string())?;
        if *bytes_read > MAX_DECOMPRESSED_LOG_BYTES {
            return Err(format!(
                "structured logs exceed the {} byte read limit",
                MAX_DECOMPRESSED_LOG_BYTES
            ));
        }
        if !line.ends_with(b"\n") {
            if allow_trailing_partial {
                return Ok(());
            }
            return Err(format!(
                "rotated structured log {} ends with a partial entry",
                path.display()
            ));
        }
        line.pop();
        if line.ends_with(b"\r") {
            line.pop();
        }
        if line.is_empty() {
            continue;
        }
        entries.push(serde_json::from_slice(&line).map_err(|error| {
            format!(
                "structured log {} contains invalid JSON: {error}",
                path.display()
            )
        })?);
    }
}

fn rotated_path(base: &Path, index: u32) -> PathBuf {
    let mut path: OsString = base.as_os_str().to_owned();
    path.push(format!(".{index}.gz"));
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::write::GzEncoder;
    use flate2::Compression;

    use super::*;

    fn entry(message: &str, timestamp: &str) -> LogEntry {
        LogEntry {
            log: message.to_string(),
            stream: "stdout".to_string(),
            time: timestamp.to_string(),
        }
    }

    #[test]
    fn reads_rotated_logs_oldest_first_and_ignores_a_partial_live_tail() {
        let temporary = tempfile::tempdir().unwrap();
        let base = temporary.path().join("container.json");
        let rotated = rotated_path(&base, 1);
        let mut encoder =
            GzEncoder::new(std::fs::File::create(rotated).unwrap(), Compression::fast());
        writeln!(
            encoder,
            "{}",
            serde_json::to_string(&entry("old\n", "2026-07-14T12:00:00Z")).unwrap()
        )
        .unwrap();
        encoder.finish().unwrap();
        std::fs::write(
            &base,
            format!(
                "{}\n{{\"log\":\"partial",
                serde_json::to_string(&entry("new\n", "2026-07-14T12:00:01Z")).unwrap()
            ),
        )
        .unwrap();

        assert_eq!(
            read_log_files(temporary.path(), 1).unwrap(),
            vec![
                entry("old\n", "2026-07-14T12:00:00Z"),
                entry("new\n", "2026-07-14T12:00:01Z")
            ]
        );
    }

    #[test]
    fn rejects_invalid_complete_entries() {
        let temporary = tempfile::tempdir().unwrap();
        std::fs::write(temporary.path().join("container.json"), "not-json\n").unwrap();
        assert!(read_log_files(temporary.path(), 0)
            .unwrap_err()
            .contains("invalid JSON"));
    }
}
