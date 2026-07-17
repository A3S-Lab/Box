use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, Request, Response, StatusCode};
use axum::routing::any;
use axum::Router;
use base64::Engine as _;
use serde_json::json;
use sha2::Digest as _;

use super::super::pull::ImagePuller;
use super::super::store::ImageStore;
use super::{ImageReference, RegistryAuth, RegistryProtocol, RegistryPuller};

const USERNAME: &str = "fixture-user";
const PASSWORD: &str = "fixture-secret-password";
const REPOSITORY: &str = "a3s/app";

#[derive(Clone, Debug)]
struct RecordedRequest {
    path: String,
    authorization: Option<String>,
}

#[derive(Clone)]
struct RegistryFixtureState {
    expected_authorization: String,
    manifests: Arc<HashMap<String, FixtureContent>>,
    blobs: Arc<HashMap<String, FixtureContent>>,
    redirected_layer_digest: String,
    public_manifests: Arc<AtomicBool>,
    external_layer_redirect: Arc<Mutex<Option<String>>>,
    corrupt_manifest: Arc<Mutex<Option<String>>>,
    corrupt_blob: Arc<Mutex<Option<String>>>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

#[derive(Clone)]
struct FixtureContent {
    bytes: Vec<u8>,
    media_type: &'static str,
    digest: String,
}

struct RegistryFixture {
    reference: ImageReference,
    index_digest: String,
    manifest_digest: String,
    config_digest: String,
    layer_digest: String,
    manifest_bytes: Vec<u8>,
    config_bytes: Vec<u8>,
    layer_bytes: Vec<u8>,
    public_manifests: Arc<AtomicBool>,
    external_layer_redirect: Arc<Mutex<Option<String>>>,
    corrupt_manifest: Arc<Mutex<Option<String>>>,
    corrupt_blob: Arc<Mutex<Option<String>>>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    task: tokio::task::JoinHandle<()>,
}

impl RegistryFixture {
    async fn start() -> Self {
        let config_bytes = serde_json::to_vec(&json!({
            "architecture": "amd64",
            "os": "linux",
            "config": {"Cmd": ["sh", "-c", "echo fixture-ok"]},
            "rootfs": {"type": "layers", "diff_ids": []},
            "history": []
        }))
        .unwrap();
        let layer_bytes = b"streamed-layer-payload".repeat(256);
        let config_digest = digest(&config_bytes);
        let layer_digest = digest(&layer_bytes);

        let manifest_bytes = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": config_digest,
                "size": config_bytes.len()
            },
            "layers": [{
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": layer_digest,
                "size": layer_bytes.len()
            }]
        }))
        .unwrap();
        let manifest_digest = digest(&manifest_bytes);

        let wrong_manifest = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": config_digest,
                "size": config_bytes.len()
            },
            "layers": []
        }))
        .unwrap();
        let wrong_manifest_digest = digest(&wrong_manifest);

        let index_bytes = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": wrong_manifest_digest,
                    "size": wrong_manifest.len(),
                    "platform": {"architecture": "arm64", "os": "linux"}
                },
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": manifest_digest,
                    "size": manifest_bytes.len(),
                    "platform": {"architecture": "amd64", "os": "linux"}
                }
            ]
        }))
        .unwrap();
        let index_digest = digest(&index_bytes);

        let manifests = HashMap::from([
            (
                "latest".to_string(),
                FixtureContent {
                    bytes: index_bytes,
                    media_type: "application/vnd.oci.image.index.v1+json",
                    digest: index_digest.clone(),
                },
            ),
            (
                manifest_digest.clone(),
                FixtureContent {
                    bytes: manifest_bytes.clone(),
                    media_type: "application/vnd.oci.image.manifest.v1+json",
                    digest: manifest_digest.clone(),
                },
            ),
            (
                wrong_manifest_digest.clone(),
                FixtureContent {
                    bytes: wrong_manifest,
                    media_type: "application/vnd.oci.image.manifest.v1+json",
                    digest: wrong_manifest_digest,
                },
            ),
        ]);
        let blobs = HashMap::from([
            (
                config_digest.clone(),
                FixtureContent {
                    bytes: config_bytes.clone(),
                    media_type: "application/octet-stream",
                    digest: config_digest.clone(),
                },
            ),
            (
                layer_digest.clone(),
                FixtureContent {
                    bytes: layer_bytes.clone(),
                    media_type: "application/octet-stream",
                    digest: layer_digest.clone(),
                },
            ),
        ]);

        let requests = Arc::new(Mutex::new(Vec::new()));
        let public_manifests = Arc::new(AtomicBool::new(false));
        let external_layer_redirect = Arc::new(Mutex::new(None));
        let corrupt_manifest = Arc::new(Mutex::new(None));
        let corrupt_blob = Arc::new(Mutex::new(None));
        let state = RegistryFixtureState {
            expected_authorization: format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{USERNAME}:{PASSWORD}"))
            ),
            manifests: Arc::new(manifests),
            blobs: Arc::new(blobs),
            redirected_layer_digest: layer_digest.clone(),
            public_manifests: Arc::clone(&public_manifests),
            external_layer_redirect: Arc::clone(&external_layer_redirect),
            corrupt_manifest: Arc::clone(&corrupt_manifest),
            corrupt_blob: Arc::clone(&corrupt_blob),
            requests: Arc::clone(&requests),
        };
        let app = Router::new()
            .route("/*path", any(registry_handler))
            .with_state(state);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::Server::from_tcp(listener)
                .unwrap()
                .serve(app.into_make_service())
                .await
                .unwrap();
        });

        Self {
            reference: ImageReference {
                registry: addr.to_string(),
                repository: REPOSITORY.to_string(),
                tag: Some("latest".to_string()),
                digest: None,
            },
            index_digest,
            manifest_digest,
            config_digest,
            layer_digest,
            manifest_bytes,
            config_bytes,
            layer_bytes,
            public_manifests,
            external_layer_redirect,
            corrupt_manifest,
            corrupt_blob,
            requests,
            task,
        }
    }

    fn request_snapshot(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn corrupt_manifest(&self, reference: &str) {
        *self.corrupt_manifest.lock().unwrap() = Some(reference.to_string());
    }

    fn corrupt_blob(&self, digest: &str) {
        *self.corrupt_blob.lock().unwrap() = Some(digest.to_string());
    }

    fn redirect_layer_to(&self, location: String) {
        *self.external_layer_redirect.lock().unwrap() = Some(location);
    }

    fn allow_anonymous_manifests(&self) {
        self.public_manifests.store(true, Ordering::Relaxed);
    }
}

impl Drop for RegistryFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone)]
struct RedirectTargetState {
    content: FixtureContent,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

struct RedirectTarget {
    url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    task: tokio::task::JoinHandle<()>,
}

impl RedirectTarget {
    async fn start(bytes: Vec<u8>, digest: String) -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = RedirectTargetState {
            content: FixtureContent {
                bytes,
                media_type: "application/octet-stream",
                digest,
            },
            requests: Arc::clone(&requests),
        };
        let app = Router::new()
            .route("/external-layer", any(redirect_target_handler))
            .with_state(state);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::Server::from_tcp(listener)
                .unwrap()
                .serve(app.into_make_service())
                .await
                .unwrap();
        });
        Self {
            url: format!("http://{addr}/external-layer"),
            requests,
            task,
        }
    }
}

impl Drop for RedirectTarget {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn redirect_target_handler(
    State(state): State<RedirectTargetState>,
    request: Request<Body>,
) -> Response<Body> {
    state.requests.lock().unwrap().push(RecordedRequest {
        path: request.uri().path().to_string(),
        authorization: request
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
    });
    response(
        StatusCode::OK,
        Some(state.content.media_type),
        Some(&state.content.digest),
        state.content.bytes,
    )
}

async fn registry_handler(
    State(state): State<RegistryFixtureState>,
    request: Request<Body>,
) -> Response<Body> {
    let path = request.uri().path().to_string();
    let authorization = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    state.requests.lock().unwrap().push(RecordedRequest {
        path: path.clone(),
        authorization: authorization.clone(),
    });

    if request.method() != Method::GET {
        return response(StatusCode::METHOD_NOT_ALLOWED, None, None, Vec::new());
    }
    if path == "/v2/" {
        // This registry advertises Basic only on protected resources. That is
        // the production behavior that oci-distribution does not negotiate.
        return response(StatusCode::OK, None, None, Vec::new());
    }
    let manifest_prefix = format!("/v2/{REPOSITORY}/manifests/");
    let anonymous_manifest_allowed =
        state.public_manifests.load(Ordering::Relaxed) && path.starts_with(&manifest_prefix);
    if authorization.as_deref() != Some(&state.expected_authorization)
        && !anonymous_manifest_allowed
    {
        return Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("www-authenticate", "Basic realm=\"A3S OCI Registry\"")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"errors":[{"code":"UNAUTHORIZED","message":"authentication required","detail":{}}]}"#,
            ))
            .unwrap();
    }

    if let Some(reference) = path.strip_prefix(&manifest_prefix) {
        return match state.manifests.get(reference) {
            Some(content) => {
                let mut bytes = content.bytes.clone();
                if state.corrupt_manifest.lock().unwrap().as_deref() == Some(reference) {
                    bytes[0] ^= 0x01;
                }
                response(
                    StatusCode::OK,
                    Some(content.media_type),
                    Some(&content.digest),
                    bytes,
                )
            }
            None => response(StatusCode::NOT_FOUND, None, None, Vec::new()),
        };
    }

    let blob_prefix = format!("/v2/{REPOSITORY}/blobs/");
    if let Some(blob_digest) = path.strip_prefix(&blob_prefix) {
        if blob_digest == state.redirected_layer_digest {
            let location = state
                .external_layer_redirect
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| format!("/redirected/{blob_digest}"));
            return Response::builder()
                .status(StatusCode::TEMPORARY_REDIRECT)
                .header("location", location)
                .body(Body::empty())
                .unwrap();
        }
        return match state.blobs.get(blob_digest) {
            Some(content) => blob_response(&state, blob_digest, content),
            None => response(StatusCode::NOT_FOUND, None, None, Vec::new()),
        };
    }

    if let Some(blob_digest) = path.strip_prefix("/redirected/") {
        return match state.blobs.get(blob_digest) {
            Some(content) => blob_response(&state, blob_digest, content),
            None => response(StatusCode::NOT_FOUND, None, None, Vec::new()),
        };
    }

    response(StatusCode::NOT_FOUND, None, None, Vec::new())
}

fn blob_response(
    state: &RegistryFixtureState,
    digest: &str,
    content: &FixtureContent,
) -> Response<Body> {
    let mut bytes = content.bytes.clone();
    if state.corrupt_blob.lock().unwrap().as_deref() == Some(digest) {
        bytes[0] ^= 0x01;
    }
    response(
        StatusCode::OK,
        Some(content.media_type),
        Some(&content.digest),
        bytes,
    )
}

fn response(
    status: StatusCode,
    media_type: Option<&str>,
    digest: Option<&str>,
    body: Vec<u8>,
) -> Response<Body> {
    let mut builder = Response::builder().status(status);
    if let Some(media_type) = media_type {
        builder = builder.header("content-type", media_type);
    }
    if let Some(digest) = digest {
        builder = builder.header("docker-content-digest", digest);
    }
    builder.body(Body::from(body)).unwrap()
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", sha2::Sha256::digest(bytes))
}

fn assert_blob(path: &Path, digest: &str, expected: &[u8]) {
    let hex = digest.strip_prefix("sha256:").unwrap();
    assert_eq!(
        std::fs::read(path.join("blobs/sha256").join(hex)).unwrap(),
        expected
    );
}

#[tokio::test]
async fn basic_challenge_pull_resolves_index_streams_blobs_and_follows_redirect() {
    let fixture = RegistryFixture::start().await;
    let puller = RegistryPuller::with_auth_arch_and_protocol(
        RegistryAuth::basic(USERNAME, PASSWORD),
        "amd64".to_string(),
        RegistryProtocol::Http,
    );
    let target = tempfile::tempdir().unwrap();

    assert_eq!(
        puller
            .pull_manifest_digest(&fixture.reference)
            .await
            .unwrap(),
        fixture.index_digest
    );
    puller
        .pull_with_store(&fixture.reference, target.path(), None)
        .await
        .unwrap();

    assert_blob(
        target.path(),
        &fixture.manifest_digest,
        &fixture.manifest_bytes,
    );
    assert_blob(target.path(), &fixture.config_digest, &fixture.config_bytes);
    assert_blob(target.path(), &fixture.layer_digest, &fixture.layer_bytes);
    let index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(target.path().join("index.json")).unwrap()).unwrap();
    assert_eq!(index["manifests"][0]["digest"], fixture.manifest_digest);

    let requests = fixture.request_snapshot();
    assert!(requests.iter().any(|request| {
        request.path.ends_with("/manifests/latest") && request.authorization.is_none()
    }));
    for suffix in [
        "/manifests/latest",
        &format!("/manifests/{}", fixture.manifest_digest),
        &format!("/blobs/{}", fixture.config_digest),
        &format!("/blobs/{}", fixture.layer_digest),
        &format!("/redirected/{}", fixture.layer_digest),
    ] {
        assert!(
            requests.iter().any(|request| {
                request.path.ends_with(suffix)
                    && request.authorization.as_deref()
                        == Some(&format!(
                            "Basic {}",
                            base64::engine::general_purpose::STANDARD
                                .encode(format!("{USERNAME}:{PASSWORD}"))
                        ))
            }),
            "missing authenticated request ending in {suffix}"
        );
    }
}

#[tokio::test]
async fn image_puller_common_to_explicit_pull_and_run_uses_basic_fallback() {
    let fixture = RegistryFixture::start().await;
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(ImageStore::new(root.path(), 10 * 1024 * 1024).unwrap());
    let registry_puller = RegistryPuller::with_auth_arch_and_protocol(
        RegistryAuth::basic(USERNAME, PASSWORD),
        "amd64".to_string(),
        RegistryProtocol::Http,
    );
    // Both `a3s-box pull` and run's implicit image preparation call this same
    // cache-first ImagePuller path.
    let puller = ImagePuller::with_registry_puller(Arc::clone(&store), registry_puller);
    let full_reference = fixture.reference.full_reference();

    let image = puller.pull(&full_reference).await.unwrap();

    assert_eq!(image.manifest_digest(), fixture.manifest_digest);
    let stored = store.get(&full_reference).await.unwrap();
    assert_eq!(stored.digest, fixture.index_digest);
    assert_blob(&stored.path, &fixture.config_digest, &fixture.config_bytes);
    assert_blob(&stored.path, &fixture.layer_digest, &fixture.layer_bytes);
}

#[tokio::test]
async fn cross_origin_blob_redirect_does_not_forward_basic_credentials() {
    let fixture = RegistryFixture::start().await;
    let redirect_target =
        RedirectTarget::start(fixture.layer_bytes.clone(), fixture.layer_digest.clone()).await;
    fixture.redirect_layer_to(redirect_target.url.clone());
    let puller = RegistryPuller::with_auth_arch_and_protocol(
        RegistryAuth::basic(USERNAME, PASSWORD),
        "amd64".to_string(),
        RegistryProtocol::Http,
    );
    let target = tempfile::tempdir().unwrap();

    puller
        .pull_with_store(&fixture.reference, target.path(), None)
        .await
        .unwrap();

    let requests = redirect_target.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/external-layer");
    assert!(requests[0].authorization.is_none());
}

#[tokio::test]
async fn blob_unauthorized_after_public_manifest_retries_with_basic() {
    let fixture = RegistryFixture::start().await;
    fixture.allow_anonymous_manifests();
    let puller = RegistryPuller::with_auth_arch_and_protocol(
        RegistryAuth::basic(USERNAME, PASSWORD),
        "amd64".to_string(),
        RegistryProtocol::Http,
    );
    let target = tempfile::tempdir().unwrap();

    puller
        .pull_with_store(&fixture.reference, target.path(), None)
        .await
        .unwrap();

    let config_path = format!("/v2/{REPOSITORY}/blobs/{}", fixture.config_digest);
    let requests = fixture.request_snapshot();
    assert!(requests
        .iter()
        .any(|request| { request.path == config_path && request.authorization.is_none() }));
    assert!(requests
        .iter()
        .any(|request| { request.path == config_path && request.authorization.is_some() }));
    assert_blob(target.path(), &fixture.config_digest, &fixture.config_bytes);
    assert_blob(target.path(), &fixture.layer_digest, &fixture.layer_bytes);
}

#[tokio::test]
async fn anonymous_pull_does_not_attempt_preemptive_basic() {
    let fixture = RegistryFixture::start().await;
    let puller = RegistryPuller::with_auth_arch_and_protocol(
        RegistryAuth::anonymous(),
        "amd64".to_string(),
        RegistryProtocol::Http,
    );

    let error = puller
        .pull_manifest_digest(&fixture.reference)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Failed to pull manifest"));
    assert!(fixture
        .request_snapshot()
        .iter()
        .all(|request| request.authorization.is_none()));
}

#[tokio::test]
async fn empty_basic_credentials_do_not_enable_preemptive_retry() {
    for auth in [
        RegistryAuth::basic("", PASSWORD),
        RegistryAuth::basic(USERNAME, ""),
    ] {
        let fixture = RegistryFixture::start().await;
        let puller = RegistryPuller::with_auth_arch_and_protocol(
            auth,
            "amd64".to_string(),
            RegistryProtocol::Http,
        );

        puller
            .pull_manifest_digest(&fixture.reference)
            .await
            .unwrap_err();
        assert!(fixture
            .request_snapshot()
            .iter()
            .all(|request| request.authorization.is_none()));
    }
}

#[tokio::test]
async fn failed_basic_pull_never_exposes_credentials() {
    let fixture = RegistryFixture::start().await;
    let wrong_username = "credential-user-must-not-leak";
    let wrong_password = "credential-password-must-not-leak";
    let puller = RegistryPuller::with_auth_arch_and_protocol(
        RegistryAuth::basic(wrong_username, wrong_password),
        "amd64".to_string(),
        RegistryProtocol::Http,
    );

    let message = puller
        .pull_manifest_digest(&fixture.reference)
        .await
        .unwrap_err()
        .to_string();
    assert!(!message.contains(wrong_username));
    assert!(!message.contains(wrong_password));
}

#[tokio::test]
async fn basic_pull_rejects_corrupted_manifest_bytes() {
    let fixture = RegistryFixture::start().await;
    fixture.corrupt_manifest("latest");
    let puller = RegistryPuller::with_auth_arch_and_protocol(
        RegistryAuth::basic(USERNAME, PASSWORD),
        "amd64".to_string(),
        RegistryProtocol::Http,
    );

    let message = puller
        .pull_manifest_digest(&fixture.reference)
        .await
        .unwrap_err()
        .to_string();
    assert!(message.contains("Docker-Content-Digest header digest mismatch"));
    assert!(!message.contains(USERNAME));
    assert!(!message.contains(PASSWORD));
}

#[tokio::test]
async fn basic_pull_rejects_corrupted_layer_and_removes_partial_blob() {
    let fixture = RegistryFixture::start().await;
    fixture.corrupt_blob(&fixture.layer_digest);
    let puller = RegistryPuller::with_auth_arch_and_protocol(
        RegistryAuth::basic(USERNAME, PASSWORD),
        "amd64".to_string(),
        RegistryProtocol::Http,
    );
    let target = tempfile::tempdir().unwrap();

    let message = puller
        .pull_with_store(&fixture.reference, target.path(), None)
        .await
        .unwrap_err()
        .to_string();
    assert!(message.contains("layer digest mismatch"));
    assert!(!message.contains(USERNAME));
    assert!(!message.contains(PASSWORD));

    let layer_hex = fixture.layer_digest.strip_prefix("sha256:").unwrap();
    let layer_path = target.path().join("blobs/sha256").join(layer_hex);
    assert!(!layer_path.exists());
    assert!(!layer_path.with_extension("partial").exists());
}
