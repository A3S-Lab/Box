//! Exact MicroVM shim discovery for managed restart recovery.

use std::path::Path;

use sysinfo::{Pid, System};

#[derive(Debug, Clone, Copy)]
pub(super) struct LocatedProcess {
    pub(super) pid: u32,
    pub(super) start_time: Option<u64>,
}

pub(super) fn locate_microvm_process(
    execution_id: &str,
    recorded: Option<(u32, Option<u64>)>,
) -> Result<Option<LocatedProcess>, String> {
    let system = System::new_all();

    if let Some((pid, expected_start_time)) = recorded {
        if !crate::process::is_process_alive_with_identity(pid, expected_start_time) {
            return Ok(None);
        }
        let Some(process) = system.process(Pid::from_u32(pid)) else {
            return Ok(None);
        };
        if !shim_command_targets_execution(process.cmd(), execution_id) {
            return Ok(None);
        }
        return Ok(Some(LocatedProcess {
            pid,
            start_time: crate::process::pid_start_time(pid),
        }));
    }

    let mut matches = system
        .processes()
        .values()
        .filter(|process| shim_command_targets_execution(process.cmd(), execution_id))
        .map(|process| LocatedProcess {
            pid: process.pid().as_u32(),
            start_time: crate::process::pid_start_time(process.pid().as_u32()),
        });
    let first = matches.next();
    if matches.next().is_some() {
        return Err(format!(
            "multiple MicroVM shim processes claim execution {execution_id}"
        ));
    }
    Ok(first)
}

fn shim_command_targets_execution(command: &[String], execution_id: &str) -> bool {
    let Some(executable) = command.first() else {
        return false;
    };
    let Some(file_name) = Path::new(executable)
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    if !matches!(file_name, "a3s-box-shim" | "a3s-box-shim.exe") {
        return false;
    }

    let Some(config_index) = command.iter().position(|argument| argument == "--config") else {
        return false;
    };
    let Some(config) = command.get(config_index + 1) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(config)
        .ok()
        .and_then(|value| {
            value
                .get("box_id")
                .and_then(serde_json::Value::as_str)
                .map(|box_id| box_id == execution_id)
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_match_requires_exact_binary_argument_and_internal_id() {
        let command = vec![
            "/usr/bin/a3s-box-shim".to_string(),
            "--config".to_string(),
            r#"{"box_id":"11111111-1111-4111-8111-111111111111"}"#.to_string(),
        ];

        assert!(shim_command_targets_execution(
            &command,
            "11111111-1111-4111-8111-111111111111"
        ));
        assert!(!shim_command_targets_execution(&command, "11111111"));
    }

    #[test]
    fn command_match_rejects_lookalikes_and_malformed_json() {
        let lookalike = vec![
            "/tmp/not-a3s-box-shim".to_string(),
            "--config".to_string(),
            r#"{"box_id":"box-1"}"#.to_string(),
        ];
        let malformed = vec![
            "a3s-box-shim".to_string(),
            "--config".to_string(),
            "not-json".to_string(),
        ];

        assert!(!shim_command_targets_execution(&lookalike, "box-1"));
        assert!(!shim_command_targets_execution(&malformed, "box-1"));
    }

    #[test]
    fn recorded_non_shim_process_is_never_accepted() {
        let located = locate_microvm_process(
            std::process::id().to_string().as_str(),
            Some((
                std::process::id(),
                crate::process::pid_start_time(std::process::id()),
            )),
        )
        .unwrap();
        assert!(located.is_none());
    }
}
