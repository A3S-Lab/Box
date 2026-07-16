use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::control::test_support::TestHarness;

use super::*;

struct TestCredentialVerifier;

#[async_trait]
impl CredentialVerifier for TestCredentialVerifier {
    async fn verify(
        &self,
        credential: &PresentedCredential,
    ) -> AuthenticationResult<AuthenticatedAccount> {
        if credential.scheme() != CredentialScheme::ApiKey
            || credential.expose_secret() != "e2b_a1b2c3"
        {
            return Err(AuthenticationError::Invalid);
        }
        Ok(AuthenticatedAccount {
            owner_id: "owner-1".to_string(),
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
        Method::POST,
        "/sandboxes/sandbox-1/connect",
        Some(json!({"timeout": 222})),
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

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
    let bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}
