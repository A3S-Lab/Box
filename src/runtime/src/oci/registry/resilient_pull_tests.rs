use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::header::{CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, RANGE};
use axum::http::{Method, Request, Response, StatusCode};
use axum::routing::any;
use axum::Router;
use serde_json::json;
use sha2::Digest as _;

use super::super::store::ImageStore;
use super::{
    ImageReference, PullProgress, PullProgressState, RegistryAuth, RegistryProtocol,
    RegistryPullPolicy, RegistryPuller,
};

const REPOSITORY: &str = "a3s/resilient";

#[derive(Clone, Copy)]
enum LayerFault {
    Normal,
    DropFirst { prefix_bytes: usize },
    Stall,
    ServiceUnavailable,
    Delay(Duration),
}

#[derive(Clone)]
struct BlobFixture {
    digest: String,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
struct LayerRequest {
    digest: String,
    range: Option<String>,
}

#[derive(Clone)]
struct RegistryState {
    manifest: BlobFixture,
    config: BlobFixture,
    layers: Arc<HashMap<String, BlobFixture>>,
    fault: LayerFault,
    requests: Arc<Mutex<Vec<LayerRequest>>>,
    active_layer_requests: Arc<AtomicUsize>,
    max_active_layer_requests: Arc<AtomicUsize>,
}

struct ResilientRegistryFixture {
    reference: ImageReference,
    layers: Vec<BlobFixture>,
    requests: Arc<Mutex<Vec<LayerRequest>>>,
    active_layer_requests: Arc<AtomicUsize>,
    max_active_layer_requests: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl ResilientRegistryFixture {
    async fn start(layer_bytes: Vec<Vec<u8>>, fault: LayerFault) -> Self {
        assert!(!layer_bytes.is_empty());
        let config_bytes = serde_json::to_vec(&json!({
            "architecture": "amd64",
            "os": "linux",
            "config": {},
            "rootfs": {"type": "layers", "diff_ids": []},
            "history": []
        }))
        .unwrap();
        let config = BlobFixture::new(config_bytes);
        let layers = layer_bytes
            .into_iter()
            .map(BlobFixture::new)
            .collect::<Vec<_>>();
        let manifest_bytes = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": config.digest,
                "size": config.bytes.len()
            },
            "layers": layers.iter().map(|layer| json!({
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": layer.digest,
                "size": layer.bytes.len()
            })).collect::<Vec<_>>()
        }))
        .unwrap();
        let manifest = BlobFixture::new(manifest_bytes);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let active_layer_requests = Arc::new(AtomicUsize::new(0));
        let max_active_layer_requests = Arc::new(AtomicUsize::new(0));
        let state = RegistryState {
            manifest,
            config,
            layers: Arc::new(
                layers
                    .iter()
                    .cloned()
                    .map(|layer| (layer.digest.clone(), layer))
                    .collect(),
            ),
            fault,
            requests: Arc::clone(&requests),
            active_layer_requests: Arc::clone(&active_layer_requests),
            max_active_layer_requests: Arc::clone(&max_active_layer_requests),
        };
        let app = Router::new()
            .route("/*path", any(registry_handler))
            .with_state(state);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::Server::from_tcp(listener)
                .unwrap()
                .serve(app.into_make_service())
                .await
                .unwrap();
        });

        Self {
            reference: ImageReference {
                registry: address.to_string(),
                repository: REPOSITORY.to_string(),
                tag: Some("latest".to_string()),
                digest: None,
            },
            layers,
            requests,
            active_layer_requests,
            max_active_layer_requests,
            task,
        }
    }

    fn requests(&self) -> Vec<LayerRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn max_active_layer_requests(&self) -> usize {
        self.max_active_layer_requests.load(Ordering::SeqCst)
    }

    fn active_layer_requests(&self) -> usize {
        self.active_layer_requests.load(Ordering::SeqCst)
    }
}

impl Drop for ResilientRegistryFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl BlobFixture {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            digest: digest(&bytes),
            bytes,
        }
    }
}

async fn registry_handler(
    State(state): State<RegistryState>,
    request: Request<Body>,
) -> Response<Body> {
    if request.method() != Method::GET {
        return empty_response(StatusCode::METHOD_NOT_ALLOWED);
    }
    let path = request.uri().path();
    if path == "/v2/" {
        return empty_response(StatusCode::OK);
    }
    if path == "/v2/a3s/resilient/manifests/latest" {
        return complete_response(
            StatusCode::OK,
            "application/vnd.oci.image.manifest.v1+json",
            &state.manifest,
        );
    }

    let blob_prefix = format!("/v2/{REPOSITORY}/blobs/");
    let Some(blob_digest) = path.strip_prefix(&blob_prefix) else {
        return empty_response(StatusCode::NOT_FOUND);
    };
    if blob_digest == state.config.digest {
        return complete_response(StatusCode::OK, "application/octet-stream", &state.config);
    }
    let Some(layer) = state.layers.get(blob_digest) else {
        return empty_response(StatusCode::NOT_FOUND);
    };
    let range = request
        .headers()
        .get(RANGE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let request_number = {
        let mut requests = state.requests.lock().unwrap();
        let number = requests
            .iter()
            .filter(|request| request.digest == blob_digest)
            .count()
            + 1;
        requests.push(LayerRequest {
            digest: blob_digest.to_string(),
            range: range.clone(),
        });
        number
    };

    match state.fault {
        LayerFault::DropFirst { prefix_bytes } if request_number == 1 => {
            let prefix_bytes = prefix_bytes.min(layer.bytes.len());
            Response::builder()
                .status(StatusCode::OK)
                .header(CONTENT_TYPE, "application/octet-stream")
                .header(CONTENT_LENGTH, layer.bytes.len().to_string())
                .body(Body::from(layer.bytes[..prefix_bytes].to_vec()))
                .unwrap()
        }
        LayerFault::Stall => stalled_response(layer.bytes.len()),
        LayerFault::ServiceUnavailable => empty_response(StatusCode::SERVICE_UNAVAILABLE),
        LayerFault::Delay(delay) => {
            let active = state.active_layer_requests.fetch_add(1, Ordering::SeqCst) + 1;
            state
                .max_active_layer_requests
                .fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(delay).await;
            state.active_layer_requests.fetch_sub(1, Ordering::SeqCst);
            ranged_response(layer, range.as_deref())
        }
        LayerFault::Normal | LayerFault::DropFirst { .. } => {
            ranged_response(layer, range.as_deref())
        }
    }
}

fn ranged_response(layer: &BlobFixture, range: Option<&str>) -> Response<Body> {
    let Some(range) = range else {
        return complete_response(StatusCode::OK, "application/octet-stream", layer);
    };
    let Some(offset) = range
        .strip_prefix("bytes=")
        .and_then(|value| value.strip_suffix('-'))
        .and_then(|value| value.parse::<usize>().ok())
    else {
        return empty_response(StatusCode::RANGE_NOT_SATISFIABLE);
    };
    if offset >= layer.bytes.len() {
        return empty_response(StatusCode::RANGE_NOT_SATISFIABLE);
    }
    let end = layer.bytes.len() - 1;
    Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(CONTENT_TYPE, "application/octet-stream")
        .header(CONTENT_LENGTH, (layer.bytes.len() - offset).to_string())
        .header(
            CONTENT_RANGE,
            format!("bytes {offset}-{end}/{}", layer.bytes.len()),
        )
        .body(Body::from(layer.bytes[offset..].to_vec()))
        .unwrap()
}

fn stalled_response(content_length: usize) -> Response<Body> {
    let (sender, body) = Body::channel();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        drop(sender);
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/octet-stream")
        .header(CONTENT_LENGTH, content_length.to_string())
        .body(body)
        .unwrap()
}

fn complete_response(
    status: StatusCode,
    media_type: &'static str,
    content: &BlobFixture,
) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, media_type)
        .header(CONTENT_LENGTH, content.bytes.len().to_string())
        .header("docker-content-digest", &content.digest)
        .body(Body::from(content.bytes.clone()))
        .unwrap()
}

fn empty_response(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .unwrap()
}

fn pull_policy(
    max_attempts: usize,
    no_progress_timeout: Duration,
    max_concurrent_downloads: usize,
) -> RegistryPullPolicy {
    RegistryPullPolicy::try_new(
        max_attempts,
        Duration::from_millis(1),
        Duration::from_millis(2),
        no_progress_timeout,
        max_concurrent_downloads,
    )
    .unwrap()
}

fn puller(policy: RegistryPullPolicy) -> RegistryPuller {
    RegistryPuller::with_auth_arch_and_protocol(
        RegistryAuth::anonymous(),
        "amd64".to_string(),
        RegistryProtocol::Http,
    )
    .with_pull_policy(policy)
}

fn blob_path(root: &Path, digest: &str) -> PathBuf {
    root.join("blobs/sha256")
        .join(digest.strip_prefix("sha256:").unwrap())
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", sha2::Sha256::digest(bytes))
}

async fn seed_blob(store: &ImageStore, blob_digest: &str, bytes: &[u8]) -> PathBuf {
    let source = tempfile::tempdir().unwrap();
    let blobs = source.path().join("blobs/sha256");
    std::fs::create_dir_all(&blobs).unwrap();
    std::fs::write(
        source.path().join("oci-layout"),
        r#"{"imageLayoutVersion":"1.0.0"}"#,
    )
    .unwrap();
    std::fs::write(source.path().join("index.json"), r#"{"manifests":[]}"#).unwrap();
    std::fs::write(
        blobs.join(blob_digest.strip_prefix("sha256:").unwrap()),
        bytes,
    )
    .unwrap();
    let stored = store
        .put(
            "seed.example/a3s/base:latest",
            &digest(b"seed-image-layout"),
            source.path(),
        )
        .await
        .unwrap();
    blob_path(&stored.path, blob_digest)
}

#[tokio::test]
async fn interrupted_body_resumes_with_exact_range_and_reports_actual_bytes() {
    let layer_bytes = (0..128 * 1024)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    let prefix_bytes = 8192;
    let fixture = ResilientRegistryFixture::start(
        vec![layer_bytes.clone()],
        LayerFault::DropFirst { prefix_bytes },
    )
    .await;
    let events = Arc::new(Mutex::new(Vec::<PullProgress>::new()));
    let recorded_events = Arc::clone(&events);
    let puller = puller(pull_policy(3, Duration::from_millis(100), 1)).with_progress_event_fn(
        Arc::new(move |event| {
            recorded_events.lock().unwrap().push(event);
        }),
    );
    let target = tempfile::tempdir().unwrap();

    puller
        .pull_with_store(&fixture.reference, target.path(), None)
        .await
        .unwrap();

    let layer = &fixture.layers[0];
    assert_eq!(
        std::fs::read(blob_path(target.path(), &layer.digest)).unwrap(),
        layer_bytes
    );
    assert!(!blob_path(target.path(), &layer.digest)
        .with_extension("partial")
        .exists());
    let requests = fixture.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].range, None);
    assert_eq!(requests[1].range.as_deref(), Some("bytes=8192-"));

    let events = events.lock().unwrap();
    let retry = events
        .iter()
        .find(|event| event.state == PullProgressState::Retrying)
        .unwrap();
    assert_eq!(retry.downloaded_bytes, prefix_bytes as u64);
    assert_eq!(retry.attempt, 2);
    assert_eq!(retry.retry_delay_ms, Some(1));
    let complete = events
        .iter()
        .find(|event| event.state == PullProgressState::Complete)
        .unwrap();
    assert_eq!(complete.downloaded_bytes, layer_bytes.len() as u64);
    assert_eq!(complete.total_bytes, layer_bytes.len() as u64);
    assert_eq!(complete.attempt, 2);
}

#[tokio::test]
async fn no_progress_timeout_stops_after_the_configured_attempt_bound() {
    let fixture =
        ResilientRegistryFixture::start(vec![b"stalled-layer".repeat(1024)], LayerFault::Stall)
            .await;
    let target = tempfile::tempdir().unwrap();

    let error = puller(pull_policy(3, Duration::from_millis(30), 1))
        .pull_with_store(&fixture.reference, target.path(), None)
        .await
        .unwrap_err()
        .to_string();

    assert_eq!(fixture.requests().len(), 3);
    assert!(error.contains("after 3 attempt(s)"), "{error}");
    assert!(error.contains("no byte progress"), "{error}");
}

#[tokio::test]
async fn retryable_http_status_uses_exactly_the_configured_attempt_count() {
    let fixture = ResilientRegistryFixture::start(
        vec![b"temporarily-unavailable-layer".repeat(512)],
        LayerFault::ServiceUnavailable,
    )
    .await;
    let target = tempfile::tempdir().unwrap();

    let error = puller(pull_policy(4, Duration::from_millis(100), 1))
        .pull_with_store(&fixture.reference, target.path(), None)
        .await
        .unwrap_err()
        .to_string();

    assert_eq!(fixture.requests().len(), 4);
    assert!(error.contains("after 4 attempt(s)"), "{error}");
    assert!(error.contains("HTTP 503"), "{error}");
}

#[tokio::test]
async fn layer_downloads_reach_but_never_exceed_the_concurrency_bound() {
    let layers = (0..5)
        .map(|index| vec![b'a' + index as u8; 16 * 1024 + index])
        .collect::<Vec<_>>();
    let fixture =
        ResilientRegistryFixture::start(layers, LayerFault::Delay(Duration::from_millis(60))).await;
    let target = tempfile::tempdir().unwrap();

    puller(pull_policy(1, Duration::from_secs(1), 2))
        .pull_with_store(&fixture.reference, target.path(), None)
        .await
        .unwrap();

    assert_eq!(fixture.requests().len(), 5);
    assert_eq!(fixture.max_active_layer_requests(), 2);
    assert_eq!(fixture.active_layer_requests(), 0);
    for layer in &fixture.layers {
        assert_eq!(
            std::fs::read(blob_path(target.path(), &layer.digest)).unwrap(),
            layer.bytes
        );
    }
}

#[tokio::test]
async fn verified_cross_image_layer_reuse_avoids_network_and_is_copy_safe() {
    let layer_bytes = b"verified-shared-layer".repeat(1024);
    let fixture =
        ResilientRegistryFixture::start(vec![layer_bytes.clone()], LayerFault::Normal).await;
    let root = tempfile::tempdir().unwrap();
    let store = ImageStore::new(&root.path().join("images"), u64::MAX).unwrap();
    let source_blob = seed_blob(&store, &fixture.layers[0].digest, &layer_bytes).await;
    let target = root.path().join("pull-target");

    puller(pull_policy(1, Duration::from_secs(1), 1))
        .pull_with_store(&fixture.reference, &target, Some(&store))
        .await
        .unwrap();

    assert!(fixture.requests().is_empty());
    let target_blob = blob_path(&target, &fixture.layers[0].digest);
    assert_eq!(std::fs::read(&target_blob).unwrap(), layer_bytes);
    std::fs::write(source_blob, vec![b'x'; layer_bytes.len()]).unwrap();
    assert_eq!(std::fs::read(target_blob).unwrap(), layer_bytes);
}

#[tokio::test]
async fn same_size_corrupt_cross_image_layer_is_rejected_and_downloaded() {
    let layer_bytes = b"network-fallback-layer".repeat(1024);
    let fixture =
        ResilientRegistryFixture::start(vec![layer_bytes.clone()], LayerFault::Normal).await;
    let mut corrupt_bytes = layer_bytes.clone();
    corrupt_bytes[0] ^= 0xff;
    let root = tempfile::tempdir().unwrap();
    let store = ImageStore::new(&root.path().join("images"), u64::MAX).unwrap();
    seed_blob(&store, &fixture.layers[0].digest, &corrupt_bytes).await;
    let target = root.path().join("pull-target");

    puller(pull_policy(1, Duration::from_secs(1), 1))
        .pull_with_store(&fixture.reference, &target, Some(&store))
        .await
        .unwrap();

    assert_eq!(fixture.requests().len(), 1);
    assert_eq!(
        std::fs::read(blob_path(&target, &fixture.layers[0].digest)).unwrap(),
        layer_bytes
    );
    assert!(std::fs::read_dir(target.join("blobs/sha256"))
        .unwrap()
        .all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".blob-reuse-")));
}
