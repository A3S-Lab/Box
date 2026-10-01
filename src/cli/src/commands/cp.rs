//! `a3s-box cp` command — Copy files or directories between host and a box
//! (running, or freezer-paused for managed OCI Sandbox file I/O).
//!
//! Uses the selected runtime session's native file protocol for single files.
//! Directories are archived with `tar` before transfer.
//!
//! Syntax:
//!   a3s-box cp <box>:/path/in/box /host/path   (box → host)
//!   a3s-box cp /host/path <box>:/path/in/box   (host → box)

use clap::Args;

use a3s_box_core::error::BoxError;
#[cfg(unix)]
use a3s_box_core::exec::DEFAULT_EXEC_TIMEOUT_NS;
use a3s_box_core::exec::{
    ExecRequest, FileOp, FileRequest, FilesystemEntryKind, FilesystemOp, FilesystemRequest,
};

use self::session::{connect_copy_session, CopySession};

mod session;

/// Timeout for directory transfers (60 seconds).
const DIR_TRANSFER_TIMEOUT_NS: u64 = 60_000_000_000;
const MAX_CP_REQUEST_ID_BYTES: usize = 512;

fn mint_cli_cp_request_id() -> String {
    format!("cli-cp-{}", uuid::Uuid::new_v4().simple())
}

fn mint_cli_file_request_id() -> String {
    format!("cli-file-{}", uuid::Uuid::new_v4().simple())
}

fn annotate_cp_unavailable(error: BoxError, request_id: &str) -> BoxError {
    let unavailable = error
        .to_string()
        .to_ascii_lowercase()
        .contains("unavailable");
    if !unavailable {
        return error;
    }
    let hint = format!(" (reuse request_id {request_id} on retry)");
    match error {
        BoxError::StateError(message) => BoxError::StateError(message + &hint),
        BoxError::ExecError(message) => BoxError::ExecError(message + &hint),
        BoxError::TimeoutError(message) => BoxError::TimeoutError(message + &hint),
        BoxError::ConfigError(message) => BoxError::ConfigError(message + &hint),
        BoxError::Other(message) => BoxError::Other(message + &hint),
        BoxError::IoError(error) => {
            BoxError::IoError(std::io::Error::other(format!("{error}{hint}")))
        }
        other => BoxError::StateError(format!("{other}{hint}")),
    }
}

async fn execute_copy_command(
    session: &CopySession,
    mut request: ExecRequest,
) -> Result<a3s_box_core::exec::ExecOutput, BoxError> {
    let request_id = match request.request_id.take() {
        Some(request_id) => {
            if request_id.is_empty()
                || request_id.len() > MAX_CP_REQUEST_ID_BYTES
                || request_id.contains('\0')
            {
                return Err(BoxError::ConfigError(
                    "copy exec request_id must be a non-empty UTF-8 string of at most 512 bytes without NUL"
                        .to_string(),
                ));
            }
            request_id
        }
        None => mint_cli_cp_request_id(),
    };
    request.request_id = Some(request_id.clone());
    match session.execute(request).await {
        Ok(output) => Ok(output),
        Err(error) => Err(annotate_cp_unavailable(error, &request_id)),
    }
}

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

pub async fn execute(args: CpArgs) -> Result<(), BoxError> {
    let src = parse_endpoint(&args.src);
    let dst = parse_endpoint(&args.dst);

    match (src, dst) {
        (Endpoint::Box { name, path }, Endpoint::Host(host_path)) => {
            copy_from_box(&name, &path, &host_path).await
        }
        (Endpoint::Host(host_path), Endpoint::Box { name, path }) => {
            copy_to_box(&host_path, &name, &path).await
        }
        (Endpoint::Host(_), Endpoint::Host(_)) => Err(BoxError::ConfigError(
            "Both source and destination are host paths. One must be a box path (BOX:/path)."
                .to_string(),
        )),
        (Endpoint::Box { .. }, Endpoint::Box { .. }) => Err(BoxError::ConfigError(
            "Copying between two boxes is not supported. Copy to host first.".to_string(),
        )),
    }
}

/// Copy a file or directory from a box to the host.
async fn copy_from_box(box_name: &str, box_path: &str, host_path: &str) -> Result<(), BoxError> {
    let session = connect_copy_session(box_name).await?;

    if is_directory_in_box(&session, box_path).await? {
        copy_dir_from_box(&session, box_name, box_path, host_path).await
    } else {
        copy_file_from_box(&session, box_name, box_path, host_path).await
    }
}

/// Copy a file or directory from the host to a box.
async fn copy_to_box(host_path: &str, box_name: &str, box_path: &str) -> Result<(), BoxError> {
    let meta = std::fs::metadata(host_path)
        .map_err(|error| super::io_error(format!("Failed to stat {host_path}"), error))?;

    let session = connect_copy_session(box_name).await?;

    if meta.is_dir() {
        copy_dir_to_box(&session, host_path, box_name, box_path).await
    } else {
        copy_file_to_box(&session, host_path, box_name, box_path).await
    }
}

/// Check if a path is a directory inside the box.
async fn is_directory_in_box(session: &CopySession, box_path: &str) -> Result<bool, BoxError> {
    let response = session
        .filesystem(FilesystemRequest {
            op: FilesystemOp::Stat,
            path: box_path.to_string(),
            destination: None,
            depth: 0,
            user: None,
            request_id: None,
        })
        .await?;
    if !response.success {
        return Err(BoxError::ExecError(format!(
            "Failed to stat {box_path} in box: {}",
            response
                .error
                .unwrap_or_else(|| "guest returned an unspecified error".to_string())
        )));
    }
    let entry = response.entry.ok_or_else(|| {
        BoxError::ExecError(format!(
            "Guest stat response for {box_path} did not include metadata"
        ))
    })?;
    match entry.kind {
        FilesystemEntryKind::Directory => Ok(true),
        FilesystemEntryKind::File => Ok(false),
        FilesystemEntryKind::Unspecified => Err(BoxError::ConfigError(format!(
            "Unsupported file type in box: {box_path}"
        ))),
    }
}

#[cfg(unix)]
async fn restore_file_mode_in_box(
    session: &CopySession,
    box_path: &str,
    mode: u32,
) -> Result<(), BoxError> {
    let request = ExecRequest {
        request_id: None,
        cmd: vec![
            "chmod".to_string(),
            format!("{mode:o}"),
            box_path.to_string(),
        ],
        timeout_ns: DEFAULT_EXEC_TIMEOUT_NS,
        env: vec![],
        working_dir: None,
        rootfs: None,
        stdin: None,
        stdin_streaming: false,
        user: None,
        streaming: false,
    };

    let output = execute_copy_command(session, request).await?;
    if output.exit_code != 0 {
        return Err(BoxError::ExecError(format!(
            "Failed to set permissions on {box_path} in box: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Single-file transfers
// ---------------------------------------------------------------------------

/// Copy a single file from a box to the host.
async fn copy_file_from_box(
    session: &CopySession,
    box_name: &str,
    box_path: &str,
    host_path: &str,
) -> Result<(), BoxError> {
    use base64::Engine;
    let response = session
        .transfer_file(FileRequest {
            op: FileOp::Download,
            guest_path: box_path.to_string(),
            data: None,
            user: None,
            max_bytes: None,
            request_id: None,
        })
        .await?;
    if !response.success {
        return Err(BoxError::ExecError(format!(
            "Failed to read {box_path} in box: {}",
            response
                .error
                .unwrap_or_else(|| "guest returned an unspecified error".to_string())
        )));
    }

    let decoded = base64::engine::general_purpose::STANDARD
        .decode(response.data.unwrap_or_default())
        .map_err(|error| {
            BoxError::ExecError(format!(
                "Guest returned invalid file content for {box_path}: {error}"
            ))
        })?;
    if response.size != decoded.len() as u64 {
        return Err(BoxError::ExecError(format!(
            "Guest returned {} bytes for {box_path}, expected {}",
            decoded.len(),
            response.size
        )));
    }

    write_host_copy_destination(host_path, &decoded)?;

    println!(
        "{box_name}:{box_path} → {host_path} ({} bytes)",
        decoded.len()
    );
    Ok(())
}

/// Copy a single file from the host to a box.
async fn copy_file_to_box(
    session: &CopySession,
    host_path: &str,
    box_name: &str,
    box_path: &str,
) -> Result<(), BoxError> {
    let content = read_host_copy_source(host_path)?;
    let len = content.len();

    use base64::Engine;
    let response = session
        .transfer_file(FileRequest {
            op: FileOp::Upload,
            guest_path: box_path.to_string(),
            data: Some(base64::engine::general_purpose::STANDARD.encode(&content)),
            user: None,
            max_bytes: None,
            request_id: Some(mint_cli_file_request_id()),
        })
        .await?;
    if !response.success {
        return Err(BoxError::ExecError(format!(
            "Failed to write {box_path} in box: {}",
            response
                .error
                .unwrap_or_else(|| "guest returned an unspecified error".to_string())
        )));
    }
    if response.size != len as u64 {
        return Err(BoxError::ExecError(format!(
            "Guest wrote {} bytes to {box_path}, expected {len}",
            response.size
        )));
    }

    #[cfg(unix)]
    restore_file_mode_in_box(session, box_path, host_file_mode(host_path)).await?;

    println!("{host_path} → {box_name}:{box_path} ({len} bytes)");
    Ok(())
}

/// Source file's permission bits (lower 12) for `cp` to restore in the box.
#[cfg(unix)]
fn host_file_mode(host_path: &str) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(host_path)
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0o644)
}

// ---------------------------------------------------------------------------
// Directory transfers
// ---------------------------------------------------------------------------

/// Copy a directory from a box to the host using tar.
async fn copy_dir_from_box(
    session: &CopySession,
    box_name: &str,
    box_path: &str,
    host_path: &str,
) -> Result<(), BoxError> {
    // Archive the directory inside the box and base64-encode it
    let request = ExecRequest {
        request_id: None,
        cmd: vec![
            "sh".to_string(),
            "-c".to_string(),
            // `set -o pipefail` so a `tar` failure (EACCES, missing file)
            // propagates instead of being masked by base64's exit 0 — otherwise
            // a truncated archive extracts and `cp` falsely reports success.
            format!(
                "set -o pipefail; tar -cf - -C {} . | base64",
                shell_escape(box_path)
            ),
        ],
        timeout_ns: DIR_TRANSFER_TIMEOUT_NS,
        env: vec![],
        working_dir: None,
        rootfs: None,
        stdin: None,
        stdin_streaming: false,
        user: None,
        streaming: false,
    };

    let output = execute_copy_command(session, request).await?;

    if output.exit_code != 0 {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(BoxError::ExecError(format!(
            "Failed to archive {box_path} in box: {stderr}"
        )));
    }

    // Decode base64 tar archive
    use base64::Engine;
    let encoded = String::from_utf8_lossy(&output.stdout);
    let clean: String = encoded.chars().filter(|c| !c.is_whitespace()).collect();
    let tar_data = base64::engine::general_purpose::STANDARD
        .decode(&clean)
        .map_err(|error| BoxError::ExecError(format!("Failed to decode tar archive: {error}")))?;

    // Create destination directory and extract
    super::commit::refuse_archive_ancestor_reparse(std::path::Path::new(host_path))?;
    std::fs::create_dir_all(host_path).map_err(|error| {
        super::io_error(format!("Failed to create directory {host_path}"), error)
    })?;

    extract_tar_to_dir(&tar_data, host_path)?;

    println!(
        "{box_name}:{box_path}/ → {host_path}/ ({} bytes archived)",
        tar_data.len()
    );
    Ok(())
}

/// Copy a directory from the host to a box using tar.
async fn copy_dir_to_box(
    session: &CopySession,
    host_path: &str,
    box_name: &str,
    box_path: &str,
) -> Result<(), BoxError> {
    // Create tar archive of the host directory
    let tar_data = create_tar_from_dir(host_path)?;

    // Base64-encode and send to box
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&tar_data);

    // Create destination directory and extract inside the box
    let request = ExecRequest {
        request_id: None,
        cmd: vec![
            "sh".to_string(),
            "-c".to_string(),
            format!(
                "set -o pipefail; mkdir -p {} && echo '{}' | base64 -d | tar -xf - -C {}",
                shell_escape(box_path),
                encoded,
                shell_escape(box_path)
            ),
        ],
        timeout_ns: DIR_TRANSFER_TIMEOUT_NS,
        env: vec![],
        working_dir: None,
        rootfs: None,
        stdin: None,
        stdin_streaming: false,
        user: None,
        streaming: false,
    };

    let output = execute_copy_command(session, request).await?;

    if output.exit_code != 0 {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(BoxError::ExecError(format!(
            "Failed to extract archive in box at {box_path}: {stderr}"
        )));
    }

    println!(
        "{host_path}/ → {box_name}:{box_path}/ ({} bytes archived)",
        tar_data.len()
    );
    Ok(())
}

/// Read a host file for `cp` into a box.
fn read_host_copy_source(host_path: &str) -> Result<Vec<u8>, BoxError> {
    super::commit::refuse_archive_ancestor_reparse(std::path::Path::new(host_path))?;
    std::fs::read(host_path)
        .map_err(|error| super::io_error(format!("Failed to read {host_path}"), error))
}

fn write_host_copy_destination(host_path: &str, bytes: &[u8]) -> Result<(), BoxError> {
    super::commit::refuse_archive_ancestor_reparse(std::path::Path::new(host_path))?;
    std::fs::write(host_path, bytes)
        .map_err(|error| super::io_error(format!("Failed to write to {host_path}"), error))
}

/// Create a tar archive from a host directory using the `tar` command.
fn create_tar_from_dir(dir_path: &str) -> Result<Vec<u8>, BoxError> {
    let path = std::path::Path::new(dir_path);
    super::commit::refuse_archive_ancestor_reparse(path)?;
    #[cfg(windows)]
    {
        super::commit::refuse_directory_reparse(path)?;
        super::commit::refuse_nested_directory_reparse(path)?;
    }
    let output = std::process::Command::new("tar")
        .args(["-cf", "-", "-C", dir_path, "."])
        .output()
        .map_err(|error| super::io_error("Failed to run tar", error))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(BoxError::ExecError(format!("tar failed: {stderr}")));
    }

    Ok(output.stdout)
}

/// Extract a tar archive to a host directory using the `tar` command.
fn extract_tar_to_dir(tar_data: &[u8], dir_path: &str) -> Result<(), BoxError> {
    use std::io::Write;
    use std::process::Stdio;

    let path = std::path::Path::new(dir_path);
    super::commit::refuse_archive_ancestor_reparse(path)?;
    #[cfg(windows)]
    {
        super::commit::refuse_directory_reparse(path)?;
        super::commit::refuse_nested_directory_reparse(path)?;
    }
    let mut child = std::process::Command::new("tar")
        .args(["-xf", "-", "-C", dir_path])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| super::io_error("Failed to run tar", error))?;

    if let Some(ref mut stdin) = child.stdin {
        stdin
            .write_all(tar_data)
            .map_err(|error| super::io_error("Failed to write tar data", error))?;
    }
    // Close stdin by dropping it
    drop(child.stdin.take());

    let output = child
        .wait_with_output()
        .map_err(|error| super::io_error("Failed to wait for tar", error))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(BoxError::ExecError(format!(
            "tar extraction failed: {stderr}"
        )));
    }

    Ok(())
}

/// Minimal shell escaping for a file path.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use a3s_box_core::pty::PtyRequest;
    use a3s_box_core::{
        BoxConfig, CreateExecutionRequest, ExecOutput, ExecutionGeneration, ExecutionId,
        ExecutionIsolation, ExecutionManagerError, ExecutionManagerResult, ExecutionProcess,
        ExecutionSessionManager, FileResponse, FilesystemResponse, OperationId,
    };
    use a3s_box_runtime::{ManagedExecutionMetadata, ManagedRuntimeRoute};

    use super::session::{resolve_copy_route, CopyRoute};
    use crate::state::BoxRecord;

    #[cfg(windows)]
    #[test]
    fn create_tar_from_dir_does_not_follow_an_ancestor_junction() {
        use std::os::windows::process::CommandExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let outside = tmp.path().join("outside");
        let data = outside.join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("secret.txt"), b"secret").unwrap();
        let parent = tmp.path().join("parent");
        std::fs::create_dir_all(&parent).unwrap();
        let link = parent.join("link");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());

        let error = create_tar_from_dir(link.join("data").to_str().expect("utf-8"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("junction"),
            "host directory was archived through an ancestor junction: {error}"
        );
        assert_eq!(std::fs::read(data.join("secret.txt")).unwrap(), b"secret");
    }

    #[cfg(windows)]
    #[test]
    fn create_tar_from_dir_does_not_follow_a_directory_junction() {
        use std::os::windows::process::CommandExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
        let link = tmp.path().join("link");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());

        let archived = create_tar_from_dir(link.to_str().expect("utf-8"));
        let error = archived.as_ref().err().map(|error| error.to_string());
        assert!(
            error
                .as_deref()
                .is_some_and(|error| error.contains("junction")),
            "host directory junction was archived: {archived:?}"
        );
        assert_eq!(
            std::fs::read(outside.join("secret.txt")).unwrap(),
            b"secret"
        );
    }

    #[cfg(windows)]
    #[test]
    fn create_tar_from_dir_does_not_follow_a_child_directory_junction() {
        use std::os::windows::process::CommandExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
        let root = tmp.path().join("data");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("local.txt"), b"ok").unwrap();
        let link = root.join("nested");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());

        let archived = create_tar_from_dir(root.to_str().expect("utf-8"));
        let contains_secret = archived
            .as_ref()
            .ok()
            .is_some_and(|bytes| bytes.windows(6).any(|window| window == b"secret"));
        assert!(
            !contains_secret,
            "host directory archive followed a child directory junction: {archived:?}"
        );
        assert_eq!(
            std::fs::read(outside.join("secret.txt")).unwrap(),
            b"secret"
        );
    }

    #[cfg(windows)]
    #[test]
    fn extract_tar_to_dir_does_not_create_through_an_ancestor_junction() {
        use std::os::windows::process::CommandExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let source = tmp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("planted.txt"), b"planted").unwrap();
        let tar_data = create_tar_from_dir(source.to_str().expect("utf-8")).unwrap();
        let outside = tmp.path().join("outside");
        let dest = outside.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let parent = tmp.path().join("parent");
        std::fs::create_dir_all(&parent).unwrap();
        let link = parent.join("link");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());

        let error = extract_tar_to_dir(&tar_data, link.join("dest").to_str().expect("utf-8"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("junction"),
            "archive was extracted through an ancestor junction: {error}"
        );
        assert!(!dest.join("planted.txt").exists());
    }

    #[cfg(windows)]
    #[test]
    fn extract_tar_to_dir_does_not_extract_through_a_directory_junction() {
        use std::os::windows::process::CommandExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let source = tmp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("planted.txt"), b"planted").unwrap();
        let tar_data = create_tar_from_dir(source.to_str().expect("utf-8")).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
        let link = tmp.path().join("link");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());

        let error = extract_tar_to_dir(&tar_data, link.to_str().expect("utf-8"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("junction"),
            "archive was extracted through a directory junction: {error}"
        );
        assert!(!outside.join("planted.txt").exists());
        assert_eq!(
            std::fs::read(outside.join("secret.txt")).unwrap(),
            b"secret"
        );
    }

    #[cfg(windows)]
    #[test]
    fn extract_tar_to_dir_does_not_extract_through_a_child_directory_junction() {
        use std::os::windows::process::CommandExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let source = tmp.path().join("source");
        let nested = source.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("planted.txt"), b"planted").unwrap();
        let tar_data = create_tar_from_dir(source.to_str().expect("utf-8")).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let link = dest.join("nested");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());

        let extracted = extract_tar_to_dir(&tar_data, dest.to_str().expect("utf-8"));
        assert!(
            !outside.join("planted.txt").exists(),
            "archive extract wrote through a child directory junction: {extracted:?}"
        );
        assert_eq!(
            std::fs::read(outside.join("secret.txt")).unwrap(),
            b"secret"
        );
    }

    #[cfg(windows)]
    #[test]
    fn read_host_copy_source_does_not_follow_an_ancestor_junction() {
        use std::os::windows::process::CommandExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
        let parent = tmp.path().join("parent");
        std::fs::create_dir_all(&parent).unwrap();
        let link = parent.join("link");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());

        let error = read_host_copy_source(link.join("secret.txt").to_str().expect("utf-8"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("junction"),
            "host file was read through an ancestor junction: {error}"
        );
        assert_eq!(
            std::fs::read(outside.join("secret.txt")).unwrap(),
            b"secret"
        );
    }

    #[cfg(windows)]
    #[test]
    fn write_host_copy_destination_does_not_create_through_an_ancestor_junction() {
        use std::os::windows::process::CommandExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let parent = tmp.path().join("parent");
        std::fs::create_dir_all(&parent).unwrap();
        let link = parent.join("link");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());

        let error = write_host_copy_destination(
            link.join("planted.txt").to_str().expect("utf-8"),
            b"planted",
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("junction"),
            "host file was written through an ancestor junction: {error}"
        );
        assert!(!outside.join("planted.txt").exists());
    }

    #[derive(Debug, PartialEq, Eq)]
    enum SessionCall {
        Execute {
            execution_id: String,
            generation: ExecutionGeneration,
            command: Vec<String>,
            request_id: Option<String>,
        },
        TransferFile {
            execution_id: String,
            generation: ExecutionGeneration,
            operation: FileOp,
            path: String,
        },
        Filesystem {
            execution_id: String,
            generation: ExecutionGeneration,
            operation: FilesystemOp,
            path: String,
        },
    }

    #[derive(Default)]
    struct RecordingSessionManager {
        calls: Mutex<Vec<SessionCall>>,
    }

    #[async_trait::async_trait]
    impl ExecutionSessionManager for RecordingSessionManager {
        async fn execute(
            &self,
            execution_id: &ExecutionId,
            generation: ExecutionGeneration,
            request: ExecRequest,
        ) -> ExecutionManagerResult<ExecOutput> {
            self.calls.lock().unwrap().push(SessionCall::Execute {
                execution_id: execution_id.as_str().to_string(),
                generation,
                command: request.cmd,
                request_id: request.request_id,
            });
            Ok(ExecOutput {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit_code: 0,
                truncated: false,
            })
        }

        async fn start_process(
            &self,
            _execution_id: &ExecutionId,
            _generation: ExecutionGeneration,
            _request: ExecRequest,
        ) -> ExecutionManagerResult<ExecutionProcess> {
            Err(ExecutionManagerError::Unavailable(
                "streaming process is outside this test".to_string(),
            ))
        }

        async fn start_pty(
            &self,
            _execution_id: &ExecutionId,
            _generation: ExecutionGeneration,
            _request: PtyRequest,
        ) -> ExecutionManagerResult<ExecutionProcess> {
            Err(ExecutionManagerError::Unavailable(
                "PTY is outside this test".to_string(),
            ))
        }

        async fn transfer_file(
            &self,
            execution_id: &ExecutionId,
            generation: ExecutionGeneration,
            request: FileRequest,
        ) -> ExecutionManagerResult<FileResponse> {
            use base64::Engine;

            let response = match request.op {
                FileOp::Upload => FileResponse {
                    success: true,
                    data: None,
                    size: request
                        .data
                        .as_deref()
                        .and_then(|data| {
                            base64::engine::general_purpose::STANDARD.decode(data).ok()
                        })
                        .map_or(0, |data| data.len() as u64),
                    error: None,
                },
                FileOp::Download => FileResponse {
                    success: true,
                    data: Some(base64::engine::general_purpose::STANDARD.encode(b"copy data")),
                    size: 9,
                    error: None,
                },
            };
            self.calls.lock().unwrap().push(SessionCall::TransferFile {
                execution_id: execution_id.as_str().to_string(),
                generation,
                operation: request.op,
                path: request.guest_path,
            });
            Ok(response)
        }

        async fn filesystem(
            &self,
            execution_id: &ExecutionId,
            generation: ExecutionGeneration,
            request: FilesystemRequest,
        ) -> ExecutionManagerResult<FilesystemResponse> {
            self.calls.lock().unwrap().push(SessionCall::Filesystem {
                execution_id: execution_id.as_str().to_string(),
                generation,
                operation: request.op,
                path: request.path,
            });
            Ok(FilesystemResponse {
                success: true,
                entry: None,
                entries: Vec::new(),
                error: None,
            })
        }
    }

    fn test_exec_request(command: &[&str]) -> ExecRequest {
        ExecRequest {
            request_id: None,
            cmd: command.iter().map(|part| (*part).to_string()).collect(),
            timeout_ns: DIR_TRANSFER_TIMEOUT_NS,
            env: Vec::new(),
            working_dir: None,
            rootfs: None,
            stdin: None,
            stdin_streaming: false,
            user: None,
            streaming: false,
        }
    }

    fn oci_record(status: &str, generation: ExecutionGeneration) -> BoxRecord {
        let id = "11111111-1111-4111-8111-111111111111";
        let mut record =
            crate::test_helpers::fixtures::make_record(id, "managed-copy", status, Some(1));
        record.isolation = ExecutionIsolation::Sandbox;
        let mut metadata = ManagedExecutionMetadata::new(
            OperationId::new("copy-route-create").unwrap(),
            generation,
            CreateExecutionRequest {
                external_sandbox_id: "managed-copy-external".to_string(),
                config: BoxConfig {
                    isolation: ExecutionIsolation::Sandbox,
                    image: record.image.clone(),
                    ..Default::default()
                },
                labels: BTreeMap::new(),
                policy: Default::default(),
                rootfs_snapshot_id: None,
            },
        )
        .unwrap();
        metadata.runtime_route = ManagedRuntimeRoute::OciSdk;
        record.managed_execution = Some(metadata);
        record
    }

    #[test]
    fn oci_copy_route_uses_persisted_identity_without_requiring_a_socket() {
        let generation = ExecutionGeneration::new(7).unwrap();
        let mut record = oci_record("running", generation);
        record.exec_socket_path = PathBuf::from("missing-copy-socket");

        assert_eq!(
            resolve_copy_route(&record).unwrap(),
            CopyRoute::Managed {
                execution_id: ExecutionId::new(record.id).unwrap(),
                generation,
            }
        );
    }

    #[test]
    fn stopped_oci_copy_route_does_not_fall_back_to_a_socket() {
        let record = oci_record("stopped", ExecutionGeneration::new(3).unwrap());

        let error = resolve_copy_route(&record).unwrap_err();
        match error {
            BoxError::StateError(message) => {
                assert_eq!(message, "Box managed-copy is neither running nor paused");
                assert!(!message.contains("socket"));
            }
            other => panic!("expected StateError, got {other:?}"),
        }
    }

    #[test]
    fn paused_oci_copy_route_uses_managed_identity() {
        let generation = ExecutionGeneration::new(5).unwrap();
        let mut record = oci_record("paused", generation);
        record.exec_socket_path = PathBuf::from("missing-copy-socket");

        assert_eq!(
            resolve_copy_route(&record).unwrap(),
            CopyRoute::Managed {
                execution_id: ExecutionId::new(record.id).unwrap(),
                generation,
            }
        );
    }

    #[tokio::test]
    async fn managed_copy_session_fences_exec_file_and_filesystem_operations() {
        let manager = Arc::new(RecordingSessionManager::default());
        let execution_id = ExecutionId::new("managed-copy-id").unwrap();
        let generation = ExecutionGeneration::new(11).unwrap();
        let session = CopySession::Managed {
            manager: manager.clone(),
            execution_id: execution_id.clone(),
            generation,
        };

        session
            .execute(test_exec_request(&["sh", "-c", "tar -cf - ."]))
            .await
            .unwrap();
        session
            .transfer_file(FileRequest {
                op: FileOp::Download,
                guest_path: "/work/data.txt".to_string(),
                data: None,
                user: None,
                max_bytes: None,
                request_id: None,
            })
            .await
            .unwrap();
        session
            .filesystem(FilesystemRequest {
                op: FilesystemOp::Stat,
                path: "/work".to_string(),
                destination: None,
                depth: 0,
                user: None,
                request_id: None,
            })
            .await
            .unwrap();

        assert_eq!(
            *manager.calls.lock().unwrap(),
            vec![
                SessionCall::Execute {
                    execution_id: execution_id.as_str().to_string(),
                    generation,
                    command: vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        "tar -cf - .".to_string(),
                    ],
                    request_id: None,
                },
                SessionCall::TransferFile {
                    execution_id: execution_id.as_str().to_string(),
                    generation,
                    operation: FileOp::Download,
                    path: "/work/data.txt".to_string(),
                },
                SessionCall::Filesystem {
                    execution_id: execution_id.as_str().to_string(),
                    generation,
                    operation: FilesystemOp::Stat,
                    path: "/work".to_string(),
                },
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn managed_copy_restores_uploaded_file_mode_on_the_same_generation() {
        use std::os::unix::fs::PermissionsExt;

        let source = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(source.path(), b"mode data").unwrap();
        std::fs::set_permissions(source.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
        let manager = Arc::new(RecordingSessionManager::default());
        let execution_id = ExecutionId::new("managed-copy-mode").unwrap();
        let generation = ExecutionGeneration::new(5).unwrap();
        let session = CopySession::Managed {
            manager: manager.clone(),
            execution_id: execution_id.clone(),
            generation,
        };

        copy_file_to_box(
            &session,
            source.path().to_str().unwrap(),
            "managed-copy",
            "/work/mode.txt",
        )
        .await
        .unwrap();

        let calls = manager.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0],
            SessionCall::TransferFile {
                execution_id: execution_id.as_str().to_string(),
                generation,
                operation: FileOp::Upload,
                path: "/work/mode.txt".to_string(),
            }
        );
        match &calls[1] {
            SessionCall::Execute {
                execution_id: observed_id,
                generation: observed_generation,
                command,
                request_id,
            } => {
                assert_eq!(observed_id, execution_id.as_str());
                assert_eq!(*observed_generation, generation);
                assert_eq!(
                    command,
                    &vec![
                        "chmod".to_string(),
                        "750".to_string(),
                        "/work/mode.txt".to_string(),
                    ]
                );
                let request_id = request_id.as_deref().expect("minted request_id");
                assert!(
                    request_id.starts_with("cli-cp-"),
                    "unexpected request_id: {request_id}"
                );
            }
            other => panic!("expected chmod execute, got {other:?}"),
        }
    }

    #[test]
    fn mint_cli_cp_request_id_uses_stable_prefix() {
        let minted = mint_cli_cp_request_id();
        assert!(minted.starts_with("cli-cp-"), "{minted}");
    }

    #[test]
    fn annotate_cp_unavailable_surfaces_request_id() {
        let error = super::super::execution_error(
            a3s_box_core::ExecutionManagerError::Unavailable("response lost".to_string()),
        );
        let annotated = annotate_cp_unavailable(error, "cli-cp-abc");
        match annotated {
            BoxError::StateError(message) => {
                assert!(
                    message.to_ascii_lowercase().contains("unavailable"),
                    "{message}"
                );
                assert!(message.contains("reuse request_id cli-cp-abc"), "{message}");
            }
            other => panic!("expected StateError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn both_host_paths_are_a_configuration_error() {
        let error = execute(CpArgs {
            src: "/tmp/a".to_string(),
            dst: "/tmp/b".to_string(),
        })
        .await
        .unwrap_err();
        match error {
            BoxError::ConfigError(message) => {
                assert!(
                    message.contains("Both source and destination are host paths"),
                    "{message}"
                );
            }
            other => panic!("expected ConfigError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn copying_between_boxes_is_a_configuration_error() {
        let error = execute(CpArgs {
            src: "one:/tmp/a".to_string(),
            dst: "two:/tmp/b".to_string(),
        })
        .await
        .unwrap_err();
        match error {
            BoxError::ConfigError(message) => {
                assert!(
                    message.contains("Copying between two boxes is not supported"),
                    "{message}"
                );
            }
            other => panic!("expected ConfigError, got {other:?}"),
        }
    }

    // --- Endpoint parsing tests ---

    #[test]
    fn test_parse_endpoint_host_path() {
        match parse_endpoint("/tmp/file.txt") {
            Endpoint::Host(p) => assert_eq!(p, "/tmp/file.txt"),
            _ => panic!("Expected Host endpoint"),
        }
    }

    #[test]
    fn test_parse_endpoint_box_path() {
        match parse_endpoint("mybox:/tmp/file.txt") {
            Endpoint::Box { name, path } => {
                assert_eq!(name, "mybox");
                assert_eq!(path, "/tmp/file.txt");
            }
            _ => panic!("Expected Box endpoint"),
        }
    }

    #[test]
    fn test_parse_endpoint_single_char_name_is_host() {
        // Single-char prefix treated as drive letter (host path)
        match parse_endpoint("C:/path") {
            Endpoint::Host(p) => assert_eq!(p, "C:/path"),
            _ => panic!("Expected Host endpoint for drive letter"),
        }
    }

    #[test]
    fn test_parse_endpoint_relative_host_path() {
        match parse_endpoint("./local/file") {
            Endpoint::Host(p) => assert_eq!(p, "./local/file"),
            _ => panic!("Expected Host endpoint"),
        }
    }

    // --- Shell escape tests ---

    #[test]
    fn test_shell_escape_simple() {
        assert_eq!(shell_escape("/tmp/file"), "'/tmp/file'");
    }

    #[test]
    fn test_shell_escape_with_quotes() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn test_shell_escape_with_spaces() {
        assert_eq!(
            shell_escape("/path/with spaces/file"),
            "'/path/with spaces/file'"
        );
    }

    // --- Tar helper tests ---

    #[test]
    fn test_create_tar_from_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();

        // Create some test files
        std::fs::write(dir.join("file1.txt"), "hello").unwrap();
        std::fs::write(dir.join("file2.txt"), "world").unwrap();
        std::fs::create_dir(dir.join("subdir")).unwrap();
        std::fs::write(dir.join("subdir").join("nested.txt"), "nested").unwrap();

        let tar_data = create_tar_from_dir(dir.to_str().unwrap()).unwrap();
        assert!(!tar_data.is_empty());
    }

    #[test]
    fn test_create_and_extract_tar_roundtrip() {
        let src_dir = tempfile::TempDir::new().unwrap();
        let dst_dir = tempfile::TempDir::new().unwrap();

        // Create test content
        std::fs::write(src_dir.path().join("hello.txt"), "hello world").unwrap();
        std::fs::create_dir(src_dir.path().join("sub")).unwrap();
        std::fs::write(
            src_dir.path().join("sub").join("nested.txt"),
            "nested content",
        )
        .unwrap();

        // Tar and extract
        let tar_data = create_tar_from_dir(src_dir.path().to_str().unwrap()).unwrap();
        extract_tar_to_dir(&tar_data, dst_dir.path().to_str().unwrap()).unwrap();

        // Verify content
        let hello = std::fs::read_to_string(dst_dir.path().join("hello.txt")).unwrap();
        assert_eq!(hello, "hello world");

        let nested =
            std::fs::read_to_string(dst_dir.path().join("sub").join("nested.txt")).unwrap();
        assert_eq!(nested, "nested content");
    }

    #[test]
    fn test_create_tar_nonexistent_dir() {
        let result = create_tar_from_dir("/nonexistent/path/a3s_test_12345");
        assert!(result.is_err());
    }

    // --- Constant tests ---

    #[test]
    fn test_dir_transfer_timeout() {
        assert_eq!(DIR_TRANSFER_TIMEOUT_NS, 60_000_000_000);
    }
}
