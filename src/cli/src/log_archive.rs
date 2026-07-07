//! Archived logs for auto-removed boxes.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::state::BoxRecord;

const ARCHIVE_DIR: &str = "removed-logs";
const METADATA_FILE: &str = "metadata.json";
const DEFAULT_MAX_ARCHIVE_AGE_DAYS: i64 = 7;
const DEFAULT_MAX_ARCHIVES: usize = 50;
const DEFAULT_MAX_ARCHIVE_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub(crate) struct LogArchiveRetention {
    max_age_days: i64,
    max_archives: usize,
    max_total_bytes: u64,
}

impl Default for LogArchiveRetention {
    fn default() -> Self {
        Self {
            max_age_days: DEFAULT_MAX_ARCHIVE_AGE_DAYS,
            max_archives: DEFAULT_MAX_ARCHIVES,
            max_total_bytes: DEFAULT_MAX_ARCHIVE_BYTES,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RemovedLogArchive {
    pub id: String,
    pub short_id: String,
    pub name: String,
    pub image: String,
    pub removed_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub exit_code: Option<i32>,
    pub log_config: a3s_box_core::log::LogConfig,
}

impl RemovedLogArchive {
    pub(crate) fn log_dir(&self) -> PathBuf {
        archive_dir(&self.id).join("logs")
    }

    pub(crate) fn console_log(&self) -> PathBuf {
        self.log_dir().join("console.log")
    }
}

pub(crate) fn archive_removed_logs(record: &BoxRecord) -> std::io::Result<Option<PathBuf>> {
    if record.log_config.driver == a3s_box_core::log::LogDriver::None {
        return Ok(None);
    }

    let source_log_dir = record.box_dir.join("logs");
    if !source_log_dir.exists() && !record.console_log.exists() {
        return Ok(None);
    }

    let archive_dir = archive_dir(&record.id);
    if archive_dir.exists() {
        std::fs::remove_dir_all(&archive_dir)?;
    }
    std::fs::create_dir_all(archive_dir.join("logs"))?;

    if source_log_dir.exists() {
        copy_dir_contents(&source_log_dir, &archive_dir.join("logs"))?;
    } else if record.console_log.exists() {
        std::fs::copy(
            &record.console_log,
            archive_dir.join("logs").join("console.log"),
        )?;
    }

    let metadata = RemovedLogArchive {
        id: record.id.clone(),
        short_id: record.short_id.clone(),
        name: record.name.clone(),
        image: record.image.clone(),
        removed_at: Utc::now(),
        created_at: record.created_at,
        started_at: record.started_at,
        exit_code: record.exit_code,
        log_config: record.log_config.clone(),
    };
    let data = serde_json::to_vec_pretty(&metadata).map_err(std::io::Error::other)?;
    std::fs::write(archive_dir.join(METADATA_FILE), data)?;

    if let Err(error) = prune_archives(LogArchiveRetention::default()) {
        tracing::debug!(
            error = %error,
            "Failed to prune removed-log archives after archiving logs"
        );
    }

    Ok(Some(archive_dir))
}

pub(crate) fn resolve_archive(query: &str) -> Result<Option<RemovedLogArchive>, String> {
    let archives = load_archives().map_err(|e| format!("Failed to read removed logs: {e}"))?;

    if let Some(archive) = archives.iter().find(|archive| archive.id == query) {
        return Ok(Some(archive.clone()));
    }
    if let Some(archive) = archives.iter().find(|archive| archive.short_id == query) {
        return Ok(Some(archive.clone()));
    }

    let mut named: Vec<_> = archives
        .iter()
        .filter(|archive| archive.name == query)
        .cloned()
        .collect();
    if !named.is_empty() {
        named.sort_by_key(|archive| archive.removed_at);
        return Ok(named.pop());
    }

    let prefix_matches: Vec<_> = archives
        .into_iter()
        .filter(|archive| archive.id.starts_with(query) || archive.short_id.starts_with(query))
        .collect();
    match prefix_matches.len() {
        0 => Ok(None),
        1 => Ok(prefix_matches.into_iter().next()),
        count => Err(format!(
            "Ambiguous removed-log reference \"{query}\" - matches {count} archives"
        )),
    }
}

fn load_archives() -> std::io::Result<Vec<RemovedLogArchive>> {
    Ok(load_archive_entries()?
        .into_iter()
        .map(|entry| entry.archive)
        .collect())
}

struct ArchiveEntry {
    archive: RemovedLogArchive,
    path: PathBuf,
    size_bytes: u64,
}

fn load_archive_entries() -> std::io::Result<Vec<ArchiveEntry>> {
    let root = archive_root();
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut archives = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path().join(METADATA_FILE);
        if !path.exists() {
            continue;
        }
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        if let Ok(archive) = serde_json::from_slice::<RemovedLogArchive>(&data) {
            let path = entry.path();
            let size_bytes = dir_size(&path)?;
            archives.push(ArchiveEntry {
                archive,
                path,
                size_bytes,
            });
        }
    }
    Ok(archives)
}

pub(crate) fn prune_archives(retention: LogArchiveRetention) -> std::io::Result<usize> {
    let mut entries = load_archive_entries()?;
    let now = Utc::now();
    let mut removed = 0;

    let mut kept_entries = Vec::with_capacity(entries.len());
    for entry in entries {
        let too_old = now
            .signed_duration_since(entry.archive.removed_at)
            .num_days()
            > retention.max_age_days;
        if too_old {
            match remove_archive_dir(&entry.path) {
                Ok(()) => {
                    removed += 1;
                    continue;
                }
                Err(_) => kept_entries.push(entry),
            }
        } else {
            kept_entries.push(entry);
        }
    }
    entries = kept_entries;

    entries.sort_by_key(|entry| entry.archive.removed_at);
    while entries.len() > retention.max_archives {
        let entry = entries.remove(0);
        if remove_archive_dir(&entry.path).is_ok() {
            removed += 1;
        }
    }

    let mut total_bytes = entries.iter().map(|entry| entry.size_bytes).sum::<u64>();
    while total_bytes > retention.max_total_bytes && !entries.is_empty() {
        let entry = entries.remove(0);
        total_bytes = total_bytes.saturating_sub(entry.size_bytes);
        if remove_archive_dir(&entry.path).is_ok() {
            removed += 1;
        }
    }

    Ok(removed)
}

fn remove_archive_dir(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        std::fs::remove_dir_all(path)?;
    }
    Ok(())
}

fn dir_size(path: &Path) -> std::io::Result<u64> {
    let mut size = 0;
    if !path.exists() {
        return Ok(size);
    }
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            size += dir_size(&entry.path())?;
        } else if metadata.is_file() {
            size += metadata.len();
        }
    }
    Ok(size)
}

fn copy_dir_contents(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_contents(&from, &to)?;
        } else if file_type.is_file() {
            std::fs::copy(from, to)?;
        }
    }
    Ok(())
}

fn archive_root() -> PathBuf {
    a3s_box_core::dirs_home().join(ARCHIVE_DIR)
}

fn archive_dir(id: &str) -> PathBuf {
    archive_root().join(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &Path) -> Self {
            static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
            let lock = LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self {
                _lock: lock,
                key,
                previous,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn archives_and_resolves_auto_removed_logs_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("A3S_HOME", tmp.path());
        let box_dir = tmp.path().join("box");
        std::fs::create_dir_all(box_dir.join("logs")).unwrap();
        std::fs::write(box_dir.join("logs").join("container.json"), "{}\n").unwrap();

        let mut record = crate::test_helpers::fixtures::make_record(
            "550e8400-e29b-41d4-a716-446655440000",
            "web",
            "dead",
            None,
        );
        record.auto_remove = true;
        record.box_dir = box_dir;
        record.console_log = record.box_dir.join("logs").join("console.log");

        let archive_path = archive_removed_logs(&record).unwrap().unwrap();
        assert!(archive_path.join("logs").join("container.json").exists());

        let archive = resolve_archive("web").unwrap().unwrap();
        assert_eq!(archive.id, record.id);
        assert!(archive.log_dir().join("container.json").exists());
    }

    #[test]
    fn prunes_archives_by_age_and_count() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("A3S_HOME", tmp.path());

        write_archive("old", Utc::now() - chrono::Duration::days(30), 8);
        write_archive("keep-1", Utc::now() - chrono::Duration::days(3), 8);
        write_archive("keep-2", Utc::now() - chrono::Duration::days(2), 8);
        write_archive("keep-3", Utc::now() - chrono::Duration::days(1), 8);

        let removed = prune_archives(LogArchiveRetention {
            max_age_days: 7,
            max_archives: 2,
            max_total_bytes: u64::MAX,
        })
        .unwrap();

        assert_eq!(removed, 2);
        assert!(resolve_archive("old").unwrap().is_none());
        assert!(resolve_archive("keep-1").unwrap().is_none());
        assert!(resolve_archive("keep-2").unwrap().is_some());
        assert!(resolve_archive("keep-3").unwrap().is_some());
    }

    #[test]
    fn prunes_archives_by_total_size() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("A3S_HOME", tmp.path());

        write_archive("large-1", Utc::now() - chrono::Duration::days(3), 128);
        write_archive("large-2", Utc::now() - chrono::Duration::days(2), 128);
        write_archive("large-3", Utc::now() - chrono::Duration::days(1), 128);

        let before = dir_size(&archive_root()).unwrap();
        let removed = prune_archives(LogArchiveRetention {
            max_age_days: 7,
            max_archives: 10,
            max_total_bytes: before.saturating_sub(1),
        })
        .unwrap();

        assert_eq!(removed, 1);
        assert!(resolve_archive("large-1").unwrap().is_none());
        assert!(resolve_archive("large-2").unwrap().is_some());
        assert!(resolve_archive("large-3").unwrap().is_some());
    }

    fn write_archive(id: &str, removed_at: DateTime<Utc>, payload_bytes: usize) {
        let dir = archive_dir(id);
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        std::fs::write(
            dir.join("logs").join("console.log"),
            vec![b'x'; payload_bytes],
        )
        .unwrap();
        let metadata = RemovedLogArchive {
            id: id.to_string(),
            short_id: id.to_string(),
            name: id.to_string(),
            image: "alpine:latest".to_string(),
            removed_at,
            created_at: removed_at,
            started_at: Some(removed_at),
            exit_code: Some(1),
            log_config: a3s_box_core::log::LogConfig::default(),
        };
        std::fs::write(
            dir.join(METADATA_FILE),
            serde_json::to_vec_pretty(&metadata).unwrap(),
        )
        .unwrap();
    }
}
