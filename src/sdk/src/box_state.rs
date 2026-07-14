//! SDK-local reader/writer for the shared `boxes.json` state format.
//!
//! The state file is still the source of truth for container metadata, but the
//! SDK must not depend on the CLI crate just to read it. Keep this module
//! format-compatible with the CLI state schema.

use std::path::{Path, PathBuf};

pub(crate) use a3s_box_runtime::BoxRecord;

/// Persistent state file backed by JSON.
pub(crate) struct StateFile {
    path: PathBuf,
    records: Vec<BoxRecord>,
}

impl StateFile {
    /// Load state from disk. Creates an empty state if the file does not exist.
    pub(crate) fn load(path: &Path) -> std::io::Result<Self> {
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            return Ok(Self {
                path: path.to_path_buf(),
                records: Vec::new(),
            });
        }

        let data = std::fs::read_to_string(path)?;
        let records = serde_json::from_str::<Vec<BoxRecord>>(&data)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;

        Ok(Self {
            path: path.to_path_buf(),
            records,
        })
    }

    /// Save state atomically under the same advisory lock used by the CLI.
    pub(crate) fn save(&self) -> std::io::Result<()> {
        let _lock = StateLock::acquire(&self.path)?;
        self.write_to_disk()
    }

    /// Atomically apply a synchronous mutation to state under the state lock.
    pub(crate) fn modify<R>(
        path: &Path,
        f: impl FnOnce(&mut StateFile) -> std::io::Result<R>,
    ) -> std::io::Result<R> {
        let _lock = StateLock::acquire(path)?;
        let mut state = Self::load_unlocked(path)?;
        let output = f(&mut state)?;
        state.write_to_disk()?;
        Ok(output)
    }

    fn load_unlocked(path: &Path) -> std::io::Result<Self> {
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            return Ok(Self {
                path: path.to_path_buf(),
                records: Vec::new(),
            });
        }

        let data = std::fs::read_to_string(path)?;
        let records = serde_json::from_str::<Vec<BoxRecord>>(&data)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;

        Ok(Self {
            path: path.to_path_buf(),
            records,
        })
    }

    fn write_to_disk(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let data = serde_json::to_vec_pretty(&self.records).map_err(std::io::Error::other)?;
        let tmp_path = self.path.with_extension("json.tmp");
        a3s_box_core::fs_atomic::write_durable(&tmp_path, &self.path, &data)
    }

    pub(crate) fn find_by_id(&self, id: &str) -> Option<&BoxRecord> {
        self.records.iter().find(|record| record.id == id)
    }

    pub(crate) fn find_by_id_mut(&mut self, id: &str) -> Option<&mut BoxRecord> {
        self.records.iter_mut().find(|record| record.id == id)
    }

    pub(crate) fn remove_by_id(&mut self, id: &str) -> bool {
        let before = self.records.len();
        self.records.retain(|record| record.id != id);
        self.records.len() < before
    }

    pub(crate) fn find_by_name(&self, name: &str) -> Option<&BoxRecord> {
        self.records.iter().find(|record| record.name == name)
    }

    pub(crate) fn records_mut(&mut self) -> &mut Vec<BoxRecord> {
        &mut self.records
    }

    pub(crate) fn find_by_id_prefix(&self, prefix: &str) -> Vec<&BoxRecord> {
        self.records
            .iter()
            .filter(|record| record.id.starts_with(prefix) || record.short_id.starts_with(prefix))
            .collect()
    }

    pub(crate) fn list(&self, all: bool) -> Vec<&BoxRecord> {
        self.records
            .iter()
            .filter(|record| all || record.status == "running")
            .collect()
    }
}

struct StateLock {
    #[cfg(unix)]
    _file: std::fs::File,
}

impl StateLock {
    #[cfg(unix)]
    fn acquire(state_path: &Path) -> std::io::Result<Self> {
        use std::os::unix::io::AsRawFd;

        let lock_path = state_path
            .parent()
            .map(|parent| parent.join("boxes.json.lock"))
            .unwrap_or_else(|| PathBuf::from("boxes.json.lock"));
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;

        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }

        Ok(Self { _file: file })
    }

    #[cfg(not(unix))]
    fn acquire(_state_path: &Path) -> std::io::Result<Self> {
        Ok(Self {})
    }
}
