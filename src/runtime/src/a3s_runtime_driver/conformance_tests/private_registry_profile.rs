use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use a3s_runtime::contract::{
    ArtifactRef, RuntimeInspection, RuntimeUnitState, SecretReference, SecretTarget,
};
use a3s_runtime::{FileRuntimeStateStore, RuntimeClient};
use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, Request, Response, StatusCode};
use axum::routing::any;
use axum::Router;
use base64::Engine as _;
use serde_json::json;
use sha2::Digest as _;

use super::fixture::BoxRuntimeConformanceFixture;
use super::{
    external, protocol, require, Result, PRIVATE_REGISTRY_PASSWORD,
    PRIVATE_REGISTRY_SECRET_REFERENCE, PRIVATE_REGISTRY_USERNAME,
};

const REPOSITORY: &str = "a3s/private-runtime";
const OCI_IMAGE_INDEX: &str = "application/vnd.oci.image.index.v1+json";

#[derive(Clone)]
struct RegistryContent {
    bytes: Vec<u8>,
    digest: String,
    media_type: String,
}

#[derive(Clone, Debug)]
struct RecordedRequest {
    authorized: bool,
    path: String,
}

#[derive(Clone)]
struct RegistryState {
    expected_authorization: String,
    manifests: Arc<HashMap<String, RegistryContent>>,
    blobs: Arc<HashMap<String, RegistryContent>>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

struct PrivateRegistry {
    authority: String,
    index_digest: String,
    manifest_digest: String,
    config_digest: String,
    layer_digests: Vec<String>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    task: tokio::task::JoinHandle<()>,
}

impl PrivateRegistry {
    async fn start(source_layout: &Path) -> Result<Self> {
        let source_index = read_json(&source_layout.join("index.json"), "source OCI index")?;
        let selected = source_index
            .get("manifests")
            .and_then(serde_json::Value::as_array)
            .and_then(|manifests| manifests.first())
            .ok_or_else(|| protocol("cached source OCI index has no selected manifest"))?;
        let manifest_digest = descriptor_digest(selected, "selected manifest")?;
        let manifest_media_type = descriptor_media_type(selected, "selected manifest")?;
        let manifest_bytes = read_blob(source_layout, &manifest_digest)?;
        let manifest = serde_json::from_slice::<serde_json::Value>(&manifest_bytes)
            .map_err(|error| external("decode cached source image manifest", error))?;

        let config = manifest
            .get("config")
            .ok_or_else(|| protocol("cached source image manifest has no config descriptor"))?;
        let config_digest = descriptor_digest(config, "image config")?;
        let config_media_type = descriptor_media_type(config, "image config")?;
        let config_bytes = read_blob(source_layout, &config_digest)?;

        let layers = manifest
            .get("layers")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| protocol("cached source image manifest has no layer descriptors"))?;
        require(
            !layers.is_empty(),
            "cached source image has no filesystem layers",
        )?;
        let mut blobs = HashMap::from([(
            config_digest.clone(),
            RegistryContent {
                bytes: config_bytes,
                digest: config_digest.clone(),
                media_type: config_media_type,
            },
        )]);
        let mut layer_digests = Vec::with_capacity(layers.len());
        for layer in layers {
            let digest = descriptor_digest(layer, "image layer")?;
            let media_type = descriptor_media_type(layer, "image layer")?;
            let bytes = read_blob(source_layout, &digest)?;
            blobs.insert(
                digest.clone(),
                RegistryContent {
                    bytes,
                    digest: digest.clone(),
                    media_type,
                },
            );
            layer_digests.push(digest);
        }

        let index_bytes = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": OCI_IMAGE_INDEX,
            "manifests": [{
                "mediaType": manifest_media_type,
                "digest": manifest_digest,
                "size": manifest_bytes.len(),
                "platform": {
                    "architecture": oci_architecture(),
                    "os": "linux"
                }
            }]
        }))
        .map_err(|error| external("encode private-registry image index", error))?;
        let index_digest = digest(&index_bytes);
        let manifests = HashMap::from([
            (
                index_digest.clone(),
                RegistryContent {
                    bytes: index_bytes,
                    digest: index_digest.clone(),
                    media_type: OCI_IMAGE_INDEX.into(),
                },
            ),
            (
                manifest_digest.clone(),
                RegistryContent {
                    bytes: manifest_bytes,
                    digest: manifest_digest.clone(),
                    media_type: manifest_media_type,
                },
            ),
        ]);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = RegistryState {
            expected_authorization: format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!(
                    "{PRIVATE_REGISTRY_USERNAME}:{PRIVATE_REGISTRY_PASSWORD}"
                ))
            ),
            manifests: Arc::new(manifests),
            blobs: Arc::new(blobs),
            requests: Arc::clone(&requests),
        };
        let app = Router::new()
            .route("/*path", any(registry_handler))
            .with_state(state);
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .map_err(|error| external("bind private OCI registry", error))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| external("configure private OCI registry listener", error))?;
        let authority = listener
            .local_addr()
            .map_err(|error| external("inspect private OCI registry listener", error))?
            .to_string();
        let task = tokio::spawn(async move {
            if let Err(error) = axum::Server::from_tcp(listener)
                .expect("private registry listener must remain valid")
                .serve(app.into_make_service())
                .await
            {
                tracing::error!(%error, "Private registry conformance server failed");
            }
        });

        Ok(Self {
            authority,
            index_digest,
            manifest_digest,
            config_digest,
            layer_digests,
            requests,
            task,
        })
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for PrivateRegistry {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) async fn run(fixture: &BoxRuntimeConformanceFixture) -> Result<()> {
    require(
        std::env::var("A3S_REGISTRY_PROTOCOL").as_deref() == Ok("http"),
        "set A3S_REGISTRY_PROTOCOL=http only for the loopback private-registry gate",
    )?;

    let result = run_inner(fixture).await;
    let cleanup = fixture.cleanup_registered().await;
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup_error)) => Err(protocol(format!(
            "private-registry gate failed: {error}; cleanup also failed: {cleanup_error}"
        ))),
    }
}

async fn run_inner(fixture: &BoxRuntimeConformanceFixture) -> Result<()> {
    let source_reference = std::env::var("A3S_BOX_RUNTIME_CONFORMANCE_IMAGE")
        .map_err(|error| external("read cached R17 image reference", error))?;
    let source_store = crate::ImageStore::new(
        &fixture.home_dir.join("images"),
        crate::DEFAULT_IMAGE_CACHE_SIZE,
    )
    .map_err(|error| external("open cached R17 image store", error))?;
    let source = source_store
        .get(&source_reference)
        .await
        .ok_or_else(|| protocol("pinned R17 source image was not cached before the HTTP gate"))?;
    let registry = PrivateRegistry::start(&source.path).await?;

    let private_home = fixture
        .home_dir
        .parent()
        .ok_or_else(|| protocol("R17 home has no parent for private-registry isolation"))?
        .join(format!(
            "a3s-runtime-conformance-private-{}",
            uuid::Uuid::new_v4().simple()
        ));
    std::fs::create_dir(&private_home)
        .map_err(|error| external("create private-registry provider home", error))?;
    fixture.register_provider_home(private_home.clone());
    let state_root = private_home.join("runtime-state");
    fixture.register_state_root(state_root.clone());
    let driver = fixture.private_registry_driver(private_home.clone())?;
    let client = fixture.client_with(
        driver.clone(),
        Arc::new(FileRuntimeStateStore::new(&state_root)),
    );

    let image_reference = format!(
        "{}/{REPOSITORY}@{}",
        registry.authority, registry.index_digest
    );
    let mut request = fixture.cases.service(
        "security-private-registry",
        "printf 'r17-private-registry-ready\\n'; exec sleep 3600",
    );
    request.spec.artifact = ArtifactRef {
        uri: format!("oci://{image_reference}"),
        digest: registry.index_digest.clone(),
        media_type: OCI_IMAGE_INDEX.into(),
    };
    request.spec.secrets = vec![SecretReference {
        name: "private-registry-credential".into(),
        reference: PRIVATE_REGISTRY_SECRET_REFERENCE.into(),
        target: SecretTarget::RegistryCredential,
    }];
    request.validate().map_err(super::invalid)?;

    let calls_before = fixture.secret_materialization_calls();
    let running = client.apply(&request).await?;
    require(
        running.state == RuntimeUnitState::Running,
        "private-registry Service did not reach running",
    )?;
    require(
        fixture.secret_materialization_calls() == calls_before + 1,
        "private-registry credential was not resolved exactly once",
    )?;
    require(
        driver
            .transient_registry_auth
            .as_ref()
            .is_some_and(|broker| broker.pending() == 0),
        "private-registry authorization remained in the transient broker after pull",
    )?;

    let requests = registry.requests();
    let protected = requests
        .iter()
        .filter(|request| request.path != "/v2/")
        .collect::<Vec<_>>();
    require(
        protected.iter().any(|request| request.authorized),
        "private registry never observed an authorized protected request",
    )?;
    for path in [
        format!("/v2/{REPOSITORY}/manifests/{}", registry.index_digest),
        format!("/v2/{REPOSITORY}/manifests/{}", registry.manifest_digest),
        format!("/v2/{REPOSITORY}/blobs/{}", registry.config_digest),
    ] {
        require(
            protected
                .iter()
                .any(|request| request.path == path && request.authorized),
            format!("private-registry pull omitted {path}"),
        )?;
    }
    for digest in &registry.layer_digests {
        let path = format!("/v2/{REPOSITORY}/blobs/{digest}");
        require(
            protected
                .iter()
                .any(|request| request.path == path && request.authorized),
            format!("private-registry pull omitted layer {digest}"),
        )?;
    }

    let record = driver
        .manager
        .managed_records()
        .await
        .map_err(|error| external("load private-registry Box record", error))?
        .into_iter()
        .find(|record| {
            record.labels.get(super::super::metadata::UNIT_LABEL) == Some(&request.spec.unit_id)
        })
        .ok_or_else(|| protocol("private-registry execution record was not retained"))?;
    let creation_intent = serde_json::to_vec(
        &record
            .managed_execution
            .as_ref()
            .ok_or_else(|| protocol("private-registry record lost creation intent"))?
            .request,
    )
    .map_err(|error| external("encode private-registry creation intent", error))?;
    require(
        !contains_registry_plaintext(&creation_intent),
        "private-registry creation intent persisted credential plaintext",
    )?;
    require(
        !private_home.join("auth/credentials.json").exists(),
        "Runtime Secret credential was copied into the Box credential store",
    )?;
    if let Some(path) = find_plaintext_file(&private_home)? {
        return Err(protocol(format!(
            "private-registry credential plaintext persisted in {}",
            path.display()
        )));
    }

    let stopped = client
        .stop(
            &fixture
                .cases
                .action("security-private-registry-stop", &request.spec),
        )
        .await?;
    require(
        matches!(
            stopped,
            RuntimeInspection::Found { ref observation, .. }
                if observation.state == RuntimeUnitState::Stopped
        ),
        "private-registry Service did not stop cleanly",
    )?;
    client
        .remove(
            &fixture
                .cases
                .action("security-private-registry-remove", &request.spec),
        )
        .await?;
    require(
        driver
            .manager
            .managed_records()
            .await
            .map_err(|error| external("load removed private-registry inventory", error))?
            .is_empty(),
        "private-registry removal retained a Box execution record",
    )?;
    require(
        driver
            .transient_registry_auth
            .as_ref()
            .is_some_and(|broker| broker.pending() == 0),
        "private-registry removal left a transient authorization",
    )?;
    if let Some(path) = find_plaintext_file(&private_home)? {
        return Err(protocol(format!(
            "private-registry removal left credential plaintext in {}",
            path.display()
        )));
    }
    Ok(())
}

async fn registry_handler(
    State(state): State<RegistryState>,
    request: Request<Body>,
) -> Response<Body> {
    let path = request.uri().path().to_string();
    let authorized = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some(state.expected_authorization.as_str());
    state.requests.lock().unwrap().push(RecordedRequest {
        authorized,
        path: path.clone(),
    });

    if request.method() != Method::GET {
        return response(StatusCode::METHOD_NOT_ALLOWED, None, None, Vec::new());
    }
    if path == "/v2/" {
        return response(StatusCode::OK, None, None, Vec::new());
    }
    if !authorized {
        return Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("www-authenticate", "Basic realm=\"A3S R17 Registry\"")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"errors":[{"code":"UNAUTHORIZED","message":"authentication required","detail":{}}]}"#,
            ))
            .expect("static private-registry rejection response must be valid");
    }

    let manifest_prefix = format!("/v2/{REPOSITORY}/manifests/");
    if let Some(reference) = path.strip_prefix(&manifest_prefix) {
        return state.manifests.get(reference).map_or_else(
            || response(StatusCode::NOT_FOUND, None, None, Vec::new()),
            |content| {
                response(
                    StatusCode::OK,
                    Some(&content.media_type),
                    Some(&content.digest),
                    content.bytes.clone(),
                )
            },
        );
    }
    let blob_prefix = format!("/v2/{REPOSITORY}/blobs/");
    if let Some(reference) = path.strip_prefix(&blob_prefix) {
        return state.blobs.get(reference).map_or_else(
            || response(StatusCode::NOT_FOUND, None, None, Vec::new()),
            |content| {
                response(
                    StatusCode::OK,
                    Some(&content.media_type),
                    Some(&content.digest),
                    content.bytes.clone(),
                )
            },
        );
    }
    response(StatusCode::NOT_FOUND, None, None, Vec::new())
}

fn response(
    status: StatusCode,
    media_type: Option<&str>,
    digest: Option<&str>,
    bytes: Vec<u8>,
) -> Response<Body> {
    let mut builder = Response::builder().status(status);
    if let Some(media_type) = media_type {
        builder = builder.header("content-type", media_type);
    }
    if let Some(digest) = digest {
        builder = builder.header("docker-content-digest", digest);
    }
    builder
        .body(Body::from(bytes))
        .expect("private-registry fixture response must be valid")
}

fn read_json(path: &Path, label: &str) -> Result<serde_json::Value> {
    let bytes = std::fs::read(path).map_err(|error| external(label, error))?;
    serde_json::from_slice(&bytes).map_err(|error| external(label, error))
}

fn read_blob(layout: &Path, digest: &str) -> Result<Vec<u8>> {
    let digest = digest
        .strip_prefix("sha256:")
        .ok_or_else(|| protocol("source OCI descriptor is not a SHA-256 digest"))?;
    require(
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "source OCI descriptor has an invalid SHA-256 digest",
    )?;
    std::fs::read(layout.join("blobs/sha256").join(digest))
        .map_err(|error| external("read cached source OCI blob", error))
}

fn descriptor_digest(value: &serde_json::Value, label: &str) -> Result<String> {
    value
        .get("digest")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| protocol(format!("{label} has no digest")))
}

fn descriptor_media_type(value: &serde_json::Value, label: &str) -> Result<String> {
    value
        .get("mediaType")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| protocol(format!("{label} has no media type")))
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", sha2::Sha256::digest(bytes))
}

fn oci_architecture() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        architecture => architecture,
    }
}

fn contains_registry_plaintext(bytes: &[u8]) -> bool {
    [PRIVATE_REGISTRY_USERNAME, PRIVATE_REGISTRY_PASSWORD]
        .iter()
        .any(|secret| {
            bytes
                .windows(secret.len())
                .any(|window| window == secret.as_bytes())
        })
}

fn find_plaintext_file(root: &Path) -> Result<Option<PathBuf>> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| external("inspect private-registry persistence path", error))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            for entry in std::fs::read_dir(&path)
                .map_err(|error| external("scan private-registry persistence directory", error))?
            {
                pending.push(
                    entry
                        .map_err(|error| external("read private-registry directory entry", error))?
                        .path(),
                );
            }
        } else if metadata.is_file() {
            let bytes = std::fs::read(&path)
                .map_err(|error| external("scan private-registry persistence file", error))?;
            if contains_registry_plaintext(&bytes) {
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}
