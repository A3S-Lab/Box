//! Canonical Compose data model and compatibility conversion.

use std::collections::BTreeMap;

use serde::Serialize;

use super::diagnostic::{pointer_segment, ComposeDiagnostic};
use super::{
    ComposeConfig, ComposeDiagnosticCode, ComposeNormalizationError, DependsOn, DependsOnCondition,
    DnsConfig, EnvVars, HealthcheckConfig, Labels, NetworkDeclaration, ServiceConfig,
    ServiceNetworkConfig, ServiceNetworks, StringOrList, VolumeDeclaration,
};

/// Deterministic, syntax-independent Compose project model.
///
/// All semantic maps use `BTreeMap`, alternate list/map spellings are collapsed
/// into one representation, and supported driver defaults are explicit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NormalizedComposeConfig {
    /// Canonically ordered service definitions.
    pub services: BTreeMap<String, NormalizedServiceConfig>,
    /// Canonically ordered named volume declarations.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub volumes: BTreeMap<String, NormalizedVolumeDeclaration>,
    /// Canonically ordered named network declarations.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub networks: BTreeMap<String, NormalizedNetworkDeclaration>,
}

/// Canonical service definition independent of ACL/YAML spelling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NormalizedServiceConfig {
    /// OCI image reference.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Tokenized entrypoint override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
    /// Tokenized command override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// Canonically ordered inline environment.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,
    /// Environment files in precedence order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub env_file: Vec<String>,
    /// Validated and normalized TCP port mappings.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<String>,
    /// Volume mounts in source order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<String>,
    /// Canonically ordered dependency conditions.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub depends_on: BTreeMap<String, NormalizedDependsOn>,
    /// Canonically ordered service networks.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub networks: BTreeMap<String, NormalizedServiceNetwork>,
    /// Requested virtual CPU count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u32>,
    /// Compose memory limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_limit: Option<String>,
    /// Compose restart policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart: Option<String>,
    /// DNS servers in source order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dns: Vec<String>,
    /// tmpfs mounts in source order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tmpfs: Vec<String>,
    /// Linux capabilities to add.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cap_add: Vec<String>,
    /// Linux capabilities to drop.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cap_drop: Vec<String>,
    /// Whether privileged execution is requested.
    #[serde(skip_serializing_if = "is_false")]
    pub privileged: bool,
    /// Canonically ordered service labels.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Optional service health check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<NormalizedHealthcheckConfig>,
    /// Working directory inside the workload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// Workload hostname.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Static host entries in source order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extra_hosts: Vec<String>,
}

/// Canonical dependency condition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NormalizedDependsOn {
    /// Validated dependency condition.
    pub condition: String,
}

/// Canonical per-service network settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NormalizedServiceNetwork {
    /// Validated, sorted, deduplicated DNS aliases.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

/// Canonical health-check settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NormalizedHealthcheckConfig {
    /// Tokenized health-check command.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub test: Vec<String>,
    /// Whether the health check is explicitly disabled.
    #[serde(skip_serializing_if = "is_false")]
    pub disable: bool,
    /// Interval between health checks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval: Option<String>,
    /// Timeout for one health check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    /// Consecutive failures before unhealthy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retries: Option<u32>,
    /// Startup grace period.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_period: Option<String>,
}

/// Canonical named-volume declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NormalizedVolumeDeclaration {
    /// Validated volume driver (`local`).
    pub driver: String,
}

/// Canonical named-network declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NormalizedNetworkDeclaration {
    /// Validated network driver (`bridge`).
    pub driver: String,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl NormalizedComposeConfig {
    /// Serialize a byte-stable, pretty JSON representation with a final newline.
    pub fn to_canonical_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self).map(|json| format!("{json}\n"))
    }

    /// Compute a deterministic dependency-first service order.
    pub fn service_order(&self) -> Result<Vec<String>, ComposeNormalizationError> {
        let mut diagnostics = Vec::new();
        for (service_name, service) in &self.services {
            for dependency in service.depends_on.keys() {
                if !self.services.contains_key(dependency) {
                    diagnostics.push(ComposeDiagnostic::new(
                        ComposeDiagnosticCode::InvalidValue,
                        format!(
                            "/services/{}/depends_on/{}",
                            pointer_segment(service_name),
                            pointer_segment(dependency)
                        ),
                        format!(
                            "service {service_name:?} depends on undefined service {dependency:?}"
                        ),
                    ));
                }
            }
        }
        if !diagnostics.is_empty() {
            return Err(ComposeNormalizationError::new(diagnostics));
        }

        let mut state = BTreeMap::<String, u8>::new();
        let mut order = Vec::new();
        for service_name in self.services.keys() {
            visit_service(self, service_name, &mut state, &mut order)?;
        }
        Ok(order)
    }

    /// Convert the canonical model into the compatibility model consumed by
    /// the current Runtime translation layer.
    pub fn into_config(self) -> ComposeConfig {
        self.into()
    }
}

fn visit_service(
    config: &NormalizedComposeConfig,
    service_name: &str,
    state: &mut BTreeMap<String, u8>,
    order: &mut Vec<String>,
) -> Result<(), ComposeNormalizationError> {
    match state.get(service_name) {
        Some(1) => {
            return Err(ComposeNormalizationError::one(ComposeDiagnostic::new(
                ComposeDiagnosticCode::InvalidValue,
                format!("/services/{}/depends_on", pointer_segment(service_name)),
                format!("dependency cycle detected involving service {service_name:?}"),
            )));
        }
        Some(2) => return Ok(()),
        _ => {}
    }
    state.insert(service_name.to_string(), 1);
    if let Some(service) = config.services.get(service_name) {
        for dependency in service.depends_on.keys() {
            visit_service(config, dependency, state, order)?;
        }
    }
    state.insert(service_name.to_string(), 2);
    order.push(service_name.to_string());
    Ok(())
}

impl From<NormalizedComposeConfig> for ComposeConfig {
    fn from(config: NormalizedComposeConfig) -> Self {
        Self {
            version: None,
            services: config
                .services
                .into_iter()
                .map(|(name, service)| (name, service.into()))
                .collect(),
            volumes: config
                .volumes
                .into_iter()
                .map(|(name, declaration)| {
                    (
                        name,
                        Some(VolumeDeclaration {
                            driver: Some(declaration.driver),
                        }),
                    )
                })
                .collect(),
            networks: config
                .networks
                .into_iter()
                .map(|(name, declaration)| {
                    (
                        name,
                        Some(NetworkDeclaration {
                            driver: Some(declaration.driver),
                        }),
                    )
                })
                .collect(),
        }
    }
}

impl From<NormalizedServiceConfig> for ServiceConfig {
    fn from(service: NormalizedServiceConfig) -> Self {
        Self {
            image: service.image,
            entrypoint: service.entrypoint.map(StringOrList::List),
            command: service.command.map(StringOrList::List),
            environment: if service.environment.is_empty() {
                EnvVars::Empty
            } else {
                EnvVars::Map(service.environment.into_iter().collect())
            },
            env_file: list_or_empty(service.env_file),
            ports: service.ports,
            volumes: service.volumes,
            depends_on: if service.depends_on.is_empty() {
                DependsOn::Empty
            } else {
                DependsOn::Map(
                    service
                        .depends_on
                        .into_iter()
                        .map(|(name, dependency)| {
                            (
                                name,
                                DependsOnCondition {
                                    condition: dependency.condition,
                                },
                            )
                        })
                        .collect(),
                )
            },
            networks: if service.networks.is_empty() {
                ServiceNetworks::Empty
            } else {
                ServiceNetworks::Map(
                    service
                        .networks
                        .into_iter()
                        .map(|(name, network)| {
                            (
                                name,
                                Some(ServiceNetworkConfig {
                                    aliases: network.aliases,
                                }),
                            )
                        })
                        .collect(),
                )
            },
            cpus: service.cpus,
            mem_limit: service.mem_limit,
            restart: service.restart,
            dns: if service.dns.is_empty() {
                DnsConfig::Empty
            } else {
                DnsConfig::List(service.dns)
            },
            tmpfs: list_or_empty(service.tmpfs),
            cap_add: service.cap_add,
            cap_drop: service.cap_drop,
            privileged: service.privileged,
            labels: if service.labels.is_empty() {
                Labels::Empty
            } else {
                Labels::Map(service.labels.into_iter().collect())
            },
            healthcheck: service.healthcheck.map(|healthcheck| HealthcheckConfig {
                test: list_or_empty(healthcheck.test),
                disable: healthcheck.disable,
                interval: healthcheck.interval,
                timeout: healthcheck.timeout,
                retries: healthcheck.retries,
                start_period: healthcheck.start_period,
            }),
            working_dir: service.working_dir,
            hostname: service.hostname,
            extra_hosts: list_or_empty(service.extra_hosts),
        }
    }
}

fn list_or_empty(values: Vec<String>) -> StringOrList {
    if values.is_empty() {
        StringOrList::Empty
    } else {
        StringOrList::List(values)
    }
}
