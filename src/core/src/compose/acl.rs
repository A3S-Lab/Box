//! Closed-schema A3S ACL parser for Compose applications.

use std::collections::HashMap;

use a3s_acl::{Block, Document, Lexer, Token, Value};
use thiserror::Error;

use super::diagnostic::{child_path, pointer_segment, ComposeDiagnostic};
use super::interpolation::interpolate_compose_scalar;
use super::schema::{
    DEPENDS_ON_FIELDS, HEALTHCHECK_FIELDS, NETWORK_FIELDS, SERVICE_FIELDS, SERVICE_NETWORK_FIELDS,
    VOLUME_FIELDS,
};
use super::{
    ComposeConfig, ComposeDiagnosticCode, DependsOn, DependsOnCondition, DnsConfig, EnvVars,
    HealthcheckConfig, Labels, NetworkDeclaration, ServiceConfig, ServiceNetworkConfig,
    ServiceNetworks, StringOrList, VolumeDeclaration,
};

/// An invalid A3S Compose ACL document.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{diagnostic}")]
pub struct ComposeAclError {
    diagnostic: ComposeDiagnostic,
}

impl ComposeAclError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            diagnostic: ComposeDiagnostic::new(ComposeDiagnosticCode::InvalidValue, "/", message),
        }
    }

    fn syntax(message: impl Into<String>) -> Self {
        Self {
            diagnostic: ComposeDiagnostic::new(ComposeDiagnosticCode::Syntax, "/", message),
        }
    }

    fn interpolation(message: impl Into<String>) -> Self {
        Self {
            diagnostic: ComposeDiagnostic::new(ComposeDiagnosticCode::Interpolation, "/", message),
        }
    }

    fn unsupported_field(path: impl Into<String>, field: &str) -> Self {
        Self {
            diagnostic: ComposeDiagnostic::unsupported_field(path, field),
        }
    }

    fn unsupported_value(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            diagnostic: ComposeDiagnostic::new(
                ComposeDiagnosticCode::UnsupportedValue,
                path,
                message,
            ),
        }
    }

    /// Structured diagnostic suitable for CLI, API, or Cloud callers.
    pub fn diagnostic(&self) -> &ComposeDiagnostic {
        &self.diagnostic
    }
}

pub(super) fn parse_compose_acl(
    source: &str,
    environment: &HashMap<String, String>,
) -> Result<ComposeConfig, ComposeAclError> {
    validate_balanced_braces(source)?;
    let mut document = a3s_acl::parse(source)
        .map_err(|error| ComposeAclError::syntax(format!("invalid A3S ACL: {error}")))?;
    interpolate_document_values(&mut document, environment)?;
    resolve_environment_calls(&mut document, environment)?;
    convert_document(document)
}

fn validate_balanced_braces(source: &str) -> Result<(), ComposeAclError> {
    let mut depth = 0usize;
    for token in Lexer::new(source).tokenize() {
        match token.token {
            Token::LeftBrace => depth += 1,
            Token::RightBrace if depth == 0 => {
                return Err(ComposeAclError::syntax(
                    "compose ACL contains an unmatched closing brace",
                ));
            }
            Token::RightBrace => depth -= 1,
            _ => {}
        }
    }
    if depth != 0 {
        return Err(ComposeAclError::syntax(
            "compose ACL contains an unclosed block or object",
        ));
    }
    Ok(())
}

fn interpolate_document_values(
    document: &mut Document,
    environment: &HashMap<String, String>,
) -> Result<(), ComposeAclError> {
    for block in &mut document.blocks {
        interpolate_block_values(block, environment)?;
    }
    Ok(())
}

fn interpolate_block_values(
    block: &mut Block,
    environment: &HashMap<String, String>,
) -> Result<(), ComposeAclError> {
    for value in block.attributes.values_mut() {
        interpolate_value(value, environment)?;
    }
    for nested in &mut block.blocks {
        interpolate_block_values(nested, environment)?;
    }
    Ok(())
}

fn interpolate_value(
    value: &mut Value,
    environment: &HashMap<String, String>,
) -> Result<(), ComposeAclError> {
    match value {
        Value::String(text) => {
            *text = interpolate_compose_scalar(text, environment).map_err(|error| {
                ComposeAclError::interpolation(format!("invalid Compose interpolation: {error}"))
            })?;
        }
        Value::List(values) | Value::Call(_, values) => {
            for value in values {
                interpolate_value(value, environment)?;
            }
        }
        Value::Object(entries) => {
            for (_, value) in entries {
                interpolate_value(value, environment)?;
            }
        }
        Value::Number(_) | Value::Bool(_) | Value::Null => {}
    }
    Ok(())
}

fn resolve_environment_calls(
    document: &mut Document,
    environment: &HashMap<String, String>,
) -> Result<(), ComposeAclError> {
    for block in &mut document.blocks {
        resolve_block_environment(block, environment)?;
    }
    Ok(())
}

fn resolve_block_environment(
    block: &mut Block,
    environment: &HashMap<String, String>,
) -> Result<(), ComposeAclError> {
    for value in block.attributes.values_mut() {
        resolve_value_environment(value, environment)?;
    }
    for nested in &mut block.blocks {
        resolve_block_environment(nested, environment)?;
    }
    Ok(())
}

fn resolve_value_environment(
    value: &mut Value,
    environment: &HashMap<String, String>,
) -> Result<(), ComposeAclError> {
    match value {
        Value::Call(name, arguments) => {
            if name != "env" {
                return Err(ComposeAclError::unsupported_value(
                    "/",
                    format!("unsupported ACL function {name:?}; only env(\"NAME\") is supported"),
                ));
            }
            let [Value::String(variable)] = arguments.as_slice() else {
                return Err(ComposeAclError::invalid(
                    "env() must receive exactly one string environment variable name",
                ));
            };
            let resolved = environment.get(variable).cloned().ok_or_else(|| {
                ComposeAclError::invalid(format!(
                    "environment variable {variable:?} referenced by env() is not set"
                ))
            })?;
            *value = Value::String(resolved);
        }
        Value::List(values) => {
            for value in values {
                resolve_value_environment(value, environment)?;
            }
        }
        Value::Object(entries) => {
            for (_, value) in entries {
                resolve_value_environment(value, environment)?;
            }
        }
        Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null => {}
    }
    Ok(())
}

fn convert_document(document: Document) -> Result<ComposeConfig, ComposeAclError> {
    let mut services = HashMap::new();
    let mut volumes = HashMap::new();
    let mut networks = HashMap::new();

    for block in document.blocks {
        match block.name.as_str() {
            "service" => {
                let name = named_block_label(&block, "service")?;
                validate_compose_name("service", &name)?;
                let config = parse_service(&block, &name)?;
                if services.insert(name.clone(), config).is_some() {
                    return Err(ComposeAclError::invalid(format!(
                        "duplicate service block {name:?}"
                    )));
                }
            }
            "volume" => {
                let name = named_block_label(&block, "volume")?;
                validate_compose_name("volume", &name)?;
                let path = format!("/volumes/{}", pointer_segment(&name));
                validate_plain_block(&block, VOLUME_FIELDS, &path)?;
                let declaration = VolumeDeclaration {
                    driver: optional_string(&block, "driver", &path)?,
                };
                if volumes.insert(name.clone(), Some(declaration)).is_some() {
                    return Err(ComposeAclError::invalid(format!(
                        "duplicate volume block {name:?}"
                    )));
                }
            }
            "network" => {
                let name = named_block_label(&block, "network")?;
                validate_compose_name("network", &name)?;
                let path = format!("/networks/{}", pointer_segment(&name));
                validate_plain_block(&block, NETWORK_FIELDS, &path)?;
                let declaration = NetworkDeclaration {
                    driver: optional_string(&block, "driver", &path)?,
                };
                if networks.insert(name.clone(), Some(declaration)).is_some() {
                    return Err(ComposeAclError::invalid(format!(
                        "duplicate network block {name:?}"
                    )));
                }
            }
            name => {
                return Err(ComposeAclError::unsupported_field(
                    format!("/{}", pointer_segment(name)),
                    name,
                ));
            }
        }
    }

    if services.is_empty() {
        return Err(ComposeAclError::invalid(
            "compose.acl must contain at least one service block",
        ));
    }

    Ok(ComposeConfig {
        version: None,
        services,
        volumes,
        networks,
    })
}

fn parse_service(block: &Block, name: &str) -> Result<ServiceConfig, ComposeAclError> {
    let path = format!("/services/{}", pointer_segment(name));
    validate_attributes(block, SERVICE_FIELDS, &path)?;
    let healthcheck = parse_service_healthcheck(block, &path)?;

    Ok(ServiceConfig {
        image: optional_string(block, "image", &path)?,
        entrypoint: optional_string_or_list(block, "entrypoint", &path)?,
        command: optional_string_or_list(block, "command", &path)?,
        environment: optional_env_vars(block, "environment", &path)?,
        env_file: optional_string_or_list(block, "env_file", &path)?.unwrap_or_default(),
        ports: optional_string_list(block, "ports", &path)?.unwrap_or_default(),
        volumes: optional_string_list(block, "volumes", &path)?.unwrap_or_default(),
        depends_on: optional_depends_on(block, "depends_on", &path)?,
        networks: optional_service_networks(block, "networks", &path)?,
        cpus: optional_integer(block, "cpus", &path)?,
        mem_limit: optional_string(block, "mem_limit", &path)?,
        restart: optional_string(block, "restart", &path)?,
        dns: optional_dns(block, "dns", &path)?,
        tmpfs: optional_string_or_list(block, "tmpfs", &path)?.unwrap_or_default(),
        cap_add: optional_string_list(block, "cap_add", &path)?.unwrap_or_default(),
        cap_drop: optional_string_list(block, "cap_drop", &path)?.unwrap_or_default(),
        privileged: optional_bool(block, "privileged", &path)?.unwrap_or(false),
        labels: optional_labels(block, "labels", &path)?,
        healthcheck,
        working_dir: optional_string(block, "working_dir", &path)?,
        hostname: optional_string(block, "hostname", &path)?,
        extra_hosts: optional_string_or_list(block, "extra_hosts", &path)?.unwrap_or_default(),
    })
}

fn parse_service_healthcheck(
    service: &Block,
    service_path: &str,
) -> Result<Option<HealthcheckConfig>, ComposeAclError> {
    let mut nested_healthcheck = None;
    for nested in &service.blocks {
        if nested.name != "healthcheck" {
            return Err(ComposeAclError::unsupported_field(
                child_path(service_path, &nested.name),
                &nested.name,
            ));
        }
        if nested_healthcheck.replace(nested).is_some() {
            return Err(ComposeAclError::invalid(format!(
                "{service_path} contains more than one healthcheck block"
            )));
        }
    }

    let attribute_healthcheck = service.attributes.get("healthcheck");
    if attribute_healthcheck.is_some() && nested_healthcheck.is_some() {
        return Err(ComposeAclError::invalid(format!(
            "{service_path} declares healthcheck both as an attribute and a block"
        )));
    }

    if let Some(value) = attribute_healthcheck {
        return parse_healthcheck(value, service_path).map(Some);
    }
    if let Some(block) = nested_healthcheck {
        return parse_healthcheck_block(block, service_path).map(Some);
    }
    Ok(None)
}

fn parse_healthcheck(
    value: &Value,
    service_path: &str,
) -> Result<HealthcheckConfig, ComposeAclError> {
    let path = child_path(service_path, "healthcheck");
    let Value::Object(entries) = value else {
        return Err(ComposeAclError::invalid(format!(
            "{path} must be an object or a healthcheck block"
        )));
    };
    let fields = object_fields(entries, HEALTHCHECK_FIELDS, &path)?;
    parse_healthcheck_fields(&fields, &path)
}

fn parse_healthcheck_block(
    block: &Block,
    service_path: &str,
) -> Result<HealthcheckConfig, ComposeAclError> {
    let path = child_path(service_path, "healthcheck");
    if !block.labels.is_empty() {
        return Err(ComposeAclError::invalid(format!(
            "{path} block cannot have labels"
        )));
    }
    validate_plain_block(block, HEALTHCHECK_FIELDS, &path)?;
    let fields = block
        .attributes
        .iter()
        .map(|(name, value)| (name.as_str(), value))
        .collect::<HashMap<_, _>>();
    parse_healthcheck_fields(&fields, &path)
}

fn parse_healthcheck_fields(
    fields: &HashMap<&str, &Value>,
    path: &str,
) -> Result<HealthcheckConfig, ComposeAclError> {
    Ok(HealthcheckConfig {
        test: fields
            .get("test")
            .map(|value| string_or_list_value(value, &format!("{path}.test")))
            .transpose()?
            .unwrap_or_default(),
        disable: fields
            .get("disable")
            .map(|value| bool_value(value, &format!("{path}.disable")))
            .transpose()?
            .unwrap_or(false),
        interval: fields
            .get("interval")
            .map(|value| string_value(value, &format!("{path}.interval")))
            .transpose()?,
        timeout: fields
            .get("timeout")
            .map(|value| string_value(value, &format!("{path}.timeout")))
            .transpose()?,
        retries: fields
            .get("retries")
            .map(|value| integer_value(value, &format!("{path}.retries")))
            .transpose()?,
        start_period: fields
            .get("start_period")
            .map(|value| string_value(value, &format!("{path}.start_period")))
            .transpose()?,
    })
}

fn named_block_label(block: &Block, kind: &str) -> Result<String, ComposeAclError> {
    let [name] = block.labels.as_slice() else {
        return Err(ComposeAclError::invalid(format!(
            "{kind} blocks require exactly one string label"
        )));
    };
    Ok(name.clone())
}

fn validate_compose_name(kind: &str, name: &str) -> Result<(), ComposeAclError> {
    let mut bytes = name.bytes();
    let valid = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if !valid {
        return Err(ComposeAclError::invalid(format!(
            "{kind} name {name:?} must start with an ASCII letter or digit and contain only letters, digits, '.', '_', or '-'"
        )));
    }
    Ok(())
}

fn validate_plain_block(
    block: &Block,
    attributes: &[&str],
    path: &str,
) -> Result<(), ComposeAclError> {
    if !block.blocks.is_empty() {
        return Err(ComposeAclError::invalid(format!(
            "{path} cannot contain nested blocks"
        )));
    }
    validate_attributes(block, attributes, path)
}

fn validate_attributes(block: &Block, allowed: &[&str], path: &str) -> Result<(), ComposeAclError> {
    let mut unknown = block
        .attributes
        .keys()
        .filter(|field| !allowed.contains(&field.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    unknown.sort();
    if !unknown.is_empty() {
        let first = &unknown[0];
        return Err(ComposeAclError {
            diagnostic: ComposeDiagnostic::new(
                ComposeDiagnosticCode::UnsupportedField,
                child_path(path, first),
                format!("unsupported Compose field(s): {}", unknown.join(", ")),
            ),
        });
    }
    Ok(())
}

fn optional_string(
    block: &Block,
    field: &str,
    path: &str,
) -> Result<Option<String>, ComposeAclError> {
    block
        .attributes
        .get(field)
        .map(|value| string_value(value, &format!("{path}.{field}")))
        .transpose()
}

fn string_value(value: &Value, path: &str) -> Result<String, ComposeAclError> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| ComposeAclError::invalid(format!("{path} must be a string")))
}

fn optional_string_list(
    block: &Block,
    field: &str,
    path: &str,
) -> Result<Option<Vec<String>>, ComposeAclError> {
    block
        .attributes
        .get(field)
        .map(|value| string_list_value(value, &format!("{path}.{field}")))
        .transpose()
}

fn string_list_value(value: &Value, path: &str) -> Result<Vec<String>, ComposeAclError> {
    let Value::List(values) = value else {
        return Err(ComposeAclError::invalid(format!(
            "{path} must be a list of strings"
        )));
    };
    values
        .iter()
        .map(|value| string_value(value, path))
        .collect()
}

fn optional_string_or_list(
    block: &Block,
    field: &str,
    path: &str,
) -> Result<Option<StringOrList>, ComposeAclError> {
    block
        .attributes
        .get(field)
        .map(|value| string_or_list_value(value, &format!("{path}.{field}")))
        .transpose()
}

fn string_or_list_value(value: &Value, path: &str) -> Result<StringOrList, ComposeAclError> {
    match value {
        Value::String(value) => Ok(StringOrList::Single(value.clone())),
        Value::List(_) => string_list_value(value, path).map(StringOrList::List),
        _ => Err(ComposeAclError::invalid(format!(
            "{path} must be a string or a list of strings"
        ))),
    }
}

fn optional_integer<T>(block: &Block, field: &str, path: &str) -> Result<Option<T>, ComposeAclError>
where
    T: TryFrom<u64>,
{
    block
        .attributes
        .get(field)
        .map(|value| integer_value(value, &format!("{path}.{field}")))
        .transpose()
}

fn integer_value<T>(value: &Value, path: &str) -> Result<T, ComposeAclError>
where
    T: TryFrom<u64>,
{
    let Value::Number(number) = value else {
        return Err(ComposeAclError::invalid(format!(
            "{path} must be a nonnegative integer"
        )));
    };
    if !number.is_finite() || *number < 0.0 || number.fract() != 0.0 || *number > u64::MAX as f64 {
        return Err(ComposeAclError::invalid(format!(
            "{path} must be a nonnegative integer"
        )));
    }
    T::try_from(*number as u64)
        .map_err(|_| ComposeAclError::invalid(format!("{path} is out of range")))
}

fn optional_bool(block: &Block, field: &str, path: &str) -> Result<Option<bool>, ComposeAclError> {
    block
        .attributes
        .get(field)
        .map(|value| bool_value(value, &format!("{path}.{field}")))
        .transpose()
}

fn bool_value(value: &Value, path: &str) -> Result<bool, ComposeAclError> {
    value
        .as_bool()
        .ok_or_else(|| ComposeAclError::invalid(format!("{path} must be a boolean")))
}

fn optional_env_vars(block: &Block, field: &str, path: &str) -> Result<EnvVars, ComposeAclError> {
    let Some(value) = block.attributes.get(field) else {
        return Ok(EnvVars::Empty);
    };
    match value {
        Value::List(_) => string_list_value(value, &format!("{path}.{field}")).map(EnvVars::List),
        Value::Object(entries) => {
            string_map_value(entries, &format!("{path}.{field}")).map(EnvVars::Map)
        }
        _ => Err(ComposeAclError::invalid(format!(
            "{path}.{field} must be an object or a list of KEY=value strings"
        ))),
    }
}

fn optional_labels(block: &Block, field: &str, path: &str) -> Result<Labels, ComposeAclError> {
    let Some(value) = block.attributes.get(field) else {
        return Ok(Labels::Empty);
    };
    match value {
        Value::List(_) => string_list_value(value, &format!("{path}.{field}")).map(Labels::List),
        Value::Object(entries) => {
            string_map_value(entries, &format!("{path}.{field}")).map(Labels::Map)
        }
        _ => Err(ComposeAclError::invalid(format!(
            "{path}.{field} must be an object or a list of label strings"
        ))),
    }
}

fn string_map_value(
    entries: &[(String, Value)],
    path: &str,
) -> Result<HashMap<String, String>, ComposeAclError> {
    let mut output = HashMap::new();
    for (key, value) in entries {
        let value = string_value(value, &format!("{path}.{key}"))?;
        if output.insert(key.clone(), value).is_some() {
            return Err(ComposeAclError::invalid(format!(
                "{path} contains duplicate key {key:?}"
            )));
        }
    }
    Ok(output)
}

fn optional_dns(block: &Block, field: &str, path: &str) -> Result<DnsConfig, ComposeAclError> {
    let Some(value) = block.attributes.get(field) else {
        return Ok(DnsConfig::Empty);
    };
    match value {
        Value::String(value) => Ok(DnsConfig::Single(value.clone())),
        Value::List(_) => string_list_value(value, &format!("{path}.{field}")).map(DnsConfig::List),
        _ => Err(ComposeAclError::invalid(format!(
            "{path}.{field} must be a string or a list of strings"
        ))),
    }
}

fn optional_depends_on(
    block: &Block,
    field: &str,
    path: &str,
) -> Result<DependsOn, ComposeAclError> {
    let Some(value) = block.attributes.get(field) else {
        return Ok(DependsOn::Empty);
    };
    let field_path = child_path(path, field);
    match value {
        Value::List(_) => string_list_value(value, &field_path).map(DependsOn::List),
        Value::Object(entries) => {
            let mut dependencies = HashMap::new();
            for (name, value) in entries {
                validate_compose_name("dependency service", name)?;
                let condition = match value {
                    Value::Null => "service_started".to_string(),
                    Value::Object(fields) => {
                        let dependency_path = child_path(&field_path, name);
                        let fields = object_fields(fields, DEPENDS_ON_FIELDS, &dependency_path)?;
                        fields
                            .get("condition")
                            .map(|value| {
                                string_value(value, &child_path(&dependency_path, "condition"))
                            })
                            .transpose()?
                            .unwrap_or_else(|| "service_started".to_string())
                    }
                    _ => {
                        return Err(ComposeAclError::invalid(format!(
                            "{} must be an object or null",
                            child_path(&field_path, name)
                        )));
                    }
                };
                if !matches!(
                    condition.as_str(),
                    "service_started" | "service_healthy" | "service_completed_successfully"
                ) {
                    return Err(ComposeAclError::unsupported_value(
                        child_path(&child_path(&field_path, name), "condition"),
                        format!("unsupported depends_on condition {condition:?}"),
                    ));
                }
                if dependencies
                    .insert(name.clone(), DependsOnCondition { condition })
                    .is_some()
                {
                    return Err(ComposeAclError::invalid(format!(
                        "{field_path} contains duplicate service {name:?}"
                    )));
                }
            }
            Ok(DependsOn::Map(dependencies))
        }
        _ => Err(ComposeAclError::invalid(format!(
            "{field_path} must be a list of service names or an object"
        ))),
    }
}

fn optional_service_networks(
    block: &Block,
    field: &str,
    path: &str,
) -> Result<ServiceNetworks, ComposeAclError> {
    let Some(value) = block.attributes.get(field) else {
        return Ok(ServiceNetworks::Empty);
    };
    let field_path = child_path(path, field);
    match value {
        Value::List(_) => string_list_value(value, &field_path).map(ServiceNetworks::List),
        Value::Object(entries) => {
            let mut networks = HashMap::new();
            for (name, value) in entries {
                validate_compose_name("network", name)?;
                let config = match value {
                    Value::Null => None,
                    Value::Object(fields) => {
                        let network_path = child_path(&field_path, name);
                        let fields = object_fields(fields, SERVICE_NETWORK_FIELDS, &network_path)?;
                        let aliases = fields
                            .get("aliases")
                            .map(|value| {
                                string_list_value(value, &child_path(&network_path, "aliases"))
                            })
                            .transpose()?
                            .unwrap_or_default();
                        Some(ServiceNetworkConfig { aliases })
                    }
                    _ => {
                        return Err(ComposeAclError::invalid(format!(
                            "{} must be an object or null",
                            child_path(&field_path, name)
                        )));
                    }
                };
                if networks.insert(name.clone(), config).is_some() {
                    return Err(ComposeAclError::invalid(format!(
                        "{field_path} contains duplicate network {name:?}"
                    )));
                }
            }
            Ok(ServiceNetworks::Map(networks))
        }
        _ => Err(ComposeAclError::invalid(format!(
            "{field_path} must be a list of network names or an object"
        ))),
    }
}

fn object_fields<'a>(
    entries: &'a [(String, Value)],
    allowed: &[&str],
    path: &str,
) -> Result<HashMap<&'a str, &'a Value>, ComposeAclError> {
    let mut output = HashMap::new();
    for (key, value) in entries {
        if !allowed.contains(&key.as_str()) {
            return Err(ComposeAclError::unsupported_field(
                child_path(path, key),
                key,
            ));
        }
        if output.insert(key.as_str(), value).is_some() {
            return Err(ComposeAclError::invalid(format!(
                "{path} contains duplicate attribute {key:?}"
            )));
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMPLETE: &str = r#"
service "api" {
  image = "ghcr.io/a3s/api:latest"
  entrypoint = ["/bin/api"]
  command = ["serve", "--port", "8080"]
  environment = {
    PORT = "8080"
    TOKEN = env("API_TOKEN")
  }
  env_file = ["base.env", "local.env"]
  ports = ["8080:8080"]
  volumes = ["data:/data"]
  depends_on = {
    db = { condition = "service_healthy" }
  }
  networks = {
    backend = { aliases = ["service-api"] }
  }
  cpus = 2
  mem_limit = "1g"
  restart = "unless-stopped"
  dns = ["1.1.1.1"]
  tmpfs = "/tmp"
  cap_add = ["NET_ADMIN"]
  cap_drop = ["SYS_ADMIN"]
  privileged = false
  labels = { tier = "api" }
  working_dir = "/app"
  hostname = "api"
  extra_hosts = ["host.internal:10.0.0.1"]

  healthcheck {
    test = ["CMD", "curl", "-f", "http://localhost:8080/health"]
    interval = "10s"
    timeout = "3s"
    retries = 3
    start_period = "5s"
  }
}

service "db" {
  image = "postgres:17"
}

volume "data" {
  driver = "local"
}

network "backend" {
  driver = "bridge"
}
"#;

    #[test]
    fn parses_complete_closed_acl_schema() {
        let environment = HashMap::from([("API_TOKEN".to_string(), "secret".to_string())]);
        let config = parse_compose_acl(COMPLETE, &environment).expect("valid compose ACL");

        assert_eq!(config.services.len(), 2);
        let api = &config.services["api"];
        assert_eq!(api.image.as_deref(), Some("ghcr.io/a3s/api:latest"));
        assert_eq!(
            api.command.as_ref().unwrap().to_vec(),
            ["serve", "--port", "8080"]
        );
        assert_eq!(api.environment.to_pairs().len(), 2);
        assert!(api
            .environment
            .to_pairs()
            .contains(&("TOKEN".to_string(), "secret".to_string())));
        assert_eq!(api.depends_on.services(), ["db"]);
        assert_eq!(api.networks.names(), ["backend"]);
        assert_eq!(api.cpus, Some(2));
        assert_eq!(api.healthcheck.as_ref().unwrap().retries, Some(3));
        assert_eq!(
            config.volumes["data"].as_ref().unwrap().driver.as_deref(),
            Some("local")
        );
        assert_eq!(
            config.networks["backend"]
                .as_ref()
                .unwrap()
                .driver
                .as_deref(),
            Some("bridge")
        );
    }

    #[test]
    fn rejects_unknown_blocks_attributes_and_nested_fields() {
        for source in [
            "database \"db\" {}",
            "service \"api\" { image = \"api\" typo = true }",
            "service \"api\" { image = \"api\" deploy {} }",
            "service \"api\" { image = \"api\" healthcheck { typo = 1 } }",
            "service \"api\" { image = \"api\"",
            "service \"api\" { image = \"api\" } }",
        ] {
            assert!(
                parse_compose_acl(source, &HashMap::new()).is_err(),
                "source should fail: {source}"
            );
        }
    }

    #[test]
    fn rejects_invalid_labels_types_numbers_and_functions() {
        for source in [
            "service {}",
            "service \"bad/name\" { image = \"api\" }",
            "service \"api\" { ports = \"8080:80\" }",
            "service \"api\" { cpus = -1 }",
            "service \"api\" { privileged = \"true\" }",
            "service \"api\" { image = concat(\"a\", \"b\") }",
        ] {
            assert!(
                parse_compose_acl(source, &HashMap::new()).is_err(),
                "source should fail: {source}"
            );
        }
    }

    #[test]
    fn reports_missing_environment_values() {
        let error = parse_compose_acl(
            "service \"api\" { environment = { TOKEN = env(\"MISSING\") } }",
            &HashMap::new(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("MISSING"));
        assert!(error.to_string().contains("not set"));
    }

    #[test]
    fn parses_nested_healthcheck_after_multibyte_string() {
        let source = r#"
service "api" {
  image = "api:latest"
  labels = { description = "服务" }

  healthcheck {
    test = ["CMD", "true"]
  }
}
"#;

        let config = parse_compose_acl(source, &HashMap::new()).expect("valid Unicode ACL");

        assert_eq!(
            config.services["api"]
                .healthcheck
                .as_ref()
                .unwrap()
                .test
                .to_vec(),
            ["CMD", "true"]
        );
    }
}
