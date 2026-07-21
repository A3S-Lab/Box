//! StateFile persistence layer.

use std::path::{Path, PathBuf};

use a3s_box_runtime::BoxStateStore;

use super::BoxRecord;
use crate::state::policy::{is_record_pid_live, should_restart};

/// Persistent state file backed by JSON.
pub struct StateFile {
    store: BoxStateStore,
}

/// In-memory result of a reconcile pass: which records changed, and which dead
/// boxes need host-resource teardown. The teardown + persistence run in
/// [`StateFile::flush_reconcile`] under the state lock — never from the bare
/// (possibly unlocked) read path.
struct ReconcileOutcome {
    changed: bool,
    stopped: Vec<BoxRecord>,
    removed: Vec<BoxRecord>,
    #[allow(dead_code)]
    restart_candidates: Vec<String>,
}

impl StateFile {
    /// Load state from disk. Creates an empty state if the file doesn't exist.
    pub fn load(path: &Path) -> Result<Self, std::io::Error> {
        let mut state = Self {
            store: BoxStateStore::load_or_quarantine(path)?,
        };
        // Reconcile in memory so the caller sees accurate live/dead status.
        let outcome = state.reconcile();
        // Persist the change + run teardown under the runtime-owned state
        // transaction. A fresh locked read prevents lost updates and duplicate
        // teardown when several readers observe the same dead execution.
        if outcome.changed {
            let _ = Self::flush_reconcile(path);
        }
        Ok(state)
    }

    /// Load from the default path (~/.a3s/boxes.json).
    pub fn load_default() -> Result<Self, std::io::Error> {
        let home = a3s_box_core::dirs_home();
        Self::load(&home.join("boxes.json"))
    }

    /// Load the default state **read-only**: no reconcile sweep, no PID-liveness
    /// cleanup, no write-back, and no quarantine of a corrupt file. For
    /// consumers that only need a snapshot of the records (e.g. metrics
    /// scraping) and must not cause side effects.
    ///
    /// A corrupt file is surfaced as an `Err` so the caller can distinguish it
    /// from an empty/absent file. Swallowing the parse error would make
    /// `/metrics` report a falsely healthy all-zero snapshot.
    pub(crate) fn load_readonly() -> Result<Self, std::io::Error> {
        let home = a3s_box_core::dirs_home();
        Self::load_readonly_from(home.join("boxes.json"))
    }

    /// Inner [`load_readonly`] over an explicit path (testable).
    pub(crate) fn load_readonly_from(path: PathBuf) -> Result<Self, std::io::Error> {
        Ok(Self {
            store: BoxStateStore::load_readonly(path)?,
        })
    }

    /// Save state to disk atomically under the cross-process state lock.
    pub fn save(&self) -> Result<(), std::io::Error> {
        self.store.save()
    }

    /// Atomically apply `f` to the on-disk state under the exclusive
    /// cross-process lock: load fresh → mutate → save, all while the lock is
    /// held. This is the race-free read-modify-write primitive — every writer
    /// should mutate through it (or, for async work, snapshot inputs before the
    /// await and call `modify` afterward to re-apply only its owned fields), so
    /// the monitor/compose/health/CLI cannot clobber each other.
    ///
    /// `f` MUST be synchronous and MUST NOT `.await` (holding an OS lock across
    /// a task yield would serialize or deadlock the async runtime).
    pub fn modify<R, E>(f: impl FnOnce(&mut StateFile) -> Result<R, E>) -> Result<R, E>
    where
        E: From<std::io::Error>,
    {
        let path = a3s_box_core::dirs_home().join("boxes.json");
        BoxStateStore::modify_or_quarantine(&path, |store| Self::with_runtime_store(store, f))
    }

    fn with_runtime_store<R, E>(
        store: &mut BoxStateStore,
        f: impl FnOnce(&mut StateFile) -> Result<R, E>,
    ) -> Result<R, E> {
        let placeholder = BoxStateStore::from_records(store.path().to_path_buf(), Vec::new());
        let mut state = Self {
            store: std::mem::replace(store, placeholder),
        };
        let output = f(&mut state);
        *store = state.store;
        output
    }

    /// Append a record atomically under the state lock (load fresh → push →
    /// save). Use this instead of `load_default()? + add()` so concurrent
    /// appends/removals cannot lose records. Loads WITHOUT the reconcile sweep —
    /// appending a box must not pay an O(N) PID-liveness/cleanup pass over every
    /// other box (the high-concurrency fork bottleneck).
    pub fn add_record(record: BoxRecord) -> Result<(), std::io::Error> {
        let path = a3s_box_core::dirs_home().join("boxes.json");
        BoxStateStore::modify_or_quarantine(&path, |store| {
            store.records_mut().push(record);
            Ok::<(), std::io::Error>(())
        })
    }

    /// Remove a record by id atomically under the state lock. Returns whether a
    /// record was removed.
    pub fn remove_record(id: &str) -> Result<bool, std::io::Error> {
        Self::modify(|sf| Ok::<bool, std::io::Error>(sf.store.remove_by_id(id)))
    }

    /// Add a record and persist.
    pub fn add(&mut self, record: BoxRecord) -> Result<(), std::io::Error> {
        self.store.records_mut().push(record);
        self.save()
    }

    /// Drop a record from this in-memory handle WITHOUT persisting.
    ///
    /// Used by callers that already removed the record from disk atomically via
    /// [`remove_record`](Self::remove_record); this keeps their in-memory view
    /// consistent without a second `save` that would clobber concurrent writers.
    pub(crate) fn forget(&mut self, id: &str) {
        self.store.remove_by_id(id);
    }

    /// Remove a record by ID and persist.
    pub fn remove(&mut self, id: &str) -> Result<bool, std::io::Error> {
        if self.store.remove_by_id(id) {
            self.save()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Find a record by exact ID.
    pub fn find_by_id(&self, id: &str) -> Option<&BoxRecord> {
        self.store.find_by_id(id)
    }

    /// Find a mutable record by exact ID.
    pub fn find_by_id_mut(&mut self, id: &str) -> Option<&mut BoxRecord> {
        self.store.find_by_id_mut(id)
    }

    /// Find a record by exact name.
    pub fn find_by_name(&self, name: &str) -> Option<&BoxRecord> {
        self.store.find_by_name(name)
    }

    /// Find records matching an ID prefix (must be unique).
    pub fn find_by_id_prefix(&self, prefix: &str) -> Vec<&BoxRecord> {
        self.store.find_by_id_prefix(prefix)
    }

    /// List records, optionally filtering to running-only.
    pub fn list(&self, all: bool) -> Vec<&BoxRecord> {
        self.store.list(all)
    }

    /// All records (for iteration).
    pub fn records(&self) -> &[BoxRecord] {
        self.store.records()
    }

    #[cfg(test)]
    pub(super) fn records_mut(&mut self) -> &mut Vec<BoxRecord> {
        self.store.records_mut()
    }

    /// Reconcile IN MEMORY: check PID liveness for active boxes, mark dead ones,
    /// capture their exit code, and drop auto-remove records. Returns the changed
    /// flag and the dead boxes needing teardown, but performs NO disk write and
    /// NO host-resource teardown itself. Both of those happen in
    /// [`Self::flush_reconcile`] under the cross-process lock, because this runs
    /// on every (often unlocked) `load()`; writing/tearing-down here was a
    /// lost-update race (clobbering a concurrent `run`/monitor write) and a
    /// double-teardown race (two reads cleaning up the same box).
    fn reconcile(&mut self) -> ReconcileOutcome {
        let mut changed = false;
        let mut restart_candidates = Vec::new();
        let mut auto_remove_records = Vec::new();
        let mut stopped_resource_records = Vec::new();

        for record in self.store.records_mut() {
            if !matches!(record.status.as_str(), "running" | "paused") {
                continue;
            }

            let has_live_pid = is_record_pid_live(record);
            if !has_live_pid {
                // guest-init writes the container exit code into the writable
                // rootfs (`/.a3s_exit_code`) on exit. Resolve the provider-specific
                // host path so overlay, copy fallback, and APFS-backed rootfses all
                // report the real code; liveness polling alone would yield exit 0.
                #[cfg(target_os = "windows")]
                {
                    let persisted =
                        a3s_box_runtime::rootfs::read_persisted_exit_code(&record.box_dir);
                    if record.box_dir.join("rootfs").is_dir() {
                        let fallback = record.exit_code.or(persisted).unwrap_or(0);
                        match a3s_box_runtime::vm::collect_windows_guest_result(
                            &record.box_dir,
                            &record.log_config,
                            fallback,
                        ) {
                            Ok(code) => record.exit_code = Some(code),
                            Err(error) => {
                                tracing::warn!(
                                    box_id = %record.id,
                                    %error,
                                    "Failed to collect completed Windows guest result"
                                );
                                record.exit_code = Some(if fallback == 0 { 1 } else { fallback });
                            }
                        }
                    } else if record.exit_code.is_none() {
                        record.exit_code = persisted;
                    }
                }
                #[cfg(not(target_os = "windows"))]
                if record.exit_code.is_none() {
                    record.exit_code =
                        a3s_box_runtime::rootfs::read_persisted_exit_code(&record.box_dir);
                }
                record.status = "dead".to_string();
                record.pid = None;
                record.health_status = "none".to_string();
                record.health_retries = 0;
                changed = true;

                if record.auto_remove {
                    auto_remove_records.push(record.clone());
                    continue;
                }

                stopped_resource_records.push(record.clone());

                if should_restart(record) {
                    restart_candidates.push(record.id.clone());
                }
            }
        }

        if !auto_remove_records.is_empty() {
            self.store
                .records_mut()
                .retain(|record| !auto_remove_records.iter().any(|r| r.id == record.id));
            changed = true;
        }

        ReconcileOutcome {
            changed,
            stopped: stopped_resource_records,
            removed: auto_remove_records,
            restart_candidates,
        }
    }

    /// Persist the reconcile sweep (mark-dead, exit-code capture, auto-remove)
    /// AND run the dead boxes' host-resource teardown for `path` — all under the
    /// cross-process state lock. Re-loads fresh under the lock and re-reconciles,
    /// so a concurrent writer is never clobbered and two readers cannot tear down
    /// the same box twice (the second's fresh re-load sees the box already gone).
    fn flush_reconcile(path: &Path) -> std::io::Result<()> {
        BoxStateStore::modify_or_quarantine(path, |store| {
            Self::with_runtime_store(store, |state| {
                let outcome = state.reconcile();
                if outcome.changed {
                    for record in &outcome.stopped {
                        crate::cleanup::cleanup_stopped_box(record)
                            .map_err(|error| std::io::Error::other(error.to_string()))?;
                    }
                    for record in &outcome.removed {
                        crate::cleanup::cleanup_removed_box(record)
                            .map_err(|error| std::io::Error::other(error.to_string()))?;
                    }
                }
                Ok::<(), std::io::Error>(())
            })
        })
    }

    /// Get box IDs that are pending restart (dead boxes with active restart policy).
    ///
    /// This can be called after load to check if any boxes need restarting.
    pub fn pending_restarts(&self) -> Vec<String> {
        self.store
            .records()
            .iter()
            .filter(|r| r.status == "dead" && should_restart(r))
            .map(|r| r.id.clone())
            .collect()
    }

    /// Find all records matching a label key-value pair.
    pub fn find_by_label(&self, key: &str, value: &str) -> Vec<&BoxRecord> {
        self.store
            .records()
            .iter()
            .filter(|r| r.labels.get(key).is_some_and(|v| v == value))
            .collect()
    }
}
