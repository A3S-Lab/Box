//! Shared policy for legacy socket and OCI SDK process sessions.

use std::collections::{BTreeMap, HashMap};

use a3s_box_core::{ExecutionGeneration, ExecutionId};
#[cfg(unix)]
use a3s_box_core::{ExecutionManagerError, ExecutionManagerResult, SecurityConfig};

use crate::BoxRecord;

pub(super) fn has_oci_runtime(record: &BoxRecord) -> bool {
    record
        .managed_execution
        .as_ref()
        .and_then(|metadata| metadata.oci_runtime.as_ref())
        .is_some()
}

/// Merge the environment fixed at Sandbox creation into an exec/PTY request.
///
/// The guest normally inherits these values from guest-init, but the execution
/// contract must not depend on which user a later request selects. Per-request
/// values remain authoritative, matching OCI/Docker exec environment semantics.
pub(super) fn inherit_container_environment(
    container: &HashMap<String, String>,
    request: &mut Vec<String>,
) {
    let mut merged: BTreeMap<String, String> = container
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let mut malformed = Vec::new();
    for entry in std::mem::take(request) {
        if let Some((key, value)) = entry.split_once('=') {
            merged.insert(key.to_string(), value.to_string());
        } else {
            malformed.push(entry);
        }
    }
    request.extend(
        merged
            .into_iter()
            .map(|(key, value)| format!("{key}={value}")),
    );
    request.extend(malformed);
}

/// Apply the creation-time security policy to a legacy guest exec/PTY request.
///
/// A MicroVM's guest-init control plane is intentionally more privileged than
/// its workloads. Every later process must therefore receive the same private
/// `A3S_SEC_*` controls as the main process. Request and container environment
/// entries cannot override these runtime-owned values, and the guest filters
/// them before the workload observes its environment.
#[cfg(unix)]
pub(super) fn inherit_execution_security_environment(
    record: &BoxRecord,
    request: &mut Vec<String>,
) -> ExecutionManagerResult<()> {
    let security = if let Some(metadata) = record.managed_execution.as_ref() {
        let config = &metadata.request.config;
        SecurityConfig::from_options(
            &config.security_opt,
            &config.cap_add,
            &config.cap_drop,
            config.privileged,
        )
    } else {
        SecurityConfig::from_options(
            &record.security_opt,
            &record.cap_add,
            &record.cap_drop,
            record.privileged,
        )
    };
    security
        .validate()
        .map_err(ExecutionManagerError::InvalidRequest)?;

    let mut merged = BTreeMap::new();
    let mut malformed = Vec::new();
    for entry in std::mem::take(request) {
        if let Some((key, value)) = entry.split_once('=') {
            if !key.starts_with("A3S_SEC_") {
                merged.insert(key.to_string(), value.to_string());
            }
        } else {
            malformed.push(entry);
        }
    }
    for (key, value) in security.to_env_vars() {
        merged.insert(key, value);
    }
    request.extend(
        merged
            .into_iter()
            .map(|(key, value)| format!("{key}={value}")),
    );
    request.extend(malformed);
    Ok(())
}

/// Record only environment key names at the final host-to-runtime boundary.
pub(super) fn debug_session_environment(
    execution_id: &ExecutionId,
    generation: ExecutionGeneration,
    operation: &str,
    container: &HashMap<String, String>,
    request: &[String],
) {
    let mut container_keys: Vec<&str> = container.keys().map(String::as_str).collect();
    container_keys.sort_unstable();
    let mut request_keys: Vec<&str> = request
        .iter()
        .filter_map(|entry| entry.split_once('=').map(|(key, _)| key))
        .collect();
    request_keys.sort_unstable();
    request_keys.dedup();
    let malformed_request_entries = request.iter().filter(|entry| !entry.contains('=')).count();

    tracing::debug!(
        %execution_id,
        generation = generation.get(),
        operation,
        container_env_count = container.len(),
        container_env_keys = ?container_keys,
        merged_request_env_count = request.len(),
        merged_request_env_keys = ?request_keys,
        malformed_request_entries,
        "Prepared managed execution session environment"
    );
}

#[cfg(test)]
mod tests {
    use super::inherit_container_environment;
    use std::collections::HashMap;

    #[test]
    fn request_environment_overrides_inherited_container_values() {
        let container = HashMap::from([
            ("ALPHA".to_string(), "container".to_string()),
            ("BETA".to_string(), "container".to_string()),
        ]);
        let mut request = vec!["BETA=request".to_string(), "GAMMA=request".to_string()];

        inherit_container_environment(&container, &mut request);

        assert_eq!(
            request,
            ["ALPHA=container", "BETA=request", "GAMMA=request"]
        );
    }

    #[test]
    fn malformed_request_entries_are_preserved_after_inherited_values() {
        let container = HashMap::from([("ALPHA".to_string(), "container".to_string())]);
        let mut request = vec!["MALFORMED".to_string()];

        inherit_container_environment(&container, &mut request);

        assert_eq!(request, ["ALPHA=container", "MALFORMED"]);
    }
}
