//! Durable atomic file writes for persisted state.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Atomically and **durably** write `bytes` to `final_path`: write to the
/// caller-chosen `tmp_path` sibling, `fsync` it, rename over `final_path`, then
/// best-effort `fsync` the parent directory.
///
/// Plain `write` + `rename` gives atomicity against a torn read but NOT crash
/// durability: on power loss the rename's directory entry can be journaled while
/// the temp file's data blocks are still buffered (delayed allocation), leaving
/// `final_path` present but zero-length/truncated. That truncated file then
/// fails to parse and gets quarantined — orphaning everything it tracked, the
/// exact outcome the quarantine logic exists to prevent. `fsync`-before-rename
/// closes that window so a hard crash cannot corrupt the persisted state.
///
/// The caller supplies `tmp_path` so it can pick a collision-free name (e.g. a
/// per-process/per-call unique suffix) when concurrent writers are possible.
pub fn write_durable(tmp_path: &Path, final_path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    {
        let mut f = std::fs::File::create(tmp_path)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(tmp_path, final_path)?;
    // Best-effort parent-dir fsync so the rename itself is durable. Not every
    // filesystem requires or permits a directory fsync, so failures are ignored.
    if let Some(dir) = final_path.parent() {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// Per-process sequence used to keep quarantine names unique even when more
/// than one store is quarantined during the same clock tick.
static QUARANTINE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Build a collision-resistant sibling name for a corrupt JSON store.
///
/// Seconds-only names allowed a second corruption in the same second to
/// overwrite the first recovery copy.  Include nanoseconds, the process id,
/// and a process-local sequence so independent stores and concurrent callers
/// retain every copy.  The `json.corrupt-` prefix is intentionally stable for
/// operators and existing cleanup tooling.
fn quarantine_candidate(path: &Path) -> Option<PathBuf> {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    for _ in 0..128 {
        let sequence = QUARANTINE_SEQ.fetch_add(1, Ordering::Relaxed);
        let suffix = format!(
            "json.corrupt-{}-{}-{}-{}",
            elapsed.as_secs(),
            elapsed.subsec_nanos(),
            std::process::id(),
            sequence
        );
        let candidate = path.with_extension(suffix);

        // The generated tuple is practically unique across processes.  Still
        // skip an existing entry so a clock/sequence collision can never
        // silently overwrite an earlier quarantine copy.
        match std::fs::symlink_metadata(&candidate) {
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(candidate),
            Err(_) => return None,
        }
    }
    None
}

/// Copy a regular file to a fresh quarantine sibling without replacing an
/// existing path.  The source remains untouched.
pub fn quarantine_copy(path: &Path) -> Option<PathBuf> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    let backup = quarantine_candidate(path)?;
    let mut source = std::fs::File::open(path).ok()?;
    let mut destination = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&backup)
    {
        Ok(file) => file,
        Err(_) => return None,
    };
    if std::io::copy(&mut source, &mut destination).is_err() || destination.sync_all().is_err() {
        let _ = std::fs::remove_file(&backup);
        return None;
    }
    Some(backup)
}

/// Move a corrupt regular store file aside to a fresh quarantine sibling.
///
/// If a hard-link move is not possible (for example across filesystems), a
/// durable copy is retained instead and the source is left in place.  In
/// either case an existing quarantine file is never replaced.
pub fn quarantine_corrupt(path: &Path) -> Option<PathBuf> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    let backup = quarantine_candidate(path)?;
    // A hard link is an atomic, no-replace move primitive on filesystems that
    // support it: unlike Unix `rename`, it cannot overwrite a quarantine file
    // if another process wins the candidate race.  Remove the source only
    // after the recovery link exists.
    if std::fs::hard_link(path, &backup).is_ok() {
        let _ = std::fs::remove_file(path);
        return Some(backup);
    }
    // `quarantine_copy` generates a new candidate, because the failed rename
    // may have raced with another writer that claimed the first one.
    quarantine_copy(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_durable_round_trips_and_replaces_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let tmp = dir.path().join("state.json.tmp");

        write_durable(&tmp, &path, b"hello").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert!(
            !tmp.exists(),
            "temp file must be renamed away, not left behind"
        );

        // A subsequent write replaces the contents atomically.
        write_durable(&tmp, &path, b"world!!").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"world!!");
        assert!(!tmp.exists());
    }

    #[test]
    fn quarantine_copy_keeps_multiple_same_tick_backups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, b"corrupt state").unwrap();

        let first = quarantine_copy(&path).expect("first quarantine copy should succeed");
        let second = quarantine_copy(&path).expect("second quarantine copy should succeed");

        assert_ne!(first, second, "quarantine names must never collide");
        assert_eq!(std::fs::read(first).unwrap(), b"corrupt state");
        assert_eq!(std::fs::read(second).unwrap(), b"corrupt state");
        assert!(path.exists(), "copy quarantine must preserve the source");
    }

    #[test]
    fn quarantine_corrupt_moves_only_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, b"corrupt").unwrap();

        let backup = quarantine_corrupt(&path).expect("regular file should be quarantined");
        assert!(!path.exists());
        assert_eq!(std::fs::read(backup).unwrap(), b"corrupt");

        let directory = dir.path().join("directory.json");
        std::fs::create_dir(&directory).unwrap();
        assert!(quarantine_corrupt(&directory).is_none());
    }
}
