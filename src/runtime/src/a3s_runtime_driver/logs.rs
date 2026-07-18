//! Ordered Runtime log cursors over Box's structured json-file projection.

use a3s_box_core::ExecutionManager;
use a3s_runtime::contract::{RuntimeLogChunk, RuntimeLogQuery, RuntimeLogStream};
use a3s_runtime::{RuntimeError, RuntimeResult, RuntimeUnitRecord};
use chrono::DateTime;
use sha2::{Digest, Sha256};

use super::metadata::{local_identity, map_execution_error, provider_identity_matches};
use super::BoxRuntimeDriver;

const RECORDS_PER_SECOND: u64 = 1_000_000;

impl BoxRuntimeDriver {
    pub(super) async fn read_runtime_logs(
        &self,
        unit: &RuntimeUnitRecord,
        query: &RuntimeLogQuery,
    ) -> RuntimeResult<Vec<RuntimeLogChunk>> {
        query.validate().map_err(RuntimeError::InvalidRequest)?;
        if query.unit_id != unit.spec.unit_id || query.generation != unit.spec.generation {
            return Err(RuntimeError::InvalidRequest(
                "Runtime log query identity does not match its unit record".into(),
            ));
        }
        let record =
            self.find_generation(&unit.spec)
                .await?
                .ok_or_else(|| RuntimeError::NotFound {
                    unit_id: unit.spec.unit_id.clone(),
                })?;
        provider_identity_matches(&unit.observation, &record)?;
        let (execution_id, local_generation, _) = local_identity(&record)?;
        let entries = self
            .manager
            .read_logs(&execution_id, local_generation)
            .await
            .map_err(|error| map_execution_error(&unit.spec.unit_id, error))?;
        project_logs(entries, query)
    }
}

fn project_logs(
    entries: Vec<a3s_box_core::log::LogEntry>,
    query: &RuntimeLogQuery,
) -> RuntimeResult<Vec<RuntimeLogChunk>> {
    let requested_cursor = query.cursor.as_deref().map(LogCursor::parse).transpose()?;
    if requested_cursor
        .as_ref()
        .is_some_and(|cursor| cursor.generation != query.generation)
    {
        return Err(RuntimeError::InvalidRequest(
            "Box log cursor belongs to another Runtime generation".into(),
        ));
    }
    let target = requested_cursor.as_ref().map(LogCursor::encode);
    let mut cursor_found = target.is_none();
    let mut chunks = Vec::with_capacity(query.limit as usize);
    let mut prior_timestamp = None;
    let mut current_second = None;
    let mut ordinal = 0_u64;

    for entry in entries {
        let stream = match entry.stream.as_str() {
            "stdout" => RuntimeLogStream::Stdout,
            "stderr" => RuntimeLogStream::Stderr,
            value => {
                return Err(RuntimeError::Protocol(format!(
                    "Box structured log contains unsupported stream {value:?}"
                )))
            }
        };
        if entry.log.len() > 1024 * 1024 {
            return Err(RuntimeError::Protocol(
                "Box log record exceeds the Runtime one-MiB chunk bound".into(),
            ));
        }
        let timestamp_ns = DateTime::parse_from_rfc3339(&entry.time)
            .map_err(|_| RuntimeError::Protocol("Box log timestamp is invalid".into()))?
            .timestamp_nanos_opt()
            .ok_or_else(|| RuntimeError::Protocol("Box log timestamp is out of range".into()))?;
        if prior_timestamp.is_some_and(|prior| timestamp_ns < prior) {
            return Err(RuntimeError::Protocol(
                "Box log records are not ordered by provider timestamp".into(),
            ));
        }
        prior_timestamp = Some(timestamp_ns);
        let second = timestamp_ns.div_euclid(1_000_000_000);
        if current_second != Some(second) {
            current_second = Some(second);
            ordinal = 0;
        }
        if ordinal >= RECORDS_PER_SECOND {
            return Err(RuntimeError::Protocol(
                "Box emitted more than one million log records in one second".into(),
            ));
        }
        let raw = RawLogRecord {
            generation: query.generation,
            timestamp_ns,
            ordinal,
            stream,
            data: entry.log,
        };
        ordinal += 1;
        let cursor = LogCursor::new(&raw).encode();
        let sequence = log_sequence(second, ordinal)?;

        if !cursor_found {
            if target.as_deref() == Some(cursor.as_str()) {
                cursor_found = true;
            }
            continue;
        }
        if query.stream.is_some_and(|requested| requested != stream) {
            continue;
        }
        let observed_at_ms = u64::try_from(timestamp_ns.div_euclid(1_000_000))
            .map_err(|_| RuntimeError::Protocol("Box log timestamp precedes the epoch".into()))?;
        let chunk = RuntimeLogChunk {
            schema: RuntimeLogChunk::SCHEMA.into(),
            cursor,
            sequence,
            observed_at_ms,
            stream,
            data: raw.data,
        };
        chunk.validate().map_err(RuntimeError::Protocol)?;
        chunks.push(chunk);
        if chunks.len() == query.limit as usize {
            break;
        }
    }

    if !cursor_found {
        return Err(RuntimeError::Protocol(
            "Box log cursor is no longer available; the stream contains an explicit rotation gap"
                .into(),
        ));
    }
    if chunks
        .windows(2)
        .any(|pair| pair[0].sequence >= pair[1].sequence)
    {
        return Err(RuntimeError::Protocol(
            "Box log projection produced unordered Runtime chunks".into(),
        ));
    }
    Ok(chunks)
}

struct RawLogRecord {
    generation: u64,
    timestamp_ns: i64,
    ordinal: u64,
    stream: RuntimeLogStream,
    data: String,
}

struct LogCursor {
    generation: u64,
    timestamp_ns: i64,
    ordinal: u64,
    stream: RuntimeLogStream,
    digest: String,
}

impl LogCursor {
    fn new(record: &RawLogRecord) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"a3s-box-log-cursor-v1\0");
        hash.update(record.generation.to_be_bytes());
        hash.update(record.timestamp_ns.to_be_bytes());
        hash.update(record.ordinal.to_be_bytes());
        hash.update(match record.stream {
            RuntimeLogStream::Stdout => b"stdout".as_slice(),
            RuntimeLogStream::Stderr => b"stderr".as_slice(),
        });
        hash.update(record.data.as_bytes());
        let digest = format!("{:x}", hash.finalize());
        Self {
            generation: record.generation,
            timestamp_ns: record.timestamp_ns,
            ordinal: record.ordinal,
            stream: record.stream,
            digest: digest[..16].into(),
        }
    }

    fn parse(value: &str) -> RuntimeResult<Self> {
        let fields = value.split(':').collect::<Vec<_>>();
        if fields.len() != 6 || fields[0] != "v1" {
            return Err(RuntimeError::InvalidRequest(
                "invalid Box log cursor".into(),
            ));
        }
        let generation = fields[1]
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| RuntimeError::InvalidRequest("invalid Box log cursor".into()))?;
        let timestamp_ns = fields[2]
            .parse::<i64>()
            .map_err(|_| RuntimeError::InvalidRequest("invalid Box log cursor".into()))?;
        let ordinal = fields[3]
            .parse::<u64>()
            .map_err(|_| RuntimeError::InvalidRequest("invalid Box log cursor".into()))?;
        if ordinal >= RECORDS_PER_SECOND {
            return Err(RuntimeError::InvalidRequest(
                "invalid Box log cursor".into(),
            ));
        }
        let stream = match fields[4] {
            "o" => RuntimeLogStream::Stdout,
            "e" => RuntimeLogStream::Stderr,
            _ => {
                return Err(RuntimeError::InvalidRequest(
                    "invalid Box log cursor".into(),
                ))
            }
        };
        let digest = fields[5];
        if digest.len() != 16 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(RuntimeError::InvalidRequest(
                "invalid Box log cursor".into(),
            ));
        }
        Ok(Self {
            generation,
            timestamp_ns,
            ordinal,
            stream,
            digest: digest.into(),
        })
    }

    fn encode(&self) -> String {
        let stream = match self.stream {
            RuntimeLogStream::Stdout => "o",
            RuntimeLogStream::Stderr => "e",
        };
        format!(
            "v1:{}:{}:{}:{stream}:{}",
            self.generation, self.timestamp_ns, self.ordinal, self.digest
        )
    }
}

fn log_sequence(second: i64, ordinal_after_increment: u64) -> RuntimeResult<u64> {
    u64::try_from(second)
        .ok()
        .and_then(|second| second.checked_mul(RECORDS_PER_SECOND))
        .and_then(|base| base.checked_add(ordinal_after_increment))
        .ok_or_else(|| RuntimeError::Protocol("Box log sequence overflowed".into()))
}

#[cfg(test)]
mod tests {
    use a3s_box_core::log::LogEntry;

    use super::*;

    fn query(cursor: Option<String>, stream: Option<RuntimeLogStream>) -> RuntimeLogQuery {
        RuntimeLogQuery {
            schema: RuntimeLogQuery::SCHEMA.into(),
            unit_id: "unit-1".into(),
            generation: 7,
            cursor,
            limit: 10,
            stream,
        }
    }

    #[test]
    fn cursor_resume_preserves_same_timestamp_total_order_and_filtering() {
        let entries = vec![
            LogEntry {
                log: "first\n".into(),
                stream: "stdout".into(),
                time: "2026-07-17T00:00:00.123456789Z".into(),
            },
            LogEntry {
                log: "second\n".into(),
                stream: "stderr".into(),
                time: "2026-07-17T00:00:00.123456789Z".into(),
            },
            LogEntry {
                log: "third\n".into(),
                stream: "stdout".into(),
                time: "2026-07-17T00:00:01Z".into(),
            },
        ];
        let all = project_logs(entries.clone(), &query(None, None)).unwrap();
        assert_eq!(all.len(), 3);
        assert!(all[0].sequence < all[1].sequence && all[1].sequence < all[2].sequence);

        let resumed = project_logs(
            entries,
            &query(Some(all[0].cursor.clone()), Some(RuntimeLogStream::Stdout)),
        )
        .unwrap();
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed[0].data, "third\n");
    }

    #[test]
    fn missing_valid_cursor_reports_an_explicit_gap() {
        let cursor = LogCursor {
            generation: 7,
            timestamp_ns: 1,
            ordinal: 0,
            stream: RuntimeLogStream::Stdout,
            digest: "0123456789abcdef".into(),
        }
        .encode();
        let error = project_logs(Vec::new(), &query(Some(cursor), None)).unwrap_err();
        assert!(error.to_string().contains("rotation gap"));
    }
}
