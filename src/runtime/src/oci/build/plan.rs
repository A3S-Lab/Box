//! Closed A3S ACL contract for OCI image builds owned by A3S Box.
//!
//! A build plan contains immutable product intent. Invocation-only values such
//! as the destination tag and output verbosity stay outside the canonical ACL
//! identity.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use a3s_acl::{
    canonical_bytes_with_schema, canonical_digest_with_schema, parse_with_limits,
    validate_document, AttributeSchema, Block, BlockSchema, CanonicalError, Cardinality, Document,
    ParseLimits, Schema, SchemaDiagnosticCode, Value, ValueSchema,
};
use a3s_box_core::platform::Platform;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::engine::{BuildConfig, BuildNetworkPolicy};

const BUILD_LABEL: &str = "oci";
const MAX_PLAN_PATH_BYTES: usize = 255;
const MAX_TARGET_BYTES: usize = 128;
const BUILD_PLAN_LIMITS: ParseLimits = ParseLimits {
    max_document_bytes: 16 * 1024,
    max_nesting_depth: 2,
    max_collection_items: 16,
    max_token_bytes: 1024,
    max_diagnostics: 16,
};

/// Cache behavior admitted by the Box build-plan contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BuildCachePolicy {
    /// Reuse only content-addressed native build-engine cache entries.
    ContentAddressed,
    /// Rebuild every layer.
    Disabled,
}

impl BuildCachePolicy {
    fn parse(value: &str) -> Result<Self, BoxBuildPlanError> {
        match value {
            "content-addressed" => Ok(Self::ContentAddressed),
            "disabled" => Ok(Self::Disabled),
            _ => Err(BoxBuildPlanError::invalid(
                "cache",
                "must be content-addressed or disabled",
            )),
        }
    }

    /// Stable ACL representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ContentAddressed => "content-addressed",
            Self::Disabled => "disabled",
        }
    }
}

/// Invocation-only options excluded from the canonical build-plan digest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoxBuildOptions {
    /// Optional local image reference assigned by the native image store.
    pub tag: Option<String>,
    /// Suppress native build-engine progress output.
    pub quiet: bool,
}

/// Immutable, closed Box-owned OCI build plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoxBuildPlan {
    context: String,
    file: String,
    platform: Platform,
    target: Option<String>,
    network: BuildNetworkPolicy,
    cache: BuildCachePolicy,
}

impl BoxBuildPlan {
    /// Current closed ACL schema identity.
    pub const SCHEMA: &'static str = "a3s.box.build-plan.v1";

    /// Parse and validate one bounded A3S ACL build plan.
    pub fn parse_acl(source: &str) -> Result<Self, BoxBuildPlanError> {
        let document = parse_with_limits(source, BUILD_PLAN_LIMITS).map_err(|error| {
            BoxBuildPlanError::AclParse {
                message: error.message,
                line: error.line,
                column: error.column,
            }
        })?;
        let schema = build_plan_schema();
        let report = validate_document(&document, &schema);
        if let Some(diagnostic) = report.diagnostics.into_iter().next() {
            return Err(BoxBuildPlanError::Schema {
                code: diagnostic.code,
                path: diagnostic.path,
            });
        }

        let block = document.blocks.first().ok_or_else(|| {
            BoxBuildPlanError::invalid("build", "must contain exactly one build block")
        })?;
        if block.labels.first().map(String::as_str) != Some(BUILD_LABEL) {
            return Err(BoxBuildPlanError::invalid(
                "label",
                "must be the exact value oci",
            ));
        }

        let schema_value = required_string(block, "schema")?;
        if schema_value != Self::SCHEMA {
            return Err(BoxBuildPlanError::invalid(
                "schema",
                "is not a supported Box build-plan schema",
            ));
        }
        let context = normalize_repository_path(required_string(block, "context")?, true)
            .map_err(|reason| BoxBuildPlanError::invalid("context", reason))?;
        let file = normalize_repository_path(required_string(block, "file")?, false)
            .map_err(|reason| BoxBuildPlanError::invalid("file", reason))?;
        let platform = parse_platform(required_string(block, "platform")?)?;
        let network = BuildNetworkPolicy::parse_acl(required_string(block, "network")?)
            .ok_or_else(|| BoxBuildPlanError::invalid("network", "must be none or outbound"))?;
        let cache = BuildCachePolicy::parse(required_string(block, "cache")?)?;
        let target = block
            .attributes
            .get("target")
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| BoxBuildPlanError::invalid("target", "must be a string"))
            })
            .transpose()?
            .map(normalize_target)
            .transpose()
            .map_err(|reason| BoxBuildPlanError::invalid("target", reason))?;

        Ok(Self {
            context,
            file,
            platform,
            target,
            network,
            cache,
        })
    }

    /// Canonical A3S ACL bytes represented as UTF-8 text with one final LF.
    pub fn canonical_acl(&self) -> Result<String, BoxBuildPlanError> {
        let bytes = canonical_bytes_with_schema(&self.document(), &build_plan_schema())?;
        String::from_utf8(bytes).map_err(|_| BoxBuildPlanError::CanonicalEncoding)
    }

    /// Lowercase SHA-256 identity over the canonical ACL bytes.
    pub fn canonical_digest(&self) -> Result<String, BoxBuildPlanError> {
        canonical_digest_with_schema(&self.document(), &build_plan_schema())
            .map_err(BoxBuildPlanError::from)
    }

    /// Resolve repository-relative paths and compile into Box's existing native
    /// build engine. Both paths are canonicalized once, so later symlink swaps
    /// cannot redirect this compiled invocation outside the admitted source.
    pub fn compile(
        &self,
        source_root: &Path,
        options: BoxBuildOptions,
    ) -> Result<BuildConfig, BoxBuildPlanError> {
        let source_root = canonical_source_root(source_root)?;
        let context_dir = resolve_plan_path(&source_root, &self.context, "context", PathKind::Dir)?;
        let dockerfile_path = resolve_plan_path(&source_root, &self.file, "file", PathKind::File)?;

        Ok(BuildConfig {
            context_dir,
            dockerfile_path,
            tag: options.tag,
            build_args: HashMap::new(),
            quiet: options.quiet,
            platforms: vec![self.platform.clone()],
            target: self.target.clone(),
            no_cache: self.cache == BuildCachePolicy::Disabled,
            network: self.network,
            metrics: None,
            run_pool: None,
        })
    }

    /// Repository-relative build context.
    pub fn context(&self) -> &str {
        &self.context
    }

    /// Repository-relative Dockerfile or Containerfile.
    pub fn file(&self) -> &str {
        &self.file
    }

    /// Exact single target platform.
    pub fn platform(&self) -> &Platform {
        &self.platform
    }

    /// Optional multi-stage target.
    pub fn target(&self) -> Option<&str> {
        self.target.as_deref()
    }

    /// Network policy for Dockerfile execution instructions.
    pub const fn network(&self) -> BuildNetworkPolicy {
        self.network
    }

    /// Native build-cache policy.
    pub const fn cache(&self) -> BuildCachePolicy {
        self.cache
    }

    fn document(&self) -> Document {
        let mut attributes = HashMap::from([
            (
                "cache".to_string(),
                Value::String(self.cache.as_str().to_string()),
            ),
            ("context".to_string(), Value::String(self.context.clone())),
            ("file".to_string(), Value::String(self.file.clone())),
            (
                "network".to_string(),
                Value::String(self.network.as_acl().to_string()),
            ),
            (
                "platform".to_string(),
                Value::String(self.platform.to_string()),
            ),
            (
                "schema".to_string(),
                Value::String(Self::SCHEMA.to_string()),
            ),
        ]);
        if let Some(target) = &self.target {
            attributes.insert("target".to_string(), Value::String(target.clone()));
        }
        Document {
            blocks: vec![Block {
                name: "build".to_string(),
                labels: vec![BUILD_LABEL.to_string()],
                blocks: Vec::new(),
                attributes,
            }],
        }
    }
}

/// Stable failures from build-plan admission and source resolution.
#[derive(Debug, Error)]
pub enum BoxBuildPlanError {
    /// The bounded ACL parser rejected the source.
    #[error("Box build plan ACL is invalid at {line}:{column}: {message}")]
    AclParse {
        message: String,
        line: usize,
        column: usize,
    },
    /// The closed schema rejected an attribute, block, label count, or type.
    #[error("Box build plan schema rejected {path}: {code}")]
    Schema {
        code: SchemaDiagnosticCode,
        path: String,
    },
    /// A closed contract value was invalid.
    #[error("Box build plan field {field} {reason}")]
    InvalidValue {
        field: &'static str,
        reason: &'static str,
    },
    /// The caller did not provide a usable absolute source root.
    #[error("Box build source root {reason}")]
    InvalidSourceRoot { reason: &'static str },
    /// A plan path was missing, the wrong kind, or escaped the source root.
    #[error("Box build plan {field} path {reason}")]
    UnsafePath {
        field: &'static str,
        reason: &'static str,
    },
    /// The validated AST could not be canonicalized.
    #[error("Box build plan canonicalization failed: {0}")]
    Canonical(#[from] CanonicalError),
    /// ACL canonical bytes must always be UTF-8.
    #[error("Box build plan canonical output was not UTF-8")]
    CanonicalEncoding,
}

impl BoxBuildPlanError {
    fn invalid(field: &'static str, reason: &'static str) -> Self {
        Self::InvalidValue { field, reason }
    }
}

#[derive(Clone, Copy)]
enum PathKind {
    Dir,
    File,
}

fn build_plan_schema() -> Schema {
    let body = Schema::new()
        .attribute("schema", AttributeSchema::required(ValueSchema::string()))
        .attribute("context", AttributeSchema::required(ValueSchema::string()))
        .attribute("file", AttributeSchema::required(ValueSchema::string()))
        .attribute("platform", AttributeSchema::required(ValueSchema::string()))
        .attribute("target", AttributeSchema::optional(ValueSchema::string()))
        .attribute("network", AttributeSchema::required(ValueSchema::string()))
        .attribute("cache", AttributeSchema::required(ValueSchema::string()));
    Schema::new().block(
        "build",
        BlockSchema::new(body)
            .occurrences(Cardinality::exactly(1))
            .labels(Cardinality::exactly(1)),
    )
}

fn required_string<'a>(
    block: &'a Block,
    field: &'static str,
) -> Result<&'a str, BoxBuildPlanError> {
    block
        .attributes
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| BoxBuildPlanError::invalid(field, "must be a string"))
}

fn normalize_repository_path(value: &str, allow_root: bool) -> Result<String, &'static str> {
    if value.is_empty()
        || value.len() > MAX_PLAN_PATH_BYTES
        || value.starts_with('/')
        || value.contains(['\0', '\\', '%'])
    {
        return Err("must be a bounded relative POSIX path");
    }
    let value = value.strip_prefix("./").unwrap_or(value);
    if value == "." {
        return allow_root
            .then(|| ".".to_string())
            .ok_or("cannot be the repository root");
    }
    let segments = value.split('/').collect::<Vec<_>>();
    if segments.iter().any(|segment| {
        segment.is_empty()
            || matches!(*segment, "." | "..")
            || !segment.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'@' | b'+')
            })
    }) {
        return Err("contains an unsafe path segment");
    }
    Ok(segments.join("/"))
}

fn normalize_target(value: &str) -> Result<String, &'static str> {
    if value.is_empty()
        || value.len() > MAX_TARGET_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err("must be a bounded Dockerfile stage name");
    }
    Ok(value.to_string())
}

fn parse_platform(value: &str) -> Result<Platform, BoxBuildPlanError> {
    match value {
        "linux/amd64" => Ok(Platform::linux_amd64()),
        "linux/arm64" => Ok(Platform::linux_arm64()),
        _ => Err(BoxBuildPlanError::invalid(
            "platform",
            "must be linux/amd64 or linux/arm64",
        )),
    }
}

fn canonical_source_root(source_root: &Path) -> Result<PathBuf, BoxBuildPlanError> {
    if !source_root.is_absolute() {
        return Err(BoxBuildPlanError::InvalidSourceRoot {
            reason: "must be absolute",
        });
    }
    let root = source_root
        .canonicalize()
        .map_err(|_| BoxBuildPlanError::InvalidSourceRoot {
            reason: "does not exist or cannot be resolved",
        })?;
    if !root.is_dir() {
        return Err(BoxBuildPlanError::InvalidSourceRoot {
            reason: "must be a directory",
        });
    }
    Ok(root)
}

fn resolve_plan_path(
    source_root: &Path,
    relative: &str,
    field: &'static str,
    kind: PathKind,
) -> Result<PathBuf, BoxBuildPlanError> {
    let unresolved = if relative == "." {
        source_root.to_path_buf()
    } else {
        source_root.join(relative)
    };
    let resolved = unresolved
        .canonicalize()
        .map_err(|_| BoxBuildPlanError::UnsafePath {
            field,
            reason: "does not exist or cannot be resolved",
        })?;
    if !resolved.starts_with(source_root) {
        return Err(BoxBuildPlanError::UnsafePath {
            field,
            reason: "escapes the admitted source root",
        });
    }
    let expected_kind = match kind {
        PathKind::Dir => resolved.is_dir(),
        PathKind::File => resolved.is_file(),
    };
    if !expected_kind {
        return Err(BoxBuildPlanError::UnsafePath {
            field,
            reason: match kind {
                PathKind::Dir => "must resolve to a directory",
                PathKind::File => "must resolve to a regular file",
            },
        });
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests;
