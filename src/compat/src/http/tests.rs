use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::control::test_support::TestHarness;
use crate::volume::tests::support::ServiceHarness as VolumeServiceHarness;
use crate::volume::VolumeService;

use super::*;

struct TestCredentialVerifier;

#[async_trait]
impl CredentialVerifier for TestCredentialVerifier {
    async fn verify(
        &self,
        credential: &PresentedCredential,
    ) -> AuthenticationResult<AuthenticatedAccount> {
        if credential.scheme() != CredentialScheme::ApiKey {
            return Err(AuthenticationError::Invalid);
        }
        let owner_id = match credential.expose_secret() {
            "e2b_a1b2c3" => "owner-1",
            "e2b_b2c3d4" => "owner-2",
            _ => return Err(AuthenticationError::Invalid),
        };
        Ok(AuthenticatedAccount {
            owner_id: owner_id.to_string(),
            client_id: "fixture-client".to_string(),
        })
    }
}

struct TestCursorDecoder;

impl CursorDecoder for TestCursorDecoder {
    fn decode(&self, value: &str) -> CursorResult<Option<crate::control::SandboxCursor>> {
        if value == "cursor-0" {
            Ok(None)
        } else {
            Err(CursorError::Invalid)
        }
    }
}

fn app() -> Router {
    let harness = TestHarness::new();
    lifecycle_router(LifecycleHttpState::new(
        harness.service,
        Arc::new(TestCredentialVerifier),
        Arc::new(TestCursorDecoder),
        LifecycleHttpConfig {
            domain: Some("fixture.invalid:3443".to_string()),
            ..LifecycleHttpConfig::default()
        },
    ))
}

fn app_with_volumes(volumes: Arc<VolumeService>) -> Router {
    let harness = TestHarness::new();
    lifecycle_router(
        LifecycleHttpState::new(
            harness.service,
            Arc::new(TestCredentialVerifier),
            Arc::new(TestCursorDecoder),
            LifecycleHttpConfig {
                domain: Some("fixture.invalid:3443".to_string()),
                ..LifecycleHttpConfig::default()
            },
        )
        .with_volume_service(volumes),
    )
}

#[tokio::test]
async fn router_serves_owner_scoped_volume_control_and_bearer_content() {
    let volumes = VolumeServiceHarness::new();
    let app = app_with_volumes(Arc::new(volumes.service.clone()));

    let response = send(
        &app,
        Method::POST,
        "/volumes",
        Some(json!({"name": "data"})),
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = body_json(response).await;
    let volume_id = created["volumeID"].as_str().unwrap();
    let token = created["token"].as_str().unwrap();
    assert_eq!(created["name"], "data");

    let response = send(&app, Method::GET, "/volumes", None, true).await;
    assert_eq!(response.status(), StatusCode::OK);
    let listed = body_json(response).await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["volumeID"], volume_id);
    assert!(listed[0].get("token").is_none());

    let response = send_raw(
        &app,
        Method::GET,
        &format!("/volumes/{volume_id}"),
        Body::empty(),
        &[("x-api-key", "e2b_b2c3d4")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let authorization = format!("Bearer {token}");
    let response = send_raw(
        &app,
        Method::POST,
        &format!("/volumecontent/{volume_id}/dir?path=%2Fnested%2Fdeep&force=true&mode=493"),
        Body::empty(),
        &[("authorization", &authorization)],
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(body_json(response).await["path"], "/nested/deep");

    let response = send_raw(
        &app,
        Method::PUT,
        &format!("/volumecontent/{volume_id}/file?path=%2Fnested%2Fdeep%2Fvalue.txt"),
        Body::from("hello-volume"),
        &[
            ("authorization", &authorization),
            ("content-type", "application/octet-stream"),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(body_json(response).await["size"], 12);

    let response = send_raw(
        &app,
        Method::GET,
        &format!("/volumecontent/{volume_id}/file?path=%2Fnested%2Fdeep%2Fvalue.txt"),
        Body::empty(),
        &[("authorization", &authorization)],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(&body_bytes(response).await[..], b"hello-volume");

    let response = send_raw(
        &app,
        Method::PATCH,
        &format!("/volumecontent/{volume_id}/path?path=%2Fnested%2Fdeep%2Fvalue.txt"),
        Body::from(r#"{"mode":384}"#),
        &[
            ("authorization", &authorization),
            ("content-type", "application/json"),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["mode"], 384);

    let response = send_raw(
        &app,
        Method::GET,
        &format!("/volumecontent/{volume_id}/dir?path=%2F&depth=3"),
        Body::empty(),
        &[("authorization", &authorization)],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let entries = body_json(response).await;
    assert_eq!(entries.as_array().unwrap().len(), 3);
    assert_eq!(entries[2]["path"], "/nested/deep/value.txt");

    let response = send_raw(
        &app,
        Method::GET,
        &format!("/volumecontent/{volume_id}/path?path=%2F"),
        Body::empty(),
        &[("authorization", "Bearer wrong-token")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(response).await["code"], "forbidden");

    let response = send_raw(
        &app,
        Method::GET,
        &format!("/volumecontent/{volume_id}/path?path=%2F"),
        Body::empty(),
        &[("x-api-key", "e2b_a1b2c3")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let response = send(
        &app,
        Method::DELETE,
        &format!("/volumes/{volume_id}"),
        None,
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let response = send_raw(
        &app,
        Method::GET,
        &format!("/volumecontent/{volume_id}/path?path=%2F"),
        Body::empty(),
        &[("authorization", &authorization)],
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn router_serves_the_pinned_official_lifecycle_shape() {
    let app = app();
    let create_body = json!({
        "allow_internet_access": false,
        "autoPause": true,
        "autoPauseMemory": false,
        "autoResume": {"enabled": false},
        "envVars": {"ALPHA": "one", "BETA": "two"},
        "metadata": {"purpose": "fixture", "team": "alpha beta"},
        "secure": true,
        "templateID": "fixture-template",
        "timeout": 321
    });
    let response = send(&app, Method::POST, "/sandboxes", Some(create_body), true).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = body_json(response).await;
    assert_eq!(created["sandboxID"], "sandbox-1");
    assert_eq!(created["clientID"], "fixture-client");
    assert_eq!(created["envdAccessToken"], "fixture-envd-token");
    assert_eq!(created["trafficAccessToken"], "fixture-traffic-token");
    assert_eq!(created["domain"], "fixture.invalid:3443");

    let response = send(
        &app,
        Method::GET,
        "/sandboxes/sandbox-1/logs?start=0&limit=2",
        None,
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let logs = body_json(response).await;
    assert_eq!(logs["logs"].as_array().unwrap().len(), 2);
    assert_eq!(logs["logEntries"].as_array().unwrap().len(), 2);
    assert_eq!(logs["logEntries"][0]["message"], "starting");
    assert_eq!(logs["logEntries"][1]["level"], "error");
    let legacy_line: Value =
        serde_json::from_str(logs["logs"][1]["line"].as_str().unwrap()).unwrap();
    assert_eq!(legacy_line["logger"], "a3s-box-runtime");
    assert_eq!(legacy_line["stream"], "stderr");

    let response = send(
        &app,
        Method::GET,
        "/v2/sandboxes/sandbox-1/logs?direction=backward&limit=1&level=error&search=failed",
        None,
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let logs = body_json(response).await;
    assert_eq!(logs["logs"].as_array().unwrap().len(), 1);
    assert_eq!(logs["logs"][0]["message"], "failed once");
    assert_eq!(logs["logs"][0]["fields"]["stream"], "stderr");

    let response = send(
        &app,
        Method::POST,
        "/sandboxes/sandbox-1/connect",
        Some(json!({"timeout": 222})),
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let response = send(
        &app,
        Method::POST,
        "/sandboxes/sandbox-1/pause",
        Some(json!({"memory": true})),
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let response = send(&app, Method::POST, "/sandboxes/sandbox-1/pause", None, true).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);

    let response = send(&app, Method::GET, "/sandboxes/sandbox-1", None, true).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["state"], "paused");

    let response = send(
        &app,
        Method::POST,
        "/sandboxes/sandbox-1/resume",
        Some(json!({"timeout": 400, "autoPause": true})),
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let response = send(&app, Method::POST, "/sandboxes/sandbox-1/pause", None, true).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let response = send(
        &app,
        Method::POST,
        "/sandboxes/sandbox-1/connect",
        Some(json!({"timeout": 222})),
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let response = send(
        &app,
        Method::GET,
        "/v2/sandboxes?limit=2&metadata=team%3Dalpha%252520beta&nextToken=cursor-0&state=running%2Cpaused",
        None,
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let listed = body_json(response).await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["sandboxID"], "sandbox-1");
    assert_eq!(listed[0]["state"], "running");

    let response = send(
        &app,
        Method::GET,
        "/sandboxes?metadata=team%3Dalpha%252520beta&state=paused",
        None,
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await.as_array().unwrap().len(), 1);

    let response = send(
        &app,
        Method::POST,
        "/sandboxes/sandbox-1/refreshes",
        Some(json!({"duration": 60})),
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let response = send(
        &app,
        Method::POST,
        "/sandboxes/sandbox-1/refreshes",
        Some(json!({})),
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let response = send(
        &app,
        Method::POST,
        "/sandboxes/sandbox-1/refreshes",
        None,
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let response = send(
        &app,
        Method::POST,
        "/sandboxes/sandbox-1/refreshes",
        Some(json!({"duration": 3_600, "futureField": true})),
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let response = send(&app, Method::GET, "/sandboxes/sandbox-1", None, true).await;
    assert_eq!(response.status(), StatusCode::OK);
    let detail = body_json(response).await;
    assert_eq!(detail["allowInternetAccess"], false);
    assert_eq!(detail["lifecycle"]["onTimeout"], "pause");
    assert_eq!(detail["endAt"], "2026-07-14T13:00:00Z");

    let response = send(
        &app,
        Method::POST,
        "/sandboxes/sandbox-1/refreshes",
        Some(json!({"duration": 3_601})),
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let response = send(
        &app,
        Method::POST,
        "/sandboxes/sandbox-1/timeout",
        Some(json!({"timeout": 123})),
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let response = send(&app, Method::DELETE, "/sandboxes/sandbox-1", None, true).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let response = send(
        &app,
        Method::DELETE,
        "/sandboxes/missing-sandbox",
        None,
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(response).await["code"], 404);
}

#[tokio::test]
async fn router_serves_single_and_batch_runtime_metrics() {
    let app = app();
    let response = send(
        &app,
        Method::POST,
        "/sandboxes",
        Some(json!({
            "templateID": "runtime-envd-template",
            "timeout": 60
        })),
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let response = send(
        &app,
        Method::GET,
        "/sandboxes/sandbox-1/metrics",
        None,
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let metrics = body_json(response).await;
    assert_eq!(metrics.as_array().unwrap().len(), 1);
    assert_eq!(metrics[0]["timestamp"], "2026-07-14T12:00:00Z");
    assert_eq!(metrics[0]["timestampUnix"], test_timestamp());
    assert_eq!(metrics[0]["cpuCount"], 2);
    assert_eq!(metrics[0]["memCache"], 0);
    assert_eq!(metrics[0]["diskTotal"], 1_073_741_824_u64);

    let response = send(
        &app,
        Method::GET,
        "/sandboxes/sandbox-1/metrics?start=0&end=1",
        None,
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_json(response).await.as_array().unwrap().is_empty());

    let response = send(
        &app,
        Method::GET,
        "/sandboxes/metrics?sandbox_ids=sandbox-1,missing-sandbox",
        None,
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let batch = body_json(response).await;
    assert_eq!(batch["sandboxes"]["sandbox-1"]["cpuCount"], 2);
    assert!(batch["sandboxes"].get("missing-sandbox").is_none());

    let response = send(
        &app,
        Method::GET,
        "/sandboxes/sandbox-1/metrics?start=2&end=1",
        None,
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let response = send(
        &app,
        Method::GET,
        "/sandboxes/missing-sandbox/metrics",
        None,
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

fn test_timestamp() -> i64 {
    chrono::DateTime::parse_from_rfc3339("2026-07-14T12:00:00Z")
        .unwrap()
        .timestamp()
}

#[tokio::test]
async fn router_requires_authentication_and_maps_invalid_json() {
    let app = app();
    let response = send(&app, Method::GET, "/v2/sandboxes", None, false).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let request = Request::builder()
        .method(Method::POST)
        .uri("/sandboxes")
        .header("x-api-key", "e2b_a1b2c3")
        .header("content-type", "application/json")
        .body(Body::from("{"))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn presented_credentials_redact_secret_debug_material() {
    let headers = axum::http::HeaderMap::from_iter([(
        axum::http::HeaderName::from_static("x-api-key"),
        axum::http::HeaderValue::from_static("e2b_a1b2c3"),
    )]);
    let credential = PresentedCredential::from_headers(&headers).unwrap();
    let debug = format!("{credential:?}");
    assert!(!debug.contains("e2b_a1b2c3"));
    assert!(debug.contains("REDACTED"));
}

async fn send(
    app: &Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
    authenticated: bool,
) -> axum::response::Response {
    let mut builder = Request::builder().method(method).uri(uri);
    if authenticated {
        builder = builder.header("x-api-key", "e2b_a1b2c3");
    }
    let body = if let Some(body) = body {
        builder = builder.header("content-type", "application/json");
        Body::from(serde_json::to_vec(&body).unwrap())
    } else {
        Body::empty()
    };
    app.clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap()
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = body_bytes(response).await;
    serde_json::from_slice(&bytes).unwrap()
}

async fn body_bytes(response: axum::response::Response) -> hyper::body::Bytes {
    hyper::body::to_bytes(response.into_body()).await.unwrap()
}

async fn send_raw(
    app: &Router,
    method: Method,
    uri: &str,
    body: Body,
    headers: &[(&str, &str)],
) -> axum::response::Response {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    app.clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap()
}
