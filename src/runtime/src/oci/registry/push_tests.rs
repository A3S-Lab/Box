use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::body::{Body, HttpBody};
use axum::extract::State;
use axum::http::{Method, Request, Response, StatusCode};
use axum::routing::any;
use axum::Router;
use sha2::{Digest, Sha256};

use super::{ImageReference, RegistryAuth, RegistryProtocol, RegistryPusher};

const REPOSITORY: &str = "a3s/app";

#[derive(Clone)]
struct PushRegistryState {
    base_url: String,
    manifest_bytes: Arc<Mutex<Option<Vec<u8>>>>,
}

struct PushRegistryFixture {
    reference: ImageReference,
    manifest_bytes: Arc<Mutex<Option<Vec<u8>>>>,
    task: tokio::task::JoinHandle<()>,
}

impl PushRegistryFixture {
    async fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let manifest_bytes = Arc::new(Mutex::new(None));
        let state = PushRegistryState {
            base_url: format!("http://{address}"),
            manifest_bytes: Arc::clone(&manifest_bytes),
        };
        let app = Router::new()
            .route("/*path", any(push_registry_handler))
            .with_state(state);
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
            manifest_bytes,
            task,
        }
    }

    fn pushed_manifest(&self) -> Vec<u8> {
        self.manifest_bytes
            .lock()
            .unwrap()
            .clone()
            .expect("manifest was not pushed")
    }
}

impl Drop for PushRegistryFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn push_registry_handler(
    State(state): State<PushRegistryState>,
    request: Request<Body>,
) -> Response<Body> {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let upload_path = format!("/v2/{REPOSITORY}/blobs/uploads/");
    let manifest_path = format!("/v2/{REPOSITORY}/manifests/latest");

    if method == Method::GET && path == "/v2/" {
        return response(StatusCode::OK, None, None, Body::empty());
    }
    if method == Method::POST && path == upload_path {
        let location = format!("{}{upload_path}fixture", state.base_url);
        return response(StatusCode::ACCEPTED, Some(&location), None, Body::empty());
    }
    if method == Method::PATCH && path == format!("{upload_path}fixture") {
        let _ = body_bytes(request.into_body()).await;
        let location = format!("{}{upload_path}fixture", state.base_url);
        return response(StatusCode::ACCEPTED, Some(&location), None, Body::empty());
    }
    if method == Method::PUT && path == format!("{upload_path}fixture") {
        let _ = body_bytes(request.into_body()).await;
        let location = format!("{}/v2/{REPOSITORY}/blobs/fixture", state.base_url);
        return response(StatusCode::CREATED, Some(&location), None, Body::empty());
    }
    if method == Method::PUT && path == manifest_path {
        let bytes = body_bytes(request.into_body()).await;
        let digest = digest(&bytes);
        *state.manifest_bytes.lock().unwrap() = Some(bytes);
        let location = format!("{}/v2/{REPOSITORY}/manifests/{digest}", state.base_url);
        return response(
            StatusCode::CREATED,
            Some(&location),
            Some(&digest),
            Body::empty(),
        );
    }
    if method == Method::GET && path == manifest_path {
        let bytes = state
            .manifest_bytes
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_default();
        let digest = digest(&bytes);
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/vnd.oci.image.manifest.v1+json")
            .header("docker-content-digest", digest)
            .body(Body::from(bytes))
            .unwrap();
    }

    response(StatusCode::NOT_FOUND, None, None, Body::empty())
}

fn response(
    status: StatusCode,
    location: Option<&str>,
    digest: Option<&str>,
    body: Body,
) -> Response<Body> {
    let mut builder = Response::builder().status(status);
    if let Some(location) = location {
        builder = builder.header("location", location);
    }
    if let Some(digest) = digest {
        builder = builder.header("docker-content-digest", digest);
    }
    builder.body(body).unwrap()
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

async fn body_bytes(mut body: Body) -> Vec<u8> {
    let mut bytes = Vec::new();
    while let Some(chunk) = body.data().await {
        bytes.extend_from_slice(&chunk.unwrap());
    }
    bytes
}

fn write_blob(layout: &Path, bytes: &[u8]) -> String {
    let digest = digest(bytes);
    let hex = digest.strip_prefix("sha256:").unwrap();
    let blobs = layout.join("blobs/sha256");
    std::fs::create_dir_all(&blobs).unwrap();
    std::fs::write(blobs.join(hex), bytes).unwrap();
    digest
}

#[tokio::test]
async fn push_preserves_exact_content_addressed_manifest_bytes() {
    let fixture = PushRegistryFixture::start().await;
    let layout = tempfile::tempdir().unwrap();
    let config = br#"{"architecture":"amd64","os":"linux"}"#;
    let first_layer = b"a3s-box-registry-push-layer-one";
    let second_layer = b"a3s-box-registry-push-layer-two";
    let config_digest = write_blob(layout.path(), config);
    let first_layer_digest = write_blob(layout.path(), first_layer);
    let second_layer_digest = write_blob(layout.path(), second_layer);
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config.len()
        },
        "layers": [
            {
                "mediaType": "application/vnd.oci.image.layer.v1.tar",
                "digest": first_layer_digest,
                "size": first_layer.len()
            },
            {
                "mediaType": "application/vnd.oci.image.layer.v1.tar",
                "digest": second_layer_digest,
                "size": second_layer.len()
            }
        ]
    });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
    let manifest_digest = write_blob(layout.path(), &manifest_bytes);
    std::fs::write(
        layout.path().join("index.json"),
        serde_json::json!({
            "schemaVersion": 2,
            "manifests": [{
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": manifest_digest,
                "size": manifest_bytes.len()
            }]
        })
        .to_string(),
    )
    .unwrap();

    let pusher =
        RegistryPusher::with_auth_and_protocol(RegistryAuth::anonymous(), RegistryProtocol::Http);
    let result = pusher
        .push(&fixture.reference, layout.path())
        .await
        .unwrap();

    assert_eq!(result.manifest_digest, manifest_digest);
    assert_eq!(fixture.pushed_manifest(), manifest_bytes);
}
