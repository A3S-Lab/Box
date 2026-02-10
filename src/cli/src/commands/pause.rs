//! `a3s-box pause` command — Pause one or more running boxes.
//!
//! Sends SIGSTOP to the box process and updates the status to "paused".

use clap::Args;

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct PauseArgs {
    /// Box name(s) or ID(s)
    #[arg(required = true)]
    pub boxes: Vec<String>,
}

pub async fn execute(args: PauseArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = StateFile::load_default()?;
    let mut errors: Vec<String> = Vec::new();

    for query in &args.boxes {
        if let Err(e) = pause_one(&mut state, query) {
            errors.push(format!("{query}: {e}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n").into())
    }
}

fn pause_one(
    state: &mut StateFile,
    query: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let record = resolve::resolve(state, query)?;

    if record.status != "running" {
        return Err(format!("Box {} is not running", record.name).into());
    }

    let box_id = record.id.clone();
    let name = record.name.clone();

    if let Some(pid) = record.pid {
        // Safety: sending SIGSTOP to pause the process
        unsafe {
            libc::kill(pid as i32, libc::SIGSTOP);
        }
    }

    // Update status to paused
    let record = resolve::resolve_mut(state, &box_id)?;
    record.status = "paused".to_string();
    state.save()?;

    println!("{name}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::BoxRecord;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn make_record(id: &str, name: &str, status: &str, pid: Option<u32>) -> BoxRecord {
        let short_id = BoxRecord::make_short_id(id);
        BoxRecord {
            id: id.to_string(),
            short_id,
            name: name.to_string(),
            image: "alpine:latest".to_string(),
            status: status.to_string(),
            pid,
            cpus: 2,
            memory_mb: 512,
            volumes: vec![],
            env: HashMap::new(),
            cmd: vec![],
            entrypoint: None,
            box_dir: PathBuf::from("/tmp").join(id),
            socket_path: PathBuf::from("/tmp").join(id).join("grpc.sock"),
            exec_socket_path: PathBuf::from("/tmp").join(id).join("sockets").join("exec.sock"),
            console_log: PathBuf::from("/tmp").join(id).join("console.log"),
            created_at: chrono::Utc::now(),
            started_at: None,
            auto_remove: false,
            hostname: None,
            user: None,
            workdir: None,
            restart_policy: "no".to_string(),
            port_map: vec![],
            labels: HashMap::new(),
        }
    }

    fn setup_state(records: Vec<BoxRecord>) -> (TempDir, StateFile) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("boxes.json");
        let mut sf = StateFile::load(&path).unwrap();
        for r in records {
            sf.add(r).unwrap();
        }
        (tmp, sf)
    }

    #[test]
    fn test_pause_rejects_non_running() {
        let (_tmp, mut state) = setup_state(vec![
            make_record("id-1", "stopped_box", "stopped", None),
        ]);
        let result = pause_one(&mut state, "stopped_box");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not running"));
    }

    #[test]
    fn test_pause_rejects_created() {
        let (_tmp, mut state) = setup_state(vec![
            make_record("id-1", "created_box", "created", None),
        ]);
        let result = pause_one(&mut state, "created_box");
        assert!(result.is_err());
    }

    #[test]
    fn test_pause_rejects_already_paused() {
        let (_tmp, mut state) = setup_state(vec![
            make_record("id-1", "paused_box", "paused", Some(99999)),
        ]);
        let result = pause_one(&mut state, "paused_box");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not running"));
    }

    #[test]
    fn test_pause_not_found() {
        let (_tmp, mut state) = setup_state(vec![]);
        let result = pause_one(&mut state, "nonexistent");
        assert!(result.is_err());
    }
}
