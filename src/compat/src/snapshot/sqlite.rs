use async_trait::async_trait;
use tokio_rusqlite::rusqlite::{params, ErrorCode, OptionalExtension};
use tokio_rusqlite::Connection;

use super::{
    SnapshotId, SnapshotRecord, SnapshotReplaceResult, SnapshotRepository,
    SnapshotRepositoryError, SnapshotRepositoryResult, SnapshotState,
};

#[derive(Clone)]
pub struct SqliteSnapshotRepository {
    connection: Connection,
}

impl SqliteSnapshotRepository {
    pub(crate) fn new(connection: Connection) -> Self {
        Self { connection }
    }

    async fn call<F, R>(&self, function: F) -> SnapshotRepositoryResult<R>
    where
        F: FnOnce(&mut tokio_rusqlite::rusqlite::Connection) -> SnapshotRepositoryResult<R>
            + Send
            + 'static,
        R: Send + 'static,
    {
        self.connection
            .call(function)
            .await
            .map_err(map_async_error)
    }
}

impl std::fmt::Debug for SqliteSnapshotRepository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SqliteSnapshotRepository")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl SnapshotRepository for SqliteSnapshotRepository {
    async fn insert(&self, record: SnapshotRecord) -> SnapshotRepositoryResult<()> {
        validate_record(&record)?;
        let snapshot_id = record.snapshot_id().clone();
        let record_json = serialize_record(&record)?;
        self.call(move |connection| {
            match connection.execute(
                "INSERT INTO snapshot_records(snapshot_id, record_json) VALUES (?1, ?2)",
                params![snapshot_id.as_str(), record_json],
            ) {
                Ok(_) => Ok(()),
                Err(error)
                    if error
                        .sqlite_error_code()
                        .is_some_and(|code| code == ErrorCode::ConstraintViolation) =>
                {
                    Err(SnapshotRepositoryError::Duplicate)
                }
                Err(error) => Err(unavailable("insert SQLite snapshot record", error)),
            }
        })
        .await
    }

    async fn get(
        &self,
        snapshot_id: &SnapshotId,
    ) -> SnapshotRepositoryResult<Option<SnapshotRecord>> {
        let snapshot_id = snapshot_id.clone();
        let record = self
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT record_json FROM snapshot_records WHERE snapshot_id = ?1",
                        [snapshot_id.as_str()],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(|error| unavailable("read SQLite snapshot record", error))
            })
            .await?;
        record
            .map(|serialized| deserialize_record(&serialized))
            .transpose()
    }

    async fn get_by_reference(
        &self,
        owner_id: &str,
        reference: &str,
    ) -> SnapshotRepositoryResult<Option<SnapshotRecord>> {
        let owner_id = owner_id.to_string();
        let reference = reference.to_string();
        let record = self
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT record_json FROM snapshot_records \
                         WHERE owner_id = ?1 AND reference = ?2",
                        params![owner_id, reference],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(|error| unavailable("read SQLite snapshot reference", error))
            })
            .await?;
        record
            .map(|serialized| deserialize_record(&serialized))
            .transpose()
    }

    async fn list(&self, owner_id: &str) -> SnapshotRepositoryResult<Vec<SnapshotRecord>> {
        let owner_id = owner_id.to_string();
        let records = self
            .call(move |connection| {
                let mut statement = connection
                    .prepare(
                        "SELECT record_json FROM snapshot_records \
                         WHERE owner_id = ?1 AND state = 'active' \
                         ORDER BY julianday(created_at), snapshot_id",
                    )
                    .map_err(|error| unavailable("prepare SQLite snapshot list", error))?;
                let records = statement
                    .query_map([owner_id], |row| row.get::<_, String>(0))
                    .map_err(|error| unavailable("query SQLite snapshot list", error))?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|error| unavailable("read SQLite snapshot list", error))?;
                Ok(records)
            })
            .await?;
        records
            .iter()
            .map(|record| deserialize_record(record))
            .collect()
    }

    async fn list_in_state(
        &self,
        state: SnapshotState,
    ) -> SnapshotRepositoryResult<Vec<SnapshotRecord>> {
        let state = state.as_str().to_string();
        let records = self
            .call(move |connection| {
                let mut statement = connection
                    .prepare(
                        "SELECT record_json FROM snapshot_records WHERE state = ?1 \
                         ORDER BY julianday(created_at), snapshot_id",
                    )
                    .map_err(|error| unavailable("prepare SQLite snapshot reconciliation", error))?;
                let records = statement
                    .query_map([state], |row| row.get::<_, String>(0))
                    .map_err(|error| unavailable("query SQLite snapshot reconciliation", error))?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|error| unavailable("read SQLite snapshot reconciliation", error))?;
                Ok(records)
            })
            .await?;
        records
            .iter()
            .map(|record| deserialize_record(record))
            .collect()
    }

    async fn replace(
        &self,
        expected: SnapshotState,
        replacement: SnapshotRecord,
    ) -> SnapshotRepositoryResult<SnapshotReplaceResult> {
        validate_record(&replacement)?;
        let snapshot_id = replacement.snapshot_id().clone();
        let expected = expected.as_str().to_string();
        let record_json = serialize_record(&replacement)?;
        self.call(move |connection| {
            let updated = connection
                .execute(
                    "UPDATE snapshot_records SET record_json = ?1 \
                     WHERE snapshot_id = ?2 AND state = ?3",
                    params![record_json, snapshot_id.as_str(), expected],
                )
                .map_err(|error| unavailable("replace SQLite snapshot record", error))?;
            if updated == 1 {
                return Ok(SnapshotReplaceResult::Updated);
            }
            existence_result(connection, &snapshot_id)
        })
        .await
    }

    async fn delete(
        &self,
        snapshot_id: &SnapshotId,
        expected: SnapshotState,
    ) -> SnapshotRepositoryResult<SnapshotReplaceResult> {
        let snapshot_id = snapshot_id.clone();
        let expected = expected.as_str().to_string();
        self.call(move |connection| {
            let deleted = connection
                .execute(
                    "DELETE FROM snapshot_records WHERE snapshot_id = ?1 AND state = ?2",
                    params![snapshot_id.as_str(), expected],
                )
                .map_err(|error| unavailable("delete SQLite snapshot record", error))?;
            if deleted == 1 {
                return Ok(SnapshotReplaceResult::Updated);
            }
            existence_result(connection, &snapshot_id)
        })
        .await
    }
}

fn existence_result(
    connection: &tokio_rusqlite::rusqlite::Connection,
    snapshot_id: &SnapshotId,
) -> SnapshotRepositoryResult<SnapshotReplaceResult> {
    let exists = connection
        .query_row(
            "SELECT 1 FROM snapshot_records WHERE snapshot_id = ?1",
            [snapshot_id.as_str()],
            |_| Ok(()),
        )
        .optional()
        .map_err(|error| unavailable("inspect SQLite snapshot conflict", error))?;
    Ok(if exists.is_some() {
        SnapshotReplaceResult::Conflict
    } else {
        SnapshotReplaceResult::NotFound
    })
}

fn validate_record(record: &SnapshotRecord) -> SnapshotRepositoryResult<()> {
    record
        .validate()
        .map_err(|error| SnapshotRepositoryError::Corrupt(error.to_string()))
}

fn serialize_record(record: &SnapshotRecord) -> SnapshotRepositoryResult<String> {
    serde_json::to_string(record).map_err(|error| {
        SnapshotRepositoryError::Corrupt(format!("serialize SQLite snapshot record: {error}"))
    })
}

fn deserialize_record(record: &str) -> SnapshotRepositoryResult<SnapshotRecord> {
    let record: SnapshotRecord = serde_json::from_str(record).map_err(|error| {
        SnapshotRepositoryError::Corrupt(format!("deserialize SQLite snapshot record: {error}"))
    })?;
    validate_record(&record)?;
    Ok(record)
}

fn unavailable(context: &str, error: impl std::fmt::Display) -> SnapshotRepositoryError {
    SnapshotRepositoryError::Unavailable(format!("{context}: {error}"))
}

fn map_async_error(
    error: tokio_rusqlite::Error<SnapshotRepositoryError>,
) -> SnapshotRepositoryError {
    match error {
        tokio_rusqlite::Error::Error(error) => error,
        tokio_rusqlite::Error::ConnectionClosed => {
            SnapshotRepositoryError::Unavailable("SQLite repository connection closed".to_string())
        }
        _ => SnapshotRepositoryError::Unavailable(format!("SQLite repository failed: {error}")),
    }
}
