//! Shared stopped guest-native rootfs capture through the maintenance VM.

use a3s_box_core::error::BoxError;

/// Stopped managed Sandbox product ops must use OCI-mapped host-rootfs metadata,
/// not host subordinate UIDs from a bare directory walk.
pub(crate) fn stopped_sandbox_uses_managed_host_rootfs(record: &crate::state::BoxRecord) -> bool {
    record.isolation.is_sandbox() && record.managed_execution.is_some()
}

pub(crate) async fn archive_stopped_guest_native_rootfs<W>(
    record: &crate::state::BoxRecord,
    output: &mut W,
) -> Result<u64, BoxError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    ensure_stopped_rootfs_is_unowned(record)?;
    let config = crate::boot::config_from_record(record).map_err(BoxError::ConfigError)?;
    let written =
        a3s_box_runtime::archive_stopped_guest_native_rootfs(config, record.id.clone(), output)
            .await?;
    if written == 0 {
        return Err(BoxError::ExecError(
            "Guest rootfs maintenance archive was empty".into(),
        ));
    }
    Ok(written)
}

/// Verify that an offline reader cannot race a live or transitional VM owner.
///
/// The lifecycle lock serializes well-behaved CLI operations; this explicit
/// state and PID fence also fails closed for stale records and direct callers.
pub(crate) fn ensure_stopped_rootfs_is_unowned(
    record: &crate::state::BoxRecord,
) -> Result<(), BoxError> {
    if !matches!(
        record.status.as_str(),
        "created" | "stopped" | "dead" | "failed"
    ) {
        return Err(BoxError::StateError(format!(
            "Cannot inspect box '{}' offline while its lifecycle state is {}",
            record.name, record.status
        )));
    }
    if record.pid.is_some_and(|pid| {
        crate::process::is_process_alive_with_identity(pid, record.pid_start_time)
    }) {
        return Err(BoxError::StateError(format!(
            "Cannot inspect box '{}' offline because its host process is still live",
            record.name
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::make_record;

    #[test]
    fn offline_rootfs_ownership_rejects_transitional_state_and_live_pid() {
        let paused = make_record("id", "box", "paused", None);
        match ensure_stopped_rootfs_is_unowned(&paused) {
            Err(BoxError::StateError(message)) => {
                assert!(message.contains("paused"), "{message}");
            }
            other => panic!("expected StateError, got {other:?}"),
        }

        let stopped_but_live = make_record("id", "box", "stopped", Some(std::process::id()));
        match ensure_stopped_rootfs_is_unowned(&stopped_but_live) {
            Err(BoxError::StateError(message)) => {
                assert!(message.contains("still live"), "{message}");
            }
            other => panic!("expected StateError, got {other:?}"),
        }

        let stopped = make_record("id", "box", "stopped", None);
        ensure_stopped_rootfs_is_unowned(&stopped).unwrap();
    }
}
