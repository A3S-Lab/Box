use std::path::Path;

use a3s_box_core::ExecutionIsolation;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use tower::ServiceExt;

use crate::control::{EnvdMode, SandboxIdentityProvider, TemplateProvider, TokenScope};
use crate::http::CredentialHash;
use crate::routing::{CODE_INTERPRETER_PORT, ENVD_PORT};

use super::config::E2bCompatConfig;
use super::{E2bCompatService, E2bConfigError, UuidSandboxIdentityProvider};

fn parse_config(root: &Path) -> E2bCompatConfig {
    write_test_tls(root);
    E2bCompatConfig::parse_with_environment(&acl_config(root), test_environment).unwrap()
}

fn acl_config(root: &Path) -> String {
    let database = acl_path(&root.join("lifecycle.sqlite3"));
    let runtime_home = acl_path(&root.join("runtime"));
    let runtime_state = acl_path(&root.join("managed-executions.json"));
    let certificate = acl_path(&root.join("gateway-cert.pem"));
    let private_key = acl_path(&root.join("gateway-key.pem"));
    let hash = CredentialHash::derive("e2b_a1b2c3", 100_000, &[3; 16]).unwrap();
    format!(
        r#"
e2b_compat {{
  api_listen = "127.0.0.1:3001"
  api_public_url = "https://api.box.example.com"
  sandbox_domain = "box.example.com"
  database_path = "{database}"
  runtime_home = "{runtime_home}"
  runtime_state_path = "{runtime_state}"
  max_json_bytes = 2097152

  gateway {{
    listen = "127.0.0.1:3002"
    tls_certificate_path = "{certificate}"
    tls_private_key_path = "{private_key}"
    max_connections = 1024
    handshake_timeout_ms = 5000
    connect_timeout_ms = 2000
    drain_timeout_seconds = 10
  }}

  supervisor {{
    interval_seconds = 5
    batch_size = 100
    reconciliation_page_size = 200
  }}

  account "primary" {{
    scheme = "api_key"
    owner_id = "owner-production"
    client_id = "client-production"
    hash = "{hash}"
  }}

  token_key "2026-07" {{
    version = 7
    active = true
    encryption_key = env("TOKEN_ENCRYPTION")
    digest_key = env("TOKEN_DIGEST")
  }}

  template_policy "fixture-template" {{
    image = "alpine:3.20"
    envd_version = "0.1.3"
    envd_mode = "runtime"
    isolation = "sandbox"
    network = "none"
    command = ["/bin/sh", "-c", "while :; do sleep 60; done"]
    read_only = false
    stdin_open = false

    resources {{
      vcpus = 2
      memory_mb = 512
      disk_mb = 1024
    }}

    route {{
      port = 49983
      token_scope = "envd"
    }}

    route {{
      port = 49999
      token_scope = "traffic"
    }}
  }}
}}
"#
    )
}

fn write_test_tls(root: &Path) {
    let rcgen::CertifiedKey { cert, key_pair } = rcgen::generate_simple_self_signed(vec![
        "*.box.example.com".to_string(),
        "sandbox.box.example.com".to_string(),
    ])
    .unwrap();
    std::fs::write(root.join("gateway-cert.pem"), cert.pem()).unwrap();
    std::fs::write(root.join("gateway-key.pem"), key_pair.serialize_pem()).unwrap();
}

fn acl_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn test_environment(name: &str) -> Option<String> {
    match name {
        "TOKEN_ENCRYPTION" => Some(hex::encode([7_u8; 32])),
        "TOKEN_DIGEST" => Some(hex::encode([8_u8; 32])),
        _ => None,
    }
}

#[tokio::test]
async fn parses_acl_into_strict_runtime_credentials_and_template_policy() {
    let root = tempfile::tempdir().unwrap();
    let config = parse_config(root.path());

    assert_eq!(config.api_listen().to_string(), "127.0.0.1:3001");
    assert_eq!(
        config.api_public_url().as_str(),
        "https://api.box.example.com/"
    );
    assert_eq!(config.sandbox_domain(), "box.example.com");
    assert_eq!(config.gateway().listen().to_string(), "127.0.0.1:3002");
    assert_eq!(config.gateway().max_connections().get(), 1024);
    assert_eq!(config.supervisor().batch_size().get(), 100);
    assert_eq!(config.templates.len(), 1);
    let template = config.templates.resolve("fixture-template").await.unwrap();
    assert_eq!(template.config.isolation, ExecutionIsolation::Sandbox);
    assert_eq!(template.config.image, "alpine:3.20");
    assert_eq!(template.config.resources.memory_mb, 512);
    assert_eq!(template.envd_mode, EnvdMode::Runtime);
    assert_eq!(
        template.routing.token_scope(ENVD_PORT),
        Some(TokenScope::Envd)
    );
    assert_eq!(
        template.routing.token_scope(CODE_INTERPRETER_PORT),
        Some(TokenScope::Traffic)
    );

    let debug = format!("{config:?}");
    assert!(!debug.contains(&hex::encode([7_u8; 32])));
    assert!(!debug.contains(&hex::encode([8_u8; 32])));
    assert!(!debug.contains("e2b_a1b2c3"));
}

#[test]
fn rejects_plaintext_token_keys_missing_environment_and_unknown_fields() {
    let root = tempfile::tempdir().unwrap();
    let input = acl_config(root.path());

    let plaintext = input.replace(
        "env(\"TOKEN_ENCRYPTION\")",
        &format!("\"{}\"", hex::encode([7_u8; 32])),
    );
    assert!(
        E2bCompatConfig::parse_with_environment(&plaintext, test_environment)
            .unwrap_err()
            .to_string()
            .contains("must use env")
    );

    let missing = E2bCompatConfig::parse_with_environment(&input, |_| None).unwrap_err();
    assert!(missing.to_string().contains("TOKEN_ENCRYPTION"));
    assert!(!missing.to_string().contains(&hex::encode([7_u8; 32])));

    let unknown = input.replace(
        "max_json_bytes = 2097152",
        "max_json_bytes = 2097152\n  accidental_backend = \"unsafe\"",
    );
    assert!(
        E2bCompatConfig::parse_with_environment(&unknown, test_environment)
            .unwrap_err()
            .to_string()
            .contains("unknown attribute accidental_backend")
    );
}

#[tokio::test]
async fn defaults_templates_to_broker_envd_and_rejects_unknown_modes() {
    let root = tempfile::tempdir().unwrap();
    let input = acl_config(root.path());
    let broker = input.replace("    envd_mode = \"runtime\"\n", "");
    let config = E2bCompatConfig::parse_with_environment(&broker, test_environment).unwrap();
    let template = config.templates.resolve("fixture-template").await.unwrap();
    assert_eq!(template.envd_mode, EnvdMode::Broker);

    let invalid = input.replace("envd_mode = \"runtime\"", "envd_mode = \"sidecar\"");
    assert!(
        E2bCompatConfig::parse_with_environment(&invalid, test_environment)
            .unwrap_err()
            .to_string()
            .contains("envd_mode must be broker or runtime")
    );
}

#[test]
fn rejects_missing_or_unsafe_gateway_configuration() {
    let root = tempfile::tempdir().unwrap();
    let input = acl_config(root.path());

    let missing = input.replace(
        &input[input.find("  gateway {").unwrap()..input.find("  supervisor {").unwrap()],
        "",
    );
    assert!(
        E2bCompatConfig::parse_with_environment(&missing, test_environment)
            .unwrap_err()
            .to_string()
            .contains("gateway block is required")
    );

    let same_listener = input.replace("listen = \"127.0.0.1:3002\"", "listen = \"127.0.0.1:3001\"");
    assert!(
        E2bCompatConfig::parse_with_environment(&same_listener, test_environment)
            .unwrap_err()
            .to_string()
            .contains("must differ")
    );

    let relative_key = input.replace(
        &format!(
            "tls_private_key_path = \"{}\"",
            acl_path(&root.path().join("gateway-key.pem"))
        ),
        "tls_private_key_path = \"gateway-key.pem\"",
    );
    assert!(
        E2bCompatConfig::parse_with_environment(&relative_key, test_environment)
            .unwrap_err()
            .to_string()
            .contains("absolute normalized path")
    );

    let unbounded = input.replace("max_connections = 1024", "max_connections = 0");
    assert!(
        E2bCompatConfig::parse_with_environment(&unbounded, test_environment)
            .unwrap_err()
            .to_string()
            .contains("max_connections must be between")
    );
}

#[tokio::test]
async fn loader_requires_the_acl_extension_before_reading() {
    let root = tempfile::tempdir().unwrap();
    let error = E2bCompatConfig::load(root.path().join("service.conf"))
        .await
        .unwrap_err();
    assert!(matches!(error, E2bConfigError::InvalidExtension(_)));
}

#[test]
fn uuid_identity_provider_generates_valid_independent_ids() {
    let provider = UuidSandboxIdentityProvider;
    let first = provider.next_identity().unwrap();
    let second = provider.next_identity().unwrap();

    assert!(first.sandbox_id.as_str().starts_with("sandbox-"));
    assert!(first.operation_id.as_str().starts_with("e2b-create-"));
    assert_ne!(first.sandbox_id, second.sandbox_id);
    assert_ne!(first.operation_id, second.operation_id);
}

#[tokio::test]
async fn production_composition_wires_auth_sqlite_routing_and_supervision_without_launching_runtime(
) {
    let root = tempfile::tempdir().unwrap();
    let service = E2bCompatService::build(parse_config(root.path()))
        .await
        .unwrap();
    assert_eq!(service.listen().to_string(), "127.0.0.1:3001");
    assert_eq!(service.gateway_listen().to_string(), "127.0.0.1:3002");
    assert_eq!(service.sandbox_domain(), "box.example.com");
    assert!(service
        .route_parser()
        .parse_host(
            "49983-sandbox-example.box.example.com",
            &axum::http::HeaderMap::new(),
        )
        .is_ok());
    let report = service.reconcile_startup().await.unwrap();
    assert_eq!(report.examined, 0);

    let unauthenticated = service
        .router()
        .oneshot(
            Request::builder()
                .uri("/v2/sandboxes")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    let authenticated = service
        .router()
        .oneshot(
            Request::builder()
                .uri("/v2/sandboxes")
                .header("x-api-key", "e2b_a1b2c3")
                .header(header::ACCEPT, "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(authenticated.status(), StatusCode::OK);
}
