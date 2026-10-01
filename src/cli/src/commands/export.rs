//! `a3s-box export` command — Export a box's filesystem to a tar archive.

use a3s_box_core::error::BoxError;
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

pub async fn execute(args: ExportArgs) -> Result<(), BoxError> {
    let initial_state = StateFile::load_default()?;
    let box_id = resolve::resolve(&initial_state, &args.name)
        .map_err(super::IntoBoxError::into_box_error)?
        .id
        .clone();
    let lifecycle_lock = crate::lifecycle::acquire_box_lifecycle_lock(&box_id).await?;
    let state = StateFile::load_default()?;
    let record = state.find_by_id(&box_id).ok_or_else(|| {
        BoxError::StateError(format!(
            "Box '{}' was removed while waiting for its lifecycle lock",
            args.name
        ))
    })?;

    if uses_live_sandbox_host_rootfs(record) {
        // Managed Sandbox pause/resume uses the same lifecycle lock; release the
        // CLI guard before host-rootfs capture so generation fencing stays exclusive.
        drop(lifecycle_lock);
        export_live_sandbox_host(record, &args.output).await?;
    } else if record.status == "running" {
        export_live_guest(record, &args.output).await?;
        drop(lifecycle_lock);
    } else if record.status == "paused" {
        return Err(BoxError::StateError(format!(
            "Cannot export paused MicroVM box '{}'; resume it first, or use a Sandbox",
            record.name
        )));
    } else {
        if super::rootfs_capture::stopped_sandbox_uses_managed_host_rootfs(record) {
            drop(lifecycle_lock);
            export_stopped_sandbox_host(record, &args.output).await?;
        } else if a3s_box_runtime::rootfs::guest_native_ext4_generation_exists(&record.box_dir)? {
            let output = std::path::Path::new(&args.output);
            super::commit::refuse_archive_ancestor_reparse(output)?;
            let mut file = tokio::fs::File::create(output).await.map_err(|error| {
                super::io_error(format!("Failed to create {}", args.output), error)
            })?;
            super::rootfs_capture::archive_stopped_guest_native_rootfs(record, &mut file).await?;
            file.sync_all().await?;
            drop(lifecycle_lock);
        } else {
            let rootfs_dir = super::resolve_box_rootfs(&record.box_dir).ok_or_else(|| {
                BoxError::StateError(rootfs_not_found_message(&args.name, &record.box_dir))
            })?;
            // Match stopped commit: fail closed without guest rootfs metadata so
            // NTFS host mode/ownership is never archived as guest filesystem truth.
            let rootfs_metadata =
                super::commit::read_guest_rootfs_metadata(&rootfs_dir).map_err(|error| {
                    BoxError::StateError(format!(
                        "Cannot export stopped box '{}': {error}",
                        record.name
                    ))
                })?;
            super::commit::create_tar_from_guest_metadata(
                &rootfs_dir,
                &rootfs_metadata,
                std::path::Path::new(&args.output),
            )?;
            drop(lifecycle_lock);
        }
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
) -> Result<(), BoxError> {
    let live_pid = record.pid.is_some_and(|pid| {
        crate::process::is_process_alive_with_identity(pid, record.pid_start_time)
    });
    if !live_pid {
        return Err(BoxError::StateError(format!(
            "Cannot export box '{}' because its host process is not live",
            record.name
        )));
    }
    // Quiesce via managed pause when Running; already-Paused captures in place.
    super::commit::capture_live_host_rootfs_tar(record, std::path::Path::new(output), true).await
}

#[cfg(not(all(unix, target_os = "linux")))]
async fn export_live_sandbox_host(
    record: &crate::state::BoxRecord,
    _output: &str,
) -> Result<(), BoxError> {
    Err(BoxError::ConfigError(format!(
        "Live Sandbox host-rootfs export is unavailable for box '{}' on this platform",
        record.name
    )))
}

#[cfg(all(unix, target_os = "linux"))]
async fn export_stopped_sandbox_host(
    record: &crate::state::BoxRecord,
    output: &str,
) -> Result<(), BoxError> {
    super::rootfs_capture::ensure_stopped_rootfs_is_unowned(record)?;
    // Already stopped: capture OCI-mapped host rootfs without pause/resume.
    super::commit::capture_live_host_rootfs_tar(record, std::path::Path::new(output), false).await
}

#[cfg(not(all(unix, target_os = "linux")))]
async fn export_stopped_sandbox_host(
    record: &crate::state::BoxRecord,
    _output: &str,
) -> Result<(), BoxError> {
    Err(BoxError::ConfigError(format!(
        "Stopped Sandbox host-rootfs export is unavailable for box '{}' on this platform",
        record.name
    )))
}

#[cfg(unix)]
async fn export_live_guest(record: &crate::state::BoxRecord, output: &str) -> Result<(), BoxError> {
    let live_pid = record.pid.is_some_and(|pid| {
        crate::process::is_process_alive_with_identity(pid, record.pid_start_time)
    });
    if !live_pid {
        return Err(BoxError::StateError(format!(
            "Cannot export running box '{}' because its host process is not live",
            record.name
        )));
    }
    if !record.exec_socket_path.exists() {
        return Err(BoxError::StateError(format!(
            "Cannot export running box '{}' because its guest archive endpoint is unavailable",
            record.name
        )));
    }
    let output = std::path::Path::new(output);
    super::commit::refuse_archive_ancestor_reparse(output)?;
    let client = a3s_box_runtime::ExecClient::connect(&record.exec_socket_path).await?;
    let mut file = tokio::fs::File::create(output)
        .await
        .map_err(|error| super::io_error(format!("Failed to create {output}"), error))?;
    let written = client.archive_rootfs(&mut file, true).await?;
    if written == 0 {
        return Err(BoxError::ExecError(
            "Guest rootfs archive was empty".to_string(),
        ));
    }
    file.sync_all().await?;
    Ok(())
}

#[cfg(not(unix))]
async fn export_live_guest(
    record: &crate::state::BoxRecord,
    _output: &str,
) -> Result<(), BoxError> {
    Err(BoxError::ConfigError(format!(
        "Live filesystem export is unavailable for box '{}' on this platform",
        record.name
    )))
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

    #[cfg(not(all(unix, target_os = "linux")))]
    #[tokio::test]
    async fn live_sandbox_host_export_is_a_configuration_error() {
        let record = crate::test_helpers::fixtures::make_record("id", "web", "running", Some(1));
        let error = export_live_sandbox_host(&record, "web.tar")
            .await
            .unwrap_err();
        match error {
            BoxError::ConfigError(message) => {
                assert!(
                    message.contains("Live Sandbox host-rootfs export is unavailable"),
                    "{message}"
                );
                assert!(message.contains("web"), "{message}");
            }
            other => panic!("expected ConfigError, got {other:?}"),
        }
    }
}
