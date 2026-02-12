//! Log processing — tails raw console.log and writes structured JSON logs.

use a3s_box_core::log::{LogConfig, LogDriver, LogEntry};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use tokio::task::JoinHandle;

/// Start a background log processor that tails `console.log` and writes
/// structured JSON lines to `container.json`.
///
/// Returns a handle that can be aborted when the box stops.
pub fn spawn_log_processor(
    console_log: PathBuf,
    log_dir: PathBuf,
    config: LogConfig,
) -> Option<JoinHandle<()>> {
    match config.driver {
        LogDriver::None => None,
        LogDriver::JsonFile => {
            let max_size = config.max_size();
            let max_file = config.max_file();
            Some(tokio::task::spawn_blocking(move || {
                run_json_file_processor(&console_log, &log_dir, max_size, max_file);
            }))
        }
    }
}

/// Path to the structured JSON log file.
pub fn json_log_path(log_dir: &Path) -> PathBuf {
    log_dir.join("container.json")
}

/// Tail console.log and write Docker-compatible JSON lines to container.json.
fn run_json_file_processor(
    console_log: &Path,
    log_dir: &Path,
    max_size: u64,
    max_file: u32,
) {
    // Wait for console.log to appear
    for _ in 0..300 {
        if console_log.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let file = match std::fs::File::open(console_log) {
        Ok(f) => f,
        Err(_) => return,
    };

    let reader = BufReader::new(file);
    let json_path = json_log_path(log_dir);
    let mut writer = match RotatingWriter::new(&json_path, max_size, max_file) {
        Ok(w) => w,
        Err(_) => return,
    };

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => {
                // EOF — poll for more data
                std::thread::sleep(std::time::Duration::from_millis(200));
                continue;
            }
        };

        let entry = LogEntry {
            log: format!("{}\n", line),
            stream: "stdout".to_string(),
            time: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        };

        if let Ok(json) = serde_json::to_string(&entry) {
            let _ = writer.write_line(&json);
        }
    }
}

/// A file writer that rotates when the file exceeds `max_size`.
struct RotatingWriter {
    path: PathBuf,
    file: std::fs::File,
    written: u64,
    max_size: u64,
    max_file: u32,
}

impl RotatingWriter {
    fn new(path: &Path, max_size: u64, max_file: u32) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let written = file.metadata()?.len();
        Ok(Self {
            path: path.to_path_buf(),
            file,
            written,
            max_size,
            max_file,
        })
    }

    fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        let bytes = format!("{}\n", line);
        self.file.write_all(bytes.as_bytes())?;
        self.file.flush()?;
        self.written += bytes.len() as u64;

        if self.written >= self.max_size {
            self.rotate()?;
        }
        Ok(())
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        // Shift existing rotated files: .2 → .3, .1 → .2, etc.
        for i in (1..self.max_file).rev() {
            let from = rotated_path(&self.path, i);
            let to = rotated_path(&self.path, i + 1);
            if from.exists() {
                std::fs::rename(&from, &to)?;
            }
        }

        // Delete the oldest if it exceeds max_file
        let oldest = rotated_path(&self.path, self.max_file);
        if oldest.exists() {
            std::fs::remove_file(&oldest)?;
        }

        // Current → .1
        let rotated = rotated_path(&self.path, 1);
        std::fs::rename(&self.path, &rotated)?;

        // Open a fresh file
        self.file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

/// Generate a rotated file path: container.json → container.json.1
fn rotated_path(base: &Path, index: u32) -> PathBuf {
    let mut p = base.as_os_str().to_owned();
    p.push(format!(".{}", index));
    PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::TempDir;

    #[test]
    fn test_rotating_writer_basic() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.json");
        let mut writer = RotatingWriter::new(&path, 1024, 3).unwrap();
        writer.write_line(r#"{"log":"hello\n","stream":"stdout","time":"2026-01-01T00:00:00Z"}"#).unwrap();

        let mut content = String::new();
        std::fs::File::open(&path).unwrap().read_to_string(&mut content).unwrap();
        assert!(content.contains("hello"));
    }

    #[test]
    fn test_rotating_writer_rotation() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.json");
        // Small max_size to trigger rotation quickly
        let mut writer = RotatingWriter::new(&path, 50, 2).unwrap();

        for i in 0..5 {
            writer.write_line(&format!(r#"{{"log":"line {}\n"}}"#, i)).unwrap();
        }

        // Should have rotated — check .1 exists
        assert!(rotated_path(&path, 1).exists());
        // .3 should not exist (max_file=2)
        assert!(!rotated_path(&path, 3).exists());
    }

    #[test]
    fn test_json_log_path() {
        let p = json_log_path(Path::new("/tmp/logs"));
        assert_eq!(p, PathBuf::from("/tmp/logs/container.json"));
    }
}
