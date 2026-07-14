use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::digest::sha256;
use crate::exports::read_public_exports;
use crate::model::{CompatibilityManifest, ContractInventory, SourceLock};
use crate::openapi::{read_json_schema, read_openapi};
use crate::proto::read_protobuf_contracts;

const CONTROL_OPENAPI: &str = "spec/e2b/openapi.yml";
const ENVD_OPENAPI: &str = "spec/e2b/envd/envd.yaml";
const VOLUME_OPENAPI: &str = "spec/e2b/openapi-volumecontent.yml";
const PROCESS_PROTO: &str = "spec/e2b/envd/process/process.proto";
const FILESYSTEM_PROTO: &str = "spec/e2b/envd/filesystem/filesystem.proto";
const MCP_SCHEMA: &str = "spec/e2b/mcp-server.json";

#[derive(Debug, Clone)]
pub struct FixturePaths {
    root: PathBuf,
}

impl FixturePaths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn repository_default() -> Self {
        Self::new(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join("compat/e2b"),
        )
    }

    fn source_lock(&self) -> PathBuf {
        self.root.join("upstream.lock.json")
    }

    fn contract_inventory(&self) -> PathBuf {
        self.root.join("inventory/contracts.json")
    }

    fn public_exports(&self) -> PathBuf {
        self.root.join("inventory/public-exports.json")
    }

    fn manifest(&self) -> PathBuf {
        self.root.join("manifests/v1.json")
    }
}

struct GeneratedFixture {
    contracts: Vec<u8>,
    public_exports: Vec<u8>,
    manifest: Vec<u8>,
}

pub fn generate_fixture(paths: &FixturePaths) -> Result<()> {
    let fixture = build_fixture(paths)?;
    write_generated(&paths.contract_inventory(), &fixture.contracts)?;
    write_generated(&paths.public_exports(), &fixture.public_exports)?;
    write_generated(&paths.manifest(), &fixture.manifest)?;
    Ok(())
}

pub fn verify_fixture(paths: &FixturePaths) -> Result<()> {
    let fixture = build_fixture(paths)?;
    verify_generated(
        &paths.contract_inventory(),
        &fixture.contracts,
        "contract inventory",
    )?;
    verify_generated(
        &paths.public_exports(),
        &fixture.public_exports,
        "public export inventory",
    )?;
    verify_generated(
        &paths.manifest(),
        &fixture.manifest,
        "compatibility manifest",
    )?;
    Ok(())
}

fn build_fixture(paths: &FixturePaths) -> Result<GeneratedFixture> {
    let source_lock_bytes = std::fs::read(paths.source_lock()).with_context(|| {
        format!(
            "failed to read E2B upstream lock {}",
            paths.source_lock().display()
        )
    })?;
    let source_lock: SourceLock =
        serde_json::from_slice(&source_lock_bytes).context("failed to parse E2B upstream lock")?;
    validate_source_lock(paths, &source_lock)?;

    let control_plane_tags = source_lock
        .compatibility
        .control_plane_tags
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let openapi = vec![
        read_openapi(
            &paths.root.join(CONTROL_OPENAPI),
            "control-plane",
            Some(&control_plane_tags),
        )?,
        read_openapi(&paths.root.join(ENVD_OPENAPI), "envd", None)?,
        read_openapi(&paths.root.join(VOLUME_OPENAPI), "volume-content", None)?,
    ];
    let protobuf = read_protobuf_contracts(
        &paths.root.join("spec/e2b/envd"),
        &["filesystem/filesystem.proto", "process/process.proto"],
    )?;
    let mcp = read_json_schema(&paths.root.join(MCP_SCHEMA))?;
    let contract_inventory = ContractInventory {
        schema_version: 1,
        compatibility_id: source_lock.compatibility.id.clone(),
        openapi,
        protobuf,
        mcp,
    };
    let public_export_inventory = read_public_exports(&paths.root, &source_lock)?;
    let contracts = pretty_json(&contract_inventory)?;
    let public_exports = pretty_json(&public_export_inventory)?;
    let manifest = build_manifest(
        &source_lock,
        &contract_inventory,
        &contracts,
        &public_exports,
    )?;

    Ok(GeneratedFixture {
        contracts,
        public_exports,
        manifest: pretty_json(&manifest)?,
    })
}

fn validate_source_lock(paths: &FixturePaths, source_lock: &SourceLock) -> Result<()> {
    if source_lock.schema_version != 1 {
        bail!(
            "unsupported E2B upstream lock schema version {}",
            source_lock.schema_version
        );
    }
    if source_lock.compatibility.id.trim().is_empty()
        || source_lock.compatibility.version.trim().is_empty()
    {
        bail!("E2B upstream lock compatibility identity and version must be non-empty");
    }
    if source_lock.compatibility.control_plane_tags.is_empty() {
        bail!("E2B upstream lock must select at least one public control-plane tag");
    }
    for required in ["e2b", "code-interpreter"] {
        let source = source_lock
            .sources
            .get(required)
            .with_context(|| format!("E2B upstream lock is missing source {required}"))?;
        if source.repository.trim().is_empty() || source.commit.len() != 40 {
            bail!("E2B upstream source {required} has an invalid repository or commit");
        }
        for language in ["python", "typescript"] {
            if source
                .packages
                .get(language)
                .is_none_or(|version| version.trim().is_empty())
            {
                bail!("E2B upstream source {required} is missing {language} package version");
            }
        }
    }

    let mut locked_paths = BTreeSet::new();
    for file in &source_lock.files {
        validate_relative_path(&file.local_path)?;
        if !locked_paths.insert(file.local_path.as_str()) {
            bail!("duplicate E2B upstream lock path {}", file.local_path);
        }
        if !source_lock.sources.contains_key(&file.source) {
            bail!(
                "E2B upstream lock file {} names unknown source {}",
                file.local_path,
                file.source
            );
        }
        if file.source_path.trim().is_empty() {
            bail!(
                "E2B upstream lock file {} has an empty source path",
                file.local_path
            );
        }
        let bytes = std::fs::read(paths.root.join(&file.local_path)).with_context(|| {
            format!(
                "failed to read vendored E2B source {}",
                paths.root.join(&file.local_path).display()
            )
        })?;
        let actual = sha256(&bytes);
        if actual != file.sha256 {
            bail!(
                "vendored E2B source {} digest mismatch: expected {}, got {}",
                file.local_path,
                file.sha256,
                actual
            );
        }
    }
    for required in [
        CONTROL_OPENAPI,
        ENVD_OPENAPI,
        VOLUME_OPENAPI,
        PROCESS_PROTO,
        FILESYSTEM_PROTO,
        MCP_SCHEMA,
    ] {
        if !locked_paths.contains(required) {
            bail!("E2B upstream lock is missing required contract {required}");
        }
    }
    Ok(())
}

fn validate_relative_path(path: &str) -> Result<()> {
    let path = Path::new(path);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!(
            "E2B upstream lock contains unsafe local path {}",
            path.display()
        );
    }
    Ok(())
}

fn build_manifest(
    source_lock: &SourceLock,
    inventory: &ContractInventory,
    contract_inventory_bytes: &[u8],
    public_export_bytes: &[u8],
) -> Result<CompatibilityManifest> {
    let e2b = source_lock
        .sources
        .get("e2b")
        .context("E2B source disappeared after lock validation")?;
    let interpreter = source_lock
        .sources
        .get("code-interpreter")
        .context("code-interpreter source disappeared after lock validation")?;
    let file_digests = source_lock
        .files
        .iter()
        .map(|file| (file.local_path.as_str(), file.sha256.clone()))
        .collect::<BTreeMap<_, _>>();
    let proto_digests = inventory
        .protobuf
        .iter()
        .map(|file| (file.path.as_str(), file.descriptor_digest.clone()))
        .collect::<BTreeMap<_, _>>();

    Ok(CompatibilityManifest {
        schema_version: 1,
        compatibility_id: source_lock.compatibility.id.clone(),
        status: "contract-fixture".to_string(),
        full_compatibility: false,
        e2b_git_commit: e2b.commit.clone(),
        code_interpreter_git_commit: interpreter.commit.clone(),
        python_e2b_version: package(e2b, "python")?,
        typescript_e2b_version: package(e2b, "typescript")?,
        python_code_interpreter_version: package(interpreter, "python")?,
        typescript_code_interpreter_version: package(interpreter, "typescript")?,
        control_openapi_digest: locked_digest(&file_digests, CONTROL_OPENAPI)?,
        envd_openapi_digest: locked_digest(&file_digests, ENVD_OPENAPI)?,
        volume_content_openapi_digest: locked_digest(&file_digests, VOLUME_OPENAPI)?,
        process_descriptor_digest: descriptor_digest(&proto_digests, "process/process.proto")?,
        filesystem_descriptor_digest: descriptor_digest(
            &proto_digests,
            "filesystem/filesystem.proto",
        )?,
        mcp_schema_digest: locked_digest(&file_digests, MCP_SCHEMA)?,
        contract_inventory_digest: sha256(contract_inventory_bytes),
        public_export_inventory_digest: sha256(public_export_bytes),
        a3s_compat_version: source_lock.compatibility.version.clone(),
    })
}

fn package(source: &crate::model::UpstreamSource, language: &str) -> Result<String> {
    source
        .packages
        .get(language)
        .cloned()
        .with_context(|| format!("validated source lost {language} package version"))
}

fn locked_digest(digests: &BTreeMap<&str, String>, path: &str) -> Result<String> {
    digests
        .get(path)
        .cloned()
        .with_context(|| format!("validated E2B upstream lock lost contract {path}"))
}

fn descriptor_digest(digests: &BTreeMap<&str, String>, path: &str) -> Result<String> {
    digests
        .get(path)
        .cloned()
        .with_context(|| format!("generated Protobuf inventory lost descriptor {path}"))
}

fn pretty_json(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value).context("failed to serialize fixture JSON")?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn write_generated(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create generated fixture directory {}",
                parent.display()
            )
        })?;
    }
    std::fs::write(path, bytes)
        .with_context(|| format!("failed to write generated fixture {}", path.display()))
}

fn verify_generated(path: &Path, expected: &[u8], description: &str) -> Result<()> {
    let actual = std::fs::read(path).with_context(|| {
        format!(
            "missing generated E2B {description} {}; run a3s-box-e2b-contract generate",
            path.display()
        )
    })?;
    if actual != expected {
        bail!(
            "generated E2B {description} is stale at {}; run a3s-box-e2b-contract generate",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_in_e2b_fixture_is_current() {
        verify_fixture(&FixturePaths::repository_default())
            .expect("checked-in E2B compatibility fixture should be current");
    }

    #[test]
    fn pinned_fixture_covers_protocol_and_cross_language_entrypoints() {
        let paths = FixturePaths::repository_default();
        let contracts: serde_json::Value = serde_json::from_slice(
            &std::fs::read(paths.contract_inventory()).expect("read contract inventory"),
        )
        .expect("parse contract inventory");
        let openapi = contracts["openapi"].as_array().expect("OpenAPI contracts");
        let control = openapi
            .iter()
            .find(|contract| contract["name"] == "control-plane")
            .expect("control-plane contract");
        let operations = control["operations"].as_array().expect("HTTP operations");
        assert!(operations.iter().any(|operation| {
            operation["method"] == "POST" && operation["path"] == "/sandboxes"
        }));
        assert!(operations.iter().any(|operation| {
            operation["method"] == "DELETE" && operation["path"] == "/sandboxes/{sandboxID}"
        }));
        assert!(!operations.iter().any(|operation| {
            operation["path"]
                .as_str()
                .is_some_and(|path| path.starts_with("/admin") || path.starts_with("/nodes"))
        }));
        let envd = openapi
            .iter()
            .find(|contract| contract["name"] == "envd")
            .expect("envd contract");
        assert!(envd["authentication_headers"]
            .as_array()
            .expect("envd authentication headers")
            .iter()
            .any(|header| header == "X-Access-Token"));

        let protobuf = contracts["protobuf"]
            .as_array()
            .expect("Protobuf contracts");
        let process = protobuf
            .iter()
            .find(|contract| contract["path"] == "process/process.proto")
            .expect("Process contract");
        let process_methods = process["services"][0]["methods"]
            .as_array()
            .expect("Process methods");
        assert!(process_methods
            .iter()
            .any(|method| method["name"] == "Start" && method["server_streaming"] == true));
        assert!(process_methods.iter().any(|method| {
            method["name"] == "StreamInput" && method["client_streaming"] == true
        }));

        let exports: serde_json::Value = serde_json::from_slice(
            &std::fs::read(paths.public_exports()).expect("read public export inventory"),
        )
        .expect("parse public export inventory");
        let packages = &exports["packages"];
        assert!(has_symbol(&packages["python-e2b"]["symbols"], "Sandbox"));
        assert!(has_symbol(
            &packages["python-e2b"]["symbols"],
            "AsyncSandbox"
        ));
        assert!(has_symbol(
            &packages["typescript-e2b"]["symbols"],
            "Sandbox"
        ));
        assert!(has_symbol(
            &packages["typescript-e2b"]["type_only_symbols"],
            "SandboxInfo"
        ));
        assert!(has_symbol(
            &packages["python-code-interpreter"]["symbols"],
            "Execution"
        ));
        assert!(has_symbol(
            &packages["typescript-code-interpreter"]["type_only_symbols"],
            "Execution"
        ));
    }

    fn has_symbol(symbols: &serde_json::Value, expected: &str) -> bool {
        symbols
            .as_array()
            .is_some_and(|symbols| symbols.iter().any(|symbol| symbol == expected))
    }

    #[test]
    fn rejects_parent_directory_in_locked_path() {
        assert!(validate_relative_path("../secret").is_err());
        assert!(validate_relative_path("/absolute").is_err());
        assert!(validate_relative_path("spec/e2b/openapi.yml").is_ok());
    }
}
