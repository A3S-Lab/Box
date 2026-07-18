//! Pure, typed, deterministic Compose parsing and normalization.

use std::collections::{BTreeMap, HashMap};

use serde_yaml::{Mapping, Value};

use super::diagnostic::{child_path, ComposeDiagnostic};
use super::schema::{
    DEPENDS_ON_FIELDS, HEALTHCHECK_FIELDS, NETWORK_FIELDS, ROOT_FIELDS, SERVICE_FIELDS,
    SERVICE_NETWORK_FIELDS, VOLUME_FIELDS,
};
use super::{
    ComposeConfig, ComposeDiagnosticCode, ComposeNormalizationError, DependsOn, DependsOnCondition,
    EnvVars, HealthcheckConfig, NormalizedComposeConfig, NormalizedDependsOn,
    NormalizedHealthcheckConfig, NormalizedNetworkDeclaration, NormalizedServiceConfig,
    NormalizedServiceNetwork, NormalizedVolumeDeclaration, ServiceConfig, ServiceNetworks,
};

/// Syntax used by one in-memory Compose source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeSourceFormat {
    /// Canonical A3S Agent Configuration Language.
    Acl,
    /// Bounded Docker Compose-compatible YAML.
    Yaml,
}

/// Parse and normalize one Compose source without filesystem or process access.
///
/// Environment lookup is explicit, making the result fully determined by the
/// three arguments. Callers own `.env` or process-environment loading.
pub fn normalize_compose(
    source: &str,
    format: ComposeSourceFormat,
    environment: &HashMap<String, String>,
) -> Result<NormalizedComposeConfig, ComposeNormalizationError> {
    let config = match format {
        ComposeSourceFormat::Acl => {
            ComposeConfig::from_acl_str_with_environment(source, environment)
                .map_err(|error| ComposeNormalizationError::one(error.diagnostic().clone()))?
        }
        ComposeSourceFormat::Yaml => parse_yaml(source, environment)?,
    };
    normalize_compose_config(config)
}

/// Normalize an already parsed compatibility model.
pub fn normalize_compose_config(
    config: ComposeConfig,
) -> Result<NormalizedComposeConfig, ComposeNormalizationError> {
    let ComposeConfig {
        version: _,
        services: raw_services,
        volumes: raw_volumes,
        networks: raw_networks,
    } = config;

    let mut diagnostics = Vec::new();
    let mut volumes = BTreeMap::new();
    let mut networks = BTreeMap::new();
    let mut services = BTreeMap::new();

    for (name, declaration) in raw_volumes {
        validate_name(
            "volume",
            &name,
            &format!("/volumes/{}", pointer(&name)),
            &mut diagnostics,
        );
        let path = format!("/volumes/{}/driver", pointer(&name));
        let driver = declaration
            .and_then(|declaration| declaration.driver)
            .unwrap_or_else(|| "local".to_string());
        if driver != "local" {
            diagnostics.push(ComposeDiagnostic::new(
                ComposeDiagnosticCode::UnsupportedValue,
                path,
                format!("unsupported volume driver {driver:?}; only \"local\" is supported"),
            ));
        }
        volumes.insert(name, NormalizedVolumeDeclaration { driver });
    }

    for (name, declaration) in raw_networks {
        validate_name(
            "network",
            &name,
            &format!("/networks/{}", pointer(&name)),
            &mut diagnostics,
        );
        let path = format!("/networks/{}/driver", pointer(&name));
        let driver = declaration
            .and_then(|declaration| declaration.driver)
            .unwrap_or_else(|| "bridge".to_string());
        if driver != "bridge" {
            diagnostics.push(ComposeDiagnostic::new(
                ComposeDiagnosticCode::UnsupportedValue,
                path,
                format!("unsupported network driver {driver:?}; only \"bridge\" is supported"),
            ));
        }
        networks.insert(name, NormalizedNetworkDeclaration { driver });
    }

    for (name, service) in raw_services {
        let path = format!("/services/{}", pointer(&name));
        validate_name("service", &name, &path, &mut diagnostics);
        services.insert(name, normalize_service(service, &path, &mut diagnostics));
    }

    if services.is_empty() {
        diagnostics.push(ComposeDiagnostic::new(
            ComposeDiagnosticCode::InvalidValue,
            "/services",
            "Compose projects must define at least one service",
        ));
    }

    let normalized = NormalizedComposeConfig {
        services,
        volumes,
        networks,
    };
    if let Err(error) = normalized.service_order() {
        diagnostics.extend(error.into_diagnostics());
    }

    if diagnostics.is_empty() {
        Ok(normalized)
    } else {
        Err(ComposeNormalizationError::new(diagnostics))
    }
}

fn parse_yaml(
    source: &str,
    environment: &HashMap<String, String>,
) -> Result<ComposeConfig, ComposeNormalizationError> {
    let interpolated = super::interpolate_compose_yaml(source, environment).map_err(|error| {
        ComposeNormalizationError::one(ComposeDiagnostic::new(
            ComposeDiagnosticCode::Interpolation,
            "/",
            error.to_string(),
        ))
    })?;
    let value: Value = serde_yaml::from_str(&interpolated).map_err(yaml_error)?;
    let diagnostics = unsupported_yaml_fields(&value);
    if !diagnostics.is_empty() {
        return Err(ComposeNormalizationError::new(diagnostics));
    }
    serde_yaml::from_value(value).map_err(yaml_error)
}

fn yaml_error(error: serde_yaml::Error) -> ComposeNormalizationError {
    let mut diagnostic = ComposeDiagnostic::new(
        ComposeDiagnosticCode::Syntax,
        "/",
        format!("invalid Compose YAML: {error}"),
    );
    if let Some(location) = error.location() {
        diagnostic = diagnostic.with_location(location.line(), location.column());
    }
    ComposeNormalizationError::one(diagnostic)
}

fn unsupported_yaml_fields(value: &Value) -> Vec<ComposeDiagnostic> {
    let mut diagnostics = Vec::new();
    let Some(root) = value.as_mapping() else {
        return diagnostics;
    };

    validate_mapping_fields(root, ROOT_FIELDS, "/", &mut diagnostics);

    if let Some(services) = mapping_field(root, "services").and_then(Value::as_mapping) {
        for (name, service) in string_entries(services) {
            let service_path = format!("/services/{}", pointer(name));
            let Some(service) = service.as_mapping() else {
                continue;
            };
            validate_mapping_fields(service, SERVICE_FIELDS, &service_path, &mut diagnostics);

            if let Some(healthcheck) =
                mapping_field(service, "healthcheck").and_then(Value::as_mapping)
            {
                validate_mapping_fields(
                    healthcheck,
                    HEALTHCHECK_FIELDS,
                    &child_path(&service_path, "healthcheck"),
                    &mut diagnostics,
                );
            }
            if let Some(dependencies) =
                mapping_field(service, "depends_on").and_then(Value::as_mapping)
            {
                for (dependency, condition) in string_entries(dependencies) {
                    if let Some(condition) = condition.as_mapping() {
                        validate_mapping_fields(
                            condition,
                            DEPENDS_ON_FIELDS,
                            &format!(
                                "{}/{}",
                                child_path(&service_path, "depends_on"),
                                pointer(dependency)
                            ),
                            &mut diagnostics,
                        );
                    }
                }
            }
            if let Some(networks) = mapping_field(service, "networks").and_then(Value::as_mapping) {
                for (network, config) in string_entries(networks) {
                    if let Some(config) = config.as_mapping() {
                        validate_mapping_fields(
                            config,
                            SERVICE_NETWORK_FIELDS,
                            &format!(
                                "{}/{}",
                                child_path(&service_path, "networks"),
                                pointer(network)
                            ),
                            &mut diagnostics,
                        );
                    }
                }
            }
        }
    }

    validate_declaration_fields(root, "volumes", VOLUME_FIELDS, &mut diagnostics);
    validate_declaration_fields(root, "networks", NETWORK_FIELDS, &mut diagnostics);
    diagnostics
}

fn validate_declaration_fields(
    root: &Mapping,
    section: &str,
    allowed: &[&str],
    diagnostics: &mut Vec<ComposeDiagnostic>,
) {
    let Some(declarations) = mapping_field(root, section).and_then(Value::as_mapping) else {
        return;
    };
    for (name, declaration) in string_entries(declarations) {
        if let Some(declaration) = declaration.as_mapping() {
            validate_mapping_fields(
                declaration,
                allowed,
                &format!("/{section}/{}", pointer(name)),
                diagnostics,
            );
        }
    }
}

fn validate_mapping_fields(
    mapping: &Mapping,
    allowed: &[&str],
    path: &str,
    diagnostics: &mut Vec<ComposeDiagnostic>,
) {
    for (field, _) in string_entries(mapping) {
        if !allowed.contains(&field) {
            diagnostics.push(ComposeDiagnostic::unsupported_field(
                child_path(path, field),
                field,
            ));
        }
    }
}

fn mapping_field<'a>(mapping: &'a Mapping, field: &str) -> Option<&'a Value> {
    mapping.get(Value::String(field.to_string()))
}

fn string_entries(mapping: &Mapping) -> impl Iterator<Item = (&str, &Value)> {
    mapping
        .iter()
        .filter_map(|(key, value)| key.as_str().map(|key| (key, value)))
}

fn normalize_service(
    service: ServiceConfig,
    path: &str,
    diagnostics: &mut Vec<ComposeDiagnostic>,
) -> NormalizedServiceConfig {
    let ServiceConfig {
        image,
        entrypoint,
        command,
        environment,
        env_file,
        ports,
        volumes,
        depends_on,
        networks,
        cpus,
        mem_limit,
        restart,
        dns,
        tmpfs,
        cap_add,
        cap_drop,
        privileged,
        labels,
        healthcheck,
        working_dir,
        hostname,
        extra_hosts,
    } = service;

    if image.as_deref().is_none_or(str::is_empty) {
        diagnostics.push(ComposeDiagnostic::new(
            ComposeDiagnosticCode::InvalidValue,
            child_path(path, "image"),
            "Compose services must specify a non-empty image",
        ));
    }

    let ports = ports
        .into_iter()
        .enumerate()
        .filter_map(
            |(index, port)| match crate::normalize_port_maps(std::slice::from_ref(&port)) {
                Ok(mut normalized) => normalized.pop(),
                Err(error) => {
                    diagnostics.push(ComposeDiagnostic::new(
                        ComposeDiagnosticCode::InvalidValue,
                        format!("{path}/ports/{index}"),
                        error,
                    ));
                    None
                }
            },
        )
        .collect();
    let networks = normalize_networks(networks, path, diagnostics);
    if networks.len() > 1 {
        diagnostics.push(ComposeDiagnostic::new(
            ComposeDiagnosticCode::UnsupportedValue,
            child_path(path, "networks"),
            "Box services currently support exactly one explicit Compose network",
        ));
    }

    NormalizedServiceConfig {
        image,
        entrypoint: entrypoint.map(|value| value.to_vec()),
        command: command.map(|value| value.to_vec()),
        environment: normalize_environment(environment, path, diagnostics),
        env_file: env_file.to_vec(),
        ports,
        volumes,
        depends_on: normalize_dependencies(depends_on, path, diagnostics),
        networks,
        cpus,
        mem_limit,
        restart,
        dns: dns.to_vec(),
        tmpfs: tmpfs.to_vec(),
        cap_add,
        cap_drop,
        privileged,
        labels: labels.to_map().into_iter().collect(),
        healthcheck: healthcheck.map(normalize_healthcheck),
        working_dir,
        hostname,
        extra_hosts: extra_hosts.to_vec(),
    }
}

fn normalize_environment(
    environment: EnvVars,
    service_path: &str,
    diagnostics: &mut Vec<ComposeDiagnostic>,
) -> BTreeMap<String, String> {
    match environment {
        EnvVars::Empty => BTreeMap::new(),
        EnvVars::Map(values) => values.into_iter().collect(),
        EnvVars::List(values) => {
            let mut normalized = BTreeMap::new();
            for (index, entry) in values.into_iter().enumerate() {
                let Some((key, value)) = entry.split_once('=') else {
                    diagnostics.push(ComposeDiagnostic::new(
                        ComposeDiagnosticCode::InvalidValue,
                        format!("{service_path}/environment/{index}"),
                        "environment list entries must use KEY=value syntax",
                    ));
                    continue;
                };
                normalized.insert(key.to_string(), value.to_string());
            }
            normalized
        }
    }
}

fn normalize_dependencies(
    depends_on: DependsOn,
    service_path: &str,
    diagnostics: &mut Vec<ComposeDiagnostic>,
) -> BTreeMap<String, NormalizedDependsOn> {
    let dependencies = match depends_on {
        DependsOn::Empty => return BTreeMap::new(),
        DependsOn::List(names) => names
            .into_iter()
            .map(|name| {
                (
                    name,
                    DependsOnCondition {
                        condition: "service_started".to_string(),
                    },
                )
            })
            .collect(),
        DependsOn::Map(dependencies) => dependencies,
    };
    dependencies
        .into_iter()
        .map(|(name, dependency)| {
            let path = format!("{service_path}/depends_on/{}", pointer(&name));
            validate_name("dependency service", &name, &path, diagnostics);
            if !matches!(
                dependency.condition.as_str(),
                "service_started" | "service_healthy" | "service_completed_successfully"
            ) {
                diagnostics.push(ComposeDiagnostic::new(
                    ComposeDiagnosticCode::UnsupportedValue,
                    child_path(&path, "condition"),
                    format!(
                        "unsupported depends_on condition {:?}",
                        dependency.condition
                    ),
                ));
            }
            (
                name,
                NormalizedDependsOn {
                    condition: dependency.condition,
                },
            )
        })
        .collect()
}

fn normalize_networks(
    networks: ServiceNetworks,
    service_path: &str,
    diagnostics: &mut Vec<ComposeDiagnostic>,
) -> BTreeMap<String, NormalizedServiceNetwork> {
    let networks = match networks {
        ServiceNetworks::Empty => return BTreeMap::new(),
        ServiceNetworks::List(names) => names.into_iter().map(|name| (name, None)).collect(),
        ServiceNetworks::Map(networks) => networks,
    };
    networks
        .into_iter()
        .map(|(name, config)| {
            let path = format!("{service_path}/networks/{}", pointer(&name));
            validate_name("network", &name, &path, diagnostics);
            let mut aliases = config.map(|config| config.aliases).unwrap_or_default();
            for (index, alias) in aliases.iter().enumerate() {
                if let Err(error) = crate::dns::validate_hostname(alias) {
                    diagnostics.push(ComposeDiagnostic::new(
                        ComposeDiagnosticCode::InvalidValue,
                        format!("{path}/aliases/{index}"),
                        format!("invalid network alias: {error}"),
                    ));
                }
            }
            aliases.sort();
            aliases.dedup();
            (name, NormalizedServiceNetwork { aliases })
        })
        .collect()
}

fn normalize_healthcheck(healthcheck: HealthcheckConfig) -> NormalizedHealthcheckConfig {
    NormalizedHealthcheckConfig {
        test: healthcheck.test.to_vec(),
        disable: healthcheck.disable,
        interval: healthcheck.interval,
        timeout: healthcheck.timeout,
        retries: healthcheck.retries,
        start_period: healthcheck.start_period,
    }
}

fn validate_name(kind: &str, name: &str, path: &str, diagnostics: &mut Vec<ComposeDiagnostic>) {
    let mut bytes = name.bytes();
    let valid = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if !valid {
        diagnostics.push(ComposeDiagnostic::new(
            ComposeDiagnosticCode::InvalidValue,
            path,
            format!(
                "{kind} names must start with an ASCII letter or digit and contain only letters, digits, '.', '_', or '-'"
            ),
        ));
    }
}

fn pointer(value: &str) -> String {
    super::diagnostic::pointer_segment(value)
}
