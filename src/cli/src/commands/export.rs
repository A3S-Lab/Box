//! `a3s-box export` command — Export a box's filesystem to a tar archive.

use clap::Args;

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct ExportArgs {
    /// Box name or ID to export
    pub name: String,

    /// Output file path (e.g., "mybox.tar")
    #[arg(short, long)]
    pub output: String,
}

pub async fn execute(args: ExportArgs) -> Result<(), Box<dyn std::error::Error>> {
    let initial_state = StateFile::load_default()?;
    let box_id = resolve::resolve(&initial_state, &args.name)?.id.clone();
    let lifecycle_lock = crate::lifecycle::acquire_box_lifecycle_lock(&box_id).await?;
    let state = StateFile::load_default()?;
    let record = state.find_by_id(&box_id).ok_or_else(|| {
        format!(
            "Box '{}' was removed while waiting for its lifecycle lock",
            args.name
        )
    })?;

    if uses_live_sandbox_host_rootfs(&record) {
        // Managed Sandbox pause/resume uses the same lifecycle lock; release the
        // CLI guard before host-rootfs capture so generation fencing stays exclusive.
        drop(lifecycle_lock);
        export_live_sandbox_host(record, &args.output).await?;
    } else if record.status == "running" {
        export_live_guest(record, &args.output).await?;
        drop(lifecycle_lock);
    } else if record.status == "paused" {
        return Err(format!(
            "Cannot export paused MicroVM box '{}'; resume it first, or use a Sandbox",
            record.name
        )
        .into());
    } else {
        if a3s_box_runtime::rootfs::guest_native_ext4_generation_exists(&record.box_dir)? {
            let mut file = tokio::fs::File::create(&args.output)
                .await
                .map_err(|error| format!("Failed to create {}: {error}", args.output))?;
            super::rootfs_capture::archive_stopped_guest_native_rootfs(record, &mut file).await?;
            file.sync_all().await?;
        } else {
            let rootfs_dir = super::resolve_box_rootfs(&record.box_dir)
                .ok_or_else(|| rootfs_not_found_message(&args.name, &record.box_dir))?;
            let file = std::fs::File::create(&args.output)
                .map_err(|e| format!("Failed to create {}: {e}", args.output))?;

            let mut builder = tar::Builder::new(file);
            builder.follow_symlinks(false);
            builder
                .append_dir_all(".", &rootfs_dir)
                .map_err(|e| format!("Failed to archive filesystem: {e}"))?;
            builder
                .finish()
                .map_err(|e| format!("Failed to finalize archive: {e}"))?;
        }
        drop(lifecycle_lock);
    }

    let size = std::fs::metadata(&args.output)
        .map(|m| m.len())
        .unwrap_or(0);

    println!("{}", export_success_line(&args.name, &args.output, size));
    Ok(())
}

/// SandboxViaOci host-rootfs capture works while Running or freezer-Paused
/// (same quiesce surface as live commit/snapshot). MicroVM guest archives stay
/// Running-only.
fn uses_live_sandbox_host_rootfs(record: &crate::state::BoxRecord) -> bool {
    record.isolation.is_sandbox() && matches!(record.status.as_str(), "running" | "paused")
}

#[cfg(all(unix, target_os = "linux"))]
async fn export_live_sandbox_host(
    record: &crate::state::BoxRecord,
    output: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let live_pid = record.pid.is_some_and(|pid| {
        crate::process::is_process_alive_with_identity(pid, record.pid_start_time)
    });
    if !live_pid {
        return Err(format!(
            "Cannot export box '{}' because its host process is not live",
            record.name
        )
        .into());
    }
    // Quiesce via managed pause when Running; already-Paused captures in place.
    super::commit::capture_live_host_rootfs_tar(record, std::path::Path::new(output), true).await
}

#[cfg(not(all(unix, target_os = "linux")))]
async fn export_live_sandbox_host(
    record: &crate::state::BoxRecord,
    _output: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    Err(format!(
        "Live Sandbox host-rootfs export is unavailable for box '{}' on this platform",
        record.name
    )
    .into())
}

#[cfg(unix)]
async fn export_live_guest(
    record: &crate::state::BoxRecord,
    output: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let live_pid = record.pid.is_some_and(|pid| {
        crate::process::is_process_alive_with_identity(pid, record.pid_start_time)
    });
    if !live_pid {
        return Err(format!(
            "Cannot export running box '{}' because its host process is not live",
            record.name
        )
        .into());
    }
    if !record.exec_socket_path.exists() {
        return Err(format!(
            "Cannot export running box '{}' because its guest archive endpoint is unavailable",
            record.name
        )
        .into());
    }
    let client = a3s_box_runtime::ExecClient::connect(&record.exec_socket_path).await?;
    let mut file = tokio::fs::File::create(output)
        .await
        .map_err(|error| format!("Failed to create {output}: {error}"))?;
    let written = client.archive_rootfs(&mut file, true).await?;
    if written == 0 {
        return Err("Guest rootfs archive was empty".into());
    }
    file.sync_all().await?;
    Ok(())
}

#[cfg(not(unix))]
async fn export_live_guest(
    record: &crate::state::BoxRecord,
    _output: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    Err(format!(
        "Live filesystem export is unavailable for box '{}' on this platform",
        record.name
    )
    .into())
}

fn rootfs_not_found_message(name: &str, box_dir: &std::path::Path) -> String {
    format!(
        "Rootfs not found for box '{}' under {} (looked for merged/ and rootfs/). \
         For overlay-backed boxes the filesystem is only available while the box exists; \
         export a running box.",
        name,
        box_dir.display()
    )
}

fn export_success_line(name: &str, output: &str, size: u64) -> String {
    format!(
        "Exported {} to {} ({})",
        name,
        output,
        crate::output::format_bytes(size)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn rootfs_not_found_message_mentions_box_path_and_expected_dirs() {
        let message = rootfs_not_found_message("web", Path::new("/tmp/a3s/boxes/web"));

        assert!(message.contains("Rootfs not found for box 'web'"));
        assert!(message.contains("/tmp/a3s/boxes/web"));
        assert!(message.contains("merged/ and rootfs/"));
        assert!(message.contains("export a running box"));
    }

    #[test]
    fn export_success_line_formats_archive_size() {
        assert_eq!(
            export_success_line("web", "web.tar", 1536),
            "Exported web to web.tar (1.5 KB)"
        );
    }

    #[test]
    fn live_sandbox_host_rootfs_accepts_running_and_paused() {
        let mut running =
            crate::test_helpers::fixtures::make_record("id", "box", "running", Some(1));
        running.isolation = a3s_box_core::ExecutionIsolation::Sandbox;
        let mut paused = crate::test_helpers::fixtures::make_record("id", "box", "paused", Some(1));
        paused.isolation = a3s_box_core::ExecutionIsolation::Sandbox;
        let mut stopped = crate::test_helpers::fixtures::make_record("id", "box", "stopped", None);
        stopped.isolation = a3s_box_core::ExecutionIsolation::Sandbox;
        let mut microvm_paused =
            crate::test_helpers::fixtures::make_record("id", "box", "paused", Some(1));
        microvm_paused.isolation = a3s_box_core::ExecutionIsolation::Microvm;

        assert!(uses_live_sandbox_host_rootfs(&running));
        assert!(uses_live_sandbox_host_rootfs(&paused));
        assert!(!uses_live_sandbox_host_rootfs(&stopped));
        assert!(!uses_live_sandbox_host_rootfs(&microvm_paused));
    }
}
