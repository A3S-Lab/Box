//! `a3s-box cp` command — Copy files between host and a running box.
//!
//! Uses the exec channel to transfer file contents via base64 encoding.
//!
//! Syntax:
//!   a3s-box cp <box>:/path/in/box /host/path   (box → host)
//!   a3s-box cp /host/path <box>:/path/in/box   (host → box)

use a3s_box_core::exec::{ExecRequest, DEFAULT_EXEC_TIMEOUT_NS};
use a3s_box_runtime::ExecClient;
use clap::Args;

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct CpArgs {
    /// Source path (HOST_PATH or BOX:CONTAINER_PATH)
    pub src: String,

    /// Destination path (HOST_PATH or BOX:CONTAINER_PATH)
    pub dst: String,
}

/// Parsed copy endpoint — either a host path or a box:path pair.
enum Endpoint {
    Host(String),
    Box { name: String, path: String },
}

fn parse_endpoint(s: &str) -> Endpoint {
    // Docker convention: "container:/path" means container path
    // A bare path (no colon, or colon after drive letter on Windows) means host
    if let Some((name, path)) = s.split_once(':') {
        // Avoid treating "C:\path" as a container reference
        if name.len() > 1 {
            return Endpoint::Box {
                name: name.to_string(),
                path: path.to_string(),
            };
        }
    }
    Endpoint::Host(s.to_string())
}

pub async fn execute(args: CpArgs) -> Result<(), Box<dyn std::error::Error>> {
    let src = parse_endpoint(&args.src);
    let dst = parse_endpoint(&args.dst);

    match (src, dst) {
        (Endpoint::Box { name, path }, Endpoint::Host(host_path)) => {
            copy_from_box(&name, &path, &host_path).await
        }
        (Endpoint::Host(host_path), Endpoint::Box { name, path }) => {
            copy_to_box(&host_path, &name, &path).await
        }
        (Endpoint::Host(_), Endpoint::Host(_)) => {
            Err("Both source and destination are host paths. One must be a box path (BOX:/path).".into())
        }
        (Endpoint::Box { .. }, Endpoint::Box { .. }) => {
            Err("Copying between two boxes is not supported. Copy to host first.".into())
        }
    }
}

/// Copy a file from a box to the host.
async fn copy_from_box(
    box_name: &str,
    box_path: &str,
    host_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = connect_exec(box_name).await?;

    // Read file via base64 to safely transfer binary content
    let request = ExecRequest {
        cmd: vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("base64 < {}", shell_escape(box_path)),
        ],
        timeout_ns: DEFAULT_EXEC_TIMEOUT_NS,
        env: vec![],
        working_dir: None,
        stdin: None,
    };

    let output = client.exec_command(&request).await?;

    if output.exit_code != 0 {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to read {box_path} in box: {stderr}").into());
    }

    // Decode base64 and write to host
    use base64::Engine;
    let encoded = String::from_utf8_lossy(&output.stdout);
    let clean: String = encoded.chars().filter(|c| !c.is_whitespace()).collect();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&clean)
        .map_err(|e| format!("Failed to decode file content: {e}"))?;

    std::fs::write(host_path, &decoded)
        .map_err(|e| format!("Failed to write to {host_path}: {e}"))?;

    println!("{box_name}:{box_path} → {host_path} ({} bytes)", decoded.len());
    Ok(())
}

/// Copy a file from the host to a box.
async fn copy_to_box(
    host_path: &str,
    box_name: &str,
    box_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let content = std::fs::read(host_path)
        .map_err(|e| format!("Failed to read {host_path}: {e}"))?;

    let client = connect_exec(box_name).await?;

    // Encode as base64 and write via shell
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&content);

    // Use echo + base64 -d to write the file
    // Split into chunks to avoid argument length limits
    let request = ExecRequest {
        cmd: vec![
            "sh".to_string(),
            "-c".to_string(),
            format!(
                "echo '{}' | base64 -d > {}",
                encoded,
                shell_escape(box_path)
            ),
        ],
        timeout_ns: DEFAULT_EXEC_TIMEOUT_NS,
        env: vec![],
        working_dir: None,
        stdin: None,
    };

    let output = client.exec_command(&request).await?;

    if output.exit_code != 0 {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to write {box_path} in box: {stderr}").into());
    }

    println!("{host_path} → {box_name}:{box_path} ({} bytes)", content.len());
    Ok(())
}

/// Connect to a box's exec server.
async fn connect_exec(box_name: &str) -> Result<ExecClient, Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, box_name)?;

    if record.status != "running" {
        return Err(format!("Box {} is not running", record.name).into());
    }

    let exec_socket_path = if !record.exec_socket_path.as_os_str().is_empty() {
        record.exec_socket_path.clone()
    } else {
        record.box_dir.join("sockets").join("exec.sock")
    };

    if !exec_socket_path.exists() {
        return Err(format!(
            "Exec socket not found for box {} at {}",
            record.name,
            exec_socket_path.display()
        )
        .into());
    }

    ExecClient::connect(&exec_socket_path).await.map_err(|e| e.into())
}

/// Minimal shell escaping for a file path.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
