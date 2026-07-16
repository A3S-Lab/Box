use std::collections::BTreeSet;
use std::fmt;
use std::net::SocketAddr;
use std::num::{NonZeroU16, NonZeroU32, NonZeroUsize};
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use a3s_acl::{Block, Document, Value};
use a3s_box_core::{BoxConfig, ExecutionIsolation, NetworkMode, ResourceConfig};
use axum::http::uri::Authority;
use thiserror::Error;
use url::Url;

use crate::control::{EnvdMode, ResolvedTemplate, TokenKeyMaterial, TokenScope};
use crate::gateway::DataPlaneGatewayConfig;
use crate::http::{CredentialHash, CredentialScheme, HashedAccountCredential};
use crate::routing::{SandboxDomain, SandboxRoutePolicy, ENVD_PORT};

use super::StaticTemplateProvider;

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const DEFAULT_MAX_JSON_BYTES: usize = 1024 * 1024;
const MIN_MAX_JSON_BYTES: usize = 1024;
const MAX_MAX_JSON_BYTES: usize = 16 * 1024 * 1024;
const MAX_COMMAND_PARTS: usize = 256;
const MAX_COMMAND_BYTES: usize = 64 * 1024;
const TOKEN_KEY_BYTES: usize = 32;

/// Validated service maintenance cadence.
#[derive(Debug, Clone, Copy)]
pub struct SupervisorConfig {
    interval: Duration,
    batch_size: NonZeroU32,
    reconciliation_page_size: NonZeroU32,
}

impl SupervisorConfig {
    pub const fn interval(self) -> Duration {
        self.interval
    }

    pub const fn batch_size(self) -> NonZeroU32 {
        self.batch_size
    }

    pub const fn reconciliation_page_size(self) -> NonZeroU32 {
        self.reconciliation_page_size
    }
}

/// Fully resolved ACL configuration. Secret key material is redacted from Debug output.
pub struct E2bCompatConfig {
    pub(crate) api_listen: SocketAddr,
    pub(crate) api_public_url: Url,
    pub(crate) sandbox_domain: SandboxDomain,
    pub(crate) sandbox_public_domain: String,
    pub(crate) database_path: PathBuf,
    pub(crate) runtime_home: PathBuf,
    pub(crate) runtime_state_path: PathBuf,
    pub(crate) max_json_bytes: usize,
    pub(crate) gateway: DataPlaneGatewayConfig,
    pub(crate) supervisor: SupervisorConfig,
    pub(crate) credentials: Vec<HashedAccountCredential>,
    pub(crate) active_token_version: u32,
    pub(crate) token_keys: Vec<TokenKeyMaterial>,
    pub(crate) templates: StaticTemplateProvider,
}

impl E2bCompatConfig {
    pub async fn load(path: impl AsRef<Path>) -> E2bConfigResult<Self> {
        let path = path.as_ref();
        if path.extension().and_then(|extension| extension.to_str()) != Some("acl") {
            return Err(E2bConfigError::InvalidExtension(path.to_path_buf()));
        }
        let metadata = tokio::fs::metadata(path)
            .await
            .map_err(|source| E2bConfigError::Read {
                path: path.to_path_buf(),
                source,
            })?;
        if metadata.len() > MAX_CONFIG_BYTES {
            return Err(invalid(format!(
                "ACL configuration exceeds the {MAX_CONFIG_BYTES}-byte limit"
            )));
        }
        let input =
            tokio::fs::read_to_string(path)
                .await
                .map_err(|source| E2bConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                })?;
        if input.len() as u64 > MAX_CONFIG_BYTES {
            return Err(invalid(format!(
                "ACL configuration exceeds the {MAX_CONFIG_BYTES}-byte limit"
            )));
        }
        Self::parse_with_environment(&input, |name| std::env::var(name).ok())
    }

    pub fn parse(input: &str) -> E2bConfigResult<Self> {
        Self::parse_with_environment(input, |name| std::env::var(name).ok())
    }

    pub fn api_listen(&self) -> SocketAddr {
        self.api_listen
    }

    pub fn api_public_url(&self) -> &Url {
        &self.api_public_url
    }

    pub fn sandbox_domain(&self) -> &str {
        self.sandbox_domain.as_str()
    }

    pub fn sandbox_public_domain(&self) -> &str {
        &self.sandbox_public_domain
    }

    pub fn supervisor(&self) -> SupervisorConfig {
        self.supervisor
    }

    pub fn gateway(&self) -> &DataPlaneGatewayConfig {
        &self.gateway
    }

    pub(super) fn parse_with_environment<F>(
        input: &str,
        mut environment: F,
    ) -> E2bConfigResult<Self>
    where
        F: FnMut(&str) -> Option<String>,
    {
        let document = a3s_acl::parse(input)?;
        parse_document(document, &mut environment)
    }
}

impl fmt::Debug for E2bCompatConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("E2bCompatConfig")
            .field("api_listen", &self.api_listen)
            .field("api_public_url", &self.api_public_url)
            .field("sandbox_domain", &self.sandbox_domain)
            .field("sandbox_public_domain", &self.sandbox_public_domain)
            .field("database_path", &self.database_path)
            .field("runtime_home", &self.runtime_home)
            .field("runtime_state_path", &self.runtime_state_path)
            .field("max_json_bytes", &self.max_json_bytes)
            .field("gateway", &self.gateway)
            .field("supervisor", &self.supervisor)
            .field("credential_count", &self.credentials.len())
            .field("active_token_version", &self.active_token_version)
            .field("token_key_count", &self.token_keys.len())
            .field("template_count", &self.templates.len())
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum E2bConfigError {
    #[error("E2B compatibility configuration must use the .acl extension: {0}")]
    InvalidExtension(PathBuf),
    #[error("failed to read E2B compatibility ACL configuration {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse E2B compatibility ACL configuration: {0}")]
    Parse(#[from] a3s_acl::ParseError),
    #[error("invalid E2B compatibility ACL configuration: {0}")]
    Invalid(String),
}

pub type E2bConfigResult<T> = std::result::Result<T, E2bConfigError>;

fn parse_document<F>(document: Document, environment: &mut F) -> E2bConfigResult<E2bCompatConfig>
where
    F: FnMut(&str) -> Option<String>,
{
    if document.blocks.len() != 1 || document.blocks[0].name != "e2b_compat" {
        return Err(invalid(
            "configuration must contain exactly one e2b_compat block",
        ));
    }
    let root = &document.blocks[0];
    require_no_labels(root, "e2b_compat")?;
    ensure_shape(
        root,
        &[
            "api_listen",
            "api_public_url",
            "sandbox_domain",
            "sandbox_public_domain",
            "database_path",
            "runtime_home",
            "runtime_state_path",
            "max_json_bytes",
        ],
        &[
            "supervisor",
            "gateway",
            "account",
            "token_key",
            "template_policy",
        ],
        "e2b_compat",
    )?;

    let api_listen = required_string(root, "api_listen", "e2b_compat")?
        .parse::<SocketAddr>()
        .map_err(|_| invalid("e2b_compat.api_listen must be an IP socket address"))?;
    if api_listen.port() == 0 {
        return Err(invalid("e2b_compat.api_listen port must be non-zero"));
    }
    let api_public_url = parse_public_url(required_string(root, "api_public_url", "e2b_compat")?)?;
    let sandbox_domain = SandboxDomain::new(required_string(root, "sandbox_domain", "e2b_compat")?)
        .map_err(|error| invalid(format!("e2b_compat.sandbox_domain: {error}")))?;
    let sandbox_public_domain = parse_sandbox_public_domain(
        optional_string(root, "sandbox_public_domain", "e2b_compat")?,
        &sandbox_domain,
    )?;
    let database_path = required_absolute_path(root, "database_path", "e2b_compat")?;
    let runtime_home = required_absolute_path(root, "runtime_home", "e2b_compat")?;
    let runtime_state_path = required_absolute_path(root, "runtime_state_path", "e2b_compat")?;
    if database_path == runtime_state_path {
        return Err(invalid(
            "database_path and runtime_state_path must identify different files",
        ));
    }
    let max_json_bytes =
        optional_usize(root, "max_json_bytes", "e2b_compat")?.unwrap_or(DEFAULT_MAX_JSON_BYTES);
    if !(MIN_MAX_JSON_BYTES..=MAX_MAX_JSON_BYTES).contains(&max_json_bytes) {
        return Err(invalid(format!(
            "e2b_compat.max_json_bytes must be between {MIN_MAX_JSON_BYTES} and {MAX_MAX_JSON_BYTES}"
        )));
    }

    let supervisor = parse_supervisor(single_child(root, "supervisor", "e2b_compat")?)?;
    let gateway = parse_gateway(single_child(root, "gateway", "e2b_compat")?)?;
    if gateway.listen == api_listen {
        return Err(invalid(
            "e2b_compat.gateway.listen must differ from e2b_compat.api_listen",
        ));
    }
    let credentials = parse_credentials(children(root, "account"))?;
    let (active_token_version, token_keys) =
        parse_token_keys(children(root, "token_key"), environment)?;
    let templates =
        StaticTemplateProvider::new(parse_templates(children(root, "template_policy"))?)
            .map_err(|error| invalid(error.to_string()))?;

    Ok(E2bCompatConfig {
        api_listen,
        api_public_url,
        sandbox_domain,
        sandbox_public_domain,
        database_path,
        runtime_home,
        runtime_state_path,
        max_json_bytes,
        gateway,
        supervisor,
        credentials,
        active_token_version,
        token_keys,
        templates,
    })
}

fn parse_gateway(block: &Block) -> E2bConfigResult<DataPlaneGatewayConfig> {
    const MAX_CONNECTIONS: usize = 100_000;
    const MAX_TIMEOUT_MILLISECONDS: u64 = 60_000;
    const MAX_DRAIN_SECONDS: u64 = 300;

    let context = "e2b_compat.gateway";
    require_no_labels(block, context)?;
    ensure_shape(
        block,
        &[
            "listen",
            "tls_certificate_path",
            "tls_private_key_path",
            "max_connections",
            "handshake_timeout_ms",
            "connect_timeout_ms",
            "drain_timeout_seconds",
        ],
        &[],
        context,
    )?;
    let listen = required_string(block, "listen", context)?
        .parse::<SocketAddr>()
        .map_err(|_| invalid("e2b_compat.gateway.listen must be an IP socket address"))?;
    if listen.port() == 0 {
        return Err(invalid("e2b_compat.gateway.listen port must be non-zero"));
    }
    let certificate_path = required_absolute_path(block, "tls_certificate_path", context)?;
    let private_key_path = required_absolute_path(block, "tls_private_key_path", context)?;
    if certificate_path == private_key_path {
        return Err(invalid(
            "e2b_compat.gateway TLS certificate and private key paths must differ",
        ));
    }
    let max_connections = NonZeroUsize::new(required_usize(block, "max_connections", context)?)
        .filter(|value| value.get() <= MAX_CONNECTIONS)
        .ok_or_else(|| {
            invalid(format!(
                "{context}.max_connections must be between 1 and {MAX_CONNECTIONS}"
            ))
        })?;
    let handshake_timeout_ms = required_u64(block, "handshake_timeout_ms", context)?;
    let connect_timeout_ms = required_u64(block, "connect_timeout_ms", context)?;
    if !(100..=MAX_TIMEOUT_MILLISECONDS).contains(&handshake_timeout_ms) {
        return Err(invalid(format!(
            "{context}.handshake_timeout_ms must be between 100 and {MAX_TIMEOUT_MILLISECONDS}"
        )));
    }
    if !(10..=MAX_TIMEOUT_MILLISECONDS).contains(&connect_timeout_ms) {
        return Err(invalid(format!(
            "{context}.connect_timeout_ms must be between 10 and {MAX_TIMEOUT_MILLISECONDS}"
        )));
    }
    let drain_timeout_seconds = required_u64(block, "drain_timeout_seconds", context)?;
    if !(1..=MAX_DRAIN_SECONDS).contains(&drain_timeout_seconds) {
        return Err(invalid(format!(
            "{context}.drain_timeout_seconds must be between 1 and {MAX_DRAIN_SECONDS}"
        )));
    }

    Ok(DataPlaneGatewayConfig {
        listen,
        certificate_path,
        private_key_path,
        max_connections,
        handshake_timeout: Duration::from_millis(handshake_timeout_ms),
        connect_timeout: Duration::from_millis(connect_timeout_ms),
        drain_timeout: Duration::from_secs(drain_timeout_seconds),
    })
}

fn parse_public_url(value: String) -> E2bConfigResult<Url> {
    let url = Url::parse(&value)
        .map_err(|_| invalid("e2b_compat.api_public_url must be an absolute HTTP(S) URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid(
            "e2b_compat.api_public_url must be an HTTP(S) origin without credentials, query, or fragment",
        ));
    }
    Ok(url)
}

fn parse_sandbox_public_domain(
    value: Option<String>,
    sandbox_domain: &SandboxDomain,
) -> E2bConfigResult<String> {
    let value = value.unwrap_or_else(|| sandbox_domain.as_str().to_string());
    let authority = Authority::from_str(&value).map_err(|_| {
        invalid(
            "e2b_compat.sandbox_public_domain must be the sandbox domain with an optional TCP port",
        )
    })?;
    let port_is_valid = authority
        .port()
        .map(|port| port.as_str().parse::<NonZeroU16>().is_ok())
        .unwrap_or(true);
    if authority.host() != sandbox_domain.as_str() || !port_is_valid {
        return Err(invalid(
            "e2b_compat.sandbox_public_domain must match sandbox_domain and may include one non-zero TCP port",
        ));
    }
    Ok(value)
}

fn parse_supervisor(block: &Block) -> E2bConfigResult<SupervisorConfig> {
    require_no_labels(block, "e2b_compat.supervisor")?;
    ensure_shape(
        block,
        &["interval_seconds", "batch_size", "reconciliation_page_size"],
        &[],
        "e2b_compat.supervisor",
    )?;
    let interval_seconds = required_u64(block, "interval_seconds", "e2b_compat.supervisor")?;
    if !(1..=3600).contains(&interval_seconds) {
        return Err(invalid(
            "e2b_compat.supervisor.interval_seconds must be between 1 and 3600",
        ));
    }
    let batch_size = required_nonzero_u32(block, "batch_size", "e2b_compat.supervisor")?;
    let reconciliation_page_size =
        required_nonzero_u32(block, "reconciliation_page_size", "e2b_compat.supervisor")?;
    if batch_size.get() > 10_000 || reconciliation_page_size.get() > 10_000 {
        return Err(invalid(
            "supervisor batch sizes cannot exceed 10000 records",
        ));
    }
    Ok(SupervisorConfig {
        interval: Duration::from_secs(interval_seconds),
        batch_size,
        reconciliation_page_size,
    })
}

fn parse_credentials(blocks: Vec<&Block>) -> E2bConfigResult<Vec<HashedAccountCredential>> {
    if blocks.is_empty() {
        return Err(invalid("at least one account block is required"));
    }
    let mut labels = BTreeSet::new();
    let mut identities = BTreeSet::new();
    let mut credentials = Vec::with_capacity(blocks.len());
    for block in blocks {
        let label = single_label(block, "e2b_compat.account")?;
        if !labels.insert(label.to_string()) {
            return Err(invalid(format!(
                "account label {label:?} is configured more than once"
            )));
        }
        let context = format!("e2b_compat.account[{label}]");
        ensure_shape(
            block,
            &["scheme", "owner_id", "client_id", "hash"],
            &[],
            &context,
        )?;
        let scheme_name = required_string(block, "scheme", &context)?;
        let scheme = match scheme_name.as_str() {
            "api_key" => CredentialScheme::ApiKey,
            "bearer" => CredentialScheme::Bearer,
            "supabase" => CredentialScheme::Supabase,
            _ => {
                return Err(invalid(format!(
                    "{context}.scheme must be api_key, bearer, or supabase"
                )))
            }
        };
        let owner_id = required_nonempty_string(block, "owner_id", &context, 128)?;
        let client_id = required_nonempty_string(block, "client_id", &context, 128)?;
        if !identities.insert((scheme_name, client_id.clone())) {
            return Err(invalid(format!(
                "credential scheme and client ID are duplicated in {context}"
            )));
        }
        let hash = required_string(block, "hash", &context)?
            .parse::<CredentialHash>()
            .map_err(|error| invalid(format!("{context}.hash: {error}")))?;
        credentials.push(
            HashedAccountCredential::new(scheme, owner_id, client_id, hash)
                .map_err(|error| invalid(format!("{context}: {error}")))?,
        );
    }
    Ok(credentials)
}

fn parse_token_keys<F>(
    blocks: Vec<&Block>,
    environment: &mut F,
) -> E2bConfigResult<(u32, Vec<TokenKeyMaterial>)>
where
    F: FnMut(&str) -> Option<String>,
{
    if blocks.is_empty() {
        return Err(invalid("at least one token_key block is required"));
    }
    let mut labels = BTreeSet::new();
    let mut versions = BTreeSet::new();
    let mut active_version = None;
    let mut materials = Vec::with_capacity(blocks.len());
    for block in blocks {
        let label = single_label(block, "e2b_compat.token_key")?;
        if !labels.insert(label.to_string()) {
            return Err(invalid(format!(
                "token key label {label:?} is configured more than once"
            )));
        }
        let context = format!("e2b_compat.token_key[{label}]");
        ensure_shape(
            block,
            &["version", "active", "encryption_key", "digest_key"],
            &[],
            &context,
        )?;
        let version = required_u32(block, "version", &context)?;
        if version == 0 || !versions.insert(version) {
            return Err(invalid(format!(
                "{context}.version must be unique and greater than zero"
            )));
        }
        if required_bool(block, "active", &context)? && active_version.replace(version).is_some() {
            return Err(invalid("exactly one token key can be active"));
        }
        let encryption = required_environment_key(block, "encryption_key", &context, environment)?;
        let digest = required_environment_key(block, "digest_key", &context, environment)?;
        if encryption == digest {
            return Err(invalid(format!(
                "{context} must use independent encryption and digest keys"
            )));
        }
        materials.push(
            TokenKeyMaterial::new(version, &encryption, &digest)
                .map_err(|error| invalid(format!("{context}: {error}")))?,
        );
    }
    let active_version = active_version.ok_or_else(|| invalid("one token key must be active"))?;
    Ok((active_version, materials))
}

fn required_environment_key<F>(
    block: &Block,
    field: &str,
    context: &str,
    environment: &mut F,
) -> E2bConfigResult<[u8; TOKEN_KEY_BYTES]>
where
    F: FnMut(&str) -> Option<String>,
{
    let value = required_value(block, field, context)?;
    let variable = match value {
        Value::Call(name, arguments)
            if name == "env"
                && arguments.len() == 1
                && matches!(&arguments[0], Value::String(_)) =>
        {
            arguments[0].as_str().unwrap_or_default()
        }
        _ => {
            return Err(invalid(format!(
                "{context}.{field} must use env(\"VARIABLE\")"
            )))
        }
    };
    if !valid_environment_name(variable) {
        return Err(invalid(format!(
            "{context}.{field} references an invalid environment variable name"
        )));
    }
    let encoded = environment(variable).ok_or_else(|| {
        invalid(format!(
            "environment variable {variable} required by {context}.{field} is unavailable"
        ))
    })?;
    let decoded = hex::decode(encoded).map_err(|_| {
        invalid(format!(
            "environment variable {variable} must contain a 32-byte hexadecimal key"
        ))
    })?;
    decoded.try_into().map_err(|_| {
        invalid(format!(
            "environment variable {variable} must contain a 32-byte hexadecimal key"
        ))
    })
}

fn valid_environment_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && (bytes[0].is_ascii_uppercase() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_')
}

fn parse_templates(blocks: Vec<&Block>) -> E2bConfigResult<Vec<(String, ResolvedTemplate)>> {
    if blocks.is_empty() {
        return Err(invalid("at least one template_policy block is required"));
    }
    let mut labels = BTreeSet::new();
    let mut templates = Vec::with_capacity(blocks.len());
    for block in blocks {
        let template_id = single_label(block, "e2b_compat.template_policy")?.to_string();
        if !labels.insert(template_id.clone()) {
            return Err(invalid(format!(
                "template policy {template_id:?} is configured more than once"
            )));
        }
        let context = format!("e2b_compat.template_policy[{template_id}]");
        ensure_shape(
            block,
            &[
                "image",
                "envd_version",
                "envd_mode",
                "isolation",
                "network",
                "command",
                "entrypoint",
                "user",
                "workdir",
                "read_only",
                "stdin_open",
            ],
            &["resources", "route"],
            &context,
        )?;

        let image = required_nonempty_string(block, "image", &context, 512)?;
        if image.chars().any(char::is_whitespace) {
            return Err(invalid(format!(
                "{context}.image cannot contain whitespace"
            )));
        }
        let envd_version = required_nonempty_string(block, "envd_version", &context, 128)?;
        let envd_mode = match optional_string(block, "envd_mode", &context)?.as_deref() {
            None | Some("broker") => EnvdMode::Broker,
            Some("runtime") => EnvdMode::Runtime,
            Some(_) => {
                return Err(invalid(format!(
                    "{context}.envd_mode must be broker or runtime"
                )))
            }
        };
        let isolation = match optional_string(block, "isolation", &context)?.as_deref() {
            None => ExecutionIsolation::Microvm,
            Some("sandbox") => ExecutionIsolation::Sandbox,
            Some(_) => {
                return Err(invalid(format!(
                "{context}.isolation accepts only the explicit value sandbox; omit it for MicroVM"
            )))
            }
        };
        let network = match optional_string(block, "network", &context)?.as_deref() {
            None | Some("tsi") => NetworkMode::Tsi,
            Some("none") => NetworkMode::None,
            Some(_) => return Err(invalid(format!("{context}.network must be tsi or none"))),
        };
        let resources_block = single_child(block, "resources", &context)?;
        let resources = parse_resources(resources_block, &context)?;
        let command = optional_string_list(block, "command", &context)?.unwrap_or_default();
        let entrypoint_override = optional_string_list(block, "entrypoint", &context)?;
        if entrypoint_override.as_ref().is_some_and(Vec::is_empty) {
            return Err(invalid(format!("{context}.entrypoint cannot be empty")));
        }
        let user = optional_nonempty_string(block, "user", &context, 128)?;
        let workdir = optional_nonempty_string(block, "workdir", &context, 4096)?;
        let read_only = optional_bool(block, "read_only", &context)?.unwrap_or(false);
        let stdin_open = optional_bool(block, "stdin_open", &context)?.unwrap_or(false);
        let routing = parse_routes(children(block, "route"), &context)?;

        templates.push((
            template_id,
            ResolvedTemplate {
                config: BoxConfig {
                    isolation,
                    image,
                    resources,
                    cmd: command,
                    entrypoint_override,
                    user,
                    workdir,
                    network,
                    read_only,
                    stdin_open,
                    ..BoxConfig::default()
                },
                envd_version,
                envd_mode,
                routing,
            },
        ));
    }
    Ok(templates)
}

fn parse_resources(block: &Block, parent: &str) -> E2bConfigResult<ResourceConfig> {
    let context = format!("{parent}.resources");
    require_no_labels(block, &context)?;
    ensure_shape(block, &["vcpus", "memory_mb", "disk_mb"], &[], &context)?;
    let vcpus = required_u32(block, "vcpus", &context)?;
    let memory_mb = required_u32(block, "memory_mb", &context)?;
    let disk_mb = required_u32(block, "disk_mb", &context)?;
    if vcpus == 0 || vcpus > 256 {
        return Err(invalid(format!(
            "{context}.vcpus must be between 1 and 256"
        )));
    }
    if memory_mb < 16 {
        return Err(invalid(format!("{context}.memory_mb must be at least 16")));
    }
    if disk_mb == 0 {
        return Err(invalid(format!(
            "{context}.disk_mb must be greater than zero"
        )));
    }
    Ok(ResourceConfig {
        vcpus,
        memory_mb,
        disk_mb,
        timeout: ResourceConfig::default().timeout,
    })
}

fn parse_routes(blocks: Vec<&Block>, parent: &str) -> E2bConfigResult<SandboxRoutePolicy> {
    let mut ports = Vec::with_capacity(blocks.len() + 1);
    for (index, block) in blocks.into_iter().enumerate() {
        let context = format!("{parent}.route[{index}]");
        require_no_labels(block, &context)?;
        ensure_shape(block, &["port", "token_scope"], &[], &context)?;
        let port = required_u16(block, "port", &context)?;
        if port == 0 {
            return Err(invalid(format!("{context}.port must be non-zero")));
        }
        let scope = match required_string(block, "token_scope", &context)?.as_str() {
            "envd" => TokenScope::Envd,
            "traffic" => TokenScope::Traffic,
            _ => {
                return Err(invalid(format!(
                    "{context}.token_scope must be envd or traffic"
                )))
            }
        };
        ports.push((port, scope));
    }
    if !ports.iter().any(|(port, _)| *port == ENVD_PORT) {
        ports.push((ENVD_PORT, TokenScope::Envd));
    }
    SandboxRoutePolicy::new(ports)
        .map_err(|error| invalid(format!("{parent} has an invalid route policy: {error}")))
}

fn ensure_shape(
    block: &Block,
    attributes: &[&str],
    child_blocks: &[&str],
    context: &str,
) -> E2bConfigResult<()> {
    for attribute in block.attributes.keys() {
        if !attributes.contains(&attribute.as_str()) {
            return Err(invalid(format!(
                "{context} contains unknown attribute {attribute}"
            )));
        }
    }
    for child in &block.blocks {
        if !child_blocks.contains(&child.name.as_str()) {
            return Err(invalid(format!(
                "{context} contains unknown block {}",
                child.name
            )));
        }
    }
    Ok(())
}

fn children<'a>(block: &'a Block, name: &str) -> Vec<&'a Block> {
    block
        .blocks
        .iter()
        .filter(|child| child.name == name)
        .collect()
}

fn single_child<'a>(block: &'a Block, name: &str, context: &str) -> E2bConfigResult<&'a Block> {
    let matches = children(block, name);
    match matches.as_slice() {
        [child] => Ok(child),
        [] => Err(invalid(format!("{context}.{name} block is required"))),
        _ => Err(invalid(format!(
            "{context} can contain only one {name} block"
        ))),
    }
}

fn require_no_labels(block: &Block, context: &str) -> E2bConfigResult<()> {
    if block.labels.is_empty() {
        Ok(())
    } else {
        Err(invalid(format!("{context} does not accept block labels")))
    }
}

fn single_label<'a>(block: &'a Block, context: &str) -> E2bConfigResult<&'a str> {
    match block.labels.as_slice() {
        [label] if !label.trim().is_empty() && label.len() <= 128 => Ok(label),
        _ => Err(invalid(format!(
            "{context} requires exactly one non-empty string label"
        ))),
    }
}

fn required_value<'a>(block: &'a Block, field: &str, context: &str) -> E2bConfigResult<&'a Value> {
    block
        .attributes
        .get(field)
        .ok_or_else(|| invalid(format!("{context}.{field} is required")))
}

fn required_string(block: &Block, field: &str, context: &str) -> E2bConfigResult<String> {
    match required_value(block, field, context)? {
        Value::String(value) => Ok(value.clone()),
        _ => Err(invalid(format!("{context}.{field} must be a string"))),
    }
}

fn optional_string(block: &Block, field: &str, context: &str) -> E2bConfigResult<Option<String>> {
    block
        .attributes
        .get(field)
        .map(|value| match value {
            Value::String(value) => Ok(value.clone()),
            _ => Err(invalid(format!("{context}.{field} must be a string"))),
        })
        .transpose()
}

fn required_nonempty_string(
    block: &Block,
    field: &str,
    context: &str,
    max_bytes: usize,
) -> E2bConfigResult<String> {
    let value = required_string(block, field, context)?;
    validate_nonempty_string(value, field, context, max_bytes)
}

fn optional_nonempty_string(
    block: &Block,
    field: &str,
    context: &str,
    max_bytes: usize,
) -> E2bConfigResult<Option<String>> {
    optional_string(block, field, context)?
        .map(|value| validate_nonempty_string(value, field, context, max_bytes))
        .transpose()
}

fn validate_nonempty_string(
    value: String,
    field: &str,
    context: &str,
    max_bytes: usize,
) -> E2bConfigResult<String> {
    if value.trim().is_empty() || value.len() > max_bytes || value.contains('\0') {
        Err(invalid(format!(
            "{context}.{field} must be non-empty and no more than {max_bytes} bytes"
        )))
    } else {
        Ok(value)
    }
}

fn required_bool(block: &Block, field: &str, context: &str) -> E2bConfigResult<bool> {
    match required_value(block, field, context)? {
        Value::Bool(value) => Ok(*value),
        _ => Err(invalid(format!("{context}.{field} must be a boolean"))),
    }
}

fn optional_bool(block: &Block, field: &str, context: &str) -> E2bConfigResult<Option<bool>> {
    block
        .attributes
        .get(field)
        .map(|value| match value {
            Value::Bool(value) => Ok(*value),
            _ => Err(invalid(format!("{context}.{field} must be a boolean"))),
        })
        .transpose()
}

fn required_u64(block: &Block, field: &str, context: &str) -> E2bConfigResult<u64> {
    number_as_u64(required_value(block, field, context)?, field, context)
}

fn required_u32(block: &Block, field: &str, context: &str) -> E2bConfigResult<u32> {
    let value = required_u64(block, field, context)?;
    u32::try_from(value).map_err(|_| invalid(format!("{context}.{field} exceeds the u32 range")))
}

fn required_u16(block: &Block, field: &str, context: &str) -> E2bConfigResult<u16> {
    let value = required_u64(block, field, context)?;
    u16::try_from(value)
        .map_err(|_| invalid(format!("{context}.{field} exceeds the TCP port range")))
}

fn required_nonzero_u32(block: &Block, field: &str, context: &str) -> E2bConfigResult<NonZeroU32> {
    NonZeroU32::new(required_u32(block, field, context)?)
        .ok_or_else(|| invalid(format!("{context}.{field} must be greater than zero")))
}

fn optional_usize(block: &Block, field: &str, context: &str) -> E2bConfigResult<Option<usize>> {
    block
        .attributes
        .get(field)
        .map(|value| {
            let value = number_as_u64(value, field, context)?;
            usize::try_from(value)
                .map_err(|_| invalid(format!("{context}.{field} exceeds the platform range")))
        })
        .transpose()
}

fn required_usize(block: &Block, field: &str, context: &str) -> E2bConfigResult<usize> {
    let value = required_u64(block, field, context)?;
    usize::try_from(value)
        .map_err(|_| invalid(format!("{context}.{field} exceeds the platform range")))
}

fn number_as_u64(value: &Value, field: &str, context: &str) -> E2bConfigResult<u64> {
    match value {
        Value::Number(value)
            if value.is_finite()
                && *value >= 0.0
                && value.fract() == 0.0
                && *value <= u64::MAX as f64 =>
        {
            Ok(*value as u64)
        }
        _ => Err(invalid(format!(
            "{context}.{field} must be a non-negative integer"
        ))),
    }
}

fn optional_string_list(
    block: &Block,
    field: &str,
    context: &str,
) -> E2bConfigResult<Option<Vec<String>>> {
    let Some(value) = block.attributes.get(field) else {
        return Ok(None);
    };
    let Value::List(values) = value else {
        return Err(invalid(format!(
            "{context}.{field} must be a list of strings"
        )));
    };
    if values.len() > MAX_COMMAND_PARTS {
        return Err(invalid(format!(
            "{context}.{field} cannot contain more than {MAX_COMMAND_PARTS} entries"
        )));
    }
    let mut total_bytes = 0usize;
    let mut strings = Vec::with_capacity(values.len());
    for value in values {
        let Value::String(value) = value else {
            return Err(invalid(format!(
                "{context}.{field} must contain only strings"
            )));
        };
        if value.contains('\0') {
            return Err(invalid(format!(
                "{context}.{field} cannot contain NUL bytes"
            )));
        }
        total_bytes = total_bytes
            .checked_add(value.len())
            .ok_or_else(|| invalid(format!("{context}.{field} is too large")))?;
        strings.push(value.clone());
    }
    if total_bytes > MAX_COMMAND_BYTES {
        return Err(invalid(format!(
            "{context}.{field} cannot exceed {MAX_COMMAND_BYTES} bytes"
        )));
    }
    Ok(Some(strings))
}

fn required_absolute_path(block: &Block, field: &str, context: &str) -> E2bConfigResult<PathBuf> {
    let path = PathBuf::from(required_nonempty_string(block, field, context, 4096)?);
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid(format!(
            "{context}.{field} must be an absolute normalized path"
        )));
    }
    Ok(path)
}

fn invalid(message: impl Into<String>) -> E2bConfigError {
    E2bConfigError::Invalid(message.into())
}
