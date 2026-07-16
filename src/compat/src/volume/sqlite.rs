use async_trait::async_trait;
use tokio_rusqlite::rusqlite::{params, ErrorCode, OptionalExtension};
use tokio_rusqlite::Connection;

use super::{
    VolumeId, VolumeRecord, VolumeReplaceResult, VolumeRepository, VolumeRepositoryError,
    VolumeRepositoryResult, VolumeState,
};

#[derive(Clone)]
pub struct SqliteVolumeRepository {
    connection: Connection,
}

impl SqliteVolumeRepository {
    pub(crate) fn new(connection: Connection) -> Self {
        Self { connection }
    }

    async fn call<F, R>(&self, function: F) -> VolumeRepositoryResult<R>
    where
        F: FnOnce(&mut tokio_rusqlite::rusqlite::Connection) -> VolumeRepositoryResult<R>
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

impl std::fmt::Debug for SqliteVolumeRepository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SqliteVolumeRepository")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl VolumeRepository for SqliteVolumeRepository {
    async fn insert(&self, record: VolumeRecord) -> VolumeRepositoryResult<()> {
        validate_record(&record)?;
        let volume_id = record.volume_id().clone();
        let record_json = serialize_record(&record)?;
        self.call(move |connection| {
            match connection.execute(
                "INSERT INTO volume_records(volume_id, record_json) VALUES (?1, ?2)",
                params![volume_id.as_str(), record_json],
            ) {
                Ok(_) => Ok(()),
                Err(error)
                    if error
                        .sqlite_error_code()
                        .is_some_and(|code| code == ErrorCode::ConstraintViolation) =>
                {
                    Err(VolumeRepositoryError::Duplicate)
                }
                Err(error) => Err(unavailable("insert SQLite volume record", error)),
            }
        })
        .await
    }

    async fn get(&self, volume_id: &VolumeId) -> VolumeRepositoryResult<Option<VolumeRecord>> {
        let volume_id = volume_id.clone();
        let record = self
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT record_json FROM volume_records WHERE volume_id = ?1",
                        [volume_id.as_str()],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(|error| unavailable("read SQLite volume record", error))
            })
            .await?;
        record
            .map(|serialized| deserialize_record(&serialized))
            .transpose()
    }

    async fn get_by_owner_name(
        &self,
        owner_id: &str,
        name: &str,
    ) -> VolumeRepositoryResult<Option<VolumeRecord>> {
        let owner_id = owner_id.to_string();
        let name = name.to_string();
        let record = self
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT record_json FROM volume_records \
                         WHERE owner_id = ?1 AND name = ?2",
                        params![owner_id, name],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(|error| unavailable("read SQLite volume by owner and name", error))
            })
            .await?;
        record
            .map(|serialized| deserialize_record(&serialized))
            .transpose()
    }

    async fn list(&self, owner_id: &str) -> VolumeRepositoryResult<Vec<VolumeRecord>> {
        let owner_id = owner_id.to_string();
        let records = self
            .call(move |connection| {
                let mut statement = connection
                    .prepare(
                        "SELECT record_json FROM volume_records \
                         WHERE owner_id = ?1 AND state = 'active' \
                         ORDER BY julianday(created_at), volume_id",
                    )
                    .map_err(|error| unavailable("prepare SQLite volume list", error))?;
                statement
                    .query_map([owner_id], |row| row.get::<_, String>(0))
                    .map_err(|error| unavailable("query SQLite volume list", error))?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|error| unavailable("read SQLite volume list", error))
            })
            .await?;
        records
            .iter()
            .map(|record| deserialize_record(record))
            .collect()
    }

    async fn list_in_state(&self, state: VolumeState) -> VolumeRepositoryResult<Vec<VolumeRecord>> {
        let state = state.as_str().to_string();
        let records = self
            .call(move |connection| {
                let mut statement = connection
                    .prepare(
                        "SELECT record_json FROM volume_records WHERE state = ?1 \
                         ORDER BY julianday(created_at), volume_id",
                    )
                    .map_err(|error| unavailable("prepare SQLite volume reconciliation", error))?;
                statement
                    .query_map([state], |row| row.get::<_, String>(0))
                    .map_err(|error| unavailable("query SQLite volume reconciliation", error))?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|error| unavailable("read SQLite volume reconciliation", error))
            })
            .await?;
        records
            .iter()
            .map(|record| deserialize_record(record))
            .collect()
    }

    async fn replace(
        &self,
        expected: VolumeState,
        replacement: VolumeRecord,
    ) -> VolumeRepositoryResult<VolumeReplaceResult> {
        validate_record(&replacement)?;
        let volume_id = replacement.volume_id().clone();
        let expected = expected.as_str().to_string();
        let record_json = serialize_record(&replacement)?;
        self.call(move |connection| {
            let updated = connection
                .execute(
                    "UPDATE volume_records SET record_json = ?1 \
                     WHERE volume_id = ?2 AND state = ?3",
                    params![record_json, volume_id.as_str(), expected],
                )
                .map_err(|error| unavailable("replace SQLite volume record", error))?;
            if updated == 1 {
                return Ok(VolumeReplaceResult::Updated);
            }
            existence_result(connection, &volume_id)
        })
        .await
    }

    async fn delete(
        &self,
        volume_id: &VolumeId,
        expected: VolumeState,
    ) -> VolumeRepositoryResult<VolumeReplaceResult> {
        let volume_id = volume_id.clone();
        let expected = expected.as_str().to_string();
        self.call(move |connection| {
            let deleted = connection
                .execute(
                    "DELETE FROM volume_records WHERE volume_id = ?1 AND state = ?2",
                    params![volume_id.as_str(), expected],
                )
                .map_err(|error| unavailable("delete SQLite volume record", error))?;
            if deleted == 1 {
                return Ok(VolumeReplaceResult::Updated);
            }
            existence_result(connection, &volume_id)
        })
        .await
    }
}

fn existence_result(
    connection: &tokio_rusqlite::rusqlite::Connection,
    volume_id: &VolumeId,
) -> VolumeRepositoryResult<VolumeReplaceResult> {
    let exists = connection
        .query_row(
            "SELECT 1 FROM volume_records WHERE volume_id = ?1",
            [volume_id.as_str()],
            |_| Ok(()),
        )
        .optional()
        .map_err(|error| unavailable("inspect SQLite volume conflict", error))?;
    Ok(if exists.is_some() {
        VolumeReplaceResult::Conflict
    } else {
        VolumeReplaceResult::NotFound
    })
}

fn validate_record(record: &VolumeRecord) -> VolumeRepositoryResult<()> {
    record
        .validate()
        .map_err(|error| VolumeRepositoryError::Corrupt(error.to_string()))
}

fn serialize_record(record: &VolumeRecord) -> VolumeRepositoryResult<String> {
    serde_json::to_string(record).map_err(|error| {
        VolumeRepositoryError::Corrupt(format!("serialize SQLite volume record: {error}"))
    })
}

fn deserialize_record(record: &str) -> VolumeRepositoryResult<VolumeRecord> {
    let record: VolumeRecord = serde_json::from_str(record).map_err(|error| {
        VolumeRepositoryError::Corrupt(format!("deserialize SQLite volume record: {error}"))
    })?;
    validate_record(&record)?;
    Ok(record)
}

fn unavailable(context: &str, error: impl std::fmt::Display) -> VolumeRepositoryError {
    VolumeRepositoryError::Unavailable(format!("{context}: {error}"))
}

fn map_async_error(error: tokio_rusqlite::Error<VolumeRepositoryError>) -> VolumeRepositoryError {
    match error {
        tokio_rusqlite::Error::Error(error) => error,
        tokio_rusqlite::Error::ConnectionClosed => {
            VolumeRepositoryError::Unavailable("SQLite repository connection closed".to_string())
        }
        _ => VolumeRepositoryError::Unavailable(format!("SQLite repository failed: {error}")),
    }
}
