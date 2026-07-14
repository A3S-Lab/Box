use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use chrono::SecondsFormat;
use tokio_rusqlite::rusqlite::{params, OptionalExtension, TransactionBehavior};
use tokio_rusqlite::Connection;

use super::{
    CompareAndSwapResult, RepositoryError, RepositoryResult, SandboxGeneration, SandboxId,
    SandboxListFilter, SandboxPage, SandboxRecord, SandboxRepository,
};

const LATEST_SCHEMA_VERSION: i64 = 1;
const INITIAL_MIGRATION_NAME: &str = "lifecycle_records";
const INITIAL_MIGRATION: &str = include_str!("../../../migrations/0001_lifecycle_records.sql");

#[derive(Clone)]
pub struct SqliteSandboxRepository {
    connection: Connection,
}

impl std::fmt::Debug for SqliteSandboxRepository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SqliteSandboxRepository")
            .finish_non_exhaustive()
    }
}

impl SqliteSandboxRepository {
    pub async fn open(path: impl AsRef<Path>) -> RepositoryResult<Self> {
        let connection = Connection::open(path)
            .await
            .map_err(|error| unavailable("open SQLite repository", error))?;
        let repository = Self { connection };
        repository.configure_and_migrate().await?;
        Ok(repository)
    }

    async fn configure_and_migrate(&self) -> RepositoryResult<()> {
        self.call(|connection| {
            connection
                .busy_timeout(Duration::from_secs(5))
                .map_err(|error| unavailable("configure SQLite busy timeout", error))?;
            connection
                .pragma_update(None, "foreign_keys", "ON")
                .map_err(|error| unavailable("enable SQLite foreign keys", error))?;
            connection
                .pragma_update(None, "synchronous", "NORMAL")
                .map_err(|error| unavailable("configure SQLite synchronization", error))?;
            let journal_mode: String = connection
                .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
                .map_err(|error| unavailable("enable SQLite WAL mode", error))?;
            if !journal_mode.eq_ignore_ascii_case("wal") {
                return Err(RepositoryError::Unavailable(format!(
                    "SQLite refused WAL mode and selected {journal_mode}"
                )));
            }
            connection
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS compatibility_schema_migrations (\
                        version INTEGER PRIMARY KEY NOT NULL,\
                        name TEXT NOT NULL,\
                        applied_at TEXT NOT NULL\
                    ) STRICT;",
                )
                .map_err(|error| unavailable("create SQLite migration table", error))?;

            let mut statement = connection
                .prepare(
                    "SELECT version, name FROM compatibility_schema_migrations ORDER BY version",
                )
                .map_err(|error| unavailable("prepare SQLite migration query", error))?;
            let applied = statement
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|error| unavailable("query SQLite migrations", error))?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|error| unavailable("read SQLite migrations", error))?;
            drop(statement);

            match applied.as_slice() {
                [] => apply_initial_migration(connection)?,
                [(1, name)] if name == INITIAL_MIGRATION_NAME => {}
                _ => {
                    return Err(RepositoryError::Corrupt(format!(
                        "unsupported SQLite migration history: {applied:?}"
                    )));
                }
            }
            Ok(())
        })
        .await
    }

    async fn call<F, R>(&self, function: F) -> RepositoryResult<R>
    where
        F: FnOnce(&mut tokio_rusqlite::rusqlite::Connection) -> RepositoryResult<R>
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

#[async_trait]
impl SandboxRepository for SqliteSandboxRepository {
    async fn insert(&self, record: SandboxRecord) -> RepositoryResult<()> {
        validate_record(&record)?;
        let sandbox_id = record.sandbox_id().clone();
        let record_json = serialize_record(&record)?;
        self.call(move |connection| {
            match connection.execute(
                "INSERT INTO sandbox_records(sandbox_id, record_json) VALUES (?1, ?2)",
                params![sandbox_id.as_str(), record_json],
            ) {
                Ok(_) => Ok(()),
                Err(error)
                    if error.sqlite_error_code().is_some_and(|code| {
                        code == tokio_rusqlite::rusqlite::ErrorCode::ConstraintViolation
                    }) =>
                {
                    let existing = connection
                        .query_row(
                            "SELECT 1 FROM sandbox_records WHERE sandbox_id = ?1",
                            [sandbox_id.as_str()],
                            |_| Ok(()),
                        )
                        .optional()
                        .map_err(|query_error| {
                            unavailable("inspect SQLite insert conflict", query_error)
                        })?;
                    if existing.is_some() {
                        Err(RepositoryError::Duplicate(sandbox_id))
                    } else {
                        Err(RepositoryError::Corrupt(format!(
                            "SQLite rejected lifecycle record: {error}"
                        )))
                    }
                }
                Err(error) => Err(unavailable("insert SQLite lifecycle record", error)),
            }
        })
        .await
    }

    async fn get(&self, sandbox_id: &SandboxId) -> RepositoryResult<Option<SandboxRecord>> {
        let sandbox_id = sandbox_id.clone();
        let record_json = self
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT record_json FROM sandbox_records WHERE sandbox_id = ?1",
                        [sandbox_id.as_str()],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(|error| unavailable("read SQLite lifecycle record", error))
            })
            .await?;
        record_json
            .map(|record| deserialize_record(&record))
            .transpose()
    }

    async fn list(&self, filter: &SandboxListFilter) -> RepositoryResult<SandboxPage> {
        let owner_id = filter.owner_id.clone();
        let after_created_at = filter.after.as_ref().map(|cursor| {
            cursor
                .created_at
                .to_rfc3339_opts(SecondsFormat::AutoSi, true)
        });
        let after_sandbox_id = filter
            .after
            .as_ref()
            .map(|cursor| cursor.sandbox_id.to_string());
        let records = self
            .call(move |connection| {
                let mut statement = connection
                    .prepare(
                        "SELECT record_json FROM sandbox_records \
                         WHERE owner_id = ?1 \
                           AND state IN ('running', 'paused') \
                           AND (\
                               ?2 IS NULL \
                               OR created_at > ?2 \
                               OR (created_at = ?2 AND sandbox_id > ?3)\
                           ) \
                         ORDER BY created_at, sandbox_id",
                    )
                    .map_err(|error| unavailable("prepare SQLite lifecycle list", error))?;
                let records = statement
                    .query_map(
                        params![owner_id, after_created_at, after_sandbox_id],
                        |row| row.get::<_, String>(0),
                    )
                    .map_err(|error| unavailable("query SQLite lifecycle list", error))?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|error| unavailable("read SQLite lifecycle list", error))?;
                Ok(records)
            })
            .await?;

        let mut matching = records
            .iter()
            .map(|record| deserialize_record(record))
            .collect::<RepositoryResult<Vec<_>>>()?;
        matching.retain(|record| {
            record.public_state().is_some_and(|state| {
                (filter.states.is_empty() || filter.states.contains(&state))
                    && filter
                        .metadata
                        .iter()
                        .all(|(key, value)| record.metadata().get(key) == Some(value))
            })
        });

        let limit = filter.limit.get() as usize;
        let has_more = matching.len() > limit;
        matching.truncate(limit);
        let next = if has_more {
            matching.last().map(|last| super::SandboxCursor {
                created_at: last.created_at(),
                sandbox_id: last.sandbox_id().clone(),
            })
        } else {
            None
        };
        Ok(SandboxPage {
            records: matching,
            next,
        })
    }

    async fn compare_and_swap(
        &self,
        sandbox_id: &SandboxId,
        expected: SandboxGeneration,
        replacement: SandboxRecord,
    ) -> RepositoryResult<CompareAndSwapResult> {
        if replacement.sandbox_id() != sandbox_id || replacement.generation() <= expected {
            return Err(RepositoryError::Corrupt(
                "invalid compare-and-swap replacement".to_string(),
            ));
        }
        validate_record(&replacement)?;
        let expected_generation = i64::try_from(expected.get()).map_err(|_| {
            RepositoryError::Corrupt("SQLite CAS generation exceeds signed 64-bit range".into())
        })?;
        let sandbox_id = sandbox_id.clone();
        let record_json = serialize_record(&replacement)?;
        self.call(move |connection| {
            let updated = connection
                .execute(
                    "UPDATE sandbox_records SET record_json = ?1 \
                     WHERE sandbox_id = ?2 AND generation = ?3",
                    params![record_json, sandbox_id.as_str(), expected_generation],
                )
                .map_err(|error| unavailable("update SQLite lifecycle record", error))?;
            if updated == 1 {
                return Ok(CompareAndSwapResult::Updated);
            }

            let actual = connection
                .query_row(
                    "SELECT generation FROM sandbox_records WHERE sandbox_id = ?1",
                    [sandbox_id.as_str()],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(|error| unavailable("inspect SQLite CAS conflict", error))?;
            match actual {
                None => Ok(CompareAndSwapResult::NotFound),
                Some(actual) => {
                    let actual = u64::try_from(actual).map_err(|_| {
                        RepositoryError::Corrupt(
                            "SQLite lifecycle generation is negative".to_string(),
                        )
                    })?;
                    Ok(CompareAndSwapResult::Conflict {
                        actual_generation: SandboxGeneration::new(actual).map_err(|error| {
                            RepositoryError::Corrupt(format!(
                                "invalid SQLite lifecycle generation: {error}"
                            ))
                        })?,
                    })
                }
            }
        })
        .await
    }
}

fn apply_initial_migration(
    connection: &mut tokio_rusqlite::rusqlite::Connection,
) -> RepositoryResult<()> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| unavailable("begin SQLite migration", error))?;
    transaction
        .execute_batch(INITIAL_MIGRATION)
        .map_err(|error| RepositoryError::Corrupt(format!("apply SQLite migration 1: {error}")))?;
    transaction
        .execute(
            "INSERT INTO compatibility_schema_migrations(version, name, applied_at) \
             VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
            params![LATEST_SCHEMA_VERSION, INITIAL_MIGRATION_NAME],
        )
        .map_err(|error| unavailable("record SQLite migration", error))?;
    transaction
        .commit()
        .map_err(|error| unavailable("commit SQLite migration", error))
}

fn serialize_record(record: &SandboxRecord) -> RepositoryResult<String> {
    serde_json::to_string(record).map_err(|error| {
        RepositoryError::Corrupt(format!("serialize lifecycle record for SQLite: {error}"))
    })
}

fn deserialize_record(record: &str) -> RepositoryResult<SandboxRecord> {
    let record: SandboxRecord = serde_json::from_str(record).map_err(|error| {
        RepositoryError::Corrupt(format!("deserialize SQLite lifecycle record: {error}"))
    })?;
    validate_record(&record)?;
    Ok(record)
}

fn validate_record(record: &SandboxRecord) -> RepositoryResult<()> {
    record.validate_persisted().map_err(|error| {
        RepositoryError::Corrupt(format!("invalid SQLite lifecycle record: {error}"))
    })?;
    i64::try_from(record.generation().get()).map_err(|_| {
        RepositoryError::Corrupt("SQLite lifecycle generation exceeds signed 64-bit range".into())
    })?;
    if record
        .execution_generation()
        .is_some_and(|generation| i64::try_from(generation.get()).is_err())
    {
        return Err(RepositoryError::Corrupt(
            "SQLite execution generation exceeds signed 64-bit range".into(),
        ));
    }
    Ok(())
}

fn unavailable(context: &str, error: impl std::fmt::Display) -> RepositoryError {
    RepositoryError::Unavailable(format!("{context}: {error}"))
}

fn map_async_error(error: tokio_rusqlite::Error<RepositoryError>) -> RepositoryError {
    match error {
        tokio_rusqlite::Error::Error(error) => error,
        tokio_rusqlite::Error::ConnectionClosed => {
            RepositoryError::Unavailable("SQLite repository connection closed".to_string())
        }
        _ => RepositoryError::Unavailable(format!("SQLite repository failed: {error}")),
    }
}

#[cfg(test)]
mod tests;
