//! Runtime planning and launch-time revalidation for optional host bind policy.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use a3s_box_core::{
    resolve_execution, resolve_execution_with_host_mounts, BoxError, CreateExecutionRequest,
    HostBindMount, HostMountPolicyEvaluator, ResolvedExecutionPlan, ResolvedHostMount, Result,
};

pub(crate) fn resolve_managed_execution_plan(
    home_dir: &Path,
    request: &CreateExecutionRequest,
) -> Result<ResolvedExecutionPlan> {
    let Some(host_policy) = request
        .config
        .security_policy
        .as_ref()
        .and_then(|policy| policy.host_mount_policy())
    else {
        return resolve_execution(&request.config);
    };

    let evaluator = HostMountPolicyEvaluator::for_host(host_policy)?;
    let volume_mounts = request
        .config
        .volumes
        .iter()
        .map(|value| HostBindMount::parse(value))
        .collect::<Result<Vec<_>>>()?;
    let workspace_mount = if request.config.workspace.as_os_str().is_empty() {
        None
    } else {
        if !request.config.workspace.is_absolute() {
            return Err(BoxError::ConfigError(format!(
                "host mount policy requires an absolute workspace path: {}",
                request.config.workspace.display()
            )));
        }
        Some(HostBindMount {
            source: request.config.workspace.clone(),
            destination: "/workspace".to_string(),
            read_only: false,
        })
    };
    let mut all_mounts = volume_mounts.clone();
    all_mounts.extend(workspace_mount.iter().cloned());
    validate_destinations(&all_mounts)?;

    let named_sources = named_volume_sources(home_dir, &request.policy.volume_names)?;
    let mut seen_named_sources = HashSet::new();
    let mut resolved = Vec::new();
    for mut mount in volume_mounts {
        let canonical = canonical_bind_source(&mount)?;
        if named_sources.contains(&canonical) {
            seen_named_sources.insert(canonical);
            continue;
        }
        mount.source = canonical;
        let assessment = evaluator.evaluate(&mount.source)?;
        resolved.push(ResolvedHostMount::new(mount, assessment)?);
    }

    if let Some(mut mount) = workspace_mount {
        mount.source = canonical_bind_source(&mount)?;
        let assessment = evaluator.evaluate(&mount.source)?;
        resolved.push(ResolvedHostMount::new(mount, assessment)?);
    }
    let missing_named_sources = named_sources
        .difference(&seen_named_sources)
        .cloned()
        .collect::<HashSet<_>>();
    if !missing_named_sources.is_empty() {
        return Err(BoxError::ConfigError(format!(
            "managed volume policy contains mount points not present in the request: {}",
            display_paths(&missing_named_sources)
        )));
    }
    resolved.sort_by(|left, right| {
        left.destination
            .cmp(&right.destination)
            .then_with(|| left.source.cmp(&right.source))
    });

    resolve_execution_with_host_mounts(&request.config, resolved)
}

pub(crate) fn validate_runtime_mount_configuration(
    plan: &ResolvedExecutionPlan,
    config: &a3s_box_core::BoxConfig,
    managed_volume_sources: &HashSet<PathBuf>,
) -> Result<()> {
    let host_policy_enabled = plan
        .security_policy
        .as_ref()
        .and_then(|policy| policy.host_mounts.as_ref())
        .is_some();
    if !host_policy_enabled {
        if plan.host_mounts.is_empty() {
            return Ok(());
        }
        return Err(BoxError::StateError(
            "execution plan has host mount evidence without a host mount policy".to_string(),
        ));
    }

    revalidate_host_mount_plan(plan)?;

    let volume_mounts = config
        .volumes
        .iter()
        .map(|value| HostBindMount::parse(value))
        .collect::<Result<Vec<_>>>()?;
    let workspace_mount = (!config.workspace.as_os_str().is_empty()).then(|| HostBindMount {
        source: config.workspace.clone(),
        destination: "/workspace".to_string(),
        read_only: false,
    });
    let mut all_mounts = volume_mounts.clone();
    all_mounts.extend(workspace_mount.iter().cloned());
    validate_destinations(&all_mounts)?;

    let mut seen_managed_sources = HashSet::new();
    let mut seen_host_destinations = HashSet::new();
    for mount in volume_mounts {
        let canonical = canonical_bind_source(&mount)?;
        if managed_volume_sources.contains(&canonical) {
            seen_managed_sources.insert(canonical);
            continue;
        }
        validate_runtime_host_mount(plan, &canonical, &mount.destination, mount.read_only)?;
        seen_host_destinations.insert(mount.destination);
    }
    if let Some(mount) = workspace_mount {
        let canonical = canonical_bind_source(&mount)?;
        validate_runtime_host_mount(plan, &canonical, &mount.destination, mount.read_only)?;
        seen_host_destinations.insert(mount.destination);
    }

    let missing_managed_sources = managed_volume_sources
        .difference(&seen_managed_sources)
        .cloned()
        .collect::<HashSet<_>>();
    if !missing_managed_sources.is_empty() {
        return Err(BoxError::StateError(format!(
            "managed volume provenance is missing from the runtime configuration: {}",
            display_paths(&missing_managed_sources)
        )));
    }
    let missing_host_destinations = plan
        .host_mounts
        .iter()
        .filter(|mount| !seen_host_destinations.contains(&mount.destination))
        .map(|mount| mount.destination.clone())
        .collect::<Vec<_>>();
    if !missing_host_destinations.is_empty() {
        return Err(BoxError::StateError(format!(
            "planned host mounts are missing from the runtime configuration: {}",
            missing_host_destinations.join(", ")
        )));
    }
    Ok(())
}

pub(crate) fn revalidate_host_mount_plan(plan: &ResolvedExecutionPlan) -> Result<()> {
    let Some(host_policy) = plan
        .security_policy
        .as_ref()
        .and_then(|policy| policy.host_mounts.as_ref())
    else {
        if plan.host_mounts.is_empty() {
            return Ok(());
        }
        return Err(BoxError::StateError(
            "execution plan has host mount evidence without a host mount policy".to_string(),
        ));
    };
    let evaluator = HostMountPolicyEvaluator::for_host(host_policy)?;
    for mount in &plan.host_mounts {
        evaluator.revalidate(&mount.assessment)?.ensure_allowed()?;
    }
    Ok(())
}

pub(crate) fn validate_runtime_host_mount(
    plan: &ResolvedExecutionPlan,
    source: &Path,
    destination: &str,
    read_only: bool,
) -> Result<()> {
    let Some(expected) = plan
        .host_mounts
        .iter()
        .find(|mount| mount.destination == destination)
    else {
        return Err(BoxError::ConfigError(format!(
            "unplanned host mount at {destination}: {}",
            source.display()
        )));
    };
    if source != expected.source || read_only != expected.read_only {
        return Err(BoxError::ConfigError(format!(
            "host mount changed after planning for {destination}: expected {} ({}), found {} ({})",
            expected.source.display(),
            access_mode(expected.read_only),
            source.display(),
            access_mode(read_only)
        )));
    }
    Ok(())
}

fn canonical_bind_source(mount: &HostBindMount) -> Result<PathBuf> {
    if !mount.source.is_absolute() {
        return Err(BoxError::ConfigError(format!(
            "host mount policy requires an absolute bind source: {}",
            mount.source.display()
        )));
    }
    mount.source.canonicalize().map_err(|error| {
        BoxError::ConfigError(format!(
            "host mount policy cannot resolve {}: {error}",
            mount.source.display()
        ))
    })
}

pub(crate) fn named_volume_sources(home_dir: &Path, names: &[String]) -> Result<HashSet<PathBuf>> {
    if names.is_empty() {
        return Ok(HashSet::new());
    }
    let volumes_root = home_dir.join("volumes").canonicalize().map_err(|error| {
        BoxError::ConfigError(format!(
            "host mount policy cannot resolve managed volume root {}: {error}",
            home_dir.join("volumes").display()
        ))
    })?;
    let mut sources = HashSet::new();
    for name in names {
        let mut components = Path::new(name).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            return Err(BoxError::ConfigError(format!(
                "invalid managed volume name in execution policy: {name:?}"
            )));
        }
        let source = volumes_root.join(name).canonicalize().map_err(|error| {
            BoxError::ConfigError(format!(
                "host mount policy cannot resolve managed volume {name:?}: {error}"
            ))
        })?;
        if source.parent() != Some(volumes_root.as_path()) {
            return Err(BoxError::ConfigError(format!(
                "managed volume {name:?} resolves outside {}",
                volumes_root.display()
            )));
        }
        sources.insert(source);
    }
    Ok(sources)
}

fn validate_destinations(mounts: &[HostBindMount]) -> Result<()> {
    for (index, left) in mounts.iter().enumerate() {
        let left_components = guest_components(&left.destination);
        for right in mounts.iter().skip(index + 1) {
            let right_components = guest_components(&right.destination);
            if is_component_prefix(&left_components, &right_components)
                || is_component_prefix(&right_components, &left_components)
            {
                return Err(BoxError::ConfigError(format!(
                    "host mount policy rejects duplicate or overlapping destinations {} and {}",
                    left.destination, right.destination
                )));
            }
        }
    }
    Ok(())
}

fn guest_components(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|component| !component.is_empty())
        .collect()
}

fn is_component_prefix(left: &[&str], right: &[&str]) -> bool {
    left.len() <= right.len() && left.iter().zip(right).all(|(left, right)| left == right)
}

fn display_paths(paths: &HashSet<PathBuf>) -> String {
    let mut paths = paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    paths.sort();
    paths.join(", ")
}

const fn access_mode(read_only: bool) -> &'static str {
    if read_only {
        "read-only"
    } else {
        "read-write"
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use a3s_box_core::{
        BoxConfig, ExecutionRecordPolicy, HostMountOutcome, HostMountPolicy, SandboxSecurityPolicy,
    };

    use super::*;

    fn request(config: BoxConfig, volume_names: Vec<String>) -> CreateExecutionRequest {
        CreateExecutionRequest {
            external_sandbox_id: "host-mount-policy-test".to_string(),
            config,
            labels: BTreeMap::new(),
            policy: ExecutionRecordPolicy {
                volume_names,
                ..ExecutionRecordPolicy::default()
            },
            rootfs_snapshot_id: None,
        }
    }

    #[test]
    fn planning_classifies_binds_but_excludes_managed_volumes() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path();
        let managed = home.join("volumes/cache");
        let bind = fixture.path().join("source");
        std::fs::create_dir_all(&managed).unwrap();
        std::fs::create_dir_all(&bind).unwrap();
        std::fs::write(managed.join(".env"), b"managed data").unwrap();
        let config = BoxConfig {
            volumes: vec![
                format!("{}:/cache:rw", managed.display()),
                format!("{}:/workspace:ro", bind.display()),
            ],
            security_policy: Some(
                SandboxSecurityPolicy::new().host_mounts(HostMountPolicy::agent_safe()),
            ),
            ..BoxConfig::default()
        };

        let plan =
            resolve_managed_execution_plan(home, &request(config, vec!["cache".to_string()]))
                .unwrap();

        assert_eq!(plan.host_mounts.len(), 1);
        assert_eq!(plan.host_mounts[0].destination, "/workspace");
        assert_eq!(plan.host_mounts[0].source, bind.canonicalize().unwrap());
    }

    #[test]
    fn one_named_volume_can_be_mounted_at_multiple_destinations() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path();
        let managed = home.join("volumes/cache");
        std::fs::create_dir_all(&managed).unwrap();
        std::fs::write(managed.join(".env"), b"managed data").unwrap();
        let config = BoxConfig {
            volumes: vec![
                format!("{}:/cache:rw", managed.display()),
                format!("{}:/backup-cache:ro", managed.display()),
            ],
            security_policy: Some(
                SandboxSecurityPolicy::new().host_mounts(HostMountPolicy::agent_safe()),
            ),
            ..BoxConfig::default()
        };
        let request = request(config.clone(), vec!["cache".to_string()]);

        let plan = resolve_managed_execution_plan(home, &request).unwrap();
        let managed_sources = named_volume_sources(home, &request.policy.volume_names).unwrap();

        assert!(plan.host_mounts.is_empty());
        validate_runtime_mount_configuration(&plan, &config, &managed_sources).unwrap();
    }

    #[test]
    fn missing_named_volume_provenance_fails_closed() {
        let fixture = tempfile::tempdir().unwrap();
        let managed = fixture.path().join("volumes/cache");
        let bind = fixture.path().join("bind");
        std::fs::create_dir_all(&managed).unwrap();
        std::fs::create_dir_all(&bind).unwrap();
        let config = BoxConfig {
            volumes: vec![format!("{}:/workspace:rw", bind.display())],
            security_policy: Some(
                SandboxSecurityPolicy::new().host_mounts(HostMountPolicy::agent_safe()),
            ),
            ..BoxConfig::default()
        };

        let error = resolve_managed_execution_plan(
            fixture.path(),
            &request(config, vec!["cache".to_string()]),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("mount points not present"));
    }

    #[test]
    fn explicit_workspace_is_never_mistaken_for_named_volume_provenance() {
        let fixture = tempfile::tempdir().unwrap();
        let managed = fixture.path().join("volumes/cache");
        std::fs::create_dir_all(&managed).unwrap();
        std::fs::write(
            managed.join(".env"),
            b"sensitive if exported as a workspace",
        )
        .unwrap();
        let config = BoxConfig {
            workspace: managed,
            security_policy: Some(
                SandboxSecurityPolicy::new().host_mounts(HostMountPolicy::agent_safe()),
            ),
            ..BoxConfig::default()
        };

        let error = resolve_managed_execution_plan(
            fixture.path(),
            &request(config, vec!["cache".to_string()]),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("host mount policy denied"));
    }

    #[test]
    fn planning_rejects_sensitive_bind_before_record_creation() {
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join(".env"), b"TOKEN=secret").unwrap();
        let config = BoxConfig {
            volumes: vec![format!("{}:/workspace:rw", source.display())],
            security_policy: Some(
                SandboxSecurityPolicy::new().host_mounts(HostMountPolicy::agent_safe()),
            ),
            ..BoxConfig::default()
        };

        let error = resolve_managed_execution_plan(fixture.path(), &request(config, Vec::new()))
            .unwrap_err()
            .to_string();

        assert!(error.contains("host mount policy denied"));
    }

    #[test]
    fn audit_findings_are_persisted_without_enforcement_claim() {
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("source");
        std::fs::create_dir_all(source.join(".aws")).unwrap();
        let config = BoxConfig {
            volumes: vec![format!("{}:/workspace:rw", source.display())],
            security_policy: Some(
                SandboxSecurityPolicy::new()
                    .host_mounts(HostMountPolicy::agent_safe().audit_only()),
            ),
            ..BoxConfig::default()
        };

        let plan =
            resolve_managed_execution_plan(fixture.path(), &request(config, Vec::new())).unwrap();

        assert_eq!(
            plan.host_mounts[0].assessment.outcome,
            HostMountOutcome::Audit
        );
        assert!(plan
            .required_controls
            .contains(&"host-mount-policy-audit".to_string()));
    }

    #[test]
    fn duplicate_and_overlapping_destinations_are_rejected() {
        let fixture = tempfile::tempdir().unwrap();
        let first = fixture.path().join("first");
        let second = fixture.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        for destinations in [
            ("/workspace", "/workspace"),
            ("/workspace", "/workspace/nested"),
        ] {
            let config = BoxConfig {
                volumes: vec![
                    format!("{}:{}:rw", first.display(), destinations.0),
                    format!("{}:{}:rw", second.display(), destinations.1),
                ],
                security_policy: Some(
                    SandboxSecurityPolicy::new().host_mounts(HostMountPolicy::agent_safe()),
                ),
                ..BoxConfig::default()
            };
            let error =
                resolve_managed_execution_plan(fixture.path(), &request(config, Vec::new()))
                    .unwrap_err()
                    .to_string();
            assert!(error.contains("duplicate or overlapping"));
        }
    }

    #[test]
    fn launch_revalidation_detects_source_replacement() {
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        let config = BoxConfig {
            volumes: vec![format!("{}:/workspace:rw", source.display())],
            security_policy: Some(
                SandboxSecurityPolicy::new().host_mounts(HostMountPolicy::agent_safe()),
            ),
            ..BoxConfig::default()
        };
        let plan =
            resolve_managed_execution_plan(fixture.path(), &request(config, Vec::new())).unwrap();
        std::fs::rename(&source, fixture.path().join("old-source")).unwrap();
        std::fs::create_dir(&source).unwrap();

        let error = revalidate_host_mount_plan(&plan).unwrap_err().to_string();
        assert!(error.contains("identity changed"));
    }

    #[cfg(unix)]
    #[test]
    fn raw_symlink_swap_is_detected_even_when_the_planned_target_is_unchanged() {
        let fixture = tempfile::tempdir().unwrap();
        let first = fixture.path().join("first");
        let second = fixture.path().join("second");
        let source = fixture.path().join("source-link");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::os::unix::fs::symlink(&first, &source).unwrap();
        let config = BoxConfig {
            volumes: vec![format!("{}:/workspace:ro", source.display())],
            security_policy: Some(
                SandboxSecurityPolicy::new().host_mounts(HostMountPolicy::agent_safe()),
            ),
            ..BoxConfig::default()
        };
        let plan =
            resolve_managed_execution_plan(fixture.path(), &request(config.clone(), Vec::new()))
                .unwrap();
        std::fs::remove_file(&source).unwrap();
        std::os::unix::fs::symlink(&second, &source).unwrap();

        // Revalidating only the canonical target cannot observe a changed raw
        // symlink. The runtime request-to-plan comparison must catch it.
        revalidate_host_mount_plan(&plan).unwrap();
        let error = validate_runtime_mount_configuration(&plan, &config, &HashSet::new())
            .unwrap_err()
            .to_string();

        assert!(error.contains("host mount changed after planning"));
    }

    #[test]
    fn access_mode_drift_is_rejected_at_runtime_compilation() {
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        let original = BoxConfig {
            volumes: vec![format!("{}:/workspace:ro", source.display())],
            security_policy: Some(
                SandboxSecurityPolicy::new().host_mounts(HostMountPolicy::agent_safe()),
            ),
            ..BoxConfig::default()
        };
        let plan =
            resolve_managed_execution_plan(fixture.path(), &request(original.clone(), Vec::new()))
                .unwrap();
        let mut changed = original;
        changed.volumes = vec![format!("{}:/workspace:rw", source.display())];

        let error = validate_runtime_mount_configuration(&plan, &changed, &HashSet::new())
            .unwrap_err()
            .to_string();

        assert!(error.contains("host mount changed after planning"));
    }
}
