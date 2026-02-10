//! State management for box instances.
//!
//! Persists box metadata to `~/.a3s/boxes.json` with atomic writes.
//! On every load, dead PIDs are reconciled to mark boxes as dead.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Metadata record for a single box instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoxRecord {
    /// Full UUID
    pub id: String,
    /// First 12 hex chars of the UUID (no dashes)
    pub short_id: String,
    /// User-assigned or auto-generated name
    pub name: String,
    /// OCI image reference
    pub image: String,
    /// "created" | "running" | "stopped" | "dead"
    pub status: String,
    /// Shim process PID (set when running)
    pub pid: Option<u32>,
    /// Number of vCPUs
    pub cpus: u32,
    /// Memory in MB
    pub memory_mb: u32,
    /// Volume mounts ("host:guest" pairs)
    pub volumes: Vec<String>,
    /// Environment variables
    pub env: HashMap<String, String>,
    /// Entrypoint override
    pub cmd: Vec<String>,
    /// Entrypoint override (if set via --entrypoint)
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    /// Box working directory (~/.a3s/boxes/<id>/)
    pub box_dir: PathBuf,
    /// Path to gRPC socket
    pub socket_path: PathBuf,
    /// Path to exec socket
    #[serde(default)]
    pub exec_socket_path: PathBuf,
    /// Path to console log
    pub console_log: PathBuf,
    /// Creation timestamp
    pub created_at: DateTime<Utc>,
    /// Start timestamp
    pub started_at: Option<DateTime<Utc>>,
    /// Whether to auto-remove on stop
    pub auto_remove: bool,
}

impl BoxRecord {
    /// Generate a short ID from a full UUID (first 12 hex characters, no dashes).
    pub fn make_short_id(id: &str) -> String {
        id.replace('-', "").chars().take(12).collect()
    }
}

/// Persistent state file backed by JSON.
pub struct StateFile {
    path: PathBuf,
    records: Vec<BoxRecord>,
}

impl StateFile {
    /// Load state from disk. Creates an empty state if the file doesn't exist.
    pub fn load(path: &Path) -> Result<Self, std::io::Error> {
        if path.exists() {
            let data = std::fs::read_to_string(path)?;
            let records: Vec<BoxRecord> =
                serde_json::from_str(&data).unwrap_or_default();
            let mut sf = Self {
                path: path.to_path_buf(),
                records,
            };
            sf.reconcile();
            Ok(sf)
        } else {
            // Ensure parent directory exists
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            Ok(Self {
                path: path.to_path_buf(),
                records: Vec::new(),
            })
        }
    }

    /// Load from the default path (~/.a3s/boxes.json).
    pub fn load_default() -> Result<Self, std::io::Error> {
        let home = dirs::home_dir()
            .map(|h| h.join(".a3s"))
            .unwrap_or_else(|| PathBuf::from(".a3s"));
        Self::load(&home.join("boxes.json"))
    }

    /// Save state to disk atomically (write to .tmp, then rename).
    pub fn save(&self) -> Result<(), std::io::Error> {
        let data = serde_json::to_string_pretty(&self.records)
            .map_err(std::io::Error::other)?;
        let tmp_path = self.path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &data)?;
        std::fs::rename(&tmp_path, &self.path)?;
        Ok(())
    }

    /// Add a record and persist.
    pub fn add(&mut self, record: BoxRecord) -> Result<(), std::io::Error> {
        self.records.push(record);
        self.save()
    }

    /// Remove a record by ID and persist.
    pub fn remove(&mut self, id: &str) -> Result<bool, std::io::Error> {
        let len_before = self.records.len();
        self.records.retain(|r| r.id != id);
        if self.records.len() < len_before {
            self.save()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Find a record by exact ID.
    pub fn find_by_id(&self, id: &str) -> Option<&BoxRecord> {
        self.records.iter().find(|r| r.id == id)
    }

    /// Find a mutable record by exact ID.
    pub fn find_by_id_mut(&mut self, id: &str) -> Option<&mut BoxRecord> {
        self.records.iter_mut().find(|r| r.id == id)
    }

    /// Find a record by exact name.
    pub fn find_by_name(&self, name: &str) -> Option<&BoxRecord> {
        self.records.iter().find(|r| r.name == name)
    }

    /// Find records matching an ID prefix (must be unique).
    pub fn find_by_id_prefix(&self, prefix: &str) -> Vec<&BoxRecord> {
        self.records
            .iter()
            .filter(|r| r.id.starts_with(prefix) || r.short_id.starts_with(prefix))
            .collect()
    }

    /// List records, optionally filtering to running-only.
    pub fn list(&self, all: bool) -> Vec<&BoxRecord> {
        if all {
            self.records.iter().collect()
        } else {
            self.records.iter().filter(|r| r.status == "running").collect()
        }
    }

    /// All records (for iteration).
    pub fn records(&self) -> &[BoxRecord] {
        &self.records
    }

    /// Reconcile: check PID liveness for running boxes, mark dead ones.
    fn reconcile(&mut self) {
        let mut changed = false;
        for record in &mut self.records {
            if record.status == "running" {
                if let Some(pid) = record.pid {
                    if !is_process_alive(pid) {
                        record.status = "dead".to_string();
                        record.pid = None;
                        changed = true;
                    }
                } else {
                    // Running but no PID — mark as dead
                    record.status = "dead".to_string();
                    changed = true;
                }
            }
        }
        if changed {
            let _ = self.save();
        }
    }
}

/// Check if a process is alive by sending signal 0.
fn is_process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Adjectives for random name generation.
const ADJECTIVES: &[&str] = &[
    "bold", "calm", "cool", "dark", "fast", "glad", "keen", "kind",
    "loud", "mild", "neat", "pale", "pure", "rare", "safe", "slim",
    "soft", "tall", "tiny", "vast", "warm", "wise", "zen", "agile",
    "brave", "eager", "happy", "lucid", "noble", "quick", "sharp",
    "vivid",
];

/// Nouns (notable computer scientists) for random name generation.
const NOUNS: &[&str] = &[
    "turing", "hopper", "lovelace", "dijkstra", "knuth", "ritchie",
    "thompson", "torvalds", "wozniak", "cerf", "berners", "mccarthy",
    "backus", "kay", "lamport", "hoare", "church", "neumann", "shannon",
    "boole", "babbage", "hamilton", "liskov", "wing", "rivest", "shamir",
    "diffie", "hellman", "stallman", "pike", "kernighan", "stroustrup",
];

/// Generate a random Docker-style name (adjective_noun).
pub fn generate_name() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let adj = ADJECTIVES[rng.gen_range(0..ADJECTIVES.len())];
    let noun = NOUNS[rng.gen_range(0..NOUNS.len())];
    format!("{adj}_{noun}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_state_path(tmp: &TempDir) -> PathBuf {
        tmp.path().join("boxes.json")
    }

    fn sample_record(id: &str, name: &str, status: &str) -> BoxRecord {
        let short_id = BoxRecord::make_short_id(id);
        BoxRecord {
            id: id.to_string(),
            short_id,
            name: name.to_string(),
            image: "alpine:latest".to_string(),
            status: status.to_string(),
            pid: if status == "running" { Some(99999) } else { None },
            cpus: 2,
            memory_mb: 512,
            volumes: vec![],
            env: HashMap::new(),
            cmd: vec![],
            entrypoint: None,
            box_dir: PathBuf::from("/tmp/boxes").join(id),
            socket_path: PathBuf::from("/tmp/boxes").join(id).join("grpc.sock"),
            exec_socket_path: PathBuf::from("/tmp/boxes").join(id).join("sockets").join("exec.sock"),
            console_log: PathBuf::from("/tmp/boxes").join(id).join("console.log"),
            created_at: Utc::now(),
            started_at: if status == "running" { Some(Utc::now()) } else { None },
            auto_remove: false,
        }
    }

    // --- BoxRecord tests ---

    #[test]
    fn test_make_short_id() {
        let id = "550e8400-e29b-41d4-a716-446655440000";
        assert_eq!(BoxRecord::make_short_id(id), "550e8400e29b");
    }

    #[test]
    fn test_make_short_id_no_dashes() {
        let id = "abcdef1234567890";
        assert_eq!(BoxRecord::make_short_id(id), "abcdef123456");
    }

    #[test]
    fn test_make_short_id_short_input() {
        let id = "abc";
        assert_eq!(BoxRecord::make_short_id(id), "abc");
    }

    #[test]
    fn test_make_short_id_empty() {
        assert_eq!(BoxRecord::make_short_id(""), "");
    }

    #[test]
    fn test_box_record_serialization() {
        let record = sample_record("test-id-123", "my_box", "created");
        let json = serde_json::to_string(&record).unwrap();
        let parsed: BoxRecord = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.id, "test-id-123");
        assert_eq!(parsed.name, "my_box");
        assert_eq!(parsed.status, "created");
        assert_eq!(parsed.image, "alpine:latest");
        assert_eq!(parsed.cpus, 2);
        assert_eq!(parsed.memory_mb, 512);
        assert!(parsed.pid.is_none());
    }

    #[test]
    fn test_box_record_serialization_with_env() {
        let mut record = sample_record("env-id", "env_box", "created");
        record.env.insert("FOO".to_string(), "bar".to_string());
        record.env.insert("BAZ".to_string(), "qux".to_string());
        record.volumes = vec!["/host:/guest".to_string()];
        record.cmd = vec!["sh".to_string(), "-c".to_string(), "echo hi".to_string()];

        let json = serde_json::to_string(&record).unwrap();
        let parsed: BoxRecord = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.env.get("FOO").unwrap(), "bar");
        assert_eq!(parsed.env.get("BAZ").unwrap(), "qux");
        assert_eq!(parsed.volumes, vec!["/host:/guest"]);
        assert_eq!(parsed.cmd, vec!["sh", "-c", "echo hi"]);
    }

    #[test]
    fn test_box_record_serialization_running() {
        let record = sample_record("run-id", "runner", "running");
        let json = serde_json::to_string(&record).unwrap();
        let parsed: BoxRecord = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.status, "running");
        assert_eq!(parsed.pid, Some(99999));
        assert!(parsed.started_at.is_some());
    }

    // --- StateFile basic tests ---

    #[test]
    fn test_load_empty() {
        let tmp = TempDir::new().unwrap();
        let sf = StateFile::load(&test_state_path(&tmp)).unwrap();
        assert!(sf.records().is_empty());
    }

    #[test]
    fn test_load_creates_parent_dir() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nested").join("dir").join("boxes.json");
        let sf = StateFile::load(&path).unwrap();
        assert!(sf.records().is_empty());
        assert!(path.parent().unwrap().exists());
    }

    #[test]
    fn test_load_corrupt_json_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let path = test_state_path(&tmp);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not valid json!!!").unwrap();

        let sf = StateFile::load(&path).unwrap();
        assert!(sf.records().is_empty());
    }

    #[test]
    fn test_add_and_find() {
        let tmp = TempDir::new().unwrap();
        let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();

        let record = sample_record("abc-def-123", "test_box", "created");
        sf.add(record).unwrap();

        assert_eq!(sf.records().len(), 1);
        assert!(sf.find_by_id("abc-def-123").is_some());
        assert!(sf.find_by_name("test_box").is_some());
    }

    #[test]
    fn test_add_multiple() {
        let tmp = TempDir::new().unwrap();
        let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();

        sf.add(sample_record("id1", "box1", "created")).unwrap();
        sf.add(sample_record("id2", "box2", "stopped")).unwrap();
        sf.add(sample_record("id3", "box3", "dead")).unwrap();

        assert_eq!(sf.records().len(), 3);
        assert!(sf.find_by_id("id1").is_some());
        assert!(sf.find_by_id("id2").is_some());
        assert!(sf.find_by_id("id3").is_some());
    }

    #[test]
    fn test_find_by_name_not_found() {
        let tmp = TempDir::new().unwrap();
        let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();
        sf.add(sample_record("id1", "box1", "created")).unwrap();

        assert!(sf.find_by_name("nonexistent").is_none());
    }

    #[test]
    fn test_find_by_id_not_found() {
        let tmp = TempDir::new().unwrap();
        let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();
        sf.add(sample_record("id1", "box1", "created")).unwrap();

        assert!(sf.find_by_id("wrong-id").is_none());
    }

    // --- Remove tests ---

    #[test]
    fn test_remove() {
        let tmp = TempDir::new().unwrap();
        let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();

        let record = sample_record("abc-def-123", "test_box", "created");
        sf.add(record).unwrap();

        assert!(sf.remove("abc-def-123").unwrap());
        assert!(sf.records().is_empty());
    }

    #[test]
    fn test_remove_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();
        assert!(!sf.remove("nonexistent").unwrap());
    }

    #[test]
    fn test_remove_preserves_others() {
        let tmp = TempDir::new().unwrap();
        let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();

        sf.add(sample_record("id1", "box1", "created")).unwrap();
        sf.add(sample_record("id2", "box2", "created")).unwrap();
        sf.add(sample_record("id3", "box3", "created")).unwrap();

        assert!(sf.remove("id2").unwrap());
        assert_eq!(sf.records().len(), 2);
        assert!(sf.find_by_id("id1").is_some());
        assert!(sf.find_by_id("id2").is_none());
        assert!(sf.find_by_id("id3").is_some());
    }

    // --- Prefix search tests ---

    #[test]
    fn test_find_by_id_prefix() {
        let tmp = TempDir::new().unwrap();
        let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();

        sf.add(sample_record("abc-def-123", "box1", "created")).unwrap();
        sf.add(sample_record("abc-def-456", "box2", "created")).unwrap();
        sf.add(sample_record("xyz-000-111", "box3", "created")).unwrap();

        assert_eq!(sf.find_by_id_prefix("abc").len(), 2);
        assert_eq!(sf.find_by_id_prefix("xyz").len(), 1);
        assert_eq!(sf.find_by_id_prefix("zzz").len(), 0);
    }

    #[test]
    fn test_find_by_short_id_prefix() {
        let tmp = TempDir::new().unwrap();
        let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();

        // UUID format: "550e8400-e29b-41d4-a716-446655440000"
        // short_id:    "550e8400e29b"
        sf.add(sample_record("550e8400-e29b-41d4-a716-446655440000", "box1", "created")).unwrap();

        // Search by short_id prefix
        let matches = sf.find_by_id_prefix("550e8400e");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "box1");
    }

    // --- List filter tests ---

    #[test]
    fn test_list_filter() {
        let tmp = TempDir::new().unwrap();
        let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();

        sf.add(sample_record("id1", "box1", "created")).unwrap();
        let mut running = sample_record("id2", "box2", "running");
        running.pid = Some(99999);
        sf.add(running).unwrap();

        let all = sf.list(true);
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_list_all_statuses() {
        let tmp = TempDir::new().unwrap();
        let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();

        sf.add(sample_record("id1", "box1", "created")).unwrap();
        sf.add(sample_record("id2", "box2", "stopped")).unwrap();
        sf.add(sample_record("id3", "box3", "dead")).unwrap();

        // None are "running", so list(false) should return empty
        let running = sf.list(false);
        assert_eq!(running.len(), 0);

        // list(true) returns all
        let all = sf.list(true);
        assert_eq!(all.len(), 3);
    }

    // --- Persistence tests ---

    #[test]
    fn test_persistence() {
        let tmp = TempDir::new().unwrap();
        let path = test_state_path(&tmp);

        {
            let mut sf = StateFile::load(&path).unwrap();
            sf.add(sample_record("persist-id", "persist_box", "created")).unwrap();
        }

        {
            let sf = StateFile::load(&path).unwrap();
            assert_eq!(sf.records().len(), 1);
            assert_eq!(sf.find_by_id("persist-id").unwrap().name, "persist_box");
        }
    }

    #[test]
    fn test_persistence_multiple_records() {
        let tmp = TempDir::new().unwrap();
        let path = test_state_path(&tmp);

        {
            let mut sf = StateFile::load(&path).unwrap();
            sf.add(sample_record("id1", "box1", "created")).unwrap();
            sf.add(sample_record("id2", "box2", "stopped")).unwrap();
        }

        {
            let sf = StateFile::load(&path).unwrap();
            assert_eq!(sf.records().len(), 2);

            let rec1 = sf.find_by_id("id1").unwrap();
            assert_eq!(rec1.name, "box1");
            assert_eq!(rec1.status, "created");

            let rec2 = sf.find_by_id("id2").unwrap();
            assert_eq!(rec2.name, "box2");
            assert_eq!(rec2.status, "stopped");
        }
    }

    #[test]
    fn test_persistence_after_remove() {
        let tmp = TempDir::new().unwrap();
        let path = test_state_path(&tmp);

        {
            let mut sf = StateFile::load(&path).unwrap();
            sf.add(sample_record("id1", "box1", "created")).unwrap();
            sf.add(sample_record("id2", "box2", "created")).unwrap();
            sf.remove("id1").unwrap();
        }

        {
            let sf = StateFile::load(&path).unwrap();
            assert_eq!(sf.records().len(), 1);
            assert!(sf.find_by_id("id1").is_none());
            assert!(sf.find_by_id("id2").is_some());
        }
    }

    // --- Reconcile tests ---

    #[test]
    fn test_reconcile_marks_dead_pid() {
        let tmp = TempDir::new().unwrap();
        let path = test_state_path(&tmp);

        // Create a state file with a "running" box with a dead PID
        {
            let mut sf = StateFile::load(&path).unwrap();
            let mut record = sample_record("dead-pid-id", "dead_pid_box", "created");
            // Manually set to running with an impossible PID
            record.status = "running".to_string();
            record.pid = Some(4294967); // Very unlikely to be a real process
            sf.records.push(record);
            sf.save().unwrap();
        }

        // Reload — reconcile should mark it as dead
        {
            let sf = StateFile::load(&path).unwrap();
            let record = sf.find_by_id("dead-pid-id").unwrap();
            assert_eq!(record.status, "dead");
            assert!(record.pid.is_none());
        }
    }

    #[test]
    fn test_reconcile_running_without_pid() {
        let tmp = TempDir::new().unwrap();
        let path = test_state_path(&tmp);

        {
            let mut sf = StateFile::load(&path).unwrap();
            let mut record = sample_record("no-pid-id", "no_pid_box", "created");
            record.status = "running".to_string();
            record.pid = None; // Running but no PID
            sf.records.push(record);
            sf.save().unwrap();
        }

        {
            let sf = StateFile::load(&path).unwrap();
            let record = sf.find_by_id("no-pid-id").unwrap();
            assert_eq!(record.status, "dead");
        }
    }

    #[test]
    fn test_reconcile_ignores_non_running() {
        let tmp = TempDir::new().unwrap();
        let path = test_state_path(&tmp);

        {
            let mut sf = StateFile::load(&path).unwrap();
            sf.add(sample_record("created-id", "created_box", "created")).unwrap();
            sf.add(sample_record("stopped-id", "stopped_box", "stopped")).unwrap();
        }

        {
            let sf = StateFile::load(&path).unwrap();
            assert_eq!(sf.find_by_id("created-id").unwrap().status, "created");
            assert_eq!(sf.find_by_id("stopped-id").unwrap().status, "stopped");
        }
    }

    // --- Atomic save tests ---

    #[test]
    fn test_atomic_save() {
        let tmp = TempDir::new().unwrap();
        let path = test_state_path(&tmp);

        let mut sf = StateFile::load(&path).unwrap();
        sf.add(sample_record("id1", "box1", "created")).unwrap();

        let tmp_path = path.with_extension("json.tmp");
        assert!(!tmp_path.exists());
        assert!(path.exists());
    }

    // --- Mutation tests ---

    #[test]
    fn test_find_by_id_mut() {
        let tmp = TempDir::new().unwrap();
        let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();

        sf.add(sample_record("mut-id", "mut_box", "created")).unwrap();

        let record = sf.find_by_id_mut("mut-id").unwrap();
        record.status = "running".to_string();
        record.pid = Some(12345);

        assert_eq!(sf.find_by_id("mut-id").unwrap().status, "running");
    }

    #[test]
    fn test_find_by_id_mut_not_found() {
        let tmp = TempDir::new().unwrap();
        let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();
        assert!(sf.find_by_id_mut("nonexistent").is_none());
    }

    #[test]
    fn test_mutation_persists_after_save() {
        let tmp = TempDir::new().unwrap();
        let path = test_state_path(&tmp);

        {
            let mut sf = StateFile::load(&path).unwrap();
            sf.add(sample_record("mut-save-id", "mut_save", "created")).unwrap();

            let record = sf.find_by_id_mut("mut-save-id").unwrap();
            record.status = "stopped".to_string();
            sf.save().unwrap();
        }

        {
            let sf = StateFile::load(&path).unwrap();
            assert_eq!(sf.find_by_id("mut-save-id").unwrap().status, "stopped");
        }
    }

    // --- Name generation tests ---

    #[test]
    fn test_generate_name() {
        let name = generate_name();
        assert!(name.contains('_'));
        let parts: Vec<&str> = name.split('_').collect();
        assert_eq!(parts.len(), 2);
        assert!(!parts[0].is_empty());
        assert!(!parts[1].is_empty());
    }

    #[test]
    fn test_generate_name_uniqueness() {
        // Generate 50 names and check that at least some are unique
        // (with 32*32=1024 combinations, collisions in 50 are unlikely)
        let names: Vec<String> = (0..50).map(|_| generate_name()).collect();
        let unique: std::collections::HashSet<&String> = names.iter().collect();
        assert!(unique.len() > 1, "Expected unique names, got all identical");
    }

    #[test]
    fn test_generate_name_uses_valid_words() {
        let name = generate_name();
        let parts: Vec<&str> = name.split('_').collect();

        assert!(
            ADJECTIVES.contains(&parts[0]),
            "Adjective '{}' not in word list",
            parts[0]
        );
        assert!(
            NOUNS.contains(&parts[1]),
            "Noun '{}' not in word list",
            parts[1]
        );
    }

    #[test]
    fn test_word_lists_not_empty() {
        assert!(!ADJECTIVES.is_empty());
        assert!(!NOUNS.is_empty());
    }
}
